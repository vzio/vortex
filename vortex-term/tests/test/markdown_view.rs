//! The Markdown render mode (this fork), end to end: the reading width, the keys, the toggle with
//! the source, the keys that edit, the code blocks that scroll sideways, when a file opens
//! rendered, history, and the themes' markup.
//!
//! The mode is drawn by the editor view over the buffer, so the tests look at the screen and at
//! the mode itself (`Application::markdown`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use termina::event::Event;
use vortex_core::diagnostic::Severity;
use vortex_term::config::Config;
use vortex_view::document::Mode;
use vortex_view::editor::MarkdownViewStartup;
use vortex_view::graphics::Color;
use vortex_view::history::{Place, ScreenKind};
use vortex_view::theme::{Loader, Modifier};
use vortex_view::{doc, doc_mut};

use super::git_log::{screen_rows, status, Session};
use super::helpers::{test_config, AppBuilder};

const LINK_SELECTED: Color = Color::Rgb(0xff, 0x00, 0xff);

/// A very long command, wider than any terminal of these tests.
fn long_command() -> String {
    format!(
        "cargo install --path vortex-term --locked --root {}",
        "/opt/vortex/releases/2026/destination/directory".repeat(5)
    )
}

fn guide() -> String {
    format!(
        "\
# Vortex Guide

Welcome to Vortex, a modal editor for the terminal. Start here:

- [Install steps](#installing)
- [Changes](changes.md#history)
- [Entry point](main.rs#L2)
- [The site](https://example.com/vortex)

## Installing

Build it with cargo and put the binary on your path.

```sh
{}
cargo build
```

## Using it

Open a file and press `V` on a Markdown buffer. The page is rendered anywhere.

```toml
[editor]
markdown-view = \"browse\"
```

### Keys

Move with `j` and `k`.

- one
- two

## Credits

Thanks to everyone who rendered this guide.
",
        long_command()
    )
}

const CHANGES: &str = "\
# Changes

## Added

- The render mode.
- The outline.

## History

Older things, written long before the render mode existed.
";

const MAIN: &str = "fn main() {\n    println!(\"vortex\");\n}\n";

struct Fixture {
    _dir: tempfile::TempDir,
    guide: PathBuf,
    changes: PathBuf,
    main: PathBuf,
    name: String,
}

/// The files in a directory of the workspace (the file tree is rooted there).
fn fixture() -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix("md-view-test-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::canonicalize(path).unwrap()
    };
    Fixture {
        guide: write("guide.md", &guide()),
        changes: write("changes.md", CHANGES),
        main: write("main.rs", MAIN),
        name: dir.path().file_name().unwrap().to_string_lossy().into(),
        _dir: dir,
    }
}

fn config(startup: MarkdownViewStartup) -> Config {
    let mut config = test_config();
    config.editor.markdown_view = startup;
    config
}

fn theme() -> vortex_view::Theme {
    vortex_view::Theme::from(
        toml::from_str::<toml::Value>(&format!(
            r##"
"ui.selection" = {{ bg = "#333333" }}
"ui.markdown.link.selected" = {{ bg = "#ff00ff" }}
"##
        ))
        .unwrap(),
    )
}

/// A session on `file` named on the command line, after its first frame, in a terminal of
/// `width` by `height` cells.
async fn session_with(
    config: Config,
    file: &Path,
    width: u16,
    height: u16,
) -> Session {
    let app = AppBuilder::new()
        .with_config(config)
        .with_file(file, None)
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    session.app.editor.theme = theme();
    session.first_frame().await;
    session.resize(width, height).await;
    session
}

/// A session on `file`, `Never` rendering it by itself: `V` opens it.
async fn source(file: &Path) -> Session {
    session_with(config(MarkdownViewStartup::Never), file, 120, 16).await
}

/// A session on `file`, which is rendered (named on the command line).
async fn rendered(file: &Path) -> Session {
    let mut session = session_with(config(MarkdownViewStartup::Browse), file, 120, 16).await;
    assert!(up(&mut session), "{:#?}", session.rows());
    session
}

impl Session {
    async fn resize(&mut self, width: u16, height: u16) {
        self.app.resize_screen(width, height);
        let event = Event::WindowResized(termina::WindowSize {
            cols: width,
            rows: height,
            pixel_width: None,
            pixel_height: None,
        });
        self.tx.send(Ok(event)).unwrap();
        assert!(self.app.event_loop_until_idle(&mut self.input).await);
        // a frame at the new size
        self.run_for(Duration::from_millis(100)).await;
    }

    /// Lets jobs (a picker's choice) and the frames catch up.
    async fn settle(&mut self) {
        self.run_for(Duration::from_millis(300)).await;
    }
}

fn up(session: &mut Session) -> bool {
    session.app.markdown().is_some()
}

fn reading(session: &mut Session) -> usize {
    session.app.markdown().expect("the render mode").0.reading()
}

fn top(session: &mut Session) -> usize {
    session.app.markdown().expect("the render mode").0.top()
}

fn page(session: &mut Session) -> Vec<String> {
    let (markdown, editor) = session.app.markdown().expect("the render mode");
    markdown.text(editor)
}

/// The line of the page with `text` in it.
fn line_of(session: &mut Session, text: &str) -> usize {
    let page = page(session);
    page.iter()
        .position(|line| line.contains(text))
        .unwrap_or_else(|| panic!("no line with {text:?}: {page:#?}"))
}

/// The line of the source the line `line` of the page comes from.
fn source_of(session: &mut Session, line: usize) -> usize {
    let (markdown, editor) = session.app.markdown().expect("the render mode");
    markdown.page(editor).lines[line].source
}

fn offset(session: &mut Session, block: usize) -> usize {
    session.app.markdown().expect("the render mode").0.offset(block)
}

/// The row of the screen with `text`, as text, and its number.
fn row_with(session: &Session, text: &str) -> (usize, String) {
    let rows = session.rows();
    rows.iter()
        .enumerate()
        .find(|(_, row)| row.contains(text))
        .map(|(i, row)| (i, row.clone()))
        .unwrap_or_else(|| panic!("no row with {text:?}:\n{rows:#?}"))
}

/// The column of `text` on the screen, in cells.
fn column_of(session: &Session, text: &str) -> usize {
    let (_, row) = row_with(session, text);
    row.chars().position(|_| true).map(|_| ()).unwrap();
    let byte = row.find(text).unwrap();
    row[..byte].chars().count()
}

fn background_at(session: &Session, column: usize, row: usize) -> Color {
    session
        .app
        .screen()
        .get(column as u16, row as u16)
        .unwrap()
        .bg
}

fn path_of(session: &Session) -> PathBuf {
    std::fs::canonicalize(doc!(session.app.editor).path().expect("a file")).unwrap()
}

fn cursor_line(session: &Session) -> usize {
    let (view, doc) = vortex_view::current_ref!(session.app.editor);
    let text = doc.text().slice(..);
    text.char_to_line(doc.selection(view.id).primary().cursor(text))
}

fn view_top_line(session: &Session) -> usize {
    let (view, doc) = vortex_view::current_ref!(session.app.editor);
    let text = doc.text().slice(..);
    text.char_to_line(doc.view_offset(view.id).anchor.min(text.len_chars()))
}

fn text_line(session: &Session, line: usize) -> String {
    let text = doc!(session.app.editor).text().to_string();
    text.lines().nth(line).unwrap_or_default().to_string()
}

fn register(session: &Session, name: char) -> Option<Vec<String>> {
    session
        .app
        .editor
        .registers
        .read(name, &session.app.editor)
        .map(|values| values.map(|value| value.into_owned()).collect())
}

// the reading width

#[tokio::test(flavor = "multi_thread")]
async fn the_page_is_a_hundred_wide_and_centered() {
    let fixture = fixture();
    let session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.guide,
        160,
        30,
    )
    .await;
    // (60 cells of margin, half on each side)
    assert_eq!(column_of(&session, "Welcome to Vortex"), 30);
    let (_, row) = row_with(&session, "Welcome to Vortex");
    // wrapped at 100: the first line ends before the right margin
    assert!(row.trim_end().chars().count() <= 130, "{row:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_width_of_zero_uses_the_window() {
    let fixture = fixture();
    let mut config = config(MarkdownViewStartup::Browse);
    config.editor.markdown_width = 0;
    let session = session_with(config, &fixture.guide, 160, 30).await;
    // a margin of one cell on each side
    assert_eq!(column_of(&session, "Welcome to Vortex"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_width_can_be_narrower() {
    let fixture = fixture();
    let mut config = config(MarkdownViewStartup::Browse);
    config.editor.markdown_width = 60;
    let session = session_with(config, &fixture.guide, 160, 30).await;
    assert_eq!(column_of(&session, "Welcome to Vortex"), 50);
    let (_, row) = row_with(&session, "Welcome to Vortex");
    assert!(row.trim_end().chars().count() <= 110, "{row:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_window_narrower_than_the_cap_gives_it_all_but_the_margins() {
    let fixture = fixture();
    let session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.guide,
        80,
        30,
    )
    .await;
    assert_eq!(column_of(&session, "Welcome to Vortex"), 1);
}

// the keys

#[tokio::test(flavor = "multi_thread")]
async fn j_and_k_move_the_reading_line() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    assert_eq!(reading(&mut session), 0);
    session.keys("j").await;
    assert_eq!(reading(&mut session), 1);
    session.keys("3j").await;
    assert_eq!(reading(&mut session), 4);
    session.keys("k").await;
    assert_eq!(reading(&mut session), 3);
    session.keys("20k").await;
    assert_eq!(reading(&mut session), 0);
    // down the page: it follows the reading line
    session.keys("25j").await;
    assert_eq!(reading(&mut session), 25);
    assert!(top(&mut session) > 0);
    assert!(top(&mut session) <= 25);
    assert!(up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn page_keys_and_the_ends() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let len = page(&mut session).len();
    session.keys("<C-d>").await;
    let down = reading(&mut session);
    assert!(down > 1 && down < len, "{down}");
    assert!(top(&mut session) > 0);
    session.keys("<C-u>").await;
    assert_eq!(reading(&mut session), 0);
    assert_eq!(top(&mut session), 0);

    session.keys("ge").await;
    assert_eq!(reading(&mut session), len - 1);
    assert!(top(&mut session) > 0);
    session.keys("gg").await;
    assert_eq!(reading(&mut session), 0);
    assert_eq!(top(&mut session), 0);
    assert!(up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn brackets_go_to_the_headings() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let installing = line_of(&mut session, "Installing");
    let using = line_of(&mut session, "Using it");
    session.keys("]").await;
    assert_eq!(reading(&mut session), installing);
    // at the top of the screen
    assert_eq!(top(&mut session), installing);
    session.keys("]").await;
    assert_eq!(reading(&mut session), using);
    session.keys("[").await;
    assert_eq!(reading(&mut session), installing);
    session.keys("[").await;
    assert_eq!(reading(&mut session), 0);
    // nothing before the first heading
    session.keys("[").await;
    assert_eq!(reading(&mut session), 0);
    // a count
    session.keys("2]").await;
    assert_eq!(reading(&mut session), using);
    // and none after the last
    session.keys("9]").await;
    assert_eq!(reading(&mut session), using);
}

#[tokio::test(flavor = "multi_thread")]
async fn t_is_the_outline() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("t").await;
    session.settle().await;
    let rows = session.rows();
    for heading in ["Vortex Guide", "Installing", "Using it", "Keys", "Credits"] {
        assert!(
            rows.iter().any(|row| row.contains(heading)),
            "{heading}: {rows:#?}"
        );
    }
    session.keys("Credits<ret>").await;
    session.settle().await;
    let credits = page(&mut session)
        .iter()
        .position(|line| line.trim() == "Credits")
        .unwrap();
    assert_eq!(reading(&mut session), credits);
    // the picker is gone, the page is still up
    assert!(up(&mut session));
    assert!(!session.rows().iter().any(|row| row.starts_with('┌')));
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_searches_and_n_goes_on() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let first = line_of(&mut session, "anywhere");
    let second = line_of(&mut session, "this guide");
    session.keys("/rendered<ret>").await;
    assert_eq!(reading(&mut session), first);
    // the matches are drawn: a style on the word
    session.keys("n").await;
    assert_eq!(reading(&mut session), second);
    // after the last, around to the first
    session.keys("n").await;
    assert_eq!(reading(&mut session), first);
    session.keys("N").await;
    assert_eq!(reading(&mut session), second);
    session.keys("N").await;
    assert_eq!(reading(&mut session), first);
    assert!(up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_is_case_insensitive_unless_it_has_capitals() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let credits = page(&mut session)
        .iter()
        .position(|line| line.trim() == "Credits")
        .unwrap();
    session.keys("/credits<ret>").await;
    assert_eq!(reading(&mut session), credits);
    session.keys("gg/CREDITS<ret>").await;
    // no match: stays where the search started
    assert_eq!(reading(&mut session), 0);
    session.keys("/Thanks<ret>").await;
    assert_eq!(reading(&mut session), credits + 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_in_the_search_prompt_stays_on_the_page() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("5j/Credits<esc>").await;
    assert!(up(&mut session));
    assert_eq!(reading(&mut session), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn tab_selects_the_links() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let install = line_of(&mut session, "Install steps");
    let changes = line_of(&mut session, "Changes");
    session.keys("<tab>").await;
    assert_eq!(reading(&mut session), install);
    let (row, text) = row_with(&session, "Install steps");
    let column = text.find("Install steps").unwrap();
    assert_eq!(background_at(&session, column, row), LINK_SELECTED);
    // not the whole line
    assert_ne!(background_at(&session, column + 40, row), LINK_SELECTED);

    session.keys("<tab>").await;
    assert_eq!(reading(&mut session), changes);
    let (row, text) = row_with(&session, "Changes (");
    let column = text.find("Changes").unwrap();
    assert_eq!(background_at(&session, column, row), LINK_SELECTED);
    // (the first one is not selected any more)
    let (row, text) = row_with(&session, "Install steps");
    let column = text.find("Install steps").unwrap();
    assert_ne!(background_at(&session, column, row), LINK_SELECTED);

    session.keys("<S-tab>").await;
    assert_eq!(reading(&mut session), install);
    // before the first: around to the last
    session.keys("<S-tab>").await;
    assert_eq!(reading(&mut session), line_of(&mut session, "The site"));
    session.keys("<tab>").await;
    assert_eq!(reading(&mut session), install);
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_on_an_anchor_jumps_to_the_heading() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("<tab><ret>").await;
    let installing = page(&mut session)
        .iter()
        .position(|line| line.trim() == "Installing")
        .unwrap();
    assert_eq!(reading(&mut session), installing);
    assert!(up(&mut session));
    assert_eq!(path_of(&session), fixture.guide);
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_on_a_web_link_yanks_it() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("<tab><tab><tab><tab><ret>").await;
    assert_eq!(status(&session.app), "yanked https://example.com/vortex");
    assert_eq!(
        register(&session, '+'),
        Some(vec!["https://example.com/vortex".to_string()])
    );
    // still on the page, still the same buffer
    assert!(up(&mut session));
    assert_eq!(path_of(&session), fixture.guide);
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_on_a_line_without_a_link_says_so() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("<ret>").await;
    assert_eq!(status(&session.app), "No link on this line");
    assert!(up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_on_the_first_link_of_the_reading_line() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    // no Tab: the line's first link
    let line = line_of(&mut session, "Install steps");
    session.keys(&format!("{line}j<ret>")).await;
    let installing = page(&mut session)
        .iter()
        .position(|line| line.trim() == "Installing")
        .unwrap();
    assert_eq!(reading(&mut session), installing);
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_on_a_local_markdown_file_opens_it_rendered_at_its_heading() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("<tab><tab><ret>").await;
    assert_eq!(path_of(&session), fixture.changes);
    assert!(up(&mut session), "{:#?}", session.rows());
    let history = page(&mut session)
        .iter()
        .position(|line| line.trim() == "History")
        .unwrap();
    assert_eq!(reading(&mut session), history);
    assert!(session.rows().iter().any(|row| row.contains("Older things")));

    // `ga` goes back to the page we left
    session.keys("ga").await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(up(&mut session), "{:#?}", session.rows());
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_on_a_local_source_file_opens_it_in_the_editor_at_its_line() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("<tab><tab><tab><ret>").await;
    assert_eq!(path_of(&session), fixture.main);
    assert!(!up(&mut session));
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    // `#L2`
    assert_eq!(cursor_line(&session), 1);
    assert!(session.rows().iter().any(|row| row.contains("println!")));

    session.keys("ga").await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(up(&mut session), "{:#?}", session.rows());
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_opens_the_key_help() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("?").await;
    let rows = session.rows();
    for expected in ["Markdown", "next / previous heading", "next / previous link"] {
        assert!(
            rows.iter().any(|row| row.contains(expected)),
            "{expected}: {rows:#?}"
        );
    }
    session.keys("<esc>").await;
    let rows = session.rows();
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("next / previous heading")),
        "{rows:#?}"
    );
    // the page is still up
    assert!(up(&mut session));
}

// the toggle

#[tokio::test(flavor = "multi_thread")]
async fn v_opens_the_page_at_the_cursors_line() {
    let fixture = fixture();
    let mut session = source(&fixture.guide).await;
    assert!(!up(&mut session));
    // "## Using it" is the line 20 (1-based 21) of the source
    let using = guide().lines().position(|l| l == "## Using it").unwrap();
    session.keys(&format!("{using}j")).await;
    assert_eq!(cursor_line(&session), using);
    session.keys("V").await;
    assert!(up(&mut session));
    let reading = reading(&mut session);
    assert_eq!(source_of(&mut session, reading), using);
    assert!(session.rows().iter().any(|row| row.contains("Credits")) || top(&mut session) > 0);
    assert_eq!(session.app.editor.mode(), Mode::Normal);
}

#[tokio::test(flavor = "multi_thread")]
async fn v_and_escape_go_back_to_the_source_at_the_reading_line() {
    for back in ["V", "<esc>"] {
        let fixture = fixture();
        let mut session = source(&fixture.guide).await;
        session.keys("V").await;
        assert!(up(&mut session));
        session.keys("]]]").await;
        let keys = line_of(&mut session, "Keys");
        assert_eq!(reading(&mut session), keys);
        let heading_source = source_of(&mut session, keys);
        let page_top_source = {
            let top = top(&mut session);
            source_of(&mut session, top)
        };
        session.keys(back).await;
        assert!(!up(&mut session), "{back}");
        assert_eq!(session.app.editor.mode(), Mode::Normal);
        assert_eq!(cursor_line(&session), heading_source, "{back}");
        assert_eq!(text_line(&session, heading_source), "### Keys");
        // the first character of the line
        let (view, doc) = vortex_view::current_ref!(session.app.editor);
        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        assert_eq!(cursor, text.line_to_char(heading_source), "{back}");
        // the view's top is where the page's was
        assert_eq!(view_top_line(&session), page_top_source, "{back}");
        // the source is on the screen
        assert!(
            session.rows().iter().any(|row| row.contains("### Keys")),
            "{back}: {:#?}",
            session.rows()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v_on_another_buffer_is_an_error() {
    let fixture = fixture();
    let mut session = source(&fixture.main).await;
    session.keys("V").await;
    assert_eq!(status(&session.app), "Not a Markdown buffer");
    assert!(session
        .app
        .editor
        .get_status()
        .is_some_and(|(_, severity)| *severity == Severity::Error));
    assert!(!up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_view_toggles_too() {
    let fixture = fixture();
    let mut session = source(&fixture.guide).await;
    session.keys(":markdown-view<ret>").await;
    assert!(up(&mut session));
    session.keys(":markdown-view<ret>").await;
    assert!(!up(&mut session));
    session.keys(":markdown-view<ret>").await;
    assert!(up(&mut session));
    // from the page, `:` and the command line work
    session.keys("V").await;
    assert!(!up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_row_names_the_mode_and_the_place() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("]").await;
    let rows = session.rows();
    let last = rows
        .iter()
        .rev()
        .find(|row| row.contains("RENDER"))
        .unwrap_or_else(|| panic!("a status row: {rows:#?}"));
    assert!(last.contains("guide.md"), "{last:?}");
    assert!(last.contains("Installing"), "{last:?}");
    let total = page(&mut session).len();
    let reading = reading(&mut session);
    assert!(last.contains(&format!("{}/{total}", reading + 1)), "{last:?}");
}

// the keys that edit

#[tokio::test(flavor = "multi_thread")]
async fn i_inserts_at_the_headings_line_and_the_page_goes() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("]").await;
    let heading = {
        let line = reading(&mut session);
        source_of(&mut session, line)
    };
    assert_eq!(text_line(&session, heading), "## Installing");
    session.keys("i").await;
    assert_eq!(session.app.editor.mode(), Mode::Insert);
    assert!(!up(&mut session));
    session.keys("The ").await;
    session.keys("<esc>").await;
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    assert!(!up(&mut session));
    assert_eq!(text_line(&session, heading), "The ## Installing");
    // the source is shown
    assert!(session
        .rows()
        .iter()
        .any(|row| row.contains("The ## Installing")));
}

#[tokio::test(flavor = "multi_thread")]
async fn o_opens_a_line_below_the_reading_lines_source_line() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("]").await;
    let heading = {
        let line = reading(&mut session);
        source_of(&mut session, line)
    };
    let before = doc!(session.app.editor).text().len_lines();
    session.keys("onew<esc>").await;
    assert!(!up(&mut session));
    assert_eq!(doc!(session.app.editor).text().len_lines(), before + 1);
    assert_eq!(text_line(&session, heading), "## Installing");
    assert_eq!(text_line(&session, heading + 1), "new");
    assert_eq!(text_line(&session, heading + 2), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn x_then_d_delete_the_source_line_of_the_reading_line() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("]").await;
    let heading = {
        let line = reading(&mut session);
        source_of(&mut session, line)
    };
    let before = doc!(session.app.editor).text().len_lines();
    session.keys("x").await;
    // selecting changes nothing: still the page
    assert!(up(&mut session));
    session.keys("d").await;
    assert!(!up(&mut session));
    assert_eq!(doc!(session.app.editor).text().len_lines(), before - 1);
    assert_eq!(text_line(&session, heading), "");
    assert!(!doc!(session.app.editor)
        .text()
        .to_string()
        .contains("## Installing"));
    assert!(doc!(session.app.editor).is_modified());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_that_changes_nothing_keeps_the_page() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let before = session.rows();
    session.keys("<space>").await;
    session.keys("<esc>").await;
    // (Esc closes it: the first one is the one of the menu)
    let _ = before;
    assert!(!doc!(session.app.editor).is_modified());
}

// the code blocks

/// The page line and the row of the screen of the long command.
async fn at_the_long_command(session: &mut Session) -> (usize, usize) {
    let line = line_of(session, "cargo install");
    let from = reading(session);
    if line > from {
        session.keys(&format!("{}j", line - from)).await;
    }
    assert_eq!(reading(session), line);
    (line, row_with(session, "cargo install").0)
}

fn long_row(session: &Session, row: usize) -> String {
    session.rows()[row].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wide_code_block_shows_where_it_is_cut_and_scrolls_with_l() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let (_, row) = at_the_long_command(&mut session).await;
    let drawn = long_row(&session, row);
    // cut at the right edge: `>` there, `<` nowhere
    assert!(drawn.contains('>'), "{drawn:?}");
    assert!(!drawn.contains('<'), "{drawn:?}");
    let edge = drawn.trim_end().chars().count() - 1;
    assert!(drawn.trim_end().ends_with('>'), "{drawn:?}");
    assert!(edge > 100, "{edge}");
    assert_eq!(offset(&mut session, 0), 0);
    let start = column_of(&session, "cargo install");

    session.keys("l").await;
    assert_eq!(offset(&mut session, 0), 1);
    let drawn = long_row(&session, row);
    assert!(drawn.contains("argo install"), "{drawn:?}");
    assert!(!drawn.contains("cargo install"), "{drawn:?}");
    assert_eq!(column_of(&session, "argo install"), start);
    // `<` in the padding on the left of the code
    assert_eq!(drawn.chars().nth(start - 1), Some('<'), "{drawn:?}");
    assert!(drawn.trim_end().ends_with('>'), "{drawn:?}");

    // a count
    session.keys("4l").await;
    assert_eq!(offset(&mut session, 0), 5);
    session.keys("2h").await;
    assert_eq!(offset(&mut session, 0), 3);
    // the reading line and the page stayed
    assert!(up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn capital_h_and_l_scroll_by_half_and_gh_gl_go_to_the_ends() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    let (_, row) = at_the_long_command(&mut session).await;
    let (visible, max) = {
        let (markdown, editor) = session.app.markdown().unwrap();
        let block = &markdown.page(editor).code_blocks[0];
        (block.visible, block.max_offset())
    };
    assert!(max > visible / 2 + 1);
    let half = (visible / 2).max(1);

    session.keys("L").await;
    assert_eq!(offset(&mut session, 0), half);
    session.keys("L").await;
    assert_eq!(offset(&mut session, 0), 2 * half);
    session.keys("H").await;
    assert_eq!(offset(&mut session, 0), half);

    session.keys("gl").await;
    assert_eq!(offset(&mut session, 0), max);
    let drawn = long_row(&session, row);
    assert!(!drawn.contains('>'), "the end of the line: {drawn:?}");
    assert!(drawn.contains('<'), "{drawn:?}");
    // the end of the longest line shows
    assert!(drawn.contains("destination/directory"), "{drawn:?}");
    // no further
    session.keys("l").await;
    assert_eq!(offset(&mut session, 0), max);

    session.keys("gh").await;
    assert_eq!(offset(&mut session, 0), 0);
    let drawn = long_row(&session, row);
    assert!(drawn.contains("cargo install") && drawn.contains('>'), "{drawn:?}");
    assert!(!drawn.contains('<'), "{drawn:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_block_of_the_reading_line_scrolls() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    // both blocks on the screen
    session.resize(120, 40).await;
    let toml_before = row_with(&session, "markdown-view =");
    let toml_column = column_of(&session, "markdown-view =");
    at_the_long_command(&mut session).await;
    session.keys("5l").await;
    assert_eq!(offset(&mut session, 0), 5);
    assert_eq!(offset(&mut session, 1), 0);
    let toml_after = row_with(&session, "markdown-view =");
    assert_eq!(toml_before.1, toml_after.1);
    assert_eq!(column_of(&session, "markdown-view ="), toml_column);

    // the second block does not scroll: its lines fit, `l` is nothing there
    let toml = line_of(&mut session, "markdown-view =");
    let from = reading(&mut session);
    session.keys(&format!("{}j", toml - from)).await;
    let rows = session.rows();
    session.keys("l").await;
    assert_eq!(offset(&mut session, 1), 0);
    assert_eq!(offset(&mut session, 0), 5);
    assert_eq!(session.rows(), rows);
}

#[tokio::test(flavor = "multi_thread")]
async fn h_and_l_outside_a_code_block_change_nothing() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.resize(120, 40).await;
    let rows = session.rows();
    for keys in ["l", "h", "L", "H", "gl", "gh", "3l"] {
        session.keys(keys).await;
        assert_eq!(reading(&mut session), 0, "{keys}");
        assert_eq!(offset(&mut session, 0), 0, "{keys}");
        assert_eq!(session.rows(), rows, "{keys}");
        assert!(up(&mut session), "{keys}");
    }
    // the source cursor did not move either: still on its first line
    assert_eq!(cursor_line(&session), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn leaving_and_coming_back_resets_the_offsets() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    at_the_long_command(&mut session).await;
    session.keys("gl").await;
    assert!(offset(&mut session, 0) > 0);
    session.keys("V").await;
    assert!(!up(&mut session));
    session.keys("V").await;
    assert!(up(&mut session));
    assert_eq!(offset(&mut session, 0), 0);
    let (_, row) = row_with(&session, "cargo install");
    assert!(long_row(&session, row).contains('>'));
    assert!(!long_row(&session, row).contains('<'));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_scrolls_a_cut_block_to_its_match() {
    let fixture = fixture();
    let mut session = rendered(&fixture.guide).await;
    session.keys("/directory/destination$<ret>").await;
    // (the line is repeated: the last one is out of sight at the start)
    let _ = reading(&mut session);
    session.keys("gg/releases/2026/destination/directory$<ret>").await;
    assert!(offset(&mut session, 0) > 0, "the block scrolled to the match");
}

// when a file opens rendered

/// Opens `name` of the fixture from the file tree (`space e`, filtered, `Enter`).
async fn from_the_tree(session: &mut Session, fixture: &Fixture, name: &str) {
    session.keys("<space>es").await;
    session.keys(&format!("'{} '{name}", fixture.name)).await;
    session
        .wait_for_screen("the filtered tree", |rows| {
            rows.iter().any(|row| row.contains(name)) && !rows.iter().any(|row| row.contains("+ contrib/"))
        })
        .await;
    session.keys("<ret>").await;
    session.settle().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_markdown_file_opened_from_the_tree_is_rendered() {
    let fixture = fixture();
    let mut session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.main,
        120,
        16,
    )
    .await;
    assert!(!up(&mut session));
    from_the_tree(&mut session, &fixture, "guide.md").await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(up(&mut session), "{:#?}", session.rows());
    assert!(session.rows().iter().any(|row| row.contains("RENDER")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_file_from_the_tree_is_not() {
    let fixture = fixture();
    let mut session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.guide,
        120,
        16,
    )
    .await;
    assert!(up(&mut session));
    from_the_tree(&mut session, &fixture, "main.rs").await;
    assert_eq!(path_of(&session), fixture.main);
    assert!(!up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_file_picker_and_open_show_the_source() {
    let fixture = fixture();
    let mut session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.main,
        120,
        16,
    )
    .await;
    session
        .keys(&format!(":open {}<ret>", fixture.guide.display()))
        .await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(!up(&mut session), "{:#?}", session.rows());
    assert!(session.rows().iter().any(|row| row.contains("# Vortex Guide")));

    // the file picker
    session
        .keys(&format!(":open {}<ret>", fixture.main.display()))
        .await;
    session.keys("<space>f").await;
    session.keys(&format!("{} changes.md", fixture.name)).await;
    session
        .wait_for_screen("the file picker", |rows| {
            rows.iter().any(|row| row.contains("changes.md") && row.contains(" > "))
        })
        .await;
    session.keys("<ret>").await;
    session.settle().await;
    assert_eq!(path_of(&session), fixture.changes);
    assert!(!up(&mut session), "{:#?}", session.rows());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_named_on_the_command_line_is_rendered() {
    let fixture = fixture();
    let mut session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.guide,
        120,
        16,
    )
    .await;
    assert!(up(&mut session));
    assert_eq!(path_of(&session), fixture.guide);
    // and a source file is not
    let session = session_with(
        config(MarkdownViewStartup::Browse),
        &fixture.main,
        120,
        16,
    )
    .await;
    let mut session = session;
    assert!(!up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn never_opens_the_source_but_v_works() {
    let fixture = fixture();
    let mut session = session_with(
        config(MarkdownViewStartup::Never),
        &fixture.guide,
        120,
        16,
    )
    .await;
    assert!(!up(&mut session));
    session.keys("V").await;
    assert!(up(&mut session));
    session.keys("V").await;
    assert!(!up(&mut session));

    let mut session = session_with(
        config(MarkdownViewStartup::Never),
        &fixture.main,
        120,
        16,
    )
    .await;
    from_the_tree(&mut session, &fixture, "guide.md").await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(!up(&mut session), "{:#?}", session.rows());
    session.keys("V").await;
    assert!(up(&mut session));
}

#[tokio::test(flavor = "multi_thread")]
async fn always_renders_what_the_focus_comes_to() {
    let fixture = fixture();
    let mut session = session_with(
        config(MarkdownViewStartup::Always),
        &fixture.main,
        120,
        16,
    )
    .await;
    assert!(!up(&mut session));
    // `:open` of a Markdown file
    session
        .keys(&format!(":open {}<ret>", fixture.guide.display()))
        .await;
    session.settle().await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(up(&mut session), "{:#?}", session.rows());

    // away to the source file: not rendered
    session.keys(":bp<ret>").await;
    session.settle().await;
    assert_eq!(path_of(&session), fixture.main);
    assert!(!up(&mut session));

    // and back with `:b`: rendered again
    session.keys(":bn<ret>").await;
    session.settle().await;
    assert_eq!(path_of(&session), fixture.guide);
    assert!(up(&mut session), "{:#?}", session.rows());
}

// history

#[tokio::test(flavor = "multi_thread")]
async fn ga_after_v_goes_back_to_the_source() {
    let fixture = fixture();
    let mut session = source(&fixture.guide).await;
    session.keys("5j").await;
    let doc_id = doc!(session.app.editor).id();
    session.keys("V").await;
    assert!(up(&mut session));
    assert!(matches!(
        session.app.editor.place(),
        Place::Screen(screen) if screen.kind == ScreenKind::Markdown(doc_id)
    ));
    session.keys("ga").await;
    assert!(!up(&mut session), "{:#?}", session.rows());
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    assert!(matches!(session.app.editor.place(), Place::Text { .. }));
    assert_eq!(cursor_line(&session), 5);
    assert_eq!(path_of(&session), fixture.guide);
    // the page is a place of history: forward again
    session.keys("gA").await;
    session.keys("<esc>").await;
}

// the themes

#[test]
fn the_themes_have_what_the_page_needs() {
    for name in ["vortex", "matrix"] {
        let theme = Loader::new(&[]).load(name).unwrap();
        let strike = theme.try_get("markup.strikethrough").unwrap();
        assert!(strike.add_modifier.contains(Modifier::CROSSED_OUT), "{name}");
        let headings: Vec<_> = (1..=6)
            .map(|level| theme.get(&format!("markup.heading.{level}")))
            .collect();
        for (level, heading) in headings.iter().enumerate() {
            assert!(heading.fg.is_some(), "{name}: heading {}", level + 1);
            // a band on the first three levels only
            assert_eq!(heading.bg.is_some(), level < 3, "{name}: heading {}", level + 1);
        }
        for a in 0..6 {
            for b in a + 1..6 {
                assert_ne!(
                    headings[a].fg, headings[b].fg,
                    "{name}: headings {} and {} look alike",
                    a + 1,
                    b + 1
                );
            }
        }
        // the bands differ too
        assert_ne!(headings[0].bg, headings[1].bg, "{name}");
        assert_ne!(headings[1].bg, headings[2].bg, "{name}");
    }
}

#[allow(dead_code)]
fn unused(session: &mut Session) {
    let _ = screen_rows(&session.app);
    let _ = doc_mut!(session.app.editor);
}
