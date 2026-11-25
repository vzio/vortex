//! The dashboard (this fork, see FORK.md, "Dashboard"): the main menu `vx` and `vx <folder>`
//! open on, and `:dashboard` brings back. A logo, the project's git state and languages, and
//! the actions to start with, each a key.
//!
//! It isn't a buffer nor a window: it is an overlay of the compositor, over everything (every
//! window and the statuslines), that takes every key while it is up. Typing does nothing: only
//! `Esc` closes it, and its own actions, which open what they open (a picker over it, `Esc` in
//! which comes back to it; what the picker opens closes it). Nothing underneath changes while it
//! is up, so closing it leaves the windows, screens and cursors as they were: on a bare `vx`, the
//! empty buffer.
//!
//! The stats are read in the background ([`stats`]) and the drawing ([`render`]) animates them in
//! as they arrive, on the clock of [`timeline`]: the title opens from `VX` to `VORTEX` once
//! everything is in, over a [`vortex`] that spins, slows down or stands still as configured.
//! Once nothing moves anymore, no more frames are asked for.

pub mod charset;
pub mod languages;
pub mod render;
pub mod stats;
pub mod timeline;
pub mod title;
pub mod vortex;
pub mod wrap;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use vortex_loader::workspace_trust::TrustQuery;
use vortex_view::document::Mode;
use vortex_view::graphics::{CursorKind, Rect};
use vortex_view::input::KeyEvent;
use vortex_view::keyboard::KeyCode;
use vortex_view::{screen::Content, Editor, ViewId};

use tui::buffer::Buffer as Surface;

use crate::commands::{self, MappableCommand};
use crate::compositor::{self, Component, Compositor, Event, EventResult};
use crate::keymap::Keymaps;
use vortex_view::editor::DashboardBackground;

pub use render::Hints;
pub use stats::Stats;
pub use timeline::Timeline;

pub struct Dashboard {
    stats: Arc<Stats>,
    timeline: Timeline,
    dir: PathBuf,
    hints: Hints,
    /// What the editor showed when it opened: an action that opened something else (a file
    /// picked in its picker) closes it.
    under: Under,
    /// It went (an action opened something): drawn no more, and off the compositor at the next
    /// event.
    gone: bool,
    /// Only the vortex (`:vortex`).
    vortex_only: bool,
    /// The glitches of the title and the background, from frame to frame.
    glitches: std::cell::RefCell<render::Glitches>,
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        self.stats.closed.store(true, Ordering::Relaxed);
    }
}

/// The compositor id of the dashboard's layer.
pub const ID: &str = "dashboard";

/// What a key does on the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyResult {
    /// Not one of its keys: nothing happens (it takes every key).
    Ignored,
    /// Done, the dashboard stays (a picker opened over it).
    Consumed,
    /// Done, and the dashboard goes.
    Close,
}

/// What the editor shows, for the dashboard to see that one of its actions opened something.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Under {
    focus: ViewId,
    content: Option<Content>,
    windows: usize,
    documents: usize,
}

impl Under {
    fn of(editor: &Editor) -> Self {
        Self {
            focus: editor.tree.focus,
            content: editor.window_content(editor.tree.focus),
            windows: editor.tree.windows().count(),
            documents: editor.documents.len(),
        }
    }
}

/// Opens the dashboard over everything (`:dashboard`, startup), or `:vortex` alone.
pub fn open(compositor: &mut Compositor, editor: &mut Editor, vortex_only: bool) {
    let Some(view) = compositor.find::<crate::ui::EditorView>() else {
        return;
    };
    let dashboard = if vortex_only {
        Dashboard::open_vortex(editor, &view.keymaps)
    } else {
        Dashboard::open(editor, &view.keymaps)
    };
    compositor.remove(ID);
    compositor.push(Box::new(dashboard));
}

/// Whether the dashboard is up (a screen of its own, which keeps the main theme).
pub fn is_up(compositor: &mut Compositor) -> bool {
    compositor
        .find_id::<Dashboard>(ID)
        .is_some_and(|dashboard| !dashboard.gone)
}

impl Dashboard {
    /// Opens the dashboard over the editor, which it leaves as it is.
    pub fn open(editor: &mut Editor, keymaps: &Keymaps) -> Self {
        let dir = vortex_stdx::env::current_working_dir();
        let (stats, timeline) = load(editor, dir.clone());
        Self {
            stats,
            timeline,
            dir,
            hints: hints(keymaps),
            under: Under::of(editor),
            gone: false,
            vortex_only: false,
            glitches: Default::default(),
        }
    }

    /// `:vortex`: only the vortex and the logo (unless `dashboard-title = false`), as
    /// `dashboard-background` moves it (`off` shows it still), over the focused view like the
    /// dashboard, and gone the same way.
    pub fn open_vortex(editor: &mut Editor, keymaps: &Keymaps) -> Self {
        let mut dashboard = Self::open(editor, keymaps);
        dashboard.vortex_only = true;
        if dashboard.timeline.background == DashboardBackground::Off {
            dashboard.timeline.background = DashboardBackground::Static;
        }
        dashboard
    }

    /// A dashboard with the given stats, for tests: nothing is read.
    pub fn with_stats(editor: &Editor, stats: Arc<Stats>, born: Instant, hints: Hints) -> Self {
        Self {
            stats,
            timeline: Timeline::new(born, editor.config().dashboard_background),
            dir: vortex_stdx::env::current_working_dir(),
            hints,
            under: Under::of(editor),
            gone: false,
            vortex_only: false,
            glitches: Default::default(),
        }
    }

    /// Reads the stats again, its animations from the start, when the working directory is no
    /// longer the one they were read in (`:cd`, a pick in `:worktrees`): the dashboard always
    /// shows the working directory.
    pub fn follow_cwd(&mut self, editor: &Editor) {
        let dir = vortex_stdx::env::current_working_dir();
        // `:vortex` shows no stats
        if dir == self.dir || self.vortex_only {
            return;
        }
        let (stats, timeline) = load(editor, dir.clone());
        self.stats.closed.store(true, Ordering::Relaxed);
        self.stats = stats;
        self.timeline = timeline;
        self.dir = dir;
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn timeline(&self) -> Timeline {
        self.timeline
    }

    /// Whether an action of the dashboard opened something (a file picked in its picker, the
    /// file tree's): then it goes.
    fn opened_something(&self, editor: &Editor) -> bool {
        Under::of(editor) != self.under
    }

    /// Handles `key`, one of its own (`i`, `Esc`, `f`, `e`, `l` and `w` in a git repository, `t`
    /// with other worktrees, `:`, `q`); any other does nothing.
    pub fn handle_key(&self, key: KeyEvent, cx: &mut commands::Context) -> KeyResult {
        if !key.modifiers.is_empty() && key.modifiers != vortex_view::keyboard::KeyModifiers::SHIFT
        {
            return KeyResult::Ignored;
        }
        let command = match key.code {
            KeyCode::Esc => return KeyResult::Close,
            KeyCode::Char('i') => {
                MappableCommand::insert_mode.execute(cx);
                return KeyResult::Close;
            }
            // the command line, over it: what it opens closes it
            KeyCode::Char(':') => MappableCommand::command_mode,
            KeyCode::Char('f') => MappableCommand::file_picker,
            KeyCode::Char('e') => MappableCommand::toggle_explorer_root,
            KeyCode::Char('l') if self.stats.shows_git() => MappableCommand::commit_picker,
            // watch mode, on the editor: the dashboard closes
            KeyCode::Char('w') if self.stats.shows_git() => {
                if let Ok(watch) = ":watch".parse::<MappableCommand>() {
                    watch.execute(cx);
                }
                return KeyResult::Close;
            }
            KeyCode::Char('t') if self.stats.shows_worktrees() => {
                match ":worktrees".parse::<MappableCommand>() {
                    Ok(worktrees) => worktrees,
                    Err(_) => return KeyResult::Ignored,
                }
            }
            KeyCode::Char('q') => match ":quit".parse::<MappableCommand>() {
                Ok(quit) => quit,
                Err(_) => return KeyResult::Ignored,
            },
            _ => return KeyResult::Ignored,
        };
        command.execute(cx);
        KeyResult::Consumed
    }

    pub fn render_now(&self, editor: &Editor, area: Rect, surface: &mut Surface) {
        self.render_at(editor, area, surface, Instant::now());
    }

    /// Draws the dashboard as it is at `now`.
    pub fn render_at(&self, editor: &Editor, area: Rect, surface: &mut Surface, now: Instant) {
        let model = render::Model {
            stats: &self.stats,
            timeline: self.timeline,
            dir: &self.dir,
            hints: &self.hints,
            title: editor.config().dashboard_title,
            vortex_only: self.vortex_only,
            glitches: Some(&self.glitches),
        };
        render::render(&model, &editor.theme, area, surface, now);
    }
}

impl Component for Dashboard {
    fn handle_event(&mut self, event: &Event, cx: &mut compositor::Context) -> EventResult {
        // gone: off the compositor, and the event goes on to what is under it
        if self.gone || self.opened_something(cx.editor) {
            return EventResult::Ignored(Some(Box::new(|compositor: &mut Compositor, _| {
                compositor.remove(ID);
            })));
        }
        let Event::Key(key) = event else {
            // the mouse and pastes do nothing either while it is up
            return match event {
                Event::Mouse(_) | Event::Paste(_) => EventResult::Consumed(None),
                _ => EventResult::Ignored(None),
            };
        };
        let mut ccx = commands::Context {
            editor: cx.editor,
            count: None,
            register: None,
            callback: Vec::new(),
            on_next_key_callback: None,
            jobs: cx.jobs,
        };
        let result = self.handle_key(*key, &mut ccx);
        let mut callbacks = std::mem::take(&mut ccx.callback);
        if result == KeyResult::Close {
            self.gone = true;
            callbacks.insert(
                0,
                Box::new(|compositor: &mut Compositor, _: &mut compositor::Context| {
                    compositor.remove(ID);
                }),
            );
        }
        if callbacks.is_empty() {
            return EventResult::Consumed(None);
        }
        EventResult::Consumed(Some(Box::new(
            move |compositor: &mut Compositor, cx: &mut compositor::Context| {
                for callback in callbacks {
                    callback(compositor, cx)
                }
            },
        )))
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut compositor::Context) {
        if self.gone || self.opened_something(cx.editor) {
            self.gone = true;
            return;
        }
        self.follow_cwd(cx.editor);
        self.render_now(cx.editor, area, surface);
    }

    fn cursor(
        &self,
        _area: Rect,
        _editor: &Editor,
    ) -> (Option<vortex_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}

/// Starts reading the stats of `dir`, on a new timeline.
fn load(editor: &Editor, dir: PathBuf) -> (Arc<Stats>, Timeline) {
    let timeline = Timeline::new(Instant::now(), editor.config().dashboard_background);
    let trust_full = editor
        .workspace_trust
        .query(&vortex_loader::find_workspace_in(&dir).0, TrustQuery::Git)
        .is_trusted();
    let stats = stats::load(
        dir,
        trust_full,
        editor.syn_loader.load_full(),
        editor.config().changes.clone(),
        timeline,
    );
    (stats, timeline)
}

/// The shortest keys of the normal mode keymap for the actions.
pub fn hints(keymaps: &Keymaps) -> Hints {
    let map = keymaps.map();
    let Some(normal) = map.get(&Mode::Normal) else {
        return Hints::default();
    };
    let reverse = normal.reverse_map();
    let keys = |command: &str| -> Option<String> {
        let sequences = reverse.get(command)?;
        let shortest = sequences.iter().min_by_key(|keys| keys.len())?;
        Some(
            shortest
                .iter()
                .map(|key| key.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    Hints {
        files: keys("file_picker"),
        tree: keys("toggle_explorer_root"),
        history: keys("commit_picker"),
        worktrees: keys("worktrees").or_else(|| Some(":wt".to_string())),
        watch: keys("watch").or_else(|| Some(":watch".to_string())),
        quit: Some(":q".to_string()),
    }
}

#[cfg(test)]
mod cost {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use tui::buffer::Buffer as Surface;
    use vortex_view::editor::DashboardBackground;
    use vortex_view::graphics::Rect;

    use super::render::{self, tests as render_tests, Model};
    use super::Timeline;

    /// The time the first frame of each background mode takes, printed: each in a new thread,
    /// so it also computes the vortex's geometry, as the first frame of a dashboard does. Run it
    /// optimized: `cargo test -p vortex-term --lib --profile integration first_frame_cost --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn first_frame_cost() {
        let runs = 40;
        for mode in [
            DashboardBackground::On,
            DashboardBackground::Settle,
            DashboardBackground::Drain,
            DashboardBackground::Grow,
            DashboardBackground::GrowOn,
            DashboardBackground::Static,
            DashboardBackground::Off,
        ] {
            let mut total = Duration::ZERO;
            for _ in 0..runs {
                total += std::thread::spawn(move || {
                    let theme = vortex_view::theme::Loader::new(&[]).default();
                    let area = Rect::new(0, 0, 160, 50);
                    let mut surface = Surface::empty(area);
                    let born = Instant::now();
                    let stats = render_tests::loaded(born);
                    let hints = render_tests::hints();
                    let model = Model {
                        stats: &stats,
                        timeline: Timeline::new(born, mode),
                        dir: Path::new("/src/vortex"),
                        hints: &hints,
                        title: true,
                        vortex_only: false,
                        glitches: None,
                    };
                    let start = Instant::now();
                    render::render(&model, &theme, area, &mut surface, born);
                    start.elapsed()
                })
                .join()
                .unwrap();
            }
            println!("{mode:?}: first frame {:?} at 160x50", total / runs);
        }
    }
}
