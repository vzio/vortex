//! Watch mode (fork feature): follow the changes made to the working tree as they happen, while
//! an agent (or a formatter, a script, another editor) is at work.
//!
//! - `:watch` (or `vx --watch`) starts a session: the baseline is taken (see
//!   `vortex_view::watch`), the working tree is watched with the shared [`RepoWatch`] machinery
//!   (gitignore-aware, debounced, the open buffers reloaded with the stale-buffer rule) and every
//!   batch of changed files is diffed against the baseline on the watcher's thread.
//! - Watch mode is a mode: while it is on, its keys (`[keys.watch]`: `v`, `n`, `N`, `c`, `Esc`)
//!   come first and its state, change and activity are on the [mode bar](crate::ui::modebar).
//! - While following, a change opens its file (or switches to it), jumps to its first new hunk,
//!   centers it and flashes it; other keys don't move anything. `v` looks around: your own
//!   cursor, drawn as in select mode, moves and searches freely while new changes are only
//!   counted; `v` again (or `n` up to the newest change) follows again.
//! - Keys that would edit or open something else wait: the bar says that `Esc` leaves watch mode
//!   first. A typed command other than the watch commands and `:goto` leaves it.
//! - Leaving watch mode (`Esc`, `:watch`, a typed command, debug mode) keeps the session: it goes
//!   on recording the changes (markers) without following them, and `:watch` enters it again
//!   where it was, following or looking around. `:watch-stop` stops it for good; its changes stay
//!   (gutter markers, `n`/`N`, `:watch-changes`) until the next watch or `:watch-clear`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use tui::text::Span;
use vortex_core::Selection;
use vortex_event::register_hook;
use vortex_vcs::FileChange;
use vortex_view::input::{MouseButton, MouseEventKind};
use vortex_view::{
    editor::{Action, ChangesConfig},
    events::DocumentDidOpen,
    graphics::{Color, Modifier, Style},
    watch::{
        Baseline, FileKind, FileUpdate, Flash, Gone, HunkLines, Quiet, Rejected, Review, Session,
        SessionHunk, Step, Target,
    },
    Editor, Theme,
};

use crate::commands::changes::{self, ChangeRow, What};
use crate::compositor::{Compositor, Context};
use crate::job::Callback;
use crate::keymap::ReverseKeymap;
use crate::live::RepoWatch;
use crate::ui::{modebar, overlay::overlaid, picker};

/// How long the hunk following jumped to stays highlighted, fading out.
const FLASH: Duration = Duration::from_millis(900);
/// How often the flash is redrawn while it fades.
const FLASH_FRAME: Duration = Duration::from_millis(45);

static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// The statusline's mark of a left session blinks when a change arrives: lit this long...
const BLINK_LIT: Duration = Duration::from_secs(1);
/// ...every this long...
const BLINK_PERIOD: Duration = Duration::from_secs(4);
/// ...this many times, then it stays steady.
const BLINKS: u32 = 10;
/// The latest blinking: an earlier one's redraws stop.
static BLINKING: AtomicU64 = AtomicU64::new(0);

fn trust_full(editor: &Editor, cwd: &Path) -> bool {
    editor
        .workspace_trust
        .query(
            &vortex_loader::find_workspace_in(cwd).0,
            vortex_loader::workspace_trust::TrustQuery::Git,
        )
        .is_trusted()
}

/// `~/code/app` for the status line.
fn display(path: &Path) -> String {
    vortex_stdx::path::fold_home_dir(path).display().to_string()
}

/// `:watch` (and the dashboard's `w`): enters watch mode, resuming the session left with `Esc`
/// if there is one, else starting one; leaves it when it is on (the session goes on recording).
pub fn toggle(editor: &mut Editor) -> anyhow::Result<()> {
    if editor.watch.is_active() {
        leave_mode(editor);
        Ok(())
    } else if editor.watch.live().is_some() {
        resume(editor);
        Ok(())
    } else {
        start(editor)
    }
}

/// Enters watch mode again in the session left with `Esc`: where you were, following or looking
/// around as you were; the changes made meanwhile are counted as new.
fn resume(editor: &mut Editor) {
    // one mode at a time
    crate::debug::mode::leave(editor);
    let Some(session) = editor.watch.live() else {
        return;
    };
    session.resume();
    let new = session.new_count();
    session.activity = Some(match new {
        0 => "back: nothing changed meanwhile".to_string(),
        new => format!("back: {new} new change{} meanwhile", plural(new)),
    });
    catch_up(editor);
}

/// Coming back while following catches up: the changes made meanwhile are followed one after
/// another, in the order they happened, `watch-catch-up` seconds each, then following goes on
/// live. With `watch-catch-up = 0`, straight to the newest. Looking around, nothing moves: they
/// are counted as new.
fn catch_up(editor: &mut Editor) {
    let pace = editor.config().watch_catch_up.duration();
    let Some(session) = editor.watch.active() else {
        return;
    };
    if !session.following || session.missed().is_empty() {
        return;
    }
    if pace.is_zero() {
        let newest = session.missed().pop().expect("a change missed");
        go_to(editor, &newest, Action::Replace);
        if let Some(session) = editor.watch.session.as_mut() {
            session.followed(&newest);
        }
        return;
    }
    let id = session.id;
    if let Some(token) = session.start_catch_up() {
        catch_up_step(editor, id, token);
    }
}

/// Shows the oldest change not seen yet of the catch-up `token`, then waits `watch-catch-up`
/// for the next. With none left, the catch-up ends and following goes on live: the changes
/// made during it were queued behind the others.
fn catch_up_step(editor: &mut Editor, id: u64, token: u64) {
    let pace = editor.config().watch_catch_up.duration();
    let Some(session) = editor.watch.active().filter(|session| session.id == id) else {
        return;
    };
    if session.catch_up.as_ref().map(|catch_up| catch_up.token) != Some(token) {
        return;
    }
    let Some(target) = session.missed().into_iter().next() else {
        session.catch_up = None;
        session.activity = Some("caught up: following".into());
        return;
    };
    let baseline = session.baseline.clone();
    let place = format!(
        "{}:{}",
        relative(&baseline, &target.path),
        target.lines.start + 1
    );
    // a buffer with unsaved edits of its own is skipped, as following does
    let unsaved = editor
        .document_by_path(&target.path)
        .is_some_and(|doc| doc.is_modified());
    let activity = if unsaved {
        format!(
            "skipped {}: unsaved edits",
            relative(&baseline, &target.path)
        )
    } else {
        go_to(editor, &target, Action::Replace);
        format!("followed {place}")
    };
    if let Some(session) = editor.watch.session.as_mut() {
        session.caught_up(&target);
        session.activity = Some(activity);
    }
    tokio::spawn(async move {
        tokio::time::sleep(pace).await;
        crate::job::dispatch(move |editor, _| catch_up_step(editor, id, token)).await;
    });
}

/// Leaves watch mode, if it is on: the session goes on recording changes, without following
/// them, until watch mode is entered again or the session is stopped.
pub fn leave_mode(editor: &mut Editor) {
    if let Some(session) = editor.watch.active() {
        session.leave();
        crate::modes::restore_editing(editor);
    }
}

/// Starts a watch session in the repository of the working directory, dropping the previous
/// session's changes.
pub fn start(editor: &mut Editor) -> anyhow::Result<()> {
    let cwd = vortex_stdx::env::current_working_dir();
    let trust_full = trust_full(editor, &cwd);
    let state = editor
        .diff_providers
        .repo_state(&cwd, trust_full)
        .map_err(|_| anyhow::anyhow!("not in a git repository: nothing to watch"))?;
    // canonical, like the paths of the watcher's events and of the documents
    let work_dir = vortex_stdx::path::canonicalize(&state.work_dir);
    let git_dir = vortex_stdx::path::canonicalize(&state.git_dir);
    let canonical = |path: &Path| match path.strip_prefix(&state.work_dir) {
        Ok(relative) => work_dir.join(relative),
        Err(_) => vortex_stdx::path::canonicalize(path),
    };

    // the files already dirty: their content now is their baseline, so that earlier work
    // never counts as a change of the session
    let changes = editor
        .diff_providers
        .changed_files(&cwd, trust_full)
        .context("can't read the status of the repository")?;
    let mut dirty = HashMap::new();
    for change in &changes {
        let paths = match change {
            FileChange::Renamed { from_path, to_path } => vec![from_path.as_path(), to_path],
            change => vec![change.path()],
        };
        for path in paths {
            let path = canonical(path);
            let content = std::fs::read(&path).ok().map(Arc::from);
            dirty.insert(path, content);
        }
    }

    clear(editor);
    // one mode at a time
    crate::debug::mode::leave(editor);
    let baseline = Arc::new(Baseline::new(
        work_dir.clone(),
        git_dir,
        state.head,
        trust_full,
        editor.diff_providers.clone(),
        dirty,
    ));
    let id = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
    let config = editor.config().changes.clone();
    let reader = baseline.clone();
    let tap: crate::live::Tap = Box::new(move |paths: &[PathBuf]| {
        let updates = read_updates(&reader, &config, paths);
        Box::new(move |editor: &mut Editor| apply(editor, id, updates))
    });
    let Some(watcher) = RepoWatch::start_tapped(editor, &work_dir, Some(tap)) else {
        bail!("can't watch {}", display(&work_dir));
    };
    let mut session = Session::new(id, baseline);
    session.watcher = Some(Box::new(watcher));
    session.activity = Some(match changes.len() {
        0 => format!("watching {}", display(&work_dir)),
        dirty => format!(
            "watching {} · {dirty} file{} already uncommitted, not counted",
            display(&work_dir),
            plural(dirty)
        ),
    });
    editor.watch.session = Some(session);
    Ok(())
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Stops the session for good (`:watch-stop`), watch mode on or left: the working tree is no
/// longer watched. The changes stay. Returns the number of changes of the session, `None` when
/// no session was watching.
pub fn stop(editor: &mut Editor) -> Option<usize> {
    let session = editor.watch.live()?;
    session.stop();
    let count = session.change_count();
    crate::modes::restore_editing(editor);
    Some(count)
}

/// The result of `:watch-stop` for the status line: short.
pub fn ended_message(count: usize) -> String {
    format!("Watch ended: {count} change{}", plural(count))
}

/// Forgets the session: its markers, and `n`/`N` search again.
pub fn clear(editor: &mut Editor) {
    if editor.watch.session.take().is_none() {
        return;
    }
    for doc in editor.documents_mut() {
        doc.clear_watch_base();
    }
    crate::modes::restore_editing(editor);
}

/// The hash of some lines, or of a file's content.
fn hash(parts: &[&[&str]]) -> u64 {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Diffs the changed files against their baseline. Runs on the watcher's thread.
fn read_updates(baseline: &Baseline, config: &ChangesConfig, paths: &[PathBuf]) -> Vec<FileUpdate> {
    let mut updates = Vec::new();
    for path in paths {
        if !baseline.covers(path) || path.is_dir() {
            continue;
        }
        let now = match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            // being written, or unreadable: the next event will tell
            Err(_) => continue,
        };
        let before = baseline.get(path);
        let kind = match (&before, &now) {
            (before, now) if before.as_deref() == now.as_deref() => FileKind::Unchanged,
            (None, Some(_)) => FileKind::Created,
            (Some(_), None) => FileKind::Deleted,
            _ => FileKind::Modified,
        };
        let texts = (
            changes::text(before.as_deref()),
            changes::text(now.as_deref()),
        );
        let quiet = if config.is_lockfile(path) {
            Quiet::Lockfile
        } else if texts.0.is_err() || texts.1.is_err() {
            Quiet::Binary
        } else {
            Quiet::No
        };
        let mut hunks = Vec::new();
        if let (Quiet::No, FileKind::Modified | FileKind::Created, Ok(old), Ok(new)) =
            (quiet, kind, texts.0, texts.1)
        {
            let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
            let change = FileChange::Modified { path: path.clone() };
            let (old_lines, new_lines) = (lines(old), lines(new));
            let slice = |lines: &[&'_ str], range: &std::ops::Range<u32>| -> Vec<String> {
                let end = (range.end as usize).min(lines.len());
                let start = (range.start as usize).min(end);
                lines[start..end]
                    .iter()
                    .map(|line| line.to_string())
                    .collect()
            };
            for (line_hunk, _) in changes::hunks(&change, old, new) {
                let removed = slice(&old_lines, &line_hunk.before);
                let added = slice(&new_lines, &line_hunk.after);
                let removed: Vec<&str> = removed.iter().map(String::as_str).collect();
                let added: Vec<&str> = added.iter().map(String::as_str).collect();
                hunks.push(HunkLines {
                    key: hash(&[&removed, &added]),
                    before: line_hunk.before,
                    after: line_hunk.after,
                });
            }
        }
        let content = {
            let mut hasher = DefaultHasher::new();
            now.hash(&mut hasher);
            hasher.finish()
        };
        updates.push(FileUpdate {
            path: path.clone(),
            kind,
            quiet,
            hunks,
            content,
            modified: std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok(),
        });
    }
    updates
}

/// Takes in a batch of the session `id`, on the main thread, after the open buffers of the
/// changed files were reloaded: marks their buffers, and follows the newest change.
fn apply(editor: &mut Editor, id: u64, updates: Vec<FileUpdate>) {
    let Some(session) = editor.watch.live().filter(|session| session.id == id) else {
        return;
    };
    let applied = session.apply(updates);
    let baseline = session.baseline.clone();
    // out of watch mode (left with `Esc`) the changes are recorded, not followed: the
    // statusline's mark blinks
    if !session.active && !applied.changed.is_empty() {
        let now = Instant::now();
        session.recorded = Some(now);
        blink(now);
    }
    // out of watch mode the changes are recorded, not followed; during a catch-up, they wait
    // their turn behind the changes it is showing
    let following = session.active && session.following && session.catch_up.is_none();
    for path in &applied.changed {
        if let Some(doc) = editor.document_by_path_mut(path) {
            if doc.watch_diff().is_none() {
                doc.set_watch_base(baseline.get(path).as_deref());
            }
        }
    }
    // the files that changed but aren't followed into, for the bar
    let session = editor.watch.session.as_mut().unwrap();
    let quiet = applied.changed.iter().rev().find_map(|path| {
        let file = session.file(path)?;
        let note = match file.quiet {
            Quiet::Lockfile => "lockfile, not followed",
            Quiet::Binary => "binary, not followed",
            Quiet::No if file.kind == FileKind::Deleted => "deleted",
            Quiet::No => return None,
        };
        Some(format!("{} {note}", relative(&baseline, path)))
    });
    if !following {
        if let Some(note) = quiet {
            session.activity = Some(note);
        }
        return;
    }
    // the newest file, but a buffer with unsaved edits of its own is never overwritten: its file
    // is skipped (the statusline warns that it changed on disk)
    let unsaved = |editor: &Editor, target: &Target| {
        editor
            .document_by_path(&target.path)
            .is_some_and(|doc| doc.is_modified())
    };
    let skipped = applied
        .to_follow
        .iter()
        .rev()
        .find(|target| unsaved(editor, target))
        .map(|target| target.path.clone());
    let target = applied
        .to_follow
        .into_iter()
        .rev()
        .find(|target| !unsaved(editor, target));
    let mut activity = quiet;
    if let Some(target) = target {
        go_to(editor, &target, Action::Replace);
        activity = Some(format!(
            "followed {}:{}",
            relative(&baseline, &target.path),
            target.lines.start + 1
        ));
        if let Some(session) = editor.watch.session.as_mut() {
            session.followed(&target);
        }
    }
    if let Some(path) = skipped {
        activity = Some(format!(
            "skipped {}: unsaved edits",
            relative(&baseline, &path)
        ));
    }
    if let Some(session) = editor.watch.session.as_mut() {
        session.see_all();
        if activity.is_some() {
            session.activity = activity;
        }
    }
}

/// A path of the session, relative to its working tree.
fn relative(baseline: &Baseline, path: &Path) -> String {
    path.strip_prefix(baseline.work_dir())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Opens the file of `target` (or switches to the split that shows it), puts the cursor on the
/// first line of the hunk, centers it and flashes it.
fn go_to(editor: &mut Editor, target: &Target, action: Action) {
    // the session keeps its own order of changes (`n`/`N`, `:watch-changes`): following and `n`
    // are not history's jumps (a pick in `:watch-changes` is, as any picker's); with a terminal
    // focused (this fork), it happens in the window for documents, the terminal keeping the keys
    editor.without_history(|editor| {
        editor.in_document_window(|editor| go_to_unrecorded(editor, target, action))
    });
}

fn go_to_unrecorded(editor: &mut Editor, target: &Target, action: Action) {
    let shown = editor.document_by_path(&target.path).and_then(|doc| {
        let doc_id = doc.id();
        editor
            .tree
            .views()
            .find(|(view, _)| view.doc == doc_id)
            .map(|(view, _)| view.id)
    });
    match shown {
        Some(view_id) if matches!(action, Action::Replace) => editor.focus(view_id),
        _ => {
            if let Err(err) = editor.open(&target.path, action) {
                editor.set_error(format!("unable to open {}: {err}", target.path.display()));
                return;
            }
        }
    }
    let (view, doc) = vortex_view::current!(editor);
    if doc.watch_diff().is_none() {
        if let Some(session) = editor.watch.session.as_ref() {
            doc.set_watch_base(session.baseline.get(&target.path).as_deref());
        }
    }
    let text = doc.text();
    let last = text.len_lines().saturating_sub(1);
    let first = (target.lines.start as usize).min(last);
    let end = (target.lines.end as usize).clamp(first + 1, last + 1);
    doc.set_selection(view.id, Selection::point(text.line_to_char(first)));
    vortex_view::align_view(doc, view, vortex_view::Align::Center);
    let doc_id = doc.id();
    if let Some(session) = editor.watch.session.as_mut() {
        session.flash = Some(Flash {
            doc: doc_id,
            lines: first..end,
            start: Instant::now(),
        });
        tokio::spawn(async {
            let frames = FLASH.as_millis() / FLASH_FRAME.as_millis() + 1;
            for _ in 0..frames {
                tokio::time::sleep(FLASH_FRAME).await;
                vortex_event::request_redraw();
            }
        });
    }
}

/// `n` (`forward`) or `N`: goes to the next or previous change of the session, in the order
/// they happened. `false` when `n`/`N` should search instead: there is no session, or it ended
/// and a search was made since, or it ended without changes.
pub(crate) fn jump(editor: &mut Editor, forward: bool) -> bool {
    let Some(session) = editor.watch.session.as_ref() else {
        return false;
    };
    if !session.active && session.search_owns_n {
        return false;
    }
    let (view, doc) = vortex_view::current_ref!(editor);
    let cursor = doc.path().map(|path| {
        let text = doc.text().slice(..);
        let line = doc.selection(view.id).primary().cursor_line(text) as u32;
        (path, line)
    });
    let active = session.active;
    let target = match session.step(cursor, forward) {
        Some(Step::To(target)) => target,
        // the notes are the bar's (out of watch mode, nothing moves and nothing is said)
        step => {
            let note = match step {
                None if !active => return false,
                None => "no changes yet this session",
                Some(Step::AtNewest) => "at the newest change",
                _ => "at the oldest change",
            };
            if active {
                editor.watch.session.as_mut().unwrap().activity = Some(note.into());
            }
            return true;
        }
    };
    go_to(editor, &target, Action::Replace);
    let Some(session) = editor.watch.session.as_mut() else {
        return true;
    };
    if session.went_to(&target) {
        session.activity = Some("at the newest change: following again".into());
    }
    true
}

/// `v` in watch mode: look around (your own cursor, following stops), or follow again, from the
/// newest change.
pub(crate) fn look(editor: &mut Editor) {
    let Some(session) = editor.watch.active() else {
        editor.set_error("watch mode is off");
        return;
    };
    if !session.following {
        session.following = true;
        let newest = session.hunks_in_order().pop();
        session.activity = Some("following again".into());
        if let Some(target) = newest {
            go_to(editor, &target, Action::Replace);
            if let Some(session) = editor.watch.session.as_mut() {
                session.followed(&target);
            }
        }
    } else {
        session.pause();
        session.activity = None;
    }
}

/// Whether you are looking around in watch mode: the cursor is drawn as in select mode.
pub(crate) fn looking(editor: &Editor) -> bool {
    editor
        .watch
        .session
        .as_ref()
        .is_some_and(|session| session.active && !session.following)
}

/// `Esc` in watch mode: leaves it, to edit where you are; the session goes on recording, and
/// `:watch` comes back to it. Nothing is printed: the bar going away says it.
pub(crate) fn leave(editor: &mut Editor) {
    if !editor.watch.is_active() {
        editor.set_error("watch mode is off");
        return;
    }
    leave_mode(editor);
}

/// What a command is to watch mode (and, through [`effect_of`], to debug mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Runs either way: the watch commands, the command line, the palette, focusing the
    /// terminal panel, a bookmark.
    None,
    /// Moving (and selecting, scrolling, copying): runs while you look around, not while
    /// following.
    Move,
    /// Searching: as moving; once watch mode ended, `n`/`N` repeat the search.
    Search,
    /// Anything else (editing, opening a file yourself, a picker...): waits for watch mode to
    /// end (`Esc`).
    End,
}

/// The static commands that change nothing.
const NONE: &[&str] = &[
    "no_op",
    "watch_look",
    "watch_next_change",
    "watch_prev_change",
    "watch_changes",
    "watch_leave",
    "watch_accept",
    "watch_reject",
    "watch_yank",
    // entering debug mode ends watch mode (one mode at a time)
    "debug_mode",
    "normal_mode",
    "command_mode",
    "command_palette",
    "select_register",
    "search_next",
    "search_prev",
    "toggle_terminal",
    "rerun_last_command",
    // the terminals (this fork): a window of their own, and their own lines
    "hsplit_terminal",
    "vsplit_terminal",
    "terminal_view",
    "terminal_edit_command",
    "terminal_select",
    "terminal_next_prompt",
    "terminal_prev_prompt",
    "terminal_select_output",
    // setting or removing a bookmark (history and bookmarks) neither moves nor opens anything
    "toggle_bookmark",
];

/// The static commands that search.
const SEARCH: &[&str] = &[
    "search",
    "rsearch",
    "extend_search_next",
    "extend_search_prev",
    "search_selection",
    "search_selection_detect_word_boundaries",
    "make_search_word_bounded",
];

/// The static commands that move the cursor, the selections or the view, without opening anything
/// (besides the `move_`, `extend_`, `find_`, `till_`, `rotate_selections_` and `goto_` ones).
const MOVE: &[&str] = &[
    "repeat_last_motion",
    "page_up",
    "page_down",
    "half_page_up",
    "half_page_down",
    "page_cursor_up",
    "page_cursor_down",
    "page_cursor_half_up",
    "page_cursor_half_down",
    "select_all",
    "select_regex",
    "split_selection",
    "split_selection_on_newline",
    "merge_selections",
    "merge_consecutive_selections",
    "select_line_above",
    "select_line_below",
    "shrink_to_line_bounds",
    "collapse_selection",
    "flip_selections",
    "ensure_selections_forward",
    "select_mode",
    "exit_select_mode",
    "trim_selections",
    "keep_selections",
    "remove_selections",
    "keep_primary_selection",
    "remove_primary_selection",
    "copy_selection_on_next_line",
    "copy_selection_on_prev_line",
    "expand_selection",
    "shrink_selection",
    "select_next_sibling",
    "select_prev_sibling",
    "select_all_siblings",
    "select_all_children",
    "match_brackets",
    "select_textobject_around",
    "select_textobject_inner",
    "jump_view_right",
    "jump_view_left",
    "jump_view_up",
    "jump_view_down",
    "rotate_view",
    "rotate_view_reverse",
    "align_view_middle",
    "align_view_top",
    "align_view_center",
    "align_view_bottom",
    "scroll_up",
    "scroll_down",
    "yank",
    "yank_to_clipboard",
    "yank_to_primary_clipboard",
    "yank_joined",
    "yank_joined_to_clipboard",
    "yank_main_selection_to_clipboard",
    "yank_joined_to_primary_clipboard",
    "yank_main_selection_to_primary_clipboard",
    "goto_word",
    "extend_to_word",
];

/// The `goto_*` static commands that may open another buffer: they end watch mode. History
/// (`history_back`, `history_forward`, `history_picker`) and the bookmarks' `next_bookmark`,
/// `prev_bookmark` and `bookmark_picker` end it too, as any command in no list does.
const OPENS: &[&str] = &[
    "goto_definition",
    "goto_declaration",
    "goto_type_definition",
    "goto_implementation",
    "goto_reference",
    "goto_file",
    "goto_file_hsplit",
    "goto_file_vsplit",
    "goto_last_accessed_file",
    "goto_last_modified_file",
    "goto_next_buffer",
    "goto_previous_buffer",
    "goto_next_tabstop",
    "goto_prev_tabstop",
];

/// What a static command is, by name: debug mode sorts commands the same way.
pub(crate) fn effect_of(name: &str) -> Effect {
    effect(name)
}

/// What a static command does to watch mode, by name.
fn effect(name: &str) -> Effect {
    if NONE.contains(&name) {
        Effect::None
    } else if SEARCH.contains(&name) {
        Effect::Search
    } else if MOVE.contains(&name)
        || name.starts_with("move_") && !matches!(name, "move_lines_up" | "move_lines_down")
        || name.starts_with("extend_")
        || name.starts_with("find_")
        || name.starts_with("till_")
        || name.starts_with("rotate_selections_")
        || name.starts_with("goto_") && !OPENS.contains(&name)
    {
        Effect::Move
    } else {
        Effect::End
    }
}

/// The note on the bar for a key that waits for watch mode to end.
pub const LEAVE_TO_EDIT: &str = "Esc leaves watch mode, then edit";
/// The note on the bar for a movement while following.
pub const LOOK_TO_MOVE: &str = "following: v looks around to move";

/// Decides whether a command runs in watch mode, and what it does to it. `false` when it waits
/// for watch mode to end (the bar says so).
fn take_effect(editor: &mut Editor, effect: Effect) -> bool {
    let Some(session) = editor.watch.session.as_mut() else {
        return true;
    };
    if !session.active {
        if effect == Effect::Search {
            session.search_owns_n = true;
        }
        return true;
    }
    match effect {
        Effect::None => true,
        Effect::Move | Effect::Search if !session.following => true,
        Effect::Move | Effect::Search => {
            session.activity = Some(LOOK_TO_MOVE.into());
            false
        }
        Effect::End => {
            session.activity = Some(LEAVE_TO_EDIT.into());
            false
        }
    }
}

/// Called before a static command runs: `false` when it must not run (watch mode holds it).
pub(crate) fn before_command(editor: &mut Editor, name: &str) -> bool {
    if editor.watch.session.is_none() {
        return true;
    }
    take_effect(editor, effect(name))
}

/// Called before a typed command (`:...`) runs. A typed command is meant: `:goto` looks around,
/// the watch commands keep watch mode, any other leaves it.
pub(crate) fn before_typed(editor: &mut Editor, name: &str) {
    let Some(session) = editor.watch.active() else {
        return;
    };
    match name {
        "watch" | "watch-stop" | "watch-clear" | "watch-changes" => {}
        "goto" => session.pause(),
        _ => leave_mode(editor),
    }
}

/// Whether watch mode holds a mouse event over the views, which then does nothing. While
/// following, the mouse moves nothing (clicks, drags, the wheel), as the movement keys: the bar
/// says that `v` looks around. A middle-click paste edits: it waits for watch mode to end, in both
/// states. While looking around, the rest works as usual.
pub(crate) fn holds_mouse(editor: &mut Editor, kind: MouseEventKind) -> bool {
    let middle_paste = editor.config().middle_click_paste;
    let Some(session) = editor.watch.active() else {
        return false;
    };
    let note = match kind {
        MouseEventKind::Moved => return false,
        MouseEventKind::Up(MouseButton::Middle) if middle_paste => LEAVE_TO_EDIT,
        _ if !session.following => return false,
        MouseEventKind::Down(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => LOOK_TO_MOVE,
        // the button's release after a held press (the left one would yank a selection)
        MouseEventKind::Up(_) => return true,
    };
    session.activity = Some(note.into());
    true
}

/// Whether the statusline's mark is lit, `elapsed` after a change arrived: lit for a second
/// every four, ten times; `None` once it is steady.
pub(crate) fn blink_lit(elapsed: Duration) -> Option<bool> {
    if elapsed >= BLINK_PERIOD * BLINKS {
        return None;
    }
    let phase = elapsed.as_millis() % BLINK_PERIOD.as_millis();
    Some(phase < BLINK_LIT.as_millis())
}

/// Redraws at each turn of the blinking that starts at `start`, until it is steady: an earlier
/// blinking stops. Nothing runs once it is steady, so the editor idles.
fn blink(start: Instant) {
    let blinking = BLINKING.fetch_add(1, Ordering::Relaxed) + 1;
    tokio::spawn(async move {
        for turn in 0..BLINKS {
            let period = BLINK_PERIOD * turn;
            for at in [period + BLINK_LIT, period + BLINK_PERIOD] {
                tokio::time::sleep_until((start + at).into()).await;
                if BLINKING.load(Ordering::Relaxed) != blinking {
                    return;
                }
                vortex_event::request_redraw();
            }
        }
    });
}

/// The statusline's `watch` element (in the default statusline): a session left with `Esc` that
/// is still watching, as `●` and the count of the changes made since (`● 3`), lit for a second
/// every four when a change arrives, ten times, then steady. Nothing without a session, once it
/// is stopped, and while watch mode is on (the mode bar shows it).
pub(crate) fn statusline(editor: &Editor) -> Option<Span<'static>> {
    let session = editor
        .watch
        .session
        .as_ref()
        .filter(|session| !session.active && session.is_live())?;
    let theme = &editor.theme;
    let lit = session
        .recorded
        .and_then(|at| blink_lit(at.elapsed()))
        .unwrap_or(false);
    let style = if lit {
        theme
            .try_get_exact("ui.statusline.watch.lit")
            .unwrap_or_else(|| Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else {
        theme
            .try_get_exact("ui.statusline.watch")
            .unwrap_or_default()
    };
    let text = match session.new_since_left() {
        0 => " ● ".to_string(),
        count => format!(" ● {count} "),
    };
    Some(Span::styled(text, style))
}

/// The rows watch mode takes on the mode bar.
pub const BAR_ROWS: u16 = 2;

/// Watch mode's mode bar: its state, the current change and the latest activity, then its keys.
pub(crate) fn bar(editor: &Editor, keys: &ReverseKeymap) -> Option<modebar::Bar> {
    use modebar::{Row, Segment};
    let session = editor.watch.session.as_ref().filter(|s| s.active)?;
    let state = if let Some((done, total)) = session.catch_up_progress() {
        Segment::state(format!("WATCH ● catching up {done}/{total}"))
            .scope("ui.modebar.state.following")
    } else if session.following {
        Segment::state("WATCH ● following").scope("ui.modebar.state.following")
    } else {
        Segment::state("WATCH ◌ looking").scope("ui.modebar.state.looking")
    };
    let (at, hunks) = session.position();
    let files = session.file_count();
    let mut middle = Vec::new();
    if let Some((path, _)) = session.at.as_ref() {
        middle.push(relative(&session.baseline, path));
    }
    middle.push(match (at, hunks) {
        (_, 0) => "no changes yet".to_string(),
        (Some(at), hunks) => format!("{at}/{hunks} change{}", plural(hunks)),
        (None, hunks) => format!("{hunks} change{}", plural(hunks)),
    });
    if files > 0 {
        middle.push(format!("{files} file{}", plural(files)));
    }
    // the changes not seen yet: while looking around, or made while watch mode was left
    let new = session.new_count();
    if new > 0 {
        middle.push(format!("{new} new"));
    }
    let activity = session.activity.clone().unwrap_or_default();
    let look = if session.following { "look" } else { "follow" };
    Some(modebar::Bar {
        rows: vec![
            Row {
                left: vec![state],
                middle: vec![Segment::info(middle.join(" · "))],
                right: vec![Segment::info(activity)],
                line: None,
            },
            Row {
                left: vec![
                    Segment::key("watch_look", look, keys),
                    Segment::key("watch_next_change", "next", keys),
                    Segment::key("watch_prev_change", "previous", keys),
                    Segment::key("watch_accept", "accept", keys),
                    Segment::key("watch_reject", "reject", keys),
                    Segment::key("watch_yank", "yank", keys),
                    Segment::key("watch_changes", "changes", keys),
                    Segment::key("watch_leave", "leave", keys),
                ],
                ..Row::default()
            },
        ],
    })
}

/// The lines of `doc` flashing and their style, fading out; `None` when it isn't flashing.
pub(crate) fn flash_style(
    editor: &Editor,
    doc: vortex_view::DocumentId,
) -> Option<(std::ops::Range<usize>, Style)> {
    let flash = editor.watch.session.as_ref()?.flash.as_ref()?;
    if flash.doc != doc {
        return None;
    }
    let elapsed = flash.start.elapsed();
    if elapsed >= FLASH {
        return None;
    }
    let theme: &Theme = &editor.theme;
    let color = theme.get_or("ui.highlight.watch", "ui.selection").bg;
    // toward the background drawn under it: a mode's screen while one is on (watch mode's when
    // following), else the buffer's, so that it ends on it without a flicker
    let background = match crate::modes::current(editor) {
        Some(_) => crate::modes::screen_style(theme).bg,
        None => theme.get("ui.background").bg,
    };
    let left = 1.0 - elapsed.as_secs_f32() / FLASH.as_secs_f32();
    let bg = match crate::ui::diff_view::blend(background, color, left) {
        Some(color) => Some(color),
        // without RGB colors to blend, the flash shows whole, then not at all
        None => color.filter(|_| left > 0.4),
    }?;
    Some((flash.lines.clone(), Style::default().bg(bg)))
}

/// Sets the baseline of the documents opened while their file is a change of the session, for
/// their gutter markers.
pub(crate) fn register_hooks() {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let editor = &mut *event.editor;
        let Some(session) = editor.watch.session.as_ref() else {
            return Ok(());
        };
        let baseline = session.baseline.clone();
        let Some(doc) = editor.documents.get_mut(&event.doc) else {
            return Ok(());
        };
        let Some(path) = doc.path() else {
            return Ok(());
        };
        if session.file(path).is_some() && doc.watch_diff().is_none() {
            let base = baseline.get(path);
            doc.set_watch_base(base.as_deref());
        }
        Ok(())
    });
}

/// The rows of the session's changes: the most recently changed file first, and in a file its
/// newest hunk first. Accepted changes are marked; rejected ones, no longer changes, have a row
/// of their own (where they were, with the lines they had) under their file.
fn session_rows(editor: &Editor, session: &Session) -> Vec<ChangeRow> {
    let config = &editor.config().changes;
    let mut rows = Vec::new();
    let mut files = session.files_newest_first();
    // a file whose changes were all rejected is back to its baseline: listed for them
    for change in session.rejected() {
        if session.file(&change.path).is_none() && !files.iter().any(|(p, _)| *p == &change.path) {
            files.push((&change.path, &EMPTY_FILE));
        }
    }
    for (path, file) in files {
        let rejected: Vec<_> = session
            .rejected()
            .iter()
            .filter(|change| &change.path == path)
            .collect();
        let mut group = if session.file(path).is_some() {
            let before = session.baseline.get(path);
            let now = match file.kind {
                FileKind::Deleted => None,
                _ => std::fs::read(path).ok(),
            };
            let change = match file.kind {
                FileKind::Created => FileChange::Added { path: path.clone() },
                FileKind::Deleted => FileChange::Deleted { path: path.clone() },
                _ => FileChange::Modified { path: path.clone() },
            };
            changes::commit_group(&change, before.as_deref(), now.as_deref(), config)
        } else {
            let change = FileChange::Modified { path: path.clone() };
            changes::file_group(&change, None, changes::Body::Hunks, Vec::new())
        };
        let seq_of = |row: &ChangeRow| match &row.what {
            What::Hunk(hunk) => file
                .hunks
                .iter()
                .find(|session_hunk| session_hunk.after.start == hunk.lines.start)
                .map_or(0, |session_hunk| session_hunk.seq),
            What::File(_) => u64::MAX,
        };
        let mut keyed: Vec<(u64, ChangeRow)> = group
            .drain(..)
            .map(|mut row| {
                let seq = seq_of(&row);
                let accepted = file
                    .hunks
                    .iter()
                    .any(|hunk| hunk.seq == seq && hunk.review == Some(Review::Accepted));
                if accepted {
                    row.marks = Some(changes::Marks {
                        review: Some(Review::Accepted),
                        ..Default::default()
                    });
                }
                (seq, row)
            })
            .collect();
        for change in rejected {
            keyed.push((change.seq, rejected_row(path, change)));
        }
        // the divider stays first
        keyed.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        rows.extend(keyed.into_iter().map(|(_, row)| row));
    }
    rows
}

/// A file without hunks, for the files whose changes were all rejected.
static EMPTY_FILE: vortex_view::watch::SessionFile = vortex_view::watch::SessionFile::EMPTY;

/// The row of a rejected change: where it was, with the lines it had.
fn rejected_row(path: &Path, change: &Rejected) -> ChangeRow {
    let removed: Vec<&str> = change
        .removed
        .iter()
        .map(|line| line.trim_end_matches(['\n', '\r']))
        .collect();
    let added: Vec<&str> = change
        .added
        .iter()
        .map(|line| line.trim_end_matches(['\n', '\r']))
        .collect();
    let line_hunk = vortex_vcs::staging::LineHunk {
        before: 0..removed.len() as u32,
        after: 0..added.len() as u32,
    };
    let edit = match (removed.is_empty(), added.is_empty()) {
        (true, _) => changes::Edit::Add,
        (_, true) => changes::Edit::Remove,
        _ => changes::Edit::Change,
    };
    let hunk = changes::Hunk {
        lines: change.line..change.line,
        added: added.len() as u32,
        removed: removed.len() as u32,
        edit,
        preview: Arc::new(changes::preview(&removed, &added, &line_hunk)),
    };
    ChangeRow {
        change: FileChange::Modified {
            path: path.to_owned(),
        },
        marks: Some(changes::Marks {
            review: Some(Review::Rejected),
            ..Default::default()
        }),
        what: What::Hunk(hunk),
    }
}

/// `:watch-changes`: the picker of the session's changes, one row per hunk under a divider per
/// file, the newest first. Enter goes to the change.
pub fn open_changes(editor: &mut Editor, compositor: &mut Compositor) {
    let Some(session) = editor.watch.session.as_mut() else {
        editor.set_error("no watch session");
        return;
    };
    if session.active {
        session.pause();
    }
    let session = editor.watch.session.as_ref().unwrap();
    if session.is_empty() {
        editor.set_status("No changes this session");
        return;
    }
    let rows = session_rows(editor, session);
    // the totals on the picker's prompt row, not in the status line
    let totals = changes::Totals::of(&rows);
    let cwd = vortex_stdx::env::current_working_dir();
    let enter = |cx: &mut Context, row: &ChangeRow| {
        if !open_row(cx.editor, row, Action::Replace) {
            return;
        }
        cx.jobs.callback(async move {
            let call = |_: &mut Editor, compositor: &mut Compositor| {
                compositor.remove(picker::ID);
            };
            Ok(Callback::EditorCompositor(Box::new(call)))
        });
    };
    let open = |cx: &mut Context, row: &ChangeRow, action: Action| {
        open_row(cx.editor, row, action);
    };
    let picker = changes::picker(editor, cwd, rows, false, enter, open)
        .with_info(format!("this session: {}", totals.describe()));
    compositor.push(Box::new(overlaid(picker)));
}

/// Goes to the change of `row`. `false` when it can't (a deleted file).
fn open_row(editor: &mut Editor, row: &ChangeRow, action: Action) -> bool {
    let path = row.change.path().to_path_buf();
    if matches!(row.change, FileChange::Deleted { .. }) {
        editor.set_error(format!("{} was deleted", display(&path)));
        return false;
    }
    let lines = match &row.what {
        What::Hunk(hunk) => hunk.lines.clone(),
        What::File(_) => row.line()..row.line() + 1,
    };
    let seq = editor.watch.session.as_ref().and_then(|session| {
        let file = session.file(&path)?;
        file.hunks
            .iter()
            .find(|hunk| hunk.after.start == lines.start)
            .map(|hunk| hunk.seq)
    });
    let target = Target {
        path,
        seq: seq.unwrap_or(0),
        lines,
    };
    go_to(editor, &target, action);
    if seq.is_some() {
        if let Some(session) = editor.watch.session.as_mut() {
            session.went_to(&target);
        }
    }
    true
}

/// The current change: the hunks of the change followed or gone to last (`n`/`N`, the picker),
/// in the order of its file.
fn current_change(editor: &Editor) -> anyhow::Result<(Target, Vec<SessionHunk>)> {
    let session = editor.watch.session.as_ref().context("no watch session")?;
    let target = session.current().map_err(|gone| match gone {
        Gone::NoChange => anyhow::anyhow!("no change yet: n goes to one"),
        Gone::Rewritten => anyhow::anyhow!("that change is gone: its lines were changed again"),
        Gone::Rejected => anyhow::anyhow!("that change was rejected"),
    })?;
    let hunks = session.change_of(&target);
    Ok((target, hunks))
}

/// `src/main.rs:12-14`, one per hunk, as `Space :` and `C-y` write them.
fn places(path: &Path, hunks: &[SessionHunk]) -> Vec<String> {
    let path = crate::yank_place::shown_path(path, false);
    hunks
        .iter()
        .map(|hunk| changes::place(&path, &hunk.after))
        .collect()
}

/// `y` in watch mode: yanks the current change's `file:lines` to the clipboard, one place per
/// hunk (a removed hunk is the line before it), as `Space :` does.
pub(crate) fn yank(editor: &mut Editor) {
    match current_change(editor) {
        Ok((target, hunks)) => crate::yank_place::yank(editor, places(&target.path, &hunks)),
        Err(err) => editor.set_error(err.to_string()),
    }
}

/// The text of the file the current change is in, checked: no unsaved edits of your own, and its
/// change still there with the same lines (the watcher may not have taken in a write yet).
fn checked_text(
    editor: &mut Editor,
    target: &Target,
    hunks: &[SessionHunk],
) -> anyhow::Result<String> {
    let path = &target.path;
    let shown = crate::yank_place::shown_path(path, false);
    crate::handlers::disk::check_paths(editor, std::slice::from_ref(path));
    if let Some(doc) = editor.document_by_path(path) {
        anyhow::ensure!(
            !doc.is_modified(),
            "{shown} has unsaved changes: write it first"
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("can't read {shown}"))?;
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{shown} is binary"))?;
    let session = editor.watch.session.as_ref().context("no watch session")?;
    let config = editor.config().changes.clone();
    let now = read_updates(&session.baseline, &config, std::slice::from_ref(path));
    let there = |hunk: &SessionHunk| {
        now.iter().any(|update| {
            update
                .hunks
                .iter()
                .any(|now| now.key == hunk.key() && now.after == hunk.after)
        })
    };
    anyhow::ensure!(
        !hunks.is_empty() && hunks.iter().all(there),
        "{shown} changed since: try again"
    );
    Ok(text)
}

fn line_hunk(hunk: &SessionHunk) -> vortex_vcs::staging::LineHunk {
    vortex_vcs::staging::LineHunk {
        before: hunk.before.clone(),
        after: hunk.after.clone(),
    }
}

/// `a` in watch mode: accepts the current change: stages its hunks, as staging a hunk in the
/// diff view does (the index gets those lines of the file). The change is marked accepted.
pub(crate) fn accept(editor: &mut Editor) {
    let result = current_change(editor).and_then(|(target, hunks)| {
        let work = checked_text(editor, &target, &hunks)?;
        let path = &target.path;
        let trust_full = trust_full(editor, path.parent().unwrap_or(path));
        let index = editor
            .diff_providers
            .index_file(path, trust_full)?
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("the index has a binary file"))
            })
            .transpose()?;
        let index_text = index.as_deref().unwrap_or("");
        let staged = hunks.iter().fold(index_text.to_string(), |index, hunk| {
            vortex_vcs::staging::stage_hunk(&index, &work, &line_hunk(hunk))
        });
        if index.is_none() || staged != index_text {
            editor
                .diff_providers
                .set_index_file(path, Some(staged.as_bytes()), trust_full)?;
        }
        if let Some(session) = editor.watch.session.as_mut() {
            for hunk in &hunks {
                session.accept(path, hunk.seq);
            }
        }
        Ok(format!("staged {}", places(path, &hunks).join(", ")))
    });
    match result {
        Ok(message) => editor.set_status(message),
        Err(err) => editor.set_error(err.to_string()),
    }
}

/// What rejecting the current change does once it was checked.
pub(crate) enum Rejection {
    /// Done, with the message for the status line.
    Done(String),
    /// The change is the whole of a file the session created: rejecting it deletes the file,
    /// after a confirmation ([`delete_created`]).
    ConfirmDelete(PathBuf),
}

/// `x` in watch mode: rejects the current change: its lines go back to what they were when the
/// watch started (the baseline), as one edit of the file's buffer, then a write (`u` in the
/// buffer undoes it), with the rules of reverting a hunk: not with unsaved edits of your own, not
/// once staged (accepted), and a file the session created asks before it is deleted. The change
/// is marked rejected.
pub(crate) fn reject(editor: &mut Editor) -> anyhow::Result<Rejection> {
    let (target, hunks) = current_change(editor)?;
    let work = checked_text(editor, &target, &hunks)?;
    let path = target.path.clone();
    let shown = crate::yank_place::shown_path(&path, false);
    let baseline = editor
        .watch
        .session
        .as_ref()
        .context("no watch session")?
        .baseline
        .clone();
    let base = baseline.get(&path);
    let base_text = match base.as_deref() {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("{shown} was binary"))?
            .to_string(),
        None => String::new(),
    };
    // staged: the index has those lines
    let trust_full = trust_full(editor, path.parent().unwrap_or(&path));
    let index = editor.diff_providers.index_file(&path, trust_full)?;
    if let Some(index) = index
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    {
        let staged = hunks
            .iter()
            .all(|hunk| vortex_vcs::staging::revert_hunk(index, &work, &line_hunk(hunk)) == work);
        anyhow::ensure!(
            !staged,
            "{} is staged: unstage it first",
            places(&path, &hunks).join(", ")
        );
    }
    // from the last hunk up, so that the lines of those above stay where they are
    let reverted = hunks.iter().rev().fold(work.clone(), |text, hunk| {
        vortex_vcs::staging::revert_hunk(&base_text, &text, &line_hunk(hunk))
    });
    if base.is_none() && reverted.is_empty() {
        return Ok(Rejection::ConfirmDelete(path));
    }
    anyhow::ensure!(reverted != work, "{shown} has nothing to reject there");
    let places = places(&path, &hunks);
    let doc = match editor.document_by_path(&path) {
        Some(doc) => doc.id(),
        None => editor.open(&path, Action::Load)?,
    };
    replace_text(editor, doc, &reverted);
    let message = format!("rejected {}", places.join(", "));
    // said once it is written, instead of `'file' written`
    editor.save_messages.insert(doc, message.clone());
    if let Err(err) = editor.save::<PathBuf>(doc, None, false) {
        editor.save_messages.remove(&doc);
        return Err(err);
    }
    mark_rejected(editor, &target, &hunks, &base_text, &work);
    Ok(Rejection::Done(message))
}

/// Records the change of `hunks` (of the text `work`, against `base`) as rejected.
fn mark_rejected(
    editor: &mut Editor,
    target: &Target,
    hunks: &[SessionHunk],
    base: &str,
    work: &str,
) {
    let (base_lines, work_lines) = (changes::lines(base), changes::lines(work));
    let slice = |lines: &[&str], range: &std::ops::Range<u32>| -> Vec<String> {
        let end = (range.end as usize).min(lines.len());
        let start = (range.start as usize).min(end);
        lines[start..end]
            .iter()
            .map(|line| line.to_string())
            .collect()
    };
    let Some(session) = editor.watch.session.as_mut() else {
        return;
    };
    // a hunk above moves the lines of those below by the lines it gave back
    let mut shift: i64 = 0;
    for hunk in hunks {
        let line = (hunk.after.start as i64 + shift).max(0) as u32;
        shift += hunk.before.len() as i64 - hunk.after.len() as i64;
        session.reject(Rejected {
            path: target.path.clone(),
            seq: hunk.seq,
            line,
            removed: slice(&base_lines, &hunk.before),
            added: slice(&work_lines, &hunk.after),
        });
    }
}

/// Deletes the file the session created whose whole content was the change rejected, after the
/// confirmation: like reverting an untracked file.
pub(crate) fn delete_created(editor: &mut Editor, path: &Path) -> anyhow::Result<String> {
    let (target, hunks) = current_change(editor)?;
    anyhow::ensure!(target.path == path, "the change moved: try again");
    let work = checked_text(editor, &target, &hunks)?;
    crate::commands::hunks::delete_untracked(editor, path)?;
    mark_rejected(editor, &target, &hunks, "", &work);
    Ok(format!(
        "rejected {}: deleted it",
        crate::yank_place::shown_path(path, false)
    ))
}

/// Changes the text of `doc` to `text`, as one undoable step.
fn replace_text(editor: &mut Editor, doc: vortex_view::DocumentId, text: &str) {
    let view = editor.get_synced_view_id(doc);
    let document = editor
        .documents
        .get_mut(&doc)
        .expect("the document is open");
    let transaction =
        vortex_core::diff::compare_ropes(document.text(), &vortex_core::Rope::from(text));
    let view = editor.tree.get_mut(view);
    document.apply(&transaction, view.id);
    document.append_changes_to_history(view);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effects() {
        assert_eq!(effect("move_char_left"), Effect::Move);
        assert_eq!(effect("goto_line_start"), Effect::Move);
        assert_eq!(effect("goto_file"), Effect::End);
        assert_eq!(effect("goto_next_buffer"), Effect::End);
        assert_eq!(effect("move_lines_up"), Effect::End);
        assert_eq!(effect("extend_line_below"), Effect::Move);
        assert_eq!(effect("search"), Effect::Search);
        assert_eq!(effect("search_next"), Effect::None);
        assert_eq!(effect("insert_mode"), Effect::End);
        assert_eq!(effect("file_picker"), Effect::End);
        assert_eq!(effect("delete_selection"), Effect::End);
        assert_eq!(effect("toggle_terminal"), Effect::None);
        // history and bookmarks
        assert_eq!(effect("toggle_bookmark"), Effect::None);
        for name in [
            "history_back",
            "history_forward",
            "history_picker",
            "next_bookmark",
            "prev_bookmark",
            "bookmark_picker",
            "goto_last_accessed_file",
        ] {
            assert_eq!(effect(name), Effect::End, "{name}");
        }
    }

    #[test]
    fn the_mark_blinks_a_second_every_four_ten_times_then_is_steady() {
        let at = |millis: u64| blink_lit(Duration::from_millis(millis));
        assert_eq!(at(0), Some(true));
        assert_eq!(at(999), Some(true));
        assert_eq!(at(1000), Some(false));
        assert_eq!(at(3999), Some(false));
        assert_eq!(at(4000), Some(true));
        assert_eq!(at(36_500), Some(true));
        assert_eq!(at(39_999), Some(false));
        assert_eq!(at(40_000), None);
        assert_eq!(at(400_000), None);
    }

    #[test]
    fn the_catch_up_pace_is_a_number_of_seconds() {
        let pace = |toml: &str| {
            toml::from_str::<vortex_view::editor::Config>(toml)
                .map(|config| config.watch_catch_up.millis)
        };
        assert_eq!(pace("").unwrap(), 1000);
        assert_eq!(pace("watch-catch-up = 0").unwrap(), 0);
        assert_eq!(pace("watch-catch-up = 2").unwrap(), 2000);
        assert_eq!(pace("watch-catch-up = 0.25").unwrap(), 250);
        assert!(pace("watch-catch-up = -1").is_err());
    }

    /// A renamed or removed command left in a list would silently fall to "ends watch mode".
    #[test]
    fn every_listed_command_is_a_static_command() {
        let names: Vec<&str> = crate::commands::MappableCommand::STATIC_COMMAND_LIST
            .iter()
            .map(|command| command.name())
            .collect();
        for (list, commands) in [
            ("NONE", NONE),
            ("SEARCH", SEARCH),
            ("MOVE", MOVE),
            ("OPENS", OPENS),
        ] {
            for command in commands {
                assert!(
                    names.contains(command),
                    "{command} in {list} is not a static command"
                );
            }
        }
    }
}
