//! The debugger (fork feature): sessions with the debug adapters configured in
//! `languages.toml`, and the debug panel, a screen in a window of its own (variables, watch,
//! call stack, breakpoints, console). The state the panel shows is `vortex-view`'s `editor.debug`; this
//! module talks to the adapters, loads that state, draws it and routes keys to it.

pub mod bar;
pub mod breakpoints;
pub mod cpu;
pub mod engine;
pub mod launch;
pub mod load;
pub mod memory;
pub mod mode;
pub mod panel;
pub mod playback;
pub mod record;
pub(crate) mod render;
pub mod rewind;
mod session;
pub mod threads;

pub use panel::{
    focus_section, handle_key, handle_mouse, show, toggle, toggle_focus, Handled, FOCUS_KEY,
};
pub use render::render;
pub use session::{end, end_with, handle_message, start};

use vortex_view::debug::ThreadKey;

/// The key of thread `thread` of `session`.
pub fn spectator_key(
    session: vortex_dap::registry::DebugAdapterId,
    thread: vortex_dap::ThreadId,
) -> ThreadKey {
    ThreadKey { session, thread }
}
