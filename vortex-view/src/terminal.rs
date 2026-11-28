//! The terminals (this fork): each one a shell in a window of its own, a screen (see
//! [`crate::screen`]). Here are their shells, the scratch terminal that `C-t` shows and hides,
//! the place its window had while hidden, and the buffers a `vx` in a terminal waits on.
//! `vortex-term` draws them and sends them keys. In view mode a terminal's lines are a read-only
//! text document ([`crate::Document::screen_lines`]) in a text window over the terminal's.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    editor::{get_terminal_provider, ExternalTerminal},
    screen::ScreenId,
    tree::Place,
    DocumentId,
};

pub use vortex_terminal::*;

/// Where the scratch terminal opens: along which edge of the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalPosition {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

/// `[editor.terminal]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct TerminalConfig {
    /// Where the scratch terminal opens. Defaults to the bottom.
    pub position: TerminalPosition,
    /// The size of the scratch terminal's window, in percent of the editor in the direction of
    /// `position`. Defaults to 33. `height` is an older name for it.
    pub size: u16,
    /// The shell to run, with its arguments. Defaults to `$SHELL`.
    pub shell: Vec<String>,
    /// Report when a command runs and how it ended (bash and zsh, without touching their rc
    /// files). Defaults to true.
    pub shell_integration: bool,
    /// Lines of scrollback. Defaults to 10000.
    pub scrollback: usize,
    /// The lines of the strip a hidden scratch terminal shows as when its last command is
    /// rerun, at a top or bottom `position`. Defaults to 4.
    pub rerun_size: u16,
    /// The external terminal the debugger runs programs in (`runInTerminal`). Defaults to a tmux
    /// or WezTerm split when Vortex runs in one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// What `[editor.terminal]` may say: `height` is kept as an older name of `size`.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct RawTerminalConfig {
    position: TerminalPosition,
    size: Option<u16>,
    height: Option<u16>,
    shell: Vec<String>,
    shell_integration: bool,
    scrollback: usize,
    rerun_size: u16,
    command: String,
    args: Vec<String>,
}

impl Default for RawTerminalConfig {
    fn default() -> Self {
        let config = TerminalConfig::default();
        Self {
            position: config.position,
            size: None,
            height: None,
            shell: config.shell,
            shell_integration: config.shell_integration,
            scrollback: config.scrollback,
            rerun_size: config.rerun_size,
            command: config.command,
            args: config.args,
        }
    }
}

impl<'de> Deserialize<'de> for TerminalConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawTerminalConfig::deserialize(deserializer)?;
        Ok(Self {
            position: raw.position,
            // `size` wins when both are there
            size: raw.size.or(raw.height).unwrap_or(DEFAULT_SIZE),
            shell: raw.shell,
            shell_integration: raw.shell_integration,
            scrollback: raw.scrollback,
            rerun_size: raw.rerun_size,
            command: raw.command,
            args: raw.args,
        })
    }
}

const DEFAULT_SIZE: u16 = 33;

impl TerminalConfig {
    /// The debugger's external terminal, when there is one.
    pub fn external(&self) -> Option<ExternalTerminal> {
        (!self.command.is_empty()).then(|| ExternalTerminal {
            command: self.command.clone(),
            args: self.args.clone(),
        })
    }

    /// `size`, kept between 10 and 90 percent.
    pub fn share(&self) -> u16 {
        self.size.clamp(10, 90)
    }
}

impl Default for TerminalConfig {
    fn default() -> Self {
        let external = get_terminal_provider().unwrap_or_default();
        Self {
            command: external.command,
            args: external.args,
            position: TerminalPosition::default(),
            size: DEFAULT_SIZE,
            shell: Vec::new(),
            shell_integration: true,
            scrollback: 10_000,
            rerun_size: 4,
        }
    }
}

/// A buffer a `vx` in a terminal waits on (`vx --wait` as `$EDITOR`): `done` is called when it
/// closes, and the terminal it was started from gets the focus back.
pub struct Waiter {
    pub doc: DocumentId,
    /// The terminal it was started in.
    pub terminal: ScreenId,
    pub done: Box<dyn FnOnce() + Send>,
}

/// One terminal: the shell in it, and view mode's lines while it is on.
pub struct Terminal {
    pub shell: Shell,
    /// View mode: the lines as they were when it started, which the text of `lines` is made of.
    /// `None`: the window shows the live screen.
    pub snapshot: Option<Snapshot>,
    /// View mode's text document, in the text window over the terminal's.
    pub lines: Option<DocumentId>,
    /// The edit command key asked the shell for its command line, which opens in a buffer when
    /// it comes.
    pub awaiting_command_line: bool,
    /// A rerun of the last command waiting for its end, shared with the shell's events.
    pub rerun: RerunWatch,
}

impl Terminal {
    pub fn new(shell: Shell, rerun: RerunWatch) -> Self {
        Self {
            shell,
            snapshot: None,
            lines: None,
            awaiting_command_line: false,
            rerun,
        }
    }

    /// Whether view mode is on.
    pub fn viewing(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The name of its document: the shell and its working directory, `zsh ~/src/vortex`.
    pub fn name(&self) -> String {
        let shell = self.shell.name();
        match self.shell.cwd() {
            Some(cwd) => format!(
                "{shell} {}",
                vortex_stdx::path::fold_home_dir(&cwd).display()
            ),
            None => shell,
        }
    }

    /// The window got the keys: the result of the last command has been seen; a program that
    /// asked is told.
    pub fn focus_in(&self) {
        self.shell.acknowledge();
        if self.shell.input_modes().focus_reporting {
            self.shell.write(b"\x1b[I");
        }
    }

    /// The window lost the keys.
    pub fn focus_out(&self) {
        if self.shell.input_modes().focus_reporting {
            self.shell.write(b"\x1b[O");
        }
    }
}

/// A rerun of the scratch terminal's last command (`C-T`), until it ends: shared between the
/// terminal and the task that receives its shell's events, which looks at it only on a change
/// of status. Empty, nothing is watched.
#[derive(Clone, Default)]
pub struct RerunWatch(Arc<Mutex<Option<Rerun>>>);

struct Rerun {
    command: String,
    /// When it started running (OSC 133 `C`).
    started: Option<Instant>,
}

/// How a rerun command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerunEnd {
    pub command: String,
    /// Its exit code, when the shell gave one.
    pub code: Option<i32>,
    pub took: Duration,
}

impl RerunWatch {
    /// Watches `command`, which is about to run.
    pub fn start(&self, command: String) {
        *self.0.lock() = Some(Rerun {
            command,
            started: None,
        });
    }

    /// The shell's status changed to `status`: the end of the watched command, once.
    pub fn observe(&self, status: ShellStatus) -> Option<RerunEnd> {
        let mut watched = self.0.lock();
        let rerun = watched.as_mut()?;
        let code = match (status, rerun.started) {
            (ShellStatus::Running, _) => {
                rerun.started = Some(Instant::now());
                return None;
            }
            (ShellStatus::Finished(code), Some(_)) => Some(code),
            (ShellStatus::Idle, Some(_)) => None,
            _ => return None,
        };
        let rerun = watched.take()?;
        Some(RerunEnd {
            command: rerun.command,
            code,
            took: rerun.started.map(|at| at.elapsed()).unwrap_or_default(),
        })
    }
}

/// The scratch terminal shown as the rerun strip: small at its edge, without the focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strip {
    /// Where the scratch terminal was hidden from before the strip showed: where it goes back
    /// when the strip is hidden, instead of where the strip was.
    pub place: Option<Place>,
}

/// The terminals of the editor.
#[derive(Default)]
pub struct Terminals {
    /// The terminals, by their screen.
    pub list: HashMap<ScreenId, Terminal>,
    /// The scratch terminal (`C-t`, `:sh`): the one that is hidden instead of closed.
    pub scratch: Option<ScreenId>,
    /// Where the scratch terminal's window was when it was hidden, to show it there again.
    pub scratch_place: Option<Place>,
    /// Buffers a `vx` in a terminal waits on.
    pub waiters: Vec<Waiter>,
    /// The buffers a terminal's command line is edited in, and their terminal.
    pub edits: HashMap<DocumentId, ScreenId>,
    /// The scratch terminal's window is the rerun strip.
    pub strip: Option<Strip>,
}

impl Terminals {
    pub fn get(&self, screen: ScreenId) -> Option<&Terminal> {
        self.list.get(&screen)
    }

    pub fn get_mut(&mut self, screen: ScreenId) -> Option<&mut Terminal> {
        self.list.get_mut(&screen)
    }

    pub fn is_terminal(&self, screen: ScreenId) -> bool {
        self.list.contains_key(&screen)
    }

    /// The terminal whose lines `doc` shows in view mode, if any.
    pub fn of_lines(&self, doc: DocumentId) -> Option<ScreenId> {
        self.list
            .iter()
            .find(|(_, terminal)| terminal.lines == Some(doc))
            .map(|(&screen, _)| screen)
    }

    /// The scratch terminal, while its shell runs.
    pub fn scratch(&self) -> Option<&Terminal> {
        self.get(self.scratch?)
    }

    /// The screen `screen` closed: if it was a terminal (its shell exited, or its window was
    /// closed), drops it, which kills its shell, and answers the `vx` in it that waited on a
    /// buffer.
    pub fn screen_closed(&mut self, screen: ScreenId) {
        if self.list.remove(&screen).is_none() {
            return;
        }
        if self.scratch == Some(screen) {
            self.scratch = None;
            self.scratch_place = None;
            self.strip = None;
        }
        self.edits.retain(|_, terminal| *terminal != screen);
        let (done, waiting): (Vec<_>, _) = std::mem::take(&mut self.waiters)
            .into_iter()
            .partition(|waiter| waiter.terminal == screen);
        self.waiters = waiting;
        for waiter in done {
            (waiter.done)();
        }
    }

    /// Calls the waiters of `doc`, which was closed. Returns the terminal one of them came from,
    /// to give it the focus back.
    pub fn document_closed(&mut self, doc: DocumentId) -> Option<ScreenId> {
        let (done, waiting): (Vec<_>, _) = std::mem::take(&mut self.waiters)
            .into_iter()
            .partition(|waiter| waiter.doc == doc);
        self.waiters = waiting;
        let terminal = done.first().map(|waiter| waiter.terminal);
        for waiter in done {
            (waiter.done)();
        }
        terminal.filter(|terminal| self.list.contains_key(terminal))
    }

    /// The scratch terminal's window was hidden from `place`: it is shown there again, or, if
    /// it was the strip, where it was before the strip.
    pub fn scratch_hidden(&mut self, place: Place) {
        self.scratch_place = match self.strip.take() {
            Some(strip) => strip.place,
            None => Some(place),
        };
    }

    pub fn is_awaited(&self, doc: DocumentId) -> bool {
        self.waiters.iter().any(|waiter| waiter.doc == doc)
    }
}

impl crate::Editor {
    /// Where a terminal's window goes when it is shown: the scratch terminal back where it was
    /// hidden, else at `[editor.terminal] position` and `size`; another terminal as a split.
    pub fn terminal_placement(&mut self, screen: ScreenId) -> crate::screen::Placement {
        use crate::{screen::Placement, tree::Layout};
        if self.terminals.scratch != Some(screen) {
            return Placement::Split(Layout::Horizontal);
        }
        let config = self.config().terminal.clone();
        let place = self.terminals.scratch_place.take();
        edge_placement(config.position, config.share(), place)
    }

    /// Where the rerun strip goes: along the scratch terminal's edge, `rerun-size` lines and
    /// its statusline tall at the top or bottom, never more than its usual size; at the left or
    /// right, a strip of lines can't stand, so at its usual width.
    pub fn strip_placement(&self) -> crate::screen::Placement {
        let config = &self.config().terminal;
        let share = match config.position {
            TerminalPosition::Bottom | TerminalPosition::Top => {
                let total = u32::from(self.tree.area().height.max(1));
                let rows = u32::from(config.rerun_size.max(1)) + 1;
                // rounded up: the tree rounds a share to the nearest row
                let percent = (rows * 100).div_ceil(total);
                (percent.min(u32::from(config.share())) as u16).max(1)
            }
            TerminalPosition::Left | TerminalPosition::Right => config.share(),
        };
        edge_placement(config.position, share, None)
    }

    /// Where the debug panel's window goes (this fork): where it was, else along the edge of
    /// `[editor.debugger.panel]`.
    pub fn debug_panel_placement(&mut self) -> crate::screen::Placement {
        let config = self.config().debugger.panel;
        let place = self.debug.panel_place.take();
        edge_placement(config.position, config.share(), place)
    }
}

/// A window along the edge `position`, taking `share` percent, or back at `place`.
fn edge_placement(
    position: TerminalPosition,
    share: u16,
    place: Option<Place>,
) -> crate::screen::Placement {
    use crate::{screen::Placement, tree::Layout};
    let (layout, at_end) = match position {
        TerminalPosition::Bottom => (Layout::Horizontal, true),
        TerminalPosition::Top => (Layout::Horizontal, false),
        TerminalPosition::Left => (Layout::Vertical, false),
        TerminalPosition::Right => (Layout::Vertical, true),
    };
    match place {
        Some(place) => Placement::Back {
            place,
            fallback: layout,
            at_end,
        },
        None => Placement::Edge {
            layout,
            at_end,
            share,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> TerminalConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn defaults() {
        let config = parse("");
        assert_eq!(config.position, TerminalPosition::Bottom);
        assert_eq!(config.size, 33);
        assert!(config.shell_integration);
        assert_eq!(config.scrollback, 10_000);
        assert_eq!(config.rerun_size, 4);
        assert_eq!(parse("rerun-size = 6").rerun_size, 6);
    }

    #[test]
    fn position_and_size() {
        let config = parse("position = \"right\"\nsize = 40\n");
        assert_eq!(config.position, TerminalPosition::Right);
        assert_eq!(config.size, 40);
        assert_eq!(parse("size = 5").share(), 10);
        assert_eq!(parse("size = 95").share(), 90);
    }

    #[test]
    fn height_is_an_older_name_of_size() {
        assert_eq!(parse("height = 25").size, 25);
        // `size` wins
        assert_eq!(parse("height = 25\nsize = 45").size, 45);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<TerminalConfig>("hieght = 20").is_err());
        assert!(toml::from_str::<TerminalConfig>("position = \"middle\"").is_err());
    }

    /// A rerun ends at the first end of a command after it started running, once; nothing is
    /// watched before `start`.
    #[test]
    fn a_rerun_ends_once() {
        let watch = RerunWatch::default();
        assert_eq!(watch.observe(ShellStatus::Running), None);
        assert_eq!(watch.observe(ShellStatus::Finished(0)), None);
        watch.start("cargo check".into());
        // the end of an earlier command, before this one ran
        assert_eq!(watch.observe(ShellStatus::Finished(1)), None);
        assert_eq!(watch.observe(ShellStatus::Running), None);
        let end = watch.observe(ShellStatus::Finished(101)).unwrap();
        assert_eq!(end.command, "cargo check");
        assert_eq!(end.code, Some(101));
        assert_eq!(watch.observe(ShellStatus::Finished(0)), None);
        // a shell that gives no exit code
        watch.start("make".into());
        watch.observe(ShellStatus::Running);
        assert_eq!(watch.observe(ShellStatus::Idle).unwrap().code, None);
    }

    #[test]
    fn the_debuggers_keys_still_work() {
        let config = parse("command = \"tmux\"\nargs = [\"split-window\"]\n");
        let external = config.external().unwrap();
        assert_eq!(external.command, "tmux");
        assert_eq!(external.args, ["split-window"]);
    }
}
