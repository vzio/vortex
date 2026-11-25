//! The mouse over a terminal's window: a click focuses it, and the rest goes to the program when
//! it asked for the mouse; the wheel scrolls the scrollback (or becomes arrow keys on the
//! alternate screen). In view mode the focused window takes the mouse as a document does.

use vortex_view::{
    input::{MouseButton, MouseEvent, MouseEventKind},
    Editor,
};

use super::input::{encode_mouse, MouseAction};

/// Sends the mouse event to the terminal under it. Returns whether it was a terminal's.
pub fn handle_mouse(editor: &mut Editor, mouse: &MouseEvent) -> bool {
    let over = |area: vortex_view::graphics::Rect| {
        mouse.column >= area.x
            && mouse.column < area.right()
            && mouse.row >= area.y
            && mouse.row < area.bottom()
    };
    // a terminal in view mode is covered by the text window of its lines, which takes the mouse
    // as a document does
    let Some((view, screen, area)) = editor.tree.windows().find_map(|(window, _)| {
        let vortex_view::tree::Window::Screen(view) = window else {
            return None;
        };
        editor.terminals.get(view.screen)?;
        let area = view.inner_area();
        over(area).then_some((view.id, view.screen, area))
    }) else {
        return false;
    };
    let focused = editor.tree.focus == view;
    if !focused {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            // the wheel scrolls a terminal without focusing it
            if !matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                return true;
            }
        } else {
            editor.focus(view);
        }
    }
    let scroll_lines = editor.config().scroll_lines.unsigned_abs().max(1) as u16;
    let Some(terminal) = editor.terminals.get(screen) else {
        return true;
    };
    let shell = &terminal.shell;
    let col = mouse.column - area.x;
    let row = mouse.row - area.y;
    match encode_mouse(mouse, col, row, scroll_lines, &shell.input_modes()) {
        MouseAction::Send(bytes) => shell.write(&bytes),
        MouseAction::Scroll(lines) => shell.scroll(lines),
        MouseAction::None => {}
    }
    true
}
