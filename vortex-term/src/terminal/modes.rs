//! A terminal window's modes, with Vortex keys:
//!
//! - **insert** (the editor's insert mode): every key goes to the shell, translated as xterm
//!   would, except `Esc`, which goes to normal mode. Entering a terminal's window lands here.
//! - **normal** (normal mode): Vortex keys, nothing typed. `Esc` goes through to the program, as
//!   many times as pressed, and the mode doesn't change. A key that moves or searches starts view
//!   mode and moves there; one that enters insert mode (`i`, `a`, `c`...) types again.
//! - **view** (normal or select mode, with [`Terminal::snapshot`]): the lines of the terminal,
//!   scrollback included, as they were when it started, are a read-only text document in a text
//!   window over the terminal's ([`vortex_view::Document::screen_lines`]), walked with Vortex's
//!   movement and search keys; `Enter` starts a selection, `y` yanks; `]`/`[` go from prompt to
//!   prompt and `o` selects a command's output (with OSC 133 markers). `Esc` stops selecting,
//!   then goes back to normal mode, the terminal's window shown again.
//!
//! The keys of view and normal mode that are the terminal's own are the layers
//! `[keys.terminal]` and `[keys.terminal-view]`.

use vortex_core::{Range, Rope, Selection};
use vortex_view::{
    document::Mode,
    input::KeyEvent,
    keyboard::KeyCode,
    screen::ScreenId,
    terminal::{LineKind, Snapshot, Terminal},
    view::ViewPosition,
    Document, Editor, ViewId,
};

use super::input::{encode_key, encode_paste};

/// The focused window's terminal, with the focused window: the terminal's own, or the text
/// window of its lines in view mode.
pub fn focused(editor: &Editor) -> Option<(ScreenId, ViewId)> {
    let focus = editor.tree.focus;
    if let Some(screen) = editor.tree.focused_screen() {
        return editor
            .terminals
            .is_terminal(screen)
            .then_some((screen, focus));
    }
    let view = editor.tree.try_get(focus)?;
    let screen = editor.document(view.doc)?.lines_of()?;
    editor
        .terminals
        .is_terminal(screen)
        .then_some((screen, focus))
}

fn terminal(editor: &Editor) -> Option<&Terminal> {
    let (screen, _) = focused(editor)?;
    editor.terminals.get(screen)
}

/// Whether the focused window is a terminal in view mode.
pub fn viewing(editor: &Editor) -> bool {
    terminal(editor).is_some_and(Terminal::viewing)
}

/// What a key did to the focused terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum Handled {
    /// It isn't the terminal's: the keymap (with the terminal's layers first) has it.
    No,
    Yes,
}

/// A key for the focused terminal, before the keymap. `idle`: no key, count or register is
/// pending, so that `Esc` is free.
pub fn handle_key(editor: &mut Editor, key: KeyEvent, idle: bool) -> Handled {
    let Some((screen, _)) = focused(editor) else {
        return Handled::No;
    };
    let mode = editor.mode();
    let Some(terminal) = editor.terminals.get(screen) else {
        return Handled::No;
    };
    let esc = key.code == KeyCode::Esc && key.modifiers.is_empty();
    match mode {
        Mode::Insert if esc => {
            editor.mode = Mode::Normal;
            Handled::Yes
        }
        Mode::Insert => {
            if let Some(bytes) = encode_key(key, &terminal.shell.input_modes()) {
                terminal.shell.write(&bytes);
            }
            Handled::Yes
        }
        _ if !esc || !idle => Handled::No,
        Mode::Select => {
            // stops selecting; the selection stays
            editor.mode = Mode::Normal;
            if !terminal.viewing() {
                leave_view(editor, screen);
            }
            Handled::Yes
        }
        Mode::Normal if terminal.viewing() => {
            leave_view(editor, screen);
            Handled::Yes
        }
        Mode::Normal => {
            if let Some(bytes) = encode_key(key, &terminal.shell.input_modes()) {
                terminal.shell.write(&bytes);
            }
            Handled::Yes
        }
    }
}

/// A paste in the focused terminal: it goes to the shell, but in view mode.
pub fn handle_paste(editor: &mut Editor, text: &str) -> bool {
    let Some(terminal) = terminal(editor) else {
        return false;
    };
    if !terminal.viewing() {
        terminal
            .shell
            .write(&encode_paste(text, &terminal.shell.input_modes()));
    }
    true
}

/// Before a static command runs with a terminal focused: a command that moves, selects, scrolls
/// or searches acts on the terminal's lines, starting view mode from normal mode. Returns whether
/// it does (then the fork's modes don't hold it: it isn't about their files).
pub fn before_command(editor: &mut Editor, name: &str) -> bool {
    let Some((screen, view)) = focused(editor) else {
        return false;
    };
    if editor.mode() == Mode::Insert {
        return false;
    }
    // the terminal's own commands (its layers' defaults: typing again, view mode, prompts, the
    // command line) aren't held by watch or debug mode
    if is_terminal_command(name) {
        return true;
    }
    if !walks_lines(name) {
        return false;
    }
    if !viewing(editor) {
        enter_view(editor, screen, view);
    }
    true
}

/// Whether a terminal's window takes the static command `name` (while not typing): its own, or
/// one that walks its lines (which starts view mode).
pub fn takes(name: &str) -> bool {
    is_terminal_command(name) || walks_lines(name)
}

/// The commands of a terminal's key layers.
fn is_terminal_command(name: &str) -> bool {
    matches!(name, "insert_mode" | "append_mode") || name.starts_with("terminal_")
}

/// The commands that walk a terminal's lines: moving, selecting, scrolling, searching, yanking,
/// but not moving between windows.
fn walks_lines(name: &str) -> bool {
    use crate::watch::Effect;
    if name.starts_with("jump_view_")
        || name.starts_with("swap_view_")
        || name.starts_with("rotate_view")
        || name == "transpose_view"
    {
        return false;
    }
    matches!(name, "search_next" | "search_prev")
        || matches!(crate::watch::effect_of(name), Effect::Move | Effect::Search)
}

/// After a key with a terminal focused: a command that entered insert mode (`i`, `a`, `c`...)
/// types in the shell again, on its live screen.
pub fn after_key(editor: &mut Editor) {
    let Some((screen, _)) = focused(editor) else {
        return;
    };
    if editor.mode() == Mode::Insert && viewing(editor) {
        leave_view(editor, screen);
        editor.mode = Mode::Insert;
    }
}

/// `v` in a terminal's normal mode: starts view mode.
pub fn view(editor: &mut Editor) {
    if let Some((screen, view)) = focused(editor) {
        if !viewing(editor) {
            enter_view(editor, screen, view);
        }
    }
}

/// Starts view mode in the terminal's window `view`: the terminal's lines as they are become a
/// read-only text document in a text window over it, the cursor where the program's is (or at
/// the bottom of what was shown), the view where it was.
pub fn enter_view(editor: &mut Editor, screen: ScreenId, view: ViewId) {
    let Some(terminal) = editor.terminals.get_mut(screen) else {
        return;
    };
    let snapshot = terminal.shell.snapshot();
    let name = terminal.name();
    let rows = editor
        .tree
        .screen_view(view)
        .map(|view| view.inner_area().height.max(1) as usize)
        .unwrap_or(1);
    let terminal = editor.terminals.get_mut(screen).unwrap();
    let mut text = String::new();
    for line in &snapshot.lines {
        text.push_str(&line.text);
        text.push('\n');
    }
    let text = Rope::from(text);
    let last = snapshot.lines.len().saturating_sub(1);
    let (line, col) = match snapshot.cursor {
        Some((line, col)) if snapshot.view_top == snapshot.screen_top => (line, col),
        _ => ((snapshot.view_top + rows - 1).min(last), 0),
    };
    let line = line.min(last);
    let char_in_line = snapshot
        .lines
        .get(line)
        .map(|l| l.cols.iter().take_while(|&&c| c < col).count())
        .unwrap_or(0);
    let pos = text.line_to_char(line) + char_in_line;
    let top = text.line_to_char(snapshot.view_top.min(last));
    terminal.snapshot = Some(snapshot);
    let lines = Document::screen_lines(
        screen,
        name,
        text,
        editor.config.clone(),
        editor.syn_loader.clone(),
    );
    let (doc, lines_view) = editor.cover_with_document(view, lines);
    if let Some(terminal) = editor.terminals.get_mut(screen) {
        terminal.lines = Some(doc);
    }
    let document = editor.documents.get_mut(&doc).unwrap();
    document.set_selection(lines_view, Selection::point(pos));
    document.set_view_offset(
        lines_view,
        ViewPosition {
            anchor: top,
            ..ViewPosition::default()
        },
    );
    if editor.mode() != Mode::Normal {
        editor.mode = Mode::Normal;
    }
}

/// Back to the live screen, in normal mode: the window of its lines goes, and the terminal's is
/// shown again.
pub fn leave_view(editor: &mut Editor, screen: ScreenId) {
    let Some(terminal) = editor.terminals.get_mut(screen) else {
        return;
    };
    terminal.snapshot = None;
    // the live screen, at its bottom
    terminal.shell.scroll(-(i32::MAX / 2));
    if let Some(doc) = terminal.lines.take() {
        editor.uncover_document(doc);
    }
    editor.mode = Mode::Normal;
}

/// The lines of view mode, in the focused window: their document's id.
fn lines(editor: &Editor) -> Option<vortex_view::DocumentId> {
    let (screen, _) = focused(editor)?;
    editor.terminals.get(screen)?.lines
}

/// The lines where a prompt starts (the first line of each run of prompt lines).
fn prompt_starts(snapshot: &Snapshot) -> Vec<usize> {
    let lines = &snapshot.lines;
    (0..lines.len())
        .filter(|&i| {
            lines[i].kind == LineKind::Prompt && (i == 0 || lines[i - 1].kind != LineKind::Prompt)
        })
        .collect()
}

/// Says why the prompt keys do nothing: the shell sends no markers.
fn without_markers(editor: &mut Editor) -> bool {
    let markers = terminal(editor).is_some_and(|terminal| terminal.shell.has_markers());
    if !markers {
        editor.set_error("no prompts: the shell sends no OSC 133 markers");
    }
    !markers
}

/// `]` (`forward`) or `[` in view mode: the cursor to the start of the next or previous prompt,
/// `count` times; in select mode the selection extends to it.
pub fn goto_prompt(editor: &mut Editor, forward: bool, count: usize) {
    let Some((screen, view)) = focused(editor) else {
        return;
    };
    if !viewing(editor) || without_markers(editor) {
        return;
    }
    let Some(doc) = lines(editor) else {
        return;
    };
    let starts = prompt_starts(
        editor
            .terminals
            .get(screen)
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap(),
    );
    let extend = editor.mode() == Mode::Select;
    let document = editor.documents.get_mut(&doc).unwrap();
    let text = document.text().clone();
    let selection = document.selection(view).clone();
    let head_line = text.char_to_line(selection.primary().cursor(text.slice(..)));
    let mut line = head_line;
    for _ in 0..count.max(1) {
        let next = if forward {
            starts.iter().copied().find(|&start| start > line)
        } else {
            starts.iter().rev().copied().find(|&start| start < line)
        };
        match next {
            Some(next) => line = next,
            None => break,
        }
    }
    if line == head_line {
        return;
    }
    let pos = text.line_to_char(line);
    let range = selection.primary();
    let range = if extend {
        range.put_cursor(text.slice(..), pos, true)
    } else {
        Range::point(pos)
    };
    document.set_selection(view, Selection::single(range.anchor, range.head));
}

/// `o` in view mode: selects the whole output of the command under the cursor (from the line
/// after its prompt and command to the line before the next prompt), or of the last command's
/// when the cursor is on the last prompt.
pub fn select_output(editor: &mut Editor) {
    let Some((screen, view)) = focused(editor) else {
        return;
    };
    if !viewing(editor) || without_markers(editor) {
        return;
    }
    let Some(doc) = lines(editor) else {
        return;
    };
    let snapshot = editor
        .terminals
        .get(screen)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    let lines = &snapshot.lines;
    let starts = prompt_starts(snapshot);
    let document = editor.documents.get(&doc).unwrap();
    let text = document.text().clone();
    let cursor_line = text.char_to_line(document.selection(view).primary().cursor(text.slice(..)));
    // the output of the prompt starting at `start`: the lines after its prompt lines, to the next
    // prompt, without the blank lines at its end
    let output = |start: usize| {
        let first = (start..lines.len())
            .find(|&i| lines[i].kind != LineKind::Prompt)
            .unwrap_or(lines.len());
        let mut end = starts
            .iter()
            .copied()
            .find(|&s| s > start)
            .unwrap_or(lines.len());
        while end > first && lines[end - 1].text.trim().is_empty() {
            end -= 1;
        }
        (first < end).then_some(first..end)
    };
    let at = starts.iter().rposition(|&start| start <= cursor_line);
    let found = match at {
        Some(at) => {
            output(starts[at]).or_else(|| at.checked_sub(1).and_then(|at| output(starts[at])))
        }
        None => None,
    };
    let Some(range) = found else {
        editor.set_error("no output here");
        return;
    };
    let from = text.line_to_char(range.start);
    let to = text.line_to_char(range.end);
    let document = editor.documents.get_mut(&doc).unwrap();
    document.set_selection(view, Selection::single(from, to));
}
