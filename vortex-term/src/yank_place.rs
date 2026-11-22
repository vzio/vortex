//! Yanking places (this fork): `src/main.rs:42`, `src/main.rs:12-14`, to the clipboard, from the
//! selections (`Space :`) and from the rows of the pickers that list places (`C-y`).
//!
//! A path is relative to the workspace root (the folder with `.git`, `.jj`, ... above the working
//! directory) when the file is in it, absolute otherwise or when asked for. Lines count from 1.

use std::path::Path;

use vortex_core::{coords_at_pos, Range, RopeSlice};
use vortex_view::{DocumentId, Editor};

/// The register places go to: the clipboard.
const CLIPBOARD: char = '+';

/// How a place is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `src/main.rs:12-14`: the lines of the selection.
    Lines,
    /// `/home/me/project/src/main.rs:12-14`.
    Absolute,
    /// `src/main.rs:42:7`: the cursor's line and column.
    Column,
}

/// `path` relative to the workspace root when it is in it, else as it is.
pub fn shown_path(path: &Path, absolute: bool) -> String {
    let (root, _) = vortex_loader::find_workspace();
    shown_path_in(path, &root, absolute)
}

fn shown_path_in(path: &Path, root: &Path, absolute: bool) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if !absolute && !relative.as_os_str().is_empty() => {
            relative.display().to_string()
        }
        _ => path.display().to_string(),
    }
}

/// `42` for one line, `12-14` for several: `first` and `last` count from 0, both included.
pub fn lines(first: usize, last: usize) -> String {
    if last > first {
        format!("{}-{}", first + 1, last + 1)
    } else {
        format!("{}", first + 1)
    }
}

/// The place of a selection's range in `path` (already shown, see [`shown_path`]).
fn range_place(path: &str, text: RopeSlice, range: &Range, form: Form) -> String {
    match form {
        Form::Lines | Form::Absolute => {
            let (first, last) = range.line_range(text);
            format!("{path}:{}", lines(first, last))
        }
        Form::Column => {
            let cursor = coords_at_pos(text, range.cursor(text));
            format!("{path}:{}:{}", cursor.row + 1, cursor.col + 1)
        }
    }
}

/// The places of the selections of the focused view, one per selection, without repeats.
pub fn selection_places(editor: &Editor, form: Form) -> Result<Vec<String>, &'static str> {
    let (view, doc) = vortex_view::current_ref!(editor);
    let Some(path) = doc.path() else {
        return Err("This buffer has no file");
    };
    let path = shown_path(path, form == Form::Absolute);
    let text = doc.text().slice(..);
    let mut places: Vec<String> = Vec::new();
    for range in doc.selection(view.id).iter() {
        let place = range_place(&path, text, range, form);
        if !places.contains(&place) {
            places.push(place);
        }
    }
    Ok(places)
}

/// `path:line` for a line (from 0) of a buffer, if the buffer has a file.
pub fn doc_place(editor: &Editor, doc: DocumentId, line: usize) -> Option<String> {
    let path = editor.document(doc)?.path()?;
    Some(format!("{}:{}", shown_path(path, false), line + 1))
}

/// Writes `places` to the clipboard, one per line, and says so: `yanked src/main.rs:12-14`, or
/// `yanked 3 places`.
pub fn yank(editor: &mut Editor, places: Vec<String>) {
    let said = match places.as_slice() {
        [] => return,
        [one] => format!("yanked {one}"),
        many => format!("yanked {} places", many.len()),
    };
    match editor.registers.write(CLIPBOARD, vec![places.join("\n")]) {
        Ok(()) => editor.set_status(said),
        Err(err) => editor.set_error(err.to_string()),
    }
}

/// Yanks the places of the selections, or says why there are none.
pub fn yank_selections(editor: &mut Editor, form: Form) {
    match selection_places(editor, form) {
        Ok(places) => yank(editor, places),
        Err(err) => editor.set_error(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use vortex_core::Rope;

    #[test]
    fn a_path_in_the_workspace_is_relative() {
        let root = PathBuf::from("/work/project");
        let path = root.join("src/main.rs");
        assert_eq!(shown_path_in(&path, &root, false), "src/main.rs");
        assert_eq!(
            shown_path_in(&path, &root, true),
            "/work/project/src/main.rs"
        );
        let outside = PathBuf::from("/etc/hosts");
        assert_eq!(shown_path_in(&outside, &root, false), "/etc/hosts");
    }

    #[test]
    fn lines_count_from_one_and_a_range_has_a_dash() {
        assert_eq!(lines(41, 41), "42");
        assert_eq!(lines(11, 13), "12-14");
    }

    #[test]
    fn a_selection_is_its_lines_and_a_cursor_its_column() {
        let text = Rope::from("fn main() {\n    let x = 1;\n    x\n}\n");
        let text = text.slice(..);
        // `x = 1;\n    x` from line 2 to line 3
        let range = Range::new(20, 32);
        assert_eq!(range_place("a.rs", text, &range, Form::Lines), "a.rs:2-3");
        // a whole line selected with `x`: up to the start of the next one, still one line
        let line = Range::new(12, 27);
        assert_eq!(range_place("a.rs", text, &line, Form::Lines), "a.rs:2");
        // the cursor at `x` of line 3
        let cursor = Range::point(31);
        assert_eq!(range_place("a.rs", text, &cursor, Form::Column), "a.rs:3:5");
    }
}
