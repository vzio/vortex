//! The screens guard: the commands that walk documents or windows, run with every kind of
//! screen open, focused and unfocused, so that a later change that assumes "a window holds
//! editable text" fails here.
//!
//! One helper ([`run`]) takes the kind of screen and the case (the screen focused, or the text
//! window next to it) and runs the whole list of commands; a new kind of screen is one more
//! variant of [`Kind`] and one more pair of tests.
//!
//! The layout is `a.txt | b` with the screen either covering `b` (the hex view, the diff and the
//! Markdown page cover the window they open from) or in a window of its own (the terminal and the
//! debug panel, in a split of `b`). After every command the guard checks that:
//!
//! - the focus has a text window to fall back to (`text_focus`), and that exists;
//! - every shown window is a text window of a document of the editor, or a screen of the editor;
//! - the text of `a.txt` is what the commands left it (only `"ap`, `u` and `U` in its own window
//!   change it);
//! - the screen is open or gone as the command does to it (the table in [`after`]);
//! - the status line only said what [`messages`] expects.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use termina::event::{Event, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;
use vortex_term::{application::Application, config::Config};
use vortex_view::{
    document::Mode,
    editor::{Config as EditorConfig, Severity},
    input::parse_macro,
    screen::ScreenKind,
    terminal::TerminalConfig,
    tree::Window,
    DocumentId, ViewId,
};

use super::git_log::{git, Repo};
use super::helpers::{test_config, test_editor_config, AppBuilder};

const A_TEXT: &str = "alpha one\nbeta two\ngamma three\n";
const B_TEXT: &str = "delta four\nepsilon five\n";
const MD_TEXT: &str = "# Title\n\nsome text here\n\n- one\n- two\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Terminal,
    Hex,
    Diff,
    Markdown,
    DebugPanel,
}

impl Kind {
    fn screen_kind(self) -> ScreenKind {
        match self {
            Kind::Terminal => ScreenKind::Terminal,
            Kind::Hex => ScreenKind::Hex,
            Kind::Diff => ScreenKind::Diff,
            Kind::Markdown => ScreenKind::Markdown,
            Kind::DebugPanel => ScreenKind::DebugPanel,
        }
    }

    /// The file of the window `b`.
    fn b_file(self) -> &'static str {
        match self {
            Kind::Markdown => "b.md",
            _ => "b.txt",
        }
    }

    /// Whether the screen covers `b` (else it has a window of its own).
    fn covers(self) -> bool {
        matches!(self, Kind::Hex | Kind::Diff | Kind::Markdown)
    }
}

/// The state of the screen of the kind under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Has a window in the layout.
    Shown,
    /// Exists, without a window in the layout (a hidden panel, a parked cover).
    Hidden,
    Gone,
}

struct Rig {
    app: Application,
    tx: UnboundedSender<std::io::Result<Event>>,
    rx: UnboundedReceiverStream<std::io::Result<Event>>,
    kind: Kind,
    /// The screen has the focus (else `a.txt`'s window).
    focused: bool,
    dir: PathBuf,
    a_view: ViewId,
    b_view: ViewId,
    a_doc: DocumentId,
    /// What the text of `a.txt` must be, `None` once it is closed on purpose.
    a_text: Option<String>,
    /// The problems (errors and warnings) the status line showed, with the directory replaced.
    problems: BTreeSet<String>,
    /// One line per command: what it did, for the report (`--nocapture`).
    log: Vec<String>,
    windows: usize,
}

fn config() -> Config {
    let mut config = test_config();
    config.editor = EditorConfig {
        terminal: TerminalConfig {
            shell: vec!["sh".to_string()],
            ..TerminalConfig::default()
        },
        ..test_editor_config()
    };
    config
}

/// A directory with the files of the layout (a plain one, not a repository).
fn plain_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(path.join("a.txt"), A_TEXT).unwrap();
    std::fs::write(path.join("b.txt"), B_TEXT).unwrap();
    std::fs::write(path.join("b.md"), MD_TEXT).unwrap();
    (dir, path)
}

/// A repository with the files of the layout, and `b.txt` changed since the commit: the diff of
/// `b.txt` has something to show.
fn repo(parent: &Path) -> PathBuf {
    let work = parent.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, "", &["init", "-q"]);
    std::fs::write(work.join("a.txt"), A_TEXT).unwrap();
    std::fs::write(work.join("b.txt"), B_TEXT).unwrap();
    git(&work, "", &["add", "-A"]);
    git(&work, "", &["commit", "-q", "-m", "Add the files"]);
    std::fs::write(work.join("b.txt"), format!("{B_TEXT}zeta six\n")).unwrap();
    work
}

impl Rig {
    async fn new(kind: Kind, focused: bool, dir: &Path) -> Self {
        let app = AppBuilder::new()
            .with_config(config())
            .with_file(dir.join("a.txt"), None)
            .build()
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut rig = Self {
            a_doc: DocumentId::default(),
            app,
            tx,
            rx: UnboundedReceiverStream::new(rx),
            kind,
            focused,
            dir: dir.to_owned(),
            a_view: ViewId::default(),
            b_view: ViewId::default(),
            a_text: Some(A_TEXT.to_string()),
            problems: BTreeSet::new(),
            log: Vec::new(),
            windows: 0,
        };
        rig.app.resize_screen(120, 40);
        rig.tx
            .send(Ok(Event::WindowResized(termina::WindowSize {
                cols: 120,
                rows: 40,
                pixel_width: None,
                pixel_height: None,
            })))
            .unwrap();
        rig.pump(Duration::from_millis(50)).await;
        rig.a_view = rig.app.editor.tree.focus;
        rig.a_doc = rig.app.editor.tree.get(rig.a_view).doc;
        // a | b
        rig.send(&format!(
            ":vsplit<ret>:e {}<ret>",
            dir.join(kind.b_file()).display()
        ))
        .await;
        rig.b_view = rig.app.editor.tree.focus;
        assert_ne!(rig.a_view, rig.b_view);
        rig.open().await;
        rig.refocus().await;
        rig.windows = rig.shown();
        rig
    }

    /// Opens the screen under test from the window `b`.
    async fn open(&mut self) {
        if !self.app.editor.tree.contains(self.b_view) {
            // `b` is gone: open from whatever text window there is
            self.b_view = self.app.editor.tree.text_focus();
        }
        let b_view = self.b_view;
        if self.app.editor.tree.is_visible(b_view) {
            self.app.editor.focus(b_view);
        }
        self.app.editor.enter_normal_mode();
        match self.kind {
            Kind::Terminal => self.send(":terminal<ret>").await,
            Kind::Hex => self.send(":hex<ret>").await,
            Kind::Diff => self.send("<space>=").await,
            Kind::Markdown => self.send("V").await,
            Kind::DebugPanel => {
                self.app.editor.show_debug_panel(true);
                self.pump(Duration::from_millis(50)).await;
            }
        }
        let start = Instant::now();
        while self.state() != State::Shown {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the {:?} screen didn't open: {:?}",
                self.kind,
                self.status()
            );
            self.pump(Duration::from_millis(50)).await;
        }
        // (a started terminal draws its prompt)
        self.pump(Duration::from_millis(100)).await;
    }

    /// Runs the event loop for `time`.
    async fn pump(&mut self, time: Duration) {
        let end = Instant::now() + time;
        loop {
            self.app.editor.reset_idle_timer();
            let idle = tokio::time::timeout(
                Duration::from_millis(20),
                self.app.event_loop_until_idle(&mut self.rx),
            )
            .await;
            if let Ok(running) = idle {
                assert!(running, "the editor quit");
            }
            if Instant::now() >= end {
                break;
            }
        }
        self.note_status();
    }

    /// Sends keys and lets the editor settle.
    async fn send(&mut self, keys: &str) {
        for key in parse_macro(keys).unwrap() {
            self.tx.send(Ok(Event::Key(KeyEvent::from(key)))).unwrap();
        }
        assert!(self.app.event_loop_until_idle(&mut self.rx).await);
        self.note_status();
        // (background results: a picker's rows, a terminal's output)
        self.pump(Duration::from_millis(40)).await;
    }

    fn status(&self) -> Option<(String, Severity)> {
        self.app
            .editor
            .get_status()
            .map(|(message, severity)| (message.to_string(), *severity))
    }

    fn note_status(&mut self) {
        if let Some((message, severity)) = self.status() {
            if matches!(severity, Severity::Error | Severity::Warning) {
                let message = message.replace(self.dir.to_str().unwrap(), "<dir>");
                self.problems.insert(message);
            }
        }
    }

    fn state(&self) -> State {
        let editor = &self.app.editor;
        let Some(id) = editor.screens.find(self.kind.screen_kind()) else {
            return State::Gone;
        };
        match editor.view_of(id) {
            Some(view) if editor.tree.is_visible(view) => State::Shown,
            _ => State::Hidden,
        }
    }

    /// The number of windows shown.
    fn shown(&self) -> usize {
        self.app.editor.tree.windows().count()
    }

    /// Gives the focus back to what the case has it in: the screen or `a.txt`'s window; normal
    /// mode, a terminal's view mode left.
    async fn refocus(&mut self) {
        let shown = self.state() == State::Shown;
        let editor = &mut self.app.editor;
        let target = if self.focused && shown {
            let id = editor.screens.find(self.kind.screen_kind()).unwrap();
            editor.view_of(id).map(|view| editor.tree.top_of(view))
        } else if editor.tree.is_visible(self.a_view) {
            Some(self.a_view)
        } else {
            None
        };
        if let Some(view) = target {
            if editor.tree.focus != view {
                editor.focus(view);
            }
        }
        editor.enter_normal_mode();
        assert_eq!(editor.mode(), Mode::Normal);
    }

    /// The text of `a.txt`, `None` once it is closed.
    fn a_text_now(&self) -> Option<String> {
        self.app
            .editor
            .document(self.a_doc)
            .map(|doc| doc.text().to_string())
    }

    /// The invariants that hold after every command.
    fn check(&mut self, label: &str) {
        if let Some(violation) = self.violations().first() {
            panic!("{label}: {violation}");
        }
    }

    fn violations(&self) -> Vec<String> {
        let editor = &self.app.editor;
        let mut violations = Vec::new();
        let focus = editor.tree.text_focus();
        if editor.tree.try_get(focus).is_none() {
            violations.push(format!("the text focus {focus:?} is not a text window"));
        }
        for (window, _) in editor.tree.windows() {
            match window {
                Window::Text(view) => {
                    if editor.document(view.doc).is_none() {
                        violations.push(format!(
                            "the window {:?} shows a document that is gone",
                            view.id
                        ));
                    }
                }
                Window::Screen(view) => {
                    if editor.screens.get(view.screen).is_none() {
                        violations.push(format!(
                            "the window {:?} shows a screen that is gone",
                            view.id
                        ));
                    }
                }
            }
        }
        if self.a_text_now() != self.a_text {
            violations.push(format!(
                "the text of a.txt is {:?}, expected {:?}",
                self.a_text_now(),
                self.a_text
            ));
        }
        violations
    }

    /// What the diff has staged (the diff's keys act on the index), for the log.
    fn staged(&self) -> String {
        if self.kind != Kind::Diff {
            return String::new();
        }
        let output = std::process::Command::new("git")
            .args(["diff", "--cached", "--numstat"])
            .current_dir(&self.dir)
            .output()
            .unwrap();
        format!(
            " staged: {:?}",
            String::from_utf8_lossy(&output.stdout).trim()
        )
    }

    /// Where the focus is, for the log.
    fn focus_name(&self) -> String {
        let editor = &self.app.editor;
        if let Some(screen) = editor.focused_screen() {
            return format!("the {:?} screen", screen.kind);
        }
        let view = editor.tree.try_get(editor.tree.focus);
        let name = view
            .and_then(|view| editor.document(view.doc))
            .map(|doc| match doc.path() {
                Some(path) => path.file_name().unwrap().to_string_lossy().to_string(),
                None => "[scratch]".to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        format!("text {name}")
    }

    /// Runs `keys` (a command), checks the invariants, logs what happened, and puts the focus
    /// back. The screen must be in the state [`after`] says.
    async fn command(&mut self, label: &str, keys: &str) {
        self.app.editor.clear_status();
        let windows_before = self.shown();
        self.send(keys).await;
        let status = self.status().map(|(message, severity)| {
            format!(
                "{severity:?}: {}",
                message.replace(self.dir.to_str().unwrap(), "<dir>")
            )
        });
        self.check(label);
        let state = self.state();
        let expected = after(self.kind, self.focused, label);
        self.log.push(format!(
            "{:?} {}: {label:<24} focus: {:<22} screen: {state:?} windows: {windows_before} -> {} status: {}{}",
            self.kind,
            if self.focused { "focused  " } else { "unfocused" },
            self.focus_name(),
            self.shown(),
            status.as_deref().unwrap_or("-").replace('\n', "\\n"),
            self.staged(),
        ));
        say(self.log.last().unwrap());
        assert_eq!(
            state, expected,
            "{label}: the screen is {state:?}, expected {expected:?}"
        );
        self.recover().await;
        self.check(label);
        self.windows = self.shown();
    }

    /// Back to the layout of the start: the screen open (where a command closed or hid it), the
    /// text windows showing their own files, the focus where the case has it.
    async fn recover(&mut self) {
        self.leave_view().await;
        // a register or count left pending (none should be: see
        // `a_register_is_not_left_selected_by_the_hex_view`)
        let editor = &mut self.app.editor;
        if editor.selected_register.is_some() || editor.count.is_some() {
            say(&format!(
                "  (input left pending: register {:?}, count {:?})",
                editor.selected_register, editor.count
            ));
            editor.selected_register = None;
            editor.count = None;
        }
        match self.state() {
            State::Shown => {}
            State::Hidden if self.kind == Kind::DebugPanel => {
                self.app.editor.show_debug_panel(false);
            }
            _ => self.open().await,
        }
        // a window of the layout shows another buffer (`:bn`): put `a.txt` back in its window
        let shows_a = |rig: &Self| {
            rig.app
                .editor
                .tree
                .try_get(rig.a_view)
                .is_some_and(|view| view.doc == rig.a_doc)
        };
        if self.app.editor.tree.try_get(self.a_view).is_some()
            && self.app.editor.tree.is_visible(self.a_view)
            && !shows_a(self)
        {
            let a_view = self.a_view;
            self.app.editor.focus(a_view);
            let a_file = self.dir.join("a.txt");
            self.send(&format!(":e {}<ret>", a_file.display())).await;
            say("  (put a.txt back in its window)");
        }
        self.refocus().await;
    }

    /// A terminal in view mode (after a search or a move in its normal mode): back to its own
    /// window.
    async fn leave_view(&mut self) {
        for _ in 0..2 {
            if self.kind == Kind::Terminal
                && vortex_term::terminal::modes::viewing(&self.app.editor)
            {
                self.app.editor.enter_normal_mode();
                self.send("<esc>").await;
            }
        }
    }

    /// A command that may open a window (a split), and the keys that close it: the layout is
    /// what it was after both. (On a focused screen the keys after `C-w` may be the screen's,
    /// so a split is not always made: the log says.)
    async fn split(&mut self, label: &str, open: &str, close: &str) {
        let before = self.shown();
        self.app.editor.clear_status();
        self.send(open).await;
        let opened = self.shown();
        self.check(label);
        let focus = self.focus_name();
        let after_open = format!(
            "{:?}{}",
            self.status().map(|(message, _)| message),
            self.staged()
        );
        assert!(opened <= before + 1, "{label}: it opens at most one window");
        if opened > before {
            // the window `open` made has the focus (a terminal in its normal mode, where the
            // keys are the editor's)
            self.app.editor.enter_normal_mode();
            self.send(close).await;
        }
        let status = self.status().map(|(message, _)| message);
        self.check(label);
        say(&format!(
            "{:?} {}: {label:<24} focus: {focus:<22} screen: {:?} windows: {before} -> {opened} -> {} status: {} (after opening: {after_open})",
            self.kind,
            if self.focused { "focused  " } else { "unfocused" },
            self.state(),
            self.shown(),
            status.as_deref().unwrap_or("-"),
        ));
        assert_eq!(self.shown(), before, "{label}: it closes it again");
        self.recover().await;
        self.check(label);
    }

    /// `"ay`, `"ap`, `u`, `U`, `u`: the registers and undo, on the window with the focus.
    async fn registers_and_undo(&mut self) {
        let text_window = self
            .app
            .editor
            .tree
            .try_get(self.app.editor.tree.focus)
            .is_some()
            && self.focus_name() == "text a.txt";
        let old = self.a_text.clone().unwrap();
        let pasted = {
            let editor = &self.app.editor;
            let view = editor.tree.get(self.a_view);
            let doc = editor.document(self.a_doc).unwrap();
            let range = doc.selection(view.id).primary();
            let (from, to) = (range.from(), range.to());
            let text = doc.text();
            let yanked: String = text.slice(from..to).to_string();
            let mut new = text.slice(..to).to_string();
            new.push_str(&yanked);
            new.push_str(&text.slice(to..).to_string());
            new
        };
        self.command("\"ay", "\"ay").await;
        if text_window {
            self.a_text = Some(pasted.clone());
        }
        self.command("\"ap", "\"ap").await;
        if text_window {
            assert_ne!(pasted, old);
        }
        // undo and redo
        if text_window {
            self.a_text = Some(old.clone());
        }
        self.command("u", "u").await;
        if text_window {
            self.a_text = Some(pasted);
        }
        self.command("U", "U").await;
        if text_window {
            self.a_text = Some(old);
        }
        self.command("u again", "u").await;
    }

    /// Ends the application (the shells with it).
    async fn finish(mut self) {
        let focus = self.app.editor.tree.text_focus();
        self.app.editor.focus(focus);
        self.app.editor.enter_normal_mode();
        for key in parse_macro(":qa!<ret>").unwrap() {
            self.tx.send(Ok(Event::Key(KeyEvent::from(key)))).unwrap();
        }
        let _ =
            tokio::time::timeout(Duration::from_secs(3), self.app.event_loop(&mut self.rx)).await;
        let errors = self.app.close().await;
        assert!(errors.is_empty(), "{errors:?}");
    }
}

/// Prints a line of the report of the run (`--nocapture`), and into the file `GUARD_LOG` names
/// when the test runs in a child process.
fn say(line: &str) {
    println!("{line}");
    if let Some(path) = std::env::var_os("GUARD_LOG") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
}

/// The state of the screen after the command `label`, which is `Shown` unless the command is the
/// screen's own way out (its own keys: `q` and `Enter` of the diff, `Esc` after a `gw` the hex
/// view doesn't know...), makes the text window take over (the page's edit keys, a terminal's
/// view mode) or hides it.
fn after(kind: Kind, focused: bool, label: &str) -> State {
    match (kind, focused, label) {
        // `/` is the diff's own too, and its `Enter` opens the file; `q` closes it
        (Kind::Diff, true, "/a" | "gw" | "macro") => State::Gone,
        (Kind::Hex, true, "gw") => State::Gone,
        // the edit keys run on the source, and the page goes
        (Kind::Markdown, true, "\"ap" | "U") => State::Gone,
        // a key that moves or searches starts the terminal's view mode: a text window of its
        // lines over the terminal's own window
        (Kind::Terminal, true, "/a" | "n" | "*" | "macro" | "\"ay") => State::Hidden,
        // `Esc` and `q` hide the panel
        (Kind::DebugPanel, true, "gA" | "gw" | "macro") => State::Hidden,
        _ => State::Shown,
    }
}

/// The problems (errors and warnings) the status line shows in the whole run: `(always,
/// sometimes)`. The test terminal has no true color, and a.txt has no language server; the rest
/// are the screens' own answers to keys of the list (a search, a macro).
///
/// `No more matches` is `sometimes`: whether `n` finds a match in `a.txt` depends on where the
/// cursor is after the drift the list repairs, which a background reload can change.
fn messages(kind: Kind, focused: bool) -> (Vec<&'static str>, Vec<&'static str>) {
    let lsp = "'lsp-restart': LSP not defined for the current document";
    let theme = "'theme': Unsupported theme: theme requires true color support";
    let more = "No more matches";
    let always = match (kind, focused) {
        (Kind::Hex, true) => vec![
            lsp,
            theme,
            "an odd number of hex digits: a byte has two (0a, not a)",
        ],
        // (the page has a language server for the Markdown file: no error)
        (Kind::Markdown, true) => vec![theme, "No matches"],
        (Kind::Terminal, true) => vec![lsp, theme, "Register [@] empty"],
        (Kind::DebugPanel, true) => vec![
            lsp,
            theme,
            "This debug adapter steps by line only (no stepping granularity)",
        ],
        _ => vec![lsp, theme],
    };
    (always, vec![more])
}

/// What a command that closes things does, run alone in a fresh editor.
#[derive(Debug, PartialEq, Eq)]
struct Isolated {
    state: State,
    /// `a.txt` is still open.
    a_open: bool,
    /// The windows after: the least and the most.
    windows: (usize, usize),
}

/// Runs `keys` once, in a fresh editor with the screen open, the focus as the case has it (in
/// the window `b`, if `in_b`), and records what it did.
async fn isolated(kind: Kind, focused: bool, dir: &Path, label: &str, keys: &str, in_b: bool) {
    let mut rig = Rig::new(kind, focused, dir).await;
    if in_b {
        let b_view = rig.b_view;
        assert!(
            rig.app.editor.tree.is_visible(b_view),
            "{label}: b is shown"
        );
        rig.app.editor.focus(b_view);
    }
    let before = rig.shown();
    rig.app.editor.clear_status();
    rig.send(keys).await;
    // (a shell that was started takes a moment to say anything)
    rig.pump(Duration::from_millis(200)).await;
    let status = rig.status().map(|(message, severity)| {
        format!(
            "{severity:?}: {}",
            message.replace(dir.to_str().unwrap(), "<dir>")
        )
    });
    let windows = rig.shown();
    let observed = Isolated {
        state: rig.state(),
        a_open: rig.a_text_now().is_some(),
        windows: (windows, windows),
    };
    say(&format!(
        "{kind:?} {}: {label:<24} (alone{}) focus: {:<22} screen: {:?} a.txt open: {} windows: {before} -> {} status: {}",
        if focused { "focused  " } else { "unfocused" },
        if in_b { ", in b" } else { "" },
        rig.focus_name(),
        observed.state,
        observed.a_open,
        observed.windows.0,
        status.as_deref().unwrap_or("-").replace('\n', "\\n"),
    ));
    // the invariants, with what the command did to `a.txt` taken in
    if !observed.a_open {
        rig.a_text = None;
    }
    rig.check(label);
    let expected = alone(kind, focused, label);
    assert_eq!(
        (observed.state, observed.a_open),
        (expected.state, expected.a_open),
        "{label}: what it did"
    );
    assert!(
        (expected.windows.0..=expected.windows.1).contains(&windows),
        "{label}: {windows} windows after, expected {:?}",
        expected.windows
    );
    // (the shell the command may have started goes with the editor)
    rig.finish().await;
}

/// What the commands that close things do to the layout of `Rig::new` (`a | b`, the screen over
/// `b` or in a window of its own), each alone.
///
/// - `:bc` closes the focused buffer: on a screen the screen, in a text window the buffer
///   (`a.txt`, which closes its window);
/// - `:bco` closes the text buffers but the one of the last focused text window (with a screen
///   focused: the one it covers, or `b`), and never a screen;
/// - `:q` and `C-w q` close the focused window and what it covers (a panel is hidden, not closed).
fn alone(kind: Kind, focused: bool, label: &str) -> Isolated {
    let covers = kind.covers();
    let (state, a_open, windows) = match (focused, label) {
        // the screen is closed, `b` is uncovered (or the terminal's window goes)
        (true, ":bc") => (State::Gone, true, 2),
        (true, ":bco") => (State::Shown, false, if covers { 1 } else { 2 }),
        (true, ":q" | "C-w q") => match kind {
            Kind::DebugPanel => (State::Hidden, true, 2),
            Kind::Terminal => (State::Gone, true, 2),
            _ if label == "C-w q" && kind == Kind::Diff => {
                // the keys after `C-w` may be the diff's own (`q` closes the diff only, and
                // `b` stays): either
                return Isolated {
                    state: State::Gone,
                    a_open: true,
                    windows: (1, 2),
                };
            }
            _ => (State::Gone, true, 1),
        },
        // `a.txt` has the focus
        (false, ":bc") => (State::Shown, false, if covers { 1 } else { 2 }),
        (false, ":bco") => (State::Shown, true, if covers { 2 } else { 3 }),
        (false, ":q" | "C-w q") => (State::Shown, true, if covers { 1 } else { 2 }),
        // a terminal for the command, shown beside what there is
        (_, ":sh echo hi") => (State::Shown, true, if covers { 3 } else { 4 }),
        // in `b`'s window (a text window here)
        (_, ":bc in b") => (State::Shown, true, 3),
        (_, ":q in b" | "C-w q in b") => (State::Shown, true, 2),
        _ => panic!("no expectation for {label}"),
    };
    Isolated {
        state,
        a_open,
        windows: (windows, windows),
    }
}

/// Runs every command with the screen `kind` open, the screen focused or not.
///
/// `dir` has `a.txt` and `b.txt` (`b.md` for the page).
async fn run(kind: Kind, focused: bool, dir: &Path) {
    let mut rig = Rig::new(kind, focused, dir).await;
    rig.check("the setup");
    assert_eq!(rig.state(), State::Shown);
    say(&format!(
        "{kind:?} {}: layout of {} windows, focus: {}",
        if focused { "focused" } else { "unfocused" },
        rig.shown(),
        rig.focus_name()
    ));

    for (label, keys) in [
        (":w", ":w<ret>"),
        (":wa", ":wa<ret>"),
        (":u", ":u<ret>"),
        (":update", ":update<ret>"),
        (":reload", ":reload<ret>"),
        (":reload-all", ":reload-all<ret>"),
        (":write-all", ":write-all<ret>"),
        (":bn", ":bn<ret>"),
        (":bp", ":bp<ret>"),
        (":buffer-next", ":buffer-next<ret>:buffer-previous<ret>"),
        ("space b", "<space>b<esc>"),
        ("space f", "<space>f<esc>"),
        ("gA", "gA<esc>"),
        ("/a", "/a<ret>"),
        ("n", "n"),
        ("*", "*"),
        ("gw", "gw<esc>"),
        ("macro", "QlQq"),
        (":lsp-restart", ":lsp-restart<ret>"),
        (":theme default", ":theme default<ret>"),
        (":set scrolloff 3", ":set scrolloff 3<ret>"),
        (":cd .", ":cd .<ret>"),
    ] {
        rig.command(label, keys).await;
    }
    rig.registers_and_undo().await;
    rig.split("C-w s", "<C-w>s", "<C-w>q").await;
    rig.split("C-w v", "<C-w>v", "<C-w>q").await;
    rig.split(":vsplit", ":vsplit<ret>", ":q<ret>").await;
    rig.split(":hsplit", ":hsplit<ret>", ":q<ret>").await;
    rig.command("C-w w", "<C-w>w").await;
    rig.command("C-w w", "<C-w>w").await;
    rig.command("C-w w", "<C-w>w").await;

    let problems: BTreeSet<String> = rig.problems.clone();
    let in_b = !kind.covers();
    rig.finish().await;
    say(&format!("PROBLEMS {kind:?} {focused} {problems:#?}"));
    let (always, sometimes) = messages(kind, focused);
    let always: BTreeSet<String> = always.into_iter().map(str::to_string).collect();
    let allowed: BTreeSet<String> = always
        .iter()
        .cloned()
        .chain(sometimes.into_iter().map(str::to_string))
        .collect();
    assert!(
        always.is_subset(&problems) && problems.is_subset(&allowed),
        "the problems of the status line: {problems:#?}, expected {always:#?} and at most {allowed:#?}"
    );

    // the commands that close things, each in a fresh editor
    for (label, keys) in [
        (":bc", ":bc<ret>"),
        (":bco", ":bco<ret>"),
        (":q", ":q<ret>"),
        ("C-w q", "<C-w>q"),
        (":sh echo hi", ":sh echo hi<ret>"),
    ] {
        isolated(kind, focused, dir, label, keys, false).await;
    }
    if in_b {
        // the buffer of the other window, which is a text window here
        for (label, keys) in [(":bc", ":bc<ret>"), (":q", ":q<ret>"), ("C-w q", "<C-w>q")] {
            isolated(kind, focused, dir, &format!("{label} in b"), keys, true).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_terminal_focused() {
    let (_dir, path) = plain_dir();
    run(Kind::Terminal, true, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_terminal_unfocused() {
    let (_dir, path) = plain_dir();
    run(Kind::Terminal, false, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_hex_view_focused() {
    let (_dir, path) = plain_dir();
    run(Kind::Hex, true, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_hex_view_unfocused() {
    let (_dir, path) = plain_dir();
    run(Kind::Hex, false, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_markdown_page_focused() {
    let (_dir, path) = plain_dir();
    run(Kind::Markdown, true, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_markdown_page_unfocused() {
    let (_dir, path) = plain_dir();
    run(Kind::Markdown, false, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_the_debug_panel_focused() {
    let (_dir, path) = plain_dir();
    run(Kind::DebugPanel, true, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_the_debug_panel_unfocused() {
    let (_dir, path) = plain_dir();
    run(Kind::DebugPanel, false, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_diff_focused() {
    let Some(path) = Repo::enter_with(module_path!(), "commands_with_a_diff_focused", repo) else {
        return;
    };
    run(Kind::Diff, true, &path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_with_a_diff_unfocused() {
    let Some(path) = Repo::enter_with(module_path!(), "commands_with_a_diff_unfocused", repo)
    else {
        return;
    };
    run(Kind::Diff, false, &path).await;
}

// The bugs the guard found, fixed: each keeps its own test.

/// Whether a picker (a frame) is on the screen.
fn picker_is_open(rig: &Rig) -> bool {
    super::git_log::screen_rows(&rig.app)
        .iter()
        .any(|row| row.contains('┌'))
}

/// `Space b` and `Space f` on a focused screen open the pickers (FORK.md, "The keys on a screen").
async fn pickers_open(rig: &mut Rig) {
    for keys in ["<space>b", "<space>f"] {
        rig.send(keys).await;
        assert!(picker_is_open(rig), "{keys} opens a picker");
        rig.send("<esc>").await;
        assert!(!picker_is_open(rig), "Esc closes the picker");
        assert_eq!(rig.state(), State::Shown, "Esc closed the picker only");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pickers_open_from_a_focused_diff() {
    let Some(path) = Repo::enter_with(module_path!(), "pickers_open_from_a_focused_diff", repo)
    else {
        return;
    };
    let mut rig = Rig::new(Kind::Diff, true, &path).await;
    pickers_open(&mut rig).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pickers_open_from_the_focused_debug_panel() {
    let (_dir, path) = plain_dir();
    let mut rig = Rig::new(Kind::DebugPanel, true, &path).await;
    pickers_open(&mut rig).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_register_is_not_left_selected_by_the_hex_view() {
    let (_dir, path) = plain_dir();
    let mut rig = Rig::new(Kind::Hex, true, &path).await;
    rig.send("\"ap").await;
    assert_eq!(rig.app.editor.selected_register, None);
    let before = rig.shown();
    rig.send("<C-w>s").await;
    assert_eq!(rig.shown(), before + 1, "C-w s splits");
    rig.finish().await;
}
