//! Windows that aren't text documents (this fork), the drawing and input half: a screen
//! document's window (see `vortex_view::screen`) is drawn, given keys, pastes and the mouse, and
//! says its mode and cursor through here, by the kind of its screen. A new kind of screen adds its
//! arm to each function; the tree, focus, hiding and the buffer picker are `vortex-view`'s.

use tui::buffer::Buffer as Surface;
use vortex_core::Position;
use vortex_view::{
    graphics::CursorKind,
    input::{KeyEvent, MouseEvent},
    screen::ScreenKind,
    Document, Editor, View,
};

/// The kind of screen the focused window shows, if it shows one.
fn focused_kind(editor: &Editor) -> Option<ScreenKind> {
    let view = editor.tree.try_get(editor.tree.focus)?;
    editor.document(view.doc)?.screen_kind()
}

/// Draws a screen document in its window (not the statusline). Returns whether `doc` is one.
pub fn render(
    editor: &Editor,
    doc: &Document,
    view: &View,
    surface: &mut Surface,
    focused: bool,
) -> bool {
    match doc.screen_kind() {
        Some(ScreenKind::Terminal) => {
            crate::terminal::render(editor, doc, view, surface, focused);
            true
        }
        None => false,
    }
}

/// The cursor of the focused screen, if the focus is on one.
pub fn cursor(editor: &Editor) -> Option<(Option<Position>, CursorKind)> {
    match focused_kind(editor)? {
        ScreenKind::Terminal => {
            crate::terminal::cursor(editor).map(|(pos, kind)| (Some(pos), kind))
        }
    }
}

/// A key for the focused screen, before the keymap. `idle`: nothing is pending (keys, a count,
/// a register). Returns whether the screen took it.
pub fn handle_key(editor: &mut Editor, key: KeyEvent, idle: bool) -> bool {
    match focused_kind(editor) {
        Some(ScreenKind::Terminal) => {
            crate::terminal::modes::handle_key(editor, key, idle)
                == crate::terminal::modes::Handled::Yes
        }
        None => false,
    }
}

/// A paste for the focused screen. Returns whether the screen took it.
pub fn handle_paste(editor: &mut Editor, text: &str) -> bool {
    match focused_kind(editor) {
        Some(ScreenKind::Terminal) => crate::terminal::modes::handle_paste(editor, text),
        None => false,
    }
}

/// A mouse event over a screen's window. Returns whether a screen took it.
pub fn handle_mouse(editor: &mut Editor, mouse: &MouseEvent) -> bool {
    crate::terminal::handle_mouse(editor, mouse)
}

/// Before a static command runs: whether it acts on the focused screen itself (then the fork's
/// modes don't hold it).
pub fn before_command(editor: &mut Editor, name: &str) -> bool {
    match focused_kind(editor) {
        Some(ScreenKind::Terminal) => crate::terminal::modes::before_command(editor, name),
        None => false,
    }
}

/// After a key went through the keymap with a screen focused.
pub fn after_key(editor: &mut Editor) {
    if let Some(ScreenKind::Terminal) = focused_kind(editor) {
        crate::terminal::modes::after_key(editor)
    }
}

/// The mode a screen's window shows in the statusline instead of the editor's, if any.
pub fn mode_label(editor: &Editor, doc: &Document) -> Option<&'static str> {
    match doc.screen_kind()? {
        ScreenKind::Terminal => {
            let terminal = editor.terminals.get(doc.id())?;
            (terminal.viewing() && editor.mode() == vortex_view::document::Mode::Normal)
                .then_some("VIEW")
        }
    }
}

/// The layer of keys of the focused screen, looked up before the fork's modes' layers.
pub fn layer(editor: &Editor) -> Option<crate::keymap::Layer> {
    match focused_kind(editor)? {
        ScreenKind::Terminal => Some(if crate::terminal::viewing(editor) {
            crate::keymap::Layer::TerminalView
        } else {
            crate::keymap::Layer::Terminal
        }),
    }
}
