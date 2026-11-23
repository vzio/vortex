//! The rulers of a view whose lines are scrolled sideways by different amounts (this fork:
//! selective soft wrap): each visual line draws them at its own offset, so a ruler stays on the
//! same column of the text on the lines that wrap and on those that don't.
//!
//! Without selective soft wrap every line has the same offset and `EditorView::render_rulers`
//! draws them down the whole view at once.

use vortex_view::graphics::Rect;
use vortex_view::theme::Style;

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

pub struct Rulers {
    /// The rulers' columns, 1-based, as in the config.
    columns: Vec<u16>,
    style: Style,
    /// The sideways offset each row of the view was drawn with, `None` for rows with no text yet
    /// (the ghost text below a line, the rows past the end of the document).
    rows: Vec<Option<usize>>,
}

impl Rulers {
    pub fn new(columns: Vec<u16>, style: Style, height: u16) -> Self {
        Self {
            columns,
            style,
            rows: vec![None; height as usize],
        }
    }

    /// Draws the rulers on `row` of the view (counted from the top of the viewport), for a line
    /// scrolled `offset` columns sideways.
    fn draw(&self, renderer: &mut TextRenderer, row: u16, offset: usize) {
        let viewport = renderer.viewport;
        for &column in &self.columns {
            let Some(col) = (column as usize).checked_sub(1 + offset) else {
                continue;
            };
            if col < viewport.width as usize {
                let y = row + renderer.offset.row as u16;
                renderer.set_style(Rect::new(viewport.x + col as u16, y, 1, 1), self.style);
            }
        }
    }
}

impl Decoration for Rulers {
    // before the line's text (and after nothing else: it's the first decoration), as the rulers
    // drawn down the whole view are
    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        let Some(row) = (pos.visual_line as usize).checked_sub(renderer.offset.row) else {
            return;
        };
        let Some(slot) = self.rows.get_mut(row) else {
            return;
        };
        let offset = renderer.offset.col;
        *slot = Some(offset);
        self.draw(renderer, row as u16, offset);
    }

    // the rows with no text: the ghost text below a line takes the line's offset, the rows past
    // the end of the document none
    fn finish_rendering(&mut self, renderer: &mut TextRenderer) {
        let last_text_row = self.rows.iter().rposition(Option::is_some);
        let mut offset = 0;
        for row in 0..self.rows.len() {
            match self.rows[row] {
                Some(line_offset) => offset = line_offset,
                None => {
                    let offset = if last_text_row.is_some_and(|last| row < last) {
                        offset
                    } else {
                        0
                    };
                    self.draw(renderer, row as u16, offset);
                }
            }
        }
    }
}
