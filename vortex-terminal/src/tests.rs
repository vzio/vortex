use std::time::{Duration, Instant};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::{Color, Shell, ShellEvent, ShellOptions, ShellStatus, Size};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    shell: Shell,
    events: UnboundedReceiver<ShellEvent>,
    seen: Vec<ShellEvent>,
    _home: tempfile::TempDir,
}

impl Harness {
    /// Starts `command` with a fresh `HOME` containing `bashrc` and `zshrc` that set a short
    /// prompt, so that the user's own rc files don't matter.
    fn start(command: &[&str], integration: bool) -> Harness {
        let mut h = Self::spawn(command, integration);
        // Input typed before the first prompt may be echoed out of order.
        h.wait_row("$");
        h
    }

    /// Starts `command` without waiting for its prompt.
    fn spawn(command: &[&str], integration: bool) -> Harness {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".bashrc"), "PS1='$ '\n").unwrap();
        std::fs::write(home.path().join(".zshrc"), "PS1='$ '\n").unwrap();
        let (tx, events) = unbounded_channel();
        let shell = Shell::spawn(
            ShellOptions {
                command: command.iter().map(|s| s.to_string()).collect(),
                cwd: Some(home.path().to_path_buf()),
                env: vec![
                    ("HOME".into(), home.path().display().to_string()),
                    ("ZDOTDIR".into(), home.path().display().to_string()),
                    ("PS1".into(), "$ ".into()),
                ],
                path_prefix: None,
                integration,
                size: Size { rows: 24, cols: 80 },
                scrollback: 1000,
            },
            tx,
        )
        .unwrap();
        Harness {
            shell,
            events,
            seen: Vec::new(),
            _home: home,
        }
    }

    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.seen.push(event);
        }
    }

    fn wait_for(&mut self, what: &str, mut cond: impl FnMut(&mut Harness) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.drain();
            if cond(self) {
                return;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}; screen:\n{}", self.screen());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_status(&mut self, status: ShellStatus) {
        self.wait_for(&format!("{status:?}"), |h| h.shell.status() == status);
    }

    fn rows(&self) -> Vec<String> {
        let size = self.shell.size();
        let mut rows = vec![String::new(); size.rows as usize];
        self.shell
            .visit_cells(|row, _, cell| rows[row as usize].push_str(&cell.text));
        rows.iter().map(|r| r.trim_end().to_string()).collect()
    }

    fn screen(&self) -> String {
        self.rows().join("\n")
    }

    fn wait_row(&mut self, row: &str) {
        self.wait_for(&format!("row {row:?}"), |h| {
            h.rows().iter().any(|r| r == row)
        });
    }

    fn prompts(&self) -> usize {
        self.rows().iter().filter(|r| *r == "$").count()
    }

    fn type_line(&self, line: &str) {
        self.shell.write(format!("{line}\r").as_bytes());
    }
}

#[test]
fn sh_echo_resize_exit() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line("echo hello");
    h.wait_row("hello");

    h.shell.resize(Size { rows: 10, cols: 40 });
    assert_eq!(h.shell.size(), Size { rows: 10, cols: 40 });
    assert_eq!(h.rows().len(), 10);

    h.type_line("exit");
    h.wait_for("exit", |h| h.seen.contains(&ShellEvent::Exited(Some(0))));
    assert!(h.seen.contains(&ShellEvent::Output));
}

#[test]
fn colors() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line(r"printf '\033[31mR\033[38;5;200mI\033[38;2;1;2;3mG\033[0m\n'");
    h.wait_row("RIG");
    // The echoed command line has the letters too, but not in this order.
    let on_output_row = h.rows().iter().position(|r| r == "RIG").unwrap() as u16;
    let mut fgs = Vec::new();
    h.shell.visit_cells(|row, col, cell| {
        if row == on_output_row && col < 3 {
            fgs.push(cell.fg);
        }
    });
    assert_eq!(
        fgs,
        [Color::Indexed(1), Color::Indexed(200), Color::Rgb(1, 2, 3)]
    );
}

#[test]
fn modes() {
    let mut h = Harness::start(&["/bin/sh"], false);
    assert!(!h.shell.input_modes().alt_screen);
    h.type_line(r"printf '\033[?1049h'");
    h.wait_for("alt screen", |h| h.shell.input_modes().alt_screen);
    h.type_line(r"printf '\033[?1049l\033[?2004h'");
    h.wait_for("bracketed paste", |h| h.shell.input_modes().bracketed_paste);
    assert!(!h.shell.input_modes().alt_screen);
}

#[test]
fn cursor_and_title() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line(r"printf '\033]0;my title\007'");
    h.wait_for("title", |h| h.shell.title().as_deref() == Some("my title"));
    assert!(h.seen.contains(&ShellEvent::Title("my title".into())));
    assert!(h.shell.cursor().is_some());
    h.type_line(r"printf '\033[?25l'");
    h.wait_for("hidden cursor", |h| h.shell.cursor().is_none());
}

#[test]
fn scrollback() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line("seq 1 100");
    h.wait_row("100");
    assert_eq!(h.shell.scroll_offset(), 0);
    h.shell.scroll(5);
    assert_eq!(h.shell.scroll_offset(), 5);
    assert!(h.shell.cursor().is_none());
    h.shell.write(b"");
    assert_eq!(h.shell.scroll_offset(), 0);
}

#[test]
fn output_events_are_coalesced() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line("echo one");
    h.wait_row("one");
    h.drain();
    h.seen.clear();
    h.shell.take_output_pending();
    h.type_line("echo two");
    h.wait_row("two");
    h.drain();
    assert_eq!(
        h.seen.iter().filter(|e| **e == ShellEvent::Output).count(),
        1
    );
}

#[test]
fn terminal_queries_are_answered() {
    let mut h = Harness::start(&["/bin/sh"], false);
    // Device status report: the answer comes back as input; the terminal shows it, then the
    // shell reads the line.
    h.type_line(r#"printf '\033[6n'; read -r x; echo "got:$x" | cat -v"#);
    h.wait_row("^[[2;1R");
    h.type_line("");
    h.wait_row("got:^[[2;1R");
}

#[test]
fn fallback_without_integration() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line("sleep 1");
    h.wait_status(ShellStatus::Running);
    h.wait_status(ShellStatus::Idle);
    assert!(h.seen.contains(&ShellEvent::Status(ShellStatus::Running)));
}

fn integration_flow(command: &[&str]) {
    let mut h = Harness::start(command, true);

    h.type_line("false");
    h.wait_status(ShellStatus::Finished(1));
    h.acknowledge_and_check_idle();

    h.type_line("true");
    h.wait_status(ShellStatus::Finished(0));

    h.type_line("sleep 1");
    h.wait_status(ShellStatus::Running);
    h.wait_status(ShellStatus::Finished(0));

    h.type_line("false");
    h.wait_status(ShellStatus::Finished(1));
    h.type_line("echo $?");
    h.wait_row("1");
    h.wait_status(ShellStatus::Finished(0));

    // An empty line after a failure keeps the failure.
    h.type_line("false");
    h.wait_status(ShellStatus::Finished(1));
    let prompts = h.prompts();
    h.type_line("");
    h.wait_for("a new prompt", |h| h.prompts() > prompts);
    assert_eq!(h.shell.status(), ShellStatus::Finished(1));

    // The statuses were all seen, in order, as events.
    h.drain();
    assert!(h.seen.contains(&ShellEvent::Status(ShellStatus::Running)));
    assert!(h
        .seen
        .contains(&ShellEvent::Status(ShellStatus::Finished(1))));
}

impl Harness {
    fn acknowledge_and_check_idle(&mut self) {
        self.shell.acknowledge();
        assert_eq!(self.shell.status(), ShellStatus::Idle);
    }
}

/// `:sh command` on a new shell: bash and zsh drop what is typed while they start, so it is held
/// until the first prompt.
#[test]
fn input_typed_before_the_prompt_is_kept() {
    for command in [&["bash"][..], &["sh"]] {
        let mut h = Harness::spawn(command, true);
        h.type_line("echo early-$((40+2))");
        h.wait_row("early-42");
    }
}

#[test]
fn bash_integration() {
    integration_flow(&["bash"]);
}

#[test]
fn bash_integration_keeps_user_prompt_command() {
    let home = tempfile::tempdir().unwrap();
    // A PROMPT_COMMAND that prints `$?` itself, as a user's would.
    std::fs::write(
        home.path().join(".bashrc"),
        "PS1='$ '\nPROMPT_COMMAND='echo status=$?'\n",
    )
    .unwrap();
    let (tx, events) = unbounded_channel();
    let shell = Shell::spawn(
        ShellOptions {
            command: vec!["bash".into()],
            cwd: Some(home.path().to_path_buf()),
            env: vec![("HOME".into(), home.path().display().to_string())],
            path_prefix: None,
            integration: true,
            size: Size { rows: 24, cols: 80 },
            scrollback: 100,
        },
        tx,
    )
    .unwrap();
    let mut h = Harness {
        shell,
        events,
        seen: Vec::new(),
        _home: home,
    };
    h.wait_row("$");
    h.type_line("(exit 7)");
    h.wait_status(ShellStatus::Finished(7));
    h.wait_row("status=7");
}

#[test]
fn zsh_integration() {
    if std::process::Command::new("zsh")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("zsh is not installed: skipped");
        return;
    }
    integration_flow(&["zsh"]);
}

impl Harness {
    fn wait_event(&mut self, what: &str, f: impl Fn(&ShellEvent) -> bool) -> ShellEvent {
        self.wait_for(what, |h| h.seen.iter().any(&f));
        self.seen.iter().find(|e| f(e)).cloned().unwrap()
    }
}

/// The lines of a prompt (written between OSC 133 `A` and `C`) are known in a snapshot, through
/// scrolling, and the output between them isn't.
#[test]
fn snapshot_knows_the_prompts() {
    use crate::LineKind;
    let mut h = Harness::start(&["bash"], true);
    h.type_line("echo one; echo two");
    h.wait_row("two");
    h.type_line("seq 1 40");
    h.wait_row("40");
    h.wait_for("the prompt after seq", |h| {
        h.rows().last().map(String::as_str) == Some("$")
            || h.rows()
                .iter()
                .rev()
                .find(|r| !r.is_empty())
                .map(String::as_str)
                == Some("$")
    });
    let snapshot = h.shell.snapshot();
    // the scrollback is in it
    assert!(snapshot.screen_top > 0, "{snapshot:?}");
    let texts: Vec<_> = snapshot.lines.iter().map(|l| l.text.as_str()).collect();
    let prompts: Vec<_> = snapshot
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Prompt)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(
        prompts,
        ["$ echo one; echo two", "$ seq 1 40", "$"],
        "{texts:#?}"
    );
    let one = texts.iter().position(|t| *t == "one").unwrap();
    assert_eq!(snapshot.lines[one].kind, LineKind::Other);
    assert_eq!(texts[one + 1], "two");
    // the cursor is on the last prompt
    let (line, col) = snapshot.cursor.unwrap();
    assert_eq!(texts[line], "$");
    assert_eq!(col, 2);
    // columns of each character
    assert_eq!(snapshot.lines[one].cols, [0, 1, 2]);
}

#[test]
fn snapshot_columns_of_wide_characters() {
    let mut h = Harness::start(&["/bin/sh"], false);
    h.type_line("printf '\\344\\275\\240x\\n'");
    h.wait_row("你x");
    let snapshot = h.shell.snapshot();
    let line = snapshot.lines.iter().find(|l| l.text == "你x").unwrap();
    assert_eq!(line.cols, [0, 2]);
}

#[test]
fn bash_reports_its_working_directory() {
    let mut h = Harness::start(&["bash"], true);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a dir");
    std::fs::create_dir(&path).unwrap();
    h.type_line(&format!("cd '{}'", path.display()));
    let want = path.clone();
    h.wait_event("the new directory", |e| *e == ShellEvent::Cwd(want.clone()));
    assert_eq!(h.shell.cwd(), Some(path));
}

/// Vortex's integration reports the command line being edited, and replaces it without running it.
#[test]
fn bash_reports_and_replaces_the_command_line() {
    let mut h = Harness::start(&["bash"], true);
    h.wait_for("the markers", |h| h.shell.has_vortex_integration());
    assert!(h.shell.at_prompt());
    h.shell.write(b"echo half typed");
    h.wait_row("$ echo half typed");
    h.shell.request_command_line();
    h.wait_event("the command line", |e| {
        *e == ShellEvent::CommandLine("echo half typed".into())
    });
    h.shell.replace_command_line("echo replaced-$((6*7))");
    h.wait_row("$ echo replaced-$((6*7))");
    // not run: Enter runs it
    assert!(!h.rows().iter().any(|r| r == "replaced-42"));
    h.shell.write(b"\r");
    h.wait_row("replaced-42");
    // while a command runs, there is no prompt
    h.type_line("sleep 1");
    h.wait_status(ShellStatus::Running);
    assert!(!h.shell.at_prompt());
    h.wait_status(ShellStatus::Finished(0));
    assert!(h.shell.at_prompt());
}

#[test]
fn no_prompt_without_markers() {
    let h = Harness::start(&["/bin/sh"], false);
    assert!(!h.shell.has_markers());
    assert!(!h.shell.at_prompt());
    assert!(!h.shell.has_vortex_integration());
}
