//! Wrapping styled text to a width: words go to the next line when they don't fit, a word wider
//! than the width is cut, and a hard break starts a new line.

use vortex_core::unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr};
use vortex_view::graphics::Style;

use super::parse::{Atom, BREAK};

/// A run of text in one style on a line of the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub text: String,
    pub style: Style,
    /// The link it belongs to (an index into the page's links).
    pub link: Option<usize>,
}

impl Piece {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            link: None,
        }
    }

    pub fn width(&self) -> usize {
        self.text.width()
    }
}

/// Appends `piece` to `pieces`, merged into the last one when they look the same.
pub fn push_piece(pieces: &mut Vec<Piece>, piece: Piece) {
    if piece.text.is_empty() {
        return;
    }
    match pieces.last_mut() {
        Some(last) if last.style == piece.style && last.link == piece.link => {
            last.text.push_str(&piece.text)
        }
        _ => pieces.push(piece),
    }
}

/// A wrapped line: its pieces, its width, and the byte in the source of its first word.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wrapped {
    pub pieces: Vec<Piece>,
    pub width: usize,
    pub source: Option<usize>,
}

impl Wrapped {
    fn push(&mut self, piece: Piece) {
        self.width += piece.width();
        push_piece(&mut self.pieces, piece);
    }
}

/// A word (its pieces, its width, the byte where it starts), a space, or a hard break.
#[derive(Debug)]
enum Token {
    Word(Vec<Piece>, usize, usize),
    Space(Style, Option<usize>),
    Break(usize),
}

fn tokens(atoms: &[Atom]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word: Option<(Vec<Piece>, usize, usize)> = None;
    let end_word = |word: &mut Option<(Vec<Piece>, usize, usize)>, tokens: &mut Vec<Token>| {
        if let Some((pieces, width, source)) = word.take() {
            tokens.push(Token::Word(pieces, width, source));
        }
    };
    for atom in atoms {
        if atom.text == BREAK {
            end_word(&mut word, &mut tokens);
            tokens.push(Token::Break(atom.source));
            continue;
        }
        for (index, grapheme) in atom.text.grapheme_indices(true) {
            if grapheme.chars().all(char::is_whitespace) {
                end_word(&mut word, &mut tokens);
                // runs of spaces are one space
                if !matches!(tokens.last(), Some(Token::Space(..))) {
                    tokens.push(Token::Space(atom.style, atom.link));
                }
                continue;
            }
            let (pieces, width, _) =
                word.get_or_insert_with(|| (Vec::new(), 0, atom.source + index));
            *width += grapheme.width();
            push_piece(
                pieces,
                Piece {
                    text: grapheme.to_string(),
                    style: atom.style,
                    link: atom.link,
                },
            );
        }
    }
    end_word(&mut word, &mut tokens);
    tokens
}

/// Cuts `pieces` into chunks at most `width` wide.
fn cut(pieces: Vec<Piece>, width: usize) -> Vec<(Vec<Piece>, usize)> {
    let mut chunks = vec![(Vec::new(), 0)];
    for piece in pieces {
        for grapheme in piece.text.graphemes(true) {
            let w = grapheme.width();
            let (_, chunk_width) = chunks.last_mut().unwrap();
            if *chunk_width + w > width && *chunk_width > 0 {
                chunks.push((Vec::new(), 0));
            }
            let (chunk, chunk_width) = chunks.last_mut().unwrap();
            *chunk_width += w;
            push_piece(
                chunk,
                Piece {
                    text: grapheme.to_string(),
                    style: piece.style,
                    link: piece.link,
                },
            );
        }
    }
    chunks
}

/// Wraps `atoms` to `width`: at least one line, empty for no text.
pub fn wrap(atoms: &[Atom], width: usize) -> Vec<Wrapped> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = Wrapped::default();
    let mut space: Option<(Style, Option<usize>)> = None;
    for token in tokens(atoms) {
        match token {
            Token::Break(source) => {
                line.source.get_or_insert(source);
                lines.push(std::mem::take(&mut line));
                space = None;
            }
            Token::Space(style, link) => {
                if line.width > 0 {
                    space = Some((style, link));
                }
            }
            Token::Word(pieces, word_width, source) => {
                let gap = usize::from(space.is_some() && line.width > 0);
                if line.width > 0 && line.width + gap + word_width > width {
                    lines.push(std::mem::take(&mut line));
                } else if gap == 1 {
                    let (style, link) = space.unwrap();
                    line.push(Piece {
                        text: " ".into(),
                        style,
                        link,
                    });
                }
                space = None;
                line.source.get_or_insert(source);
                if word_width <= width.saturating_sub(line.width) {
                    for piece in pieces {
                        line.push(piece);
                    }
                } else {
                    // wider than a line: cut, the last chunk goes on
                    let mut chunks = cut(pieces, width).into_iter().peekable();
                    while let Some((chunk, _)) = chunks.next() {
                        for piece in chunk {
                            line.push(piece);
                        }
                        if chunks.peek().is_some() {
                            let next = Wrapped {
                                source: line.source,
                                ..Default::default()
                            };
                            lines.push(std::mem::replace(&mut line, next));
                        }
                    }
                }
            }
        }
    }
    if line.width > 0 || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// The width of `atoms` on one line (the widest of its hard-broken lines).
pub fn natural_width(atoms: &[Atom]) -> usize {
    wrap(atoms, usize::MAX / 2)
        .iter()
        .map(|line| line.width)
        .max()
        .unwrap_or(0)
}

/// The width of the widest word of `atoms`.
pub fn widest_word(atoms: &[Atom]) -> usize {
    tokens(atoms)
        .iter()
        .map(|token| match token {
            Token::Word(_, width, _) => *width,
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

/// The text of `pieces`.
pub fn text(pieces: &[Piece]) -> String {
    pieces.iter().map(|piece| piece.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(text: &str, source: usize) -> Atom {
        Atom {
            text: text.into(),
            style: Style::default(),
            source,
            link: None,
        }
    }

    fn lines(atoms: &[Atom], width: usize) -> Vec<String> {
        wrap(atoms, width)
            .iter()
            .map(|line| text(&line.pieces))
            .collect()
    }

    #[test]
    fn words_go_to_the_next_line() {
        let atoms = [atom("the quick brown fox jumps", 0)];
        assert_eq!(lines(&atoms, 10), ["the quick", "brown fox", "jumps"]);
        assert_eq!(lines(&atoms, 100), ["the quick brown fox jumps"]);
    }

    #[test]
    fn a_long_word_is_cut() {
        let atoms = [atom("a abcdefghij b", 0)];
        assert_eq!(lines(&atoms, 4), ["a", "abcd", "efgh", "ij b"]);
    }

    #[test]
    fn hard_breaks_and_spaces() {
        let atoms = [atom("one  two", 0), atom(BREAK, 8), atom("three", 9)];
        assert_eq!(lines(&atoms, 20), ["one two", "three"]);
    }

    #[test]
    fn a_word_across_styles_stays_whole() {
        let bold = Style::default().add_modifier(vortex_view::theme::Modifier::BOLD);
        let atoms = [
            atom("aaa ", 0),
            Atom {
                text: "bb".into(),
                style: bold,
                source: 4,
                link: None,
            },
            atom("cc", 8),
        ];
        let wrapped = wrap(&atoms, 5);
        assert_eq!(text(&wrapped[0].pieces), "aaa");
        assert_eq!(text(&wrapped[1].pieces), "bbcc");
        assert_eq!(wrapped[1].pieces.len(), 2);
        assert_eq!(wrapped[1].source, Some(4));
    }

    #[test]
    fn each_line_knows_where_its_first_word_is() {
        let atoms = [atom("alpha beta", 10), atom(" ", 20), atom("gamma", 21)];
        let wrapped = wrap(&atoms, 6);
        let sources: Vec<_> = wrapped.iter().map(|line| line.source).collect();
        assert_eq!(sources, [Some(10), Some(16), Some(21)]);
    }

    #[test]
    fn widths() {
        let atoms = [atom("ab cdef g", 0), atom(BREAK, 9), atom("hijklmnopq", 10)];
        assert_eq!(natural_width(&atoms), 10);
        assert_eq!(widest_word(&atoms), 10);
        assert_eq!(natural_width(&[]), 0);
    }
}
