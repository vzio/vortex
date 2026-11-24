//! Draws a terminal's window: the screen of its shell cell by cell, in the colors the programs set
//! (indexed colors stay indexed, so the outer terminal's palette applies), or through the theme's
//! phosphor filter ([`phosphor`](super::phosphor)) when it sets one. In view mode, the lines of
//! the snapshot from the window's scroll position, with the selections and the cursor over them.

use tui::buffer::Buffer as Surface;

use super::phosphor::Phosphor;
use vortex_core::Position;
use vortex_view::{
    document::Mode,
    graphics::{Color, CursorKind, Modifier, Rect, Style, UnderlineStyle},
    terminal::{self, CellStyle, CursorShape, Snapshot},
    Document, Editor, View,
};

/// The base style of a terminal (`ui.terminal`, else `ui.background`) and the phosphor filter.
fn base(editor: &Editor) -> (Style, Option<Phosphor>) {
    let theme = &editor.theme;
    let base = theme
        .try_get_exact("ui.terminal")
        .unwrap_or_else(|| theme.get("ui.background"));
    // the phosphor filter (this fork), only when the theme sets one
    let phosphor = theme
        .effects()
        .terminal_phosphor
        .and_then(|color| Phosphor::new(color, base));
    let base = match &phosphor {
        Some(phosphor) => phosphor.style(base),
        None => base,
    };
    (base, phosphor)
}

/// Draws the terminal of `doc` in its window `view` (above the statusline).
pub fn render(editor: &Editor, doc: &Document, view: &View, surface: &mut Surface, focused: bool) {
    let area = view.inner_area(doc);
    let (base, phosphor) = base(editor);
    surface.set_style(area, base);
    let Some(terminal) = editor.terminals.get(doc.id()) else {
        return;
    };
    if let Some(snapshot) = &terminal.snapshot {
        render_snapshot(
            editor,
            doc,
            view,
            snapshot,
            area,
            base,
            phosphor.as_ref(),
            surface,
        );
        return;
    }
    let shell = &terminal.shell;
    // cleared before reading the screen, so output that comes after sends a new redraw
    shell.take_output_pending();
    let style = |cell: CellStyle| match &phosphor {
        Some(phosphor) => phosphor.style(base.patch(cell_style(cell))),
        None => base.patch(cell_style(cell)),
    };
    shell.visit_cells(|row, col, cell| {
        if row >= area.height || col >= area.width || cell.text.is_empty() {
            return;
        }
        let (x, y) = (area.x + col, area.y + row);
        let style = style(cell.style());
        if cell.text == " " {
            surface[(x, y)].set_symbol(" ").set_style(style);
        } else {
            surface.set_stringn(x, y, &cell.text, (area.right() - x) as usize, style);
        }
    });
    // normal mode: the cursor is Vortex's, where the program's is
    if focused && editor.mode() == Mode::Normal {
        if let Some(cursor) = shell.cursor() {
            if cursor.row < area.height && cursor.col < area.width {
                let cell = &mut surface[(area.x + cursor.col, area.y + cursor.row)];
                cell.set_style(cursor_style(editor, true));
            }
        }
    }
}

/// The style of a cursor drawn in a terminal, as the editor's in normal mode.
fn cursor_style(editor: &Editor, primary: bool) -> Style {
    let theme = &editor.theme;
    let base = theme.get("ui.cursor");
    let scope = match editor.mode() {
        Mode::Select if primary => "ui.cursor.primary.select",
        Mode::Select => "ui.cursor.select",
        _ if primary => "ui.cursor.primary.normal",
        _ => "ui.cursor.normal",
    };
    let style = theme
        .try_get(scope)
        .or_else(|| {
            primary
                .then(|| theme.try_get("ui.cursor.primary"))
                .flatten()
        })
        .unwrap_or(base);
    if style.bg.is_none() && style.fg.is_none() && style.add_modifier.is_empty() {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

/// View mode: the snapshot's lines from the window's scroll position, the selections and the
/// cursors over them.
#[allow(clippy::too_many_arguments)]
fn render_snapshot(
    editor: &Editor,
    doc: &Document,
    view: &View,
    snapshot: &Snapshot,
    area: Rect,
    base: Style,
    phosphor: Option<&Phosphor>,
    surface: &mut Surface,
) {
    let text = doc.text();
    let offset = doc.view_offset(view.id);
    let top = text.char_to_line(offset.anchor.min(text.len_chars()));
    let left = offset.horizontal_offset as u16;
    let style = |cell: CellStyle| match phosphor {
        Some(phosphor) => phosphor.style(base.patch(cell_style(cell))),
        None => base.patch(cell_style(cell)),
    };
    let theme = &editor.theme;
    let selection_style = theme.get("ui.selection");
    let primary_selection_style = theme
        .try_get("ui.selection.primary")
        .unwrap_or(selection_style);
    let selection = doc.selection(view.id);
    let primary = selection.primary_index();

    for row in 0..area.height {
        let index = top + row as usize;
        let Some(line) = snapshot.lines.get(index) else {
            break;
        };
        let y = area.y + row;
        let line_start = text.line_to_char(index);
        let line_end = line_start + line.text.chars().count();
        // the cells of the line's characters
        let cell_of = |char_index: usize| -> Option<u16> {
            let col = *line.cols.get(char_index)?;
            col.checked_sub(left).filter(|&col| col < area.width)
        };
        for (i, c) in line.text.chars().enumerate() {
            let Some(col) = cell_of(i) else {
                continue;
            };
            let x = area.x + col;
            let mut buf = [0; 4];
            surface.set_stringn(
                x,
                y,
                c.encode_utf8(&mut buf),
                (area.right() - x) as usize,
                style(line.style_at(i)),
            );
        }
        // the selections (each including the newline at a line's end, drawn after the text)
        for (n, range) in selection.iter().enumerate() {
            let (from, to) = (range.from(), range.to());
            if to <= line_start && from != to || from > line_end {
                continue;
            }
            let selected = if n == primary {
                primary_selection_style
            } else {
                selection_style
            };
            for pos in from.max(line_start)..to.min(line_end + 1) {
                let col = if pos == line_end {
                    line.cols
                        .last()
                        .map(|&col| col + 1)
                        .unwrap_or(0)
                        .checked_sub(left)
                        .filter(|&col| col < area.width)
                } else {
                    cell_of(pos - line_start)
                };
                if let Some(col) = col {
                    let cell = &mut surface[(area.x + col, y)];
                    cell.set_style(selected);
                }
            }
            // the cursor of the range
            let head = range.cursor(text.slice(..));
            if head >= line_start && head <= line_end {
                let col = if head == line_end {
                    line.cols
                        .last()
                        .map(|&col| col + 1)
                        .unwrap_or(0)
                        .checked_sub(left)
                        .filter(|&col| col < area.width)
                } else {
                    cell_of(head - line_start)
                };
                if let Some(col) = col {
                    surface[(area.x + col, y)].set_style(cursor_style(editor, n == primary));
                }
            }
        }
    }
}

fn color(color: terminal::Color) -> Option<Color> {
    match color {
        terminal::Color::Default => None,
        terminal::Color::Indexed(index) => Some(Color::Indexed(index)),
        terminal::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn cell_style(cell: CellStyle) -> Style {
    let mut style = Style::default();
    if let Some(fg) = color(cell.fg) {
        style = style.fg(fg);
    }
    if let Some(bg) = color(cell.bg) {
        style = style.bg(bg);
    }
    for (on, modifier) in [
        (cell.bold, Modifier::BOLD),
        (cell.dim, Modifier::DIM),
        (cell.italic, Modifier::ITALIC),
        (cell.strikeout, Modifier::CROSSED_OUT),
        (cell.inverse, Modifier::REVERSED),
        (cell.hidden, Modifier::HIDDEN),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    if cell.underline {
        style = style.underline_style(UnderlineStyle::Line);
    }
    style
}

/// The cursor of the focused terminal: in insert mode the program's, where and as it shows it;
/// in normal and view mode the terminal draws Vortex's itself.
pub fn cursor(editor: &Editor) -> Option<(Position, CursorKind)> {
    let (doc, view) = super::modes::focused(editor)?;
    let terminal = editor.terminals.get(doc)?;
    let document = editor.document(doc)?;
    let area = editor.tree.get(view).inner_area(document);
    if editor.mode() != Mode::Insert || terminal.viewing() {
        return Some((
            Position::new(area.y as usize, area.x as usize),
            CursorKind::Hidden,
        ));
    }
    let Some(cursor) = terminal.shell.cursor() else {
        return Some((
            Position::new(area.y as usize, area.x as usize),
            CursorKind::Hidden,
        ));
    };
    let kind = match cursor.shape {
        CursorShape::Block => CursorKind::Block,
        CursorShape::Underline => CursorKind::Underline,
        CursorShape::Beam => CursorKind::Bar,
    };
    let row = (area.y + cursor.row).min(area.bottom().saturating_sub(1));
    let col = (area.x + cursor.col).min(area.right().saturating_sub(1));
    Some((Position::new(row as usize, col as usize), kind))
}
