//! The terminals (this fork): each one a shell in a window of its own, a screen (see
//! [`crate::screen`]). Here are their shells, the scratch terminal that `C-t` shows and hides,
//! the place its window had while hidden, and the buffers a `vx` in a terminal waits on.
//! `vortex-term` draws them and sends them keys. In view mode a terminal's lines are a read-only
//! text document ([`crate::Document::screen_lines`]) in a text window over the terminal's.

use std::collections::HashMap;

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
}

impl Terminal {
    pub fn new(shell: Shell) -> Self {
        Self {
            shell,
            snapshot: None,
            lines: None,
            awaiting_command_line: false,
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

    #[test]
    fn the_debuggers_keys_still_work() {
        let config = parse("command = \"tmux\"\nargs = [\"split-window\"]\n");
        let external = config.external().unwrap();
        assert_eq!(external.command, "tmux");
        assert_eq!(external.args, ["split-window"]);
    }
}
