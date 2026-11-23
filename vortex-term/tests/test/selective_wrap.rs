//! Selective soft wrap (this fork): with soft wrap on, the lines of some syntax nodes (a
//! language's `soft-wrap.except`: Markdown's tables and code blocks) don't wrap. They stay one
//! visual line each, run past the right edge and share the view's sideways offset, while the
//! other lines wrap to the window and always start at its left edge.
//!
//! The tests open a Markdown file in a narrow terminal and read the drawn screen: where each
//! thing is found by its words, never by a fixed row.

use std::time::{Duration, Instant};

use super::git_log::Session;
use super::helpers::*;
use termina::event::{Event, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use vortex_core::diagnostic::{Diagnostic, DiagnosticProvider, Range as DiagnosticRange, Severity};
use vortex_core::text_annotations::InlineAnnotation;
use vortex_term::config::Config;
use vortex_view::annotations::diagnostics::DiagnosticFilter;
use vortex_view::document::{DocumentInlayHints, DocumentInlayHintsId};
use vortex_view::editor::MarkdownViewStartup;
use vortex_view::graphics::Color;
use vortex_view::{current_ref, doc, doc_mut};

/// The terminal: the paragraphs wrap in it, the table and the code don't fit.
const WIDTH: u16 = 60;
const HEIGHT: u16 = 32;

const PROSE_1: &str = "The editor keeps a long paragraph readable by wrapping it at the edge of the window, so that every word of it stays in sight without any sideways scrolling at all.";
const PROSE_2: &str = "Below the table comes more prose, which wraps in the same way and has to stay exactly where it is while the table slides sideways under the cursor.";
const CODE: &str = "let configuration = load_the_configuration_file_from_disk(&path).unwrap_or_default(); // kestrel";

/// The table's rows (the birds are one word each, at the start of the last cell).
const ROWS: [[&str; 4]; 3] = [
    [
        "width",
        "Number of columns the rendered page may use at most",
        "100",
        "heron lifts the cap at zero",
    ],
    [
        "margin",
        "Blank columns kept free on the left of the text",
        "4",
        "plover pairs with the lines",
    ],
    [
        "indent",
        "Spaces added in front of every nested list item",
        "2",
        "osprey likes tabs too",
    ],
];

/// Pads the cells to the widths of the columns, which make the rows about 120 wide.
fn row(cells: [&str; 4]) -> String {
    let widths = [8, 60, 7, 32];
    let cells: Vec<String> = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!("{cell:<width$}"))
        .collect();
    format!("| {} |", cells.join(" | "))
}

fn table() -> Vec<String> {
    let header = row(["option", "meaning", "default", "remarks"]);
    let delimiter: String = header
        .chars()
        .map(|c| if c == '|' { '|' } else { '-' })
        .collect();
    let mut lines = vec![header, delimiter];
    lines.extend(ROWS.map(row));
    lines
}

/// A heading, a paragraph, a table, a paragraph and a fenced code block with a long line.
fn document() -> String {
    let mut text = String::from("# Selective wrap\n\n");
    text += &format!("{PROSE_1}\n\n");
    text += &table().join("\n");
    text += &format!("\n\n{PROSE_2}\n\n");
    text += &format!("```rust\nlet short = 1;\n{CODE}\n```\n");
    text
}

/// A theme with only what the test looks at.
fn theme(styles: &str) -> vortex_view::Theme {
    // (a theme has to have a selection)
    let styles = format!("\"ui.selection\" = {{ bg = \"#333333\" }}\n{styles}");
    vortex_view::Theme::from(toml::from_str::<toml::Value>(&styles).unwrap())
}

const SELECTION: Color = Color::Rgb(0x33, 0x33, 0x33);
const PRIMARY: Color = Color::Rgb(0x44, 0x44, 0x44);
const LABEL: Color = Color::Rgb(0xff, 0x00, 0xff);
const HINT: Color = Color::Rgb(0x00, 0xff, 0x00);
const RULER: Color = Color::Rgb(0x00, 0x00, 0xff);
const UNDERLINE: Color = Color::Rgb(0xff, 0x00, 0x00);
const COLUMN: Color = Color::Rgb(0x00, 0xff, 0xff);

/// The selection, in the theme of the tests that look at it.
const SELECTION_THEME: &str = r##"
"ui.selection.primary" = { bg = "#444444" }
"##;

/// Markdown buffer where soft wrap has an override of the language.
fn languages(soft_wrap: &str) -> String {
    format!("[[language]]\nname = \"markdown\"\nsoft-wrap = {{ {soft_wrap} }}\n")
}

/// A session on a file, drawn in a `WIDTH` by `HEIGHT` terminal.
struct Page {
    session: Session,
    text: String,
    _file: tempfile::NamedTempFile,
}

impl Page {
    /// The Markdown document with the default settings and a theme.
    async fn open(styles: &str) -> Page {
        Page::open_with(&document(), ".md", None, styles, |_| {}).await
    }

    async fn open_with(
        text: &str,
        extension: &str,
        languages: Option<&str>,
        styles: &str,
        tweak: impl FnOnce(&mut Config),
    ) -> Page {
        let file = tempfile::Builder::new()
            .suffix(extension)
            .tempfile()
            .unwrap();
        std::fs::write(file.path(), text).unwrap();
        let mut config = test_config();
        // (the source, not the rendered page)
        config.editor.markdown_view = MarkdownViewStartup::Never;
        tweak(&mut config);
        let loader = test_syntax_loader(languages.map(String::from));
        let app = AppBuilder::new()
            .with_config(config)
            .with_lang_loader(loader)
            .with_file(file.path(), None)
            .build()
            .unwrap();
        let mut session = Session::with_app(app);
        session.app.editor.set_theme(theme(styles)).unwrap();
        session.first_frame().await;
        session.app.resize_screen(WIDTH, HEIGHT);
        let event = Event::WindowResized(termina::WindowSize {
            cols: WIDTH,
            rows: HEIGHT,
            pixel_width: None,
            pixel_height: None,
        });
        session.tx.send(Ok(event)).unwrap();
        assert!(session.app.event_loop_until_idle(&mut session.input).await);
        session.run_for(Duration::from_millis(100)).await;
        Page {
            session,
            text: text.to_string(),
            _file: file,
        }
    }

    async fn keys(&mut self, keys: &str) {
        self.session.keys(keys).await;
    }

    async fn close(mut self) {
        // (the buffer is unmodified or discarded)
        assert!(self.session.quits_on("<esc>:q!<ret>").await);
        let errors = self.session.app.close().await;
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// The rows of the screen.
    fn rows(&self) -> Vec<String> {
        self.session.rows()
    }

    /// The first column of the text and its width: the gutter is on its left.
    fn area(&self) -> (usize, usize) {
        let (view, doc) = current_ref!(self.session.app.editor);
        let area = view.inner_area(doc);
        (area.x as usize, area.width as usize)
    }

    /// The view's sideways offset.
    fn offset(&self) -> usize {
        let (view, doc) = current_ref!(self.session.app.editor);
        doc.view_offset(view.id).horizontal_offset
    }

    /// The lines of the document that don't wrap.
    fn unwrapped(&self) -> Vec<usize> {
        let (_, width) = self.area();
        let format = doc!(self.session.app.editor).text_format(width as u16, None);
        format
            .unwrapped
            .ranges()
            .iter()
            .flat_map(|range| range.clone())
            .collect()
    }

    fn lines(&self) -> Vec<String> {
        self.text.lines().map(String::from).collect()
    }

    /// The line of the document with `word`, which is on that line only.
    fn line_of(&self, word: &str) -> usize {
        let lines: Vec<usize> = self
            .lines()
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(word))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(lines.len(), 1, "{word:?} is on the lines {lines:?}");
        lines[0]
    }

    /// The index of the character where `word` starts in the text.
    fn char_of(&self, word: &str) -> usize {
        assert_eq!(self.text.matches(word).count(), 1, "{word:?} is not unique");
        self.text[..self.text.find(word).unwrap()].chars().count()
    }

    /// The row of the screen that starts the line `line` of the document (the gutter has its number).
    fn row_of_line(&self, line: usize) -> usize {
        let (x0, _) = self.area();
        let number = (line + 1).to_string();
        self.rows()
            .iter()
            .position(|row| row.chars().take(x0).collect::<String>().trim() == number)
            .unwrap_or_else(|| panic!("no line {line} on the screen:\n{}", self.rows().join("\n")))
    }

    /// The number of rows the line `line` takes.
    fn height_of_line(&self, line: usize) -> usize {
        let (x0, _) = self.area();
        let first = self.row_of_line(line);
        // the rows after it with no number: its wrapped rows
        let rows = self.rows();
        1 + rows[first + 1..HEIGHT as usize - 2]
            .iter()
            .take_while(|row| {
                row.chars().take(x0).all(|c| c == ' ') && row.chars().skip(x0).any(|c| c != ' ')
            })
            .count()
    }

    /// The text of the row, without the gutter.
    fn body(&self, row: usize) -> String {
        let (x0, width) = self.area();
        self.rows()[row].chars().skip(x0).take(width).collect()
    }

    /// The cell (column, row) where `word` starts on the screen, if it is there.
    fn try_cell_of(&self, word: &str) -> Option<(usize, usize)> {
        let (x0, _) = self.area();
        self.rows().iter().enumerate().find_map(|(row, text)| {
            let chars: Vec<char> = text.chars().collect();
            let word: Vec<char> = word.chars().collect();
            (x0..chars.len().saturating_sub(word.len() - 1))
                .find(|&col| chars[col..col + word.len()] == word[..])
                .map(|col| (col, row))
        })
    }

    fn cell_of(&self, word: &str) -> (usize, usize) {
        self.try_cell_of(word).unwrap_or_else(|| {
            panic!("no {word:?} on the screen:\n{}", self.rows().join("\n"));
        })
    }

    fn on_screen(&self, word: &str) -> bool {
        self.try_cell_of(word).is_some()
    }

    fn symbol(&self, column: usize, row: usize) -> String {
        self.cell(column, row).symbol.to_string()
    }

    fn cell(&self, column: usize, row: usize) -> &tui::buffer::Cell {
        self.session
            .app
            .screen()
            .get(column as u16, row as u16)
            .unwrap()
    }

    /// The cells of the text area (not the gutter) with a given background.
    fn cells_with_bg(&self, color: Color) -> Vec<(usize, usize)> {
        let (x0, width) = self.area();
        let screen = self.session.app.screen();
        let mut cells = Vec::new();
        for row in 0..screen.area.height as usize - 2 {
            for column in x0..x0 + width {
                if screen.get(column as u16, row as u16).unwrap().bg == color {
                    cells.push((column, row));
                }
            }
        }
        cells
    }

    /// The cells of a word drawn on the row of its first cell.
    fn cells_of(&self, word: &str) -> Vec<(usize, usize)> {
        let (column, row) = self.cell_of(word);
        (column..column + word.chars().count())
            .map(|column| (column, row))
            .collect()
    }

    /// Where the cursor is drawn, and the character of the text under it.
    fn cursor(&self) -> ((usize, usize), char) {
        let position = self
            .session
            .app
            .editor
            .cursor()
            .0
            .expect("the cursor is on the screen");
        let (view, doc) = current_ref!(self.session.app.editor);
        let text = doc.text().slice(..);
        let char = text.char(doc.selection(view.id).primary().cursor(text));
        ((position.col, position.row), char)
    }

    /// The index of the character of the primary cursor.
    fn cursor_char(&self) -> usize {
        let (view, doc) = current_ref!(self.session.app.editor);
        let text = doc.text().slice(..);
        doc.selection(view.id).primary().cursor(text)
    }

    /// The text of the primary selection.
    fn selected(&self) -> String {
        let (view, doc) = current_ref!(self.session.app.editor);
        let text = doc.text().slice(..);
        doc.selection(view.id).primary().slice(text).to_string()
    }

    async fn goto_line(&mut self, line: usize) {
        self.keys(&format!(":{}<ret>", line + 1)).await;
    }

    async fn mouse(&mut self, kind: MouseEventKind, column: usize, row: usize) {
        let event = MouseEvent {
            kind,
            column: column as u16,
            row: row as u16,
            modifiers: Modifiers::NONE,
        };
        self.session.tx.send(Ok(Event::Mouse(event))).unwrap();
        assert!(
            self.session
                .app
                .event_loop_until_idle(&mut self.session.input)
                .await
        );
    }

    /// Presses and releases the left button on a cell.
    async fn click(&mut self, column: usize, row: usize) {
        self.mouse(MouseEventKind::Down(MouseButton::Left), column, row)
            .await;
        self.mouse(MouseEventKind::Up(MouseButton::Left), column, row)
            .await;
    }

    /// Runs the application until `condition`.
    async fn wait_until(&mut self, what: &str, condition: impl Fn(&Page) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.session.run_for(Duration::from_millis(50)).await;
        }
    }
}

/// The part of `line` that is drawn when the text is scrolled `offset` columns sideways.
fn shifted(line: &str, offset: usize, width: usize) -> String {
    line.chars().skip(offset).take(width).collect()
}

/// The lines that don't wrap, as the document has them: the table and the code block.
fn unwrapped_lines(page: &Page) -> Vec<usize> {
    let lines = page.lines();
    let table = page.line_of("option");
    let fence = page.line_of("```rust");
    let mut expected: Vec<usize> = (table..table + 5).collect();
    expected.extend(fence..fence + 4);
    assert_eq!(page.unwrapped(), expected, "{lines:#?}");
    expected
}

/// The screen rows, but those of the lines that don't wrap, and the status line.
fn wrapped_rows(page: &Page, unwrapped: &[usize]) -> Vec<String> {
    let skip: Vec<usize> = unwrapped.iter().map(|&l| page.row_of_line(l)).collect();
    page.rows()
        .into_iter()
        .enumerate()
        .filter(|(row, _)| !skip.contains(row) && *row < HEIGHT as usize - 2)
        .map(|(_, text)| text)
        .collect()
}

// 1. The layout

/// The paragraph takes several rows, wrapped with the indicator; each row of the table and the
/// long line of the code block takes one, cut at the right edge.
#[tokio::test(flavor = "multi_thread")]
async fn the_paragraph_wraps_and_the_table_and_the_code_do_not() {
    let page = Page::open("").await;
    let (_, width) = page.area();
    let lines = page.lines();

    // the paragraph: several rows, the next ones start with the indicator
    let paragraph = page.line_of("readable");
    let first = page.row_of_line(paragraph);
    let height = page.height_of_line(paragraph);
    assert!(height >= 3, "the paragraph takes {height} rows");
    for row in first + 1..first + height {
        assert!(page.body(row).starts_with('↪'), "{:?}", page.body(row));
    }
    assert!(!page.body(first).contains('↪'));
    // all of it is on the screen: it ends on the last row
    assert!(page.body(first + height - 1).contains("at all."));

    // the table and the code block: one row per line, cut at the right edge
    let unwrapped = unwrapped_lines(&page);
    for &line in &unwrapped {
        assert_eq!(page.height_of_line(line), 1, "line {line}: {}", lines[line]);
        let body = page.body(page.row_of_line(line));
        assert_eq!(
            body.trim_end(),
            shifted(&lines[line], 0, width).trim_end(),
            "line {line}"
        );
        assert!(!body.contains('↪'));
    }
    // (the lines are longer than the view, and the last cell of the row is the text)
    for word in ["width", "margin", "indent"] {
        let line = page.line_of(word);
        assert!(lines[line].chars().count() > width + 40);
        let (x0, _) = page.area();
        let cell = page.cell(x0 + width - 1, page.row_of_line(line));
        assert_eq!(cell.symbol.as_str(), lines[line].chars().nth(width - 1).unwrap().to_string());
    }
    let code = page.line_of("kestrel");
    assert!(lines[code].chars().count() > width + 20);
    // what is past the edge is not drawn
    for word in ["heron", "plover", "osprey", "kestrel"] {
        assert!(!page.on_screen(word), "{word} is past the right edge");
    }
    // the paragraph after the table wraps too
    assert!(page.height_of_line(page.line_of("slides")) >= 3);
    page.close().await;
}

// 2. The offset

/// Moving right along a table row scrolls the table and the code sideways together; the
/// paragraphs stay put; on a line that wraps the offset is 0 again; the cursor is drawn on the
/// cell of its character.
#[tokio::test(flavor = "multi_thread")]
async fn the_unwrapped_lines_scroll_together_and_the_others_stay() {
    let mut page = Page::open("").await;
    let (_, width) = page.area();
    let lines = page.lines();
    let unwrapped = unwrapped_lines(&page);
    let before = wrapped_rows(&page, &unwrapped);
    let table_row = page.line_of("margin");

    page.goto_line(table_row).await;
    assert_eq!(page.offset(), 0, "the cursor is in view");
    page.keys("gl").await;
    let offset = page.offset();
    assert!(offset > 0, "the end of the row is past the right edge");

    // every line that doesn't wrap is drawn shifted by the same amount
    for &line in &unwrapped {
        let body = page.body(page.row_of_line(line));
        assert_eq!(
            body.trim_end(),
            shifted(&lines[line], offset, width).trim_end(),
            "line {line}"
        );
    }
    // the last column is on the screen, the first characters of the row are not
    assert!(page.on_screen("plover"));
    assert!(page.on_screen("osprey") && page.on_screen("heron") && page.on_screen("kestrel"));
    assert!(!page.on_screen("| margin"));
    assert!(!page.on_screen("let configuration"));
    // the others are drawn as before
    assert_eq!(wrapped_rows(&page, &unwrapped), before);

    // the cursor is inside the view, on the cell of its character
    let ((column, row), char) = page.cursor();
    let (x0, _) = page.area();
    assert!((x0..x0 + width).contains(&column));
    assert_eq!(row, page.row_of_line(table_row));
    assert_eq!(page.symbol(column, row), char.to_string());

    // back to the left: it scrolls back, the table follows
    page.keys("70h").await;
    assert!(page.offset() < offset);
    for &line in &unwrapped {
        let body = page.body(page.row_of_line(line));
        assert_eq!(
            body.trim_end(),
            shifted(&lines[line], page.offset(), width).trim_end()
        );
    }
    let ((column, row), char) = page.cursor();
    assert!((x0..x0 + width).contains(&column));
    assert_eq!(page.symbol(column, row), char.to_string());
    assert_eq!(wrapped_rows(&page, &unwrapped), before);

    // up to the blank line above the table, which wraps: the offset is 0, the table starts at
    // its first column again
    page.keys("gl").await;
    assert!(page.offset() > 0);
    let header = page.line_of("option");
    page.keys(&"k".repeat(table_row - header + 1)).await;
    assert_eq!(page.cursor_char(), page.char_of("| option") - 1);
    assert_eq!(page.offset(), 0);
    assert!(page.on_screen("| margin"));
    assert!(page.on_screen("let configuration"));
    assert_eq!(wrapped_rows(&page, &unwrapped), before);
    let ((column, row), _) = page.cursor();
    assert!((x0..x0 + width).contains(&column));
    assert_eq!(row, page.row_of_line(header - 1));

    // down to a table row and across it again, then to the paragraph below with `j`
    page.keys("jjgl").await;
    assert!(page.offset() > 0);
    let paragraph = page.line_of("slides");
    page.keys(&"j".repeat(paragraph - header - 1)).await;
    assert_eq!(page.offset(), 0);
    assert_eq!(wrapped_rows(&page, &unwrapped), before);
    assert!(page.on_screen("| margin"));
    page.close().await;
}

// 3. Clicks

/// With no offset, a click on the table lands on the character that is drawn there.
#[tokio::test(flavor = "multi_thread")]
async fn a_click_on_the_table_lands_right_without_an_offset() {
    let mut page = Page::open("").await;
    for (word, skip) in [("Blank", 0), ("Blank", 3), ("indent", 1), ("Spaces", 2)] {
        let (column, row) = page.cell_of(word);
        page.click(column + skip, row).await;
        assert_eq!(page.cursor_char(), page.char_of(word) + skip, "{word}+{skip}");
        assert_eq!(page.offset(), 0);
    }
    // the code block as well
    let (column, row) = page.cell_of("load_the");
    page.click(column + 2, row).await;
    assert_eq!(page.cursor_char(), page.char_of("load_the") + 2);
    page.close().await;
}

/// After scrolling sideways, a click on a shifted cell is on the character drawn there; a click
/// on a wrapped row is not shifted; a drag selects between two shifted cells.
#[tokio::test(flavor = "multi_thread")]
async fn clicks_and_drags_follow_the_offset() {
    let mut page = Page::open("").await;
    page.goto_line(page.line_of("margin")).await;
    page.keys("gl").await;
    assert!(page.offset() > 0);

    // a word of the last column of a shifted row
    let (column, row) = page.cell_of("plover");
    page.click(column + 1, row).await;
    assert_eq!(page.cursor_char(), page.char_of("plover") + 1);
    // and of another shifted row, and of the code
    let (column, row) = page.cell_of("osprey");
    page.click(column + 3, row).await;
    assert_eq!(page.cursor_char(), page.char_of("osprey") + 3);
    let (column, row) = page.cell_of("kestrel");
    page.click(column, row).await;
    assert_eq!(page.cursor_char(), page.char_of("kestrel"));
    assert!(page.offset() > 0);

    // the drag: from the start of a word to the end of another one on the same row
    let (column, row) = page.cell_of("plover");
    page.mouse(MouseEventKind::Down(MouseButton::Left), column, row)
        .await;
    let (end, row) = page.cell_of("pairs");
    page.mouse(MouseEventKind::Drag(MouseButton::Left), end + 4, row)
        .await;
    assert_eq!(page.selected(), "plover pairs");
    page.mouse(MouseEventKind::Up(MouseButton::Left), end + 4, row)
        .await;

    // a click on a word of the wrapped paragraph: not shifted, and the offset is 0 again
    page.goto_line(page.line_of("margin")).await;
    page.keys("gl").await;
    assert!(page.offset() > 0);
    let (column, row) = page.cell_of("readable");
    page.click(column + 2, row).await;
    assert_eq!(page.cursor_char(), page.char_of("readable") + 2);
    assert_eq!(page.offset(), 0);
    page.close().await;
}

// 4. Selections

/// The cells drawn with the selection's style are the cells of the selected word, in a row that
/// is shifted: one selection, then several.
#[tokio::test(flavor = "multi_thread")]
async fn selections_are_drawn_on_the_shifted_cells() {
    let mut page = Page::open(SELECTION_THEME).await;
    page.goto_line(page.line_of("indent")).await;
    page.keys("gl").await;
    assert!(page.offset() > 0);

    // a word of the last column, picked with a click and `miw`
    let (column, row) = page.cell_of("tabs");
    page.click(column, row).await;
    page.keys("miw").await;
    assert_eq!(page.selected(), "tabs");
    let word = page.selected();
    assert!(!word.is_empty());
    let drawn = page.cells_with_bg(PRIMARY);
    assert_eq!(drawn.len(), word.chars().count(), "{word:?} {drawn:?}");
    let row = drawn[0].1;
    let drawn_text: String = drawn
        .iter()
        .map(|&(column, row)| page.symbol(column, row))
        .collect();
    assert_eq!(drawn_text, word);
    assert!(drawn.iter().all(|&(_, r)| r == row));
    assert_eq!(row, page.row_of_line(page.line_of("indent")));

    // a search: the match
    page.keys("/osprey<ret>").await;
    assert!(page.offset() > 0);
    assert_eq!(page.selected(), "osprey");
    assert_eq!(page.cells_with_bg(PRIMARY), page.cells_of("osprey"));
    assert!(page.cells_with_bg(SELECTION).is_empty());

    // several: the primary has its style, the others theirs
    page.keys("%s(heron|plover|osprey)<ret>").await;
    let primary = page.selected();
    let mut others: Vec<&str> = vec!["heron", "plover", "osprey"];
    others.retain(|word| *word != primary);
    assert_eq!(page.cells_with_bg(PRIMARY), page.cells_of(&primary));
    let mut secondary = page.cells_with_bg(SELECTION);
    secondary.sort();
    let mut expected: Vec<(usize, usize)> =
        others.iter().flat_map(|word| page.cells_of(word)).collect();
    expected.sort();
    assert_eq!(secondary, expected);
    page.close().await;
}

// 5. Jump labels

/// `gw` with an offset: the labels are on the cells where the words of the shifted rows are
/// drawn, and typing one jumps to its word.
#[tokio::test(flavor = "multi_thread")]
async fn jump_labels_are_on_the_shifted_words() {
    let mut page = Page::open(r##""ui.virtual.jump-label" = { fg = "#ff00ff" }"##).await;
    page.goto_line(page.line_of("margin")).await;
    page.keys("gl").await;
    assert!(page.offset() > 0);
    let words = ["plover", "osprey", "kestrel", "readable"];
    let cells: Vec<(usize, usize)> = words.iter().map(|word| page.cell_of(word)).collect();

    page.keys("gw").await;
    let mut labels = Vec::new();
    for (word, &(column, row)) in words.iter().zip(&cells) {
        // the label replaces the first two characters of the word, in the label's color
        let (first, second) = (page.cell(column, row), page.cell(column + 1, row));
        assert_eq!(first.fg, LABEL, "{word}");
        assert_eq!(second.fg, LABEL, "{word}");
        assert_ne!(first.fg, page.cell(column + 2, row).fg);
        // and the rest of the word is where it was
        let rest: String = (column + 2..column + word.len())
            .map(|c| page.symbol(c, row))
            .collect();
        assert_eq!(rest, word[2..], "{word}");
        labels.push(format!("{}{}", first.symbol, second.symbol));
    }
    // (one label per word)
    let mut sorted = labels.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), words.len());

    // typing the label of a word in a shifted row jumps to it
    page.keys(&labels[1]).await;
    assert_eq!(page.selected(), "osprey");
    assert_eq!(page.cursor_char(), page.char_of("osprey") + "osprey".len() - 1);
    // again, for the code
    let (column, row) = page.cell_of("kestrel");
    page.keys("gw").await;
    let label = format!(
        "{}{}",
        page.symbol(column, row),
        page.symbol(column + 1, row)
    );
    page.keys(&label).await;
    assert_eq!(page.selected(), "kestrel");
    page.close().await;
}

// 6. Diagnostics

fn diagnostic(start: usize, end: usize, line: usize, message: &str) -> Diagnostic {
    Diagnostic {
        range: DiagnosticRange { start, end },
        ends_at_word: true,
        starts_at_word: true,
        zero_width: false,
        line,
        message: message.to_owned(),
        severity: Some(Severity::Error),
        code: None,
        provider: DiagnosticProvider::Lsp {
            server_id: Default::default(),
            identifier: None,
        },
        tags: Vec::new(),
        source: None,
        data: None,
    }
}

const DIAGNOSTIC_THEME: &str = r##"
"diagnostic.error" = { underline = { color = "#ff0000", style = "curl" } }
"##;

fn inline_diagnostics(config: &mut Config) {
    config.editor.inline_diagnostics.cursor_line = DiagnosticFilter::Enable(Severity::Hint);
    config.editor.inline_diagnostics.other_lines = DiagnosticFilter::Enable(Severity::Hint);
    config.editor.end_of_line_diagnostics = DiagnosticFilter::Disable;
}

async fn with_diagnostic(word: &str, message: &str) -> Page {
    let mut page = Page::open_with(&document(), ".md", None, DIAGNOSTIC_THEME, inline_diagnostics)
        .await;
    let start = page.char_of(word);
    let line = page.line_of(word);
    let diagnostic = diagnostic(start, start + word.chars().count(), line, message);
    doc_mut!(page.session.app.editor).replace_diagnostics([diagnostic], &[], None);
    page.keys("<esc>").await;
    page
}

/// The cells of the text area with an underline.
fn underlined(page: &Page) -> Vec<(usize, usize)> {
    let (x0, width) = page.area();
    let mut cells = Vec::new();
    for row in 0..HEIGHT as usize - 2 {
        for column in x0..x0 + width {
            if page.cell(column, row).underline_color == UNDERLINE {
                cells.push((column, row));
            }
        }
    }
    cells
}

/// The column where the message of the diagnostic starts, relative to the start of its word,
/// and its row relative to the row of the word.
fn ghost(page: &Page, word: (usize, usize), message: &str) -> (usize, usize) {
    let (column, row) = page.cell_of(message);
    assert!(row > word.1, "the message is below the row:\n{}", page.rows().join("\n"));
    (column - word.0, row - word.1)
}

/// Without an offset, the diagnostic of a word of a table row underlines it and has its ghost
/// text below the row; with an offset, a word past the edge has them shifted.
#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_follow_the_shifted_cells() {
    // without an offset
    let mut page = with_diagnostic("Blank", "no blanks allowed").await;
    page.goto_line(page.line_of("margin")).await;
    assert_eq!(page.offset(), 0);
    let word = page.cell_of("Blank");
    assert_eq!(underlined(&page), page.cells_of("Blank"));
    println!("{}", page.rows().join("\n"));
    let unshifted = ghost(&page, word, "no blanks allowed");
    page.close().await;

    // with an offset: the same relation to the word, at the shifted column
    let mut page = with_diagnostic("plover", "birds are not allowed").await;
    page.goto_line(page.line_of("margin")).await;
    page.keys("gl").await;
    let offset = page.offset();
    assert!(offset > 0);
    let word = page.cell_of("plover");
    assert_eq!(underlined(&page), page.cells_of("plover"));
    let (x0, _) = page.area();
    assert_eq!(word.0, x0 + page.text_column("plover") - offset);
    println!("{}", page.rows().join("\n"));
    assert_eq!(ghost(&page, word, "birds are not allowed"), unshifted);

    // moving the cursor to a paragraph (the offset is 0): the word is past the edge, so
    // nothing is underlined, the table is not drawn shifted
    page.goto_line(page.line_of("readable")).await;
    assert_eq!(page.offset(), 0);
    assert!(underlined(&page).is_empty());
    page.close().await;
}

/// The diagnostic at the end of a row is drawn after the end of the row's text, where the
/// row's end is on the screen once it's scrolled.
#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_line_diagnostic_follows_the_shifted_row() {
    let text = format!(
        "# Notes\n\n| option | meaning | remarks |\n|---|---|---|\n{}\n",
        "| width | The number of columns the rendered page may use at most | heron |"
    );
    let mut page = Page::open_with(&text, ".md", None, DIAGNOSTIC_THEME, |config| {
        config.editor.inline_diagnostics.cursor_line = DiagnosticFilter::Disable;
        config.editor.inline_diagnostics.other_lines = DiagnosticFilter::Disable;
        config.editor.end_of_line_diagnostics = DiagnosticFilter::Enable(Severity::Hint);
    })
    .await;
    let start = page.char_of("heron");
    let line = page.line_of("heron");
    let diagnostic = diagnostic(start, start + "heron".len(), line, "ok");
    doc_mut!(page.session.app.editor).replace_diagnostics([diagnostic], &[], None);
    let (x0, width) = page.area();
    let end = page.lines()[line].chars().count();
    assert!(end > width && end < width + 30, "the row is a little wider than the view");

    page.goto_line(line).await;
    page.keys("gl").await;
    let offset = page.offset();
    assert!(offset > 0);
    // the end of the row is on the screen, the word is underlined on its shifted cells
    assert_eq!(underlined(&page), page.cells_of("heron"));
    // the message is on the row of the line, after the end of its text
    let row = page.row_of_line(line);
    println!("{}", page.rows().join("\n"));
    let (column, message_row) = page.cell_of("ok");
    assert_eq!(message_row, row);
    assert!(column > x0 + end - offset, "message at {column}, the row ends at {}", x0 + end - offset);
    assert!(column <= x0 + end - offset + 3, "message at {column}, the row ends at {}", x0 + end - offset);
    page.close().await;
}

impl Page {
    /// The column of the text where `word` starts, on its line.
    fn text_column(&self, word: &str) -> usize {
        let line = self.line_of(word);
        let text = &self.lines()[line];
        text[..text.find(word).unwrap()].chars().count()
    }
}

// 7. Inlay hints

/// An inlay hint inside a table row is drawn at the shifted position, and the cursor goes by
/// the columns the hint takes.
#[tokio::test(flavor = "multi_thread")]
async fn inlay_hints_are_drawn_on_the_shifted_cells() {
    let mut page = Page::open(r##""ui.virtual.inlay-hint" = { fg = "#00ff00" }"##).await;
    let line = page.line_of("margin");
    // a hint in front of the word `plover`
    let at = page.char_of("plover");
    let hints = DocumentInlayHints {
        other_inlay_hints: vec![InlineAnnotation::new(at, "<<hint>> ")],
        ..DocumentInlayHints::empty_with_id(DocumentInlayHintsId {
            first_line: 0,
            last_line: 100,
        })
    };
    {
        let (view, doc) = vortex_view::current!(page.session.app.editor);
        doc.set_inlay_hints(view.id, hints);
    }
    page.goto_line(line).await;
    page.keys("gl").await;
    let offset = page.offset();
    assert!(offset > 0);
    let (_, row) = page.cell_of("plover");
    assert_eq!(row, page.row_of_line(line));
    let (column, hint_row) = page.cell_of("<<hint>> plover");
    assert_eq!(hint_row, row);
    // the hint is in its color and the word after it is not
    for c in column..column + "<<hint>>".len() {
        assert_eq!(page.cell(c, row).fg, HINT, "column {c}");
    }
    assert_ne!(page.cell(column + "<<hint>> ".len(), row).fg, HINT);
    // where the text column is: the hint adds its width to the columns after it
    let (x0, _) = page.area();
    let hint = "<<hint>> ".len();
    assert_eq!(column, x0 + page.text_column("plover") - offset);
    // the row is drawn shifted as a whole: the text before the hint is the row's text
    let shifted_line = shifted(&page.lines()[line], offset, 1000);
    let body = page.body(row);
    let before: String = body.chars().take(column - x0).collect();
    assert!(shifted_line.starts_with(&before));
    // the text after the hint is the rest of the row, hint columns later
    let after: String = body.chars().skip(column - x0 + hint).collect();
    let expected: String = page.lines()[line]
        .chars()
        .skip(page.text_column("plover"))
        .collect();
    assert!(expected.starts_with(after.trim_end()), "{after:?}");
    // the cursor is on its character
    let ((cursor_column, cursor_row), char) = page.cursor();
    assert_eq!(page.symbol(cursor_column, cursor_row), char.to_string());
    page.close().await;
}

// 8. Rulers

/// A ruler is at the same column of the text on every row: on the rows that wrap, and on the
/// rows that are shifted, where it moves with the text or is out of the view.
#[tokio::test(flavor = "multi_thread")]
async fn rulers_are_at_the_text_column_of_each_row() {
    let mut page = Page::open_with(
        &document(),
        ".md",
        None,
        r##""ui.virtual.ruler" = { bg = "#0000ff" }"##,
        |config| config.editor.rulers = vec![30],
    )
    .await;
    let (x0, _) = page.area();
    let ruler = x0 + 29;
    let prose = page.row_of_line(page.line_of("readable"));
    let table_row = page.line_of("margin");
    let code = page.row_of_line(page.line_of("kestrel"));
    let table_screen_row = page.row_of_line(table_row);
    let in_row = |page: &Page, row: usize| -> Vec<usize> {
        page.cells_with_bg(RULER)
            .into_iter()
            .filter(|&(_, r)| r == row)
            .map(|(c, _)| c)
            .collect()
    };

    // no offset: the ruler is at column 30 of the text on every row
    for row in [prose, prose + 1, table_screen_row, code] {
        assert_eq!(in_row(&page, row), [ruler], "row {row}");
    }

    // a small offset: it moves left with the table and the code, not with the paragraph
    page.goto_line(table_row).await;
    page.keys("60l").await;
    let offset = page.offset();
    assert!(offset > 0 && offset < 29, "offset {offset}");
    for row in [prose, prose + 1] {
        assert_eq!(in_row(&page, row), [ruler], "row {row}");
    }
    for row in [table_screen_row, code] {
        assert_eq!(in_row(&page, row), [ruler - offset], "row {row}");
    }

    // past the ruler: it is gone from the shifted rows, and stays on the paragraph's
    page.keys("gl").await;
    assert!(page.offset() > 29);
    for row in [table_screen_row, code] {
        assert_eq!(in_row(&page, row), Vec::<usize>::new(), "row {row}");
    }
    for row in [prose, prose + 1] {
        assert_eq!(in_row(&page, row), [ruler], "row {row}");
    }
    page.close().await;
}

// 9. The cursor column

/// The highlighted column is the cursor's screen column.
#[tokio::test(flavor = "multi_thread")]
async fn the_cursor_column_is_the_screen_column_of_the_cursor() {
    let mut page = Page::open_with(
        &document(),
        ".md",
        None,
        r##""ui.cursorcolumn.primary" = { bg = "#00ffff" }"##,
        |config| config.editor.cursorcolumn = true,
    )
    .await;
    let (x0, _) = page.area();
    let table_row = page.line_of("margin");
    page.goto_line(table_row).await;
    // (no offset: the cursor's column is its text column)
    let ((column, _), _) = page.cursor();
    assert_eq!(column, x0);

    for keys in ["gl", "20h", "gl"] {
        page.keys(keys).await;
        let ((column, row), _) = page.cursor();
        assert_eq!(page.cell(column, row).bg, COLUMN);
        let cells = page.cells_with_bg(COLUMN);
        assert!(cells.len() > 5, "{cells:?}");
        assert!(cells.iter().all(|&(c, _)| c == column), "{cells:?}");
    }
    assert!(page.offset() > 0);
    page.close().await;
}

// 10. Edits

const LONE_ROW: &str = "| name | a very long description of what the name means and how it is used | x |";

/// Typing the delimiter row under a row makes a table of it, and the row stops wrapping; deleting
/// it makes the row wrap again.
#[tokio::test(flavor = "multi_thread")]
async fn typing_the_delimiter_row_makes_the_table_stop_wrapping() {
    let text = format!("# Notes\n\n{LONE_ROW}\n\nprose after the table\n");
    let mut page = Page::open_with(&text, ".md", None, "", |_| {}).await;
    let line = page.line_of("| name");
    // a lone row is a paragraph: it wraps
    assert!(page.unwrapped().is_empty());
    assert!(page.height_of_line(line) >= 2);

    page.goto_line(line).await;
    page.keys("o|---|---|---|<esc>").await;
    page.wait_until("the table", |page| page.unwrapped() == [line, line + 1])
        .await;
    assert_eq!(page.height_of_line(line), 1);
    let (_, width) = page.area();
    assert!(LONE_ROW.len() > width);
    assert_eq!(
        page.body(page.row_of_line(line)).trim_end(),
        shifted(LONE_ROW, 0, width).trim_end()
    );
    assert!(!page.on_screen("prose after the table") || page.height_of_line(line + 2) >= 1);

    // taking the delimiter row away: not a table, the row wraps again
    page.keys("xd").await;
    assert!(page.text_of_doc().contains(LONE_ROW));
    page.wait_until("the paragraph", |page| page.unwrapped().is_empty())
        .await;
    assert!(page.height_of_line(line) >= 2);
    page.close().await;
}

impl Page {
    fn text_of_doc(&self) -> String {
        doc!(self.session.app.editor).text().to_string()
    }
}

// 11. Without the feature

fn long_rust() -> String {
    format!("fn main() {{\n    {CODE}\n}}\n")
}

/// A language without `except` wraps every long line, and moving right along one doesn't scroll.
#[tokio::test(flavor = "multi_thread")]
async fn a_language_without_except_wraps_all_its_lines() {
    let overrides = "[[language]]\nname = \"rust\"\nsoft-wrap = { enable = true }\n";
    let mut page = Page::open_with(&long_rust(), ".rs", Some(overrides), "", |_| {}).await;
    assert!(page.unwrapped().is_empty());
    let line = page.line_of("kestrel");
    assert!(page.height_of_line(line) >= 2);
    assert!(page.on_screen("kestrel"));
    let first = page.row_of_line(line);
    assert!(page.body(first + 1).trim_start().starts_with('↪'));

    page.goto_line(line).await;
    for keys in ["gl", "h", "gl"] {
        page.keys(keys).await;
        assert_eq!(page.offset(), 0);
    }
    page.keys("20h").await;
    assert_eq!(page.offset(), 0);
    let ((column, row), char) = page.cursor();
    assert_eq!(page.symbol(column, row), char.to_string());
    page.close().await;
}

/// The same with soft wrap on for the editor, in a plain text file.
#[tokio::test(flavor = "multi_thread")]
async fn soft_wrap_in_the_editor_config_wraps_a_text_file() {
    let text = format!("{PROSE_1}\n{PROSE_2}\n");
    let mut page = Page::open_with(&text, ".txt", None, "", |config| {
        config.editor.soft_wrap.enable = Some(true);
    })
    .await;
    assert!(page.unwrapped().is_empty());
    assert!(page.height_of_line(0) >= 3);
    page.keys("gl").await;
    assert_eq!(page.offset(), 0);
    page.close().await;
}

/// With soft wrap off, no line wraps and moving right scrolls every line, prose included.
#[tokio::test(flavor = "multi_thread")]
async fn markdown_without_soft_wrap_scrolls_every_line() {
    let overrides = languages("enable = false");
    let mut page = Page::open_with(&document(), ".md", Some(&overrides), "", |_| {}).await;
    let (_, width) = page.area();
    let lines = page.lines();
    // nothing wraps
    for (line, text) in lines.iter().enumerate() {
        if line + 1 < lines.len() {
            assert_eq!(page.height_of_line(line), 1, "line {line}");
        }
        let row = page.row_of_line(line);
        assert_eq!(
            page.body(row).trim_end(),
            shifted(text, 0, width).trim_end(),
            "line {line}"
        );
        assert!(!page.body(row).contains('↪'));
    }

    let paragraph = page.line_of("readable");
    page.goto_line(paragraph).await;
    page.keys("gl").await;
    let offset = page.offset();
    assert!(offset > 0);
    // every line is drawn shifted: the paragraph, the table, the code
    for (line, text) in lines.iter().enumerate() {
        assert_eq!(
            page.body(page.row_of_line(line)).trim_end(),
            shifted(text, offset, width).trim_end(),
            "line {line}"
        );
    }
    assert!(page.on_screen("at all."));
    let ((column, row), char) = page.cursor();
    assert_eq!(page.symbol(column, row), char.to_string());
    page.close().await;
}

/// An empty `except` makes the table wrap like prose.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_except_wraps_the_table() {
    let overrides = languages("enable = true, except = []");
    let mut page = Page::open_with(&document(), ".md", Some(&overrides), "", |_| {}).await;
    assert!(page.unwrapped().is_empty());
    let header = page.line_of("option");
    for line in header..header + 5 {
        assert!(page.height_of_line(line) >= 2, "line {line}");
        assert!(page.body(page.row_of_line(line) + 1).starts_with('↪'));
    }
    assert!(page.on_screen("heron") && page.on_screen("plover"));
    page.goto_line(page.line_of("margin")).await;
    page.keys("gl").await;
    assert_eq!(page.offset(), 0);
    let ((column, row), char) = page.cursor();
    assert_eq!(page.symbol(column, row), char.to_string());
    page.close().await;
}
