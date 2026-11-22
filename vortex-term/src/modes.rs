//! The fork's modes, watch mode and debug mode, as one thing (this fork): which one is on, leaving
//! it, and the background of its screen.
//!
//! A mode and its screen are atomic: everything a mode changes (its keys, the bar, the cursor
//! drawn as in select mode while looking around, the background, a focused debug panel) is read
//! from whether the mode is on, and leaving a mode, however it is left (`Esc`, its command, the
//! other mode starting), goes through [`restore_editing`], which gives plain editing back where
//! the mode was showing.

use tui::buffer::Buffer as Surface;
use vortex_view::{
    document::Mode as EditMode,
    graphics::{Color, Rect, Style},
    Editor, Theme,
};

/// The mode that is on: one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Watch,
    Debug,
}

/// The mode that is on, if any.
pub fn current(editor: &Editor) -> Option<Mode> {
    if editor.watch.is_active() {
        Some(Mode::Watch)
    } else if editor.debug.mode_on() {
        Some(Mode::Debug)
    } else {
        None
    }
}

/// Leaves the mode that is on, if any: watch mode as with `Esc` (its session goes on recording),
/// debug mode (its session goes on as it was).
pub fn leave(editor: &mut Editor) {
    match current(editor) {
        Some(Mode::Watch) => crate::watch::leave_mode(editor),
        Some(Mode::Debug) => crate::debug::mode::leave(editor),
        None => {}
    }
}

/// Plain editing, once a mode was left: normal mode, the keys and the cursor of the buffer (a
/// focused debug panel gives them back), on the file and the line the mode was showing (the
/// focused view, where the mode left the cursor). Called by every way out of a mode.
pub fn restore_editing(editor: &mut Editor) {
    if current(editor).is_some() {
        return;
    }
    if editor.mode() != EditMode::Normal {
        editor.mode = EditMode::Normal;
    }
    editor.debug.unfocus();
    if let Some(session) = editor.watch.session.as_mut() {
        session.flash = None;
    }
}

/// The background of a mode's screen, over the views while a mode is on, so that it doesn't look
/// like a buffer being edited: `ui.background.mode`, else the popups' background.
pub fn screen_style(theme: &Theme) -> Style {
    let background = |style: Style| Style {
        bg: style.bg,
        ..Style::default()
    };
    theme
        .try_get_exact("ui.background.mode")
        .map(background)
        .filter(|style| style.bg.is_some())
        .unwrap_or_else(|| background(theme.get("ui.popup")))
}

/// Paints a mode's screen over `area` (the views): the cells with the plain background
/// (`ui.background`'s, or none) take [`screen_style`]'s; the rest keep theirs.
pub fn paint_screen(theme: &Theme, area: Rect, surface: &mut Surface) {
    let Some(screen) = screen_style(theme).bg else {
        return;
    };
    let plain = theme.get("ui.background").bg.unwrap_or(Color::Reset);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = surface.get_mut(x, y) {
                if cell.bg == plain || cell.bg == Color::Reset {
                    cell.bg = screen;
                }
            }
        }
    }
}
