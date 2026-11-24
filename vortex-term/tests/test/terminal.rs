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
    editor::Config as EditorConfig,
    input::parse_macro,
    terminal::{ShellStatus, Terminal, TerminalConfig},
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
