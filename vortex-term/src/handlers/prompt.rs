use vortex_event::register_hook;
use vortex_view::events::DocumentFocusLost;
use vortex_view::handlers::Handlers;

use crate::job::{self};
use crate::ui;

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        // focusing a screen (this fork) leaves the document where it was: a prompt the screen
        // opens right after (the debug panel's `c`, before this job runs) must stay
        if event.editor.focused_screen().is_some() {
            return Ok(());
        }
        job::dispatch_blocking(move |_, compositor| {
            if compositor.find::<ui::Prompt>().is_some() {
                compositor.remove_type::<ui::Prompt>();
            }
        });
        Ok(())
    });
}
