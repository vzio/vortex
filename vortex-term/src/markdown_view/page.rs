//! The rendered page: the blocks of [`super::parse`] laid out at a width, as lines of styled
//! pieces, each knowing the line of the source it comes from, with the page's headings and links.

use std::collections::HashMap;

use pulldown_cmark::BlockQuoteKind;
use vortex_core::syntax;
use vortex_core::unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr};
use vortex_view::{graphics::Style, Theme};

use super::parse::{self, Block, Node, Styles, Target};
use super::table;
use super::wrap::{self, push_piece, Piece};

/// A line of the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// From the page's first column.
    pub pieces: Vec<Piece>,
    /// A background from this column to the page's right edge: a heading's band, a code block.
    pub fill: Option<(usize, Style)>,
    /// The line of the source it comes from.
    pub source: usize,
    /// The code block it is a line of: an index into [`Page::code_blocks`].
    pub code: Option<usize>,
}

impl Line {
    pub fn text(&self) -> String {
        wrap::text(&self.pieces)
    }

    /// The links on the line: their columns `start..end` and their index.
    pub fn links(&self) -> Vec<(usize, usize, usize)> {
        let mut links: Vec<(usize, usize, usize)> = Vec::new();
        let mut column = 0;
        for piece in &self.pieces {
            let width = piece.width();
            if let Some(link) = piece.link {
                match links.last_mut() {
                    Some(last) if last.2 == link && last.1 == column => last.1 += width,
                    _ => links.push((column, column + width, link)),
                }
            }
            column += width;
        }
        links
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// Its first line on the page.
    pub line: usize,
    pub level: usize,
    pub text: String,
    pub anchor: String,
}

/// A code block on the page, which scrolls sideways on its own when its lines are cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    /// Its lines on the page.
    pub lines: std::ops::Range<usize>,
    /// The column where its code starts (after its left padding).
    pub column: usize,
    /// Its widest line.
    pub widest: usize,
    /// How much of a line shows: the code's width, less a cell at each side for the markers
    /// when lines are cut.
    pub visible: usize,
}

impl CodeBlock {
    /// Whether its lines are cut: it scrolls.
    pub fn scrolls(&self) -> bool {
        self.widest > self.visible
    }

    /// The furthest it scrolls.
    pub fn max_offset(&self) -> usize {
        self.widest.saturating_sub(self.visible)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub lines: Vec<Line>,
    pub headings: Vec<Heading>,
    pub links: Vec<Target>,
    pub code_blocks: Vec<CodeBlock>,
    /// The width it was laid out at.
    pub width: usize,
    /// Where the footnotes start (the end of the body).
    pub body_end: usize,
    footnote_lines: HashMap<String, usize>,
}

impl Page {
    /// Parses and lays out `source` at `width`.
    pub fn new(source: &str, width: usize, theme: &Theme, loader: &syntax::Loader) -> Self {
        let styles = Styles::new(theme);
        let parsed = parse::parse(source, &styles, theme, loader);
        Self::lay_out(source, parsed, width, &styles)
    }

    fn lay_out(source: &str, parsed: parse::Parsed, width: usize, styles: &Styles) -> Self {
        let mut layout = Layout {
            lines: Vec::new(),
            prefixes: Vec::new(),
            width: width.max(1),
            starts: line_starts(source),
            styles,
            headings: Vec::new(),
            list_depth: 0,
            code_blocks: Vec::new(),
        };
        layout.blocks(&parsed.blocks, false);
        let body_end = layout.lines.len();
        let mut links = parsed.links;
        let mut footnote_lines = HashMap::new();
        if !parsed.footnotes.is_empty() {
            let at = parsed.footnotes[0].source;
            if body_end > 0 {
                layout.blank(at);
            }
            let rule = "─".repeat(layout.width.min(20));
            layout.emit(vec![Piece::new(rule, styles.border)], None, at);
            for footnote in &parsed.footnotes {
                footnote_lines.insert(footnote.label.clone(), layout.lines.len());
                let back = links.len();
                links.push(Target::FootnoteBack(footnote.label.clone()));
                let label = format!("[{}]", footnote.number);
                let pad = " ".repeat(label.width() + 1);
                let mut first = Piece::new(label, styles.link_label);
                first.link = Some(back);
                layout.prefixes.push(Prefix::new(
                    vec![first, Piece::new(" ", Style::default())],
                    vec![Piece::new(pad, Style::default())],
                ));
                if footnote.blocks.is_empty() {
                    layout.emit(Vec::new(), None, footnote.source);
                }
                layout.blocks(&footnote.blocks, true);
                layout.prefixes.pop();
            }
        }
        Self {
            lines: layout.lines,
            headings: layout.headings,
            links,
            code_blocks: layout.code_blocks,
            width: layout.width,
            body_end,
            footnote_lines,
        }
    }

    /// The line a link goes to on this page: a heading or a footnote; `None` for a link
    /// elsewhere or to nothing.
    pub fn internal_target(&self, target: &Target) -> Option<usize> {
        match target {
            Target::Footnote(label) => self.footnote_lines.get(label).copied(),
            Target::FootnoteBack(label) => self.lines.iter().position(|line| {
                line.pieces.iter().any(|piece| {
                    piece
                        .link
                        .is_some_and(|link| self.links[link] == Target::Footnote(label.clone()))
                })
            }),
            Target::Url(url) => url.strip_prefix('#').and_then(|anchor| self.anchor(anchor)),
        }
    }

    /// The line of the heading whose anchor is `anchor` (as written, or as GitHub would make it).
    pub fn anchor(&self, anchor: &str) -> Option<usize> {
        let decoded = percent_decode(anchor);
        let wanted = [
            decoded.clone(),
            decoded.to_lowercase(),
            parse::slug(&decoded),
        ];
        self.headings
            .iter()
            .find(|heading| wanted.contains(&heading.anchor))
            .map(|heading| heading.line)
    }

    /// The line of the page that shows the source's line `source`: the first line from it, or
    /// else the last line of the body before it.
    pub fn line_of_source(&self, source: usize) -> usize {
        if let Some(line) = self.lines.iter().position(|line| line.source == source) {
            return line;
        }
        self.lines[..self.body_end]
            .iter()
            .rposition(|line| line.source < source)
            .unwrap_or(0)
    }

    /// The heading the line `line` is under.
    pub fn section(&self, line: usize) -> Option<&Heading> {
        self.headings
            .iter()
            .rev()
            .find(|heading| heading.line <= line)
    }
}

/// `%20` and the like decoded, for anchors and paths in links.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

fn line_starts(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

/// What a container (a list's item, a quote) puts before each of its lines.
struct Prefix {
    first: Vec<Piece>,
    rest: Vec<Piece>,
    used: bool,
}

impl Prefix {
    fn new(first: Vec<Piece>, rest: Vec<Piece>) -> Self {
        Self {
            first,
            rest,
            used: false,
        }
    }

    fn same(pieces: Vec<Piece>) -> Self {
        Self::new(pieces.clone(), pieces)
    }

    fn width(&self) -> usize {
        self.rest.iter().map(Piece::width).sum()
    }
}

struct Layout<'a> {
    lines: Vec<Line>,
    prefixes: Vec<Prefix>,
    width: usize,
    starts: Vec<usize>,
    styles: &'a Styles,
    headings: Vec<Heading>,
    /// The lists open around the block being laid out.
    list_depth: usize,
    code_blocks: Vec<CodeBlock>,
}

impl Layout<'_> {
    fn line_of(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1)
    }

    /// The width left for text inside the open containers.
    fn available(&self) -> usize {
        let taken: usize = self.prefixes.iter().map(Prefix::width).sum();
        self.width.saturating_sub(taken).max(1)
    }

    /// A line of `pieces` after the containers' prefixes, from the byte `byte` of the source.
    fn emit(&mut self, pieces: Vec<Piece>, fill: Option<Style>, byte: usize) {
        let source = self.line_of(byte);
        self.emit_at_line(pieces, fill, source);
    }

    fn emit_at_line(&mut self, pieces: Vec<Piece>, fill: Option<Style>, source: usize) {
        let mut line = Vec::new();
        let mut column = 0;
        for prefix in &mut self.prefixes {
            let shown = if prefix.used {
                &prefix.rest
            } else {
                &prefix.first
            };
            for piece in shown {
                column += piece.width();
                push_piece(&mut line, piece.clone());
            }
            prefix.used = true;
        }
        for piece in pieces {
            push_piece(&mut line, piece);
        }
        self.lines.push(Line {
            pieces: line,
            fill: fill.map(|style| (column, style)),
            source,
            code: None,
        });
    }

    /// An empty line between blocks: only the containers' bars.
    fn blank(&mut self, byte: usize) {
        let source = self.line_of(byte);
        let mut line = Vec::new();
        for prefix in &self.prefixes {
            for piece in &prefix.rest {
                push_piece(&mut line, piece.clone());
            }
        }
        // (no trailing spaces: only bars stay)
        while let Some(last) = line.last_mut() {
            let trimmed = last.text.trim_end().len();
            if trimmed == 0 {
                line.pop();
            } else {
                last.text.truncate(trimmed);
                break;
            }
        }
        self.lines.push(Line {
            pieces: line,
            fill: None,
            source,
            code: None,
        });
    }

    fn blocks(&mut self, nodes: &[Node], tight: bool) {
        for (i, node) in nodes.iter().enumerate() {
            if i > 0 && !tight {
                self.blank(node.source);
            }
            self.block(node);
        }
    }

    fn block(&mut self, node: &Node) {
        match &node.block {
            Block::Paragraph(atoms) => {
                for line in wrap::wrap(atoms, self.available()) {
                    self.emit(line.pieces, None, line.source.unwrap_or(node.source));
                }
            }
            Block::Heading {
                level,
                atoms,
                anchor,
            } => {
                let style = self.styles.headings[level - 1];
                let band = style.bg.is_some().then_some(style);
                let first = self.lines.len();
                let text = atoms
                    .iter()
                    .filter(|atom| atom.text != parse::BREAK)
                    .map(|atom| atom.text.as_str())
                    .collect::<String>();
                self.headings.push(Heading {
                    line: first,
                    level: *level,
                    text: text.split_whitespace().collect::<Vec<_>>().join(" "),
                    anchor: anchor.clone(),
                });
                for line in wrap::wrap(atoms, self.available()) {
                    self.emit(line.pieces, band, line.source.unwrap_or(node.source));
                }
            }
            Block::Code { lines, first } => {
                // never wrapped: the whole lines, which the view scrolls sideways block by block
                let first_line = self.line_of(*first);
                let available = self.available();
                let style = self.styles.code_block;
                let start = self.lines.len();
                let mut widest = 0;
                for (i, spans) in lines.iter().enumerate() {
                    let mut pieces = vec![Piece::new(" ", style)];
                    for (text, span_style) in spans {
                        push_piece(&mut pieces, Piece::new(text.as_str(), *span_style));
                    }
                    widest = widest.max(pieces.iter().map(Piece::width).sum::<usize>() - 1);
                    self.emit_at_line(pieces, Some(style), first_line + i);
                }
                if lines.is_empty() {
                    self.emit_at_line(vec![Piece::new(" ", style)], Some(style), first_line);
                }
                let column = self.lines[start].fill.map_or(0, |(column, _)| column) + 1;
                let block = self.code_blocks.len();
                for line in &mut self.lines[start..] {
                    line.code = Some(block);
                }
                self.code_blocks.push(CodeBlock {
                    lines: start..self.lines.len(),
                    column,
                    widest,
                    visible: if widest < available {
                        widest.max(1)
                    } else {
                        available.saturating_sub(2).max(1)
                    },
                });
            }
            Block::Html { lines, first } => {
                let first_line = self.line_of(*first);
                let width = self.available();
                for (i, text) in lines.iter().enumerate() {
                    let shown = truncate(text, width);
                    self.emit_at_line(
                        vec![Piece::new(shown, self.styles.dim)],
                        None,
                        first_line + i,
                    );
                }
            }
            Block::Quote { kind, blocks } => {
                self.prefixes
                    .push(Prefix::same(vec![Piece::new("▎ ", self.styles.border)]));
                if let Some(kind) = kind {
                    let name = match kind {
                        BlockQuoteKind::Note => "Note",
                        BlockQuoteKind::Tip => "Tip",
                        BlockQuoteKind::Important => "Important",
                        BlockQuoteKind::Warning => "Warning",
                        BlockQuoteKind::Caution => "Caution",
                    };
                    let style = self.styles.text.patch(self.styles.bold);
                    self.emit(vec![Piece::new(name, style)], None, node.source);
                }
                self.blocks(blocks, false);
                self.prefixes.pop();
            }
            Block::List {
                start,
                tight: list_tight,
                items,
            } => {
                let depth = self.list_depth;
                self.list_depth += 1;
                let last_number = start.map(|start| start + items.len().saturating_sub(1) as u64);
                let number_width = last_number.map_or(0, |n| n.to_string().len());
                for (i, item) in items.iter().enumerate() {
                    if i > 0 && !list_tight {
                        self.blank(item.source);
                    }
                    let mut first = Vec::new();
                    match start {
                        Some(start) => {
                            let number = format!("{}.", start + i as u64);
                            first.push(Piece::new(
                                format!("{number:>width$} ", width = number_width + 1),
                                self.styles.number,
                            ));
                        }
                        None if item.task.is_none() => {
                            let bullet = ["•", "◦", "▪"][depth % 3];
                            first.push(Piece::new(format!("{bullet} "), self.styles.bullet));
                        }
                        None => {}
                    }
                    if let Some(checked) = item.task {
                        let (mark, style) = if checked {
                            ("☑ ", self.styles.checked)
                        } else {
                            ("☐ ", self.styles.unchecked)
                        };
                        first.push(Piece::new(mark, style));
                    }
                    let width: usize = first.iter().map(Piece::width).sum();
                    self.prefixes.push(Prefix::new(
                        first,
                        vec![Piece::new(" ".repeat(width), Style::default())],
                    ));
                    if item.blocks.is_empty() {
                        self.emit(Vec::new(), None, item.source);
                    }
                    self.blocks(&item.blocks, *list_tight);
                    self.prefixes.pop();
                }
                self.list_depth -= 1;
            }
            Block::Rule => {
                let rule = "─".repeat(self.available());
                self.emit(
                    vec![Piece::new(rule, self.styles.border)],
                    None,
                    node.source,
                );
            }
            Block::Table { aligns, head, rows } => {
                let width = self.available();
                for (pieces, byte) in
                    table::layout(aligns, head, rows, width, node.source, self.styles.border)
                {
                    self.emit(pieces, None, byte);
                }
            }
        }
    }
}

/// `text` cut to `width` cells, with `>` at the edge when it was cut.
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let w = grapheme.width();
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push_str(grapheme);
    }
    out.push('>');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page of `source` at `width` as text, for the tests.
    pub fn render(source: &str, width: usize) -> Vec<String> {
        let theme = Theme::default();
        let loader = vortex_core::config::default_lang_loader();
        Page::new(source, width, &theme, &loader)
            .lines
            .iter()
            .map(|line| line.text().trim_end().to_string())
            .collect()
    }

    #[test]
    #[ignore]
    fn show() {
        let source = std::fs::read_to_string(std::env::var("MD").unwrap()).unwrap();
        let width = std::env::var("W").map_or(60, |w| w.parse().unwrap());
        for line in render(&source, width) {
            println!("|{line}");
        }
    }
}
