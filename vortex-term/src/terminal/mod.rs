//! The terminals (fork feature): shells in windows of the tree, each a screen document (see
//! `vortex_view::screen`). `C-t` shows and hides the scratch terminal, the window menu opens more,
//! `:sh` runs a command in the scratch one, and a `vx` in a terminal opens files in this editor.
//! A terminal's window has modes of its own ([`modes`]): insert types in the shell, normal takes
//! Vortex keys, view walks its lines. The shell itself is `vortex-terminal`'s; this module draws
//! it, routes keys and the mouse to it, and starts and stops it.

pub mod edit;
pub mod input;
pub mod modes;
mod mouse;
pub mod phosphor;
mod render;
pub mod shim;
pub mod window;

pub use modes::{focused, viewing};
pub use mouse::handle_mouse;
pub use render::{cursor, render};
