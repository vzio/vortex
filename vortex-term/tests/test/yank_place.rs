//! Yanking places (`Space :`, `yank_place_absolute`, `yank_place_column`, and `C-y` in the
//! pickers that list places): what lands in the clipboard register `+` and what the status line
//! says.
//!
//! Files in a workspace are made in a directory inside of this crate, so that their path is
//! relative to the workspace root (the repository's); `tempfile::NamedTempFile` in the system's
//! temp directory is outside of it. The pickers of changes and of the line history are in
//! throwaway repositories: each of those tests runs again in a process of its own with the
//! repository as its working directory, as the tests of the commit log do.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use vortex_core::diagnostic::Severity;
use vortex_term::application::Application;

use super::git_log::{on_screen, Repo, Session};
use super::helpers::AppBuilder;
use super::uncommitted::{commit_all, cwd, lines, list_row, write};

/// The values of a register, if it has any.
fn register(app: &Application, name: char) -> Option<Vec<String>> {
    app.editor
        .registers
        .read(name, &app.editor)
        .map(|values| values.map(|value| value.into_owned()).collect())
}

/// The status line: its message and whether it is an error.
fn said(app: &Application) -> (String, bool) {
    app.editor
        .get_status()
        .map(|(message, severity)| (message.to_string(), *severity == Severity::Error))
        .unwrap_or_default()
}

/// What the place yanked last is, as the one value of register `+`.
fn clipboard(app: &Application) -> String {
    let values = register(app, '+').expect("the clipboard register has a value");
    assert_eq!(values.len(), 1, "{values:?}");
    values.into_iter().next().unwrap()
}

/// `n` lines `line 1`, `line 2`...
fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

/// A file in a directory of its own inside of the workspace.
struct Work {
    dir: tempfile::TempDir,
    file: PathBuf,
}

impl Work {
    fn new(name: &str, text: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("yank-place-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let file = dir.path().join(name);
        std::fs::write(&file, text).unwrap();
        Self { dir, file }
    }

    /// The place of `suffix` (`main.txt:42`) as yanked: relative to the workspace root, which is
    /// the parent of this crate.
    fn place(&self, suffix: &str) -> String {
        let dir = self.dir.path().file_name().unwrap().to_string_lossy();
        format!("vortex-term/{dir}/{suffix}")
    }

    fn session(&self) -> Session {
        open(&self.file)
    }
}

fn open(file: &Path) -> Session {
    Session::with_app(AppBuilder::new().with_file(file, None).build().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn space_colon_yanks_the_place_of_the_cursor() {
    let work = Work::new("main.txt", &numbered(60));
    let mut session = work.session();
    session.keys("41j<space>:").await;
    let place = work.place("main.txt:42");
    assert_eq!(clipboard(&session.app), place);
    assert_eq!(said(&session.app), (format!("yanked {place}"), false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_selection_over_lines_is_a_range_and_a_selected_line_is_one_line() {
    let work = Work::new("main.txt", &numbered(60));
    let mut session = work.session();

    // three lines selected with `x`
    session.keys("11jxxx<space>:").await;
    let range = work.place("main.txt:12-14");
    assert_eq!(clipboard(&session.app), range);
    assert_eq!(said(&session.app), (format!("yanked {range}"), false));

    // from the middle of a line to the middle of another one
    session.keys("<esc>gg2jlvjl<space>:").await;
    assert_eq!(clipboard(&session.app), work.place("main.txt:3-4"));

    // a line selected with `x` is just that line
    session.keys("<esc>gg4jx<space>:").await;
    let line = work.place("main.txt:5");
    assert_eq!(clipboard(&session.app), line);
    assert_eq!(said(&session.app), (format!("yanked {line}"), false));

    // within one line
    session.keys("<esc>gg5jwv<space>:").await;
    assert_eq!(clipboard(&session.app), work.place("main.txt:6"));
}

#[tokio::test(flavor = "multi_thread")]
async fn several_selections_give_one_place_each_in_order_without_repeats() {
    let work = Work::new("main.txt", &numbered(60));
    let mut session = work.session();

    // three cursors, on lines 12, 13 and 14
    session.keys("11jCC<space>:").await;
    let places: Vec<_> = [12, 13, 14]
        .iter()
        .map(|n| work.place(&format!("main.txt:{n}")))
        .collect();
    assert_eq!(clipboard(&session.app), places.join("\n"));
    assert_eq!(said(&session.app), ("yanked 3 places".into(), false));

    let text = "foo foo\nbar\nfoo\n";
    let work = Work::new("words.txt", text);
    let mut session = work.session();
    // `foo` three times: two on the first line, one on the last
    session.keys("%sfoo<ret><space>:").await;
    let expected = format!(
        "{}\n{}",
        work.place("words.txt:1"),
        work.place("words.txt:3")
    );
    assert_eq!(clipboard(&session.app), expected);
    assert_eq!(said(&session.app), ("yanked 2 places".into(), false));

    // two cursors on one line give one place, said as that place
    let work = Work::new("pair.txt", "foo foo\nbar\n");
    let mut session = work.session();
    session.keys("%sfoo<ret><space>:").await;
    let place = work.place("pair.txt:1");
    assert_eq!(clipboard(&session.app), place);
    assert_eq!(said(&session.app), (format!("yanked {place}"), false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_outside_the_workspace_gives_its_absolute_path() {
    let mut file = tempfile::Builder::new().suffix(".txt").tempfile().unwrap();
    std::io::Write::write_all(&mut file, numbered(10).as_bytes()).unwrap();
    // (not through a symlinked temp dir)
    let path = std::fs::canonicalize(file.path()).unwrap();
    let mut session = open(&path);
    session.keys("3j<space>:").await;
    let place = format!("{}:4", path.display());
    assert!(path.is_absolute());
    assert_eq!(clipboard(&session.app), place);
    assert_eq!(said(&session.app), (format!("yanked {place}"), false));

    session.keys("<esc>gg2jxx<space>:").await;
    assert_eq!(clipboard(&session.app), format!("{}:3-4", path.display()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_buffer_without_a_file_is_an_error_and_yanks_nothing() {
    let app = AppBuilder::new()
        .with_input_text("#[|o]#ne\ntwo\n")
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    session
        .app
        .editor
        .registers
        .write('+', vec!["before".to_string()])
        .unwrap();

    session.keys("<space>:").await;
    assert_eq!(
        said(&session.app),
        ("This buffer has no file".to_string(), true)
    );
    assert_eq!(
        register(&session.app, '+'),
        Some(vec!["before".to_string()])
    );

    // the other forms say the same
    session.keys("<space>?yank_place_absolute<ret>").await;
    assert_eq!(
        said(&session.app),
        ("This buffer has no file".to_string(), true)
    );
    assert_eq!(
        register(&session.app, '+'),
        Some(vec!["before".to_string()])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn yank_place_absolute_gives_the_absolute_path_and_lines() {
    let work = Work::new("main.txt", &numbered(30));
    let mut session = work.session();
    let path = std::fs::canonicalize(&work.file).unwrap();

    session.keys("<space>?yank_place_absolute<ret>").await;
    let place = format!("{}:1", path.display());
    assert_eq!(clipboard(&session.app), place);
    assert_eq!(said(&session.app), (format!("yanked {place}"), false));

    session.keys("4jxxx<space>?yank_place_absolute<ret>").await;
    assert_eq!(clipboard(&session.app), format!("{}:5-7", path.display()));
}

#[tokio::test(flavor = "multi_thread")]
async fn yank_place_column_gives_line_and_column_in_graphemes() {
    // `x` is the sixth grapheme of line 3 and the seventh character: the `e` has an accent
    // that is a character of its own
    let text = "one\ntwo\ncafe\u{301} xy\nfour\n";
    let work = Work::new("grapheme.txt", text);
    let mut session = work.session();

    session.keys("2jfx<space>?yank_place_column<ret>").await;
    let place = work.place("grapheme.txt:3:6");
    assert_eq!(clipboard(&session.app), place);
    assert_eq!(said(&session.app), (format!("yanked {place}"), false));

    // the cursor of a selection is on its last character: `r` is the fourth of `four`
    session
        .keys("<esc>gg%sx|four<ret><space>?yank_place_column<ret>")
        .await;
    assert_eq!(
        clipboard(&session.app),
        format!(
            "{}\n{}",
            work.place("grapheme.txt:3:6"),
            work.place("grapheme.txt:4:4")
        )
    );
    assert_eq!(said(&session.app), ("yanked 2 places".to_string(), false));

    // the first line, first column
    session.keys("<esc>gg<space>?yank_place_column<ret>").await;
    assert_eq!(clipboard(&session.app), work.place("grapheme.txt:1:1"));
}

// -- the pickers --------------------------------------------------------------------------------

/// A row of the screen starts a frame: a picker or the tree is open.
fn framed(session: &Session) -> bool {
    session.rows().iter().any(|row| row.contains('┌'))
}

/// Opens the key help with `?` and checks that it lists `C-y` and what it yanks, then closes it.
async fn assert_help_lists_c_y(session: &mut Session, title: &str, what: &str) {
    session.keys("?").await;
    let rows = session.rows();
    assert!(on_screen(&rows, title), "{rows:#?}");
    assert!(on_screen(&rows, "any key closes"), "{rows:#?}");
    assert!(
        rows.iter()
            .any(|row| row.contains("C-y") && row.contains(what)),
        "no `C-y {what}` in the help:\n{rows:#?}"
    );
    session.keys("j").await;
    assert!(!on_screen(&session.rows(), "any key closes"));
    assert!(framed(session));
}

/// `C-y` in a picker: runs for the screen to settle first, then presses the key.
async fn ctrl_y(session: &mut Session) {
    session.app.editor.clear_status();
    session.keys("<C-y>").await;
}

/// 40 lines in a directory of the workspace.
fn forty() -> Work {
    Work::new("forty.txt", &numbered(40))
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_history_picker_yanks_the_place_of_the_row() {
    let work = forty();
    let mut session = work.session();
    // two jumps: from line 4 to line 11, from there to line 30 (the present)
    session.keys("3j/line 11<ret>/line 30<ret>").await;
    session.keys("gA").await;
    session.run_for(Duration::from_millis(300)).await;
    assert!(on_screen(&session.rows(), "●"));

    // the cursor starts on the present
    ctrl_y(&mut session).await;
    let present = work.place("forty.txt:30");
    assert_eq!(clipboard(&session.app), present);
    assert_eq!(said(&session.app), (format!("yanked {present}"), false));
    assert!(framed(&session), "the picker stays open");
    assert!(on_screen(&session.rows(), "●"));

    for (keys, line) in [("j", 11), ("j", 4)] {
        session.keys(keys).await;
        ctrl_y(&mut session).await;
        let place = work.place(&format!("forty.txt:{line}"));
        assert_eq!(clipboard(&session.app), place);
        assert_eq!(said(&session.app), (format!("yanked {place}"), false));
        assert!(framed(&session), "the picker stays open");
    }
    // nothing moved
    assert!(on_screen(&session.rows(), "forty.txt:11"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_on_a_screen_in_the_history_picker_is_an_error() {
    let work = forty();
    let mut session = work.session();
    session.keys("3j:dashboard<ret>").await;
    session.run_for(Duration::from_millis(300)).await;
    session
        .app
        .editor
        .registers
        .write('+', vec!["before".to_string()])
        .unwrap();
    session.keys("gA").await;
    session.run_for(Duration::from_millis(300)).await;
    assert!(on_screen(&session.rows(), "the dashboard"));

    // the cursor is on the present, the dashboard
    ctrl_y(&mut session).await;
    assert_eq!(
        said(&session.app),
        ("No file at this place".to_string(), true)
    );
    assert_eq!(
        register(&session.app, '+'),
        Some(vec!["before".to_string()])
    );
    assert!(framed(&session));

    // the row below is a place in the file
    session.keys("j").await;
    ctrl_y(&mut session).await;
    let place = work.place("forty.txt:4");
    assert_eq!(clipboard(&session.app), place);
    assert_eq!(said(&session.app), (format!("yanked {place}"), false));
    assert!(framed(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_history_picker_lists_ctrl_y() {
    let work = forty();
    let mut session = work.session();
    session.keys("3j/line 11<ret>gA").await;
    session.run_for(Duration::from_millis(300)).await;
    assert_help_lists_c_y(&mut session, "History", "yank file:line").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_bookmark_picker_yanks_the_place_of_the_row() {
    let work = forty();
    let mut session = work.session();
    session.keys("2j<C-s>5j<C-s>gg<space>j").await;
    session.run_for(Duration::from_millis(300)).await;
    assert!(framed(&session));

    // the rows are in the order of the bookmarks: line 3, line 8
    ctrl_y(&mut session).await;
    let first = work.place("forty.txt:3");
    assert_eq!(clipboard(&session.app), first);
    assert_eq!(said(&session.app), (format!("yanked {first}"), false));
    assert!(framed(&session), "the picker stays open");

    session.keys("j").await;
    ctrl_y(&mut session).await;
    let second = work.place("forty.txt:8");
    assert_eq!(clipboard(&session.app), second);
    assert_eq!(said(&session.app), (format!("yanked {second}"), false));
    assert!(framed(&session), "the picker stays open");
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_bookmark_picker_of_a_buffer_without_a_file_is_an_error() {
    let app = AppBuilder::new()
        .with_input_text("#[|o]#ne\ntwo\nthree\n")
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    session.keys("j<C-s><space>j").await;
    session.run_for(Duration::from_millis(300)).await;
    assert!(framed(&session));
    session
        .app
        .editor
        .registers
        .write('+', vec!["before".to_string()])
        .unwrap();
    ctrl_y(&mut session).await;
    assert_eq!(
        said(&session.app),
        ("This buffer has no file".to_string(), true)
    );
    assert_eq!(
        register(&session.app, '+'),
        Some(vec!["before".to_string()])
    );
    assert!(framed(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_bookmark_picker_lists_ctrl_y() {
    let work = forty();
    let mut session = work.session();
    session.keys("2j<C-s>5j<C-s>gg<space>j").await;
    session.run_for(Duration::from_millis(300)).await;
    assert_help_lists_c_y(&mut session, "Bookmarks", "yank file:line").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_file_tree_yanks_the_path_of_the_selected_entry() {
    let work = forty();
    let mut session = work.session();
    // the tree with the file revealed and selected
    session.keys("<space>E").await;
    assert!(framed(&session));
    ctrl_y(&mut session).await;
    let file = work.place("forty.txt");
    assert_eq!(clipboard(&session.app), file);
    assert_eq!(said(&session.app), (format!("yanked {file}"), false));
    assert!(framed(&session), "the tree stays open");

    // `h` goes to the directory of the file
    session.keys("h").await;
    ctrl_y(&mut session).await;
    let dir = work.place("").trim_end_matches('/').to_string();
    assert!(dir.starts_with("vortex-term/yank-place-"), "{dir}");
    assert_eq!(clipboard(&session.app), dir);
    assert_eq!(said(&session.app), (format!("yanked {dir}"), false));
    assert!(framed(&session), "the tree stays open");

    // and in the filter mode
    let name = work
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    session.keys(&format!("s{name}/forty.txt")).await;
    session.run_for(Duration::from_millis(300)).await;
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), file);
    assert!(framed(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_file_tree_lists_ctrl_y() {
    let work = forty();
    let mut session = work.session();
    session.keys("<space>E?").await;
    let rows = session.rows();
    assert!(on_screen(&rows, "File tree"), "{rows:#?}");
    assert!(
        rows.iter()
            .any(|row| row.contains("C-y") && row.contains("yank the path")),
        "{rows:#?}"
    );
}

// -- the lists of changes and the line history, in a repository -----------------------------------

fn enter(test: &str) -> Option<PathBuf> {
    Repo::enter_in(module_path!(), test)
}

/// `text` with each of the lines `from..=to` (from 1) replaced by `with` and its number.
fn change(text: &str, from: usize, to: usize, with: &str) -> String {
    text.split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            let n = i + 1;
            if (from..=to).contains(&n) {
                format!("{with} {n}\n")
            } else {
                line.to_string()
            }
        })
        .collect()
}

/// a.txt, b.txt (20 lines each) and logo.bin committed, then changed: a.txt at its line 6 and
/// at its lines 12 to 14, b.txt at its line 3, logo.bin (a binary file).
fn changed_files() {
    write("a.txt", &lines("line ", 20));
    write("b.txt", &lines("line ", 20));
    std::fs::write(cwd().join("logo.bin"), [0u8, 1, 2, 3]).unwrap();
    commit_all("Add the files");
    write(
        "a.txt",
        &change(&change(&lines("line ", 20), 6, 6, "six"), 12, 14, "twelve"),
    );
    write("b.txt", &change(&lines("line ", 20), 3, 3, "three"));
    std::fs::write(cwd().join("logo.bin"), [0u8, 9, 9, 9, 9]).unwrap();
}

/// The text of the row under the cursor of a list of changes.
fn selected(session: &Session) -> String {
    let frame = session.frame();
    let row = frame[2..]
        .iter()
        .find(|row| row.starts_with(" > "))
        .unwrap_or_else(|| panic!("no cursor: {frame:#?}"));
    list_row(row, false)
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_uncommitted_changes_yanks_the_hunk_or_the_file() {
    let Some(_) = enter("ctrl_y_in_the_uncommitted_changes_yanks_the_hunk_or_the_file") else {
        return;
    };
    changed_files();
    let mut session = Session::open_log_on_uncommitted().await;
    session.wait_for_uncommitted("(3 files)").await;
    session.keys("<ret>").await;
    session
        .wait_for_screen("the list", |rows| on_screen(rows, "a.txt:12-14"))
        .await;

    // the cursor starts on the first hunk of the first file
    assert!(
        selected(&session).ends_with("a.txt:6"),
        "{}",
        selected(&session)
    );
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "a.txt:6");
    assert_eq!(said(&session.app), ("yanked a.txt:6".to_string(), false));
    assert!(on_screen(&session.rows(), "Uncommitted changes:"));

    // a hunk of several lines
    session.keys("j").await;
    assert!(
        selected(&session).ends_with("a.txt:12-14"),
        "{}",
        selected(&session)
    );
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "a.txt:12-14");
    assert_eq!(
        said(&session.app),
        ("yanked a.txt:12-14".to_string(), false)
    );

    // the next file
    session.keys("j").await;
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "b.txt:3");

    // the divider row of a file that has no hunks: its path alone
    session.keys("j").await;
    assert!(
        selected(&session).contains("logo.bin"),
        "{}",
        selected(&session)
    );
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "logo.bin");
    assert_eq!(said(&session.app), ("yanked logo.bin".to_string(), false));
    // the list stays open
    assert!(on_screen(&session.rows(), "Uncommitted changes:"));
    assert!(framed(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_uncommitted_changes_lists_ctrl_y() {
    let Some(_) = enter("question_mark_in_the_uncommitted_changes_lists_ctrl_y") else {
        return;
    };
    changed_files();
    let mut session = Session::open_log_on_uncommitted().await;
    session.wait_for_uncommitted("(3 files)").await;
    session.keys("<ret>").await;
    session
        .wait_for_screen("the list", |rows| on_screen(rows, "a.txt:12-14"))
        .await;
    assert_help_lists_c_y(&mut session, "Changes", "yank file:lines").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_changes_of_a_commit_yanks_the_hunk_or_the_file() {
    let Some(_) = enter("ctrl_y_in_the_changes_of_a_commit_yanks_the_hunk_or_the_file") else {
        return;
    };
    changed_files();
    commit_all("Change the files");
    let mut session = Session::open_log().await;
    session.keys("<ret>").await;
    session
        .wait_for_screen("the list", |rows| on_screen(rows, "a.txt:12-14"))
        .await;
    assert!(!on_screen(&session.rows(), "Uncommitted changes:"));

    assert!(
        selected(&session).ends_with("a.txt:6"),
        "{}",
        selected(&session)
    );
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "a.txt:6");
    assert_eq!(said(&session.app), ("yanked a.txt:6".to_string(), false));

    session.keys("j").await;
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "a.txt:12-14");
    assert_eq!(
        said(&session.app),
        ("yanked a.txt:12-14".to_string(), false)
    );

    session.keys("jj").await;
    assert!(
        selected(&session).contains("logo.bin"),
        "{}",
        selected(&session)
    );
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "logo.bin");
    assert!(framed(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_changes_of_a_commit_lists_ctrl_y() {
    let Some(_) = enter("question_mark_in_the_changes_of_a_commit_lists_ctrl_y") else {
        return;
    };
    changed_files();
    commit_all("Change the files");
    let mut session = Session::open_log().await;
    session.keys("<ret>").await;
    session
        .wait_for_screen("the list", |rows| on_screen(rows, "a.txt:12-14"))
        .await;
    assert_help_lists_c_y(&mut session, "Changes", "yank file:lines").await;
}

/// Starts watch mode, changes a file twice (line 3, then lines 8 and 9 of `w.txt`) and opens
/// `:watch-changes` once the session has both hunks.
async fn watch_list() -> Session {
    write("w.txt", &lines("line ", 12));
    commit_all("Add w");
    let mut session = Session::new();
    session.keys(":watch<ret>").await;
    assert!(session.app.editor.watch.is_active());
    session.run_for(Duration::from_millis(300)).await;
    for (from, to, text) in [(3, 3, "three"), (8, 9, "eight")] {
        let old = std::fs::read_to_string(cwd().join("w.txt")).unwrap();
        write("w.txt", &change(&old, from, to, text));
        let hunks = |app: &Application| {
            app.editor
                .watch
                .session
                .as_ref()
                .and_then(|session| session.file(&cwd().join("w.txt")))
                .map_or(0, |file| file.hunks.len())
        };
        let wanted = if from == 3 { 1 } else { 2 };
        for _ in 0..400 {
            if hunks(&session.app) == wanted {
                break;
            }
            session.run_for(Duration::from_millis(50)).await;
        }
        assert_eq!(hunks(&session.app), wanted, "{:?}", said(&session.app));
        // the writes have different modification times
        std::thread::sleep(Duration::from_millis(25));
    }
    session.keys(":watch-changes<ret>").await;
    session
        .wait_for_screen("the watch list", |rows| on_screen(rows, "w.txt:8-9"))
        .await;
    session
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_watch_list_yanks_the_hunk() {
    let Some(_) = enter("ctrl_y_in_the_watch_list_yanks_the_hunk") else {
        return;
    };
    let mut session = watch_list().await;
    // the newest hunk first
    assert!(
        selected(&session).ends_with("w.txt:8-9"),
        "{}",
        selected(&session)
    );
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "w.txt:8-9");
    assert_eq!(said(&session.app), ("yanked w.txt:8-9".to_string(), false));
    assert!(framed(&session), "the list stays open");

    session.keys("j").await;
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "w.txt:3");
    assert_eq!(said(&session.app), ("yanked w.txt:3".to_string(), false));
    assert!(framed(&session), "the list stays open");
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_watch_list_lists_ctrl_y() {
    let Some(_) = enter("question_mark_in_the_watch_list_lists_ctrl_y") else {
        return;
    };
    let mut session = watch_list().await;
    assert_help_lists_c_y(&mut session, "Changes", "yank file:lines").await;
}

/// `notes.txt` of 12 lines, three commits (line 3, line 4, line 9 changed after it was added),
/// open with the cursor on its first line.
async fn notes_session() -> Session {
    write("notes.txt", &lines("line ", 12));
    commit_all("Add the notes");
    for (from, to, text, summary) in [
        (3, 3, "three", "Edit three"),
        (4, 4, "four", "Edit four"),
        (9, 9, "nine", "Edit nine"),
    ] {
        let old = std::fs::read_to_string(cwd().join("notes.txt")).unwrap();
        write("notes.txt", &change(&old, from, to, text));
        commit_all(summary);
    }
    open(&cwd().join("notes.txt"))
}

/// Opens the line history with `keys` and waits for its walk to end.
async fn open_line_history(session: &mut Session, keys: &str) {
    // wide: the info row is cut when the list has half of the terminal
    session.widen().await;
    session.keys(keys).await;
    for _ in 0..400 {
        let info = session.info();
        if info.contains(" commit") && !info.contains("searching…") && !info.ends_with('…') {
            return;
        }
        session.run_for(Duration::from_millis(50)).await;
    }
    panic!(
        "the walk did not end: {:?}\n{:#?}",
        session.info(),
        session.rows()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_line_history_yanks_the_lines_followed() {
    let Some(_) = enter("ctrl_y_in_the_line_history_yanks_the_lines_followed") else {
        return;
    };
    let mut session = notes_session().await;
    // lines 3 and 4 selected: one range
    session.keys("2jxx").await;
    open_line_history(&mut session, "<space>L").await;
    assert!(
        on_screen(&session.rows(), "Edit four"),
        "{:#?}",
        session.log()
    );

    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "notes.txt:3-4");
    assert_eq!(
        said(&session.app),
        ("yanked notes.txt:3-4".to_string(), false)
    );
    assert!(on_screen(&session.rows(), "Edit three"));
    assert!(framed(&session), "the picker stays open");

    // the same from another row
    session.keys("j").await;
    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "notes.txt:3-4");
    assert!(framed(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_y_in_the_line_history_of_several_ranges_yanks_one_place_for_each() {
    let Some(_) = enter("ctrl_y_in_the_line_history_of_several_ranges_yanks_one_place_for_each")
    else {
        return;
    };
    let mut session = notes_session().await;
    // two selections: line 3 and line 9
    session.keys("%sline 3|line 9<ret>").await;
    // (the lines of the buffer are `three` and `nine` now)
    session.keys("<esc>").await;
    session.keys("%sthree|nine<ret>").await;
    open_line_history(&mut session, "<space>L").await;

    ctrl_y(&mut session).await;
    assert_eq!(clipboard(&session.app), "notes.txt:3\nnotes.txt:9");
    assert_eq!(said(&session.app), ("yanked 2 places".to_string(), false));
    assert!(framed(&session), "the picker stays open");
}

#[tokio::test(flavor = "multi_thread")]
async fn question_mark_in_the_line_history_lists_ctrl_y() {
    let Some(_) = enter("question_mark_in_the_line_history_lists_ctrl_y") else {
        return;
    };
    let mut session = notes_session().await;
    session.keys("2j").await;
    open_line_history(&mut session, "<space>L").await;
    assert_help_lists_c_y(&mut session, "Line history", "yank file:lines").await;
}
