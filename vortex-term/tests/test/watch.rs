//! Tests of watch mode: `:watch` follows the files another process changes, `n`/`N` go through
//! the session's changes, `:watch-changes` lists them.
//!
//! Like the tests of the commit log, each one runs again in a process of its own with the
//! working directory in a throwaway repository (see [`Repo::enter_with`]). The files are
//! changed with `std::fs` and `sh` from the test, like an agent or a formatter would: the editor
//! only sees them through the watcher (200 ms debounce), so the tests wait for what they expect
//! with a polling loop and a generous timeout.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use termina::event::{Event, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use vortex_term::{
    application::Application,
    watch::{LEAVE_TO_EDIT, LOOK_TO_MOVE},
};
use vortex_view::{
    current_ref,
    document::Mode,
    editor::{CatchUpPace, Severity},
    graphics::Color,
    input::parse_macro,
    watch::{FileKind, Quiet, SessionFile},
};

use super::git_log::{git, status, Repo, Session};
use super::helpers::test_config;
use super::uncommitted::{commit_all, cwd, lines, write};

/// Time for the watcher to see a write and the debounce to pass, at the most.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Between two writes that must have different modification times.
const MTIME_GAP: Duration = Duration::from_millis(25);

fn enter(test: &str) -> Option<PathBuf> {
    Repo::enter_with(module_path!(), test, make)
}

/// The repository of the tests: committed text files, a lockfile, ignored paths, a remote that
/// can't be reached, and at the start of a session a file that is already dirty and one that
/// is untracked.
fn make(parent: &Path) -> PathBuf {
    let work = parent.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, "", &["init", "-q"]);
    let put = |name: &str, text: &str| std::fs::write(work.join(name), text).unwrap();
    put(".gitignore", "ignored/\n*.log\n");
    for (name, prefix) in [
        ("a.txt", "a"),
        ("b.txt", "b"),
        ("c.txt", "c"),
        ("other.txt", "o"),
        ("dirty.txt", "d"),
        ("del.txt", "x"),
    ] {
        put(name, &lines(prefix, 10));
    }
    put("Cargo.lock", &lines("lock", 30));
    git(&work, "", &["add", "-A"]);
    git(&work, "", &["commit", "-q", "-m", "Start"]);
    // a remote nothing listens on, and the branch tracking it
    git(
        &work,
        "",
        &["remote", "add", "origin", "http://127.0.0.1:9/x.git"],
    );
    git(&work, "", &["config", "branch.main.remote", "origin"]);
    git(
        &work,
        "",
        &["config", "branch.main.merge", "refs/heads/main"],
    );
    // dirty when the watch starts: its line 2 is changed (not a change of the session)
    put(
        "dirty.txt",
        &lines("d", 10).replace("d2\n", "d2 before the watch\n"),
    );
    put("untracked.txt", &lines("u", 5));
    work
}

/// Replaces line `number` (from 1) of the file with `text`, as another program would.
fn edit(name: &str, number: usize, text: &str) {
    let path = cwd().join(name);
    let old = std::fs::read_to_string(&path).unwrap();
    let new: String = old
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            if i + 1 == number {
                format!("{text}\n")
            } else {
                line.to_string()
            }
        })
        .collect();
    std::fs::write(path, new).unwrap();
}

fn session_of(app: &Application) -> &vortex_view::watch::Session {
    app.editor.watch.session.as_ref().expect("a watch session")
}

/// The file of the session `name`.
fn file<'a>(app: &'a Application, name: &str) -> Option<&'a SessionFile> {
    app.editor
        .watch
        .session
        .as_ref()
        .and_then(|session| session.file(&cwd().join(name)))
}

/// The lines (from 0, start and end) of the hunks of the session's file `name`, in the order of the file.
fn hunks(app: &Application, name: &str) -> Vec<(u32, u32)> {
    file(app, name)
        .unwrap_or_else(|| panic!("{name} is not in the session"))
        .hunks
        .iter()
        .map(|hunk| (hunk.after.start, hunk.after.end))
        .collect()
}

/// The focused file's name and the line (from 0) of its cursor.
fn focus(app: &Application) -> (String, usize) {
    let (view, doc) = current_ref!(app.editor);
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

fn is_error(app: &Application) -> Option<String> {
    app.editor
        .get_status()
        .filter(|(_, severity)| **severity == Severity::Error)
        .map(|(message, _)| message.to_string())
}

impl Session {
    /// Runs the editor until `condition` holds.
    async fn wait_watch(&mut self, what: &str, condition: impl Fn(&Application) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !condition(&self.app) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\nfocus: {:?}\nstatus: {:?}\nscreen:\n{}",
                focus(&self.app),
                status(&self.app),
                self.rows()
                    .iter()
                    .map(|row| row.trim_end())
                    .filter(|row| !row.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            self.run_for(Duration::from_millis(50)).await;
        }
    }

    /// `:watch`, and a moment for the watcher to be armed.
    async fn start_watch(&mut self) {
        self.keys(":watch<ret>").await;
        assert!(self.app.editor.watch.is_active(), "{}", status(&self.app));
        self.run_for(Duration::from_millis(300)).await;
    }

    /// Waits for the focused file and cursor line (from 0).
    async fn wait_for_focus(&mut self, name: &str, line: usize) {
        let expected = at(name, line);
        self.wait_watch(&format!("the focus on {expected:?}"), |app| {
            focus(app) == expected
        })
        .await;
    }

    /// Changes line `number` (from 1) of a file and waits for following to take the cursor
    /// there.
    async fn change_and_follow(&mut self, name: &str, number: usize, text: &str) {
        edit(name, number, text);
        self.wait_for_focus(name, number - 1).await;
    }

    async fn wait_for_screen_text(&mut self, text: &str) {
        let wanted = text.to_string();
        self.wait_watch(&format!("{text:?} on the screen"), move |app| {
            super::git_log::on_screen(&super::git_log::screen_rows(app), &wanted)
        })
        .await;
    }

    fn on_screen(&self, text: &str) -> bool {
        super::git_log::on_screen(&self.rows(), text)
    }

    fn following(&self) -> bool {
        session_of(&self.app).following
    }

    fn watching(&self) -> bool {
        self.app.editor.watch.is_active()
    }

    fn new_count(&self) -> usize {
        session_of(&self.app).new_count()
    }

    /// The note on the right of the mode bar: the session's latest activity.
    fn activity(&self) -> String {
        session_of(&self.app).activity.clone().unwrap_or_default()
    }

    /// The text of the focused buffer.
    fn text(&self) -> String {
        let (_, doc) = current_ref!(self.app.editor);
        doc.text().to_string()
    }

    /// The anchor and head of the primary selection of the focused buffer.
    fn selection(&self) -> (usize, usize) {
        let (view, doc) = current_ref!(self.app.editor);
        let range = doc.selection(view.id).primary();
        (range.anchor, range.head)
    }

    /// The cell (column, row) where `text` starts on the screen.
    fn cell_of(&self, text: &str) -> (u16, u16) {
        self.rows()
            .iter()
            .enumerate()
            .find_map(|(y, row)| {
                row.find(text)
                    .map(|byte| (row[..byte].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("no {text:?} on the screen:\n{}", self.rows().join("\n")))
    }

    /// Presses the left mouse button on the cell `skip` columns after where `text` starts.
    async fn click_on(&mut self, text: &str, skip: u16) {
        let (column, row) = self.cell_of(text);
        self.click_cell(column + skip, row).await;
    }

    /// Presses the left mouse button on a cell of the screen.
    async fn click_cell(&mut self, column: u16, row: u16) {
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: Modifiers::NONE,
        };
        self.tx.send(Ok(Event::Mouse(event))).unwrap();
        assert!(self.app.event_loop_until_idle(&mut self.input).await);
    }

    /// Nothing is printed to the status line during a watch session.
    fn assert_quiet(&self, when: &str) {
        assert_eq!(
            self.app
                .editor
                .get_status()
                .map(|(message, _)| message.to_string()),
            None,
            "a message in the status line {when}"
        );
    }

    /// Three files changed one after the other (the newest last): a line 2, b line 5, c
    /// line 8. Ends following c.
    async fn three_changes(&mut self) {
        self.start_watch().await;
        self.change_and_follow("a.txt", 2, "a2 agent").await;
        self.change_and_follow("b.txt", 5, "b5 agent").await;
        self.change_and_follow("c.txt", 8, "c8 agent").await;
        assert!(self.following());
    }
}

// 1. The baseline

#[tokio::test(flavor = "multi_thread")]
async fn the_session_has_only_the_changes_made_since_the_watch_started() {
    let Some(_) = enter("the_session_has_only_the_changes_made_since_the_watch_started") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    assert!(session_of(&session.app).is_empty());

    // a file that was dirty (line 2 changed before the watch), one that was clean, one that was
    // untracked: another line of each changes now
    edit("dirty.txt", 7, "d7 agent");
    std::thread::sleep(MTIME_GAP);
    edit("a.txt", 4, "a4 agent");
    std::thread::sleep(MTIME_GAP);
    edit("untracked.txt", 5, "u5 agent");
    session
        .wait_watch("all three files in the session", |app| {
            ["dirty.txt", "a.txt", "untracked.txt"]
                .iter()
                .all(|name| file(app, name).is_some())
        })
        .await;

    // only the new hunks: the line changed before the watch (line 2) never shows
    assert_eq!(hunks(&session.app, "dirty.txt"), [(6, 7)]);
    assert_eq!(hunks(&session.app, "a.txt"), [(3, 4)]);
    assert_eq!(hunks(&session.app, "untracked.txt"), [(4, 5)]);
    for name in ["dirty.txt", "a.txt", "untracked.txt"] {
        assert_eq!(file(&session.app, name).unwrap().kind, FileKind::Modified);
    }
    assert_eq!(session_of(&session.app).change_count(), 3);
    // the file the session ended up on has the same hunk in its gutter
    let (view, doc) = current_ref!(session.app.editor);
    let _ = view;
    assert_eq!(doc.watch_diff().expect("a watch gutter").load().len(), 1);

    // the same for the picker
    session.keys(":watch-changes<ret>").await;
    let rows = session.list();
    assert_eq!(rows.len(), 6, "{rows:#?}");
    assert!(
        rows.iter().all(|row| !row.contains("before the watch")),
        "{rows:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_changed_back_to_its_baseline_leaves_the_session() {
    let Some(_) = enter("a_file_changed_back_to_its_baseline_leaves_the_session") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    assert_eq!(hunks(&session.app, "a.txt"), [(3, 4)]);
    edit("a.txt", 4, "a4");
    session
        .wait_watch("a.txt unchanged", |app| file(app, "a.txt").is_none())
        .await;
    assert_eq!(session_of(&session.app).change_count(), 0);
}

// 2. Created and deleted files

#[tokio::test(flavor = "multi_thread")]
async fn created_and_deleted_files_are_listed_and_a_deleted_one_is_not_followed() {
    let Some(_) = enter("created_and_deleted_files_are_listed_and_a_deleted_one_is_not_followed")
    else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;

    write("new.txt", "n1\nn2\nn3\n");
    session
        .wait_watch("new.txt in the session", |app| {
            file(app, "new.txt").is_some()
        })
        .await;
    assert_eq!(
        file(&session.app, "new.txt").unwrap().kind,
        FileKind::Created
    );
    // the whole file is the hunk
    assert_eq!(hunks(&session.app, "new.txt"), [(0, 3)]);
    session.wait_for_focus("new.txt", 0).await;

    std::fs::remove_file(cwd().join("del.txt")).unwrap();
    session
        .wait_watch("del.txt in the session", |app| {
            file(app, "del.txt").is_some()
        })
        .await;
    assert_eq!(
        file(&session.app, "del.txt").unwrap().kind,
        FileKind::Deleted
    );
    assert!(hunks(&session.app, "del.txt").is_empty());
    session.run_for(Duration::from_millis(300)).await;
    // following didn't try to open it
    assert_eq!(focus(&session.app), at("new.txt", 0));
    assert!(session
        .app
        .editor
        .document_by_path(cwd().join("del.txt"))
        .is_none());
    assert_eq!(is_error(&session.app), None);
    assert!(session.following());

    // n/N only go through the file that exists
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 1);

    session.keys(":watch-changes<ret>").await;
    // the newest file first: del.txt (its removed lines are one hunk), then new.txt
    assert_eq!(
        session.list(),
        [
            "── del.txt (deleted)  1 hunk  -10",
            "-- del.txt:1",
            "── new.txt (new)  1 hunk  +3",
            "++ new.txt:1-3",
        ]
    );

    // Enter on the deleted file can't open it
    session.keys("<ret>").await;
    assert!(is_error(&session.app).is_some_and(|message| message.contains("was deleted")));
}

// 3. Ignored files and lockfiles

#[tokio::test(flavor = "multi_thread")]
async fn ignored_files_are_not_in_the_session() {
    let Some(_) = enter("ignored_files_are_not_in_the_session") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    std::fs::create_dir(cwd().join("ignored")).unwrap();
    write("ignored/x.txt", "x\n");
    write("build.log", "built\n");
    std::thread::sleep(MTIME_GAP);
    edit("a.txt", 3, "a3 agent");
    session.wait_for_focus("a.txt", 2).await;
    // the batch with the ignored files is over by now
    session.run_for(Duration::from_millis(500)).await;
    let files: Vec<_> = session_of(&session.app)
        .files_newest_first()
        .into_iter()
        .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, ["a.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lockfile_is_listed_collapsed_and_neither_followed_nor_counted() {
    let Some(_) = enter("a_lockfile_is_listed_collapsed_and_neither_followed_nor_counted") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    assert!(session.on_screen("WATCH ● following"));

    write("Cargo.lock", &lines("locked", 40));
    session
        .wait_watch("Cargo.lock in the session", |app| {
            file(app, "Cargo.lock").is_some()
        })
        .await;
    session.run_for(Duration::from_millis(300)).await;
    let lock = file(&session.app, "Cargo.lock").unwrap();
    assert_eq!(lock.quiet, Quiet::Lockfile);
    assert!(lock.hunks.is_empty());
    // not followed: the focus stays, and it still follows the next real change
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert!(session.following());
    assert_eq!(session.new_count(), 0);
    assert!(session.on_screen("WATCH ● following"));
    // n/N don't go there
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 1);

    // looking around, it isn't counted as a new change either
    session.keys("vj").await;
    assert!(!session.following());
    write("Cargo.lock", &lines("again", 45));
    session.run_for(Duration::from_millis(600)).await;
    assert_eq!(session.new_count(), 0);
    assert!(session.on_screen("WATCH ◌ looking"));

    // the picker has its divider and no row of hunks
    session.keys(":watch-changes<ret>").await;
    let rows = session.list();
    assert_eq!(rows.len(), 3, "{rows:#?}");
    assert!(rows[0].contains("Cargo.lock"), "{rows:#?}");
    assert!(rows[1].contains("a.txt"), "{rows:#?}");
}

// 4. Bursts

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_writes_ends_on_the_newest_files_first_hunk() {
    let Some(_) = enter("a_burst_of_writes_ends_on_the_newest_files_first_hunk") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    // several quick writes, a few milliseconds apart: one debounced batch
    edit("a.txt", 2, "a2 agent");
    std::thread::sleep(MTIME_GAP);
    edit("b.txt", 6, "b6 agent");
    std::thread::sleep(MTIME_GAP);
    edit("a.txt", 9, "a9 agent");
    std::thread::sleep(MTIME_GAP);
    edit("c.txt", 4, "c4 agent");
    std::thread::sleep(MTIME_GAP);
    edit("c.txt", 8, "c8 agent");
    // the newest file is c.txt: the first of its hunks
    session.wait_for_focus("c.txt", 3).await;
    session.run_for(Duration::from_millis(500)).await;
    assert_eq!(focus(&session.app), at("c.txt", 3));
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2), (8, 9)]);
    assert_eq!(hunks(&session.app, "b.txt"), [(5, 6)]);
    assert_eq!(hunks(&session.app, "c.txt"), [(3, 4), (7, 8)]);
    assert!(session.following());
    assert_eq!(session.new_count(), 0);
}

// 5. Following

#[tokio::test(flavor = "multi_thread")]
async fn following_jumps_to_the_first_new_hunk_when_idle() {
    let Some(_) = enter("following_jumps_to_the_first_new_hunk_when_idle") else {
        return;
    };
    let mut session = Session::new();
    assert_eq!(focus(&session.app).0, "[scratch]");
    session.start_watch().await;
    assert!(session.following());
    // what it watches is said on the bar, not in the status line
    session.assert_quiet("when the watch started");
    let activity = session.activity();
    assert!(activity.starts_with("watching "), "{activity}");
    // the dirty and the untracked file
    assert!(
        activity.contains("2 files already uncommitted"),
        "{activity}"
    );

    session.change_and_follow("b.txt", 7, "b7 agent").await;
    assert!(session.following());
    session.wait_for_screen_text("WATCH ● following").await;
    assert_eq!(session_of(&session.app).new_count(), 0);

    // the next change, in another file, is followed too; the older file stays open
    session.change_and_follow("a.txt", 3, "a3 agent").await;
    assert!(session
        .app
        .editor
        .document_by_path(cwd().join("b.txt"))
        .is_some());
    // and a second hunk of a file whose first one is not new: the new one is the target
    session.change_and_follow("a.txt", 9, "a9 agent").await;
    assert_eq!(hunks(&session.app, "a.txt"), [(2, 3), (8, 9)]);
}

// 6. Following holds movement, `v` looks around

/// Following a change in a.txt, then `keys`: nothing moved, nothing was typed, following is
/// still on, and the bar says that `v` looks around.
async fn held_while_following(keys: &str) {
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    assert!(session.following());
    let (text, cursor, at) = (session.text(), session.selection(), focus(&session.app));

    session.keys(keys).await;
    assert!(session.following(), "{keys} stopped following");
    assert!(session.watching());
    assert_eq!(session.selection(), cursor, "{keys} moved the cursor");
    assert_eq!(focus(&session.app), at);
    assert_eq!(session.text(), text);
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    assert_eq!(session.activity(), LOOK_TO_MOVE);
    assert!(session.on_screen(LOOK_TO_MOVE));
    assert!(session.on_screen("WATCH ● following"));
    session.assert_quiet(&format!("after {keys}"));

    // and a change is still followed
    session.change_and_follow("b.txt", 6, "b6 agent").await;
    assert!(session.following());
}

macro_rules! held_on {
    ($name:ident, $keys:expr) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some(_) = enter(stringify!($name)) else {
                return;
            };
            held_while_following($keys).await;
        }
    };
}

held_on!(moving_with_j_is_held_while_following, "j");
held_on!(gg_is_held_while_following, "gg");
held_on!(a_word_motion_is_held_while_following, "w");
held_on!(scrolling_is_held_while_following, "<C-d>");
// (`/` is held, so the rest is typed in normal mode: held too, or harmless)
held_on!(starting_a_search_is_held_while_following, "/");

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_search_typed_while_following_changes_nothing() {
    let Some(_) = enter("a_whole_search_typed_while_following_changes_nothing") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    let (text, cursor) = (session.text(), session.selection());
    session.keys("/a8<ret>").await;
    assert!(session.following());
    assert!(session.watching());
    assert_eq!(session.selection(), cursor);
    assert_eq!(session.text(), text);
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    session.assert_quiet("after the search");
}

/// `v`, then `keys`: you move, following is off, and changes are counted, not followed.
async fn looks_around_with(keys: &str) {
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    assert!(session.following());

    session.keys("v").await;
    assert!(!session.following());
    assert!(session.watching());
    session.wait_for_screen_text("WATCH ◌ looking").await;
    session.assert_quiet("after v");
    // the badge now offers to follow again
    assert!(session.on_screen(" v  follow"));
    let cursor = session.selection();

    session.keys(keys).await;
    assert_ne!(session.selection(), cursor, "{keys} didn't move");
    assert!(!session.following());
    assert!(session.watching());
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    let before = focus(&session.app);

    // a change now doesn't take the view away
    edit("b.txt", 6, "b6 agent");
    session
        .wait_watch("b.txt counted as new", |app| {
            session_of(app).new_count() == 1
        })
        .await;
    session.wait_for_screen_text("1 new").await;
    assert_eq!(focus(&session.app), before);
    assert!(!session.following());

    std::thread::sleep(MTIME_GAP);
    edit("c.txt", 2, "c2 agent");
    session
        .wait_watch("c.txt counted as new", |app| {
            session_of(app).new_count() == 2
        })
        .await;
    session.wait_for_screen_text("2 new").await;
    assert_eq!(focus(&session.app), before);
    session.assert_quiet("while looking");
}

macro_rules! looks_with {
    ($name:ident, $keys:expr) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some(_) = enter(stringify!($name)) else {
                return;
            };
            looks_around_with($keys).await;
        }
    };
}

looks_with!(j_moves_while_looking, "j");
looks_with!(gg_moves_while_looking, "gg");
looks_with!(a_word_motion_moves_while_looking, "w");
looks_with!(scrolling_moves_while_looking, "<C-d>");
looks_with!(a_search_moves_while_looking, "/a8<ret>");

#[tokio::test(flavor = "multi_thread")]
async fn v_again_follows_again_from_the_newest_change() {
    let Some(_) = enter("v_again_follows_again_from_the_newest_change") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    session.keys("vj").await;
    assert!(!session.following());
    // two changes while looking: counted, the view stays
    edit("b.txt", 6, "b6 agent");
    std::thread::sleep(MTIME_GAP);
    edit("c.txt", 2, "c2 agent");
    session
        .wait_watch("two new changes", |app| session_of(app).new_count() == 2)
        .await;
    assert_eq!(focus(&session.app), at("a.txt", 4));

    // v: following again, on the newest change, which is no longer new
    session.keys("v").await;
    assert!(session.following());
    assert_eq!(focus(&session.app), at("c.txt", 1));
    assert_eq!(session.new_count(), 0);
    session.wait_for_screen_text("WATCH ● following").await;
    assert!(session.on_screen(" v  look"));
    assert_eq!(session.activity(), "following again");
    session.assert_quiet("after v again");

    // and it follows the next change
    session.change_and_follow("other.txt", 5, "o5 agent").await;
    assert!(session.following());
}

/// A click on the text while following does not look around: it does nothing, and the bar says
/// that `v` looks around (the whole rule: [`the_mouse_does_nothing_while_following`]).
#[tokio::test(flavor = "multi_thread")]
async fn a_click_in_the_text_while_following_does_not_look_around() {
    let Some(_) = enter("a_click_in_the_text_while_following_does_not_look_around") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    // a click on the text (the first row, well away from the bar)
    session.click_cell(3, 0).await;
    assert!(session.following());
    assert!(session.watching());
    assert_eq!(session.activity(), LOOK_TO_MOVE);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_line_keeps_following_and_esc_leaves() {
    let Some(_) = enter("the_command_line_keeps_following_and_esc_leaves") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    // opening the command line and leaving it changes nothing
    session.keys(":<esc>").await;
    assert!(session.following());
    assert!(session.watching());
    // the watch commands keep watch mode: the picker looks around
    session.keys(":watch-changes<ret>").await;
    assert!(session.watching());
    assert!(!session.following());
    session.keys("<esc>").await;
    assert!(session.watching());
    // `:goto` looks around
    session.keys("v:goto 7<ret>").await;
    assert!(session.watching());
    assert!(!session.following());
    assert_eq!(focus(&session.app), at("a.txt", 6));
    // Esc, in the text, leaves watch mode
    session.keys("<esc>").await;
    assert!(!session.watching());
    assert!(session.app.editor.watch.live().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typed_command_other_than_the_watch_commands_leaves_watch_mode() {
    let Some(_) = enter("a_typed_command_other_than_the_watch_commands_leaves_watch_mode") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys(":goto 3<ret>").await;
    // :goto only looks around
    assert!(session.watching());
    assert!(!session.following());
    assert_eq!(focus(&session.app), at("c.txt", 2));
    session.keys(":open other.txt<ret>").await;
    assert!(!session.watching());
    assert!(session.app.editor.watch.live().is_some());
    assert_eq!(focus(&session.app).0, "other.txt");
}

// 7. n and N

#[tokio::test(flavor = "multi_thread")]
async fn n_and_n_go_through_the_changes_in_the_order_they_happened() {
    let Some(_) = enter("n_and_n_go_through_the_changes_in_the_order_they_happened") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;

    // n at the newest change stays there (no wrap around), following goes on
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert_eq!(session.activity(), "at the newest change");
    assert!(session.following());
    assert!(session.watching());
    session.assert_quiet("after n");
    assert_eq!(session.new_count(), 0);

    // N goes back to the oldest, looking
    session.keys("NN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert!(!session.following());
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(!session.following());
    session.keys("n").await;
    // the newest: following again
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert!(session.following());
    assert_eq!(session.activity(), "at the newest change: following again");
    session.wait_for_screen_text("WATCH ● following").await;
    session.assert_quiet("after n reached the newest change");

    // N goes back: an older change is looked at, not followed
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(!session.following());
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    // N at the oldest change stays there
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert_eq!(session.activity(), "at the oldest change");
    assert!(!session.following());
    session.assert_quiet("after N at the oldest change");
    session.keys("nn").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert!(session.following());

    // a new change while looking is counted, and n goes to it and follows again
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    edit("other.txt", 3, "o3 agent");
    session
        .wait_watch("one new change", |app| session_of(app).new_count() == 1)
        .await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("other.txt", 2));
    assert!(session.following());
    assert_eq!(session.new_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn n_from_the_middle_of_a_file_goes_to_the_next_change_not_the_next_line() {
    let Some(_) = enter("n_from_the_middle_of_a_file_goes_to_the_next_change_not_the_next_line")
    else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    std::thread::sleep(MTIME_GAP);
    session.change_and_follow("a.txt", 8, "a8 agent").await;
    // a2 is older than a8: N goes there, and n back
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("a.txt", 7));
    assert!(session.following());
}

// 8. Keys that edit or open something are held, Esc leaves, the changes stay

/// Three changes, then `keys`, which would edit or open something: nothing happened, watch
/// mode is on and the bar says that `Esc` leaves it first. Then `Esc`: watch mode is left, the
/// session and its changes are still there.
async fn held_then_left(keys: &str) -> Session {
    let mut session = Session::new();
    session.three_changes().await;
    held(&mut session, keys).await;
    session.keys("<esc>").await;
    assert!(!session.watching(), "Esc didn't leave watch mode");
    session.assert_quiet("after Esc");
    kept(session)
}

/// `keys` change nothing in watch mode (following or looking): the bar says why.
async fn held(session: &mut Session, keys: &str) {
    let (text, cursor, at, following) = (
        session.text(),
        session.selection(),
        focus(&session.app),
        session.following(),
    );
    session.keys(keys).await;
    assert!(session.watching(), "{keys} left watch mode");
    assert_eq!(session.following(), following, "{keys} changed following");
    assert_eq!(session.text(), text, "{keys} changed the text");
    assert_eq!(session.selection(), cursor);
    assert_eq!(focus(&session.app), at);
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    assert_eq!(session.activity(), LEAVE_TO_EDIT, "{keys}");
    assert!(session.on_screen(LEAVE_TO_EDIT));
    assert!(
        !session.rows().iter().any(|row| row.starts_with('┌')),
        "{keys} opened a picker"
    );
    session.assert_quiet(&format!("after {keys}"));
}

/// Three changes, then `keys`: watch mode is left, the session and its changes are still there.
async fn leaving(keys: &str) -> Session {
    let mut session = Session::new();
    session.three_changes().await;
    session.keys(keys).await;
    assert!(!session.watching(), "{keys} didn't leave watch mode");
    kept(session)
}

/// Watch mode is left: the session stays live, its changes and gutter markers stay.
fn kept(mut session: Session) -> Session {
    assert!(session.app.editor.watch.session.is_some());
    assert!(session.app.editor.watch.live().is_some());
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 3);
    assert!(!session.on_screen("WATCH"));
    // the gutter markers stay
    let doc = session
        .app
        .editor
        .document_by_path(cwd().join("c.txt"))
        .expect("c.txt is open");
    assert_eq!(doc.watch_diff().expect("a watch gutter").load().len(), 1);
    session
}

/// From where watch mode was left, n and N go through the session; `:watch-clear` forgets it.
async fn changes_stay_after_leaving(mut session: Session) {
    // the newest change is where it was left: n stays, and says nothing
    let before = focus(&session.app);
    session.keys("n").await;
    assert_eq!(focus(&session.app), before);
    session.assert_quiet("after n at the newest change");
    session.keys("NN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    // watch mode is off: it doesn't follow again
    assert!(!session.watching());
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert!(!session.watching());
    // and back: N at the oldest change stays there, without a message
    session.keys("NNN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    session.assert_quiet("after N at the oldest change");
    session.keys("nn").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));

    // the picker still lists them
    session.keys(":watch-changes<ret>").await;
    assert_eq!(session.list().len(), 6);
    session.keys("<esc>").await;

    // :watch-clear forgets them
    session.keys(":watch-clear<ret>").await;
    assert!(session.app.editor.watch.session.is_none());
    for name in ["a.txt", "b.txt", "c.txt"] {
        let doc = session
            .app
            .editor
            .document_by_path(cwd().join(name))
            .unwrap();
        assert!(doc.watch_diff().is_none(), "{name} keeps its markers");
    }
    // n is the search again: without a pattern it doesn't go anywhere
    let before = focus(&session.app);
    session.keys("n").await;
    assert_eq!(focus(&session.app), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_text_is_held_until_esc_leaves_watch_mode() {
    let Some(_) = enter("typing_text_is_held_until_esc_leaves_watch_mode") else {
        return;
    };
    let session = held_then_left("i").await;
    changes_stay_after_leaving(session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_a_file_with_a_typed_command_leaves_watch_mode_but_the_session_stays() {
    let Some(_) =
        enter("opening_a_file_with_a_typed_command_leaves_watch_mode_but_the_session_stays")
    else {
        return;
    };
    let session = leaving(":open other.txt<ret>").await;
    assert_eq!(focus(&session.app).0, "other.txt");
    changes_stay_after_leaving(session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_picker_is_held_until_esc_leaves_watch_mode() {
    let Some(_) = enter("a_picker_is_held_until_esc_leaves_watch_mode") else {
        return;
    };
    let session = held_then_left("<space>f").await;
    changes_stay_after_leaving(session).await;
}

/// Edit and open keys, in both states: held, and `Esc` is the way out.
async fn edit_keys_are_held(looking: bool) {
    let mut session = Session::new();
    session.three_changes().await;
    if looking {
        session.keys("v").await;
        assert!(!session.following());
    }
    for keys in ["i", "d", "o", "<space>f"] {
        held(&mut session, keys).await;
        // the bar's note is for the next key
        session.app.editor.watch.session.as_mut().unwrap().activity = None;
    }
    // watch mode is still on, with its state: a change is followed or counted as before
    assert_eq!(session.following(), !looking);
    session.keys("<esc>").await;
    assert!(!session.watching());
    assert!(session.app.editor.watch.live().is_some());
    // now the keys are the editor's
    session.keys("ix<esc>").await;
    assert!(session.text().contains('x'));
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_keys_are_held_while_following() {
    let Some(_) = enter("edit_keys_are_held_while_following") else {
        return;
    };
    edit_keys_are_held(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_keys_are_held_while_looking() {
    let Some(_) = enter("edit_keys_are_held_while_looking") else {
        return;
    };
    edit_keys_are_held(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_leaves_watch_mode_and_the_session_goes_on_recording_and_says_nothing() {
    let Some(_) = enter("esc_leaves_watch_mode_and_the_session_goes_on_recording_and_says_nothing")
    else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("<esc>").await;
    assert!(!session.watching());
    session.assert_quiet("after Esc");
    assert!(session.app.editor.watch.session.is_some());
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 3);
    assert!(!session.on_screen("WATCH"));
    assert!(!session.on_screen(" v  look"));
    // the files are still watched: a change is recorded, but not followed
    assert!(session.app.editor.watch.live().is_some());
    let before = focus(&session.app);
    edit("other.txt", 3, "o3 agent");
    session
        .wait_watch("other.txt in the session", |app| {
            file(app, "other.txt").is_some()
        })
        .await;
    session.run_for(Duration::from_millis(300)).await;
    assert_eq!(focus(&session.app), before);
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 4);
    session.assert_quiet("after a change out of watch mode");
    // the keys of the editor are back, `v` is select mode again
    session.keys("v").await;
    assert_eq!(session.app.editor.mode(), Mode::Select);
    session.keys("<esc>").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_stop_stops_the_session_and_the_files_are_no_longer_watched() {
    let Some(_) = enter("watch_stop_stops_the_session_and_the_files_are_no_longer_watched") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys(":watch-stop<ret>").await;
    assert!(!session.watching());
    assert!(!session_of(&session.app).is_live());
    assert_eq!(status(&session.app), "Watch ended: 3 changes");
    let before = focus(&session.app);
    edit("other.txt", 3, "o3 agent");
    session.run_for(Duration::from_millis(800)).await;
    assert!(file(&session.app, "other.txt").is_none());
    assert_eq!(focus(&session.app), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn n_and_n_stop_at_the_newest_and_the_oldest_change() {
    let Some(_) = enter("n_and_n_stop_at_the_newest_and_the_oldest_change") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert_eq!(session.activity(), "at the newest change");
    session.wait_for_screen_text("at the newest change").await;
    assert!(session.following());
    session.assert_quiet("after n at the newest change");

    session.keys("NN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert_eq!(session.activity(), "at the oldest change");
    session.wait_for_screen_text("at the oldest change").await;
    assert!(!session.following());
    session.assert_quiet("after N at the oldest change");
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_then_watch_resumes_in_the_same_place_following() {
    let Some(_) = enter("esc_then_watch_resumes_in_the_same_place_following") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("N").await;
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert!(session.following());
    let seen_at = session_of(&session.app).at.clone();

    session.keys("<esc>").await;
    assert!(!session.watching());
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert!(session.following());
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert_eq!(session_of(&session.app).at, seen_at);
    assert_eq!(session.activity(), "back: nothing changed meanwhile");
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 3);
    session.assert_quiet("after resuming");
    session.wait_for_screen_text("WATCH ● following").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_then_watch_resumes_looking_around_where_you_were() {
    let Some(_) = enter("esc_then_watch_resumes_looking_around_where_you_were") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("v").await;
    assert!(!session.following());
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(!session.following());
    let seen_at = session_of(&session.app).at.clone();

    session.keys("<esc>").await;
    assert!(!session.watching());
    assert!(!session.following());
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert!(!session.following());
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert_eq!(session_of(&session.app).at, seen_at);
    session.wait_for_screen_text("WATCH ◌ looking").await;
    session.assert_quiet("after resuming");
    // n goes on from where it was
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_made_out_of_watch_mode_are_recorded_not_followed_and_counted_on_return() {
    let Some(_) =
        enter("changes_made_out_of_watch_mode_are_recorded_not_followed_and_counted_on_return")
    else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("<esc>").await;
    assert!(!session.watching());
    let before = focus(&session.app);

    // a.txt is open and clean: it is reloaded and gets its second marker
    edit("a.txt", 6, "a6 agent");
    session
        .wait_watch("the new hunk of a.txt", |app| {
            file(app, "a.txt").is_some_and(|file| file.hunks.len() == 2)
        })
        .await;
    session.run_for(Duration::from_millis(300)).await;
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2), (5, 6)]);
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 4);
    // nothing was followed
    assert_eq!(focus(&session.app), before);
    assert!(!session.watching());
    assert!(!session.on_screen("WATCH"));
    session.assert_quiet("after a change out of watch mode");
    let doc = session
        .app
        .editor
        .document_by_path(cwd().join("a.txt"))
        .expect("a.txt is open");
    assert_eq!(doc.watch_diff().expect("a watch gutter").load().len(), 2);

    // back: following again, the catch-up shows the change made meanwhile (and it is seen)
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert_eq!(focus(&session.app), at("a.txt", 5));
    session
        .wait_watch("the catch-up to end", |app| {
            session_of(app).catch_up.is_none()
        })
        .await;
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
    session.wait_for_screen_text("caught up: following").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_stop_after_esc_stops_the_session_and_watch_starts_a_fresh_one() {
    let Some(_) = enter("watch_stop_after_esc_stops_the_session_and_watch_starts_a_fresh_one")
    else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("<esc>").await;
    assert!(session.app.editor.watch.live().is_some());
    session.keys(":watch-stop<ret>").await;
    assert!(!session.watching());
    assert!(!session_of(&session.app).is_live());
    assert!(session.app.editor.watch.live().is_none());
    assert_eq!(status(&session.app), "Watch ended: 3 changes");
    // the changes stay until the next :watch
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 3);
    // nothing is recorded any more
    edit("other.txt", 3, "o3 agent");
    session.run_for(Duration::from_millis(800)).await;
    assert!(file(&session.app, "other.txt").is_none());

    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert!(session_of(&session.app).is_live());
    assert!(session_of(&session.app).hunks_in_order().is_empty());
    assert!(file(&session.app, "a.txt").is_none());
    assert!(session.following());
}

#[tokio::test(flavor = "multi_thread")]
async fn entering_debug_mode_leaves_watch_mode_with_the_session_live_and_watch_comes_back() {
    let Some(_) =
        enter("entering_debug_mode_leaves_watch_mode_with_the_session_live_and_watch_comes_back")
    else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    let before = focus(&session.app);
    session.keys(":debug-mode<ret>").await;
    assert!(session.app.editor.debug.mode_on());
    assert!(!session.watching());
    assert!(session.app.editor.watch.live().is_some());
    assert_eq!(session_of(&session.app).hunks_in_order().len(), 3);

    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert!(!session.app.editor.debug.mode_on());
    assert!(session.following());
    assert_eq!(focus(&session.app), before);
    assert_eq!(session.activity(), "back: nothing changed meanwhile");
}

#[tokio::test(flavor = "multi_thread")]
async fn after_a_search_made_once_the_session_was_stopped_n_repeats_the_search() {
    let Some(_) = enter("after_a_search_made_once_the_session_was_stopped_n_repeats_the_search")
    else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    session.keys(":watch-stop<ret>").await;
    assert!(!session.watching());
    // the cursor is on line 4 (a hunk); a search starts from the cursor
    session.keys("/a<ret>").await;
    let (name, line) = focus(&session.app);
    assert_eq!(name, "a.txt");
    assert!(line >= 3, "the search found line {line}");
    session.keys("n").await;
    // the search goes on to the next line; it didn't go to the hunk
    assert_eq!(focus(&session.app), at("a.txt", line + 1));
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", line));
    // the session is still there
    assert!(session.app.editor.watch.session.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn while_watching_a_search_does_not_take_n() {
    let Some(_) = enter("while_watching_a_search_does_not_take_n") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    std::thread::sleep(MTIME_GAP);
    session.change_and_follow("a.txt", 8, "a8 agent").await;
    // looking around: the search moves the cursor
    session.keys("v/a<ret>").await;
    assert!(!session.following());
    assert!(session.watching());
    // N is still the session's: the change before the newest, at line 4
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 3));
    assert!(!session.following());
}

#[tokio::test(flavor = "multi_thread")]
async fn watching_again_starts_a_fresh_session() {
    let Some(_) = enter("watching_again_starts_a_fresh_session") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    session.keys(":watch-stop<ret>").await;
    session.start_watch().await;
    // a.txt is dirty now: its edit is the new baseline, and the old markers are gone
    assert!(session_of(&session.app).is_empty());
    assert!(session.following());
    session.change_and_follow("b.txt", 3, "b3 agent").await;
    assert!(file(&session.app, "a.txt").is_none());
    assert_eq!(session_of(&session.app).change_count(), 1);
}

// 9. Unsaved buffers

#[tokio::test(flavor = "multi_thread")]
async fn a_buffer_with_unsaved_edits_is_never_overwritten_nor_followed() {
    let Some(_) = enter("a_buffer_with_unsaved_edits_is_never_overwritten_nor_followed") else {
        return;
    };
    let mut session = Session::new();
    session
        .keys(":open a.txt<ret>ix<esc>:open other.txt<ret>")
        .await;
    let a = cwd().join("a.txt");
    assert!(session
        .app
        .editor
        .document_by_path(&a)
        .unwrap()
        .is_modified());
    assert_eq!(focus(&session.app).0, "other.txt");
    // (watch mode starts after the edit: typing leaves it)
    session.start_watch().await;

    edit("a.txt", 6, "a6 agent");
    session
        .wait_watch("a.txt in the session", |app| file(app, "a.txt").is_some())
        .await;
    session.run_for(Duration::from_millis(500)).await;
    // the buffer keeps what the user typed
    let doc = session.app.editor.document_by_path(&a).unwrap();
    assert!(doc.is_modified());
    assert_eq!(doc.text().line(0).to_string(), "xa1\n");
    assert_eq!(doc.text().line(5).to_string(), "a6\n");
    // the stale-buffer warning
    let (message, severity) = session.app.editor.get_status().expect("a warning");
    assert_eq!(*severity, Severity::Warning, "{message}");
    assert!(message.contains("changed on disk"), "{message}");
    // and following stayed where it was
    assert_eq!(focus(&session.app).0, "other.txt");

    // other files still follow
    edit("b.txt", 7, "b7 agent");
    session.wait_for_focus("b.txt", 6).await;
    // the disk has the change (in the session) but the buffer doesn't
    assert_eq!(hunks(&session.app, "a.txt"), [(5, 6)]);
    let doc = session.app.editor.document_by_path(&a).unwrap();
    assert_eq!(doc.text().line(5).to_string(), "a6\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_open_buffer_is_reloaded_and_followed() {
    let Some(_) = enter("a_clean_open_buffer_is_reloaded_and_followed") else {
        return;
    };
    let mut session = Session::new();
    session.keys(":open a.txt<ret>:open other.txt<ret>").await;
    session.start_watch().await;
    edit("a.txt", 6, "a6 agent");
    session.wait_for_focus("a.txt", 5).await;
    let doc = session
        .app
        .editor
        .document_by_path(cwd().join("a.txt"))
        .unwrap();
    assert!(!doc.is_modified());
    assert_eq!(doc.text().line(5).to_string(), "a6 agent\n");
    // the gutter has the hunk
    assert_eq!(doc.watch_diff().expect("a watch gutter").load().len(), 1);
}

// 10. The picker

#[tokio::test(flavor = "multi_thread")]
async fn the_picker_lists_the_newest_file_and_hunk_first_and_enter_goes_to_it() {
    let Some(_) = enter("the_picker_lists_the_newest_file_and_hunk_first_and_enter_goes_to_it")
    else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.change_and_follow("b.txt", 5, "b5 agent").await;
    session.change_and_follow("a.txt", 8, "a8 agent").await;

    session.keys(":watch-changes<ret>").await;
    // opening it looks around
    assert!(!session.following());
    session.assert_quiet("when the picker opened");
    assert!(
        session.on_screen("this session: 3 change"),
        "{}",
        session.rows().join("\n")
    );
    // a.txt changed last: first; in it the newest hunk first
    assert_eq!(
        session.list(),
        [
            "── a.txt  2 hunks  +2 -2",
            "-+ a.txt:8",
            "-+ a.txt:2",
            "── b.txt  1 hunk  +1 -1",
            "-+ b.txt:5",
        ]
    );
    assert_eq!(session.selected_row(), "-+ a.txt:8");

    // Enter on the second file's hunk
    session.keys("jj").await;
    assert_eq!(session.selected_row(), "-+ b.txt:5");
    session.keys("<ret>").await;
    assert!(
        !session.rows().iter().any(|row| row.starts_with('┌')),
        "the picker is still open"
    );
    assert_eq!(focus(&session.app), at("b.txt", 4));

    // and on an older hunk of the first
    session.keys(":watch-changes<ret>").await;
    session.keys("j").await;
    assert_eq!(session.selected_row(), "-+ a.txt:2");
    session.keys("<ret>").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    // n goes on from there, in the order they happened
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("a.txt", 7));
    assert!(session.following());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_picker_says_so_without_a_session_or_changes() {
    let Some(_) = enter("the_picker_says_so_without_a_session_or_changes") else {
        return;
    };
    let mut session = Session::new();
    session.keys(":watch-changes<ret>").await;
    assert_eq!(is_error(&session.app).as_deref(), Some("no watch session"));
    session.start_watch().await;
    session.keys(":watch-changes<ret>").await;
    assert_eq!(status(&session.app), "No changes this session");
    assert!(!session.rows().iter().any(|row| row.starts_with('┌')));
}

// 10b. The mode bar

#[tokio::test(flavor = "multi_thread")]
async fn the_mode_bar_is_drawn_while_watching_and_gone_after_esc() {
    let Some(_) = enter("the_mode_bar_is_drawn_while_watching_and_gone_after_esc") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    session.wait_for_screen_text("WATCH ● following").await;
    for text in [
        " v  look",
        " n  next",
        " N  previous",
        " c  changes",
        " Esc  leave",
    ] {
        assert!(
            session.on_screen(text),
            "no {text:?}:\n{}",
            session.rows().join("\n")
        );
    }
    // the current change, and the activity
    assert!(session.on_screen("a.txt"));
    assert!(session.on_screen("1/1 change"));
    assert!(session.on_screen("1 file"));
    assert!(session.on_screen("followed a.txt:4"));
    // the default statusline doesn't repeat it as text
    assert!(!session.on_screen("WATCH following"));
    assert!(!session.on_screen("WATCH paused"));

    // looking: the state and the first badge change, and what is new is counted
    session.keys("v").await;
    assert!(session.on_screen("WATCH ◌ looking"));
    assert!(!session.on_screen("WATCH ● following"));
    assert!(session.on_screen(" v  follow"));
    assert!(!session.on_screen(" v  look "));
    edit("b.txt", 6, "b6 agent");
    session.wait_for_screen_text("1 new").await;

    session.keys("<esc>").await;
    assert!(!session.watching());
    for text in ["WATCH", " v  ", " n  next", " Esc  leave", "1 new"] {
        assert!(
            !session.on_screen(text),
            "{text:?} is still drawn:\n{}",
            session.rows().join("\n")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clicking_a_badge_runs_its_command() {
    let Some(_) = enter("clicking_a_badge_runs_its_command") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.run_for(Duration::from_millis(100)).await;

    // the label of the first badge looks around ...
    session.click_on(" v  look", 4).await;
    assert!(!session.following());
    assert!(session.watching());
    assert!(session.on_screen("WATCH ◌ looking"));
    // ... and then follows again, from the newest change
    session.click_on(" v  follow", 4).await;
    assert!(session.following());
    assert_eq!(focus(&session.app), at("c.txt", 7));

    // the key of a badge works as well as its label
    session.click_on(" N  previous", 1).await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(!session.following());
    session.click_on(" n  next", 5).await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert!(session.following());

    // leave
    session.click_on(" Esc  leave", 7).await;
    assert!(!session.watching());
    assert!(!session.on_screen("WATCH"));
    session.assert_quiet("after the clicks");
}

/// The `changes` badge opens the picker, as the `c` key does.
#[tokio::test(flavor = "multi_thread")]
async fn clicking_the_changes_badge_opens_the_picker() {
    let Some(_) = enter("clicking_the_changes_badge_opens_the_picker") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.run_for(Duration::from_millis(100)).await;
    session.click_on(" c  changes", 4).await;
    session.wait_for_screen_text("c.txt  1 hunk").await;
    assert_eq!(session.list().len(), 6);
    assert!(session.watching());
    assert!(!session.following());
    session.keys("<esc>").await;
    assert!(session.watching());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_remapped_in_keys_watch_shows_on_the_bar_and_works() {
    let Some(_) = enter("a_key_remapped_in_keys_watch_shows_on_the_bar_and_works") else {
        return;
    };
    let toml =
        "[keys.watch]\nl = \"watch_look\"\nv = \"no_op\"\nx = \"watch_leave\"\nesc = \"no_op\"\n";
    let loaded = vortex_term::config::Config::load(
        Ok(&toml.to_string()),
        Err(vortex_term::config::ConfigLoadError::default()),
    )
    .unwrap();
    let mut config = super::helpers::test_config();
    config.layers = loaded.layers;
    let mut session = Session::with_config(config);
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;

    assert!(
        session.on_screen(" l  look"),
        "{}",
        session.rows().join("\n")
    );
    assert!(!session.on_screen(" v  look"));
    assert!(session.on_screen(" x  leave"));
    // the defaults that weren't remapped are still there
    assert!(session.on_screen(" n  next"));

    // v is nothing now, l looks around
    session.keys("v").await;
    assert!(session.following());
    assert_eq!(session.app.editor.mode(), Mode::Normal);
    session.keys("l").await;
    assert!(!session.following());
    assert!(session.on_screen(" l  follow"));
    session.keys("l").await;
    assert!(session.following());
    // and x leaves
    session.keys("x").await;
    assert!(!session.watching());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_printed_to_the_status_line_during_a_watch_session() {
    let Some(_) = enter("nothing_is_printed_to_the_status_line_during_a_watch_session") else {
        return;
    };
    let mut session = Session::new();
    session.assert_quiet("before the watch");
    session.start_watch().await;
    session.assert_quiet("when the watch started");
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    session.assert_quiet("after following a change");
    session.change_and_follow("b.txt", 6, "b6 agent").await;
    session.keys("n").await;
    session.assert_quiet("after n");
    session.keys("N").await;
    session.assert_quiet("after N");
    session.keys("v").await;
    session.assert_quiet("after v");
    session.keys("j").await;
    session.assert_quiet("after a move");
    session.keys("v").await;
    session.assert_quiet("after v again");
    session.keys("d").await;
    session.assert_quiet("after a held key");
    session.keys("c").await;
    session.keys("<esc>").await;
    session.assert_quiet("after the picker");
    session.run_for(Duration::from_millis(300)).await;
    session.assert_quiet("a moment later");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_key_opens_the_picker_of_the_changes_and_looks() {
    let Some(_) = enter("the_c_key_opens_the_picker_of_the_changes_and_looks") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("c").await;
    assert!(session.watching());
    assert!(!session.following());
    assert_eq!(session.list().len(), 6);
    // enter goes to the change (the newest first: c.txt)
    session.keys("j<ret>").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(session.watching());
}

#[tokio::test(flavor = "multi_thread")]
async fn n_to_the_newest_change_follows_and_n_to_an_older_one_looks() {
    let Some(_) = enter("n_to_the_newest_change_follows_and_n_to_an_older_one_looks") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    // N: an older change, looking
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(!session.following());
    assert!(session.on_screen("WATCH ◌ looking"));
    // n: the newest again, following
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert!(session.following());
    assert!(session.on_screen("WATCH ● following"));
    // a change is followed again
    session.change_and_follow("other.txt", 5, "o5 agent").await;
    assert!(session.following());
}

// 11. `vx --watch`

#[tokio::test(flavor = "multi_thread")]
async fn the_watch_flag_starts_watch_mode() {
    let Some(_) = enter("the_watch_flag_starts_watch_mode") else {
        return;
    };
    let app = super::helpers::AppBuilder::new()
        .with_watch()
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    assert!(session.watching());
    assert!(session.following());
    assert!(session_of(&session.app).is_empty());
    session.run_for(Duration::from_millis(300)).await;
    session.change_and_follow("a.txt", 5, "a5 agent").await;
    assert_eq!(hunks(&session.app, "a.txt"), [(4, 5)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_watch_flag_nothing_is_watched() {
    let Some(_) = enter("without_the_watch_flag_nothing_is_watched") else {
        return;
    };
    let session = Session::new();
    assert!(!session.watching());
    assert!(session.app.editor.watch.session.is_none());
}

// 12. No network

#[tokio::test(flavor = "multi_thread")]
async fn watch_starts_promptly_with_a_remote_that_cannot_be_reached() {
    let Some(_) = enter("watch_starts_promptly_with_a_remote_that_cannot_be_reached") else {
        return;
    };
    // the repository has the remote and the tracking branch
    assert!(git(&cwd(), "", &["remote", "-v"]).contains("127.0.0.1:9"));
    assert_eq!(
        git(&cwd(), "", &["config", "branch.main.remote"]).trim(),
        "origin"
    );
    let mut session = Session::new();
    let start = Instant::now();
    session.keys(":watch<ret>").await;
    let took = start.elapsed();
    assert!(session.watching(), "{}", status(&session.app));
    assert!(took < Duration::from_secs(1), ":watch took {took:?}");
    session.run_for(Duration::from_millis(300)).await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    assert_eq!(hunks(&session.app, "a.txt"), [(3, 4)]);
    // following needed no network either
    assert!(start.elapsed() < Duration::from_secs(15));
}

// 13. Commands

#[tokio::test(flavor = "multi_thread")]
async fn watch_twice_leaves_and_resumes_and_watch_stop_says_when_there_is_nothing_to_stop() {
    let Some(_) =
        enter("watch_twice_leaves_and_resumes_and_watch_stop_says_when_there_is_nothing_to_stop")
    else {
        return;
    };
    let mut session = Session::new();
    session.keys(":watch-stop<ret>").await;
    assert!(
        is_error(&session.app).is_some_and(|error| error.ends_with("no watch session to stop")),
        "{:?}",
        is_error(&session.app)
    );
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    // :watch again leaves watch mode, and says nothing
    session.keys(":watch<ret>").await;
    assert!(!session.watching());
    assert!(session.app.editor.watch.live().is_some());
    assert_eq!(status(&session.app), "");
    // and once more resumes the same session
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert_eq!(hunks(&session.app, "a.txt"), [(3, 4)]);
    assert_eq!(status(&session.app), "");
    // :watch-stop stops it for good
    session.keys(":watch-stop<ret>").await;
    assert!(!session.watching());
    assert_eq!(status(&session.app), "Watch ended: 1 change");
    session.keys(":watch-stop<ret>").await;
    assert!(
        is_error(&session.app).is_some_and(|error| error.ends_with("no watch session to stop")),
        "{:?}",
        is_error(&session.app)
    );
    // :watch again starts a fresh session
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert!(session_of(&session.app).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_clear_says_what_it_did() {
    let Some(_) = enter("watch_clear_says_what_it_did") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    session.keys(":watch-clear<ret>").await;
    assert_eq!(
        status(&session.app),
        "Watch ended and its changes forgotten"
    );
    assert!(session.app.editor.watch.session.is_none());
    assert!(!session.watching());
    assert!(!session.on_screen("WATCH"));
    // nothing is watched any more
    let before = focus(&session.app);
    edit("b.txt", 2, "b2 agent");
    session.run_for(Duration::from_millis(800)).await;
    assert_eq!(focus(&session.app), before);
    session.keys(":watch-clear<ret>").await;
    assert!(session.app.editor.watch.session.is_none());
}

/// A directory that is not in a repository.
fn make_plain(parent: &Path) -> PathBuf {
    let work = parent.join("plain");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.txt"), "a\n").unwrap();
    // (the shell is how another process makes files)
    assert!(Command::new("sh")
        .args(["-c", "printf 'b\\n' > b.txt"])
        .current_dir(&work)
        .status()
        .unwrap()
        .success());
    work
}

fn enter_plain(test: &str) -> Option<PathBuf> {
    Repo::enter_with(module_path!(), test, make_plain)
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_outside_a_repository_is_an_error() {
    let Some(_) = enter_plain("watch_outside_a_repository_is_an_error") else {
        return;
    };
    let mut session = Session::new();
    session.keys(":watch<ret>").await;
    let error = is_error(&session.app).expect("an error");
    assert!(error.contains("not in a git repository"), "{error}");
    assert!(session.app.editor.watch.session.is_none());
    assert!(!session.watching());
    // nothing follows
    write("a.txt", "changed\n");
    session.run_for(Duration::from_millis(600)).await;
    assert_eq!(focus(&session.app).0, "[scratch]");
    session.keys(":watch-changes<ret>").await;
    assert_eq!(is_error(&session.app).as_deref(), Some("no watch session"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_watch_flag_outside_a_repository_is_an_error_and_the_editor_still_starts() {
    let Some(_) =
        enter_plain("the_watch_flag_outside_a_repository_is_an_error_and_the_editor_still_starts")
    else {
        return;
    };
    let app = super::helpers::AppBuilder::new()
        .with_watch()
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    assert!(session.app.editor.watch.session.is_none());
    assert!(is_error(&session.app).is_some_and(|error| error.contains("not in a git repository")));
    session.keys("ihello<esc>").await;
    let (_, doc) = current_ref!(session.app.editor);
    assert_eq!(doc.text().to_string(), "hello\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_while_watching_changes_nothing_of_the_session() {
    let Some(_) = enter("a_commit_while_watching_changes_nothing_of_the_session") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 4, "a4 agent").await;
    assert_eq!(hunks(&session.app, "a.txt"), [(3, 4)]);
    // the baseline is the commit the watch started on: committing the change keeps it listed
    commit_all("Agent work");
    session.run_for(Duration::from_millis(800)).await;
    assert_eq!(hunks(&session.app, "a.txt"), [(3, 4)]);
    assert!(session.watching());
}

// History and bookmarks

/// The places the focused view's history holds.
fn history_len(app: &Application) -> usize {
    app.editor.tree.get(app.editor.tree.focus).history.len()
}

#[tokio::test(flavor = "multi_thread")]
async fn following_and_n_record_no_history_jumps() {
    let Some(_) = enter("following_and_n_record_no_history_jumps") else {
        return;
    };
    let mut session = Session::new();
    // following goes through three files: none of it is a jump
    session.three_changes().await;
    assert_eq!(history_len(&session.app), 0);

    // nor are n and N through the session's changes, across files
    session.keys("NN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    session.keys("nN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert_eq!(history_len(&session.app), 0);
    assert!(session.watching());

    // ga is held while watching, and after Esc it has nowhere to go back to
    held(&mut session, "ga").await;
    session.keys("<esc>").await;
    assert!(!session.watching());
    assert_eq!(focus(&session.app), at("a.txt", 1));

    // n after leaving still goes through the session's changes, without recording either
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert_eq!(history_len(&session.app), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn setting_a_bookmark_neither_looks_nor_ends_watch_mode() {
    let Some(_) = enter("setting_a_bookmark_neither_looks_nor_ends_watch_mode") else {
        return;
    };
    let mut session = Session::new();
    session.three_changes().await;
    session.keys("<C-s>").await;
    assert!(session.watching());
    assert!(session.following());
    assert_eq!(session.app.editor.bookmarks().len(), 1);
    // and removing it
    session.keys("<C-s>").await;
    assert!(session.following());
    assert!(session.app.editor.bookmarks().is_empty());

    // following goes on
    session.change_and_follow("a.txt", 6, "a6 agent").await;
    assert!(session.following());
}

#[tokio::test(flavor = "multi_thread")]
async fn ga_is_held_until_esc_leaves_watch_mode() {
    let Some(_) = enter("ga_is_held_until_esc_leaves_watch_mode") else {
        return;
    };
    let session = held_then_left("ga").await;
    changes_stay_after_leaving(session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn going_round_the_bookmarks_is_held_until_esc_leaves_watch_mode() {
    let Some(_) = enter("going_round_the_bookmarks_is_held_until_esc_leaves_watch_mode") else {
        return;
    };
    let session = held_then_left("<C-i>").await;
    changes_stay_after_leaving(session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bookmark_picker_is_held_until_esc_leaves_watch_mode() {
    let Some(_) = enter("the_bookmark_picker_is_held_until_esc_leaves_watch_mode") else {
        return;
    };
    let session = held_then_left("<space>j").await;
    changes_stay_after_leaving(session).await;
}

// 20. A coding agent after Esc
//
// An agent doesn't `std::fs::write` a file in place: it writes a temporary file next to it and
// renames it over the target, often several files at once, often for seconds. These tests
// reproduce that after `Esc` (watch mode left, the session live) and check what the session
// recorded and what `:watch` shows on the way back.

/// Makes the names of the temporary files different.
static AGENT_TMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// An atomic write: the whole new text goes to a temporary file in the same directory, which is
/// renamed over the target. Creates the parent directories.
fn agent_write(name: &str, text: &str) {
    let path = cwd().join(name);
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let n = AGENT_TMP.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = dir.join(format!(
        ".{}.tmp.{n}",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
}

/// Like [`edit`] (line `number` from 1 becomes `text`), written with [`agent_write`].
fn agent_edit(name: &str, number: usize, text: &str) {
    let old = std::fs::read_to_string(cwd().join(name)).unwrap();
    let new: String = old
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            if i + 1 == number {
                format!("{text}\n")
            } else {
                line.to_string()
            }
        })
        .collect();
    agent_write(name, &new);
}

/// Four files written one after the other with `gap` between them (none for a burst): the open
/// `a.txt`, `other.txt` which was never opened, `b.txt`, and the new file `src/new/notes.txt` in a
/// new directory. In this order.
fn agent_burst(gap: Duration) {
    let pause = || {
        if !gap.is_zero() {
            std::thread::sleep(gap);
        }
    };
    agent_edit("a.txt", 3, "a3 agent");
    pause();
    agent_edit("other.txt", 4, "o4 agent");
    pause();
    agent_edit("b.txt", 5, "b5 agent");
    pause();
    agent_write("src/new/notes.txt", &lines("n", 3));
}

/// The write number `k` of a stream: it replaces one of the lines 1, 3, 5, 7, 9 of `b.txt`,
/// `c.txt` or `other.txt` in turn, each time with a new text. After 15 writes every file has five
/// hunks, which later writes only change.
fn agent_stream_write(k: usize) {
    let name = ["b.txt", "c.txt", "other.txt"][k % 3];
    agent_edit(name, 2 * ((k / 3) % 5) + 1, &format!("{name} stream {k}"));
}

/// The hunks of the stream after at least 15 writes: lines (from 0) 0, 2, 4, 6 and 8 of its files.
fn stream_hunks() -> Vec<(String, usize)> {
    let mut all = Vec::new();
    for name in ["b.txt", "c.txt", "other.txt"] {
        for line in [0, 2, 4, 6, 8] {
            all.push((name.to_string(), line));
        }
    }
    all
}

/// The changes of the session in the order `n` goes through them: file name and first line.
fn order_of(app: &Application) -> Vec<(String, usize)> {
    session_of(app)
        .hunks_in_order()
        .iter()
        .map(|target| {
            (
                target
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                target.lines.start as usize,
            )
        })
        .collect()
}

impl Session {
    /// Starts watch mode and leaves it again: the session is live and empty.
    async fn start_and_leave(&mut self) {
        self.start_watch().await;
        self.keys("<esc>").await;
        assert!(!self.watching());
        assert!(self.app.editor.watch.live().is_some());
        assert!(session_of(&self.app).is_empty());
    }

    /// Presses `n` `count` times and returns where each press went.
    async fn walk_n(&mut self, count: usize) -> Vec<(String, usize)> {
        let mut visited = Vec::new();
        for _ in 0..count {
            self.keys("n").await;
            visited.push(focus(&self.app));
        }
        visited
    }

    /// Waits for the burst of [`agent_burst`] to be recorded, a moment more for nothing else to
    /// come, and checks its four files.
    async fn check_burst_recorded(&mut self) {
        self.wait_watch("the four files of the burst", |app| {
            session_of(app).hunks_in_order().len() == 4
        })
        .await;
        self.run_for(Duration::from_millis(600)).await;
        assert_eq!(hunks(&self.app, "a.txt"), [(2, 3)]);
        assert_eq!(hunks(&self.app, "other.txt"), [(3, 4)]);
        assert_eq!(hunks(&self.app, "b.txt"), [(4, 5)]);
        assert_eq!(hunks(&self.app, "src/new/notes.txt"), [(0, 3)]);
        assert_eq!(
            file(&self.app, "src/new/notes.txt").unwrap().kind,
            FileKind::Created
        );
        assert_eq!(session_of(&self.app).hunks_in_order().len(), 4);
        assert_eq!(
            order_of(&self.app),
            [
                ("a.txt".to_string(), 2),
                ("other.txt".to_string(), 3),
                ("b.txt".to_string(), 4),
                ("notes.txt".to_string(), 0),
            ],
            "the hunks are not in the order the writes happened"
        );
    }

    /// `:watch` after the burst, with a catch-up that stays on the first change: it shows the
    /// oldest of the four, the other three are new, and `n` reaches them in order.
    async fn check_burst_on_return(&mut self) {
        self.keys(":watch<ret>").await;
        assert!(self.watching());
        assert_eq!(self.new_count(), 3, "activity: {:?}", self.activity());
        assert_eq!(self.activity(), "followed a.txt:3");
        self.wait_for_screen_text("catching up 1/4").await;
        self.wait_for_screen_text("3 new").await;
        let mut visited = vec![focus(&self.app)];
        visited.extend(self.walk_n(3).await);
        assert_eq!(
            visited,
            [
                at("a.txt", 2),
                at("other.txt", 3),
                at("b.txt", 4),
                at("notes.txt", 0)
            ]
        );
        assert_eq!(self.new_count(), 0);
        assert!(self.following());
    }
}

/// Atomic writes (temporary file, then rename) to files the editor never opened, 300 ms apart,
/// after `Esc`: each one is recorded with its hunk, and on return `n` reaches them in order.
#[tokio::test(flavor = "multi_thread")]
async fn agent_atomic_writes_one_at_a_time_after_esc_are_recorded_and_counted_on_return() {
    let Some(_) =
        enter("agent_atomic_writes_one_at_a_time_after_esc_are_recorded_and_counted_on_return")
    else {
        return;
    };
    let mut session = paced(60_000);
    session.start_and_leave().await;
    let before = focus(&session.app);

    for (i, (name, number)) in [("b.txt", 3), ("c.txt", 5), ("other.txt", 7)]
        .into_iter()
        .enumerate()
    {
        agent_edit(name, number, &format!("{name} agent"));
        session
            .wait_watch(&format!("{name} in the session"), |app| {
                file(app, name).is_some()
            })
            .await;
        assert_eq!(
            hunks(&session.app, name),
            [(number as u32 - 1, number as u32)]
        );
        assert_eq!(session_of(&session.app).hunks_in_order().len(), i + 1);
        session.run_for(Duration::from_millis(300)).await;
    }
    assert_eq!(focus(&session.app), before, "something was followed");
    session.assert_quiet("after the writes");

    // the catch-up (60 s per change: it stays on the first) shows the oldest change made while
    // left; the other two stay new, and `n` takes over
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert_eq!(focus(&session.app), at("b.txt", 2));
    assert_eq!(session.new_count(), 2, "activity: {:?}", session.activity());
    assert_eq!(session.activity(), "followed b.txt:3");
    session.wait_for_screen_text("catching up 1/3").await;
    session.wait_for_screen_text("2 new").await;
    let visited = session.walk_n(2).await;
    assert_eq!(visited, [at("c.txt", 4), at("other.txt", 6)]);
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
}

/// One burst without any sleep over four files (one open, one never opened, one new in a new
/// directory) after `Esc`: all four are recorded, in the order of the writes, and counted on
/// return.
#[tokio::test(flavor = "multi_thread")]
async fn agent_burst_over_four_files_after_esc_is_recorded_and_counted_on_return() {
    let Some(_) = enter("agent_burst_over_four_files_after_esc_is_recorded_and_counted_on_return")
    else {
        return;
    };
    let mut session = paced(60_000);
    session.keys(":open a.txt<ret>").await;
    session.start_and_leave().await;
    let before = focus(&session.app);

    agent_burst(Duration::ZERO);
    session.check_burst_recorded().await;
    // the open file was reloaded
    assert_eq!(focus(&session.app), before, "something was followed");
    let doc = session
        .app
        .editor
        .document_by_path(cwd().join("a.txt"))
        .expect("a.txt is open");
    assert_eq!(doc.text().line(2).to_string(), "a3 agent\n");
    assert_eq!(doc.watch_diff().expect("a watch gutter").load().len(), 1);
    session.assert_quiet("after the burst");

    session.check_burst_on_return().await;
}

/// The same four files, written 30 ms apart (one long stream for the 200 ms debounce), after
/// `Esc`: the same result as the burst.
#[tokio::test(flavor = "multi_thread")]
async fn agent_burst_with_30_ms_gaps_after_esc_is_recorded_and_counted_on_return() {
    let Some(_) = enter("agent_burst_with_30_ms_gaps_after_esc_is_recorded_and_counted_on_return")
    else {
        return;
    };
    let mut session = paced(60_000);
    session.keys(":open a.txt<ret>").await;
    session.start_and_leave().await;
    let before = focus(&session.app);

    agent_burst(Duration::from_millis(30));
    session.check_burst_recorded().await;
    assert_eq!(focus(&session.app), before, "something was followed");
    session.check_burst_on_return().await;
}

/// A stream of writes every 30 ms for about two seconds, to three files, after `Esc`: the batches
/// are cut by the one second maximum wait, so the session has changes before the stream ends;
/// at the end it has every hunk, and on return all are new and `n` walks through them.
#[tokio::test(flavor = "multi_thread")]
async fn agent_long_stream_after_esc_is_recorded_in_batches_and_counted_on_return() {
    let Some(_) = enter("agent_long_stream_after_esc_is_recorded_in_batches_and_counted_on_return")
    else {
        return;
    };
    let mut session = paced(60_000);
    session.start_and_leave().await;
    let before = focus(&session.app);

    for k in 0..66 {
        agent_stream_write(k);
        session.run_for(Duration::from_millis(30)).await;
        if k == 55 {
            // about 1.7 s in: the maximum wait has passed at least once
            assert!(
                !session_of(&session.app).hunks_in_order().is_empty(),
                "nothing recorded 1.7 s into a stream"
            );
        }
    }
    session
        .wait_watch("the 15 hunks of the stream", |app| {
            session_of(app).hunks_in_order().len() == 15
        })
        .await;
    session.run_for(Duration::from_millis(600)).await;
    for name in ["b.txt", "c.txt", "other.txt"] {
        assert_eq!(
            hunks(&session.app, name),
            [(0, 1), (2, 3), (4, 5), (6, 7), (8, 9)],
            "{name}"
        );
    }
    assert_eq!(focus(&session.app), before, "something was followed");
    let order = order_of(&session.app);

    // the catch-up (60 s per change) shows the oldest and stops: the other 14 are new
    session.keys(":watch<ret>").await;
    assert_eq!(
        session.new_count(),
        14,
        "activity: {:?}",
        session.activity()
    );
    assert!(session.activity().starts_with("followed "));
    let mut visited = vec![focus(&session.app)];
    visited.extend(session.walk_n(14).await);
    assert_eq!(
        visited, order,
        "the catch-up and n don't go through the changes in their order"
    );
    let mut sorted = visited.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, {
        let mut all = stream_hunks();
        all.sort();
        all
    });
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
}

/// The same file rewritten five times in a row (atomic writes, no sleep), each time with one more
/// inserted line far from the others, after `Esc`: the end state has the five hunks, and on
/// return all five are new and reachable with `n`.
#[tokio::test(flavor = "multi_thread")]
async fn agent_same_file_rewritten_five_times_after_esc_has_the_hunks_of_all() {
    let Some(_) = enter("agent_same_file_rewritten_five_times_after_esc_has_the_hunks_of_all")
    else {
        return;
    };
    let mut session = paced(60_000);
    session.start_and_leave().await;
    let before = focus(&session.app);

    for k in 1..=5 {
        let mut text = String::new();
        for i in 1..=10 {
            text.push_str(&format!("b{i}\n"));
            if i % 2 == 1 && i / 2 < k {
                text.push_str(&format!("inserted {i}\n"));
            }
        }
        agent_write("b.txt", &text);
    }
    session
        .wait_watch("b.txt in the session", |app| file(app, "b.txt").is_some())
        .await;
    session.run_for(Duration::from_secs(1)).await;
    assert_eq!(
        hunks(&session.app, "b.txt"),
        [(1, 2), (4, 5), (7, 8), (10, 11), (13, 14)]
    );
    assert_eq!(focus(&session.app), before, "something was followed");

    session.keys(":watch<ret>").await;
    assert_eq!(session.new_count(), 4, "activity: {:?}", session.activity());
    let mut visited = vec![focus(&session.app)];
    visited.extend(session.walk_n(4).await);
    visited.sort();
    visited.dedup();
    assert_eq!(
        visited,
        [
            at("b.txt", 1),
            at("b.txt", 4),
            at("b.txt", 7),
            at("b.txt", 10),
            at("b.txt", 13)
        ]
    );
    assert_eq!(session.new_count(), 0);
}

/// Control: the burst of four files while watch mode is on and following: all four are recorded,
/// following ends on the newest file, and nothing is new.
#[tokio::test(flavor = "multi_thread")]
async fn agent_burst_over_four_files_while_watching_is_recorded_and_followed() {
    let Some(_) = enter("agent_burst_over_four_files_while_watching_is_recorded_and_followed")
    else {
        return;
    };
    let mut session = Session::new();
    session.keys(":open a.txt<ret>").await;
    session.start_watch().await;

    agent_burst(Duration::ZERO);
    session.check_burst_recorded().await;
    assert!(session.following());
    assert_eq!(session.new_count(), 0);
    assert_eq!(focus(&session.app), at("notes.txt", 0));

    session.keys("<esc>").await;
    session.keys(":watch<ret>").await;
    assert_eq!(session.new_count(), 0);
}

/// `:watch` in the middle of a stream that goes on, with a catch-up of 0.1 s per change: every
/// change recorded while left is shown by the catch-up, oldest first, before the newer ones
/// (those recorded during it are queued behind), and none is skipped. This is the regression
/// test of the first catch-up bug: the next batch that followed marked the changes made while
/// left as seen, so they were never shown (`after the stream: 0 new, but 15 were recorded
/// while left`).
#[tokio::test(flavor = "multi_thread")]
async fn agent_watch_in_the_middle_of_a_stream_catches_up_on_every_change_made_while_left() {
    let Some(_) =
        enter("agent_watch_in_the_middle_of_a_stream_catches_up_on_every_change_made_while_left")
    else {
        return;
    };
    let mut session = paced(100);
    session.start_and_leave().await;

    // the `at` of the session (the number of the change shown last) each time it changed during
    // the catch-up
    let mut visits: Vec<u64> = Vec::new();
    let track = |session: &Session, visits: &mut Vec<u64>| {
        let session = session_of(&session.app);
        let now = session.at.as_ref().map(|(_, seq)| *seq);
        if let (Some(now), true) = (now, session.catch_up.is_some()) {
            if visits.last() != Some(&now) {
                visits.push(now);
            }
        }
    };
    let mut meanwhile = Vec::new();
    for k in 0..66 {
        agent_stream_write(k);
        session.frames(Duration::from_millis(30)).await;
        track(&session, &mut visits);
        if k == 40 {
            // about 1.3 s in
            meanwhile = order_of(&session.app);
            assert!(
                !meanwhile.is_empty(),
                "nothing recorded 1.3 s into a stream"
            );
            session.press(":watch<ret>");
            session.frames(Duration::from_millis(30)).await;
            assert!(session.watching());
            assert!(session_of(&session.app).catch_up.is_some());
            // the first change shown is the oldest one recorded while left
            assert_eq!(focus(&session.app), meanwhile[0]);
            track(&session, &mut visits);
        }
    }
    // the catch-up goes on until nothing is left to show
    let deadline = Instant::now() + TIMEOUT;
    while session_of(&session.app).catch_up.is_some() {
        assert!(
            Instant::now() < deadline,
            "the catch-up never ended: {visits:?}, activity {:?}",
            session.activity()
        );
        session.frames(Duration::from_millis(30)).await;
        track(&session, &mut visits);
    }
    session.frames(Duration::from_secs(1)).await;
    assert_eq!(session.new_count(), 0, "visits {visits:?}");
    assert!(session.following());

    // shown in order, and none skipped: every change that is still there and older than the last
    // one the catch-up showed was shown by it
    assert!(
        visits.windows(2).all(|pair| pair[0] < pair[1]),
        "not in the order the changes happened: {visits:?}"
    );
    let last = *visits.last().expect("the catch-up showed something");
    assert!(
        visits.len() >= meanwhile.len().min(3),
        "{visits:?} for {meanwhile:?}"
    );
    for change in session_of(&session.app).hunks_in_order() {
        assert!(
            change.seq > last || visits.contains(&change.seq),
            "{:?} (change {}) was skipped by the catch-up: {visits:?}",
            change.path,
            change.seq
        );
    }
}

// 21. What the new parts of watch mode do

/// A session whose catch-up shows a change every `millis` milliseconds (`watch-catch-up`).
fn paced(millis: u64) -> Session {
    let mut config = test_config();
    config.editor.watch_catch_up = CatchUpPace { millis };
    Session::with_config(config)
}

fn theme_of(toml: &str) -> vortex_view::Theme {
    vortex_view::Theme::from(toml::from_str::<toml::Value>(toml).unwrap())
}

fn channels(color: Color) -> [i32; 3] {
    match color {
        Color::Rgb(r, g, b) => [r as i32, g as i32, b as i32],
        other => panic!("{other:?} is not an RGB color"),
    }
}

/// How far two colors are from one another: the sum of the differences of their channels.
fn gap(a: Color, b: Color) -> i32 {
    let (a, b) = (channels(a), channels(b));
    (0..3).map(|i| (a[i] - b[i]).abs()).sum()
}

/// Whether each channel of `a` is within `tolerance` of the one of `b`.
fn within(a: Color, b: Color, tolerance: i32) -> bool {
    let (a, b) = (channels(a), channels(b));
    (0..3).all(|i| (a[i] - b[i]).abs() <= tolerance)
}

/// The clipboard register as the one value yanking places writes.
fn clipboard(app: &Application) -> Option<String> {
    let values: Vec<String> = app
        .editor
        .registers
        .read('+', &app.editor)?
        .map(|value| value.into_owned())
        .collect();
    assert!(values.len() <= 1, "{values:?}");
    values.into_iter().next()
}

/// What `git diff --cached` shows: the index against HEAD.
fn cached() -> String {
    git(&cwd(), "", &["diff", "--cached"])
}

/// What `git diff` shows: the working tree against the index.
fn unstaged() -> String {
    git(&cwd(), "", &["diff"])
}

fn read(name: &str) -> String {
    std::fs::read_to_string(cwd().join(name)).unwrap()
}

/// `a` with the lines of `from` replaced: `(number from 1, text)`.
fn with_lines(name: &str, changes: &[(usize, &str)]) -> String {
    let old = read(name);
    old.split_inclusive('\n')
        .enumerate()
        .map(
            |(i, line)| match changes.iter().find(|(n, _)| *n == i + 1) {
                Some((_, text)) => format!("{text}\n"),
                None => line.to_string(),
            },
        )
        .collect()
}

/// The statusline's watch mark in what is on the screen: `None` without one, `Some(None)` for
/// a bare `●`, `Some(Some(3))` for `● 3`. It is read from the row of the view's statusline (the
/// one with the mode label `NOR`): the mode bar has a `●` of its own.
fn mark_of(app: &Application) -> Option<Option<usize>> {
    let rows = super::git_log::screen_rows(app);
    let row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("NOR"))?;
    let at = row.find('●')?;
    let rest = &row[at + '●'.len_utf8()..];
    let mut chars = rest.chars();
    Some(match (chars.next(), chars.next()) {
        (Some(' '), Some(digit)) if digit.is_ascii_digit() => Some(
            rest.trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap(),
        ),
        _ => None,
    })
}

impl Session {
    /// Runs the editor for `time`, drawing frames, and returns when it is up: `keys` and
    /// `run_for` only return once nothing asks for a redraw any more, which a flash (or a
    /// catch-up showing a change every second or less) does for a second after each change, so
    /// they can't watch what happens meanwhile.
    async fn frames(&mut self, time: Duration) {
        let end = Instant::now() + time;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            // (it returns early when the editor is idle: run on until the time is up)
            self.app.editor.reset_idle_timer();
            let _ =
                tokio::time::timeout(left, self.app.event_loop_until_idle(&mut self.input)).await;
        }
    }

    /// Types `keys` without running the editor: the next `frames` handles them.
    fn press(&mut self, keys: &str) {
        for key in parse_macro(keys).unwrap() {
            self.tx.send(Ok(Event::Key(KeyEvent::from(key)))).unwrap();
        }
    }

    /// Runs frames until `condition` holds, checking every 10 ms.
    async fn wait_frames(&mut self, what: &str, condition: impl Fn(&Application) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !condition(&self.app) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\nfocus: {:?}",
                focus(&self.app)
            );
            self.frames(Duration::from_millis(10)).await;
        }
    }

    /// Like [`Session::wait_for_focus`], but looking every 10 ms, so that the first frames of a
    /// flash are still to come.
    async fn wait_for_focus_at_once(&mut self, name: &str, line: usize) {
        let expected = at(name, line);
        self.wait_frames(&format!("the focus on {expected:?}"), |app| {
            focus(app) == expected
        })
        .await;
    }

    /// Presses the mouse in the cell (column, row).
    async fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        let event = MouseEvent {
            kind,
            column,
            row,
            modifiers: Modifiers::NONE,
        };
        self.tx.send(Ok(Event::Mouse(event))).unwrap();
        assert!(self.app.event_loop_until_idle(&mut self.input).await);
    }

    /// The first position of the view of the focused buffer.
    fn offset(&self) -> vortex_view::view::ViewPosition {
        let (view, doc) = current_ref!(self.app.editor);
        doc.view_offset(view.id)
    }

    /// The statusline's mark of a left session, see [`mark_of`].
    fn mark(&self) -> Option<Option<usize>> {
        mark_of(&self.app)
    }

    /// The colors of the cell (column, row) of the screen: foreground and background.
    fn cell(&self, column: u16, row: u16) -> (Color, Color) {
        let cell = self.app.screen().get(column, row).expect("a cell");
        (cell.fg, cell.bg)
    }

    /// Leaves watch mode with a live session and three changes made while left, in three files,
    /// each one in the session before the next: a line 2, b line 5, c line 8.
    async fn left_with_three_changes(&mut self) {
        self.start_and_leave().await;
        for (name, number, text) in [
            ("a.txt", 2, "a2 agent"),
            ("b.txt", 5, "b5 agent"),
            ("c.txt", 8, "c8 agent"),
        ] {
            edit(name, number, text);
            self.wait_watch(&format!("{name} in the session"), |app| {
                file(app, name).is_some()
            })
            .await;
        }
        assert_eq!(session_of(&self.app).hunks_in_order().len(), 3);
    }

    /// Runs frames until `done`, noting each place the focus was at, starting with the current
    /// one.
    async fn focus_until(
        &mut self,
        what: &str,
        done: impl Fn(&Application) -> bool,
    ) -> Vec<(String, usize)> {
        let deadline = Instant::now() + TIMEOUT;
        let mut seen = vec![focus(&self.app)];
        while !done(&self.app) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}, focus went {seen:?}, activity {:?}",
                self.activity()
            );
            self.frames(Duration::from_millis(20)).await;
            let now = focus(&self.app);
            if seen.last() != Some(&now) {
                seen.push(now);
            }
        }
        seen
    }
}

// 21a. The flash fades toward the background under it

/// The cell sampled for the flash: well to the right of the text, on its row.
const FLASH_COLUMN: u16 = 60;

/// The background of the flashed line while it fades, `(time since the flash started, color)`,
/// sampled every 15 ms until it has been over for a while. `text` is on the flashed line. The
/// samples start with the first frame that has the flash.
async fn flash_samples(session: &mut Session, text: &str, over: Color) -> Vec<(Duration, Color)> {
    let start = session_of(&session.app)
        .flash
        .as_ref()
        .expect("a flash")
        .start;
    let (_, row) = session.cell_of(text);
    let mut samples: Vec<(Duration, Color)> = Vec::new();
    loop {
        let elapsed = start.elapsed();
        let (_, bg) = session.cell(FLASH_COLUMN, row);
        if !samples.is_empty() || bg != over {
            samples.push((elapsed, bg));
        }
        // the flash lasts 900 ms: a good while after it, nothing is left of it
        if elapsed > Duration::from_millis(1300) {
            return samples;
        }
        session.frames(Duration::from_millis(15)).await;
    }
}

/// The flash starts as its own color, moves steadily toward `to` (the background drawn under
/// it), ends within a few units of it with no frame in between that is anywhere else, and then
/// the cell is exactly `to`. `elsewhere` is a background it must never be closer to than to
/// `to` in the second half.
fn check_fade(samples: &[(Duration, Color)], from: Color, to: Color, elsewhere: Color) {
    let shown = |samples: &[(Duration, Color)]| {
        samples
            .iter()
            .map(|(elapsed, color)| format!("{}ms {:?}", elapsed.as_millis(), channels(*color)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(samples.len() > 10, "{}", shown(samples));
    let (first_at, first) = samples[0];
    assert!(
        first_at < Duration::from_millis(300),
        "the flash was not seen at once: {}",
        shown(samples)
    );
    assert!(
        gap(first, from) < gap(first, to),
        "the flash doesn't start from its color: {}",
        shown(samples)
    );
    for pair in samples.windows(2) {
        assert!(
            gap(pair[1].1, to) <= gap(pair[0].1, to),
            "the flash doesn't fade steadily toward the background: {}",
            shown(samples)
        );
    }
    for (elapsed, color) in samples {
        if *elapsed >= Duration::from_millis(450) {
            assert!(
                gap(*color, to) <= gap(*color, elsewhere),
                "{}ms: closer to the other background: {}",
                elapsed.as_millis(),
                shown(samples)
            );
        }
    }
    // the last frame that still has the flash is next to the background it ends on
    let flashed: Vec<_> = samples.iter().filter(|(_, color)| *color != to).collect();
    let (last_at, last) = flashed.last().expect("some frames with the flash");
    assert!(
        within(*last, to, 14),
        "the last frame of the flash, at {}ms, is not close to the background under it: {}",
        last_at.as_millis(),
        shown(samples)
    );
    // then it is the background, exactly, and stays
    let end = samples
        .iter()
        .rposition(|(_, color)| *color != to)
        .expect("a flashed frame")
        + 1;
    assert!(
        end < samples.len(),
        "the flash never ended: {}",
        shown(samples)
    );
    assert!(
        samples[end..].iter().all(|(_, color)| *color == to),
        "{}",
        shown(samples)
    );
}

/// The flash of a hunk followed while watch mode is on fades from `ui.highlight.watch` to
/// `ui.background.mode`, the background drawn under it on a mode's screen, not to
/// `ui.background`: its last frames don't flicker to the other.
#[tokio::test(flavor = "multi_thread")]
async fn the_flash_fades_toward_the_mode_background_while_watch_mode_is_on() {
    let Some(_) = enter("the_flash_fades_toward_the_mode_background_while_watch_mode_is_on") else {
        return;
    };
    let mut session = Session::new();
    session.app.editor.theme = theme_of(
        r##"
"ui.background" = { bg = "#ffffff" }
"ui.background.mode" = { bg = "#003060" }
"ui.highlight.watch" = { bg = "#ff8000" }
"ui.selection" = { bg = "#333333" }
"##,
    );
    session.start_watch().await;
    edit("a.txt", 4, "a4 agent");
    session.wait_for_focus_at_once("a.txt", 3).await;
    let samples = flash_samples(&mut session, "a4 agent", Color::Rgb(0, 0x30, 0x60)).await;
    check_fade(
        &samples,
        Color::Rgb(0xff, 0x80, 0),
        Color::Rgb(0, 0x30, 0x60),
        Color::Rgb(0xff, 0xff, 0xff),
    );
}

/// Without `ui.background.mode` the mode's screen is the popups' background (the fallback of
/// `modes::screen_style`): the flash fades toward `ui.popup`'s.
#[tokio::test(flavor = "multi_thread")]
async fn the_flash_fades_toward_the_popup_background_without_a_mode_background() {
    let Some(_) = enter("the_flash_fades_toward_the_popup_background_without_a_mode_background")
    else {
        return;
    };
    let mut session = Session::new();
    session.app.editor.theme = theme_of(
        r##"
"ui.background" = { bg = "#ffffff" }
"ui.popup" = { bg = "#204020" }
"ui.highlight.watch" = { bg = "#ff8000" }
"ui.selection" = { bg = "#333333" }
"##,
    );
    session.start_watch().await;
    edit("a.txt", 4, "a4 agent");
    session.wait_for_focus_at_once("a.txt", 3).await;
    let samples = flash_samples(&mut session, "a4 agent", Color::Rgb(0x20, 0x40, 0x20)).await;
    check_fade(
        &samples,
        Color::Rgb(0xff, 0x80, 0),
        Color::Rgb(0x20, 0x40, 0x20),
        Color::Rgb(0xff, 0xff, 0xff),
    );
}

/// With watch mode left, `N` goes to an older change and flashes it, fading toward
/// `ui.background` (no mode's screen is under it), not toward `ui.background.mode`.
#[tokio::test(flavor = "multi_thread")]
async fn the_flash_of_n_after_esc_fades_toward_the_buffer_background() {
    let Some(_) = enter("the_flash_of_n_after_esc_fades_toward_the_buffer_background") else {
        return;
    };
    let mut session = Session::new();
    session.app.editor.theme = theme_of(
        r##"
"ui.background" = { bg = "#ffffff" }
"ui.background.mode" = { bg = "#003060" }
"ui.highlight.watch" = { bg = "#ff8000" }
"ui.selection" = { bg = "#333333" }
"##,
    );
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.change_and_follow("b.txt", 5, "b5 agent").await;
    session.keys("<esc>").await;
    assert!(!session.watching());
    session.press("N");
    session.wait_for_focus_at_once("a.txt", 1).await;
    let samples = flash_samples(&mut session, "a2 agent", Color::Rgb(0xff, 0xff, 0xff)).await;
    check_fade(
        &samples,
        Color::Rgb(0xff, 0x80, 0),
        Color::Rgb(0xff, 0xff, 0xff),
        Color::Rgb(0, 0x30, 0x60),
    );
}

// 21b. The mouse

/// The state the mouse must leave alone while following: the cursor, the view's position, and
/// following and watch mode themselves.
fn held_state(session: &Session) -> impl PartialEq + std::fmt::Debug {
    (
        session.selection(),
        session.offset(),
        session.following(),
        session.watching(),
        focus(&session.app),
    )
}

/// A long file, committed before the session starts: the views have a lot to scroll.
fn commit_long_file() {
    write("long.txt", &lines("long", 2000));
    git(&cwd(), "", &["add", "long.txt"]);
    git(&cwd(), "", &["commit", "-q", "-m", "A long file"]);
}

/// While following, the mouse in the text does nothing: a click, a drag, the wheel in both
/// directions leave the cursor, the view's position, following and watch mode as they were,
/// and the bar says that `v` looks around.
#[tokio::test(flavor = "multi_thread")]
async fn the_mouse_does_nothing_while_following() {
    let Some(_) = enter("the_mouse_does_nothing_while_following") else {
        return;
    };
    commit_long_file();
    let mut session = Session::new();
    session.start_watch().await;
    session
        .change_and_follow("long.txt", 1000, "long1000 agent")
        .await;
    assert!(session.following());
    let before = held_state(&session);
    let (column, row) = session.cell_of("long1002");

    for (what, kind) in [
        ("a click", MouseEventKind::Down(MouseButton::Left)),
        ("a drag", MouseEventKind::Drag(MouseButton::Left)),
        ("the wheel down", MouseEventKind::ScrollDown),
        ("the wheel up", MouseEventKind::ScrollUp),
        ("a right click", MouseEventKind::Down(MouseButton::Right)),
    ] {
        session.app.editor.watch.session.as_mut().unwrap().activity = None;
        session.mouse(kind, column + 2, row).await;
        assert_eq!(held_state(&session), before, "{what} changed something");
        assert_eq!(session.activity(), LOOK_TO_MOVE, "{what}");
    }
    // the release of the held press does nothing either
    session
        .mouse(MouseEventKind::Up(MouseButton::Left), column + 2, row)
        .await;
    assert_eq!(held_state(&session), before);
    assert!(session.on_screen(LOOK_TO_MOVE));
    assert!(session.on_screen("WATCH ● following"));
    session.assert_quiet("after the mouse");
    // still following
    session
        .change_and_follow("long.txt", 500, "long500 agent")
        .await;
    assert!(session.following());
}

/// While looking around (`v`), the mouse works as usual: a click moves the cursor, the wheel
/// scrolls the view, and following stays off.
#[tokio::test(flavor = "multi_thread")]
async fn the_mouse_works_while_looking_around() {
    let Some(_) = enter("the_mouse_works_while_looking_around") else {
        return;
    };
    commit_long_file();
    let mut session = Session::new();
    session.start_watch().await;
    session
        .change_and_follow("long.txt", 1000, "long1000 agent")
        .await;
    session.keys("v").await;
    assert!(!session.following());
    let (cursor, offset) = (session.selection(), session.offset());
    let (column, row) = session.cell_of("long1003");

    session
        .mouse(MouseEventKind::Down(MouseButton::Left), column + 2, row)
        .await;
    assert_ne!(
        session.selection(),
        cursor,
        "the click didn't move the cursor"
    );
    assert_eq!(focus(&session.app), at("long.txt", 1002));
    assert_eq!(session.offset(), offset, "the click scrolled");

    session
        .mouse(MouseEventKind::ScrollDown, column + 2, row)
        .await;
    let down = session.offset();
    assert_ne!(down, offset, "the wheel down didn't scroll");
    session
        .mouse(MouseEventKind::ScrollUp, column + 2, row)
        .await;
    session
        .mouse(MouseEventKind::ScrollUp, column + 2, row)
        .await;
    assert_ne!(session.offset(), down, "the wheel up didn't scroll");
    assert!(!session.following());
    assert!(session.watching());
    assert_ne!(session.activity(), LOOK_TO_MOVE);
}

// 21c. The statusline's mark of a left session

/// The mark is `●` right after leaving, counts the changes made since (`● 3`), is gone while
/// watch mode is on and after `:watch-stop`, and a session never started has none.
#[tokio::test(flavor = "multi_thread")]
async fn the_statusline_marks_a_left_session_with_the_count_of_changes_made_since() {
    let Some(_) = enter("the_statusline_marks_a_left_session_with_the_count_of_changes_made_since")
    else {
        return;
    };
    // a catch-up of nothing: `:watch` goes straight to the newest change
    let mut session = paced(0);
    session.run_for(Duration::from_millis(100)).await;
    assert_eq!(session.mark(), None, "a mark without a session");

    session.start_watch().await;
    assert_eq!(session.mark(), None, "a mark while watch mode is on");
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    assert_eq!(session.mark(), None);

    // left: the changes so far were seen, so no count
    session.keys("<esc>").await;
    assert_eq!(session.mark(), Some(None));
    // the changes made while left are counted as they come
    for (i, (name, number)) in [("b.txt", 5), ("c.txt", 8), ("other.txt", 3)]
        .into_iter()
        .enumerate()
    {
        edit(name, number, &format!("{name} agent"));
        session
            .wait_watch(&format!("the mark to count {}", i + 1), |app| {
                mark_of(app) == Some(Some(i + 1))
            })
            .await;
    }
    // back: watch mode shows it on the bar, the statusline has no mark
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert_eq!(session.mark(), None);
    assert!(session.on_screen("WATCH ● following"));
    // left again: everything was seen
    session.keys("<esc>").await;
    assert_eq!(session.mark(), Some(None));

    session.keys(":watch-stop<ret>").await;
    assert!(session.app.editor.watch.live().is_none());
    assert_eq!(session.mark(), None, "a mark after :watch-stop");
}

/// The mark is lit (`ui.statusline.watch.lit`) for a second after a change arrives while left,
/// then plain (`ui.statusline.watch`), and lit again four seconds after it: the editor redraws
/// by itself at each turn, with nothing pressed.
#[tokio::test(flavor = "multi_thread")]
async fn the_mark_blinks_after_a_change_made_while_left() {
    let Some(_) = enter("the_mark_blinks_after_a_change_made_while_left") else {
        return;
    };
    let mut session = Session::new();
    session.app.editor.theme = theme_of(
        r##"
"ui.statusline.watch" = { fg = "#00ffff" }
"ui.statusline.watch.lit" = { fg = "#ff00ff" }
"ui.selection" = { bg = "#333333" }
"##,
    );
    const PLAIN: Color = Color::Rgb(0, 0xff, 0xff);
    const LIT: Color = Color::Rgb(0xff, 0, 0xff);
    session.start_and_leave().await;
    let (column, row) = session.cell_of("●");
    session.frames(Duration::from_millis(50)).await;
    assert_eq!(session.cell(column, row).0, PLAIN, "lit without a change");

    edit("a.txt", 2, "a2 agent");
    session
        .wait_frames("the mark to be lit", |app| {
            let screen = app.screen();
            screen.get(column, row).is_some_and(|cell| cell.fg == LIT)
        })
        .await;
    let lit_at = Instant::now();
    assert_eq!(session.mark(), Some(Some(1)));
    // a second and a bit later, nothing pressed in between
    session
        .frames((lit_at + Duration::from_millis(1300)).saturating_duration_since(Instant::now()))
        .await;
    assert_eq!(
        session.cell(column, row).0,
        PLAIN,
        "still lit after a second"
    );
    // four seconds after the change it is lit again
    session
        .frames((lit_at + Duration::from_millis(4300)).saturating_duration_since(Instant::now()))
        .await;
    assert_eq!(session.cell(column, row).0, LIT, "not lit again after four");
}

/// A click on the mark enters watch mode again.
#[tokio::test(flavor = "multi_thread")]
async fn clicking_the_mark_enters_watch_mode() {
    let Some(_) = enter("clicking_the_mark_enters_watch_mode") else {
        return;
    };
    let mut session = Session::new();
    session.start_and_leave().await;
    edit("a.txt", 2, "a2 agent");
    session
        .wait_watch("the mark to count 1", |app| mark_of(app) == Some(Some(1)))
        .await;
    let (column, row) = session.cell_of("●");
    // a click next to the mark does nothing
    session.click_cell(column.saturating_sub(10), row).await;
    assert!(!session.watching());
    session.click_cell(column, row).await;
    assert!(session.watching());
    assert!(session.on_screen("WATCH ● following"));
    assert_eq!(session.mark(), None);
}

// 21d. The catch-up

/// `:watch` with changes made while left follows them one after another, oldest first, the bar
/// counting (`catching up 1/3`), then says `caught up: following` and follows live.
#[tokio::test(flavor = "multi_thread")]
async fn the_catch_up_follows_the_changes_made_while_left_in_the_order_they_happened() {
    let Some(_) =
        enter("the_catch_up_follows_the_changes_made_while_left_in_the_order_they_happened")
    else {
        return;
    };
    let mut session = paced(300);
    session.left_with_three_changes().await;
    assert_eq!(session.mark(), Some(Some(3)));

    session.press(":watch<ret>");
    session.frames(Duration::from_millis(80)).await;
    assert!(session.watching());
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert_eq!(session.new_count(), 2);
    assert!(
        session.on_screen("catching up 1/3"),
        "{:#?}",
        session
            .rows()
            .iter()
            .filter(|row| !row.trim().is_empty())
            .collect::<Vec<_>>()
    );
    assert_eq!(session_of(&session.app).catch_up_progress(), Some((1, 3)));
    let seen = session
        .focus_until("the catch-up to end", |app| {
            session_of(app).catch_up.is_none()
        })
        .await;
    assert_eq!(
        seen,
        [at("a.txt", 1), at("b.txt", 4), at("c.txt", 7)],
        "the catch-up didn't visit the changes in the order they were made"
    );
    assert_eq!(session.activity(), "caught up: following");
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
    session.wait_for_screen_text("caught up: following").await;

    // and live again: the next change is followed
    session.change_and_follow("other.txt", 6, "o6 agent").await;
    assert!(session.following());
}

/// A change made during the catch-up is queued behind the others, not followed.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_made_during_the_catch_up_comes_after_the_others() {
    let Some(_) = enter("a_change_made_during_the_catch_up_comes_after_the_others") else {
        return;
    };
    let mut session = paced(500);
    session.left_with_three_changes().await;

    session.press(":watch<ret>");
    session.frames(Duration::from_millis(80)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    edit("other.txt", 6, "o6 agent");
    // recorded while the catch-up is still showing the earlier ones: not followed
    session
        .wait_frames("other.txt in the session", |app| {
            file(app, "other.txt").is_some()
        })
        .await;
    assert!(session_of(&session.app).catch_up.is_some());
    assert_ne!(focus(&session.app), at("other.txt", 5));
    let seen = session
        .focus_until("the catch-up to end", |app| {
            session_of(app).catch_up.is_none()
        })
        .await;
    assert_eq!(
        seen,
        [
            at("a.txt", 1),
            at("b.txt", 4),
            at("c.txt", 7),
            at("other.txt", 5)
        ]
    );
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
}

/// `v` during the catch-up looks around: the cursor stays where it is, the changes it hasn't
/// shown stay new, and nothing else moves.
#[tokio::test(flavor = "multi_thread")]
async fn v_during_the_catch_up_looks_around_and_the_rest_stay_new() {
    let Some(_) = enter("v_during_the_catch_up_looks_around_and_the_rest_stay_new") else {
        return;
    };
    let mut session = paced(400);
    session.left_with_three_changes().await;
    session.press(":watch<ret>");
    session.frames(Duration::from_millis(80)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1));

    session.keys("v").await;
    assert!(!session.following());
    assert!(session.watching());
    assert!(session_of(&session.app).catch_up.is_none());
    // longer than a step: it doesn't go on
    session.frames(Duration::from_millis(1500)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert_eq!(session.new_count(), 2);
    assert!(session.on_screen("WATCH ◌ looking"));
    assert!(session.on_screen("2 new"));
}

/// `n` during the catch-up takes over: it stops, `n` goes on from the change shown, and reaching
/// the newest follows again.
#[tokio::test(flavor = "multi_thread")]
async fn n_during_the_catch_up_takes_over() {
    let Some(_) = enter("n_during_the_catch_up_takes_over") else {
        return;
    };
    let mut session = paced(400);
    session.left_with_three_changes().await;
    session.press(":watch<ret>");
    session.frames(Duration::from_millis(80)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1));

    session.keys("n").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(session_of(&session.app).catch_up.is_none());
    session.frames(Duration::from_millis(1500)).await;
    assert_eq!(focus(&session.app), at("b.txt", 4), "the catch-up went on");
    assert_eq!(session.new_count(), 1);
    assert!(!session.following());

    session.keys("n").await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
}

/// `Esc` during the catch-up leaves watch mode: it stops, the changes it hadn't shown are still
/// new, and `:watch` again goes on from there (not from the first).
#[tokio::test(flavor = "multi_thread")]
async fn esc_during_the_catch_up_stops_it_and_watch_resumes_it_from_there() {
    let Some(_) = enter("esc_during_the_catch_up_stops_it_and_watch_resumes_it_from_there") else {
        return;
    };
    let mut session = paced(400);
    session.left_with_three_changes().await;
    session.press(":watch<ret>");
    session.frames(Duration::from_millis(80)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1));

    session.keys("<esc>").await;
    assert!(!session.watching());
    assert!(session_of(&session.app).catch_up.is_none());
    session.frames(Duration::from_millis(1500)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1), "the catch-up went on");
    assert_eq!(session.new_count(), 2, "the rest are still new");

    // it goes on with the change it hadn't shown, at once (not from the first one again)
    session.press(":watch<ret>");
    session.frames(Duration::from_millis(80)).await;
    assert!(session.watching());
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert_eq!(session.new_count(), 1);
    let seen = session
        .focus_until("the catch-up to end", |app| {
            session_of(app).catch_up.is_none()
        })
        .await;
    assert_eq!(seen, [at("b.txt", 4), at("c.txt", 7)]);
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
}

/// With `watch-catch-up = 0`, `:watch` jumps straight to the newest change: all seen.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_catch_up_pace_watch_jumps_to_the_newest_change() {
    let Some(_) = enter("without_a_catch_up_pace_watch_jumps_to_the_newest_change") else {
        return;
    };
    let mut session = paced(0);
    session.left_with_three_changes().await;
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert_eq!(focus(&session.app), at("c.txt", 7));
    assert_eq!(session.new_count(), 0);
    assert!(session.following());
    assert!(session_of(&session.app).catch_up.is_none());
    // nothing goes on in the background
    session.frames(Duration::from_millis(800)).await;
    assert_eq!(focus(&session.app), at("c.txt", 7));
}

/// Left while looking around, `:watch` doesn't move: the changes made meanwhile are only
/// counted as new.
#[tokio::test(flavor = "multi_thread")]
async fn watch_after_leaving_while_looking_around_does_not_catch_up() {
    let Some(_) = enter("watch_after_leaving_while_looking_around_does_not_catch_up") else {
        return;
    };
    let mut session = paced(200);
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.keys("v").await;
    assert!(!session.following());
    session.keys("<esc>").await;
    for (name, number) in [("b.txt", 5), ("c.txt", 8)] {
        edit(name, number, &format!("{name} agent"));
        session
            .wait_watch(&format!("{name} in the session"), |app| {
                file(app, name).is_some()
            })
            .await;
    }
    assert_eq!(session.new_count(), 2);

    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert!(!session.following());
    session.frames(Duration::from_millis(1000)).await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    assert_eq!(session.new_count(), 2);
    assert!(session_of(&session.app).catch_up.is_none());
    assert_eq!(session.activity(), "back: 2 new changes meanwhile");
}

/// `:watch-stop` after leaving ends everything: no mark, and `:watch` starts a fresh session
/// without a catch-up.
#[tokio::test(flavor = "multi_thread")]
async fn watch_stop_after_leaving_ends_the_session_and_its_catch_up() {
    let Some(_) = enter("watch_stop_after_leaving_ends_the_session_and_its_catch_up") else {
        return;
    };
    let mut session = paced(200);
    session.left_with_three_changes().await;
    let old = session_of(&session.app).id;
    session.keys(":watch-stop<ret>").await;
    assert!(session.app.editor.watch.live().is_none());
    assert_eq!(session.mark(), None);
    let before = focus(&session.app);

    session.keys(":watch<ret>").await;
    assert!(session.watching());
    assert_ne!(
        session_of(&session.app).id,
        old,
        "the old session came back"
    );
    assert!(session_of(&session.app).is_empty());
    session.frames(Duration::from_millis(1000)).await;
    assert_eq!(
        focus(&session.app),
        before,
        "a catch-up of a stopped session"
    );
    assert!(session_of(&session.app).catch_up.is_none());
}

// 21e. Review: `a` accepts, `x` rejects

/// `a` stages exactly the change followed: the index has those lines only, the other hunks of
/// the file and the other files stay unstaged, the gutter marks it `✓`.
#[tokio::test(flavor = "multi_thread")]
async fn accepting_a_change_stages_exactly_its_hunk() {
    let Some(_) = enter("accepting_a_change_stages_exactly_its_hunk") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.change_and_follow("b.txt", 5, "b5 agent").await;
    session.change_and_follow("a.txt", 8, "a8 agent").await;
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2), (7, 8)]);

    session.keys("a").await;
    assert_eq!(status(&session.app), "staged a.txt:8");
    assert_eq!(is_error(&session.app), None);
    let staged = cached();
    assert!(staged.contains("-a8\n+a8 agent"), "{staged}");
    assert!(!staged.contains("a2 agent"), "{staged}");
    assert!(!staged.contains("b5"), "{staged}");
    assert!(!staged.contains("b.txt"), "{staged}");
    let rest = unstaged();
    assert!(rest.contains("+a2 agent"), "{rest}");
    assert!(rest.contains("+b5 agent"), "{rest}");
    assert!(!rest.contains("a8 agent"), "{rest}");
    // the files are untouched, and it is still watching and following
    assert_eq!(
        read("a.txt"),
        lines("a", 10)
            .replace("a2\n", "a2 agent\n")
            .replace("a8\n", "a8 agent\n")
    );
    assert!(session.watching());
    assert!(session.following());

    // `✓` on the line of the accepted change, not on the other
    let rows = session.rows();
    let accepted = rows.iter().find(|row| row.contains("a8 agent")).unwrap();
    assert!(accepted.contains('✓'), "{accepted:?}");
    let other = rows.iter().find(|row| row.contains("a2 agent")).unwrap();
    assert!(!other.contains('✓'), "{other:?}");
}

/// A change of two hunks (one write that changed two lines far apart) is accepted whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_of_two_hunks_is_staged_whole() {
    let Some(_) = enter("a_change_of_two_hunks_is_staged_whole") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    std::fs::write(
        cwd().join("a.txt"),
        with_lines("a.txt", &[(2, "a2 agent"), (9, "a9 agent")]),
    )
    .unwrap();
    session.wait_for_focus("a.txt", 1).await;
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2), (8, 9)]);
    // another write, another change: not part of it
    std::thread::sleep(MTIME_GAP);
    edit("b.txt", 5, "b5 agent");
    session.wait_for_focus("b.txt", 4).await;
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 8));

    session.keys("a").await;
    assert_eq!(status(&session.app), "staged a.txt:2, a.txt:9");
    let staged = cached();
    assert!(staged.contains("+a2 agent"), "{staged}");
    assert!(staged.contains("+a9 agent"), "{staged}");
    assert!(!staged.contains("b5"), "{staged}");
    let rest = unstaged();
    assert!(!rest.contains("a.txt"), "{rest}");
    assert!(rest.contains("+b5 agent"), "{rest}");
}

/// `x` reverts exactly the change to what the file was when the watch started: its lines are
/// back, the other hunks untouched, `✗` is drawn where it was, `:watch-changes` has a review
/// column, and `n` goes on from it.
#[tokio::test(flavor = "multi_thread")]
async fn rejecting_a_change_reverts_exactly_its_hunk_and_marks_it() {
    let Some(_) = enter("rejecting_a_change_reverts_exactly_its_hunk_and_marks_it") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.change_and_follow("a.txt", 8, "a8 agent").await;
    session.change_and_follow("b.txt", 5, "b5 agent").await;
    // `a` first: the change in b.txt is accepted, to see both marks in the picker
    session.keys("a").await;
    assert_eq!(status(&session.app), "staged b.txt:5");
    session.keys("N").await;
    assert_eq!(focus(&session.app), at("a.txt", 7));

    session.keys("x").await;
    assert_eq!(status(&session.app), "rejected a.txt:8");
    assert_eq!(is_error(&session.app), None);
    assert_eq!(
        read("a.txt"),
        lines("a", 10).replace("a2\n", "a2 agent\n"),
        "only the line of the change went back"
    );
    // the other files and the index are as they were: only b's change is staged
    assert_eq!(read("b.txt"), lines("b", 10).replace("b5\n", "b5 agent\n"));
    let staged = cached();
    assert!(
        staged.contains("+b5 agent") && !staged.contains("a.txt"),
        "{staged}"
    );
    // the session no longer has that hunk
    session
        .wait_watch("the hunk to leave the session", |app| {
            file(app, "a.txt").is_some_and(|file| file.hunks.len() == 1)
        })
        .await;
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2)]);
    // `✗` where it was, on the line that is the baseline's again
    let rows = session.rows();
    let rejected = rows
        .iter()
        .find(|row| row.trim_end().ends_with("a8"))
        .unwrap_or_else(|| panic!("no a8 row: {rows:#?}"));
    assert!(rejected.contains('✗'), "{rejected:?}");
    let kept = rows.iter().find(|row| row.contains("a2 agent")).unwrap();
    assert!(!kept.contains('✗'), "{kept:?}");

    // the picker has the review column: `✗` on the rejected change, `✓` on the accepted one
    session.keys(":watch-changes<ret>").await;
    let rows = session.rows();
    let rejected_row = rows
        .iter()
        .find(|row| row.contains('✗'))
        .unwrap_or_else(|| panic!("no ✗ in the picker: {rows:#?}"));
    assert!(rejected_row.contains("a8"), "{rejected_row:?}");
    let accepted_row = rows
        .iter()
        .find(|row| row.contains('✓'))
        .unwrap_or_else(|| panic!("no ✓ in the picker: {rows:#?}"));
    assert!(accepted_row.contains("b.txt:5"), "{accepted_row:?}");
    session.keys("<esc>").await;

    // `n` goes on from the rejected change to the next one, b's, which is the newest
    session.keys("n").await;
    assert_eq!(focus(&session.app), at("b.txt", 4));
    assert!(session.following());
}

/// A rejection is an edit of the buffer, written: `u` in it undoes the edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejection_is_one_edit_of_the_buffer() {
    let Some(_) = enter("a_rejection_is_one_edit_of_the_buffer") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 3, "a3 agent").await;
    session.keys("x").await;
    assert_eq!(status(&session.app), "rejected a.txt:3");
    assert_eq!(read("a.txt"), lines("a", 10));
    assert_eq!(session.text(), lines("a", 10));
    // watch mode holds `u` (it edits): leave it, then undo
    session.keys("<esc>u").await;
    assert_eq!(session.text(), lines("a", 10).replace("a3\n", "a3 agent\n"));
}

/// The change was rewritten by the agent (its lines changed again) after `n` went to it: `a` and
/// `x` refuse, saying so, and change nothing on disk nor in the index.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_the_agent_rewrote_cannot_be_accepted_nor_rejected() {
    let Some(_) = enter("a_change_the_agent_rewrote_cannot_be_accepted_nor_rejected") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.change_and_follow("b.txt", 5, "b5 agent").await;
    session.keys("vN").await;
    assert_eq!(focus(&session.app), at("a.txt", 1));
    let old = session_of(&session.app)
        .file(&cwd().join("a.txt"))
        .unwrap()
        .hunks[0]
        .seq;

    // the agent rewrites the same line, and the session takes it in
    edit("a.txt", 2, "a2 rewritten");
    session
        .wait_watch("the rewritten hunk", |app| {
            file(app, "a.txt").is_some_and(|file| file.hunks[0].seq != old)
        })
        .await;
    let on_disk = read("a.txt");
    for key in ["x", "a", "y"] {
        session.keys(key).await;
        assert_eq!(
            is_error(&session.app).as_deref(),
            Some("that change is gone: its lines were changed again"),
            "{key}"
        );
        assert_eq!(read("a.txt"), on_disk, "{key} changed the file");
        assert_eq!(cached(), "", "{key} staged something");
    }
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2)]);
}

/// A buffer with unsaved edits of your own is never overwritten nor staged: `x` and `a` refuse,
/// the disk and the index stay as they were.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsaved_buffer_refuses_accepting_and_rejecting() {
    let Some(_) = enter("an_unsaved_buffer_refuses_accepting_and_rejecting") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.keys("<esc>").await;
    // an edit of your own, not written
    session.keys("ihello <esc>").await;
    assert_eq!(session.text().lines().nth(1), Some("hello a2 agent"));
    session.keys(":watch<ret>").await;
    assert!(session.watching());
    let on_disk = read("a.txt");
    for key in ["x", "a"] {
        session.keys(key).await;
        assert_eq!(
            is_error(&session.app).as_deref(),
            Some("a.txt has unsaved changes: write it first"),
            "{key}"
        );
        assert_eq!(read("a.txt"), on_disk, "{key} changed the file");
        assert_eq!(cached(), "", "{key} staged something");
        assert_eq!(session.text().lines().nth(1), Some("hello a2 agent"));
    }
}

/// A change that was accepted (staged) cannot be rejected: it tells to unstage it first, and
/// nothing changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_change_cannot_be_rejected() {
    let Some(_) = enter("a_staged_change_cannot_be_rejected") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.keys("a").await;
    assert_eq!(status(&session.app), "staged a.txt:2");
    let staged = cached();
    let on_disk = read("a.txt");

    session.keys("x").await;
    assert_eq!(
        is_error(&session.app).as_deref(),
        Some("a.txt:2 is staged: unstage it first")
    );
    assert_eq!(read("a.txt"), on_disk);
    assert_eq!(cached(), staged);
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2)]);
}

/// Without a change followed or gone to yet, `a`, `x` and `y` say so.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_current_change_review_and_yank_say_so() {
    let Some(_) = enter("without_a_current_change_review_and_yank_say_so") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    for key in ["a", "x", "y"] {
        session.keys(key).await;
        assert_eq!(
            is_error(&session.app).as_deref(),
            Some("no change yet: n goes to one"),
            "{key}"
        );
    }
    assert_eq!(cached(), "");
}

/// A file the session created, whose whole content is the change: `x` asks before deleting it;
/// anything but `y` keeps it, `y` deletes it.
#[tokio::test(flavor = "multi_thread")]
async fn rejecting_a_created_file_asks_before_deleting_it() {
    let Some(_) = enter("rejecting_a_created_file_asks_before_deleting_it") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    agent_write("fresh.txt", &lines("f", 3));
    session.wait_for_focus("fresh.txt", 0).await;
    assert_eq!(
        file(&session.app, "fresh.txt").unwrap().kind,
        FileKind::Created
    );

    session.keys("x").await;
    assert_eq!(
        status(&session.app),
        "Delete fresh.txt, created this session? (y/n)"
    );
    assert!(cwd().join("fresh.txt").exists());
    // `n` is the answer, not a key that goes to the next change
    session.keys("n").await;
    assert_eq!(status(&session.app), "Kept fresh.txt");
    assert!(cwd().join("fresh.txt").exists());
    assert_eq!(read("fresh.txt"), lines("f", 3));

    session.keys("x").await;
    assert_eq!(
        status(&session.app),
        "Delete fresh.txt, created this session? (y/n)"
    );
    session.keys("y").await;
    assert_eq!(status(&session.app), "rejected fresh.txt: deleted it");
    assert_eq!(is_error(&session.app), None);
    assert!(!cwd().join("fresh.txt").exists());
}

// 21f. `y` yanks the places of the change

/// `y` yanks the place of the change followed: `file:line`.
#[tokio::test(flavor = "multi_thread")]
async fn y_yanks_the_place_of_the_change() {
    let Some(_) = enter("y_yanks_the_place_of_the_change") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    session.change_and_follow("a.txt", 2, "a2 agent").await;
    session.keys("y").await;
    assert_eq!(status(&session.app), "yanked a.txt:2");
    assert_eq!(clipboard(&session.app).as_deref(), Some("a.txt:2"));
    assert_eq!(is_error(&session.app), None);
    assert!(session.watching());
    assert!(session.following());
}

/// A change of several hunks (one write): one place per hunk, a range for several lines.
#[tokio::test(flavor = "multi_thread")]
async fn y_yanks_one_place_per_hunk_of_the_change() {
    let Some(_) = enter("y_yanks_one_place_per_hunk_of_the_change") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    std::fs::write(
        cwd().join("a.txt"),
        with_lines(
            "a.txt",
            &[(2, "a2 agent"), (5, "a5 agent"), (6, "a6 agent")],
        ),
    )
    .unwrap();
    session.wait_for_focus("a.txt", 1).await;
    assert_eq!(hunks(&session.app, "a.txt"), [(1, 2), (4, 6)]);
    session.keys("y").await;
    assert_eq!(status(&session.app), "yanked 2 places");
    assert_eq!(
        clipboard(&session.app).as_deref(),
        Some("a.txt:2\na.txt:5-6")
    );
}

/// A write that deletes a line: its place is the line before it.
#[tokio::test(flavor = "multi_thread")]
async fn y_yanks_the_line_before_a_removed_hunk() {
    let Some(_) = enter("y_yanks_the_line_before_a_removed_hunk") else {
        return;
    };
    let mut session = Session::new();
    session.start_watch().await;
    std::fs::write(cwd().join("b.txt"), read("b.txt").replace("b5\n", "")).unwrap();
    session.wait_for_focus("b.txt", 4).await;
    assert_eq!(hunks(&session.app, "b.txt"), [(4, 4)]);
    session.keys("y").await;
    assert_eq!(status(&session.app), "yanked b.txt:4");
    assert_eq!(clipboard(&session.app).as_deref(), Some("b.txt:4"));
}
