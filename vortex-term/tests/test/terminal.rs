//! The terminals as windows (`C-t`, `:sh`, the window menu, the modes, view mode, the `vx`
//! shim), against a real shell on a pty.
//!
//! The shell is `sh` so rc files don't matter. The shell's work happens on other threads and
//! reaches the editor through the job queue, so the tests pump the event loop until a condition
//! holds, with a deadline, rather than sleeping.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use termina::event::{Event, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;
use vortex_term::{
    application::Application,
    config::Config,
    terminal::shim::{self, OpenFile, Request, Response},
};
use vortex_view::{
    current_ref, doc,
    document::Mode,
    editor::{Action, Config as EditorConfig, Severity},
    graphics::Rect,
    input::parse_macro,
    terminal::{ShellStatus, Terminal, TerminalConfig, TerminalPosition},
    DocumentId, ViewId,
};

use super::*;

const DEADLINE: Duration = Duration::from_secs(10);

fn config_with_shell(shell: &[&str]) -> Config {
    let mut config = test_config();
    config.editor = EditorConfig {
        terminal: TerminalConfig {
            shell: shell.iter().map(|s| s.to_string()).collect(),
            ..TerminalConfig::default()
        },
        ..test_editor_config()
    };
    config
}

/// An application whose input the test controls, and that keeps running between steps.
struct Harness {
    app: Application,
    tx: UnboundedSender<io::Result<Event>>,
    rx: UnboundedReceiverStream<io::Result<Event>>,
    running: bool,
}

impl Harness {
    fn new(builder: AppBuilder) -> anyhow::Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            app: builder.build()?,
            tx,
            rx: UnboundedReceiverStream::new(rx),
            running: true,
        })
    }

    fn with_shell(shell: &[&str]) -> anyhow::Result<Self> {
        Self::new(AppBuilder::new().with_config(config_with_shell(shell)))
    }

    fn sh() -> anyhow::Result<Self> {
        Self::with_shell(&["sh"])
    }

    /// Runs the event loop for a short while: until it is idle, at most `limit`.
    async fn step(&mut self, limit: Duration) {
        if !self.running {
            return;
        }
        let idle = tokio::time::timeout(limit, self.app.event_loop_until_idle(&mut self.rx));
        if let Ok(still_running) = idle.await {
            self.running = still_running;
        }
    }

    /// Sends the keys (`parse_macro` syntax) and lets the editor process them.
    async fn keys(&mut self, keys: &str) -> anyhow::Result<()> {
        for key in parse_macro(keys)? {
            self.tx.send(Ok(Event::Key(KeyEvent::from(key))))?;
        }
        self.step(Duration::from_secs(3)).await;
        Ok(())
    }

    /// Runs the event loop until `cond` holds. Panics with `what` after the deadline.
    async fn until(&mut self, what: &str, cond: impl Fn(&Application) -> bool) {
        let start = Instant::now();
        loop {
            if cond(&self.app) {
                return;
            }
            assert!(
                start.elapsed() < DEADLINE,
                "timed out waiting for: {what}\nstatus: {:?}\nscreen:\n{}",
                self.app.editor.get_status(),
                screen(&self.app)
            );
            self.step(Duration::from_millis(100)).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Runs the event loop until `future` (a blocking call on another thread) is done.
    async fn await_blocking<T: Send + 'static>(
        &mut self,
        what: &str,
        handle: tokio::task::JoinHandle<T>,
    ) -> T {
        let start = Instant::now();
        while !handle.is_finished() {
            assert!(
                start.elapsed() < DEADLINE,
                "timed out waiting for: {what}\nstatus: {:?}\nscreen:\n{}",
                self.app.editor.get_status(),
                screen(&self.app)
            );
            self.step(Duration::from_millis(100)).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.await.unwrap()
    }

    /// Runs the event loop for `duration` (to check that something does *not* happen).
    async fn pump_for(&mut self, duration: Duration) {
        let start = Instant::now();
        while start.elapsed() < duration {
            self.step(Duration::from_millis(50)).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn finish(mut self) -> anyhow::Result<()> {
        if self.running {
            // closes the shell too: it is dropped with the editor
            for key in parse_macro("<esc>:q!<ret>")? {
                self.tx.send(Ok(Event::Key(KeyEvent::from(key))))?;
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), self.app.event_loop(&mut self.rx))
                .await;
        }
        let errs = self.app.close().await;
        anyhow::ensure!(errs.is_empty(), "errors closing the app: {errs:?}");
        Ok(())
    }
}

/// The scratch terminal, while its shell runs.
fn scratch(app: &Application) -> Option<&Terminal> {
    app.editor.terminals.scratch()
}

/// The scratch terminal's window, while it is shown.
fn scratch_view(app: &Application) -> Option<ViewId> {
    app.editor.view_of(app.editor.terminals.scratch?)
}

/// The focused window's terminal document, if it is one.
fn focused_terminal(app: &Application) -> Option<DocumentId> {
    let doc = app.editor.tree.get(app.editor.tree.focus).doc;
    app.editor.terminals.is_terminal(doc).then_some(doc)
}

/// The terminal shown in the focused window, else the scratch one.
fn terminal(app: &Application) -> Option<&Terminal> {
    focused_terminal(app)
        .and_then(|doc| app.editor.terminals.get(doc))
        .or_else(|| scratch(app))
}

/// The text of a terminal's screen, rows separated by newlines.
fn screen_of(terminal: Option<&Terminal>) -> String {
    let Some(terminal) = terminal else {
        return String::from("<no shell>");
    };
    let mut rows: Vec<String> = Vec::new();
    terminal.shell.visit_cells(|row, _col, cell| {
        while rows.len() <= row as usize {
            rows.push(String::new());
        }
        let text = if cell.text.is_empty() {
            " "
        } else {
            cell.text.as_str()
        };
        rows[row as usize].push_str(text);
    });
    rows.iter()
        .map(|row| row.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of the focused (else the scratch) terminal's screen.
fn screen(app: &Application) -> String {
    screen_of(terminal(app))
}

fn screen_contains(app: &Application, needle: &str) -> bool {
    screen(app).contains(needle)
}

/// A line that ends in `sh`'s prompt: `$` from dash, `sh-3.2$` from the bash that is `sh` on
/// macOS (`#` for root).
fn has_prompt(app: &Application) -> bool {
    screen(app)
        .lines()
        .any(|line| line.trim_end().ends_with(['$', '#']))
}

fn shell_running(app: &Application) -> bool {
    scratch(app).is_some()
}

/// Opens the scratch terminal and waits until the shell has drawn its prompt.
async fn open_panel(h: &mut Harness) -> anyhow::Result<()> {
    h.keys("<C-t>").await?;
    h.until("the shell's prompt", has_prompt).await;
    Ok(())
}

/// Reads the shim's socket path out of the shell's environment (typed in insert mode).
async fn socket_path(h: &mut Harness, dir: &Path) -> anyhow::Result<PathBuf> {
    let out = dir.join("socket");
    h.keys(&format!(
        "printenv VORTEX_TERMINAL_SOCKET <gt> {}<ret>",
        out.display()
    ))
    .await?;
    let read = || std::fs::read_to_string(&out).unwrap_or_default();
    let start = Instant::now();
    while !read().ends_with('\n') {
        assert!(start.elapsed() < DEADLINE, "no socket path written");
        h.step(Duration::from_millis(50)).await;
    }
    Ok(PathBuf::from(read().trim()))
}

async fn send(socket: PathBuf, request: Request) -> tokio::task::JoinHandle<io::Result<Response>> {
    tokio::task::spawn_blocking(move || shim::send(&socket, &request))
}

fn in_editor(app: &Application) -> bool {
    focused_terminal(app).is_none()
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_t_shows_and_hides_the_scratch_terminal() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    assert!(scratch(&h.app).is_none());
    let editor_view = h.app.editor.tree.focus;

    h.keys("<C-t>").await?;
    let view = scratch_view(&h.app).expect("shown");
    assert_eq!(h.app.editor.tree.focus, view, "focused");
    assert_eq!(h.app.editor.mode(), Mode::Insert, "lands in insert mode");
    h.until("the shell's prompt", has_prompt).await;
    let pid = scratch(&h.app).unwrap().shell.pid();

    // from its normal mode, C-t hides it; the shell keeps running
    h.keys("<esc><C-t>").await?;
    assert!(scratch_view(&h.app).is_none(), "hidden");
    assert_eq!(h.app.editor.tree.focus, editor_view);
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert_eq!(scratch(&h.app).unwrap().shell.pid(), pid);

    // shown again, with the same shell, where it was
    h.keys("<C-t>").await?;
    assert!(scratch_view(&h.app).is_some());
    assert_eq!(scratch(&h.app).unwrap().shell.pid(), pid);
    assert!(focused_terminal(&h.app).is_some());
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_t_hides_it_from_another_window() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    // back to the editor's window with a window motion
    h.keys("<esc><C-w>k").await?;
    assert!(in_editor(&h.app));
    assert!(scratch_view(&h.app).is_some(), "still shown");
    h.keys("<C-t>").await?;
    assert!(scratch_view(&h.app).is_none(), "hidden");
    assert!(in_editor(&h.app));
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_mode_keys_go_to_the_shell() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let doc_text = doc!(h.app.editor).text().to_string();

    // `i`, `dd` and `x` would change a document; C-w and C-t are the shell's too
    h.keys("ixdd typed-text").await?;
    h.until("the typed text on the screen", |app| {
        screen_contains(app, "ixdd typed-text")
    })
    .await;
    assert_eq!(doc!(h.app.editor).text().to_string(), doc_text);
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.keys("<C-w><C-t>").await?;
    h.until("C-w erased a word in the shell", |app| {
        screen_contains(app, "ixdd") && !screen_contains(app, "typed-text")
    })
    .await;
    assert!(scratch_view(&h.app).is_some(), "C-t went to the shell");
    assert!(focused_terminal(&h.app).is_some());
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_goes_to_normal_mode_then_to_the_program() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    // a program that prints each byte it reads, one at a time
    h.keys("stty raw -echo; while :; do dd bs=1 count=1 2<gt>/dev/null | od -An -c; done<ret>")
        .await?;
    h.pump_for(Duration::from_millis(500)).await;
    h.keys("<esc>").await?;
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(focused_terminal(&h.app).is_some(), "the window stays");
    // the Esc that left insert mode didn't reach the program
    h.pump_for(Duration::from_millis(300)).await;
    assert_eq!(
        screen(&h.app).matches("033").count(),
        0,
        "{}",
        screen(&h.app)
    );
    // in normal mode, Esc goes through, as many times as pressed; the mode doesn't change
    h.keys("<esc><esc><esc>").await?;
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(scratch_view(&h.app).is_some());
    h.until("three Esc read by the program", |app| {
        screen(app).matches("033").count() == 3
    })
    .await;
    // `i` types again
    h.keys("iz").await?;
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.until("z read by the program", |app| {
        screen(app).lines().any(|line| line.trim() == "z")
    })
    .await;
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn sh_command_runs_in_the_scratch_terminal() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    h.keys(":sh echo hi-$((1+1))<ret>").await?;
    assert!(scratch_view(&h.app).is_some());
    assert!(focused_terminal(&h.app).is_some());
    h.until("the output", |app| {
        screen(app).lines().any(|line| line.trim() == "hi-2")
    })
    .await;

    // the long name, and no command: just shows it
    h.keys("<esc><C-t>").await?;
    assert!(scratch_view(&h.app).is_none());
    h.keys(":run-shell-command<ret>").await?;
    assert!(focused_terminal(&h.app).is_some());
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_closes_the_window() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    h.keys("exit<ret>").await?;
    h.until("the window to close", |app| !shell_running(app))
        .await;
    assert!(scratch_view(&h.app).is_none());
    assert!(in_editor(&h.app));
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(h.app.editor.documents().all(|doc| !doc.is_screen()));

    // a fresh shell
    h.keys("<C-t>").await?;
    assert!(scratch(&h.app).is_some() && focused_terminal(&h.app).is_some());
    h.until("the new prompt", has_prompt).await;
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn sh_exit_closes_the_window() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    h.keys(":sh exit<ret>").await?;
    h.until("the window to close", |app| !shell_running(app))
        .await;
    assert!(scratch_view(&h.app).is_none());

    h.keys(":sh echo again<ret>").await?;
    assert!(focused_terminal(&h.app).is_some());
    h.until("the output of the new shell", |app| {
        screen(app).lines().any(|line| line.trim() == "again")
    })
    .await;
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn shim_focus_focuses_the_editor_window() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let socket = socket_path(&mut h, dir.path()).await?;
    assert!(focused_terminal(&h.app).is_some());

    let call = send(socket, Request::Focus).await;
    let response = h.await_blocking("the answer to Focus", call).await?;
    assert_eq!(response, Response::Done);
    assert!(in_editor(&h.app));
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(scratch_view(&h.app).is_some(), "the terminal stays shown");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn shim_open_goes_to_line_and_column() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\ntwo three\nfour\n")?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let socket = socket_path(&mut h, dir.path()).await?;

    let request = Request::Open {
        files: vec![OpenFile {
            path: file.clone(),
            line: Some(2),
            column: Some(3),
        }],
        wait: false,
    };
    let call = send(socket, request).await;
    let response = h.await_blocking("the answer to Open", call).await?;
    assert_eq!(response, Response::Done);

    let editor = &h.app.editor;
    // in the window for documents, which has the focus
    assert!(in_editor(&h.app));
    assert!(scratch_view(&h.app).is_some());
    let (view, document) = current_ref!(editor);
    assert_eq!(
        document.path().map(|p| p.canonicalize().unwrap()),
        Some(file.canonicalize()?)
    );
    let text = document.text().slice(..);
    let cursor = document.selection(view.id).primary().cursor(text);
    let line = text.char_to_line(cursor);
    assert_eq!((line, cursor - text.line_to_char(line)), (1, 2));
    assert!(editor.terminals.waiters.is_empty());
    h.finish().await
}

/// `vx --wait`: the client is answered when the buffer is closed.
async fn open_and_wait(
    h: &mut Harness,
    dir: &Path,
) -> anyhow::Result<(PathBuf, tokio::task::JoinHandle<io::Result<Response>>)> {
    let file = dir.join("msg.txt");
    std::fs::write(&file, "message\n")?;
    let socket = socket_path(h, dir).await?;
    let request = Request::Open {
        files: vec![OpenFile {
            path: file.clone(),
            line: None,
            column: None,
        }],
        wait: true,
    };
    let call = send(socket, request).await;
    h.until("the file to open", |app| {
        app.editor.terminals.waiters.len() == 1
    })
    .await;
    // nothing answers while the buffer is open
    h.pump_for(Duration::from_millis(300)).await;
    assert!(!call.is_finished(), "answered before the buffer closed");
    assert!(scratch_view(&h.app).is_some());
    assert!(in_editor(&h.app));
    assert!(doc!(h.app.editor).path().is_some());
    Ok((file, call))
}

#[tokio::test(flavor = "multi_thread")]
async fn shim_wait_answers_when_the_buffer_closes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let (file, call) = open_and_wait(&mut h, dir.path()).await?;

    h.keys(":q<ret>").await?;
    let response = h.await_blocking("the answer after :q", call).await?;
    assert_eq!(response, Response::Done);
    assert!(h.running, "`:q` on the awaited buffer quit the editor");
    assert!(focused_terminal(&h.app).is_some(), "back in the terminal");
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    assert!(h.app.editor.terminals.waiters.is_empty());
    assert!(h.app.editor.documents().all(|d| d.path() != Some(&file)));
    assert_eq!(std::fs::read_to_string(&file)?, "message\n");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn shim_wait_answers_after_wq() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let (file, call) = open_and_wait(&mut h, dir.path()).await?;

    h.keys("A!<esc>:wq<ret>").await?;
    let response = h.await_blocking("the answer after :wq", call).await?;
    assert_eq!(response, Response::Done);
    assert!(h.running);
    assert!(focused_terminal(&h.app).is_some());
    assert_eq!(std::fs::read_to_string(&file)?, "message!\n");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn shim_wait_answers_after_force_quit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let (file, call) = open_and_wait(&mut h, dir.path()).await?;

    h.keys("A!<esc>:q!<ret>").await?;
    let response = h.await_blocking("the answer after :q!", call).await?;
    assert_eq!(response, Response::Done);
    assert!(h.running);
    assert!(focused_terminal(&h.app).is_some());
    assert_eq!(std::fs::read_to_string(&file)?, "message\n");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn status_of_the_last_command() -> anyhow::Result<()> {
    if std::process::Command::new("bash")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("bash is missing, skipping");
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    h.keys("<C-t>").await?;
    h.until("the prompt", |app| {
        scratch(app).is_some_and(|terminal| {
            let mut any = false;
            terminal
                .shell
                .visit_cells(|_, _, cell| any |= !cell.text.trim().is_empty());
            any
        })
    })
    .await;

    h.keys("false<ret><esc><C-t>").await?;
    assert!(scratch_view(&h.app).is_none());
    h.until("the status to be Finished(1)", |app| {
        scratch(app).map(|t| t.shell.status()) == Some(ShellStatus::Finished(1))
    })
    .await;

    // seeing the terminal again acknowledges the result
    h.keys("<C-t>").await?;
    assert_eq!(scratch(&h.app).unwrap().shell.status(), ShellStatus::Idle);
    h.finish().await
}

async fn popup_command(command: &str) -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    h.keys(command).await?;
    h.until(
        "the command to run",
        |app| matches!(app.editor.get_status(), Some((msg, _)) if msg.as_ref() == "Command run"),
    )
    .await;
    assert!(scratch(&h.app).is_none(), "{command} started the shell");
    assert!(in_editor(&h.app));
    h.finish().await
}

/// `:!` and `:shell-popup` keep their old behaviour: a popup, not a terminal.
#[tokio::test(flavor = "multi_thread")]
async fn bang_without_space_is_a_popup() -> anyhow::Result<()> {
    popup_command(":!echo popup<ret>").await
}

#[tokio::test(flavor = "multi_thread")]
async fn bang_with_space_is_a_popup() -> anyhow::Result<()> {
    popup_command(":! echo popup<ret>").await
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_popup_is_a_popup() -> anyhow::Result<()> {
    popup_command(":shell-popup echo popup<ret>").await
}

fn bash() -> bool {
    std::process::Command::new("bash")
        .arg("--version")
        .output()
        .is_ok()
}

/// The text of the focused terminal's document (view mode's lines) and its primary selection.
fn view_selection(app: &Application) -> (String, String) {
    let (view, doc) = current_ref!(app.editor);
    let text = doc.text().slice(..);
    let range = doc.selection(view.id).primary();
    (
        text.to_string(),
        text.slice(range.from()..range.to()).to_string(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_view_mode_prompts_and_edit() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    h.keys("<C-t>").await?;
    h.until("the markers", |app| {
        scratch(app).is_some_and(|t| t.shell.has_vortex_integration())
    })
    .await;
    h.keys("echo first-out<ret>").await?;
    h.until("first", |app| screen(app).lines().any(|l| l == "first-out"))
        .await;
    h.keys("printf 'a\\nb\\n'<ret>").await?;
    h.until("b", |app| screen(app).lines().any(|l| l == "b"))
        .await;
    h.pump_for(Duration::from_millis(300)).await;
    // normal mode, then a motion starts view mode
    h.keys("<esc>k").await?;
    assert!(
        vortex_term::terminal::viewing(&h.app.editor),
        "k starts view mode"
    );
    // [ to the previous prompt, o selects its output
    h.keys("[o").await?;
    let (text, selected) = view_selection(&h.app);
    assert_eq!(selected, "a\nb\n", "{text}");
    // from inside an output, `[` is its own prompt, then the one before
    h.keys("[[o").await?;
    assert_eq!(view_selection(&h.app).1, "first-out\n");
    h.keys("y").await?;
    assert_eq!(
        h.app
            .editor
            .registers
            .first('"', &h.app.editor)
            .map(|s| s.to_string()),
        Some("first-out\n".to_string())
    );
    // Esc back to normal mode, live
    h.keys("<esc>").await?;
    assert!(!vortex_term::terminal::viewing(&h.app.editor));
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    // edit the command line
    h.keys("iecho half<esc>e").await?;
    h.until("the edit buffer", in_editor).await;
    assert_eq!(doc!(h.app.editor).text().to_string(), "echo half");
    h.keys("A-done<esc>:wq<ret>").await?;
    h.until("back in the terminal with the line", |app| {
        focused_terminal(app).is_some() && screen_contains(app, "echo half-done")
    })
    .await;
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.keys("<ret>").await?;
    h.until("run", |app| screen(app).lines().any(|l| l == "half-done"))
        .await;
    h.finish().await
}

// ---------------------------------------------------------------------------------------------
// Helpers for the tests below
// ---------------------------------------------------------------------------------------------

fn config_at(shell: &[&str], position: TerminalPosition, size: u16) -> Config {
    let mut config = test_config();
    config.editor = EditorConfig {
        terminal: TerminalConfig {
            shell: shell.iter().map(|s| s.to_string()).collect(),
            position,
            size,
            ..TerminalConfig::default()
        },
        ..test_editor_config()
    };
    config
}

fn windows(app: &Application) -> usize {
    app.editor.tree.views().count()
}

fn area_of(app: &Application, view: ViewId) -> Rect {
    app.editor.tree.get(view).area
}

fn near(actual: u16, expected: f64, tolerance: f64) -> bool {
    (actual as f64 - expected).abs() <= tolerance
}

/// The process is gone (or only a zombie).
fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

/// The text of the line of the primary cursor of the focused document.
fn cursor_line_text(app: &Application) -> String {
    let (view, doc) = current_ref!(app.editor);
    let text = doc.text().slice(..);
    let line = doc.selection(view.id).primary().cursor_line(text);
    text.line(line).to_string()
}

fn cursor_line(app: &Application) -> usize {
    let (view, doc) = current_ref!(app.editor);
    let text = doc.text().slice(..);
    doc.selection(view.id).primary().cursor_line(text)
}

fn status_text(app: &Application) -> String {
    app.editor
        .get_status()
        .map(|(message, _)| message.to_string())
        .unwrap_or_default()
}

fn error_text(app: &Application) -> Option<String> {
    app.editor
        .get_status()
        .filter(|(_, severity)| **severity == Severity::Error)
        .map(|(message, _)| message.to_string())
}

fn yanked(app: &Application) -> Option<String> {
    app.editor
        .registers
        .first('"', &app.editor)
        .map(|s| s.to_string())
}

/// Waits for the first prompt of a bash with Vortex's integration, then for the screen to settle.
async fn open_bash(h: &mut Harness) -> anyhow::Result<()> {
    h.keys("<C-t>").await?;
    h.until("the markers", |app| {
        scratch(app).is_some_and(|t| t.shell.has_vortex_integration())
    })
    .await;
    h.pump_for(Duration::from_millis(300)).await;
    Ok(())
}

/// Runs `command` in the focused terminal (insert mode) and waits for a line `out` on its screen.
async fn run_and_wait(h: &mut Harness, command: &str, out: &str) -> anyhow::Result<()> {
    h.keys(command).await?;
    h.keys("<ret>").await?;
    h.until(out, |app| screen(app).lines().any(|l| l == out))
        .await;
    h.pump_for(Duration::from_millis(200)).await;
    Ok(())
}

// 1. The scratch terminal's place and size

#[tokio::test(flavor = "multi_thread")]
async fn scratch_terminal_at_the_bottom_by_default() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    let editor_view = h.app.editor.tree.focus;
    let tree = h.app.editor.tree.area();
    open_panel(&mut h).await?;
    let view = scratch_view(&h.app).expect("shown");
    let area = area_of(&h.app, view);
    assert_eq!(area.x, tree.x, "full width: {area:?} in {tree:?}");
    assert_eq!(area.width, tree.width, "full width: {area:?} in {tree:?}");
    assert_eq!(
        area.y + area.height,
        tree.y + tree.height,
        "at the bottom: {area:?} in {tree:?}"
    );
    assert!(
        near(area.height, tree.height as f64 * 0.33, 3.0),
        "about 33%: {area:?} in {tree:?}"
    );
    // the editor window is above it
    let above = area_of(&h.app, editor_view);
    assert_eq!(above.y, tree.y);
    assert!(above.y + above.height <= area.y);
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn scratch_terminal_on_the_right() -> anyhow::Result<()> {
    let mut h = Harness::new(AppBuilder::new().with_config(config_at(
        &["sh"],
        TerminalPosition::Right,
        40,
    )))?;
    let tree = h.app.editor.tree.area();
    h.keys(":sh<ret>").await?;
    h.until("the shell's prompt", has_prompt).await;
    let view = scratch_view(&h.app).expect("shown");
    assert_eq!(h.app.editor.tree.focus, view);
    let area = area_of(&h.app, view);
    assert_eq!(area.y, tree.y, "full height: {area:?} in {tree:?}");
    assert_eq!(
        area.height, tree.height,
        "full height: {area:?} in {tree:?}"
    );
    assert_eq!(
        area.x + area.width,
        tree.x + tree.width,
        "the right edge: {area:?} in {tree:?}"
    );
    assert!(
        near(area.width, tree.width as f64 * 0.40, 3.0),
        "about 40%: {area:?} in {tree:?}"
    );
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn scratch_terminal_at_the_top() -> anyhow::Result<()> {
    let mut h =
        Harness::new(AppBuilder::new().with_config(config_at(&["sh"], TerminalPosition::Top, 50)))?;
    let tree = h.app.editor.tree.area();
    h.keys(":sh<ret>").await?;
    h.until("the shell's prompt", has_prompt).await;
    let area = area_of(&h.app, scratch_view(&h.app).expect("shown"));
    assert_eq!(area.y, tree.y, "at the top: {area:?} in {tree:?}");
    assert_eq!(area.x, tree.x);
    assert_eq!(area.width, tree.width, "full width: {area:?} in {tree:?}");
    assert!(
        near(area.height, tree.height as f64 * 0.50, 3.0),
        "about 50%: {area:?} in {tree:?}"
    );
    h.finish().await
}

// 2. Hiding and showing, window motions

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_t_brings_it_back_at_the_same_place_and_size() -> anyhow::Result<()> {
    let mut h = Harness::new(AppBuilder::new().with_config(config_at(
        &["sh"],
        TerminalPosition::Right,
        40,
    )))?;
    open_panel(&mut h).await?;
    let before = area_of(&h.app, scratch_view(&h.app).unwrap());
    let pid = scratch(&h.app).unwrap().shell.pid();
    let editor_windows = windows(&h.app);

    h.keys("<esc><C-t>").await?;
    assert!(scratch_view(&h.app).is_none());
    assert_eq!(windows(&h.app), editor_windows - 1);
    // the editor window has the whole width again
    assert_eq!(
        area_of(&h.app, h.app.editor.tree.focus).width,
        h.app.editor.tree.area().width
    );
    assert!(shell_running(&h.app), "the shell keeps running");

    h.keys("<C-t>").await?;
    let view = scratch_view(&h.app).expect("shown again");
    assert_eq!(area_of(&h.app, view), before);
    assert_eq!(scratch(&h.app).unwrap().shell.pid(), pid, "the same shell");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn window_motions_go_to_and_from_the_scratch_terminal() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    let editor_view = h.app.editor.tree.focus;
    open_panel(&mut h).await?;
    let view = scratch_view(&h.app).unwrap();
    assert_eq!(h.app.editor.mode(), Mode::Insert);

    // out: normal mode, focus on the editor window
    h.keys("<esc><C-w>k").await?;
    assert_eq!(h.app.editor.tree.focus, editor_view);
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    // in: insert mode, focus on the terminal
    h.keys("<C-w>j").await?;
    assert_eq!(h.app.editor.tree.focus, view);
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    // and from its normal mode too
    h.keys("<esc><C-w>k").await?;
    assert_eq!(h.app.editor.tree.focus, editor_view);
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    h.keys("<C-w>j").await?;
    assert_eq!(h.app.editor.tree.focus, view);
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.finish().await
}

// 3. More terminals

async fn new_terminal_with(keys: &str, expect_split_wider: bool) -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    let before = windows(&h.app);
    assert!(h.app.editor.terminals.scratch.is_none());
    h.keys(keys).await?;
    assert_eq!(windows(&h.app), before + 1, "a window was added");
    let doc = focused_terminal(&h.app).expect("the new terminal is focused");
    assert_ne!(Some(doc), h.app.editor.terminals.scratch, "not the scratch");
    assert_eq!(h.app.editor.terminals.list.len(), 1);
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.until("the new shell's prompt", has_prompt).await;
    let tree = h.app.editor.tree.area();
    let area = area_of(&h.app, h.app.editor.tree.focus);
    if expect_split_wider {
        // beside the editor window
        assert_eq!(area.height, tree.height, "{area:?} in {tree:?}");
        assert!(area.width < tree.width);
    } else {
        // below it
        assert_eq!(area.width, tree.width, "{area:?} in {tree:?}");
        assert!(area.height < tree.height);
    }
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn window_menu_opens_a_terminal_in_a_horizontal_split() -> anyhow::Result<()> {
    new_terminal_with("<C-w>Ts", false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn window_menu_opens_a_terminal_in_a_vertical_split() -> anyhow::Result<()> {
    new_terminal_with("<C-w>Tv", true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_command_opens_a_terminal_in_a_horizontal_split() -> anyhow::Result<()> {
    new_terminal_with(":terminal<ret>", false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn vterminal_command_opens_a_terminal_in_a_vertical_split() -> anyhow::Result<()> {
    new_terminal_with(":vterminal<ret>", true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_t_only_shows_and_hides_the_scratch_terminal() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    h.keys(":terminal<ret>").await?;
    let other = focused_terminal(&h.app).expect("a terminal");
    h.until("its prompt", has_prompt).await;
    let before = windows(&h.app);

    // from the terminal's normal mode, C-t shows the scratch terminal, a new one
    h.keys("<esc><C-t>").await?;
    let scratch_doc = h
        .app
        .editor
        .terminals
        .scratch
        .expect("the scratch terminal");
    assert_ne!(scratch_doc, other);
    assert_eq!(windows(&h.app), before + 1);
    assert!(h.app.editor.view_of(other).is_some(), "the other stays");
    assert_eq!(focused_terminal(&h.app), Some(scratch_doc));
    h.until("the scratch prompt", has_prompt).await;

    // and hides only that one
    h.keys("<esc><C-t>").await?;
    assert!(scratch_view(&h.app).is_none());
    assert_eq!(windows(&h.app), before);
    assert!(h.app.editor.view_of(other).is_some());
    assert_eq!(h.app.editor.terminals.list.len(), 2);
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn q_closes_another_terminal_and_kills_its_shell() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    let before = windows(&h.app);
    h.keys(":terminal<ret>").await?;
    let doc = focused_terminal(&h.app).expect("a terminal");
    h.until("its prompt", has_prompt).await;
    let pid = h
        .app
        .editor
        .terminals
        .get(doc)
        .unwrap()
        .shell
        .pid()
        .unwrap();
    assert!(!process_gone(pid));

    h.keys("<esc>:q<ret>").await?;
    assert!(h.running, "`:q` quit the editor");
    assert_eq!(windows(&h.app), before, "its window is closed");
    assert!(
        h.app.editor.terminals.get(doc).is_none(),
        "the terminal is gone"
    );
    assert!(h.app.editor.terminals.list.is_empty());
    assert!(in_editor(&h.app));
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    let start = Instant::now();
    while !process_gone(pid) {
        assert!(start.elapsed() < DEADLINE, "the shell {pid} still runs");
        h.step(Duration::from_millis(50)).await;
    }
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_w_v_in_a_terminal_opens_another_terminal() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let first = focused_terminal(&h.app).unwrap();
    let before = windows(&h.app);
    h.keys("<esc><C-w>v").await?;
    assert_eq!(windows(&h.app), before + 1);
    let second = focused_terminal(&h.app).expect("a terminal is focused");
    assert_ne!(first, second);
    assert_eq!(h.app.editor.terminals.list.len(), 2);
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.until("the new prompt", has_prompt).await;
    // the first is still shown and still the scratch one
    assert!(h.app.editor.view_of(first).is_some());
    assert_eq!(h.app.editor.terminals.scratch, Some(first));
    h.finish().await
}

// 4. Resizing

#[tokio::test(flavor = "multi_thread")]
async fn splits_resize_the_shell() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let first = focused_terminal(&h.app).unwrap();
    let first_view = h.app.editor.tree.focus;
    let inner_size = |app: &Application| {
        let view = app.editor.tree.get(first_view);
        let doc = app.editor.document(first).unwrap();
        let inner = view.inner_area(doc);
        (inner.height, inner.width)
    };
    let shell_size = |app: &Application| {
        let size = app.editor.terminals.get(first).unwrap().shell.size();
        (size.rows, size.cols)
    };
    h.until("the shell sized to its window", |app| {
        shell_size(app) == inner_size(app)
    })
    .await;
    let (rows, cols) = shell_size(&h.app);
    let tree = h.app.editor.tree.area();
    assert_eq!(cols, tree.width, "full width");

    // a vertical split of the editor window above doesn't change the terminal's width
    h.keys("<esc><C-w>k<C-w>v").await?;
    h.keys("<C-w>j").await?;
    assert_eq!(focused_terminal(&h.app), Some(first));
    h.pump_for(Duration::from_millis(200)).await;
    assert_eq!(shell_size(&h.app), (rows, cols));
    assert_eq!(shell_size(&h.app), inner_size(&h.app));

    // a terminal beside it halves its width, right away
    h.keys("<esc><C-w>Tv").await?;
    let second = focused_terminal(&h.app).expect("a new terminal");
    assert_ne!(second, first);
    let (_, narrow) = shell_size(&h.app);
    assert!(
        near(narrow, cols as f64 / 2.0, 3.0),
        "about half of {cols}: {narrow}"
    );
    assert_eq!(shell_size(&h.app), inner_size(&h.app));
    let sum = area_of(&h.app, first_view).width + area_of(&h.app, h.app.editor.tree.focus).width;
    assert!(sum == cols || sum + 1 == cols, "{sum} of {cols}");

    // and the shell knows: stty reports the same
    h.until("the new prompt", has_prompt).await;
    h.keys("<esc><C-w>h").await?;
    assert_eq!(focused_terminal(&h.app), Some(first));
    let (rows, cols) = shell_size(&h.app);
    h.keys("stty size<ret>").await?;
    let wanted = format!("{rows} {cols}");
    h.until("stty size", |app| {
        screen(app).lines().any(|l| l.trim() == wanted)
    })
    .await;

    // closing the neighbour gives the width back
    h.keys("<esc><C-w>l<esc>:q<ret>").await?;
    h.until("the width back", |app| shell_size(app).1 > cols)
        .await;
    h.finish().await
}

// 5. Keys in insert mode

#[tokio::test(flavor = "multi_thread")]
async fn insert_mode_passes_c_w_and_c_t_to_a_running_program() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let windows_before = windows(&h.app);
    h.keys("stty raw -echo; while :; do dd bs=1 count=1 2<gt>/dev/null | od -An -c; done<ret>")
        .await?;
    h.pump_for(Duration::from_millis(500)).await;
    h.keys("<C-w>").await?;
    h.until("C-w (027) read by the program", |app| {
        screen_contains(app, "027")
    })
    .await;
    h.keys("<C-t>").await?;
    h.until("C-t (024) read by the program", |app| {
        screen_contains(app, "024")
    })
    .await;
    h.keys("<C-w>j").await?;
    h.until("both C-w again and j", |app| {
        screen(app).matches("027").count() == 2 && screen_contains(app, "j")
    })
    .await;
    // still in the terminal, still insert, nothing moved
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    assert!(scratch_view(&h.app).is_some());
    assert_eq!(h.app.editor.tree.focus, scratch_view(&h.app).unwrap());
    assert_eq!(windows(&h.app), windows_before);
    h.finish().await
}

// 6. View mode

#[tokio::test(flavor = "multi_thread")]
async fn view_mode_scrollback_search_select_and_yank() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    h.keys("seq 1 200<ret>").await?;
    h.until("the end of the output", |app| {
        screen(app).lines().any(|l| l == "200")
    })
    .await;
    h.pump_for(Duration::from_millis(200)).await;

    // Esc to normal mode, `gg` goes to the top of the scrollback in view mode
    h.keys("<esc>").await?;
    assert!(!vortex_term::terminal::viewing(&h.app.editor));
    h.keys("gg").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    assert_eq!(cursor_line(&h.app), 0);
    let (text, _) = view_selection(&h.app);
    assert!(
        text.contains("\n1\n2\n"),
        "the scrollback is the document:\n{text}"
    );
    assert!(text.lines().count() > 200);

    // search for a line
    h.keys("/150<ret>").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    assert_eq!(cursor_line_text(&h.app).trim_end(), "150");
    assert_eq!(view_selection(&h.app).1, "150");

    // Enter starts a selection, jj extends it, y yanks
    h.keys("<ret>").await?;
    assert_eq!(h.app.editor.mode(), Mode::Select);
    h.keys("jj").await?;
    assert_eq!(view_selection(&h.app).1, "150\n151\n152");
    h.keys("y").await?;
    assert_eq!(yanked(&h.app).as_deref(), Some("150\n151\n152"));
    // `y` ends the selection and stays in view mode
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(vortex_term::terminal::viewing(&h.app.editor));

    // Enter again, then Esc stops extending but stays in view mode
    h.keys("<ret>").await?;
    assert_eq!(h.app.editor.mode(), Mode::Select);
    h.keys("<esc>").await?;
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(
        vortex_term::terminal::viewing(&h.app.editor),
        "still viewing"
    );

    // the next Esc leaves view mode: live screen, normal mode
    h.keys("<esc>").await?;
    assert!(!vortex_term::terminal::viewing(&h.app.editor));
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(focused_terminal(&h.app).is_some());
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn output_during_view_mode_shows_after_leaving() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    h.keys("sleep 1; echo late-$((20+2))<ret>").await?;
    h.pump_for(Duration::from_millis(200)).await;
    h.keys("<esc>k").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    let (before, _) = view_selection(&h.app);
    assert!(!before.contains("late-22"));
    // the output arrives while viewing
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(2500) {
        h.step(Duration::from_millis(50)).await;
    }
    assert!(
        screen_contains(&h.app, "late-22"),
        "the shell printed it: {}",
        screen(&h.app)
    );
    assert_eq!(
        view_selection(&h.app).0,
        before,
        "the document is as it was"
    );
    h.keys("<esc>").await?;
    assert!(!vortex_term::terminal::viewing(&h.app.editor));
    // viewing again takes a new snapshot, with the output
    h.keys("k").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    assert!(view_selection(&h.app).0.contains("late-22"));
    h.finish().await
}

// 7. OSC 133: prompts, output, editing the command line

const NO_MARKERS_PROMPTS: &str = "no prompts: the shell sends no OSC 133 markers";
const NO_MARKERS_EDIT: &str = "no prompt: the shell sends no OSC 133 markers";

#[tokio::test(flavor = "multi_thread")]
async fn brackets_walk_the_prompts() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    open_bash(&mut h).await?;
    run_and_wait(&mut h, "echo out-one", "out-one").await?;
    run_and_wait(&mut h, "echo out-two", "out-two").await?;
    run_and_wait(&mut h, "echo out-three", "out-three").await?;

    // `k` starts view mode on the last output; `[` goes to its prompt, then the ones before
    h.keys("<esc>k").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    for expected in ["echo out-three", "echo out-two", "echo out-one"] {
        h.keys("[").await?;
        let line = cursor_line_text(&h.app);
        assert!(line.contains(expected), "{expected}: {line}");
    }
    // at the first prompt, `[` stays
    h.keys("[").await?;
    assert!(cursor_line_text(&h.app).contains("echo out-one"));
    for expected in ["echo out-two", "echo out-three"] {
        h.keys("]").await?;
        let line = cursor_line_text(&h.app);
        assert!(line.contains(expected), "{expected}: {line}");
    }
    // the last prompt has no command yet
    h.keys("]").await?;
    let last = cursor_line_text(&h.app);
    assert!(!last.contains("echo"), "{last}");
    assert!(
        last.trim_end().ends_with(['$', '#']),
        "the last prompt line: {last:?}"
    );
    // and the cursor is on its line, so `]` stays there
    h.keys("]").await?;
    assert_eq!(cursor_line_text(&h.app), last);
    h.finish().await
}

/// A count before `[` and `]` goes that many prompts.
#[tokio::test(flavor = "multi_thread")]
async fn a_count_before_a_bracket_goes_that_many_prompts() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    open_bash(&mut h).await?;
    run_and_wait(&mut h, "echo out-one", "out-one").await?;
    run_and_wait(&mut h, "echo out-two", "out-two").await?;
    run_and_wait(&mut h, "echo out-three", "out-three").await?;
    h.keys("<esc>k[").await?;
    assert!(cursor_line_text(&h.app).contains("echo out-three"));
    h.keys("2[").await?;
    let line = cursor_line_text(&h.app);
    assert!(line.contains("echo out-one"), "2[ from out-three: {line}");
    h.keys("2]").await?;
    let line = cursor_line_text(&h.app);
    assert!(line.contains("echo out-three"), "2] from out-one: {line}");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn o_selects_the_output_of_a_command() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    open_bash(&mut h).await?;
    run_and_wait(&mut h, "echo out-one", "out-one").await?;
    run_and_wait(&mut h, "printf 'x\\ny\\n'", "y").await?;
    h.keys("<esc>k").await?;
    // the output of the command under the cursor, from its prompt
    h.keys("[o").await?;
    assert_eq!(view_selection(&h.app).1, "x\ny\n");
    // from inside an output, `[` is its own prompt, then the one before
    h.keys("[[o").await?;
    assert_eq!(view_selection(&h.app).1, "out-one\n");
    // from the last prompt, the previous command's output
    h.keys("]]o").await?;
    assert_eq!(view_selection(&h.app).1, "x\ny\n");
    h.keys("y").await?;
    assert_eq!(yanked(&h.app).as_deref(), Some("x\ny\n"));
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_the_command_line_edit_leaves_the_line() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    open_bash(&mut h).await?;
    let windows_before = windows(&h.app);
    h.keys("echo keep<esc>e").await?;
    h.until("the edit buffer", in_editor).await;
    assert_eq!(doc!(h.app.editor).text().to_string(), "echo keep");
    assert_eq!(windows(&h.app), windows_before + 1);
    h.keys("A-changed<esc>:q!<ret>").await?;
    h.until("back in the terminal", |app| {
        focused_terminal(app).is_some()
    })
    .await;
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    assert_eq!(windows(&h.app), windows_before, "the buffer is closed");
    h.pump_for(Duration::from_millis(400)).await;
    assert!(screen_contains(&h.app, "echo keep"), "{}", screen(&h.app));
    assert!(!screen_contains(&h.app, "changed"), "{}", screen(&h.app));
    // the line is intact: it runs as typed
    h.keys("<ret>").await?;
    h.until("run", |app| screen(app).lines().any(|l| l == "keep"))
        .await;
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn e_refuses_while_a_program_runs() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut h = Harness::with_shell(&["bash"])?;
    open_bash(&mut h).await?;
    let windows_before = windows(&h.app);
    h.keys("sleep 5<ret>").await?;
    h.until("the program to run", |app| {
        scratch(app).is_some_and(|t| !t.shell.at_prompt())
    })
    .await;
    h.keys("<esc>e").await?;
    h.pump_for(Duration::from_millis(200)).await;
    assert_eq!(
        error_text(&h.app).as_deref(),
        Some("a program runs: there is no command line to edit")
    );
    assert_eq!(windows(&h.app), windows_before, "no buffer opened");
    assert!(focused_terminal(&h.app).is_some());
    assert!(h
        .app
        .editor
        .documents()
        .all(|d| d.is_screen() || d.path().is_none()));
    // let go of the program
    h.keys("i<C-c>").await?;
    h.finish().await
}

// 8. A shell without markers

#[tokio::test(flavor = "multi_thread")]
async fn without_markers_windows_modes_and_view_mode_work() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    assert!(!scratch(&h.app).unwrap().shell.has_markers());
    run_and_wait(&mut h, "echo plain-out", "plain-out").await?;

    // modes
    h.keys("<esc>").await?;
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    h.keys("k").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    assert!(view_selection(&h.app).0.contains("plain-out"));
    h.keys("<esc>").await?;
    assert!(!vortex_term::terminal::viewing(&h.app.editor));
    h.keys("i").await?;
    assert_eq!(h.app.editor.mode(), Mode::Insert);

    // windows
    h.keys("<esc><C-w>k").await?;
    assert!(in_editor(&h.app));
    h.keys("<C-w>j").await?;
    assert!(focused_terminal(&h.app).is_some());

    // `]`, `[` and `o` in view mode
    h.keys("<esc>k").await?;
    for key in ["]", "[", "o"] {
        h.keys(key).await?;
        assert_eq!(
            error_text(&h.app).as_deref(),
            Some(NO_MARKERS_PROMPTS),
            "{key}"
        );
        h.app.editor.clear_status();
    }
    assert!(
        vortex_term::terminal::viewing(&h.app.editor),
        "still viewing"
    );
    h.keys("<esc>").await?;

    // `e` in normal mode
    h.keys("e").await?;
    assert_eq!(error_text(&h.app).as_deref(), Some(NO_MARKERS_EDIT));
    assert!(focused_terminal(&h.app).is_some());
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_without_integration_has_no_markers() -> anyhow::Result<()> {
    if !bash() {
        return Ok(());
    }
    let mut config = config_with_shell(&["bash", "--norc", "--noprofile"]);
    config.editor.terminal.shell_integration = false;
    let mut h = Harness::new(AppBuilder::new().with_config(config))?;
    h.keys("<C-t>").await?;
    h.until("a prompt", |app| {
        screen(app)
            .lines()
            .any(|l| l.trim_end().ends_with(['$', '#']))
    })
    .await;
    run_and_wait(&mut h, "echo bash-out", "bash-out").await?;
    h.keys("<esc>k").await?;
    assert!(vortex_term::terminal::viewing(&h.app.editor));
    h.keys("[").await?;
    assert_eq!(error_text(&h.app).as_deref(), Some(NO_MARKERS_PROMPTS));
    h.keys("<esc>e").await?;
    assert_eq!(error_text(&h.app).as_deref(), Some(NO_MARKERS_EDIT));
    h.finish().await
}

// 9. `vx` in a terminal

/// The real `vx` client, as a shell function (the shim's script runs the test binary).
async fn define_vx(h: &mut Harness) -> anyhow::Result<()> {
    let exe = env!("CARGO_BIN_EXE_vx");
    h.keys(&format!("vx() {{ '{exe}' \"$@\"; }}<ret>")).await?;
    h.pump_for(Duration::from_millis(300)).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn vx_file_in_a_terminal_opens_it_in_the_editor_window() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("opened.txt");
    std::fs::write(&file, "one\ntwo\n")?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    define_vx(&mut h).await?;
    h.keys(&format!("vx {}<ret>", file.display())).await?;
    h.until("the file in the editor", |app| {
        in_editor(app) && doc!(app.editor).path().is_some()
    })
    .await;
    assert_eq!(
        doc!(h.app.editor).path().map(|p| p.canonicalize().unwrap()),
        Some(file.canonicalize()?)
    );
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    assert!(scratch_view(&h.app).is_some(), "the terminal stays shown");
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn vx_wait_blocks_the_shell_until_the_buffer_is_written_and_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("wait.txt");
    std::fs::write(&file, "msg\n")?;
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    define_vx(&mut h).await?;
    h.keys(&format!(
        "vx --wait {}; echo waited-$((1+1))<ret>",
        file.display()
    ))
    .await?;
    h.until("the file in the editor", |app| {
        in_editor(app) && app.editor.terminals.waiters.len() == 1
    })
    .await;
    // the shell hasn't got its prompt back
    h.pump_for(Duration::from_millis(500)).await;
    assert!(
        !screen(&h.app).lines().any(|l| l == "waited-2"),
        "{}",
        screen(&h.app)
    );
    assert!(in_editor(&h.app));

    h.keys("A!<esc>:wq<ret>").await?;
    h.until("the marker after the wait", |app| {
        screen(app).lines().any(|l| l == "waited-2")
    })
    .await;
    assert!(focused_terminal(&h.app).is_some(), "back in the terminal");
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    assert_eq!(std::fs::read_to_string(&file)?, "msg!\n");
    h.until("the prompt", has_prompt).await;
    h.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn vx_wait_returns_to_the_terminal_it_came_from() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("from.txt");
    std::fs::write(&file, "msg\n")?;
    let mut h = Harness::sh()?;
    // the scratch terminal exists, and the one that asks is another
    open_panel(&mut h).await?;
    h.keys("<esc><C-w>k:terminal<ret>").await?;
    let asker = focused_terminal(&h.app).expect("a new terminal");
    assert_ne!(Some(asker), h.app.editor.terminals.scratch);
    h.until("its prompt", has_prompt).await;
    define_vx(&mut h).await?;
    h.keys(&format!("vx --wait {}<ret>", file.display()))
        .await?;
    h.until("the file in the editor", |app| {
        in_editor(app) && app.editor.terminals.waiters.len() == 1
    })
    .await;
    h.keys(":q<ret>").await?;
    h.until("back in the terminal", |app| {
        focused_terminal(app).is_some()
    })
    .await;
    assert_eq!(focused_terminal(&h.app), Some(asker));
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.finish().await
}

// 10. Watch mode over a terminal split

fn make_watch_repo(parent: &Path) -> PathBuf {
    let work = parent.join("work");
    std::fs::create_dir_all(&work).unwrap();
    super::git_log::git(&work, "", &["init", "-q"]);
    for name in ["a.txt", "b.txt"] {
        std::fs::write(work.join(name), "1\n2\n3\n").unwrap();
    }
    super::git_log::git(&work, "", &["add", "-A"]);
    super::git_log::git(&work, "", &["commit", "-q", "-m", "Start"]);
    work
}

/// The window that isn't a terminal: its file name and the line of its cursor.
fn editor_window(app: &Application) -> Option<(String, usize)> {
    app.editor.tree.views().find_map(|(view, _)| {
        let doc = app.editor.document(view.doc)?;
        if doc.is_screen() {
            return None;
        }
        let text = doc.text().slice(..);
        Some((
            doc.path()?.file_name()?.to_string_lossy().into_owned(),
            doc.selection(view.id).primary().cursor_line(text),
        ))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_mode_follows_an_agent_in_a_terminal_split() -> anyhow::Result<()> {
    let Some(repo) = super::git_log::Repo::enter_with(
        module_path!(),
        "watch_mode_follows_an_agent_in_a_terminal_split",
        make_watch_repo,
    ) else {
        return Ok(());
    };
    let mut h = Harness::sh()?;
    h.keys(":open a.txt<ret>").await?;
    // watch mode from the editor window; then the terminal for the agent opens beside it (a
    // terminal's window can't be reached by a window motion while following)
    h.keys(":watch<ret>").await?;
    assert!(h.app.editor.watch.is_active(), "{}", status_text(&h.app));
    h.pump_for(Duration::from_millis(400)).await;
    h.keys("<C-w>Tv").await?;
    let terminal_doc = focused_terminal(&h.app).expect("the terminal is focused");
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    h.until("the prompt", has_prompt).await;
    assert!(h.app.editor.watch.is_active());
    assert_eq!(
        h.app
            .editor
            .terminals
            .get(terminal_doc)
            .unwrap()
            .shell
            .cwd()
            .map(|p| p.canonicalize().unwrap()),
        Some(repo.canonicalize()?)
    );

    // the "agent": appends to two files every 200 ms
    h.keys(concat!(
        "for i in 1 2 3 4 5; do echo \"x$i\" <gt><gt> a.txt; sleep 0.2; ",
        "echo \"y$i\" <gt><gt> b.txt; sleep 0.2; done<ret>"
    ))
    .await?;
    let start = Instant::now();
    let mut typed = false;
    while start.elapsed() < Duration::from_millis(2600) {
        h.step(Duration::from_millis(50)).await;
        assert_eq!(focused_terminal(&h.app), Some(terminal_doc), "focus kept");
        assert_eq!(h.app.editor.mode(), Mode::Insert, "mode kept");
        if !typed && start.elapsed() > Duration::from_millis(900) {
            typed = true;
            h.keys("echo still-typing<ret>").await?;
        }
    }
    h.until(
        "the agent to finish and the editor window to follow",
        // the newest file's first new hunk: its lines 4 to 8
        |app| editor_window(app) == Some(("b.txt".to_string(), 3)),
    )
    .await;
    assert_eq!(focused_terminal(&h.app), Some(terminal_doc));
    assert_eq!(h.app.editor.mode(), Mode::Insert);
    assert!(h.app.editor.watch.is_active());
    // the terminal's window still shows the terminal
    let view = h.app.editor.view_of(terminal_doc).expect("still shown");
    assert_eq!(h.app.editor.tree.get(view).doc, terminal_doc);
    // keys typed meanwhile reached the shell
    h.until("the typed command to run", |app| {
        screen(app).lines().any(|l| l.trim() == "still-typing")
    })
    .await;
    h.finish().await
}

/// In a terminal's normal mode `i` types again, also in watch mode (a terminal's own keys come
/// first).
#[tokio::test(flavor = "multi_thread")]
async fn i_returns_to_typing_in_a_terminal_while_watching() -> anyhow::Result<()> {
    let Some(_) = super::git_log::Repo::enter_with(
        module_path!(),
        "i_returns_to_typing_in_a_terminal_while_watching",
        make_watch_repo,
    ) else {
        return Ok(());
    };
    let mut h = Harness::sh()?;
    h.keys(":vterminal<ret>").await?;
    let terminal_doc = focused_terminal(&h.app).expect("a terminal");
    h.until("the prompt", has_prompt).await;
    h.keys("<esc>:watch<ret>").await?;
    assert!(h.app.editor.watch.is_active(), "{}", status_text(&h.app));
    h.pump_for(Duration::from_millis(300)).await;
    assert_eq!(focused_terminal(&h.app), Some(terminal_doc));
    assert_eq!(h.app.editor.mode(), Mode::Normal);
    h.keys("i").await?;
    assert_eq!(h.app.editor.mode(), Mode::Insert, "i typed again");
    h.keys("echo typed-while-$((3+4))<ret>").await?;
    h.until("the output", |app| {
        screen(app).lines().any(|l| l.trim() == "typed-while-7")
    })
    .await;
    h.finish().await
}

// 11. `height` is the old name of `size`

#[tokio::test(flavor = "multi_thread")]
async fn height_is_an_alias_of_size() -> anyhow::Result<()> {
    let parsed: EditorConfig = toml::from_str("[terminal]\nheight = 25\n")?;
    assert_eq!(parsed.terminal.size, 25);
    let both: EditorConfig = toml::from_str("[terminal]\nheight = 25\nsize = 60\n")?;
    assert_eq!(both.terminal.size, 60, "size wins");
    let position: EditorConfig = toml::from_str("[terminal]\nposition = \"right\"\n")?;
    assert_eq!(position.terminal.position, TerminalPosition::Right);
    assert_eq!(position.terminal.size, 33, "the default");

    let mut config = test_config();
    config.editor = EditorConfig {
        terminal: TerminalConfig {
            shell: vec!["sh".to_string()],
            ..parsed.terminal
        },
        ..test_editor_config()
    };
    let mut h = Harness::new(AppBuilder::new().with_config(config))?;
    let tree = h.app.editor.tree.area();
    open_panel(&mut h).await?;
    let area = area_of(&h.app, scratch_view(&h.app).unwrap());
    assert!(
        near(area.height, tree.height as f64 * 0.25, 3.0),
        "25%: {area:?} in {tree:?}"
    );
    h.finish().await
}

// 12. The buffer list

#[tokio::test(flavor = "multi_thread")]
async fn terminals_are_listed_by_name_and_switching_shows_the_hidden_one() -> anyhow::Result<()> {
    let mut h = Harness::sh()?;
    open_panel(&mut h).await?;
    let doc = h.app.editor.terminals.scratch.unwrap();
    let names = |app: &Application| {
        app.editor
            .documents()
            .filter(|d| d.is_screen())
            .map(|d| d.display_name().into_owned())
            .collect::<Vec<_>>()
    };
    let listed = names(&h.app);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(listed[0].starts_with("sh "), "{listed:?}");

    // hidden, it is still listed
    h.keys("<esc><C-t>").await?;
    assert!(scratch_view(&h.app).is_none());
    assert_eq!(names(&h.app), listed);

    // switching to it shows its window
    h.app.editor.switch(doc, Action::Replace);
    h.step(Duration::from_millis(200)).await;
    assert!(scratch_view(&h.app).is_some(), "shown by the switch");
    assert_eq!(focused_terminal(&h.app), Some(doc));
    h.finish().await
}
