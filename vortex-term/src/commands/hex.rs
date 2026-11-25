//! The keys and commands of the hex view (this fork). The hex view is a screen (see
//! `vortex_view::hex::HexView`, `screens.rs` and `ui/hex_view.rs`) that shows bytes; its keys are
//! handled here, before the keymap ([`handle_screen_key`]), which still gets the ones the view
//! leaves alone: `:`, `space`, and the keys with `Ctrl` or `Alt` that it doesn't use (`C-w` for
//! the windows...). No text command runs on a screen, so none can edit the text under it unseen.
//!
//! Read-only (`RO`, the default): typing a character only says how to overwrite. `i` enters
//! overwrite mode (`OVR`), where typing overwrites the byte under the cursor, in the column the
//! cursor is in; `Esc` leaves it.

use vortex_core::movement::Direction;
use vortex_view::{
    document::Mode,
    editor::Editor,
    graphics::Rect,
    hex::{self, Column, HexBuffer, HexCursor, HexView, Layout, Rows},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    screen::Placement,
    ViewId,
};

use super::Context;
use crate::{
    compositor::{self, EventResult},
    ui::{self, hex_view, PromptEvent},
};

/// The most bytes `y` yanks (their hex text is three times as long).
const MAX_YANK: u64 = 16 * 1024 * 1024;

/// What a typed character does in read-only mode.
pub const READ_ONLY: &str = "read-only";

/// Whether the focused hex screen shows a program's memory.
pub fn focused_memory(editor: &Editor) -> bool {
    editor
        .focused_screen()
        .and_then(|screen| screen.state::<HexView>())
        .is_some_and(|hex| hex.buffer.is_memory())
}

/// Whether the focused hex screen is in one of its own modal states (overwriting, selecting, a
/// `g` pending): then it takes every key, `Esc` included, before the modes' keys.
pub fn modal(editor: &Editor) -> bool {
    let view = editor.tree.focus;
    editor
        .focused_screen()
        .and_then(|screen| screen.state::<HexView>())
        .is_some_and(|hex| {
            let cursor = hex.buffer.cursor(view);
            hex.pending.is_some() || cursor.overwrite || cursor.anchor.is_some()
        })
}

/// Whether the focused window shows a hex screen.
pub fn focused(editor: &Editor) -> bool {
    editor
        .focused_screen()
        .is_some_and(|screen| screen.state::<HexView>().is_some())
}

/// The state of the focused hex screen.
fn state(editor: &mut Editor) -> Option<&mut HexView> {
    let screen = editor.tree.focused_screen()?;
    editor.screens.state_mut::<HexView>(screen)
}

/// The hex window `view`, with what moving in it needs.
struct Target<'a> {
    hex: &'a mut HexBuffer,
    view: ViewId,
    rows: Rows,
    /// The number of rows on screen.
    height: u64,
    scrolloff: u64,
}

impl Target<'_> {
    /// The focused window.
    fn of(editor: &mut Editor) -> Option<Target<'_>> {
        let view = editor.tree.focus;
        Self::at(editor, view)
    }

    fn at(editor: &mut Editor, view: ViewId) -> Option<Target<'_>> {
        let scrolloff = editor.config().scrolloff as u64;
        let window = editor.tree.screen_view(view)?;
        let (screen, area) = (window.screen, hex_view::rows_area(window));
        let hex = &mut editor.screens.state_mut::<HexView>(screen)?.buffer;
        // (the screen shows in a new window when history shows it again)
        hex.adopt_cursor(view);
        let layout = hex.cursor(view).layout;
        let rows = Rows::new(layout, hex.len(), area.width);
        Some(Target {
            hex,
            view,
            rows,
            height: (area.height as u64).max(1),
            scrolloff,
        })
    }

    fn cursor(&self) -> HexCursor {
        self.hex.cursor(self.view)
    }

    fn last(&self) -> u64 {
        self.hex.len().saturating_sub(1)
    }

    fn per_row(&self) -> u64 {
        self.rows.bytes_per_row as u64
    }

    /// Moves the cursor to `offset` (at most the last byte), extending the selection while
    /// selecting, and scrolls to it.
    fn move_to(&mut self, offset: u64) {
        let last = self.last();
        let cursor = self.hex.cursor_mut(self.view);
        cursor.offset = offset.min(last);
        cursor.digit = 0;
        self.scroll_to_cursor();
    }

    /// Moves by `rows` rows (down when positive), staying in the file.
    fn move_rows(&mut self, rows: i64) {
        let cursor = self.cursor();
        let row = self.rows.row(cursor.offset) as i64;
        let count = self.rows.count(self.hex.len()) as i64;
        let target = (row + rows).clamp(0, count - 1);
        if target == row {
            return;
        }
        let offset = cursor.offset as i64 + (target - row) * self.per_row() as i64;
        self.move_to(offset as u64);
    }

    /// Scrolls so that the cursor is on screen, `scrolloff` rows from its edges when it can.
    fn scroll_to_cursor(&mut self) {
        let height = self.height;
        let scrolloff = self.scrolloff.min(height.saturating_sub(1) / 2);
        let count = self.rows.count(self.hex.len());
        let rows = self.rows;
        let cursor = self.hex.cursor_mut(self.view);
        let row = rows.row(cursor.offset);
        let mut top = rows.row(cursor.top);
        if row < top + scrolloff {
            top = row.saturating_sub(scrolloff);
        }
        if row + scrolloff >= top + height {
            top = row + scrolloff + 1 - height;
        }
        top = top.min(count.saturating_sub(height));
        cursor.top = top * rows.bytes_per_row as u64;
    }

    /// Scrolls by `rows` rows (down when positive), the cursor following when it would leave
    /// the screen.
    fn scroll(&mut self, rows: i64) {
        let count = self.rows.count(self.hex.len());
        let max_top = count.saturating_sub(self.height) as i64;
        let per_row = self.per_row();
        let height = self.height;
        let scrolloff = self.scrolloff.min(height.saturating_sub(1) / 2);
        let layout = self.rows;
        let last = self.last();
        let cursor = self.hex.cursor_mut(self.view);
        let top = (layout.row(cursor.top) as i64 + rows).clamp(0, max_top) as u64;
        cursor.top = top * per_row;
        let row = layout.row(cursor.offset);
        // (at the edges of the file, the cursor can go to the edge of the screen)
        let first = if top == 0 { 0 } else { top + scrolloff };
        let last_row = if top as i64 == max_top {
            top + height - 1
        } else {
            (top + height - 1).saturating_sub(scrolloff).max(first)
        };
        let target = row.clamp(first, last_row);
        if target != row {
            let column = cursor.offset % per_row;
            cursor.offset = (target * per_row + column).min(last);
            cursor.digit = 0;
        }
    }
}

/// Handles a key for the focused hex screen. Returns false for the keys the view leaves to the
/// keymap (and when the focus isn't on a hex screen).
pub fn handle_screen_key(cx: &mut Context, key: KeyEvent) -> bool {
    if !focused(cx.editor) {
        return false;
    }
    // the view has no insert or select mode of the editor's
    if cx.editor.mode != Mode::Normal {
        cx.editor.mode = Mode::Normal;
    }
    let Some(target) = Target::of(cx.editor) else {
        return false;
    };
    if target.cursor().overwrite {
        if let Some(state) = state(cx.editor) {
            state.pending = None;
        }
        // memory can only be overwritten while its program is paused, and that can change
        // while overwriting (it runs, a rewind starts)
        if let Some(why) = crate::debug::memory::not_editable(cx.editor) {
            if let Some(target) = Target::of(cx.editor) {
                target.hex.commit();
                let cursor = target.hex.cursor_mut(target.view);
                cursor.overwrite = false;
                cursor.digit = 0;
            }
            cx.editor.set_status(why);
            return true;
        }
        return overwrite_key(cx, key);
    }
    let pending = state(cx.editor).and_then(|state| state.pending.take());
    if let Some(prefix) = pending {
        goto_key(cx, prefix, key);
        return true;
    }
    read_only_key(cx, key)
}

/// `Esc` the view has no use of: the hex screen closes and the window shows what it covered.
/// Bytes overwritten and not written stay: the screen is kept.
fn close(cx: &mut Context) {
    let view = cx.editor.tree.focus;
    if state(cx.editor).is_some_and(|state| state.is_modified()) {
        cx.editor.set_error("unwritten bytes: :w writes them");
        return;
    }
    crate::screens::close(cx.editor, view);
}

fn read_only_key(cx: &mut Context, key: KeyEvent) -> bool {
    use KeyCode::*;
    if key.code == Char('g') && key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        if let Some(state) = state(cx.editor) {
            state.pending = Some('g');
        }
        return true;
    }
    let Some(mut target) = Target::of(cx.editor) else {
        return false;
    };
    let cursor = target.cursor();
    let per_row = target.per_row();
    let half = (target.height / 2).max(1) as i64;
    let page = target.height as i64;
    let ctrl = key.modifiers == KeyModifiers::CONTROL;
    let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();

    match key.code {
        Char('d') if ctrl => {
            target.scroll(half);
            target.move_rows(half);
        }
        Char('u') if ctrl => {
            target.scroll(-half);
            target.move_rows(-half);
        }
        Char('f') if ctrl => {
            target.scroll(page);
            target.move_rows(page);
        }
        Char('b') if ctrl => {
            target.scroll(-page);
            target.move_rows(-page);
        }
        PageDown => {
            target.scroll(page);
            target.move_rows(page);
        }
        PageUp => {
            target.scroll(-page);
            target.move_rows(-page);
        }
        _ if !plain => return false,
        Char('h') | Left => target.move_to(cursor.offset.saturating_sub(1)),
        Char('l') | Right => target.move_to(cursor.offset + 1),
        Char('j') | Down => target.move_rows(1),
        Char('k') | Up => target.move_rows(-1),
        // a word is 4 bytes, from a multiple of 4
        Char('w') => target.move_to((cursor.offset / 4 + 1) * 4),
        Char('b') => target.move_to(cursor.offset.saturating_sub(1) / 4 * 4),
        Home => target.move_to(cursor.offset / per_row * per_row),
        End => target.move_to(cursor.offset / per_row * per_row + per_row - 1),
        Tab => {
            let cursor = target.hex.cursor_mut(target.view);
            cursor.column = match cursor.column {
                Column::Bytes => Column::Ascii,
                Column::Ascii => Column::Bytes,
            };
        }
        Char('~') => {
            let cursor = target.hex.cursor_mut(target.view);
            cursor.layout = match cursor.layout {
                Layout::Hex => Layout::Bin,
                Layout::Bin => Layout::Hex,
            };
            // the rows change width
            if let Some(mut target) = Target::of(cx.editor) {
                target.scroll_to_cursor();
            }
        }
        Char('v') => {
            let cursor = target.hex.cursor_mut(target.view);
            cursor.anchor = match cursor.anchor {
                Some(_) => None,
                None => Some(cursor.offset),
            };
        }
        Char(';') => target.hex.cursor_mut(target.view).anchor = None,
        // Esc drops a selection; with none it closes the view
        Esc if cursor.anchor.is_some() => target.hex.cursor_mut(target.view).anchor = None,
        Esc => close(cx),
        Char('y') => yank(cx),
        Char('n') => search_next(cx.editor, Direction::Forward),
        Char('N') => search_next(cx.editor, Direction::Backward),
        Char('/') => search_prompt(cx, Direction::Forward),
        Char('?') => cx.push_layer(Box::new(key_help())),
        Char('G') => goto_prompt(cx),
        Char('u') | Char('U') => undo(cx, key.code == Char('u')),
        Char('i') => {
            if target.hex.is_memory() {
                if let Some(why) = crate::debug::memory::not_editable(cx.editor) {
                    cx.editor.set_status(why);
                    return true;
                }
            }
            let Some(target) = Target::of(cx.editor) else {
                return true;
            };
            if target.hex.is_empty() {
                cx.editor
                    .set_status("the file is empty: no byte to overwrite");
            } else {
                let cursor = target.hex.cursor_mut(target.view);
                cursor.overwrite = true;
                cursor.anchor = None;
                cursor.digit = 0;
            }
        }
        // the command line, the space menu and the register for `y`
        Char(':') | Char(' ') | Char('"') => return false,
        Char(_) => cx.editor.set_status(READ_ONLY),
        _ => return false,
    }
    true
}

/// The keys of the hex view, for the key help overlay (`?`).
fn key_help() -> ui::keyhelp::KeyHelp {
    ui::keyhelp::KeyHelp::new(
        "Hex view",
        &[
            (
                "move",
                vec![
                    ("h l, j k", "previous, next byte, row"),
                    ("w b", "next, previous word (4 bytes)"),
                    ("gg ge", "first, last byte"),
                    ("gh gl, Home End", "start, end of the row"),
                    ("C-d C-u, C-f C-b", "half a page, a page"),
                    ("Tab", "hex (or bin), ASCII column"),
                    ("~", "hex, bin mode"),
                    ("G", "go to an offset"),
                ],
            ),
            (
                "search and select",
                vec![
                    ("/", "search hex bytes or \"text\""),
                    ("n N", "next, previous match"),
                    ("v", "select bytes (; or Esc stops)"),
                    ("y", "yank as hex text"),
                ],
            ),
            (
                "overwrite",
                vec![
                    ("i", "overwrite mode (Esc stops)"),
                    ("u U", "undo, redo"),
                    (":w", "write the changed bytes in place"),
                ],
            ),
            ("view", vec![("?", "these keys")]),
        ],
    )
}

/// `u` (`undo`) or `U`. In memory, a step already written is written back at once, so
/// memory keeps what the view shows: only while the program is paused.
fn undo(cx: &mut Context, undo: bool) {
    let Some(target) = Target::of(cx.editor) else {
        return;
    };
    let memory = target.hex.is_memory();
    if memory && target.hex.next_step_written(undo) {
        if let Some(why) = crate::debug::memory::not_editable(cx.editor) {
            cx.editor.set_status(why);
            return;
        }
    }
    let Some(mut target) = Target::of(cx.editor) else {
        return;
    };
    let step = if undo {
        target.hex.undo_step()
    } else {
        target.hex.redo_step()
    };
    match step {
        Some(step) => {
            target.move_to(step.offset);
            if memory && step.written {
                crate::debug::memory::write_back(cx.editor, step.offsets, undo);
            }
        }
        None if undo => cx.editor.set_status("Already at oldest change"),
        None => cx.editor.set_status("Already at newest change"),
    }
}

/// The second key of a `g` sequence.
fn goto_key(cx: &mut Context, prefix: char, key: KeyEvent) {
    // history (this fork): the hex view is a place in a window's history
    match (prefix, key.code) {
        ('g', KeyCode::Char('a')) => return crate::history::back(cx),
        ('g', KeyCode::Char('A')) => return crate::history::history_picker(cx),
        _ => {}
    }
    let Some(mut target) = Target::of(cx.editor) else {
        return;
    };
    if target.hex.is_memory() && matches!(key.code, KeyCode::Char('g' | 'e')) && prefix == 'g' {
        cx.editor.set_status("memory has no first or last byte");
        return;
    }
    let offset = target.cursor().offset;
    let per_row = target.per_row();
    match (prefix, key.code) {
        ('g', KeyCode::Char('g')) => target.move_to(0),
        ('g', KeyCode::Char('e')) => target.move_to(u64::MAX),
        ('g', KeyCode::Char('h')) => target.move_to(offset / per_row * per_row),
        ('g', KeyCode::Char('l')) => target.move_to(offset / per_row * per_row + per_row - 1),
        _ => (),
    }
}

/// A key in overwrite mode: typing overwrites bytes, the arrows move by digit.
fn overwrite_key(cx: &mut Context, key: KeyEvent) -> bool {
    use KeyCode::*;
    let Some(mut target) = Target::of(cx.editor) else {
        return false;
    };
    let cursor = target.cursor();
    let digits = cursor.layout.digits();
    let page = target.height as i64;
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return false;
    }
    match key.code {
        Esc => {
            target.hex.commit();
            let cursor = target.hex.cursor_mut(target.view);
            cursor.overwrite = false;
            cursor.digit = 0;
        }
        Left | Backspace => {
            if cursor.column == Column::Bytes && cursor.digit > 0 {
                target.hex.cursor_mut(target.view).digit -= 1;
            } else if cursor.offset > 0 {
                target.move_to(cursor.offset - 1);
                if cursor.column == Column::Bytes {
                    target.hex.cursor_mut(target.view).digit = digits - 1;
                }
            }
        }
        Right => {
            if cursor.column == Column::Bytes && cursor.digit + 1 < digits {
                target.hex.cursor_mut(target.view).digit += 1;
            } else if cursor.offset < target.last() {
                target.move_to(cursor.offset + 1);
            }
        }
        Down => target.move_rows(1),
        Up => target.move_rows(-1),
        PageDown => {
            target.scroll(page);
            target.move_rows(page);
        }
        PageUp => {
            target.scroll(-page);
            target.move_rows(-page);
        }
        Home => target.move_to(cursor.offset / target.per_row() * target.per_row()),
        End => target
            .move_to(cursor.offset / target.per_row() * target.per_row() + target.per_row() - 1),
        Tab => {
            let cursor = target.hex.cursor_mut(target.view);
            cursor.column = match cursor.column {
                Column::Bytes => Column::Ascii,
                Column::Ascii => Column::Bytes,
            };
            cursor.digit = 0;
        }
        Char(c) => {
            if let Err(message) = type_char(&mut target, c) {
                cx.editor.set_status(message);
            }
        }
        _ => (),
    }
    true
}

/// Overwrites with a typed character, in the column of the cursor: a hex digit overwrites a
/// nibble, a bit a bit, and in the ASCII column a character its byte (or bytes, past ASCII).
fn type_char(target: &mut Target, c: char) -> Result<(), &'static str> {
    let cursor = target.cursor();
    let last = target.last();
    let Some(byte) = target.hex.byte(cursor.offset) else {
        if target.hex.is_memory() {
            return Err("this memory is unreadable (??): nothing to overwrite there");
        }
        return Ok(());
    };
    match (cursor.column, cursor.layout) {
        (Column::Bytes, layout) => {
            let value = match layout {
                Layout::Hex => {
                    let nibble = c
                        .to_digit(16)
                        .ok_or("type hex digits here (0-9, a-f), or Tab for the ASCII column")?
                        as u8;
                    if cursor.digit == 0 {
                        (nibble << 4) | (byte & 0x0f)
                    } else {
                        (byte & 0xf0) | nibble
                    }
                }
                Layout::Bin => {
                    let mask = 0x80 >> cursor.digit;
                    match c {
                        '0' => byte & !mask,
                        '1' => byte | mask,
                        _ => return Err("type 0 or 1 here, or Tab for the ASCII column"),
                    }
                }
            };
            target.hex.overwrite(cursor.offset, value);
            let digits = layout.digits();
            let cursor = target.hex.cursor_mut(target.view);
            if cursor.digit + 1 < digits {
                cursor.digit += 1;
            } else if cursor.offset < last {
                cursor.offset += 1;
                cursor.digit = 0;
            }
        }
        (Column::Ascii, _) => {
            let mut buf = [0; 4];
            let encoded = c.encode_utf8(&mut buf).as_bytes();
            // all of the character or nothing: half of it would be a broken UTF-8 sequence
            if cursor.offset + encoded.len() as u64 - 1 > last {
                return Err("no room for this character before the end of the file");
            }
            if (0..encoded.len() as u64).any(|i| target.hex.byte(cursor.offset + i).is_none()) {
                return Err("this memory is unreadable (??): nothing to overwrite there");
            }
            for (i, &byte) in encoded.iter().enumerate() {
                target.hex.overwrite(cursor.offset + i as u64, byte);
            }
            target.hex.cursor_mut(target.view).offset =
                (cursor.offset + encoded.len() as u64).min(last);
        }
    }
    target.scroll_to_cursor();
    Ok(())
}

/// `y`: yanks the selection (or the byte under the cursor) as hex text, `de ad be ef`.
fn yank(cx: &mut Context) {
    // (`"a` before `y` goes through the keymap, which leaves the register in the editor)
    let register = cx
        .register
        .or_else(|| cx.editor.selected_register.take())
        .unwrap_or(cx.editor.config().default_yank_register);
    let Some(target) = Target::of(cx.editor) else {
        return;
    };
    let cursor = target.cursor();
    if target.hex.is_empty() {
        cx.editor.set_status("the file is empty: nothing to yank");
        return;
    }
    let selection = cursor.selection();
    let len = selection.end - selection.start;
    if len > MAX_YANK {
        cx.editor.set_error(format!(
            "too many bytes to yank: {}, at most {}",
            hex_view::size(len),
            hex_view::size(MAX_YANK)
        ));
        return;
    }
    let text = hex::hex_text(&target.hex.bytes(selection));
    target.hex.cursor_mut(target.view).anchor = None;
    match cx.editor.registers.write(register, vec![text]) {
        Ok(()) => cx.editor.set_status(format!(
            "yanked {} {} as hex to register {register}",
            len,
            if len == 1 { "byte" } else { "bytes" }
        )),
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

// Searching

fn search_prompt(cx: &mut Context, direction: Direction) {
    let prompt = ui::Prompt::new(
        match direction {
            Direction::Forward => "search bytes:".into(),
            Direction::Backward => "reverse search bytes:".into(),
        },
        Some('/'),
        ui::completers::none,
        move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate || input.is_empty() {
                return;
            }
            match hex::parse_pattern(input) {
                Ok(pattern) => {
                    if let Some(state) = state(cx.editor) {
                        state.buffer.search = Some(pattern);
                    }
                    search_next(cx.editor, direction);
                }
                Err(message) => cx.editor.set_error(message),
            }
        },
    );
    cx.push_layer(Box::new(prompt));
}

/// Moves to the next match of the last search, after the cursor (forward) or before it.
fn search_next(editor: &mut Editor, direction: Direction) {
    let Some(mut target) = Target::of(editor) else {
        return;
    };
    let Some(pattern) = target.hex.search.clone() else {
        editor.set_status("no search yet");
        return;
    };
    let offset = target.cursor().offset;
    let found = match direction {
        Direction::Forward => target.hex.find(&pattern, offset + 1, true),
        Direction::Backward => target.hex.find(&pattern, offset, false),
    };
    match found {
        Some((at, wrapped)) => {
            target.move_to(at);
            if wrapped {
                editor.set_status("Wrapped around document");
            }
        }
        None => {
            // memory is too big to search through: only the bytes read so far are searched
            let read = target.hex.memory().map(|memory| memory.cached_bytes());
            match read {
                Some(read) => editor.set_error(format!(
                    "not found in the {} of memory read so far: {}",
                    hex_view::size(read),
                    hex::hex_text(&pattern)
                )),
                None => editor.set_error(format!("not found: {}", hex::hex_text(&pattern))),
            }
        }
    }
}

// Going to an offset

fn goto_prompt(cx: &mut Context) {
    let memory = state(cx.editor).is_some_and(|state| state.buffer.is_memory());
    let prompt = ui::Prompt::new(
        if memory {
            "goto address:"
        } else {
            "goto offset:"
        }
        .into(),
        None,
        ui::completers::none,
        |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
            if event == PromptEvent::Validate && !input.trim().is_empty() {
                if let Err(err) = goto(cx.editor, input) {
                    cx.editor.set_error(err.to_string());
                }
            }
        },
    );
    cx.push_layer(Box::new(prompt));
}

/// Moves the cursor of the focused view to an offset, hex after `0x` (`0x1a4`), else decimal.
/// An offset past the end goes to the last byte. Fails when the focus isn't on a hex screen.
pub fn goto(editor: &mut Editor, input: &str) -> anyhow::Result<()> {
    let offset = hex::parse_offset(input).map_err(anyhow::Error::msg)?;
    let Some(mut target) = Target::of(editor) else {
        anyhow::bail!("not in the hex view");
    };
    let len = target.hex.len();
    target.move_to(offset);
    if offset >= len {
        editor.set_status(format!(
            "0x{offset:X} is past the end ({}): at the last byte",
            hex_view::size(len)
        ));
    }
    Ok(())
}

// The mouse

/// Handles a mouse event over a hex screen's window: the wheel scrolls it, a click puts the
/// cursor on a byte (in the column clicked), dragging selects. `None` for the events over other
/// windows, which the editor handles.
pub fn handle_mouse(editor: &mut Editor, event: &MouseEvent) -> Option<EventResult> {
    let MouseEvent {
        kind, row, column, ..
    } = *event;
    let is_hex = |editor: &Editor, view: ViewId| {
        editor
            .tree
            .screen_view(view)
            .is_some_and(|window| editor.screens.state::<HexView>(window.screen).is_some())
    };
    let view_id = match kind {
        // dragging goes on in the window where it started
        MouseEventKind::Drag(MouseButton::Left) => editor.tree.focus,
        _ => editor.tree.windows().find_map(|(window, _)| {
            let window = match window {
                vortex_view::tree::Window::Screen(window) => window,
                _ => return None,
            };
            let area = hex_view::rows_area(window);
            let inside = (area.left()..area.right()).contains(&column)
                && (area.top()..area.bottom()).contains(&row);
            inside.then_some(window.id)
        })?,
    };
    if !is_hex(editor, view_id) {
        return None;
    }
    let lines = editor.config().scroll_lines.max(1) as i64;
    match kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            if let Some(mut target) = Target::at(editor, view_id) {
                target.scroll(if kind == MouseEventKind::ScrollDown {
                    lines
                } else {
                    -lines
                });
            }
        }
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
            if matches!(kind, MouseEventKind::Down(_)) {
                editor.focus(view_id);
            }
            let area = hex_view::rows_area(editor.tree.screen_view(view_id)?);
            let mut target = Target::at(editor, view_id)?;
            let Some((offset, clicked)) = byte_at(&target, area, column, row) else {
                return Some(EventResult::Consumed(None));
            };
            let cursor = target.hex.cursor_mut(target.view);
            if matches!(kind, MouseEventKind::Down(_)) {
                cursor.anchor = None;
                cursor.column = clicked;
            } else if cursor.anchor.is_none() {
                cursor.anchor = Some(cursor.offset);
            }
            target.move_to(offset);
        }
        _ => return None,
    }
    Some(EventResult::Consumed(None))
}

/// The byte drawn at a screen position, and its column.
fn byte_at(target: &Target, area: Rect, column: u16, row: u16) -> Option<(u64, Column)> {
    let rows = target.rows;
    let x = column.checked_sub(area.x)? as usize;
    let y = row.checked_sub(area.y)? as u64;
    let row_start = (rows.row(target.cursor().top) + y) * rows.bytes_per_row as u64;
    let (i, clicked) = if x >= rows.ascii_x() {
        (x - rows.ascii_x(), Column::Ascii)
    } else {
        let i = (0..rows.bytes_per_row)
            .rev()
            .find(|&i| rows.byte_x(i) <= x)?;
        (i, Column::Bytes)
    };
    let offset = row_start + i.min(rows.bytes_per_row - 1) as u64;
    (offset < target.hex.len()).then_some((offset, clicked))
}

/// `:hex`: shows the current buffer's file in the hex view, a screen over its window, or (on the
/// hex screen) back as text.
pub fn toggle(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let editor = &mut *cx.editor;
    if focused(editor) {
        let view = editor.tree.focus;
        let Some(state) = state(editor) else {
            return Ok(());
        };
        if state.buffer.is_memory() {
            anyhow::bail!("this is a program's memory: it has no text to show");
        }
        if state.is_modified() {
            anyhow::bail!("unwritten bytes: :w writes them");
        }
        crate::screens::close(editor, view);
        return Ok(());
    }
    let doc = doc!(editor);
    let Some(path) = doc.path().map(ToOwned::to_owned) else {
        anyhow::bail!("the hex view shows a file: write the buffer to one first");
    };
    if doc.is_modified() {
        anyhow::bail!("the hex view shows the file on disk: write or :reload the buffer first");
    }
    editor.open_hex(&path, Placement::Cover)?;
    Ok(())
}

// Writing and reloading

/// The name of the focused hex screen if it has bytes overwritten and not written.
pub fn unwritten(editor: &Editor) -> Option<String> {
    let screen = editor.focused_screen()?;
    screen
        .state::<HexView>()
        .is_some_and(|hex| hex.is_modified())
        .then(|| screen.name.clone())
}

/// `:w` on the focused hex screen: its overwritten bytes are written, in place; memory sends them
/// to the program.
pub fn write(editor: &mut Editor, force: bool) -> anyhow::Result<()> {
    let Some(screen) = editor.tree.focused_screen() else {
        return Ok(());
    };
    // memory isn't a file: `:w` sends its overwritten bytes to the program
    if editor
        .screens
        .state::<HexView>(screen)
        .is_some_and(|hex| hex.buffer.is_memory())
    {
        return crate::debug::memory::write(editor);
    }
    write_screen(editor, screen, force)
}

/// Writes the overwritten bytes of the hex screen `screen` to its file, in place, and says how
/// many. A file changed on disk since it was read is only overwritten with `force`. The buffer
/// of the same file, if open, sees the change on disk.
pub fn write_screen(
    editor: &mut Editor,
    screen: vortex_view::screen::ScreenId,
    force: bool,
) -> anyhow::Result<()> {
    let name = editor
        .screens
        .get(screen)
        .map(|screen| screen.name.clone())
        .unwrap_or_default();
    let Some(hex) = editor.screens.state_mut::<HexView>(screen) else {
        return Ok(());
    };
    if hex.buffer.is_memory() {
        return Ok(());
    }
    if !force && hex.disk_change() == vortex_view::document::DiskChange::Changed {
        anyhow::bail!("{name} changed on disk: :write! to overwrite it");
    }
    let written = hex.write()?;
    let path = hex.path.clone();
    editor.set_status(format!(
        "'{name}' written, {written} byte{} changed in place",
        if written == 1 { "" } else { "s" },
    ));
    if let Some(path) = path {
        crate::handlers::disk::check_paths(editor, &[path]);
    }
    Ok(())
}

/// `:reload` on the focused hex screen: the bytes as they are on disk, the overwritten ones
/// dropped.
pub fn reload(editor: &mut Editor) -> anyhow::Result<()> {
    let Some(hex) = state(editor) else {
        return Ok(());
    };
    if hex.buffer.is_memory() {
        anyhow::bail!("memory isn't a file: it can't be reloaded");
    }
    hex.reload()?;
    Ok(())
}
