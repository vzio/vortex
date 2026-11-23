//! From Markdown to blocks: pulldown-cmark's events, with their places in the source, gathered
//! into a tree of blocks whose text is already styled. The layout ([`super::page`]) lays the
//! tree out at a width.

use std::collections::HashMap;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, LinkType, MetadataBlockKind, Options, Parser,
    Tag, TagEnd,
};
use vortex_core::syntax;
use vortex_view::{graphics::Style, theme::Modifier, Theme};

/// The styles of a page, from the theme.
#[derive(Debug, Clone)]
pub struct Styles {
    pub text: Style,
    pub headings: [Style; 6],
    pub bold: Style,
    pub italic: Style,
    pub strike: Style,
    pub code: Style,
    pub code_block: Style,
    pub link_text: Style,
    pub link_url: Style,
    pub link_label: Style,
    pub bullet: Style,
    pub number: Style,
    pub checked: Style,
    pub unchecked: Style,
    pub quote: Style,
    pub border: Style,
    pub dim: Style,
}

impl Styles {
    pub fn new(theme: &Theme) -> Self {
        let text = theme.get("ui.text");
        let heading = |level: usize| text.patch(theme.get(&format!("markup.heading.{level}")));
        Self {
            text,
            headings: [
                heading(1),
                heading(2),
                heading(3),
                heading(4),
                heading(5),
                heading(6),
            ],
            bold: theme
                .try_get("markup.bold")
                .unwrap_or_else(|| Style::default().add_modifier(Modifier::BOLD)),
            italic: theme
                .try_get("markup.italic")
                .unwrap_or_else(|| Style::default().add_modifier(Modifier::ITALIC)),
            strike: theme
                .try_get("markup.strikethrough")
                .unwrap_or_else(|| Style::default().add_modifier(Modifier::CROSSED_OUT)),
            code: theme.get("markup.raw.inline"),
            code_block: text.patch(theme.get("markup.raw.block")),
            link_text: theme.get("markup.link.text"),
            link_url: theme.get("markup.link.url"),
            link_label: theme.get_or("markup.link.label", "markup.link.text"),
            bullet: theme.get("markup.list.unnumbered"),
            number: theme.get("markup.list.numbered"),
            checked: theme.get("markup.list.checked"),
            unchecked: theme.get("markup.list.unchecked"),
            quote: theme.get("markup.quote"),
            border: theme.get_or("ui.markdown.border", "ui.virtual"),
            dim: theme.get_or("ui.markdown.dim", "ui.virtual"),
        }
    }
}

/// A run of text in one style: the unit of inline content.
#[derive(Debug, Clone, PartialEq)]
pub struct Atom {
    /// The text; [`BREAK`] for a hard line break.
    pub text: String,
    pub style: Style,
    /// The byte in the source where the text starts.
    pub source: usize,
    /// The link it belongs to: an index into [`Parsed::links`].
    pub link: Option<usize>,
}

/// The text of a hard line break.
pub const BREAK: &str = "\n";

/// A block of the page and the byte in the source where it starts.
#[derive(Debug, Clone)]
pub struct Node {
    pub block: Block,
    pub source: usize,
}

#[derive(Debug, Clone)]
pub enum Block {
    Paragraph(Vec<Atom>),
    Heading {
        /// 1 to 6.
        level: usize,
        atoms: Vec<Atom>,
        anchor: String,
    },
    /// A code block, highlighted: its lines, and the byte where its first line starts.
    Code {
        lines: Vec<Vec<(String, Style)>>,
        first: usize,
    },
    /// Raw HTML, shown as it is (dimmed); the byte where it starts.
    Html {
        lines: Vec<String>,
        first: usize,
    },
    Quote {
        kind: Option<BlockQuoteKind>,
        blocks: Vec<Node>,
    },
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<Item>,
    },
    Rule,
    Table {
        aligns: Vec<Alignment>,
        head: Vec<Vec<Atom>>,
        rows: Vec<Vec<Vec<Atom>>>,
    },
}

#[derive(Debug, Clone)]
pub struct Item {
    /// A task list item: checked or not.
    pub task: Option<bool>,
    pub blocks: Vec<Node>,
    pub source: usize,
}

/// A footnote's definition, listed at the end of the page.
#[derive(Debug, Clone)]
pub struct Footnote {
    pub label: String,
    /// Its number: in the order of the first references, then of the definitions.
    pub number: usize,
    pub blocks: Vec<Node>,
    pub source: usize,
}

/// Where a link goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// As written: a web address, a path, a `#heading`.
    Url(String),
    /// The footnote of this label.
    Footnote(String),
    /// Back from a footnote to its first reference.
    FootnoteBack(String),
}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub blocks: Vec<Node>,
    pub footnotes: Vec<Footnote>,
    pub links: Vec<Target>,
}

pub fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

/// GitHub's anchor of a heading: lowercase, letters, digits, `-` and `_` kept, spaces made `-`,
/// everything else dropped.
pub fn slug(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// An open container of blocks.
enum Container {
    Root(Vec<Node>),
    Quote {
        kind: Option<BlockQuoteKind>,
        blocks: Vec<Node>,
        source: usize,
    },
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<Item>,
        source: usize,
    },
    Item(Item),
    Footnote {
        label: String,
        blocks: Vec<Node>,
        source: usize,
    },
}

/// What the inline content being gathered becomes.
enum Inline {
    Paragraph,
    Heading { level: usize, id: Option<String> },
    Cell,
}

struct Table {
    aligns: Vec<Alignment>,
    head: Vec<Vec<Atom>>,
    rows: Vec<Vec<Vec<Atom>>>,
    row: Vec<Vec<Atom>>,
    source: usize,
}

struct Builder<'a> {
    styles: &'a Styles,
    theme: &'a Theme,
    loader: &'a syntax::Loader,
    stack: Vec<Container>,
    inline: Option<(Inline, Vec<Atom>, usize)>,
    /// The base style of the text (a heading's, a quote's...), innermost last.
    bases: Vec<Style>,
    /// The inline styles open (emphasis, strong...), innermost last.
    spans: Vec<Style>,
    /// The links open: their index, their destination, whether the destination is shown.
    open_links: Vec<(usize, String, bool)>,
    image: Option<(String, String)>,
    code: Option<(String, String, Option<usize>, usize)>,
    html: Option<(String, usize)>,
    table: Option<Table>,
    links: Vec<Target>,
    footnotes: Vec<Footnote>,
    footnote_numbers: HashMap<String, usize>,
    anchors: HashMap<String, usize>,
}

/// Parses `source` into blocks, styled with `styles`, code highlighted with `theme` and `loader`.
pub fn parse(source: &str, styles: &Styles, theme: &Theme, loader: &syntax::Loader) -> Parsed {
    let mut builder = Builder {
        styles,
        theme,
        loader,
        stack: vec![Container::Root(Vec::new())],
        inline: None,
        bases: vec![styles.text],
        spans: Vec::new(),
        open_links: Vec::new(),
        image: None,
        code: None,
        html: None,
        table: None,
        links: Vec::new(),
        footnotes: Vec::new(),
        footnote_numbers: HashMap::new(),
        anchors: HashMap::new(),
    };
    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        builder.event(event, range);
    }
    builder.finish()
}

impl Builder<'_> {
    fn style(&self) -> Style {
        let base = *self.bases.last().unwrap_or(&self.styles.text);
        self.spans
            .iter()
            .fold(base, |style, span| style.patch(*span))
    }

    fn link(&self) -> Option<usize> {
        self.open_links.last().map(|(index, _, _)| *index)
    }

    fn push_atom(&mut self, text: String, style: Style, source: usize) {
        let link = self.link();
        self.ensure_inline(source);
        if let Some((_, atoms, _)) = &mut self.inline {
            atoms.push(Atom {
                text,
                style,
                source,
                link,
            });
        }
    }

    /// Text outside a paragraph is the text of a tight list's item: an implicit paragraph.
    fn ensure_inline(&mut self, source: usize) {
        if self.inline.is_none() {
            self.inline = Some((Inline::Paragraph, Vec::new(), source));
        }
    }

    /// Ends the implicit paragraph of a tight list's item, if one is open.
    fn flush_inline(&mut self) {
        if let Some((Inline::Paragraph, atoms, source)) = self.inline.take() {
            self.push_block(Node {
                block: Block::Paragraph(atoms),
                source,
            });
        }
    }

    fn push_block(&mut self, node: Node) {
        match self.stack.last_mut() {
            Some(Container::Root(blocks))
            | Some(Container::Quote { blocks, .. })
            | Some(Container::Footnote { blocks, .. })
            | Some(Container::Item(Item { blocks, .. })) => blocks.push(node),
            // (a list holds items only)
            Some(Container::List { .. }) | None => {}
        }
    }

    fn event(&mut self, event: Event, range: std::ops::Range<usize>) {
        let at = range.start;
        if let Some((_, text, first, _)) = &mut self.code {
            match event {
                Event::Text(t) => {
                    first.get_or_insert(at);
                    text.push_str(&t);
                }
                Event::End(TagEnd::CodeBlock) | Event::End(TagEnd::MetadataBlock(_)) => {
                    self.end_code()
                }
                _ => {}
            }
            return;
        }
        if let Some((text, _)) = &mut self.html {
            match event {
                Event::Html(t) | Event::Text(t) => text.push_str(&t),
                Event::End(TagEnd::HtmlBlock) => {
                    let (text, first) = self.html.take().unwrap();
                    let lines = text
                        .lines()
                        .map(|line| line.replace('\t', "    "))
                        .collect();
                    self.push_block(Node {
                        block: Block::Html { lines, first },
                        source: first,
                    });
                }
                _ => {}
            }
            return;
        }
        if let Some((alt, _)) = &mut self.image {
            match event {
                Event::Text(t) | Event::Code(t) => alt.push_str(&t),
                Event::End(TagEnd::Image) => {
                    let (alt, dest) = self.image.take().unwrap();
                    let shown = if alt.trim().is_empty() { dest } else { alt };
                    let style = self.style().patch(self.styles.dim);
                    self.push_atom(format!("[image: {shown}]"), style, at);
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, at),
            Event::End(tag) => self.end(tag, at),
            Event::Text(text) => {
                let style = self.style();
                self.push_atom(text.to_string(), style, at);
            }
            Event::Code(text) | Event::InlineMath(text) => {
                let style = self.style().patch(self.styles.code);
                self.push_atom(text.to_string(), style, at);
            }
            Event::DisplayMath(text) => {
                let style = self.style().patch(self.styles.code);
                self.push_atom(BREAK.into(), style, at);
                for (i, line) in text.trim().lines().enumerate() {
                    if i > 0 {
                        self.push_atom(BREAK.into(), style, at);
                    }
                    self.push_atom(line.to_string(), style, at);
                }
                self.push_atom(BREAK.into(), style, at);
            }
            Event::InlineHtml(html) => {
                let tag = html.trim().to_ascii_lowercase();
                if matches!(tag.as_str(), "<br>" | "<br/>" | "<br />") {
                    let style = self.style();
                    self.push_atom(BREAK.into(), style, at);
                } else {
                    let style = self.style().patch(self.styles.dim);
                    self.push_atom(html.to_string(), style, at);
                }
            }
            Event::Html(html) => {
                // (outside an HTML block: shown as it is)
                let style = self.style().patch(self.styles.dim);
                self.push_atom(html.trim_end().to_string(), style, at);
            }
            Event::FootnoteReference(label) => {
                let number = self.footnote_number(&label);
                let index = self.links.len();
                self.links.push(Target::Footnote(label.to_string()));
                self.ensure_inline(at);
                if let Some((_, atoms, _)) = &mut self.inline {
                    atoms.push(Atom {
                        text: format!("[{number}]"),
                        style: self.styles.link_label,
                        source: at,
                        link: Some(index),
                    });
                }
            }
            Event::SoftBreak => {
                let style = self.style();
                self.push_atom(" ".into(), style, at);
            }
            Event::HardBreak => {
                let style = self.style();
                self.push_atom(BREAK.into(), style, at);
            }
            Event::Rule => {
                self.flush_inline();
                self.push_block(Node {
                    block: Block::Rule,
                    source: at,
                });
            }
            Event::TaskListMarker(checked) => {
                if let Some(Container::Item(item)) = self.stack.last_mut() {
                    item.task = Some(checked);
                }
            }
        }
    }

    fn footnote_number(&mut self, label: &str) -> usize {
        let next = self.footnote_numbers.len() + 1;
        *self
            .footnote_numbers
            .entry(label.to_string())
            .or_insert(next)
    }

    fn start(&mut self, tag: Tag, at: usize) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                // an explicit paragraph in a list's item: the list is loose
                if let Some(Container::Item(_)) = self.stack.last() {
                    if let Some(Container::List { tight, .. }) = self.stack.iter_mut().rev().nth(1)
                    {
                        *tight = false;
                    }
                }
                self.inline = Some((Inline::Paragraph, Vec::new(), at));
            }
            Tag::Heading { level, id, .. } => {
                self.flush_inline();
                let level = level as usize;
                self.bases.push(self.styles.headings[level - 1]);
                self.inline = Some((
                    Inline::Heading {
                        level,
                        id: id.map(|id| id.to_string()),
                    },
                    Vec::new(),
                    at,
                ));
            }
            Tag::BlockQuote(kind) => {
                self.flush_inline();
                let base = self.style().patch(self.styles.quote);
                self.bases.push(base);
                self.stack.push(Container::Quote {
                    kind,
                    blocks: Vec::new(),
                    source: at,
                });
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((language, String::new(), None, at));
            }
            Tag::MetadataBlock(kind) => {
                self.flush_inline();
                let language = match kind {
                    MetadataBlockKind::YamlStyle => "yaml",
                    MetadataBlockKind::PlusesStyle => "toml",
                };
                self.code = Some((language.to_string(), String::new(), None, at));
            }
            Tag::HtmlBlock => {
                self.flush_inline();
                self.html = Some((String::new(), at));
            }
            Tag::List(start) => {
                self.flush_inline();
                self.stack.push(Container::List {
                    start,
                    tight: true,
                    items: Vec::new(),
                    source: at,
                });
            }
            Tag::Item => {
                self.stack.push(Container::Item(Item {
                    task: None,
                    blocks: Vec::new(),
                    source: at,
                }));
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_inline();
                self.bases.push(self.styles.text);
                self.stack.push(Container::Footnote {
                    label: label.to_string(),
                    blocks: Vec::new(),
                    source: at,
                });
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.table = Some(Table {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    source: at,
                });
            }
            Tag::TableHead => {
                let base = self.style().patch(self.styles.bold);
                self.bases.push(base);
            }
            Tag::TableRow => {}
            Tag::TableCell => self.inline = Some((Inline::Cell, Vec::new(), at)),
            Tag::Emphasis => self.spans.push(self.styles.italic),
            Tag::Strong => self.spans.push(self.styles.bold),
            Tag::Strikethrough => self.spans.push(self.styles.strike),
            Tag::Superscript | Tag::Subscript => self.spans.push(Style::default()),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let index = self.links.len();
                self.links.push(Target::Url(dest_url.to_string()));
                // an autolink shows its address already; a link to a heading of the page needs none
                let shown = !matches!(link_type, LinkType::Autolink | LinkType::Email)
                    && !dest_url.starts_with('#');
                self.open_links.push((index, dest_url.to_string(), shown));
                self.spans.push(self.styles.link_text);
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some((String::new(), dest_url.to_string()));
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {}
        }
    }

    fn end(&mut self, tag: TagEnd, at: usize) {
        match tag {
            TagEnd::Paragraph => {
                if let Some((_, atoms, source)) = self.inline.take() {
                    self.push_block(Node {
                        block: Block::Paragraph(atoms),
                        source,
                    });
                }
            }
            TagEnd::Heading(_) => {
                self.bases.pop();
                if let Some((Inline::Heading { level, id }, atoms, source)) = self.inline.take() {
                    let text: String = atoms
                        .iter()
                        .filter(|atom| atom.text != BREAK)
                        .map(|atom| atom.text.as_str())
                        .collect();
                    let anchor = self.anchor(id.unwrap_or_else(|| slug(&text)));
                    self.push_block(Node {
                        block: Block::Heading {
                            level,
                            atoms,
                            anchor,
                        },
                        source,
                    });
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.bases.pop();
                if let Some(Container::Quote {
                    kind,
                    blocks,
                    source,
                }) = self.stack.pop()
                {
                    self.push_block(Node {
                        block: Block::Quote { kind, blocks },
                        source,
                    });
                }
            }
            TagEnd::List(_) => {
                self.flush_inline();
                if let Some(Container::List {
                    start,
                    tight,
                    items,
                    source,
                }) = self.stack.pop()
                {
                    self.push_block(Node {
                        block: Block::List {
                            start,
                            tight,
                            items,
                        },
                        source,
                    });
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some(Container::Item(item)) = self.stack.pop() {
                    if let Some(Container::List { items, .. }) = self.stack.last_mut() {
                        items.push(item);
                    }
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.bases.pop();
                if let Some(Container::Footnote {
                    label,
                    blocks,
                    source,
                }) = self.stack.pop()
                {
                    let number = self.footnote_number(&label);
                    self.footnotes.push(Footnote {
                        label,
                        number,
                        blocks,
                        source,
                    });
                }
            }
            TagEnd::TableHead => {
                self.bases.pop();
                if let Some(table) = &mut self.table {
                    table.head = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some((Inline::Cell, atoms, _)) = self.inline.take() {
                    if let Some(table) = &mut self.table {
                        table.row.push(atoms);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(Node {
                        block: Block::Table {
                            aligns: table.aligns,
                            head: table.head,
                            rows: table.rows,
                        },
                        source: table.source,
                    });
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.spans.pop();
            }
            TagEnd::Link => {
                self.spans.pop();
                if let Some((index, dest, shown)) = self.open_links.pop() {
                    if shown {
                        let style = self.style();
                        self.push_atom(" ".into(), style, at);
                        if let Some((_, atoms, _)) = &mut self.inline {
                            atoms.push(Atom {
                                text: format!("({dest})"),
                                style: self.styles.link_url,
                                source: at,
                                link: Some(index),
                            });
                        }
                    }
                }
            }
            TagEnd::CodeBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::HtmlBlock
            | TagEnd::Image
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => {}
        }
    }

    /// A heading's anchor, unique in the page as GitHub makes it: `-1`, `-2`... after repeats.
    fn anchor(&mut self, anchor: String) -> String {
        let seen = self.anchors.entry(anchor.clone()).or_insert(0);
        let unique = if *seen == 0 {
            anchor
        } else {
            format!("{anchor}-{seen}")
        };
        *seen += 1;
        unique
    }

    fn end_code(&mut self) {
        let Some((language, text, first, start)) = self.code.take() else {
            return;
        };
        let first = first.unwrap_or(start);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let base = self.styles.code_block;
        let lines = if language.is_empty() {
            text.lines()
                .map(|line| vec![(line.replace('\t', "    "), base)])
                .collect()
        } else {
            let highlighted = crate::ui::markdown::highlighted_code_block(
                text,
                &language,
                Some(self.theme),
                self.loader,
                None,
            );
            highlighted
                .lines
                .into_iter()
                .map(|spans| {
                    spans
                        .0
                        .into_iter()
                        .map(|span| (span.content.into_owned(), base.patch(span.style)))
                        .collect()
                })
                .collect()
        };
        self.push_block(Node {
            block: Block::Code { lines, first },
            source: start,
        });
    }

    fn finish(mut self) -> Parsed {
        self.flush_inline();
        // footnotes without references come last, in their order
        let mut footnotes = std::mem::take(&mut self.footnotes);
        footnotes.sort_by_key(|footnote| footnote.number);
        let blocks = match self.stack.into_iter().next() {
            Some(Container::Root(blocks)) => blocks,
            _ => Vec::new(),
        };
        Parsed {
            blocks,
            footnotes,
            links: self.links,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_githubs() {
        assert_eq!(slug("Getting Started"), "getting-started");
        assert_eq!(slug("API: `open()` & friends!"), "api-open--friends");
        assert_eq!(slug("Ünïcode 2.0"), "ünïcode-20");
        assert_eq!(slug("snake_case-and-dash"), "snake_case-and-dash");
    }
}
