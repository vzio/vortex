//! The cost of drawing a large Markdown buffer (this fork: selective soft wrap keeps it flat).
//! Ignored: run it with optimizations and look at the numbers it prints,
//!
//! ```sh
//! VORTEX_RUNTIME=$PWD/runtime cargo test --profile integration -p vortex-term --lib drawing_cost -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tui::buffer::Buffer;
use vortex_core::syntax::Loader;
use vortex_core::text_annotations::TextAnnotations;
use vortex_core::{Rope, Transaction};
use vortex_view::editor::Config;
use vortex_view::graphics::Rect;
use vortex_view::view::ViewPosition;
use vortex_view::{Document, Theme, ViewId};

use crate::ui::document::render_document;
use crate::ui::text_decorations::DecorationManager;
use crate::ui::EditorView;

/// How many sections the file has: 25 lines each. Much more and parsing it (the Markdown, its
/// inline Markdown and its Rust code blocks) takes longer than the parser's timeout, and the editor
/// would draw it without a syntax tree, so without highlighting or unwrapped lines.
const SECTIONS: usize = 400;

/// A large Markdown file: sections of prose, a wide table and a code block.
fn large_markdown() -> String {
    let mut text = String::new();
    for i in 0..SECTIONS {
        text.push_str(&format!("## Section {i}\n\n"));
        text.push_str(
            "A paragraph of prose that is long enough to wrap at least once in a window of a \
             hundred and twenty columns, with **strong** words, `inline code` and a \
             [link](https://example.com/some/long/path) so that the highlighting has work.\n\n",
        );
        text.push_str("| Name | Description | Default | Notes |\n|---|---|---|---|\n");
        for row in 0..6 {
            text.push_str(&format!(
                "| field_{row} | The description of a field, long enough to make the row wider than the window it is shown in | {row} | more notes in the last column |\n"
            ));
        }
        text.push_str("\n```rust\nfn main() {\n    let value = compute(\"a string\", 42); // a comment that makes this line longer than the window\n}\n```\n\n");
    }
    text
}

fn loader(overrides: Option<&str>) -> Loader {
    let mut lang = vortex_loader::config::default_lang_config();
    if let Some(overrides) = overrides {
        lang = vortex_loader::merge_toml_values(lang, toml::from_str(overrides).unwrap(), 3);
    }
    Loader::new(lang.try_into().unwrap()).unwrap()
}

/// The median time to draw a 120x50 view of `doc` at `offset`, with syntax highlighting.
fn frame_time(doc: &Document, loader: &Loader, offset: ViewPosition) -> Duration {
    let area = Rect::new(0, 0, 120, 50);
    let theme = Theme::default();
    let mut times: Vec<Duration> = (0..300)
        .map(|_| {
            let mut surface = Buffer::empty(area);
            let start = Instant::now();
            let highlighter =
                EditorView::doc_syntax_highlighter(doc, offset.anchor, area.height, loader);
            render_document(
                &mut surface,
                area,
                doc,
                offset,
                &TextAnnotations::default(),
                highlighter,
                Vec::new(),
                &theme,
                DecorationManager::default(),
            );
            start.elapsed()
        })
        .collect();
    times.sort();
    times[times.len() / 2]
}

fn markdown_doc(text: &str, loader: Arc<ArcSwap<Loader>>) -> Document {
    let mut doc = Document::from(
        Rope::from(text),
        None,
        Arc::new(ArcSwap::new(Arc::new(Config::default()))),
        loader.clone(),
    );
    let loader = loader.load();
    let markdown = loader.language_for_name("markdown").unwrap();
    doc.set_language(Some(loader.language(markdown).config().clone()), &loader);
    assert!(doc.syntax().is_some(), "the Markdown grammar is built");
    doc
}

#[test]
#[ignore = "a measurement: run it with optimizations, see the module's doc"]
fn drawing_cost() {
    let text = large_markdown();
    let rope = Rope::from(text.as_str());
    println!("{} lines", rope.len_lines());
    // a view in the middle of the file, at the start of a section
    let anchor = rope.line_to_char(rope.len_lines() / 2);
    let anchor = rope.line_to_char(
        (rope.char_to_line(anchor)..rope.len_lines())
            .find(|&line| rope.line(line).to_string().starts_with("## "))
            .unwrap(),
    );
    let at = |horizontal_offset| ViewPosition {
        anchor,
        horizontal_offset,
        vertical_offset: 0,
    };

    let cases: [(&str, Option<&str>, usize); 4] = [
        ("every line wraps (except = [])", Some(EVERY_LINE_WRAPS), 0),
        ("selective (Markdown's defaults)", None, 0),
        ("selective, scrolled 40 columns", None, 40),
        ("soft wrap off", Some(NO_SOFT_WRAP), 0),
    ];
    for (name, overrides, horizontal_offset) in cases {
        let loader = Arc::new(ArcSwap::from_pointee(loader(overrides)));
        let mut doc = markdown_doc(&text, loader.clone());
        let loader = loader.load();
        let time = frame_time(&doc, &loader, at(horizontal_offset));
        // in a word of the paragraph after the heading
        let in_prose = rope.line_to_char(rope.char_to_line(anchor) + 2) + 10;
        let (edit, find) = edit_time(&mut doc, in_prose);
        println!(
            "{name:<34} {:>6.0} µs per frame; an edit {:>5.2} ms, then finding the lines {:>5.2} ms",
            time.as_secs_f64() * 1e6,
            edit.as_secs_f64() * 1e3,
            find.as_secs_f64() * 1e3,
        );
    }
}

/// The median time of typing a character at `at` (the edit and its reparse, with the inline
/// Markdown and the code blocks injected), and of finding the
/// unwrapped lines again after it (once per version of the text, at the next frame).
fn edit_time(doc: &mut Document, at: usize) -> (Duration, Duration) {
    let view = ViewId::default();
    doc.ensure_view_init(view);
    let mut edits = Vec::new();
    let mut finds = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        let insert = Transaction::change(doc.text(), [(at, at, Some("x".into()))].into_iter());
        doc.apply(&insert, view);
        edits.push(start.elapsed());
        let start = Instant::now();
        let _ = doc.text_format(120, None);
        finds.push(start.elapsed());
    }
    edits.sort();
    finds.sort();
    (edits[edits.len() / 2], finds[finds.len() / 2])
}

const EVERY_LINE_WRAPS: &str = r#"
[[language]]
name = "markdown"
soft-wrap = { enable = true, except = [] }
"#;

const NO_SOFT_WRAP: &str = r#"
[[language]]
name = "markdown"
soft-wrap = { enable = false }
"#;
