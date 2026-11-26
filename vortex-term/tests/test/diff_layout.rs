//! The responsive diff: side by side in a wide window, stacked below `[editor.diff]
//! stack-below` columns (default 120, 0 never), `t` switching the layout, the live re-layout when
//! the terminal is resized, and what the diff does in both layouts (hunk navigation, the yanked
//! `file:lines`, staging, the footer).
//!
//! Each test runs in a throwaway repository, like those of `hunks.rs`: a file of 20 lines with
//! three changes in the work tree.

use std::path::{Path, PathBuf};

use vortex_term::config::Config;

use super::git_log::{git, on_screen, Repo, Session};
use super::helpers::{test_config, AppBuilder};
use super::screen_windows::resize;
use super::uncommitted::{cwd, lines};

fn enter(test: &str) -> Option<PathBuf> {
    Repo::enter_with(module_path!(), test, make)
}

/// `a.txt` of 20 lines with the lines 3, 10 and 17 changed in the work tree: three hunks.
fn make(parent: &Path) -> PathBuf {
    let work = parent.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, "", &["init", "-q"]);
    std::fs::write(work.join("a.txt"), lines("line ", 20)).unwrap();
    git(&work, "", &["add", "-A"]);
    git(&work, "", &["commit", "-q", "-m", "Start"]);
    let edited = lines("line ", 20)
        .replace("line 3\n", "three\n")
        .replace("line 10\n", "ten\n")
        .replace("line 17\n", "seventeen\n");
    std::fs::write(work.join("a.txt"), edited).unwrap();
    work
}

fn config(stack_below: Option<u16>) -> Config {
    let mut config = test_config();
    if let Some(stack_below) = stack_below {
        config.editor.diff.stack_below = stack_below;
    }
    config
}

/// The diff of `a.txt` opened with `Space =` in a terminal `cols` wide.
async fn diff_at(cols: u16, stack_below: Option<u16>) -> Session {
    let app = AppBuilder::new()
        .with_config(config(stack_below))
        .with_file(cwd().join("a.txt"), None)
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    resize(&mut session, cols, 30).await;
    session.keys("<space>=").await;
    session
        .wait_for_screen("the diff", |rows| on_screen(rows, "hunk "))
        .await;
    session
}

fn is_stacked(session: &Session) -> bool {
    session.footer().ends_with("· stacked")
}

/// The rows of the diff with the old and the new line of the first hunk: side by side they share
/// a row, stacked the old one is above the new one.
fn first_hunk_rows(session: &Session) -> (usize, usize) {
    let rows = session.rows();
    let old = rows
        .iter()
        .position(|row| row.contains("line 3"))
        .unwrap_or_else(|| panic!("no old line: {rows:#?}"));
    let new = rows
        .iter()
        .position(|row| row.contains("three"))
        .unwrap_or_else(|| panic!("no new line: {rows:#?}"));
    (old, new)
}

fn side_by_side(session: &Session) -> bool {
    let (old, new) = first_hunk_rows(session);
    // the old and new lines of a hunk face each other
    old == new && !is_stacked(session)
}

fn stacked(session: &Session) -> bool {
    let (old, new) = first_hunk_rows(session);
    new == old + 1 && is_stacked(session)
}

fn cached() -> String {
    git(&cwd(), "", &["diff", "--cached"])
}

fn clipboard(session: &Session) -> Option<String> {
    let values: Vec<String> = session
        .app
        .editor
        .registers
        .read('+', &session.app.editor)?
        .map(|value| value.into_owned())
        .collect();
    assert!(values.len() <= 1, "{values:?}");
    values.into_iter().next()
}

// ---------------------------------------------------------------------------------------------
// Which layout opens

/// The default `stack-below` is 120: a 100 column window stacks the diff, a 160 column one puts
/// the sides next to each other.
#[tokio::test(flavor = "multi_thread")]
async fn it_opens_stacked_below_120_columns_and_side_by_side_above() {
    let Some(_) = enter("it_opens_stacked_below_120_columns_and_side_by_side_above") else {
        return;
    };
    let session = diff_at(100, None).await;
    assert!(stacked(&session), "{:#?}", session.rows());
    drop(session);
    let session = diff_at(160, None).await;
    assert!(side_by_side(&session), "{:#?}", session.rows());
    drop(session);
    // the threshold itself: 120 columns is wide enough, 119 is not
    let session = diff_at(120, None).await;
    assert!(side_by_side(&session), "{:#?}", session.rows());
    drop(session);
    let session = diff_at(119, None).await;
    assert!(stacked(&session), "{:#?}", session.rows());
}

/// `stack-below = 0` never stacks, however narrow the window.
#[tokio::test(flavor = "multi_thread")]
async fn stack_below_0_never_stacks() {
    let Some(_) = enter("stack_below_0_never_stacks") else {
        return;
    };
    for cols in [60, 100, 160] {
        let session = diff_at(cols, Some(0)).await;
        assert!(
            side_by_side(&session),
            "{cols} columns: {:#?}",
            session.rows()
        );
    }
}

/// Another `stack-below`: 80 columns stack at 100 and not at 79.
#[tokio::test(flavor = "multi_thread")]
async fn a_stack_below_of_its_own() {
    let Some(_) = enter("a_stack_below_of_its_own") else {
        return;
    };
    let session = diff_at(100, Some(80)).await;
    assert!(side_by_side(&session), "{:#?}", session.rows());
    drop(session);
    let session = diff_at(79, Some(80)).await;
    assert!(stacked(&session), "{:#?}", session.rows());
    drop(session);
    // a split window narrower than the terminal counts by its own width
    let app = AppBuilder::new()
        .with_config(config(None))
        .with_file(cwd().join("a.txt"), None)
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    resize(&mut session, 200, 30).await;
    session.keys("<C-w>v<space>=").await;
    session
        .wait_for_screen("the diff", |rows| on_screen(rows, "hunk "))
        .await;
    // two windows of about 100 columns: the diff is stacked, as 100 is below 120
    assert!(
        on_screen(&session.rows(), "· stacked"),
        "{:#?}",
        session.rows()
    );
    resize(&mut session, 300, 30).await;
    session
        .wait_for_screen("side by side", |rows| !on_screen(rows, "· stacked"))
        .await;
}

// ---------------------------------------------------------------------------------------------
// `t`

/// `t` switches the layout and the new one stays until `t` again, whatever the width.
#[tokio::test(flavor = "multi_thread")]
async fn t_switches_the_layout_and_it_stays() {
    let Some(_) = enter("t_switches_the_layout_and_it_stays") else {
        return;
    };
    let mut session = diff_at(100, None).await;
    assert!(stacked(&session));
    session.keys("t").await;
    assert!(side_by_side(&session), "{:#?}", session.rows());
    // it stays: scrolling, hunks and a new size keep it
    session.keys("jn").await;
    assert!(!is_stacked(&session));
    resize(&mut session, 90, 30).await;
    assert!(!is_stacked(&session), "{:#?}", session.rows());
    resize(&mut session, 160, 30).await;
    assert!(!is_stacked(&session));
    session.keys("t").await;
    assert!(is_stacked(&session), "{:#?}", session.rows());
    // and the other way, still stacked in a wide window
    resize(&mut session, 100, 30).await;
    assert!(is_stacked(&session));
    resize(&mut session, 170, 30).await;
    assert!(is_stacked(&session), "{:#?}", session.rows());
    session.keys("t").await;
    assert!(!is_stacked(&session));
    // from a wide window to stacked and back
    let mut session = diff_at(160, None).await;
    assert!(side_by_side(&session));
    session.keys("t").await;
    assert!(stacked(&session), "{:#?}", session.rows());
    session.keys("t").await;
    assert!(side_by_side(&session), "{:#?}", session.rows());
}

/// The `?` help of the diff lists `t`.
#[tokio::test(flavor = "multi_thread")]
async fn the_help_lists_t() {
    let Some(_) = enter("the_help_lists_t") else {
        return;
    };
    let mut session = diff_at(100, None).await;
    session.keys("?").await;
    let rows = session.rows();
    let row = rows
        .iter()
        .find(|row| row.contains("side by side or stacked"))
        .unwrap_or_else(|| panic!("no t in the help: {rows:#?}"));
    assert!(row.contains("│ t "), "{row:?}");
}

// ---------------------------------------------------------------------------------------------
// Live

/// The terminal resized across the threshold changes the layout live, without `t`.
#[tokio::test(flavor = "multi_thread")]
async fn resizing_across_the_threshold_switches_the_layout() {
    let Some(_) = enter("resizing_across_the_threshold_switches_the_layout") else {
        return;
    };
    let mut session = diff_at(100, None).await;
    assert!(stacked(&session));
    for (cols, wide) in [
        (160, true),
        (100, false),
        (121, true),
        (118, false),
        (140, true),
    ] {
        resize(&mut session, cols, 30).await;
        session
            .wait_for_screen("the diff", |rows| on_screen(rows, "hunk "))
            .await;
        if wide {
            assert!(side_by_side(&session), "{cols}: {:#?}", session.rows());
        } else {
            assert!(stacked(&session), "{cols}: {:#?}", session.rows());
        }
        // always the same hunk, and the diff drawn in all of the terminal
        assert!(
            session.footer().starts_with("hunk 1/3"),
            "{}",
            session.footer()
        );
        assert!(
            session.rows()[0].trim_end().chars().count() <= usize::from(cols),
            "{cols}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// What it does in both layouts

/// `n`, `N`, `]c` and `[c` go through the hunks, in either layout, and the footer says which.
async fn hunk_navigation(cols: u16) {
    let mut session = diff_at(cols, None).await;
    let layout = is_stacked(&session);
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 1/3"));
    session.keys("n").await;
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 2/3"));
    session.keys("n").await;
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 3/3"));
    session.keys("N").await;
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 2/3"));
    session.keys("]c").await;
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 3/3"));
    session.keys("[c[c").await;
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 1/3"));
    // the hunk is on the screen
    session.keys("n").await;
    assert!(on_screen(&session.rows(), "ten"), "{:#?}", session.rows());
    assert_eq!(is_stacked(&session), layout, "the layout stayed");
}

/// `y` yanks the `file:lines` of the hunk shown: the new side's lines.
async fn yank(cols: u16) {
    let mut session = diff_at(cols, None).await;
    session.keys("y").await;
    assert_eq!(clipboard(&session).as_deref(), Some("a.txt:3"));
    session.keys("n").await;
    session.keys("y").await;
    assert_eq!(clipboard(&session).as_deref(), Some("a.txt:10"));
    session.keys("n").await;
    session.keys("y").await;
    assert_eq!(clipboard(&session).as_deref(), Some("a.txt:17"));
    assert!(session.footer().starts_with("hunk 3/3"));
}

/// `s` stages the hunk shown, `u` unstages it.
async fn staging(cols: u16) {
    let mut session = diff_at(cols, None).await;
    session.keys("n").await;
    session.keys("s").await;
    session.wait_for_footer("hunk 2/3 staged").await;
    let staged = cached();
    assert!(
        staged.contains("+ten") && staged.contains("-line 10"),
        "{staged}"
    );
    assert!(
        !staged.contains("three") && !staged.contains("seventeen"),
        "{staged}"
    );
    assert!(on_screen(&session.rows(), "┃"), "{:#?}", session.rows());
    // the footer of the other hunks is unchanged
    session.keys("n").await;
    assert_eq!(session.footer().split(" · ").next(), Some("hunk 3/3"));
    session.keys("N").await;
    session.keys("u").await;
    session.wait_for_footer("hunk 2/3").await;
    assert_eq!(cached(), "");
}

/// The footer: `hunk i/n`, with ` · stacked` when it is.
async fn footer(cols: u16) {
    let mut session = diff_at(cols, None).await;
    let stacked = is_stacked(&session);
    assert_eq!(
        session.footer(),
        if stacked {
            "hunk 1/3 · stacked"
        } else {
            "hunk 1/3"
        }
    );
    session.keys("G").await;
    // scrolling to the end doesn't change the hunk
    assert!(
        session.footer().starts_with("hunk "),
        "{}",
        session.footer()
    );
}

macro_rules! both_layouts {
    ($($name:ident: $body:ident;)*) => {$(
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn stacked() {
                let Some(_) = enter(concat!(stringify!($name), "::stacked")) else {
                    return;
                };
                $body(100).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn side_by_side() {
                let Some(_) = enter(concat!(stringify!($name), "::side_by_side")) else {
                    return;
                };
                $body(160).await;
            }
        }
    )*};
}

both_layouts! {
    hunk_navigation_in_both_layouts: hunk_navigation;
    yank_in_both_layouts: yank;
    staging_in_both_layouts: staging;
    the_footer_in_both_layouts: footer;
}
