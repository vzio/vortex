//! The fork's two modes, watch mode and debug mode, and their screen: a mode and its screen are
//! atomic. While a mode is on the views have the background of `ui.background.mode` (else
//! `ui.popup`'s), and the cursor is drawn as in select mode while looking around (watch, `v`) or in
//! view mode (debug, `v`); however a mode is left, plain editing is back: normal mode, the
//! buffer's keys and cursor, the debug panel not focused, on the file and line the mode was
//! showing, and the background is the buffer's.
//!
//! The tests of watch mode run in a throwaway repository, like those of `watch.rs`. The theme
//! of the tests has both scopes with colors that are easy to tell apart, and the cursor of select
//! mode is an underline, so that the drawn cursor tells looking around from editing.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use vortex_term::{debug::launch, modes};
use vortex_view::{
    current_ref,
    document::Mode,
    editor::CursorShapeConfig,
    graphics::{Color, CursorKind},
};

use super::git_log::{git, Repo, Session};
use super::helpers::test_config;
use super::uncommitted::{cwd, lines};

const MODE_BG: Color = Color::Rgb(0x11, 0x22, 0x33);
const POPUP_BG: Color = Color::Rgb(0x44, 0x55, 0x66);

fn enter(test: &str) -> Option<PathBuf> {
    Repo::enter_with(module_path!(), test, make)
}

/// A repository with committed files `a.txt`, `b.txt`, `c.txt` and `other.txt` of ten lines.
fn make(parent: &Path) -> PathBuf {
    let work = parent.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, "", &["init", "-q"]);
    for (name, prefix) in [
        ("a.txt", "a"),
        ("b.txt", "b"),
        ("c.txt", "c"),
        ("other.txt", "o"),
    ] {
        std::fs::write(work.join(name), lines(prefix, 10)).unwrap();
    }
    git(&work, "", &["add", "-A"]);
    git(&work, "", &["commit", "-q", "-m", "Start"]);
    work
}

/// A theme of the tests: the scopes in `toml`, and the selection the editor needs.
fn theme(toml: &str) -> vortex_view::Theme {
    let toml = format!("\"ui.selection\" = {{ bg = \"#333333\" }}\n{toml}");
    vortex_view::Theme::from(toml::from_str::<toml::Value>(&toml).unwrap())
}

fn mode_theme() -> vortex_view::Theme {
    theme(
        r##"
"ui.background.mode" = { bg = "#112233" }
"ui.popup" = { bg = "#445566" }
"##,
    )
}

/// A session whose select mode has an underline for a cursor (the others have the block).
fn session() -> Session {
    let mut config = test_config();
    config.editor.cursor_shape = toml::from_str::<CursorShapeConfig>(
        r#"
normal = "block"
insert = "bar"
select = "underline"
"#,
    )
    .unwrap();
    let mut session = Session::with_config(config);
    session.app.editor.theme = mode_theme();
    session
}

/// The background of a cell in the middle of the views, away from any text and from the bars.
fn background(session: &Session) -> Color {
    session.app.screen().get(60, 2).expect("a cell").bg
}

fn on_mode_screen(session: &Session) -> bool {
    background(session) == MODE_BG
}

fn text(session: &Session) -> String {
    let (_, doc) = current_ref!(session.app.editor);
    doc.text().to_string()
}

/// The focused file's name and the line (from 0) of its cursor.
fn focus(session: &Session) -> (String, usize) {
    let (view, doc) = current_ref!(session.app.editor);
    let name = doc
        .path()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .unwrap_or_else(|| "[scratch]".to_string());
    let text = doc.text().slice(..);
    (name, doc.selection(view.id).primary().cursor_line(text))
}

fn at(name: &str, line: usize) -> (String, usize) {
    (name.to_string(), line)
}

/// Plain editing: no mode, normal mode, the cursor of the buffer, the debug panel not focused, no
/// mode screen, and typing changes the text.
async fn assert_editing(session: &mut Session, how: &str) {
    assert_eq!(modes::current(&session.app.editor), None, "{how}");
    assert!(!session.app.editor.watch.is_active(), "{how}");
    assert!(!session.app.editor.debug.mode_on(), "{how}");
    assert_eq!(session.app.editor.mode(), Mode::Normal, "{how}");
    assert!(!session.app.editor.debug_panel_focused(), "{how}");
    assert_eq!(session.app.cursor_kind(), CursorKind::Hidden, "{how}");
    assert!(!on_mode_screen(session), "{how}: the mode's screen stayed");

    let before = text(session);
    session.keys("iZ<esc>").await;
    assert_ne!(text(session), before, "{how}: typing changed nothing");
    assert!(text(session).contains('Z'), "{how}");
    assert_eq!(session.app.editor.mode(), Mode::Normal, "{how}");
    assert!(!on_mode_screen(session), "{how}");
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

/// `:watch`, then a change of `c.txt` line 7 that watch mode follows, and `v`: looking around.
/// Ends in watch mode on `c.txt`, line 7 (from 0: 6), looking around.
async fn watching_c7(session: &mut Session, look: bool) {
    assert!(!on_mode_screen(session));
    session.keys(":watch<ret>").await;
    assert!(session.app.editor.watch.is_active());
    session.run_for(Duration::from_millis(300)).await;
    edit("c.txt", 7, "c7 agent");
    let deadline = Instant::now() + Duration::from_secs(20);
    while focus(session) != at("c.txt", 6) {
        assert!(
            Instant::now() < deadline,
            "following c.txt: {:?}",
            focus(session)
        );
        session.run_for(Duration::from_millis(50)).await;
    }
    if look {
        session.keys("v").await;
        assert!(looking(session));
    }
    assert!(on_mode_screen(session), "{:#?}", session.rows());
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Watch)
    );
}

fn looking(session: &Session) -> bool {
    // looking around draws the cursor of select mode: the underline of the test config
    session.app.cursor_kind() == CursorKind::Underline
}

// ---------------------------------------------------------------------------------------------
// Watch mode

/// Every way out of watch mode (while looking around), and what is back after it.
async fn leaving_watch(test: &str, keys: &str, moves: bool) {
    let Some(_) = enter(test) else {
        return;
    };
    let mut session = session();
    watching_c7(&mut session, true).await;
    assert_eq!(session.app.cursor_kind(), CursorKind::Underline);

    session.keys(keys).await;
    if !moves {
        assert_eq!(focus(&session), at("c.txt", 6), "{keys}");
    }
    assert_editing(&mut session, keys).await;
    if !moves {
        assert_eq!(focus(&session), at("c.txt", 6), "{keys}");
    }
}

macro_rules! leaves_watch {
    ($name:ident, $keys:expr, $moves:expr) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            leaving_watch(stringify!($name), $keys, $moves).await;
        }
    };
}

leaves_watch!(esc_leaves_watch_mode, "<esc>", false);
leaves_watch!(the_command_again_leaves_watch_mode, ":watch<ret>", false);
leaves_watch!(watch_stop_leaves_watch_mode, ":watch-stop<ret>", false);
leaves_watch!(watch_clear_leaves_watch_mode, ":watch-clear<ret>", false);
leaves_watch!(
    a_typed_command_leaves_watch_mode,
    ":open other.txt<ret>",
    true
);

#[tokio::test(flavor = "multi_thread")]
async fn debug_mode_while_watching_leaves_watch_mode_and_its_esc_gives_editing_back() {
    let Some(_) =
        enter("debug_mode_while_watching_leaves_watch_mode_and_its_esc_gives_editing_back")
    else {
        return;
    };
    let mut session = session();
    watching_c7(&mut session, true).await;
    session.keys(":debug-mode<ret>").await;
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    assert!(!session.app.editor.watch.is_active());
    assert!(on_mode_screen(&session));
    // watch mode's looking around is gone with it
    assert_eq!(focus(&session), at("c.txt", 6));
    session.keys("<esc>").await;
    assert_eq!(focus(&session), at("c.txt", 6));
    assert_editing(&mut session, ":debug-mode from watch mode").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_mode_following_leaves_with_esc_on_the_line_it_showed() {
    let Some(_) = enter("watch_mode_following_leaves_with_esc_on_the_line_it_showed") else {
        return;
    };
    let mut session = session();
    watching_c7(&mut session, false).await;
    // following: the cursor is the normal one, the screen is the mode's
    assert!(!looking(&session));
    session.keys("<esc>").await;
    assert_eq!(focus(&session), at("c.txt", 6));
    assert_editing(&mut session, "<esc>").await;
    assert_eq!(focus(&session), at("c.txt", 6));
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_mode_can_be_entered_again_after_leaving_and_has_its_screen_again() {
    let Some(_) = enter("watch_mode_can_be_entered_again_after_leaving_and_has_its_screen_again")
    else {
        return;
    };
    let mut session = session();
    watching_c7(&mut session, true).await;
    session.keys("<esc>").await;
    assert!(!on_mode_screen(&session));
    session.keys(":watch<ret>").await;
    assert!(session.app.editor.watch.is_active());
    assert!(on_mode_screen(&session));
    session.keys(":watch<ret>").await;
    assert!(!on_mode_screen(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_watch_mode_screen_falls_back_to_the_popup_background() {
    let Some(_) = enter("the_watch_mode_screen_falls_back_to_the_popup_background") else {
        return;
    };
    let mut session = session();
    session.app.editor.theme = theme(r##""ui.popup" = { bg = "#445566" }"##);
    assert_ne!(background(&session), POPUP_BG);
    session.keys(":watch<ret>").await;
    assert!(session.app.editor.watch.is_active());
    assert_eq!(background(&session), POPUP_BG);
    session.keys("<esc>").await;
    assert_ne!(background(&session), POPUP_BG);
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_watch_mode_while_debugging_leaves_debug_mode() {
    let Some(_) = enter("starting_watch_mode_while_debugging_leaves_debug_mode") else {
        return;
    };
    let mut session = session();
    session.keys(":debug-mode<ret>").await;
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    session.keys("<tab>").await;
    assert!(session.app.editor.debug_panel_focused());
    session.keys(":watch<ret>").await;
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Watch)
    );
    assert!(!session.app.editor.debug.mode_on());
    assert!(!session.app.editor.debug_panel_focused());
    assert!(on_mode_screen(&session));
    session.keys("<esc>").await;
    assert_editing(&mut session, ":watch from debug mode").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_mode_started_by_the_command_line_flag_has_its_screen() {
    let Some(_) = enter("watch_mode_started_by_the_command_line_flag_has_its_screen") else {
        return;
    };
    let app = super::helpers::AppBuilder::new()
        .with_config(test_config())
        .with_watch()
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    session.app.editor.theme = mode_theme();
    session.first_frame().await;
    assert!(session.app.editor.watch.is_active());
    assert!(on_mode_screen(&session));
    session.keys("<esc>").await;
    assert_editing(&mut session, "vx --watch").await;
}

// ---------------------------------------------------------------------------------------------
// Debug mode

async fn debug_on(session: &mut Session, keys: &str) {
    assert!(!on_mode_screen(session));
    session.keys(keys).await;
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug),
        "{keys}"
    );
    assert!(on_mode_screen(session), "{keys}: {:#?}", session.rows());
    // without a session debug mode starts in view mode: the cursor is drawn as in select mode
    assert!(looking(session), "{keys}: view mode");
}

macro_rules! debug_way_in {
    ($name:ident, $keys:expr) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let mut session = session();
            debug_on(&mut session, $keys).await;
            session.keys("<esc>").await;
            assert_editing(&mut session, $keys).await;
        }
    };
}

debug_way_in!(the_command_enters_debug_mode, ":debug-mode<ret>");
debug_way_in!(space_g_enters_debug_mode, "<space>G");

#[tokio::test(flavor = "multi_thread")]
async fn vx_debug_enters_debug_mode() {
    let mut session = session();
    session.first_frame().await;
    // (`vx --debug` without a program opens the picker of the templates)
    let picker = launch::start_cli(
        &mut session.app.editor,
        launch::Target {
            program: None,
            args: Vec::new(),
        },
    );
    drop(picker);
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    session.keys("<esc>").await;
    assert_editing(&mut session, "vx --debug").await;
}

/// Every way out of debug mode, with the debug panel focused and in view mode.
async fn leaving_debug(how: &str, keys: &str) {
    let mut session = session();
    debug_on(&mut session, ":debug-mode<ret>").await;
    session.keys("<tab>").await;
    assert!(
        session.app.editor.debug_panel_focused(),
        "{how}: Tab focuses the debug panel"
    );
    session.keys(keys).await;
    assert_editing(&mut session, how).await;
    assert!(!session.app.editor.debug_panel_focused());
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_in_the_focused_panel_unfocuses_it_and_then_esc_leaves_debug_mode() {
    let mut session = session();
    debug_on(&mut session, ":debug-mode<ret>").await;
    session.keys("<tab>").await;
    assert!(session.app.editor.debug_panel_focused());
    session.keys("<esc>").await;
    // the panel's Esc: back to debug mode's keys, the mode is still on
    assert!(!session.app.editor.debug_panel_focused());
    assert_eq!(
        modes::current(&session.app.editor),
        Some(modes::Mode::Debug)
    );
    assert!(on_mode_screen(&session));
    session.keys("<esc>").await;
    assert_editing(&mut session, "esc, esc").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_again_leaves_debug_mode_and_unfocuses_the_panel() {
    leaving_debug("command", ":debug-mode<ret>").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_mode_leaves_on_the_file_and_line_it_showed() {
    let mut session = session();
    session.keys("ione<ret>two<ret>three<esc>").await;
    session.keys("gg").await;
    session.keys(":debug-mode<ret>").await;
    assert!(modes::current(&session.app.editor).is_some());
    // view mode: the cursor is the user's, and moves freely
    session.keys("<down>").await;
    let place = focus(&session);
    assert_eq!(place, at("[scratch]", 1));
    session.keys("<esc>").await;
    assert_eq!(focus(&session), place);
    assert_editing(&mut session, "esc in view mode").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_mode_leaves_when_leaving_via_modes_leave_too() {
    let mut session = session();
    debug_on(&mut session, ":debug-mode<ret>").await;
    session.keys("<tab>").await;
    modes::leave(&mut session.app.editor);
    // (a frame without the mode's screen)
    session.keys(":<esc>").await;
    assert_editing(&mut session, "modes::leave").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_debug_mode_screen_falls_back_to_the_popup_background() {
    let mut session = session();
    session.app.editor.theme = theme(r##""ui.popup" = { bg = "#445566" }"##);
    session.keys(":debug-mode<ret>").await;
    assert_eq!(background(&session), POPUP_BG);
    session.keys("<esc>").await;
    assert_ne!(background(&session), POPUP_BG);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_theme_without_either_scope_leaves_the_screen_alone() {
    let mut session = session();
    session.app.editor.theme = theme("");
    let plain = background(&session);
    session.keys(":debug-mode<ret>").await;
    assert_eq!(background(&session), plain);
}

// ---------------------------------------------------------------------------------------------
// A screen focused in a mode

/// Opens `notes.md` in `dir`, rendered: a page focused over its window.
async fn a_page(session: &mut Session, dir: &Path) {
    // the page highlights with the theme's scopes: the test theme's, set as a theme is
    session.app.editor.set_theme(mode_theme()).unwrap();
    let path = dir.join("notes.md");
    std::fs::write(&path, "# Notes\n\nSome text.\n").unwrap();
    session
        .keys(&format!(":open {}<ret>", path.display()))
        .await;
    session.keys(":markdown-view<ret>").await;
    assert!(session
        .app
        .editor
        .is_screen_view(session.app.editor.tree.focus));
}

/// With a screen focused, a mode and its screen stay atomic: `Esc` closes the screen first and
/// the window shows the buffer again, the mode on, with its screen; the next `Esc` leaves the
/// mode and its screen together (the bug: `Esc` left the mode and kept its screen).
async fn esc_closes_the_screen_then_leaves(session: &mut Session, mode: modes::Mode, how: &str) {
    assert_eq!(modes::current(&session.app.editor), Some(mode), "{how}");
    assert!(session
        .app
        .editor
        .is_screen_view(session.app.editor.tree.focus));
    session.keys("<esc>").await;
    assert!(
        !session
            .app
            .editor
            .is_screen_view(session.app.editor.tree.focus),
        "{how}: Esc closes the page"
    );
    assert_eq!(modes::current(&session.app.editor), Some(mode), "{how}");
    assert!(on_mode_screen(session), "{how}: {:#?}", session.rows());
    session.keys("<esc>").await;
    assert_editing(session, how).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_on_a_screen_in_debug_mode_closes_the_screen_and_then_leaves_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = session();
    a_page(&mut session, dir.path()).await;
    session.keys(":debug-mode<ret>").await;
    esc_closes_the_screen_then_leaves(&mut session, modes::Mode::Debug, "debug mode").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_on_a_screen_in_watch_mode_closes_the_screen_and_then_leaves_the_mode() {
    let Some(_) = enter("esc_on_a_screen_in_watch_mode_closes_the_screen_and_then_leaves_the_mode")
    else {
        return;
    };
    let mut session = session();
    a_page(&mut session, &cwd()).await;
    session.keys(":watch<ret>").await;
    esc_closes_the_screen_then_leaves(&mut session, modes::Mode::Watch, "watch mode").await;
}
