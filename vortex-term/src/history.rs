//! History and bookmarks in the terminal UI (this fork): `ga`, the forward step, the history
//! picker (`gA`), showing a screen again, and the bookmark commands and picker. What a jump records and how history
//! walks are in `vortex_view::history`; bookmarks are in `vortex_view::bookmarks`.

use std::path::PathBuf;

use vortex_core::Selection;
use vortex_view::{
    align_view,
    bookmarks::{At, Bookmark, Direction},
    history::{Jump, Place, Step},
    keyboard::{KeyCode, KeyModifiers},
    Align, DocumentId, Editor,
};

use crate::{
    commands::Context,
    compositor::Compositor,
    ctrl, job,
    ui::{self, overlay::overlaid, Picker, PickerColumn},
};

/// `ga`: one place back in the focused view's history (a count goes further back).
pub fn back(cx: &mut Context) {
    let count = cx.count();
    cx.callback.push(Box::new(move |compositor, cx| {
        step(compositor, cx.editor, Step::Back, count, None);
    }));
}

/// `history_forward` (no default key, in the command palette): one place forward, after `ga`;
/// the last one is where the walk started.
pub fn forward(cx: &mut Context) {
    let count = cx.count();
    cx.callback.push(Box::new(move |compositor, cx| {
        step(compositor, cx.editor, Step::Forward, count, None);
    }));
}

/// Takes a step through the focused window's history and shows where it lands. From a screen
/// that covers the window (a diff, a page, the hex view), going back shows what the window held,
/// the screen kept in its history (`ga` again shows it again); while walking, the walk goes on
/// from there. A screen's window of its own (a terminal, the debug panel) has no history.
/// Returns whether it moved; it says why not.
pub fn step(
    compositor: &mut Compositor,
    editor: &mut Editor,
    step: Step,
    count: usize,
    _here: Option<Place>,
) -> bool {
    // a step may open another buffer or screen: it waits for watch mode to end, also from the keys that
    // don't go through the keymap (the hex view, a diff)
    let name = match step {
        Step::Back => "history_back",
        Step::Forward => "history_forward",
    };
    if !crate::watch::before_command(editor, name) {
        // watch mode holds it: the bar says so
        return false;
    }
    let _ = compositor;
    let focus = editor.tree.focus;
    if editor.is_screen_view(focus) {
        if editor.tree.under(focus).is_none() {
            editor.set_status("A screen's window has no history");
            return false;
        }
        let walking = editor
            .tree
            .get(editor.tree.text_focus())
            .history
            .is_walking();
        crate::screens::close(editor, focus);
        if !walking {
            // back to what the window showed: that was the step
            return step == Step::Back;
        }
    }
    let here = editor.place();
    match editor.history_step(step, count, here) {
        Ok(place) => {
            show(editor, place);
            true
        }
        Err(message) => {
            editor.set_status(message);
            false
        }
    }
}

/// Shows `place` in the focused window, without recording a jump.
fn show(editor: &mut Editor, place: Place) {
    match place {
        Place::Text { doc, selection } => editor.show_place(doc, selection),
        Place::Screen(screen) => {
            if let Some(screen) = screen.take() {
                editor.without_history(|editor| crate::screens::reopen(editor, screen));
            }
        }
    }
}

// The history picker

/// A row of the history picker: a place of the focused view's history, or the present.
#[derive(Debug, Clone)]
pub struct HistoryRow {
    /// Its index in the history; `None` for the present, listed first when not walking.
    pub index: Option<usize>,
    /// Where the walk is: the present, or the place a walk is at.
    pub current: bool,
    /// The file (relative to the working directory) or the buffer's name, and the line; or the
    /// screen.
    pub name: String,
    /// The line's text, trimmed; empty for a screen.
    pub text: String,
    /// What the preview shows: the buffer and the line.
    preview: Option<(DocumentId, usize)>,
}

/// A place as the picker shows it.
fn describe(editor: &Editor, place: &Place) -> (String, String, Option<(DocumentId, usize)>) {
    match place {
        Place::Text { doc, selection } => {
            let Some(document) = editor.document(*doc) else {
                return ("(closed)".to_string(), String::new(), None);
            };
            let text = document.text().slice(..);
            let cursor = selection.primary().cursor(text).min(text.len_chars());
            let line = text.char_to_line(cursor);
            let name = match document.path() {
                Some(path) => vortex_stdx::path::get_relative_path(path)
                    .display()
                    .to_string(),
                None => document.display_name().into_owned(),
            };
            let content = text.line(line).to_string();
            (
                format!("{name}:{}", line + 1),
                content.trim().to_string(),
                Some((*doc, line)),
            )
        }
        Place::Screen(screen) => (screen.name.clone(), String::new(), None),
    }
}

/// The rows of the history picker for the focused view, newest first: the present first when
/// not walking, then the places it jumped from; the row where the walk is, is `current`.
pub fn history_rows(editor: &Editor, here: &Place) -> Vec<HistoryRow> {
    let history = &editor.tree.get(editor.tree.text_focus()).history;
    let mut rows = Vec::with_capacity(history.len() + 1);
    if !history.is_walking() {
        let (name, text, preview) = describe(editor, here);
        rows.push(HistoryRow {
            index: None,
            current: true,
            name,
            text,
            preview,
        });
    }
    let places: Vec<&Place> = history.places().collect();
    for (index, place) in places.into_iter().enumerate().rev() {
        let (name, text, preview) = describe(editor, place);
        rows.push(HistoryRow {
            index: Some(index),
            current: history.current() == index,
            name,
            text,
            preview,
        });
    }
    rows
}

/// `gA`: the history picker, the focused view's places newest first, where the walk is marked.
pub fn history_picker(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        open_history_picker(compositor, cx.editor, None)
    }));
}

/// Opens the history picker; `here` is the place of the screen on top it is opened from (the
/// diff view, shown as the present), `None` for the editor's place.
pub fn open_history_picker(compositor: &mut Compositor, editor: &mut Editor, here: Option<Place>) {
    // it may open another buffer or screen: it waits for watch mode to end, as `ga` does
    if !crate::watch::before_command(editor, "history_picker") {
        return;
    }
    let present = here.clone().unwrap_or_else(|| editor.place());
    let rows = history_rows(editor, &present);
    if rows.iter().all(|row| row.index.is_none()) {
        editor.set_status("No jumps yet");
        return;
    }
    let cursor = rows.iter().position(|row| row.current).unwrap_or(0) as u32;
    let columns = [
        PickerColumn::new("", |row: &HistoryRow, _| {
            if row.current { "●" } else { " " }.into()
        }),
        PickerColumn::new("place", |row: &HistoryRow, _| row.name.as_str().into()),
        PickerColumn::new("line", |row: &HistoryRow, _| row.text.as_str().into()),
    ];
    let picker = Picker::new(columns, 1, rows, (), |cx, row: &HistoryRow, _action| {
        let Some(index) = row.index else {
            // the present: already there
            return;
        };
        cx.jobs.callback(async move {
            let call: job::Callback = job::Callback::EditorCompositor(Box::new(
                move |editor: &mut Editor, compositor: &mut Compositor| {
                    go_to_place(compositor, editor, index);
                },
            ));
            Ok(call)
        });
    })
    .with_modes()
    .without_header()
    // going there is history's own step, not a new jump
    .without_jumps()
    .with_help(
        "History",
        vec![(
            "history",
            vec![
                ("Enter", "go there"),
                ("C-y", "yank file:line"),
                ("ga", "(outside) one back"),
            ],
        )],
    )
    .with_initial_cursor(cursor)
    .with_preview(|_editor, row: &HistoryRow| {
        row.preview
            .map(|(doc, line)| (doc.into(), Some((line, line))))
    })
    .with_stacked_key_handler(ctrl!('y'), |cx, row: &HistoryRow| {
        match row
            .preview
            .and_then(|(doc, line)| crate::yank_place::doc_place(cx.editor, doc, line))
        {
            Some(place) => crate::yank_place::yank(cx.editor, vec![place]),
            None => cx.editor.set_error("No file at this place"),
        }
    });
    compositor.push(Box::new(overlaid(picker)));
}

/// Goes to the place at `index` of the focused window's history, moving the walk's position
/// there, without recording a jump. From a screen that covers the window, the screen closes
/// first (kept in the history, as `ga` keeps it).
fn go_to_place(_compositor: &mut Compositor, editor: &mut Editor, index: usize) {
    let focus = editor.tree.focus;
    if editor.is_screen_view(focus) && editor.tree.under(focus).is_some() {
        crate::screens::close(editor, focus);
    }
    let here = editor.place();
    match editor.history_go_to(index, here) {
        Ok(place) => show(editor, place),
        Err(message) => editor.set_status(message),
    }
}

// Bookmarks

/// `C-s`: sets a bookmark on the line of each cursor, or removes the one there.
pub fn toggle_bookmark(cx: &mut Context) {
    let (view, doc) = current_ref!(cx.editor);
    if doc.is_hex() {
        cx.editor
            .set_error("Bookmarks are lines of text: the hex view has none");
        return;
    }
    let text = doc.text().slice(..);
    let mut lines: Vec<usize> = doc
        .selection(view.id)
        .iter()
        .map(|range| range.cursor_line(text))
        .collect();
    lines.dedup();
    let doc_id = doc.id();
    let (set, removed) = cx.editor.toggle_bookmarks(doc_id, &lines);
    let total = cx.editor.bookmarks().len();
    let message = match (set, removed) {
        (1, 0) => format!("Bookmark {} set", number_of_last(cx.editor)),
        (0, 1) => "Bookmark removed".to_string(),
        (set, 0) => format!("{set} bookmarks set"),
        (0, removed) => format!("{removed} bookmarks removed"),
        (set, removed) => format!("{set} bookmarks set, {removed} removed"),
    };
    cx.editor.set_status(format!("{message} ({total} in all)"));
}

/// The number of the bookmark set last.
fn number_of_last(editor: &Editor) -> usize {
    let current = editor.bookmark_state.current;
    editor
        .bookmarks()
        .iter()
        .position(|bookmark| Some(bookmark.seq) == current)
        .map_or(0, |index| index + 1)
}

/// `C-i`: the next bookmark, in the order they were set.
pub fn next_bookmark(cx: &mut Context) {
    go_round(cx.editor, Direction::Next);
}

/// `C-o`: the previous bookmark.
pub fn prev_bookmark(cx: &mut Context) {
    go_round(cx.editor, Direction::Previous);
}

fn go_round(editor: &mut Editor, direction: Direction) {
    let Some(bookmark) = editor.bookmark_step(direction) else {
        editor.set_status("No bookmarks");
        return;
    };
    let total = editor.bookmarks().len();
    let number = editor
        .bookmarks()
        .iter()
        .position(|other| other.seq == bookmark.seq)
        .map_or(0, |index| index + 1);
    if go_to(editor, &bookmark) {
        editor.set_status(format!("Bookmark {number} of {total}"));
    }
}

/// Goes to a bookmark, a jump; returns whether it got there.
pub fn go_to(editor: &mut Editor, bookmark: &Bookmark) -> bool {
    editor.bookmark_state.current = Some(bookmark.seq);
    let from = editor.place();
    let opened = editor.without_history(|editor| match &bookmark.at {
        At::Buffer { doc, .. } => Ok(*doc),
        At::File { path, .. } => editor
            .open(path, vortex_view::editor::Action::Replace)
            .map_err(|err| format!("{}: {err}", path.display())),
    });
    let doc = match opened {
        Ok(doc) => doc,
        Err(err) => {
            editor.set_error(err);
            return false;
        }
    };
    // the line again: a file opened just now has its bookmarks back
    let line = editor
        .document(doc)
        .and_then(|document| {
            let text = document.text().slice(..);
            document
                .bookmarks
                .lines(text)
                .find(|&(seq, _)| seq == bookmark.seq)
        })
        .map_or(bookmark.line(), |(_, line)| line);
    editor.debug.unfocus();
    editor.without_history(|editor| {
        let view_id = editor.tree.focus;
        if editor.tree.get(view_id).doc != doc {
            editor.switch(doc, vortex_view::editor::Action::Replace);
        }
        let (view, document) = current!(editor);
        let text = document.text().slice(..);
        let line = line.min(text.len_lines().saturating_sub(1));
        let start = text.line_to_char(line);
        let pos = text
            .line(line)
            .chars()
            .position(|c| !c.is_whitespace())
            .map_or(start, |col| start + col);
        document.set_selection(view.id, Selection::point(pos));
        align_view(document, view, Align::Center);
    });
    editor.record_jump(Jump::Bookmark, from);
    true
}

/// A row of the bookmark picker.
#[derive(Debug, Clone)]
pub struct Row {
    pub number: usize,
    pub bookmark: Bookmark,
    pub path: Option<PathBuf>,
    pub name: String,
    pub text: String,
}

fn rows(editor: &Editor) -> Vec<Row> {
    editor
        .bookmarks()
        .into_iter()
        .enumerate()
        .map(|(index, bookmark)| {
            let line = bookmark.line();
            let (path, name, text) = match &bookmark.at {
                At::Buffer { doc, .. } => {
                    let document = &editor.documents[doc];
                    let text = document.text().slice(..);
                    let content = if line < text.len_lines() {
                        text.line(line).to_string()
                    } else {
                        String::new()
                    };
                    (
                        document.path().map(|path| path.to_path_buf()),
                        document.display_name().into_owned(),
                        content,
                    )
                }
                At::File { path, .. } => {
                    let content = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|text| text.lines().nth(line).map(str::to_string))
                        .unwrap_or_default();
                    (Some(path.clone()), String::new(), content)
                }
            };
            let name = match &path {
                Some(path) => vortex_stdx::path::get_relative_path(path)
                    .display()
                    .to_string(),
                None => name,
            };
            Row {
                number: index + 1,
                bookmark,
                path,
                name,
                text: text.trim().to_string(),
            }
        })
        .collect()
}

/// The key of the bookmark picker that removes the selected bookmark.
pub const REMOVE_KEY: &str = "C-x";

/// `space j`: the bookmarks in the order they were set, with their number, file and line; `Enter`
/// goes to one, `C-x` removes it.
pub fn bookmark_picker(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        open_picker(compositor, cx.editor, 0)
    }));
}

fn open_picker(compositor: &mut Compositor, editor: &mut Editor, cursor: u32) {
    let rows = rows(editor);
    if rows.is_empty() {
        editor.set_status("No bookmarks");
        return;
    }
    let columns = [
        PickerColumn::new("#", |row: &Row, _| row.number.to_string().into()),
        PickerColumn::new("file", |row: &Row, _| {
            format!("{}:{}", row.name, row.bookmark.line() + 1).into()
        }),
        PickerColumn::new("line", |row: &Row, _| row.text.as_str().into()),
    ];
    let picker = Picker::new(columns, 1, rows, (), |cx, row: &Row, action| {
        if !matches!(action, vortex_view::editor::Action::Replace) {
            // a split first, then the bookmark in it
            let doc = match &row.bookmark.at {
                At::Buffer { doc, .. } => Some(*doc),
                At::File { .. } => None,
            };
            if let Some(doc) = doc {
                cx.editor.switch(doc, action);
            }
        }
        go_to(cx.editor, &row.bookmark);
    })
    .with_modes()
    .without_header()
    .with_help(
        "Bookmarks",
        vec![(
            "bookmarks",
            vec![
                ("Enter", "go to it"),
                ("C-s C-v", "in a split"),
                ("C-x", "remove it"),
                ("C-y", "yank file:line"),
            ],
        )],
    )
    .with_initial_cursor(cursor)
    .with_preview(|_editor, row: &Row| match &row.bookmark.at {
        At::Buffer { doc, line } => Some(((*doc).into(), Some((*line, *line)))),
        At::File { path, line } => Some((path.as_path().into(), Some((*line, *line)))),
    })
    .with_stacked_key_handler(ctrl!('y'), |cx, row: &Row| {
        let place = match &row.bookmark.at {
            At::Buffer { doc, line } => crate::yank_place::doc_place(cx.editor, *doc, *line),
            At::File { path, line } => Some(format!(
                "{}:{}",
                crate::yank_place::shown_path(path, false),
                line + 1
            )),
        };
        match place {
            Some(place) => crate::yank_place::yank(cx.editor, vec![place]),
            None => cx.editor.set_error("This buffer has no file"),
        }
    })
    .with_stacked_key_handler(
        vortex_view::input::KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::CONTROL,
        },
        |cx, row: &Row| {
            cx.editor.remove_bookmark(row.bookmark.seq);
            let cursor = (row.number - 1) as u32;
            cx.editor
                .set_status(format!("Bookmark {} removed", row.number));
            // the picker again, with the numbers closing the gap
            cx.jobs.callback(async move {
                let call: job::Callback = job::Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor: &mut Compositor| {
                        compositor.remove(ui::picker::ID);
                        let left = editor.bookmarks().len() as u32;
                        if left > 0 {
                            open_picker(compositor, editor, cursor.min(left - 1));
                        }
                    },
                ));
                Ok(call)
            });
        },
    );
    compositor.push(Box::new(overlaid(picker)));
}

/// The bookmark picker's rows as text, for tests: `number  file:line  text`.
pub fn picker_rows(editor: &Editor) -> Vec<String> {
    rows(editor)
        .iter()
        .map(|row| {
            format!(
                "{}  {}:{}  {}",
                row.number,
                row.name,
                row.bookmark.line() + 1,
                row.text
            )
        })
        .collect()
}

/// The documents with bookmarks, for tests.
pub fn bookmarked_docs(editor: &Editor) -> Vec<DocumentId> {
    let mut docs: Vec<DocumentId> = editor
        .bookmarks()
        .iter()
        .filter_map(|bookmark| match bookmark.at {
            At::Buffer { doc, .. } => Some(doc),
            At::File { .. } => None,
        })
        .collect();
    docs.dedup();
    docs
}
