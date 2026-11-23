//! Tables: box-drawing borders, aligned columns, and cells that wrap inside their own column
//! when the table is wider than the page, so that a table stays a table at any width.
//!
//! The widths of the columns, when the table doesn't fit as it is:
//! 1. each column gets its widest word, or all of its text if that is narrower;
//! 2. what is left is shared out in proportion to what each column still lacks;
//! 3. if even the widest words don't fit, the width is shared in proportion to them, and words
//!    are cut.

use pulldown_cmark::Alignment;
use vortex_view::graphics::Style;

use super::parse::Atom;
use super::wrap::{natural_width, widest_word, wrap, Piece, Wrapped};

/// The columns' widths a table of `natural` widths (each column's text on one line) and `words`
/// (each column's widest word) takes in `available` cells of text (the borders and padding
/// already taken out).
pub fn column_widths(natural: &[usize], words: &[usize], available: usize) -> Vec<usize> {
    let natural: Vec<usize> = natural.iter().map(|&w| w.max(1)).collect();
    if natural.iter().sum::<usize>() <= available {
        return natural;
    }
    let base: Vec<usize> = natural
        .iter()
        .zip(words)
        .map(|(&natural, &word)| natural.min(word.max(1)))
        .collect();
    let base_sum: usize = base.iter().sum();
    if base_sum >= available {
        return share(available, &base, true);
    }
    let lacking: Vec<usize> = natural.iter().zip(&base).map(|(n, b)| n - b).collect();
    let extra = share(available - base_sum, &lacking, false);
    base.iter().zip(extra).map(|(b, e)| b + e).collect()
}

/// `total` shared in proportion to `weights` (with `at_least_one`, 1 each first when `total`
/// allows), the rounding going to the largest remainders.
fn share(total: usize, weights: &[usize], at_least_one: bool) -> Vec<usize> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let sum: usize = weights.iter().sum();
    if sum == 0 {
        return vec![0; n];
    }
    let minimum = usize::from(at_least_one && total >= n);
    let rest = total - minimum * n;
    let mut shares: Vec<usize> = weights.iter().map(|w| w * rest / sum).collect();
    let mut remainders: Vec<(usize, usize)> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| (w * rest % sum, i))
        .collect();
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut left = rest - shares.iter().sum::<usize>();
    for (_, i) in remainders {
        if left == 0 {
            break;
        }
        shares[i] += 1;
        left -= 1;
    }
    shares.iter().map(|s| s + minimum).collect()
}

/// A line of the table and the byte in the source of the row it is part of.
pub type TableLine = (Vec<Piece>, usize);

/// Lays out a table in `width` cells; its lines may be wider than `width` when even one cell
/// per column doesn't fit.
pub fn layout(
    aligns: &[Alignment],
    head: &[Vec<Atom>],
    rows: &[Vec<Vec<Atom>>],
    width: usize,
    source: usize,
    border: Style,
) -> Vec<TableLine> {
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain([head.len(), aligns.len()])
        .max()
        .unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let empty = Vec::new();
    let cell = |row: &'_ [Vec<Atom>], column: usize| -> Vec<Atom> {
        row.get(column).unwrap_or(&empty).clone()
    };
    let all_rows = || std::iter::once(head).chain(rows.iter().map(Vec::as_slice));
    let natural: Vec<usize> = (0..columns)
        .map(|c| {
            all_rows()
                .map(|row| natural_width(&cell(row, c)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let words: Vec<usize> = (0..columns)
        .map(|c| {
            all_rows()
                .map(|row| widest_word(&cell(row, c)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    // `│ ` before each column and ` ` after it, then the last `│`
    let overhead = 3 * columns + 1;
    let widths = column_widths(
        &natural,
        &words,
        width.saturating_sub(overhead).max(columns),
    );

    let rule = |left: &str, middle: &str, right: &str| -> Vec<Piece> {
        let mut text = left.to_string();
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                text.push_str(middle);
            }
            text.push_str(&"─".repeat(w + 2));
        }
        text.push_str(right);
        vec![Piece::new(text, border)]
    };

    let wrapped_row = |row: &[Vec<Atom>]| -> Vec<Vec<Wrapped>> {
        (0..columns)
            .map(|c| wrap(&cell(row, c), widths[c]))
            .collect()
    };
    let row_source = |row: &[Vec<Atom>]| -> usize {
        row.iter()
            .flat_map(|cell| cell.first())
            .map(|atom| atom.source)
            .next()
            .unwrap_or(source)
    };
    let mut lines: Vec<TableLine> = vec![(rule("┌", "┬", "┐"), source)];
    let emit_row = |lines: &mut Vec<TableLine>, cells: Vec<Vec<Wrapped>>, row_at: usize| {
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for k in 0..height {
            let mut pieces = vec![Piece::new("│", border)];
            for (c, cell) in cells.iter().enumerate() {
                let (content, used) = cell
                    .get(k)
                    .map(|line| (line.pieces.clone(), line.width))
                    .unwrap_or_default();
                let free = widths[c].saturating_sub(used);
                let (before, after) = match aligns.get(c).copied().unwrap_or(Alignment::None) {
                    Alignment::Right => (free, 0),
                    Alignment::Center => (free / 2, free - free / 2),
                    Alignment::Left | Alignment::None => (0, free),
                };
                pieces.push(Piece::new(" ".repeat(1 + before), Style::default()));
                pieces.extend(content);
                pieces.push(Piece::new(" ".repeat(after + 1), Style::default()));
                pieces.push(Piece::new("│", border));
            }
            lines.push((pieces, row_at));
        }
    };

    let body: Vec<(Vec<Vec<Wrapped>>, usize)> = rows
        .iter()
        .map(|row| (wrapped_row(row), row_source(row)))
        .collect();
    // rows of several lines are told apart by a rule between them
    let separated = body
        .iter()
        .any(|(cells, _)| cells.iter().any(|cell| cell.len() > 1));
    if !head.is_empty() {
        emit_row(&mut lines, wrapped_row(head), row_source(head));
        lines.push((rule("├", "┼", "┤"), row_source(head)));
    }
    let count = body.len();
    for (i, (cells, at)) in body.into_iter().enumerate() {
        emit_row(&mut lines, cells, at);
        if separated && i + 1 < count {
            lines.push((rule("├", "┼", "┤"), at));
        }
    }
    let last = lines.last().map(|(_, at)| *at).unwrap_or(source);
    lines.push((rule("└", "┴", "┘"), last));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_that_fits_keeps_its_widths() {
        assert_eq!(column_widths(&[3, 10, 5], &[3, 4, 5], 30), [3, 10, 5]);
        // an empty column still has a cell
        assert_eq!(column_widths(&[0, 2], &[0, 2], 30), [1, 2]);
    }

    #[test]
    fn columns_keep_their_widest_word_and_share_the_rest() {
        // natural 10 + 40 = 50 in 30: words 4 and 6 first, then 20 shared as 6 : 34 lacking
        let widths = column_widths(&[10, 40], &[4, 6], 30);
        assert_eq!(widths.iter().sum::<usize>(), 30);
        assert!(widths[0] >= 4 && widths[1] >= 6);
        assert_eq!(widths, [7, 23]);
        // a short column takes no more than its text
        assert_eq!(column_widths(&[2, 40], &[2, 6], 20), [2, 18]);
    }

    #[test]
    fn when_words_dont_fit_they_share_the_width() {
        let widths = column_widths(&[20, 20], &[20, 10], 9);
        assert_eq!(widths.iter().sum::<usize>(), 9);
        assert_eq!(widths, [6, 3]);
        // never less than one cell each, when there is room for that
        assert_eq!(column_widths(&[50, 1, 1], &[50, 1, 1], 3), [1, 1, 1]);
    }

    #[test]
    fn shares_round_to_the_largest_remainders() {
        assert_eq!(share(10, &[1, 1, 1], true), [4, 3, 3]);
        assert_eq!(share(7, &[0, 5], true), [1, 6]);
        assert_eq!(share(7, &[0, 5], false), [0, 7]);
        assert_eq!(share(0, &[1, 2], true), [0, 0]);
    }
}
