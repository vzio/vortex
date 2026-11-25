//! Screens (this fork), the drawing and input half: a screen's window (see `vortex_view::screen`)
//! is drawn, given keys, pastes and the mouse, and says its mode and cursor through here, by the
//! kind of its screen. A new kind of screen adds its arm to each function; the tree, focus,
//! covering, hiding and the buffer picker are `vortex-view`'s.
//!
//! A screen draws within its window (`ScreenView::inner_area`, the window less its statusline,
//! which [`render_statusline`] draws for every kind), never over the rest of the editor.
//! Its keys come before the keymap ([`handle_event`]); what it leaves goes to the keymap, where
//! only the commands about windows and the editor run on a screen ([`allows`]): a text command
//! has no text to act on there.

use tui::buffer::Buffer as Surface;
use vortex_core::Position;
use vortex_view::{
    document::Mode,
    graphics::{CursorKind, Rect},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    screen::{Closed, Placement, Screen, ScreenId, ScreenKind},
    tree::ScreenView,
    Document, Editor, ViewId,
};

use crate::commands;
use crate::compositor::{self, Compositor, EventResult};

/// The kind of screen the focused window shows, if it shows one.
fn focused_kind(editor: &Editor) -> Option<ScreenKind> {
    editor.focused_screen().map(|screen| screen.kind)
}

/// Whether a screen of this kind draws its own status row, the last of its window (a page's
/// position, a diff's hunks); the others get [`render_statusline`].
fn own_status_row(kind: ScreenKind) -> bool {
    matches!(
        kind,
        ScreenKind::Markdown | ScreenKind::Diff | ScreenKind::DebugPanel
    )
}

/// Draws the screen's window `view` (its screen and its statusline), within its area.
pub fn render(
    cx: &mut compositor::Context,
    view: &ScreenView,
    surface: &mut Surface,
    focused: bool,
) {
    let Some(kind) = cx.editor.screens.kind(view.screen) else {
        return;
    };
    if kind == ScreenKind::Diff {
        if let Some(mut state) = cx.editor.screens.take_state(view.screen) {
            if let Some(diff) = state.downcast_mut::<crate::ui::diff_view::DiffView>() {
                use crate::compositor::Component;
                diff.set_focused(focused);
                diff.render(view.area, surface, cx);
            }
            cx.editor.screens.put_state(view.screen, state);
        }
        return;
    }
    let editor = &mut *cx.editor;
    let area = if own_status_row(kind) {
        view.area
    } else {
        view.inner_area()
    };
    surface.set_style(area, editor.theme.get("ui.background"));
    match kind {
        ScreenKind::Terminal => crate::terminal::render(editor, view, surface, focused),
        ScreenKind::Markdown => {
            with_state::<crate::markdown_view::MarkdownView, _>(
                editor,
                view.screen,
                |markdown, editor| markdown.render(editor, area, surface, focused),
            );
        }
        ScreenKind::DebugPanel => crate::debug::render(editor, area, surface),
        ScreenKind::Hex | ScreenKind::Diff => {}
    }
    if !own_status_row(kind) {
        render_statusline(editor, view, surface, focused);
    }
}

/// Runs `f` on the state of `screen`, a `T`, with the editor at hand (the state is out of the
/// editor meanwhile). `None` when the screen is gone or isn't a `T`.
pub fn with_state<T: 'static, R>(
    editor: &mut Editor,
    screen: ScreenId,
    f: impl FnOnce(&mut T, &mut Editor) -> R,
) -> Option<R> {
    let mut state = editor.screens.take_state(screen)?;
    let result = state.downcast_mut::<T>().map(|state| f(state, editor));
    editor.screens.put_state(screen, state);
    result
}

/// `Esc` on a screen (once its own modal layers took theirs), or its own way out: the window
/// shows what the screen covered, or closes when it covered nothing (the scratch terminal and
/// the debug panel are only hidden). A screen that covered a text window is kept in that
/// window's history, so that `ga` shows it again, lightened of what only an open screen needs
/// (a diff's watch of the working tree).
pub fn close(editor: &mut Editor, view: ViewId) {
    if let Some((shown, screen)) = leave(editor, view) {
        editor.record_closed_screen(shown, screen);
    }
}

/// Closes the screen of window `view` without recording it: the text window it uncovered and
/// the screen, when history is to keep it (lightened). A window of its own hides or goes.
pub fn leave(editor: &mut Editor, view: ViewId) -> Option<(ViewId, Screen)> {
    let screen = editor.tree.screen_view(view)?.screen;
    if editor.tree.under(view).is_none() {
        editor.close_screen_window(view);
        return None;
    }
    let mut kept = editor.screens.remove(screen);
    let Closed::Uncovered(shown) = editor.close_screen_window(view) else {
        return None;
    };
    let mut screen = kept.take()?;
    (editor.tree.try_get(shown).is_some() && kept_by_history(&mut screen))
        .then_some((shown, screen))
}

/// Whether history keeps the closed `screen` to show it again, lightening it first.
fn kept_by_history(screen: &mut Screen) -> bool {
    match screen.kind {
        ScreenKind::Markdown => true,
        ScreenKind::Diff => {
            if let Some(diff) = screen.state_mut::<crate::ui::diff_view::DiffView>() {
                diff.lighten();
            }
            true
        }
        ScreenKind::Hex => true,
        ScreenKind::Terminal | ScreenKind::DebugPanel => false,
    }
}

/// Shows again, over the focused text window, a screen that history kept: on its buffer, if it
/// has one (a page's source), which the window shows first. Returns whether it could.
pub fn reopen(editor: &mut Editor, screen: Screen) -> bool {
    if let Some(source) = screen.source {
        if editor.document(source).is_none() {
            return false;
        }
        let shows = editor
            .tree
            .try_get(editor.tree.focus)
            .is_some_and(|view| view.doc == source);
        if !shows {
            editor.without_history(|editor| {
                editor.switch(source, vortex_view::editor::Action::Replace)
            });
        }
    }
    let covered = editor.tree.focus;
    let kind = screen.kind;
    let id = editor.add_screen(screen);
    match kind {
        ScreenKind::Markdown => {
            if let Some(markdown) = editor
                .screens
                .state_mut::<crate::markdown_view::MarkdownView>(id)
            {
                markdown.reopened(covered);
            }
        }
        ScreenKind::Diff => {
            with_state::<crate::ui::diff_view::DiffView, _>(editor, id, |diff, editor| {
                diff.returned(editor)
            });
        }
        ScreenKind::Hex | ScreenKind::Terminal | ScreenKind::DebugPanel => {}
    }
    editor.open_screen(id, Placement::Cover);
    true
}

/// Runs `f` with a command context made of `cx`, and gives back what it pushed for the
/// compositor as one callback.
fn with_commands<R>(
    cx: &mut compositor::Context,
    f: impl FnOnce(&mut commands::Context) -> R,
) -> (R, Option<compositor::Callback>) {
    let mut ccx = commands::Context {
        editor: cx.editor,
        count: None,
        register: None,
        callback: Vec::new(),
        on_next_key_callback: None,
        jobs: cx.jobs,
    };
    let result = f(&mut ccx);
    let callbacks = std::mem::take(&mut ccx.callback);
    let callback: Option<compositor::Callback> = (!callbacks.is_empty()).then(|| {
        Box::new(
            move |compositor: &mut Compositor, cx: &mut compositor::Context| {
                for callback in callbacks {
                    callback(compositor, cx)
                }
            },
        ) as compositor::Callback
    });
    (result, callback)
}

/// The statusline of a screen's window: the mode the screen is in, its name, and what the screen
/// says of itself on the right (a diff's hunk, the hex view's offset).
pub fn render_statusline(editor: &Editor, view: &ScreenView, surface: &mut Surface, focused: bool) {
    let Some(screen) = editor.screens.get(view.screen) else {
        return;
    };
    let area = Rect::new(
        view.area.x,
        view.area.bottom().saturating_sub(1),
        view.area.width,
        1.min(view.area.height),
    );
    if area.height == 0 {
        return;
    }
    let theme = &editor.theme;
    let base = if focused {
        theme.get("ui.statusline")
    } else {
        theme.get("ui.statusline.inactive")
    };
    surface.set_style(area, base);
    let mut x = area.x;
    if focused {
        let (label, scope) = mode_of(editor, view.screen);
        let style = base.patch(theme.get(scope));
        let text = format!(" {label} ");
        surface.set_stringn(x, area.y, &text, area.width as usize, style);
        x += text.len() as u16;
    }
    let name = format!(" {}", screen.name);
    let right = status_of(editor, view.screen);
    let room = area.right().saturating_sub(x) as usize;
    surface.set_stringn(x, area.y, &name, room, base);
    if !right.is_empty() {
        let width = right.chars().count() as u16 + 1;
        if area.width > width + (x - area.x) + name.len() as u16 {
            surface.set_stringn(area.right() - width, area.y, &right, width as usize, base);
        }
    }
}

/// The mode a screen shows in its statusline, and the theme's scope for it.
fn mode_of(editor: &Editor, screen: ScreenId) -> (&'static str, &'static str) {
    match editor.mode() {
        Mode::Insert if editor.screens.kind(screen) == Some(ScreenKind::Terminal) => {
            ("INS", "ui.statusline.insert")
        }
        Mode::Select => ("SEL", "ui.statusline.select"),
        _ => ("NOR", "ui.statusline.normal"),
    }
}

/// What a screen says of itself on the right of its statusline.
fn status_of(_editor: &Editor, _screen: ScreenId) -> String {
    String::new()
}

/// The cursor of the focused screen, if the focus is on one (or on a terminal's lines).
pub fn cursor(editor: &Editor) -> Option<(Option<Position>, CursorKind)> {
    if let Some(cursor) = crate::terminal::cursor(editor) {
        return Some((Some(cursor.0), cursor.1));
    }
    match focused_kind(editor)? {
        ScreenKind::Terminal => None,
        ScreenKind::Hex | ScreenKind::Diff | ScreenKind::Markdown | ScreenKind::DebugPanel => {
            Some((None, CursorKind::Hidden))
        }
    }
}

/// An event for the focused screen (or a terminal's lines), before the keymap. `idle`: nothing
/// is pending (keys, a count, a register). `None` when the screen leaves it to the keymap.
pub fn handle_event(
    event: &compositor::Event,
    cx: &mut compositor::Context,
    idle: bool,
) -> Option<EventResult> {
    match event {
        compositor::Event::Key(key) => handle_key(cx, *key, idle),
        compositor::Event::Paste(text) => {
            handle_paste(cx.editor, text).then_some(EventResult::Consumed(None))
        }
        _ => None,
    }
}

fn handle_key(cx: &mut compositor::Context, key: KeyEvent, idle: bool) -> Option<EventResult> {
    if crate::terminal::focused(cx.editor).is_some() {
        let handled = crate::terminal::modes::handle_key(cx.editor, key, idle)
            == crate::terminal::modes::Handled::Yes;
        return handled.then_some(EventResult::Consumed(None));
    }
    let view = cx.editor.tree.focus;
    let screen = cx.editor.tree.focused_screen()?;
    match focused_kind(cx.editor)? {
        ScreenKind::Terminal => None,
        ScreenKind::Markdown => {
            // its own keys (a count for `]`); not while keys are pending
            if !idle && cx.editor.count.is_none() {
                return None;
            }
            use crate::markdown_view::{KeyResult, MarkdownView};
            let mut state = cx.editor.screens.take_state(screen)?;
            let (result, callback) = with_commands(cx, |ccx| {
                state
                    .downcast_mut::<MarkdownView>()
                    .map(|markdown| markdown.handle_key(key, ccx))
            });
            cx.editor.screens.put_state(screen, state);
            match result? {
                KeyResult::Ignored => None,
                KeyResult::Close => {
                    crate::markdown_view::close(cx.editor, true);
                    let _ = view;
                    Some(EventResult::Consumed(callback))
                }
                KeyResult::Consumed => Some(EventResult::Consumed(callback)),
            }
        }
        ScreenKind::Diff => {
            use crate::compositor::Component;
            let mut state = cx.editor.screens.take_state(screen)?;
            let result = state
                .downcast_mut::<crate::ui::diff_view::DiffView>()
                .map(|diff| diff.handle_event(&compositor::Event::Key(key), cx));
            cx.editor.screens.put_state(screen, state);
            match result? {
                EventResult::Ignored(_) => None,
                result => Some(result),
            }
        }
        ScreenKind::DebugPanel => {
            // in normal mode, with nothing pending; a key it doesn't take goes on to debug mode's
            // layer and the keymap (`Space` and what follows)
            if !idle || cx.editor.mode() != Mode::Normal {
                return None;
            }
            match crate::debug::handle_key(cx.editor, key) {
                crate::debug::Handled::No => None,
                crate::debug::Handled::Yes => Some(EventResult::Consumed(None)),
                crate::debug::Handled::Prompt(prompt) => {
                    Some(EventResult::Consumed(Some(Box::new(
                        move |compositor: &mut Compositor, _: &mut compositor::Context| {
                            compositor.push(prompt)
                        },
                    ))))
                }
            }
        }
        ScreenKind::Hex => None,
    }
}

/// A paste for the focused screen. Returns whether the screen took it.
fn handle_paste(editor: &mut Editor, text: &str) -> bool {
    if crate::terminal::focused(editor).is_some() {
        return crate::terminal::modes::handle_paste(editor, text);
    }
    // a paste is text: a screen has none to put it in
    editor.is_screen_view(editor.tree.focus)
}

/// A mouse event over a screen's window. Returns what the screen did with it: a click focuses
/// the window, then the screen has it. `None` when it isn't over a screen.
pub fn handle_mouse_event(cx: &mut compositor::Context, mouse: &MouseEvent) -> Option<EventResult> {
    let editor = &mut *cx.editor;
    let over = |area: Rect| {
        mouse.column >= area.x
            && mouse.column < area.right()
            && mouse.row >= area.y
            && mouse.row < area.bottom()
    };
    let screen = editor.tree.windows().find_map(|(window, _)| match window {
        vortex_view::tree::Window::Screen(view) if over(view.area) => Some(view.screen),
        _ => None,
    });
    if let Some(screen) =
        screen.filter(|&screen| editor.screens.kind(screen) == Some(ScreenKind::Diff))
    {
        use crate::compositor::Component;
        if let Some(view) = editor.view_of(screen) {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && editor.tree.focus != view
            {
                editor.focus(view);
            }
        }
        let mut state = cx.editor.screens.take_state(screen)?;
        let result = state
            .downcast_mut::<crate::ui::diff_view::DiffView>()
            .map(|diff| diff.handle_event(&compositor::Event::Mouse(*mouse), cx));
        cx.editor.screens.put_state(screen, state);
        return result;
    }
    handle_mouse(cx.editor, mouse).then_some(EventResult::Consumed(None))
}

/// A mouse event over a screen's window. Returns whether a screen took it: a click focuses the
/// window, then the screen has it.
fn handle_mouse(editor: &mut Editor, mouse: &MouseEvent) -> bool {
    if crate::terminal::handle_mouse(editor, mouse) {
        return true;
    }
    let over = |area: Rect| {
        mouse.column >= area.x
            && mouse.column < area.right()
            && mouse.row >= area.y
            && mouse.row < area.bottom()
    };
    let Some((view, screen)) = editor.tree.windows().find_map(|(window, _)| match window {
        vortex_view::tree::Window::Screen(view) if over(view.area) => Some((view.id, view.screen)),
        _ => None,
    }) else {
        return false;
    };
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && editor.tree.focus != view {
        editor.focus(view);
    }
    match editor.screens.kind(screen) {
        Some(ScreenKind::Markdown) => {
            with_state::<crate::markdown_view::MarkdownView, _>(editor, screen, |page, editor| {
                page.handle_mouse(mouse, editor);
            });
        }
        Some(
            ScreenKind::Hex | ScreenKind::Diff | ScreenKind::DebugPanel | ScreenKind::Terminal,
        )
        | None => {}
    }
    true
}

/// Whether the static command `name` may run with the focus where it is: anything on a text
/// window; on a screen's window, the screen's own (a terminal's, which may walk its lines) and
/// the commands about windows and the editor, not the ones about text, which it has none of.
pub fn allows(editor: &Editor, name: &str) -> bool {
    let Some(kind) = focused_kind(editor) else {
        return true;
    };
    if editor.screens.lent.is_some() && editor.screens.lent == editor.tree.focused_screen() {
        return true;
    }
    if kind == ScreenKind::Terminal && crate::terminal::modes::takes(name) {
        return true;
    }
    acts_on_windows(name)
}

/// The static commands about windows and the editor, not about the text in a window: they run
/// on a screen's window (a file picked opens in the window for documents).
fn acts_on_windows(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "jump_view_",
        "swap_view_",
        "rotate_view",
        "dap_",
        "debug",
        "watch",
    ];
    const NAMES: &[&str] = &[
        "transpose_view",
        "hsplit",
        "vsplit",
        "hsplit_new",
        "vsplit_new",
        "wclose",
        "wonly",
        "toggle_terminal",
        "hsplit_terminal",
        "vsplit_terminal",
        "file_picker",
        "file_picker_in_current_directory",
        "file_explorer_in_current_buffer_directory",
        "buffer_picker",
        "last_picker",
        "global_search",
        "changed_file_picker",
        "commit_picker",
        "workspace_diagnostics_picker",
        "lsp_or_syntax_workspace_symbol_picker",
        "command_mode",
        "command_palette",
        "history_back",
        "history_forward",
        "history_picker",
        "bookmark_picker",
        "next_bookmark",
        "prev_bookmark",
        "toggle_explorer",
        "toggle_explorer_root",
        "select_register",
        "no_op",
        "normal_mode",
        "suspend",
        "goto_next_buffer",
        "goto_previous_buffer",
        "goto_last_accessed_file",
        "goto_last_modified_file",
        "markdown_view",
        "markdown_view_vsplit",
        "git_diff_view",
        "git_diff_view_vsplit",
        "line_history",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || NAMES.contains(&name)
}

/// Before a static command runs: whether it acts on the focused screen itself (then the fork's
/// modes don't hold it).
pub fn before_command(editor: &mut Editor, name: &str) -> bool {
    if crate::terminal::focused(editor).is_some() {
        return crate::terminal::modes::before_command(editor, name);
    }
    false
}

/// After a key went through the keymap with a screen focused.
pub fn after_key(editor: &mut Editor) {
    if crate::terminal::focused(editor).is_some() {
        crate::terminal::modes::after_key(editor)
    }
}

/// The mode a text window of a screen's lines shows in the statusline instead of the editor's
/// (a terminal's in view mode: `VIEW`).
pub fn mode_label(editor: &Editor, doc: &Document) -> Option<&'static str> {
    let screen = doc.lines_of()?;
    let terminal = editor.terminals.get(screen)?;
    (terminal.viewing() && editor.mode() == Mode::Normal).then_some("VIEW")
}

/// The layer of keys of the focused screen, looked up before the fork's modes' layers.
pub fn layer(editor: &Editor) -> Option<crate::keymap::Layer> {
    crate::terminal::focused(editor)?;
    Some(if crate::terminal::viewing(editor) {
        crate::keymap::Layer::TerminalView
    } else {
        crate::keymap::Layer::Terminal
    })
}
