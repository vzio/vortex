//! Selective soft wrap (this fork): with soft wrap on, the lines of some syntax nodes (a language's
//! `soft-wrap.except`, Markdown's tables and code blocks) don't wrap. They stay one visual line
//! each, extend past the right edge, and share the view's sideways offset, while the other lines
//! wrap to the window and always start at its left edge.
//!
//! [`UnwrappedLines`] is the set of document lines that don't wrap, found in the syntax tree by
//! [`unwrapped_lines`]. The [`TextFormat`](crate::doc_formatter::TextFormat) carries it: the
//! formatter asks it at each line start, and everything that maps screen cells to text and back
//! asks `TextFormat::col_offset` for a line's sideways offset.

use std::{ops::Range, sync::Arc};

use ropey::RopeSlice;

use crate::{doc_formatter::TextFormat, Syntax};

/// The document lines that don't soft wrap: sorted, disjoint, non-adjacent ranges of line indices.
/// Cloning is cheap (an `Arc`), and the empty set doesn't allocate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnwrappedLines(Option<Arc<[Range<usize>]>>);

impl UnwrappedLines {
    /// The set of the lines of `ranges`, in any order, overlapping or not.
    pub fn new(mut ranges: Vec<Range<usize>>) -> Self {
        ranges.retain(|range| !range.is_empty());
        ranges.sort_unstable_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        Self((!merged.is_empty()).then(|| merged.into()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Whether `line` doesn't wrap.
    pub fn contains(&self, line: usize) -> bool {
        // the first range ending after `line` is the only one that can hold it
        let ranges = self.ranges();
        let i = ranges.partition_point(|range| range.end <= line);
        ranges.get(i).is_some_and(|range| range.start <= line)
    }

    /// The ranges, sorted.
    pub fn ranges(&self) -> &[Range<usize>] {
        self.0.as_deref().unwrap_or_default()
    }
}

/// The sideways offset of each line of a view scrolled `horizontal` columns sideways
/// ([`TextFormat::col_offset`]), for code that has no `TextFormat` at hand. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct LineOffsets {
    horizontal: usize,
    soft_wrap: bool,
    unwrapped: UnwrappedLines,
}

impl LineOffsets {
    pub fn new(text_fmt: &TextFormat, horizontal: usize) -> Self {
        Self {
            horizontal,
            soft_wrap: text_fmt.soft_wrap,
            unwrapped: text_fmt.unwrapped.clone(),
        }
    }

    /// The sideways offset of the visual lines of document line `line`.
    pub fn of_line(&self, line: usize) -> usize {
        if self.horizontal == 0 || (self.soft_wrap && !self.unwrapped.contains(line)) {
            0
        } else {
            self.horizontal
        }
    }
}

/// The lines of the nodes of the root layer of `syntax` whose kind is one of `kinds`: every line a
/// node touches, from the line of its first byte to the line of its last. Nodes inside a listed
/// node aren't visited. Injected languages aren't looked at (their nodes use their own names).
pub fn unwrapped_lines(syntax: &Syntax, text: RopeSlice, kinds: &[String]) -> UnwrappedLines {
    if kinds.is_empty() {
        return UnwrappedLines::default();
    }
    let len_bytes = text.len_bytes();
    let line_of = |byte: u32| text.byte_to_line((byte as usize).min(len_bytes));
    let mut ranges = Vec::new();
    let mut cursor = syntax.tree().walk();
    loop {
        let node = cursor.node();
        let listed = node.is_named() && kinds.iter().any(|kind| kind == node.kind());
        if listed && node.end_byte() > node.start_byte() {
            let first = line_of(node.start_byte());
            let last = line_of(node.end_byte() - 1);
            ranges.push(first..last + 1);
        }
        if !listed && cursor.goto_first_child() {
            continue;
        }
        // the next node: a sibling, or the sibling of the nearest ancestor that has one
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return UnwrappedLines::new(ranges);
            }
        }
    }
}

#[cfg(test)]
// the line ranges are lists, one range or more
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_merged_and_looked_up() {
        let lines = UnwrappedLines::new(vec![8..10, 2..4, 3..5, 5..6, 7..7]);
        assert_eq!(lines.ranges(), [2..6, 8..10]);
        let inside: Vec<usize> = (0..12).filter(|&line| lines.contains(line)).collect();
        assert_eq!(inside, [2, 3, 4, 5, 8, 9]);
        assert!(!UnwrappedLines::default().contains(0));
        assert!(UnwrappedLines::default().is_empty());
    }

    use std::sync::LazyLock;

    use crate::{syntax::Loader, Rope, Transaction};

    static LOADER: LazyLock<Loader> = LazyLock::new(crate::config::default_lang_loader);

    fn markdown_kinds() -> Vec<String> {
        // Markdown's defaults in languages.toml
        [
            "pipe_table",
            "fenced_code_block",
            "indented_code_block",
            "html_block",
            "minus_metadata",
            "plus_metadata",
        ]
        .map(String::from)
        .to_vec()
    }

    fn markdown(text: &Rope) -> Syntax {
        let language = LOADER.language_for_name("markdown").unwrap();
        Syntax::new(text.slice(..), language, &LOADER).unwrap()
    }

    fn unwrapped(text: &str) -> Vec<Range<usize>> {
        let text = Rope::from_str(text);
        unwrapped_lines(&markdown(&text), text.slice(..), &markdown_kinds())
            .ranges()
            .to_vec()
    }

    const TABLE: &str = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    const FENCE: &str = "```rust\nfn main() {}\n```\n";

    #[test]
    fn tables_and_code_blocks_at_the_start_middle_and_end() {
        // at the start
        assert_eq!(unwrapped(&format!("{TABLE}\nSome prose.\n")), [0..3]);
        // in the middle
        assert_eq!(
            unwrapped(&format!("# Title\n\n{FENCE}\nMore prose.\n")),
            [2..5]
        );
        // at the end, without a final newline
        assert_eq!(
            unwrapped(&format!("Prose.\n\n{}", TABLE.trim_end())),
            [2..5]
        );
        // an indented code block, front matter and HTML
        assert_eq!(
            unwrapped(
                "---\ntitle: x\n---\n\nProse.\n\n    let x = 1;\n\n<div>\n  <p>hi</p>\n</div>\n"
            ),
            // the indented code block takes the blank line after it
            [0..3, 6..11]
        );
        // prose and lists wrap
        assert!(unwrapped("# Title\n\nSome prose.\n\n- a list\n- of items\n").is_empty());
    }

    #[test]
    fn a_table_in_a_list_or_a_quote() {
        assert_eq!(
            unwrapped(&format!("> quoted\n>\n> {}", "| a | b |\n> |---|---|\n")),
            [2..4]
        );
        assert_eq!(
            unwrapped("- item\n\n  ```\n  code\n  ```\n- next\n"),
            [2..5]
        );
    }

    #[test]
    fn edits_create_and_remove_a_table() {
        let mut text = Rope::from_str("Prose.\n\n| a | b |\n\nMore.\n");
        let mut syntax = markdown(&text);
        let lines = |syntax: &Syntax, text: &Rope| {
            unwrapped_lines(syntax, text.slice(..), &markdown_kinds())
                .ranges()
                .to_vec()
        };
        // a lone row isn't a table yet
        assert!(lines(&syntax, &text).is_empty());
        let edit = |syntax: &mut Syntax,
                    text: &mut Rope,
                    at: usize,
                    insert: Option<&str>,
                    delete: usize| {
            let old = text.clone();
            let transaction = Transaction::change(
                text,
                [(at, at + delete, insert.map(Into::into))].into_iter(),
            );
            transaction.apply(text);
            syntax
                .update(
                    old.slice(..),
                    text.slice(..),
                    transaction.changes(),
                    &LOADER,
                )
                .unwrap();
        };
        // the delimiter row makes it one
        let after_row = text.line_to_char(3);
        edit(&mut syntax, &mut text, after_row, Some("|---|---|\n"), 0);
        assert_eq!(lines(&syntax, &text), [2..4]);
        // a new row joins it
        let after_delimiter = text.line_to_char(4);
        edit(
            &mut syntax,
            &mut text,
            after_delimiter,
            Some("| 1 | 2 |\n"),
            0,
        );
        assert_eq!(lines(&syntax, &text), [2..5]);
        // deleting the delimiter row makes it prose again
        let delimiter = text.line_to_char(3);
        edit(&mut syntax, &mut text, delimiter, None, "|---|---|\n".len());
        assert!(lines(&syntax, &text).is_empty());
    }
}
