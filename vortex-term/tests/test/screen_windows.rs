//! Screens as windows, one by one: the hex view, the Markdown page, the diff and the debug panel
//! in the current window, in a split, with the window motions, a resized terminal, `:q`, `Esc` and
//! `ga`; where the debug panel goes (`[editor.debugger.panel]`); `V` in one of two windows; the
//! modes with a screen focused.
//!
//! (The terminal's own tests are in `terminal.rs`; the responsive diff is in `diff_layout.rs`.)
//!
//! Which existing tests cover which item:
//!
//! - hex: `hex_view.rs` has `a_hex_view_in_a_split_is_drawn_within_its_window` (a split, drawn
//!   within its window, `Esc`), `the_cursor_goes_with_the_screen_when_history_shows_it_again`,
//!   `the_hex_command_toggles_the_view`; not the window motions, resizing, `:q` or `ga`.
//! - page: `markdown_view.rs` covers the page itself (width, keys, `V`, links, history); not the
//!   window motions, resizing or `:q` of a window with a page.
//! - diff: `history.rs`, `hunks.rs`, `git_log.rs` cover what the diff does; not it as a window.
//! - debug panel: `debug/panel.rs` covers its keys and `Esc`/`q`/`P`/`C-g`; not where it goes,
//!   resizing, `:q` or the window motions.
//! - terminal: `terminal.rs` has the position, the motions, resizing and `:q`.
//! - modes with a screen: `modes.rs` has `Esc` on a page in either mode; not the keys' order
//!   (`screens::modes_first`) or watch mode following with a screen focused.
//!
//! The window keys of this fork's keymap are the splits, `q`, `o`, the motions and the moves
//! (`H J K L`, `t`): it has no keys to resize a window, so the sizes here change with the
//! terminal.
//!
//! Every test runs in a throwaway repository (the diff needs one), like those of `modes.rs`.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use termina::event::Event;
use vortex_term::modes;
use vortex_view::{
    current_ref, debug::config::PanelConfig, editor::MarkdownViewStartup, graphics::Rect,
    screen::ScreenKind, terminal::TerminalPosition, tree::Window, ViewId,
};

use super::git_log::{git, on_screen, status, Repo, Session};
use super::helpers::{test_config, AppBuilder};
use super::uncommitted::{cwd, lines};

fn enter(test: &str) -> Option<PathBuf> {
    Repo::enter_with(module_path!(), test, make)
}

/// `a.txt` of 20 lines with three changes in the work tree (three hunks), `b.txt` and `c.txt`
/// of ten lines, `notes.md` and `other.md` (Markdown), `data.bin` (not text).
fn make(parent: &Path) -> PathBuf {
    let work = parent.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, "", &["init", "-q"]);
    std::fs::write(work.join("a.txt"), lines("line ", 20)).unwrap();
    std::fs::write(work.join("b.txt"), lines("b", 10)).unwrap();
    std::fs::write(work.join("c.txt"), lines("c", 10)).unwrap();
    std::fs::write(
        work.join("notes.md"),
        "# Notes\n\nSome text of the notes.\n\n## More\n\nMore text.\n\n- one\n- two\n",
    )
    .unwrap();
    std::fs::write(
        work.join("other.md"),
        "# Other\n\nSome text of the other page.\n\n## Again\n\nAgain text.\n",
    )
    .unwrap();
    std::fs::write(work.join("data.bin"), b"\x00\x01\x02binary\xff\xfe\n").unwrap();
    git(&work, "", &["add", "-A"]);
    git(&work, "", &["commit", "-q", "-m", "Start"]);
    let edited = lines("line ", 20)
        .replace("line 3\n", "three\n")
        .replace("line 10\n", "ten\n")
        .replace("line 17\n", "seventeen\n");
    std::fs::write(work.join("a.txt"), edited).unwrap();
    work
}

// ---------------------------------------------------------------------------------------------
// The session

/// A session on `file` of the repository, in a terminal of `cols` x 30.
async fn session_on(file: &str, cols: u16) -> Session {
    session_with(file, test_config(), cols, 30).await
}

async fn session_with(
    file: &str,
    mut config: vortex_term::config::Config,
    cols: u16,
    rows: u16,
) -> Session {
    // a page only when `V` says so
    config.editor.markdown_view = MarkdownViewStartup::Never;
    let app = AppBuilder::new()
        .with_config(config)
        .with_file(cwd().join(file), None)
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    // (set as a theme is, so that the highlighting of the buffers has its scopes)
    let theme = toml::from_str::<toml::Value>("\"ui.selection\" = { bg = \"#333333\" }").unwrap();
    session
        .app
        .editor
        .set_theme(vortex_view::Theme::from(theme))
        .unwrap();
    resize(&mut session, cols, rows).await;
    session
}

/// Gives the terminal another size, which the application picks up with the next event.
pub(super) async fn resize(session: &mut Session, cols: u16, rows: u16) {
    session.app.resize_screen(cols, rows);
    session
        .tx
        .send(Ok(Event::WindowResized(termina::WindowSize {
            cols,
            rows,
            pixel_width: None,
            pixel_height: None,
        })))
        .unwrap();
    assert!(session.app.event_loop_until_idle(&mut session.input).await);
}

fn focus(session: &Session) -> ViewId {
    session.app.editor.tree.focus
}

/// The tree, for the messages of failed assertions.
fn describe(session: &Session) -> String {
    let editor = &session.app.editor;
    let mut out = format!("focus {:?}:", editor.tree.focus);
    for (window, focused) in editor.tree.windows() {
        let what = match window {
            Window::Text(view) => format!("text of {:?}", view.doc),
            Window::Screen(view) => format!("{:?}", editor.screens.kind(view.screen)),
        };
        out += &format!(
            " [{:?} {what} {:?}{}]",
            window.id(),
            window.area(),
            if focused { " *" } else { "" }
        );
    }
    out
}

fn focused_kind(session: &Session) -> Option<ScreenKind> {
    session
        .app
        .editor
        .focused_screen()
        .map(|screen| screen.kind)
}

/// The windows shown (not parked), with their areas, in the order of the tree.
fn windows(session: &Session) -> Vec<(ViewId, Rect)> {
    let mut windows: Vec<_> = session
        .app
        .editor
        .tree
        .windows()
        .map(|(window, _)| (window.id(), window.area()))
        .collect();
    windows.sort_by_key(|(_, area)| (area.y, area.x));
    windows
}

fn area_of(session: &Session, view: ViewId) -> Rect {
    session
        .app
        .editor
        .tree
        .window(view)
        .expect("a window")
        .area()
}

fn is_text(session: &Session, view: ViewId) -> bool {
    matches!(session.app.editor.tree.window(view), Some(Window::Text(_)))
}

/// The line (from 0) of the cursor of the text window the commands act on.
fn cursor_line(session: &Session) -> usize {
    let (view, doc) = current_ref!(session.app.editor);
    doc.selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..))
}

fn doc_name(session: &Session) -> String {
    let (_, doc) = current_ref!(session.app.editor);
    doc.path()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The rows of the screen inside `area`, without trailing spaces.
fn rows_in(session: &Session, area: Rect) -> Vec<String> {
    session
        .rows()
        .iter()
        .skip(area.y as usize)
        .take(area.height as usize)
        .map(|row| {
            row.chars()
                .skip(area.x as usize)
                .take(area.width as usize)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The rows of the screen outside the editor's area: they have nothing on them.
fn rows_below(session: &Session, area: Rect) -> Vec<String> {
    session
        .rows()
        .iter()
        .skip(area.bottom() as usize)
        .map(|row| row.trim_end().to_string())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The kinds of screens that cover a window

#[derive(Clone, Copy, Debug)]
enum Kind {
    Hex,
    Markdown,
    Diff,
}

impl Kind {
    fn screen(self) -> ScreenKind {
        match self {
            Kind::Hex => ScreenKind::Hex,
            Kind::Markdown => ScreenKind::Markdown,
            Kind::Diff => ScreenKind::Diff,
        }
    }

    /// The file of the window it opens in.
    fn file(self) -> &'static str {
        match self {
            Kind::Hex | Kind::Diff => "a.txt",
            Kind::Markdown => "notes.md",
        }
    }

    /// A line of the file to put the cursor on (from 0): a page shows the lines with text, so
    /// that is where the cursor comes back to.
    fn line(self) -> usize {
        match self {
            Kind::Markdown => 2,
            Kind::Hex | Kind::Diff => 3,
        }
    }

    /// The keys that open it in the focused window.
    fn open(self) -> &'static str {
        match self {
            Kind::Hex => ":hex<ret>",
            Kind::Markdown => "V",
            Kind::Diff => "<space>=",
        }
    }

    /// Whether `rows` (the rows of a window) show the screen.
    fn drawn(self, rows: &[String]) -> bool {
        match self {
            Kind::Hex => on_screen(rows, "0x00000000"),
            Kind::Markdown => on_screen(rows, "Notes") && !on_screen(rows, "# Notes"),
            Kind::Diff => on_screen(rows, "hunk "),
        }
    }

    /// Whether `rows` show the text of the file.
    fn text(self, rows: &[String]) -> bool {
        match self {
            Kind::Hex | Kind::Diff => on_screen(rows, TEXT_ROW) && !on_screen(rows, "hunk "),
            Kind::Markdown => on_screen(rows, "# Notes"),
        }
    }
}

/// The second row of `a.txt` in a text window (with its line number).
const TEXT_ROW: &str = "    2  line";

/// Opens `kind` in the focused window and waits for it to be drawn.
async fn open(session: &mut Session, kind: Kind) {
    session.keys(kind.open()).await;
    session
        .wait_for_screen("the screen", |rows| {
            // (the page and the hex view are drawn at once; the diff reads git)
            !matches!(kind, Kind::Diff) || on_screen(rows, "hunk ")
        })
        .await;
    assert_eq!(focused_kind(session), Some(kind.screen()), "{kind:?}");
}

// ---------------------------------------------------------------------------------------------
// 1. In the current window

/// The screen covers the window it opens in: the window is parked under it and comes back, with
/// its buffer and cursor, on `Esc`.
async fn covers_the_current_window(kind: Kind) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys(&format!("{}j", kind.line())).await;
    let text = focus(&session);
    let area = area_of(&session, text);
    assert!(is_text(&session, text));
    assert_eq!(cursor_line(&session), kind.line());
    assert!(kind.text(&rows_in(&session, area)), "{:#?}", session.rows());

    open(&mut session, kind).await;
    let screen = focus(&session);
    assert_ne!(screen, text);
    assert_eq!(session.app.editor.tree.under(screen), Some(text));
    assert!(session.app.editor.tree.is_parked(text));
    assert_eq!(windows(&session), vec![(screen, area)], "{kind:?}");
    assert!(
        kind.drawn(&rows_in(&session, area)),
        "{:#?}",
        session.rows()
    );
    assert!(
        !kind.text(&rows_in(&session, area)),
        "{:#?}",
        session.rows()
    );

    session.keys("<esc>").await;
    assert_eq!(focused_kind(&session), None, "{kind:?}");
    assert_eq!(focus(&session), text);
    assert!(!session.app.editor.tree.is_parked(text));
    assert_eq!(windows(&session), vec![(text, area)]);
    assert_eq!(doc_name(&session), kind.file());
    assert_eq!(cursor_line(&session), kind.line());
    assert!(kind.text(&rows_in(&session, area)), "{:#?}", session.rows());
    assert!(
        !kind.drawn(&rows_in(&session, area)),
        "{:#?}",
        session.rows()
    );
}

/// `ga` from the screen: the window shows what it showed, at the same cursor.
async fn ga_goes_back_to_the_covered_buffer(kind: Kind) {
    let mut session = session_on(kind.file(), 100).await;
    let line = kind.line() + 4;
    session.keys(&format!("{line}j")).await;
    let text = focus(&session);
    let area = area_of(&session, text);
    open(&mut session, kind).await;

    session.keys("ga").await;
    assert_eq!(focused_kind(&session), None, "{kind:?}");
    assert_eq!(focus(&session), text);
    assert_eq!(windows(&session), vec![(text, area)]);
    assert_eq!(doc_name(&session), kind.file());
    assert_eq!(cursor_line(&session), line, "{kind:?}");
    assert!(kind.text(&rows_in(&session, area)), "{:#?}", session.rows());
}

/// A modal layer of the screen takes `Esc` before the window is given back.
async fn esc_goes_to_the_modal_layer_first(kind: Kind) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys(&format!("{}j", kind.line())).await;
    let text = focus(&session);
    open(&mut session, kind).await;
    let screen = focus(&session);

    // the keys help
    session.keys("?").await;
    assert!(
        on_screen(&session.rows(), "Esc") || on_screen(&session.rows(), "esc"),
        "{:#?}",
        session.rows()
    );
    session.keys("<esc>").await;
    assert_eq!(
        focus(&session),
        screen,
        "{kind:?}: Esc closed the help only"
    );
    assert_eq!(focused_kind(&session), Some(kind.screen()));
    session.keys("<esc>").await;
    assert_eq!(focus(&session), text, "{kind:?}: the second Esc closes");
    assert_eq!(focused_kind(&session), None);
    assert_eq!(cursor_line(&session), kind.line());
}

/// In a split, only the window it opened in shows it: the other one still shows its text, the
/// screen is drawn within its window, and both come back.
async fn opens_in_a_split(kind: Kind, split: &str) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys(&format!("{}j", kind.line())).await;
    session.keys(split).await;
    let before = windows(&session);
    assert_eq!(before.len(), 2, "{kind:?}");
    let (first, second) = (before[0], before[1]);
    assert_eq!(focus(&session), second.0);

    open(&mut session, kind).await;
    let screen = focus(&session);
    assert_eq!(session.app.editor.tree.under(screen), Some(second.0));
    assert_eq!(
        windows(&session),
        vec![first, (screen, second.1)],
        "{kind:?}"
    );
    // the other window is text, as it was
    assert!(is_text(&session, first.0));
    assert!(
        kind.text(&rows_in(&session, first.1)),
        "{:#?}",
        session.rows()
    );
    assert!(
        !kind.drawn(&rows_in(&session, first.1)),
        "{:#?}",
        session.rows()
    );
    // and the screen is in its own
    assert!(
        kind.drawn(&rows_in(&session, second.1)),
        "{:#?}",
        session.rows()
    );
    assert!(
        !kind.text(&rows_in(&session, second.1)),
        "{:#?}",
        session.rows()
    );

    // `ga` gives the second window its text, the first is untouched
    session.keys("ga").await;
    assert_eq!(focused_kind(&session), None);
    assert_eq!(windows(&session), before, "{kind:?}");
    assert_eq!(cursor_line(&session), kind.line());
    assert!(
        kind.text(&rows_in(&session, second.1)),
        "{:#?}",
        session.rows()
    );

    // `Esc` too
    open(&mut session, kind).await;
    session.keys("<esc>").await;
    assert_eq!(windows(&session), before, "{kind:?}");
    assert!(is_text(&session, second.0));
    assert!(kind.text(&rows_in(&session, first.1)));
    assert!(kind.text(&rows_in(&session, second.1)));
}

/// The window motions go to the screen and leave it.
async fn window_motions(kind: Kind, split: &str, away: char, back: char) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys(split).await;
    let [(first, _), (second, _)] = windows(&session)[..] else {
        panic!("two windows")
    };
    open(&mut session, kind).await;
    let screen = focus(&session);
    assert_eq!(session.app.editor.tree.under(screen), Some(second));

    session.keys(&format!("<C-w>{away}")).await;
    assert_eq!(
        focus(&session),
        first,
        "{kind:?}: C-w {away}: {}",
        describe(&session)
    );
    assert_eq!(focused_kind(&session), None);
    session.keys(&format!("<C-w>{back}")).await;
    assert_eq!(focus(&session), screen, "{kind:?}: C-w {back}");
    assert_eq!(focused_kind(&session), Some(kind.screen()));

    // `C-w w` goes round
    session.keys("<C-w>w").await;
    assert_eq!(focus(&session), first, "{kind:?}: C-w w");
    session.keys("<C-w>w").await;
    assert_eq!(focus(&session), screen, "{kind:?}: C-w w again");
    // the same through the space menu
    session.keys(&format!("<space>w{away}")).await;
    assert_eq!(focus(&session), first, "{kind:?}: space w {away}");
    session.keys(&format!("<space>w{back}")).await;
    assert_eq!(focus(&session), screen, "{kind:?}: space w {back}");
    // the way out of the edge stays where it is
    session.keys(&format!("<C-w>{back}")).await;
    assert_eq!(focus(&session), screen, "{kind:?}: nothing beyond");
    // and the screen is still there, undisturbed
    assert_eq!(windows(&session).len(), 2);
    assert!(kind.drawn(&rows_in(&session, area_of(&session, screen))));
}

/// A terminal of another size: the screen follows its window, the other window and the rows
/// outside of the editor are not drawn over.
async fn resizes_with_the_terminal(kind: Kind, split: &str) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys(split).await;
    open(&mut session, kind).await;
    let screen = focus(&session);

    for (cols, rows) in [(140, 40), (100, 30), (80, 20), (130, 24), (100, 30)] {
        resize(&mut session, cols, rows).await;
        if matches!(kind, Kind::Diff) {
            session
                .wait_for_screen("the diff", |rows| on_screen(rows, "hunk "))
                .await;
        }
        let tree = session.app.editor.tree.area();
        assert_eq!((tree.width, tree.right()), (cols, cols), "{kind:?}");
        assert!(tree.bottom() <= rows, "{kind:?} {tree:?}");
        let [(_, first_area), (shown, screen_area)] = windows(&session)[..] else {
            panic!("two windows: {:?}", windows(&session))
        };
        assert_eq!(shown, screen, "{kind:?} at {cols}x{rows}");
        // they share the editor's area, with nothing in between and nothing outside of it
        let cells = |area: Rect| u32::from(area.width) * u32::from(area.height);
        assert!(
            cells(first_area) + cells(screen_area) >= cells(tree) - u32::from(tree.width) * 2,
            "{kind:?} at {cols}x{rows}: {first_area:?} {screen_area:?} in {tree:?}"
        );
        for area in [first_area, screen_area] {
            assert!(
                area.x >= tree.x
                    && area.right() <= tree.right()
                    && area.y >= tree.y
                    && area.bottom() <= tree.bottom(),
                "{kind:?} at {cols}x{rows}: {area:?} out of {tree:?}"
            );
        }
        assert!(
            first_area.right() <= screen_area.x || first_area.bottom() <= screen_area.y,
            "{kind:?} at {cols}x{rows}: {first_area:?} {screen_area:?}"
        );
        let shown_rows = rows_in(&session, screen_area);
        assert!(
            kind.drawn(&shown_rows),
            "{kind:?} at {cols}x{rows}:\n{:#?}",
            session.rows()
        );
        let text_rows = rows_in(&session, first_area);
        assert!(
            kind.text(&text_rows) && !kind.drawn(&text_rows),
            "{kind:?} at {cols}x{rows}:\n{:#?}",
            session.rows()
        );
        // nothing of the screen below the editor's area (the command line row aside)
        assert!(
            rows_below(&session, tree)
                .iter()
                .all(|row| !kind.drawn(std::slice::from_ref(row))),
            "{kind:?} at {cols}x{rows}"
        );
    }
}

/// `:q` on the screen's window in a split closes it and the window it covered: the other window
/// is what is left.
async fn q_closes_the_window_in_a_split(kind: Kind) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys("<C-w>v").await;
    let [(first, first_area), (second, _)] = windows(&session)[..] else {
        panic!("two windows")
    };
    open(&mut session, kind).await;
    session.keys(":q<ret>").await;
    assert_eq!(focused_kind(&session), None, "{kind:?}");
    let left = windows(&session);
    assert_eq!(left.len(), 1, "{kind:?}: {left:?}");
    assert_eq!(left[0].0, first);
    assert!(!session.app.editor.tree.contains(second), "{kind:?}");
    assert!(is_text(&session, first));
    // it has the whole editor now
    assert!(area_of(&session, first).width > first_area.width);
    assert!(kind.text(&rows_in(&session, area_of(&session, first))));
    assert!(session.app.editor.screens.find(kind.screen()).is_none());
}

/// `:q` on the screen of the only window quits the editor, as it does on a text window.
async fn q_on_the_only_window_quits(kind: Kind) {
    let mut session = session_on(kind.file(), 100).await;
    open(&mut session, kind).await;
    assert!(session.quits_on(":q<ret>").await, "{kind:?}");
}

macro_rules! per_kind {
    ($($(#[$attr:meta])* $name:ident: $body:ident($($arg:expr),*);)*) => {$(
        #[tokio::test(flavor = "multi_thread")]
        $(#[$attr])*
        async fn $name() {
            let Some(_) = enter(stringify!($name)) else {
                return;
            };
            $body($($arg),*).await;
        }
    )*};
}

per_kind! {
    the_hex_view_covers_the_current_window: covers_the_current_window(Kind::Hex);
    the_page_covers_the_current_window: covers_the_current_window(Kind::Markdown);
    the_diff_covers_the_current_window: covers_the_current_window(Kind::Diff);

    ga_on_the_hex_view_goes_back: ga_goes_back_to_the_covered_buffer(Kind::Hex);
    ga_on_the_page_goes_back: ga_goes_back_to_the_covered_buffer(Kind::Markdown);
    ga_on_the_diff_goes_back: ga_goes_back_to_the_covered_buffer(Kind::Diff);

    esc_on_the_hex_view_gives_the_help_its_turn: esc_goes_to_the_modal_layer_first(Kind::Hex);
    esc_on_the_page_gives_the_help_its_turn: esc_goes_to_the_modal_layer_first(Kind::Markdown);
    esc_on_the_diff_gives_the_help_its_turn: esc_goes_to_the_modal_layer_first(Kind::Diff);

    the_hex_view_in_a_vertical_split: opens_in_a_split(Kind::Hex, "<C-w>v");
    the_page_in_a_vertical_split: opens_in_a_split(Kind::Markdown, "<C-w>v");
    the_diff_in_a_vertical_split: opens_in_a_split(Kind::Diff, "<C-w>v");
    the_hex_view_in_a_horizontal_split: opens_in_a_split(Kind::Hex, "<C-w>s");
    the_page_in_a_horizontal_split: opens_in_a_split(Kind::Markdown, "<C-w>s");
    the_diff_in_a_horizontal_split: opens_in_a_split(Kind::Diff, "<C-w>s");

    window_motions_on_the_hex_view_side_by_side: window_motions(Kind::Hex, "<C-w>v", 'h', 'l');
    window_motions_on_the_page_side_by_side: window_motions(Kind::Markdown, "<C-w>v", 'h', 'l');
    window_motions_on_the_diff_side_by_side: window_motions(Kind::Diff, "<C-w>v", 'h', 'l');
    window_motions_on_the_hex_view_stacked: window_motions(Kind::Hex, "<C-w>s", 'k', 'j');
    window_motions_on_the_page_stacked: window_motions(Kind::Markdown, "<C-w>s", 'k', 'j');
    window_motions_on_the_diff_stacked: window_motions(Kind::Diff, "<C-w>s", 'k', 'j');

    the_hex_view_follows_the_terminal_side_by_side: resizes_with_the_terminal(Kind::Hex, "<C-w>v");
    the_page_follows_the_terminal_side_by_side: resizes_with_the_terminal(Kind::Markdown, "<C-w>v");
    the_diff_follows_the_terminal_side_by_side: resizes_with_the_terminal(Kind::Diff, "<C-w>v");
    the_hex_view_follows_the_terminal_stacked: resizes_with_the_terminal(Kind::Hex, "<C-w>s");
    the_page_follows_the_terminal_stacked: resizes_with_the_terminal(Kind::Markdown, "<C-w>s");
    the_diff_follows_the_terminal_stacked: resizes_with_the_terminal(Kind::Diff, "<C-w>s");

    q_on_the_hex_view_in_a_split: q_closes_the_window_in_a_split(Kind::Hex);
    q_on_the_page_in_a_split: q_closes_the_window_in_a_split(Kind::Markdown);
    q_on_the_diff_in_a_split: q_closes_the_window_in_a_split(Kind::Diff);

    q_on_the_hex_view_of_the_only_window: q_on_the_only_window_quits(Kind::Hex);
    q_on_the_page_of_the_only_window: q_on_the_only_window_quits(Kind::Markdown);
    q_on_the_diff_of_the_only_window: q_on_the_only_window_quits(Kind::Diff);
}

/// The hex view's own modal layer: `Esc` leaves overwriting first, then closes the view.
#[tokio::test(flavor = "multi_thread")]
async fn esc_on_the_hex_view_leaves_overwriting_first() {
    let Some(_) = enter("esc_on_the_hex_view_leaves_overwriting_first") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    let text = focus(&session);
    open(&mut session, Kind::Hex).await;
    let screen = focus(&session);
    // `i` starts overwriting: the view takes every key, Esc stops it
    session.keys("i").await;
    session.keys("<esc>").await;
    assert_eq!(focus(&session), screen, "Esc stopped overwriting only");
    assert_eq!(focused_kind(&session), Some(ScreenKind::Hex));
    // `v` selects: Esc drops the selection first
    session.keys("v").await;
    session.keys("<esc>").await;
    assert_eq!(focus(&session), screen, "Esc dropped the selection only");
    session.keys("<esc>").await;
    assert_eq!(focus(&session), text);
    assert_eq!(focused_kind(&session), None);
}

// ---------------------------------------------------------------------------------------------
// The debug panel

fn panel_config(position: TerminalPosition, size: u16) -> vortex_term::config::Config {
    let mut config = test_config();
    config.editor.debugger.panel = PanelConfig { position, size };
    config
}

/// The panel is shown and focused with `C-g`, along `position`. Returns the session and the text
/// window's and the panel's windows.
async fn panel_at(
    position: TerminalPosition,
    size: u16,
    cols: u16,
    rows: u16,
) -> (Session, (ViewId, Rect), (ViewId, Rect)) {
    let mut session = session_with("a.txt", panel_config(position, size), cols, rows).await;
    let text = focus(&session);
    let full = area_of(&session, text);
    session.keys("<C-g>").await;
    assert!(session.app.editor.debug_panel_shown());
    assert!(session.app.editor.debug_panel_focused());
    assert_eq!(focused_kind(&session), Some(ScreenKind::DebugPanel));
    let panel = session.app.editor.debug_panel_view().unwrap();
    assert_eq!(focus(&session), panel);
    // it is a window of the tree beside the text window, which kept its place in it
    let windows = windows(&session);
    assert_eq!(windows.len(), 2, "{windows:?}");
    let panel_area = area_of(&session, panel);
    let text_area = area_of(&session, text);
    // they share what the text window had
    assert_eq!(
        text_area.union(panel_area),
        full,
        "{text_area:?} {panel_area:?}"
    );
    (session, (text, text_area), (panel, panel_area))
}

fn near(actual: u16, expected: f64, tolerance: f64) -> bool {
    (f64::from(actual) - expected).abs() <= tolerance
}

/// Without `[editor.debugger.panel]` the panel is along the bottom, a third of the editor high.
#[tokio::test(flavor = "multi_thread")]
async fn the_panel_is_at_the_bottom_by_default() {
    let Some(_) = enter("the_panel_is_at_the_bottom_by_default") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    let text = focus(&session);
    let full = area_of(&session, text);
    session.keys("<C-g>").await;
    let panel = session.app.editor.debug_panel_view().expect("shown");
    let panel_area = area_of(&session, panel);
    let text_area = area_of(&session, text);
    assert_eq!(panel_area.width, full.width);
    assert_eq!(panel_area.x, full.x);
    assert_eq!(panel_area.bottom(), full.bottom());
    assert_eq!(text_area.y, full.y);
    assert_eq!(text_area.bottom(), panel_area.y);
    assert!(
        near(panel_area.height, f64::from(full.height) * 0.33, 1.5),
        "{panel_area:?} of {full:?}"
    );
    // the text window above still shows the buffer, the panel its own rows
    assert!(on_screen(&rows_in(&session, text_area), TEXT_ROW));
    let panel_rows = rows_in(&session, panel_area);
    assert!(!on_screen(&panel_rows, TEXT_ROW), "{:#?}", session.rows());
    assert!(
        panel_rows.iter().any(|row| !row.is_empty()),
        "something is drawn in the panel: {:#?}",
        session.rows()
    );
}

macro_rules! panel_places {
    ($($name:ident: $position:expr, $size:expr, |$text:ident, $panel:ident, $full:ident| $check:block;)*) => {$(
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some(_) = enter(stringify!($name)) else {
                return;
            };
            let (session, (_, $text), (_, $panel)) = panel_at($position, $size, 120, 40).await;
            let $full = session.app.editor.tree.area();
            $check
            // the panel draws within its own window and the text window keeps its text
            let panel_rows = rows_in(&session, $panel);
            assert!(panel_rows.iter().any(|row| !row.is_empty()), "{:#?}", session.rows());
            assert!(!on_screen(&panel_rows, TEXT_ROW), "{:#?}", session.rows());
            assert!(on_screen(&rows_in(&session, $text), TEXT_ROW), "{:#?}", session.rows());
        }
    )*};
}

panel_places! {
    the_panel_on_the_top: TerminalPosition::Top, 50, |text, panel, full| {
        assert_eq!(panel.y, 0);
        assert_eq!(panel.width, full.width);
        assert_eq!(panel.bottom(), text.y);
        assert!(near(panel.height, f64::from(text.height + panel.height) * 0.5, 1.5), "{panel:?} {text:?}");
    };
    the_panel_on_the_bottom: TerminalPosition::Bottom, 25, |text, panel, full| {
        assert_eq!(text.y, 0);
        assert_eq!(panel.x, 0);
        assert_eq!(panel.width, full.width);
        assert_eq!(text.bottom(), panel.y);
        assert!(near(panel.height, f64::from(text.height + panel.height) * 0.25, 1.5), "{panel:?} {text:?}");
    };
    the_panel_on_the_left: TerminalPosition::Left, 40, |text, panel, full| {
        assert_eq!(panel.x, 0);
        assert_eq!(panel.y, text.y);
        assert_eq!(panel.height, text.height);
        assert_eq!(panel.right() + 1, text.x, "a column between them");
        assert_eq!(text.right(), full.width);
        assert!(near(panel.width, f64::from(text.width + panel.width) * 0.4, 1.5), "{panel:?} {text:?}");
    };
    the_panel_on_the_right: TerminalPosition::Right, 60, |text, panel, full| {
        assert_eq!(text.x, 0);
        assert_eq!(panel.right(), full.width);
        assert_eq!(panel.height, text.height);
        assert_eq!(text.right() + 1, panel.x, "a column between them");
        assert!(near(panel.width, f64::from(text.width + panel.width) * 0.6, 1.5), "{panel:?} {text:?}");
    };
    the_panel_size_is_at_least_ten_percent: TerminalPosition::Bottom, 1, |text, panel, _full| {
        assert!(near(panel.height, f64::from(text.height + panel.height) * 0.10, 1.5), "{panel:?} {text:?}");
    };
    the_panel_size_is_at_most_ninety_percent: TerminalPosition::Right, 100, |text, panel, _full| {
        assert!(near(panel.width, f64::from(text.width + panel.width) * 0.90, 1.5), "{panel:?} {text:?}");
    };
}

/// The panel comes back where it was, at the size it had.
#[tokio::test(flavor = "multi_thread")]
async fn the_panel_comes_back_where_it_was() {
    let Some(_) = enter("the_panel_comes_back_where_it_was") else {
        return;
    };
    let (mut session, _, (panel, area)) = panel_at(TerminalPosition::Right, 30, 120, 40).await;
    let _ = panel;
    session.keys("<esc>").await;
    assert!(!session.app.editor.debug_panel_shown());
    assert_eq!(windows(&session).len(), 1);
    assert_eq!(focused_kind(&session), None);
    session.keys("<C-g>").await;
    let again = session.app.editor.debug_panel_view().expect("shown again");
    assert_eq!(area_of(&session, again), area);
}

/// With a split already there the panel still goes along the edge of the whole editor.
#[tokio::test(flavor = "multi_thread")]
async fn the_panel_goes_along_the_edge_of_the_editor_with_splits() {
    let Some(_) = enter("the_panel_goes_along_the_edge_of_the_editor_with_splits") else {
        return;
    };
    let mut session = session_on("a.txt", 120).await;
    session.keys("<C-w>v").await;
    let before = windows(&session);
    assert_eq!(before.len(), 2);
    session.keys("<C-g>").await;
    let panel = session.app.editor.debug_panel_view().unwrap();
    let area = area_of(&session, panel);
    assert_eq!(windows(&session).len(), 3);
    assert_eq!((area.x, area.width), (0, 120), "{area:?}");
    let tree = session.app.editor.tree.area();
    assert_eq!(area.bottom(), tree.bottom());
    // both text windows are above it, side by side
    for (id, _) in before {
        assert!(is_text(&session, id));
        assert!(area_of(&session, id).bottom() <= area.y);
    }
}

/// The window motions go to the panel and leave it (it is below the text window). With debug
/// mode on or off.
async fn panel_window_motions(debug_mode: bool) {
    let mut session = session_on("a.txt", 100).await;
    let text = focus(&session);
    // the panel is shown without taking the focus: `P` in debug mode does that
    session.keys("<space>GP").await;
    if !debug_mode {
        session.keys("<esc>").await;
    }
    assert_eq!(session.app.editor.debug.mode_on(), debug_mode);
    assert!(session.app.editor.debug_panel_shown());
    assert_eq!(focus(&session), text);
    let panel = session.app.editor.debug_panel_view().unwrap();
    let at = |session: &Session, view: ViewId, keys: &str| {
        assert_eq!(focus(session), view, "{keys}: {}", describe(session));
    };

    session.keys("<C-w>j").await;
    at(&session, panel, "C-w j");
    assert!(session.app.editor.debug_panel_focused());
    session.keys("<C-w>k").await;
    at(&session, text, "C-w k from the panel");
    assert!(!session.app.editor.debug_panel_focused());
    session.keys("<C-w>w").await;
    at(&session, panel, "C-w w");
    session.keys("<C-w>w").await;
    at(&session, text, "C-w w from the panel");
    // (`Space` is play and pause in debug mode: the menu is for editing)
    if !debug_mode {
        session.keys("<space>wj").await;
        at(&session, panel, "space w j");
        session.keys("<space>wk").await;
        at(&session, text, "space w k from the panel");
    }
    // the panel is still there
    assert!(session.app.editor.debug_panel_shown());
}

#[tokio::test(flavor = "multi_thread")]
async fn window_motions_go_to_and_from_the_panel_in_debug_mode() {
    let Some(_) = enter("window_motions_go_to_and_from_the_panel_in_debug_mode") else {
        return;
    };
    panel_window_motions(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn window_motions_go_to_and_from_the_panel() {
    let Some(_) = enter("window_motions_go_to_and_from_the_panel") else {
        return;
    };
    panel_window_motions(false).await;
}

/// A terminal of another size: the panel keeps its share of the editor and draws in its window.
#[tokio::test(flavor = "multi_thread")]
async fn the_panel_follows_the_terminal() {
    let Some(_) = enter("the_panel_follows_the_terminal") else {
        return;
    };
    for position in [TerminalPosition::Bottom, TerminalPosition::Right] {
        let (mut session, (text, _), (panel, _)) = panel_at(position, 40, 120, 40).await;
        for (cols, rows) in [(160, 50), (100, 30), (80, 24), (120, 40)] {
            resize(&mut session, cols, rows).await;
            let tree = session.app.editor.tree.area();
            assert_eq!((tree.width, tree.right()), (cols, cols));
            let text_area = area_of(&session, text);
            let panel_area = area_of(&session, panel);
            assert_eq!(
                text_area.union(panel_area),
                tree,
                "{position:?} at {cols}x{rows}: {text_area:?} {panel_area:?}"
            );
            let (along, whole) = match position {
                TerminalPosition::Bottom => (panel_area.height, tree.height),
                _ => (panel_area.width, tree.width),
            };
            assert!(
                near(along, f64::from(whole) * 0.4, 3.0),
                "{position:?} at {cols}x{rows}: {panel_area:?} of {tree:?}"
            );
            assert!(on_screen(&rows_in(&session, text_area), TEXT_ROW));
            let panel_rows = rows_in(&session, panel_area);
            assert!(!on_screen(&panel_rows, TEXT_ROW));
            assert!(panel_rows.iter().any(|row| !row.is_empty()));
            assert!(rows_below(&session, tree).iter().all(|row| row.is_empty()
                || !panel_rows
                    .iter()
                    .any(|p| !p.is_empty() && row.contains(p.as_str()))));
        }
    }
}

/// `Esc` hides the focused panel (the window goes, the text window has the focus again), and so
/// does `q`.
#[tokio::test(flavor = "multi_thread")]
async fn esc_and_q_hide_the_panel() {
    let Some(_) = enter("esc_and_q_hide_the_panel") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    let text = focus(&session);
    let full = area_of(&session, text);
    for key in ["<esc>", "q"] {
        session.keys("<C-g>").await;
        assert!(session.app.editor.debug_panel_focused(), "{key}");
        session.keys(key).await;
        assert!(!session.app.editor.debug_panel_shown(), "{key}");
        assert_eq!(focus(&session), text, "{key}");
        assert_eq!(windows(&session), vec![(text, full)], "{key}");
    }
}

/// `:q` on the focused panel closes its window (it is only hidden, not the editor): the text
/// window has the focus again.
#[tokio::test(flavor = "multi_thread")]
async fn q_on_the_panel_closes_its_window() {
    let Some(_) = enter("q_on_the_panel_closes_its_window") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    let text = focus(&session);
    session.keys("<C-g>").await;
    assert!(session.app.editor.debug_panel_focused());
    session.keys(":q<ret>").await;
    assert!(!session.app.editor.debug_panel_shown());
    assert_eq!(windows(&session).len(), 1);
    assert_eq!(doc_name(&session), "a.txt");
    assert!(
        session.app.editor.tree.is_visible(focus(&session)),
        "the focus is on a window that isn't shown: {}",
        describe(&session)
    );
    assert_eq!(focus(&session), text, "{}", describe(&session));
    assert!(!session.app.editor.debug_panel_focused());
    // `C-g` focuses it again
    session.keys("<C-g>").await;
    assert!(session.app.editor.debug_panel_focused());
}

/// `ga` on the panel: a screen's own window has no history, the panel stays; `C-g` goes back.
#[tokio::test(flavor = "multi_thread")]
async fn ga_on_the_panel_leaves_it_where_it_is() {
    let Some(_) = enter("ga_on_the_panel_leaves_it_where_it_is") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    let text = focus(&session);
    session.keys("3j<C-g>").await;
    let panel = focus(&session);
    assert!(session.app.editor.debug_panel_focused());
    session.keys("ga").await;
    assert!(
        session.app.editor.debug_panel_focused(),
        "{}",
        describe(&session)
    );
    assert_eq!(focus(&session), panel);
    assert_eq!(windows(&session).len(), 2);
    assert!(on_screen(&session.rows(), "atch"), "{:#?}", session.rows());
    session.keys("<esc><C-g>").await;
    assert_eq!(focus(&session), text);
    assert!(session.app.editor.debug_panel_shown());
    assert_eq!(cursor_line(&session), 3);
}

/// A page, a hex view or a diff covering the only text window with the panel shown: `:q` on it
/// hides that window instead of leaving the editor without a text window, and the editor stays.
async fn q_on_a_cover_of_the_last_text_window_keeps_the_panel(kind: Kind) {
    let mut session = session_on(kind.file(), 100).await;
    session.keys("<C-g><C-g>").await;
    assert!(session.app.editor.debug_panel_shown());
    open(&mut session, kind).await;
    assert!(
        !session.quits_on(":q<ret>").await,
        "{kind:?}: the panel keeps the editor"
    );
    assert!(session.app.editor.debug_panel_shown(), "{kind:?}");
    assert!(session.app.editor.screens.find(kind.screen()).is_none());
    // a document opens again beside it
    session.keys(":open b.txt<ret>").await;
    assert_eq!(doc_name(&session), "b.txt");
    assert!(session.app.editor.debug_panel_shown());
    assert!(is_text(&session, focus(&session)));
}

per_kind! {
    q_on_the_hex_view_over_the_last_text_window: q_on_a_cover_of_the_last_text_window_keeps_the_panel(Kind::Hex);
    q_on_the_page_over_the_last_text_window: q_on_a_cover_of_the_last_text_window_keeps_the_panel(Kind::Markdown);
    q_on_the_diff_over_the_last_text_window: q_on_a_cover_of_the_last_text_window_keeps_the_panel(Kind::Diff);
}

// ---------------------------------------------------------------------------------------------
// `V` in one window

/// `V` renders the page in its own window only: another window of a Markdown buffer is not
/// touched, and `V` again gives the text back.
#[tokio::test(flavor = "multi_thread")]
async fn v_toggles_the_page_in_its_window_only() {
    let Some(_) = enter("v_toggles_the_page_in_its_window_only") else {
        return;
    };
    let mut session = session_on("notes.md", 120).await;
    session.keys("<C-w>v").await;
    session.keys(":open other.md<ret>").await;
    let [(first, first_area), (second, second_area)] = windows(&session)[..] else {
        panic!("two windows")
    };
    assert_eq!(doc_name(&session), "other.md");
    let notes = |rows: &[String]| on_screen(rows, "# Notes");
    let other = |rows: &[String]| on_screen(rows, "# Other");
    assert!(notes(&rows_in(&session, first_area)));
    assert!(other(&rows_in(&session, second_area)));

    // `V` in the second window: its page only
    session.keys("V").await;
    assert_eq!(focused_kind(&session), Some(ScreenKind::Markdown));
    let page = focus(&session);
    assert_eq!(
        windows(&session),
        vec![(first, first_area), (page, second_area)]
    );
    assert!(on_screen(&rows_in(&session, second_area), "Other"));
    assert!(!other(&rows_in(&session, second_area)));
    assert!(
        notes(&rows_in(&session, first_area)),
        "the first window is untouched"
    );
    assert!(is_text(&session, first));

    // `V` in the first one: a page of its own, the second is as it was
    session.keys("<C-w>h").await;
    assert_eq!(focus(&session), first);
    session.keys("V").await;
    let first_page = focus(&session);
    assert_ne!(first_page, page);
    assert_eq!(focused_kind(&session), Some(ScreenKind::Markdown));
    assert_eq!(
        windows(&session),
        vec![(first_page, first_area), (page, second_area)]
    );
    assert!(on_screen(&rows_in(&session, first_area), "Notes"));
    assert!(!notes(&rows_in(&session, first_area)));
    assert!(on_screen(&rows_in(&session, second_area), "Other"));
    assert!(!other(&rows_in(&session, second_area)));

    // `V` again in the first: its text is back, the second still shows its page
    session.keys("V").await;
    assert_eq!(focus(&session), first);
    assert_eq!(focused_kind(&session), None);
    assert_eq!(
        windows(&session),
        vec![(first, first_area), (page, second_area)]
    );
    assert!(notes(&rows_in(&session, first_area)));
    assert!(on_screen(&rows_in(&session, second_area), "Other"));
    assert!(!other(&rows_in(&session, second_area)));

    // and in the second
    session.keys("<C-w>l").await;
    assert_eq!(focus(&session), page);
    session.keys("V").await;
    assert_eq!(focused_kind(&session), None);
    assert_eq!(
        windows(&session),
        vec![(first, first_area), (second, second_area)]
    );
    assert!(other(&rows_in(&session, second_area)));
    assert!(notes(&rows_in(&session, first_area)));
}

/// The same buffer in two splits: `V` in one renders that window only.
#[tokio::test(flavor = "multi_thread")]
async fn v_with_the_same_buffer_in_two_windows() {
    let Some(_) = enter("v_with_the_same_buffer_in_two_windows") else {
        return;
    };
    let mut session = session_on("notes.md", 120).await;
    session.keys("2j<C-w>v").await;
    let [(first, first_area), (second, second_area)] = windows(&session)[..] else {
        panic!("two windows")
    };
    session.keys("4j").await;
    let doc = session
        .app
        .editor
        .document_by_path(cwd().join("notes.md"))
        .unwrap()
        .id();
    let line_in = |session: &Session, view: ViewId| {
        let doc = session.app.editor.document(doc).unwrap();
        doc.selection(view)
            .primary()
            .cursor_line(doc.text().slice(..))
    };
    assert_eq!(
        (line_in(&session, first), line_in(&session, second)),
        (2, 6),
        "{}",
        describe(&session)
    );

    session.keys("V").await;
    let page = focus(&session);
    assert_eq!(focused_kind(&session), Some(ScreenKind::Markdown));
    assert!(on_screen(&rows_in(&session, second_area), "Notes"));
    assert!(!on_screen(&rows_in(&session, second_area), "# Notes"));
    assert!(
        on_screen(&rows_in(&session, first_area), "# Notes"),
        "{:#?}",
        session.rows()
    );
    assert_eq!(line_in(&session, first), 2, "the other window's cursor");

    session.keys("V").await;
    assert_eq!(focus(&session), second);
    assert_eq!(focused_kind(&session), None);
    assert!(!session.app.editor.tree.contains(page) || session.app.editor.tree.is_parked(page));
    assert_eq!(
        (line_in(&session, first), line_in(&session, second)),
        (2, 6)
    );
    assert!(on_screen(&rows_in(&session, first_area), "# Notes"));
    assert!(on_screen(&rows_in(&session, second_area), "# Notes"));
}

// ---------------------------------------------------------------------------------------------
// The modes with a screen focused

/// The line (from 0) of the cursor in window `view`, and the name of its buffer.
fn place_in(session: &Session, view: ViewId) -> (String, usize) {
    let tree_view = session.app.editor.tree.get(view);
    let doc = session.app.editor.document(tree_view.doc).unwrap();
    let name = doc
        .path()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (
        name,
        doc.selection(view)
            .primary()
            .cursor_line(doc.text().slice(..)),
    )
}

fn edit(name: &str, number: usize, new: &str) {
    let path = cwd().join(name);
    let old = std::fs::read_to_string(&path).unwrap();
    let new: String = old
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            if i + 1 == number {
                format!("{new}\n")
            } else {
                line.to_string()
            }
        })
        .collect();
    std::fs::write(path, new).unwrap();
}

/// Watch mode with a screen focused: the text window follows the edits made outside, the screen
/// keeps the focus and is as it was.
async fn watch_follows_with_a_screen_focused(screen: ScreenKind) {
    let mut session = session_on("a.txt", 120).await;
    let text = focus(&session);
    // the screen beside the text window, focused
    match screen {
        ScreenKind::DebugPanel => session.keys("<C-g>").await,
        ScreenKind::Hex => {
            session.keys("<C-w>v:open data.bin<ret>").await;
        }
        ScreenKind::Markdown => {
            session.keys("<C-w>v:open notes.md<ret>V").await;
        }
        _ => unreachable!(),
    }
    assert_eq!(focused_kind(&session), Some(screen));
    let screen_view = focus(&session);
    session.keys(":watch<ret>").await;
    assert!(session.app.editor.watch.is_active());
    assert_eq!(focus(&session), screen_view);
    session.run_for(Duration::from_millis(300)).await;
    let screen_rows_before = rows_in(&session, area_of(&session, screen_view));

    edit("c.txt", 7, "c7 agent");
    let deadline = Instant::now() + Duration::from_secs(20);
    let followed = loop {
        // the text window of the editor: the one watch mode moves
        let found = session
            .app
            .editor
            .tree
            .views()
            .map(|(view, _)| view.id)
            .find(|&id| place_in(&session, id) == ("c.txt".to_string(), 6));
        if let Some(id) = found {
            break id;
        }
        assert!(
            Instant::now() < deadline,
            "watch mode didn't follow c.txt with {screen:?} focused: {:#?}",
            session.rows()
        );
        session.run_for(Duration::from_millis(50)).await;
    };
    assert_eq!(
        focused_kind(&session),
        Some(screen),
        "{screen:?} keeps the focus"
    );
    assert_eq!(focus(&session), screen_view);
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Watch)
    );
    // the text window shows c.txt at the line, and its text has the edit
    let doc = session.app.editor.tree.get(followed).doc;
    let doc = session.app.editor.document(doc).unwrap();
    assert!(doc.text().to_string().contains("c7 agent"));
    let view = session.app.editor.tree.get(followed);
    let line = doc
        .selection(followed)
        .primary()
        .cursor_line(doc.text().slice(..));
    assert_eq!(line, 6);
    let _ = text;
    assert!(
        on_screen(&rows_in(&session, view.area), "c7 agent"),
        "{:#?}",
        session.rows()
    );
    // the screen is as it was (the debug panel redraws its own rows)
    let after = rows_in(&session, area_of(&session, screen_view));
    assert_eq!(after, screen_rows_before, "{screen:?} was changed");
    assert!(!on_screen(&after, "c7 agent"));
}

per_kind! {
    watch_mode_follows_with_the_panel_focused: watch_follows_with_a_screen_focused(ScreenKind::DebugPanel);
    watch_mode_follows_with_the_hex_view_focused: watch_follows_with_a_screen_focused(ScreenKind::Hex);
    watch_mode_follows_with_a_page_focused: watch_follows_with_a_screen_focused(ScreenKind::Markdown);
}

/// Debug mode with the hex view focused: its stepping keys reach debug mode, not the view, while
/// the view isn't in a modal state (`screens::modes_first`).
#[tokio::test(flavor = "multi_thread")]
async fn debug_mode_keys_come_before_the_hex_view_s() {
    let Some(_) = enter("debug_mode_keys_come_before_the_hex_view_s") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    open(&mut session, Kind::Hex).await;
    session.keys("<space>G").await;
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    assert_eq!(
        focused_kind(&session),
        Some(ScreenKind::Hex),
        "the view keeps the focus"
    );
    let offset = |session: &Session| {
        let screen = session.app.editor.focused_screen().unwrap();
        let hex = screen.state::<vortex_view::hex::HexView>().unwrap();
        hex.buffer.cursor(session.app.editor.tree.focus).offset
    };
    assert_eq!(offset(&session), 0);
    let speed = session.app.editor.debug.spectator.speed_label();

    // `j` is "slower" in debug mode, and a row down in the view
    session.keys("j").await;
    assert_ne!(
        session.app.editor.debug.spectator.speed_label(),
        speed,
        "debug mode took `j`"
    );
    assert_eq!(offset(&session), 0, "the view didn't");
    // `l` is the view's... and debug mode's (a line forward): debug mode's again
    session.keys("l").await;
    assert_eq!(offset(&session), 0);
    // `Esc` is the screen's: it closes the view, debug mode stays
    session.keys("<esc>").await;
    assert_eq!(focused_kind(&session), None);
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
}

/// In a modal state the hex view's keys go first: while overwriting, `f` is a digit, not debug
/// mode's fast forward.
#[tokio::test(flavor = "multi_thread")]
async fn the_hex_view_s_modal_keys_come_before_debug_mode() {
    let Some(_) = enter("the_hex_view_s_modal_keys_come_before_debug_mode") else {
        return;
    };
    let mut session = session_on("a.txt", 100).await;
    open(&mut session, Kind::Hex).await;
    let byte = |session: &Session| {
        let screen = session.app.editor.focused_screen().unwrap();
        let hex = screen.state::<vortex_view::hex::HexView>().unwrap();
        hex.buffer.bytes(0..1)[0]
    };
    assert_eq!(byte(&session), b'l');
    // overwriting, then debug mode (a key would be typed into the view)
    session.keys("i").await;
    vortex_term::debug::mode::enter(&mut session.app.editor);
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    session.keys("f").await;
    assert_eq!(
        byte(&session) >> 4,
        0xf,
        "the view took `f`: {:#x}",
        byte(&session)
    );
    assert_ne!(
        session.app.editor.debug.spectator.mode,
        vortex_view::debug::spectator::Mode::FastForward
    );
    // `Esc` is never the mode's first: the view's own layer takes it (overwriting stops); the
    // next one would close the view, which refuses with bytes not written, and the mode stays
    session.keys("<esc>").await;
    assert_eq!(focused_kind(&session), Some(ScreenKind::Hex));
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    session.keys("<esc>").await;
    assert_eq!(
        focused_kind(&session),
        Some(ScreenKind::Hex),
        "unwritten bytes"
    );
    assert!(
        status(&session.app).contains("unwritten bytes"),
        "{:?}",
        status(&session.app)
    );
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
}

/// Debug mode with a page focused: the mode's keys come first (`k` is "faster"), `Esc` closes
/// the page and the mode stays.
#[tokio::test(flavor = "multi_thread")]
async fn debug_mode_keys_come_before_the_page_s() {
    let Some(_) = enter("debug_mode_keys_come_before_the_page_s") else {
        return;
    };
    let mut session = session_on("notes.md", 100).await;
    open(&mut session, Kind::Markdown).await;
    session.keys("<space>G").await;
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    assert_eq!(focused_kind(&session), Some(ScreenKind::Markdown));
    let speed = session.app.editor.debug.spectator.speed_label();
    session.keys("k").await;
    assert_ne!(
        session.app.editor.debug.spectator.speed_label(),
        speed,
        "debug mode took `k`"
    );
    assert_eq!(focused_kind(&session), Some(ScreenKind::Markdown));
    session.keys("<esc>").await;
    assert_eq!(focused_kind(&session), None);
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    session.keys("<esc>").await;
    assert_eq!(modes::current(&session.app.editor), None);
}
