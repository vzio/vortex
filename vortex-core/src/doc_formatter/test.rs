use crate::doc_formatter::{DocumentFormatter, TextFormat};
use crate::text_annotations::{InlineAnnotation, Overlay, TextAnnotations};

impl TextFormat {
    fn new_test(softwrap: bool) -> Self {
        TextFormat {
            soft_wrap: softwrap,
            tab_width: 2,
            max_wrap: 3,
            max_indent_retain: 4,
            wrap_indicator: ".".into(),
            wrap_indicator_highlight: None,
            // use a prime number to allow lining up too often with repeat
            viewport_width: 17,
            soft_wrap_at_text_width: false,
            unwrapped: Default::default(),
        }
    }
}

impl<'t> DocumentFormatter<'t> {
    fn collect_to_str(&mut self) -> String {
        use std::fmt::Write;
        let mut res = String::new();
        let viewport_width = self.text_fmt.viewport_width;
        let soft_wrap_at_text_width = self.text_fmt.soft_wrap_at_text_width;
        let mut line = 0;

        for grapheme in self {
            if grapheme.visual_pos.row != line {
                line += 1;
                assert_eq!(grapheme.visual_pos.row, line);
                write!(res, "\n{}", ".".repeat(grapheme.visual_pos.col)).unwrap();
            }
            if !soft_wrap_at_text_width {
                assert!(
                    grapheme.visual_pos.col <= viewport_width as usize,
                    "softwrapped failed {}<={viewport_width}",
                    grapheme.visual_pos.col
                );
            }
            write!(res, "{}", grapheme.raw).unwrap();
        }

        res
    }
}

fn softwrap_text(text: &str) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(true),
        &TextAnnotations::default(),
        0,
    )
    .collect_to_str()
}

#[test]
fn basic_softwrap() {
    assert_eq!(
        softwrap_text(&"foo ".repeat(10)),
        "foo foo foo foo \n.foo foo foo foo \n.foo foo  "
    );
    assert_eq!(
        softwrap_text(&"fooo ".repeat(10)),
        "fooo fooo fooo \n.fooo fooo fooo \n.fooo fooo fooo \n.fooo  "
    );

    // check that we don't wrap unnecessarily
    assert_eq!(softwrap_text("\t\txxxx1xxxx2xx\n"), "    xxxx1xxxx2xx \n ");
}

#[test]
fn softwrap_indentation() {
    assert_eq!(
        softwrap_text("\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "    foo1 foo2 \n.....foo3 foo4 \n.....foo5 foo6 \n "
    );
    assert_eq!(
        softwrap_text("\t\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "      foo1 foo2 \n.foo3 foo4 foo5 \n.foo6 \n "
    );
}

#[test]
fn long_word_softwrap() {
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxx9xxx\n"),
        "    xxxx1xxxx2xxx\n.....x3xxxx4xxxx5\n.....xxxx6xxxx7xx\n.....xx8xxxx9xxx \n "
    );
    assert_eq!(
        softwrap_text("xxxxxxxx1xxxx2xxx\n"),
        "xxxxxxxx1xxxx2xxx\n. \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx 2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxx9xxx\n"),
        "    xxxx1xxxx \n.....2xxxx3xxxx4x\n.....xxx5xxxx6xxx\n.....x7xxxx8xxxx9\n.....xxx \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxx 2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxx9xxx\n"),
        "    xxxx1xxx 2xxx\n.....x3xxxx4xxxx5\n.....xxxx6xxxx7xx\n.....xx8xxxx9xxx \n "
    );
}

#[test]
fn softwrap_multichar_grapheme() {
    assert_eq!(
        softwrap_text("xxxx xxxx xxx a\u{0301}bc\n"),
        "xxxx xxxx xxx \n.ábc \n "
    )
}

fn softwrap_text_at_text_width(text: &str) -> String {
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.soft_wrap_at_text_width = true;
    let annotations = TextAnnotations::default();
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.into(), &text_fmt, &annotations, 0);
    formatter.collect_to_str()
}
#[test]
fn long_word_softwrap_text_width() {
    assert_eq!(
        softwrap_text_at_text_width("xxxxxxxx1xxxx2xxx\nxxxxxxxx1xxxx2xxx"),
        "xxxxxxxx1xxxx2xxx \nxxxxxxxx1xxxx2xxx "
    );
}

fn overlay_text(text: &str, char_pos: usize, softwrap: bool, overlays: &[Overlay]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_overlay(overlays, None),
        char_pos,
    )
    .collect_to_str()
}

#[test]
fn overlay() {
    assert_eq!(
        overlay_text(
            "foobar",
            0,
            false,
            &[Overlay::new(0, "X"), Overlay::new(2, "\t")],
        ),
        "Xo  bar "
    );
    assert_eq!(
        overlay_text(
            &"foo ".repeat(10),
            0,
            true,
            &[
                Overlay::new(2, "\t"),
                Overlay::new(5, "\t"),
                Overlay::new(16, "X"),
            ]
        ),
        "fo   f  o foo \n.foo Xoo foo foo \n.foo foo foo  "
    );
}

fn annotate_text(text: &str, softwrap: bool, annotations: &[InlineAnnotation]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_inline_annotations(annotations, None),
        0,
    )
    .collect_to_str()
}

#[test]
fn annotation() {
    assert_eq!(
        annotate_text("bar", false, &[InlineAnnotation::new(0, "foo")]),
        "foobar "
    );
    assert_eq!(
        annotate_text(
            &"foo ".repeat(10),
            true,
            &[InlineAnnotation::new(0, "foo ")]
        ),
        "foo foo foo foo \n.foo foo foo foo \n.foo foo foo  "
    );
}

#[test]
fn annotation_and_overlay() {
    let annotations = [InlineAnnotation {
        char_idx: 0,
        text: "fooo".into(),
    }];
    let overlay = [Overlay {
        char_idx: 0,
        grapheme: "\t".into(),
    }];
    assert_eq!(
        DocumentFormatter::new_at_prev_checkpoint(
            "bbar".into(),
            &TextFormat::new_test(false),
            TextAnnotations::default()
                .add_inline_annotations(annotations.as_slice(), None)
                .add_overlay(overlay.as_slice(), None),
            0,
        )
        .collect_to_str(),
        "fooo  bar "
    );
}

/// The visual lines of `text` with soft wrap on except on `unwrapped` lines (this fork: selective
/// soft wrap), each with its document line.
fn selective_rows(text: &str, unwrapped: Vec<std::ops::Range<usize>>) -> Vec<(usize, String)> {
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.unwrapped = crate::selective_wrap::UnwrappedLines::new(unwrapped);
    let annotations = TextAnnotations::default();
    let mut rows: Vec<(usize, String)> = Vec::new();
    for grapheme in
        DocumentFormatter::new_at_prev_checkpoint(text.into(), &text_fmt, &annotations, 0)
    {
        let row = grapheme.visual_pos.row;
        if rows.len() <= row {
            rows.resize(row + 1, (grapheme.line_idx, String::new()));
            rows[row].1 = ".".repeat(grapheme.visual_pos.col);
        }
        rows[row].1.push_str(&grapheme.raw.to_string());
    }
    rows.into_iter()
        .map(|(line, row)| (line, row.trim_end().to_string()))
        .collect()
}

const PROSE: &str = "the quick brown fox jumps over";
const ROW: &str = "| alpha | beta | gamma | delta |";

#[test]
fn unwrapped_lines_stay_one_visual_line() {
    let text = format!("{PROSE}\n{ROW}\n{ROW}\n{PROSE}\n");
    assert_eq!(
        selective_rows(&text, vec![1..3]),
        [
            (0, "the quick brown".into()),
            (0, ".fox jumps over".into()),
            (1, ROW.into()),
            (2, ROW.into()),
            (3, "the quick brown".into()),
            (3, ".fox jumps over".into()),
            (4, "".into()),
        ]
    );
}

#[test]
fn every_line_wraps_without_unwrapped_lines() {
    let text = format!("{PROSE}\n{ROW}\n");
    let rows = selective_rows(&text, vec![]);
    assert_eq!(rows.iter().filter(|(line, _)| *line == 1).count(), 2);
    assert!(rows.iter().all(|(_, row)| row.chars().count() <= 17));
}

#[test]
fn unwrapped_lines_at_the_start_and_the_end() {
    // at the start: the formatter starts on an unwrapped line
    let text = format!("{ROW}\n{PROSE}\n{ROW}");
    let rows = selective_rows(&text, vec![0..1, 2..3]);
    assert_eq!(rows[0], (0, ROW.into()));
    assert_eq!(rows[1], (1, "the quick brown".into()));
    assert_eq!(rows[2], (1, ".fox jumps over".into()));
    // at the end, without a final newline: the EOF cell follows the row
    assert_eq!(rows[3], (2, ROW.into()));
    assert_eq!(rows.len(), 4);
}

#[test]
fn a_line_that_wraps_after_an_unwrapped_one_wraps_as_usual() {
    // an indented line after an unwrapped one wraps exactly as after a line that wraps
    let indented = format!("  {PROSE}\n");
    let after_wrapped = selective_rows(&format!("x\n{indented}"), vec![]);
    let after_unwrapped = selective_rows(&format!("{ROW}\n{indented}"), vec![0..1]);
    assert_eq!(after_unwrapped[0], (0, ROW.into()));
    assert_eq!(after_unwrapped[1..], after_wrapped[1..]);
    assert_eq!(after_wrapped[2], (1, "... fox jumps".into()));
}

#[test]
fn starting_inside_unwrapped_lines() {
    // a formatter starting at a later block (as the renderer does at the view's anchor)
    let text = format!("{PROSE}\n{ROW}\n{ROW}\n{PROSE}\n");
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.unwrapped = crate::selective_wrap::UnwrappedLines::new(vec![1..3]);
    let annotations = TextAnnotations::default();
    let start = PROSE.len() + 1 + ROW.len() + 1; // the second row
    let rows: Vec<_> = DocumentFormatter::new_at_prev_checkpoint(
        text.as_str().into(),
        &text_fmt,
        &annotations,
        start,
    )
    .filter(|g| g.visual_pos.col == 0)
    .map(|g| (g.line_idx, g.visual_pos.row))
    .collect();
    assert_eq!(rows, [(2, 0), (3, 1), (3, 2), (4, 3)]);
}

#[test]
fn col_offset_applies_to_the_lines_that_dont_wrap() {
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.unwrapped = crate::selective_wrap::UnwrappedLines::new(vec![1..3]);
    assert_eq!(text_fmt.col_offset(0, 7), 0);
    assert_eq!(text_fmt.col_offset(1, 7), 7);
    assert_eq!(text_fmt.col_offset(2, 7), 7);
    assert_eq!(text_fmt.col_offset(3, 7), 0);
    assert!(text_fmt.is_selective());
    // soft wrap off: every line shares the offset, as before this fork
    text_fmt.soft_wrap = false;
    assert_eq!(text_fmt.col_offset(0, 7), 7);
    assert!(!text_fmt.is_selective());
}
