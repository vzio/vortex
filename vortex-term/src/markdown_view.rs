//! The Markdown render mode (this fork, see FORK.md, "Markdown render mode"): a Markdown buffer
//! shown as a rendered, read-only page, one key away from its source.
//!
//! Like the dashboard, it isn't a buffer: the [`EditorView`](crate::ui::EditorView) draws it over
//! the focused view while that view shows the buffer, in normal mode, unedited. So a key that
//! edits or leaves normal mode (`i`, `o`, `c`, `v`...), another buffer in the view, or a split
//! makes it vanish, with no hook of its own.
//!
//! Keys: its own first ([`MarkdownView::handle_key`]: `]`, `[`, `t`, `Tab`, `Enter`, `?`, `Esc`);
//! then the keymap, whose commands it takes over where it has its own version
//! ([`MarkdownView::command`]: moving, scrolling, searching, the toggle). Every other command runs
//! on the source, at the line under the reading line ([`MarkdownView::sync_source`]), so that `i`
//! inserts there and `:`, `space` and the rest work as usual.
//!
//! - [`parse`]: Markdown to styled blocks.
//! - [`page`]: the blocks laid out at a width, the headings and links.
//! - [`wrap`]: wrapping styled text.
//! - [`table`]: tables whose cells wrap in their columns.

pub mod page;
pub mod parse;
pub mod table;
pub mod wrap;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tui::buffer::Buffer as Surface;
use vortex_core::regex::{Regex, RegexBuilder};
use vortex_core::unicode::width::UnicodeWidthStr;
use vortex_core::Selection;
use vortex_view::document::Mode;
use vortex_view::editor::{Action, MarkdownViewStartup};
use vortex_view::graphics::{Rect, Style};
use vortex_view::history::Jump;
use vortex_view::screen::{Placement, Screen, ScreenKind};
use vortex_view::input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use vortex_view::keyboard::{KeyCode, KeyModifiers};
use vortex_view::view::ViewPosition;
use vortex_view::{Document, DocumentId, Editor, ViewId};

use crate::commands::{self, MappableCommand};
use crate::compositor::Compositor;
use crate::ui::{self, keyhelp::KeyHelp, overlay::overlaid, Picker, PickerColumn, PromptEvent};

use page::Page;
use parse::Target;

/// Whether `doc` is Markdown, by its language configuration.
pub fn is_markdown(doc: &Document) -> bool {
    doc.language_name() == Some("markdown")
}

/// Whether a Markdown buffer opened this way opens rendered: `browsing` for the file tree, the
/// command line and links in the render mode.
pub fn opens_rendered(editor: &Editor, doc: &Document, browsing: bool) -> bool {
    is_markdown(doc)
        && match editor.config().markdown_view {
            MarkdownViewStartup::Always => true,
            MarkdownViewStartup::Browse => browsing,
            MarkdownViewStartup::Never => false,
        }
}

/// Where the render mode starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// At the source's cursor, the top of the view at the source's top.
    Source,
    /// At these lines of the page (shown again from history).
    Page { top: usize, reading: usize },
    /// At this heading (a link to `file.md#heading`); at the top if there is none.
    Anchor(String),
}

/// What a key did in the render mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyResult {
    /// Not one of its keys: the keymap takes it.
    Ignored,
    Consumed,
    /// Back to the source.
    Close,
}

/// What the render mode does with a command of the keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandResult {
    /// Its own version ran.
    Taken,
    /// Back to the source (the toggle).
    Close,
    /// Runs on the source, at the reading line (now synced).
    Run,
}

/// The search typed in the prompt, shared with it.
#[derive(Debug, Default)]
struct Search {
    query: String,
    /// The query before this search, back on `Esc`.
    previous: String,
    /// The reading line when the search started.
    from: usize,
    changed: bool,
    aborted: bool,
}

/// The page laid out, and what it was laid out for.
struct Laid {
    page: Page,
    version: i32,
    widths: page::Widths,
    theme: String,
}

/// Where the page goes in the area drawn.
#[derive(Debug, Clone, Copy)]
struct Columns {
    widths: page::Widths,
    /// The prose's left edge.
    left: u16,
    /// The window's columns, its margins left out: what tables and code blocks may take.
    inner_left: u16,
    inner_right: u16,
}

impl Columns {
    /// Where the line `line` starts on the screen.
    fn line_left(&self, line: &page::Line) -> u16 {
        self.left
            .saturating_sub(line.shift as u16)
            .max(self.inner_left)
    }
}

pub struct MarkdownView {
    pub view: ViewId,
    pub doc: DocumentId,
    version: i32,
    laid: Option<Laid>,
    start: Option<Start>,
    /// The first line of the page on the screen.
    top: usize,
    /// The reading line: what `i` edits, where `Enter` looks for a link.
    reading: usize,
    /// The link selected with `Tab`: its line and its place among the line's links.
    selected: Option<(usize, usize)>,
    search: Rc<RefCell<Search>>,
    regex: Option<Regex>,
    matches: Vec<(usize, usize, usize)>,
    /// The reading line the source was last synced to.
    synced: Option<usize>,
    /// How far each code block is scrolled sideways, by its index; 0 when not here. Kept while
    /// the render mode is up, for the page's version.
    offsets: HashMap<usize, usize>,
    /// The area of the last drawing (the page and its status row).
    area: Rect,
}

impl MarkdownView {
    /// The render mode of the focused view's buffer, or why not.
    pub fn open(editor: &mut Editor, start: Start) -> Result<Self, &'static str> {
        let view = editor.tree.focus;
        let Some(doc_id) = editor.tree.try_get(view).map(|view| view.doc) else {
            return Err("Not a Markdown buffer");
        };
        let doc = &editor.documents[&doc_id];
        if !is_markdown(doc) {
            return Err("Not a Markdown buffer");
        }
        if editor.mode() != Mode::Normal {
            return Err("Only in normal mode");
        }
        Ok(Self {
            view,
            doc: doc_id,
            version: doc.version(),
            laid: None,
            start: Some(start),
            top: 0,
            reading: 0,
            selected: None,
            search: Default::default(),
            regex: None,
            matches: Vec::new(),
            synced: None,
            offsets: HashMap::new(),
            area: editor.tree.area(),
        })
    }

    /// Whether the text window it covers still shows its buffer (the page syncs that window's
    /// cursor to the reading line).
    pub fn covers(&self, editor: &Editor) -> bool {
        editor
            .tree
            .try_get(self.view)
            .is_some_and(|view| view.doc == self.doc)
    }

    /// Shown again over the text window `view` (history).
    pub fn reopened(&mut self, view: ViewId) {
        self.view = view;
    }

    /// The top and reading lines, for showing it again later.
    pub fn position(&self) -> (usize, usize) {
        (self.top, self.reading)
    }

    /// The page as it is now, laid out for the last area drawn.
    pub fn page(&mut self, editor: &mut Editor) -> &Page {
        self.lay_out(editor);
        &self.laid.as_ref().unwrap().page
    }

    /// The lines of the page as text (for tests).
    pub fn text(&mut self, editor: &mut Editor) -> Vec<String> {
        self.page(editor)
            .lines
            .iter()
            .map(|line| line.text())
            .collect()
    }

    pub fn reading(&self) -> usize {
        self.reading
    }

    pub fn top(&self) -> usize {
        self.top
    }

    /// The page's widths in `area` (the prose's, capped by `markdown-width`, and the window's
    /// less a cell each side), the prose's left edge (centered), and the window's columns that
    /// tables and code blocks may take.
    fn page_columns(&self, editor: &Editor) -> Columns {
        let area = self.area;
        let margin = u16::from(area.width > 20);
        let wide = (area.width - 2 * margin).max(1) as usize;
        let cap = editor.config().markdown_width as usize;
        let prose = if cap == 0 { wide } else { wide.min(cap) };
        let widths = page::Widths { prose, wide };
        let inner_left = area.x + margin;
        Columns {
            widths,
            left: inner_left + widths.left_room() as u16,
            inner_left,
            inner_right: inner_left + wide as u16,
        }
    }

    /// The rows of the page on the screen (the last row is the status row).
    fn height(&self) -> usize {
        (self.area.height as usize).saturating_sub(1).max(1)
    }

    /// Lays the page out again if the buffer, the width or the theme changed, keeping the place.
    fn lay_out(&mut self, editor: &mut Editor) {
        let widths = self.page_columns(editor).widths;
        let Some(doc) = editor.document(self.doc) else {
            return;
        };
        let version = doc.version();
        let theme = editor.theme.name().to_string();
        if self.laid.as_ref().is_some_and(|laid| {
            laid.version == version && laid.widths == widths && laid.theme == theme
        }) {
            return;
        }
        // keep the place by the lines of the source
        let kept = self.laid.as_ref().map(|laid| {
            let line = |i: usize| laid.page.lines.get(i).map_or(0, |line| line.source);
            (line(self.top), line(self.reading))
        });
        let text = doc.text().to_string();
        let loader = editor.syn_loader.load();
        let page = Page::new(&text, widths, &editor.theme, &loader);
        if self
            .laid
            .as_ref()
            .is_some_and(|laid| laid.version != version)
        {
            // other code blocks, maybe
            self.offsets.clear();
        }
        if version != self.version {
            self.version = version;
        }
        self.laid = Some(Laid {
            page,
            version,
            widths,
            theme,
        });
        if let Some((top, reading)) = kept {
            let page = &self.laid.as_ref().unwrap().page;
            self.top = page.line_of_source(top);
            self.reading = page.line_of_source(reading);
            self.synced = None;
        }
        let blocks = &self.laid.as_ref().unwrap().page.code_blocks;
        self.offsets.retain(|block, offset| {
            blocks.get(*block).is_some_and(|block| {
                *offset = (*offset).min(block.max_offset());
                *offset > 0
            })
        });
        self.find_matches();
        if let Some(start) = self.start.take() {
            self.start_at(editor, start);
        }
        self.clamp();
    }

    fn start_at(&mut self, editor: &Editor, start: Start) {
        let page = &self.laid.as_ref().unwrap().page;
        match start {
            Start::Source => {
                let Some(view) = editor.tree.try_get(self.view) else {
                    return;
                };
                let doc = &editor.documents[&self.doc];
                let text = doc.text().slice(..);
                let cursor = doc.selection(view.id).primary().cursor(text);
                let anchor = doc.view_offset(view.id).anchor.min(text.len_chars());
                self.reading = page.line_of_source(text.char_to_line(cursor));
                self.top = page.line_of_source(text.char_to_line(anchor));
                // the source was where the page starts: going back without moving keeps it
                self.synced = Some(self.reading);
            }
            Start::Page { top, reading } => {
                self.top = top;
                self.reading = reading;
            }
            Start::Anchor(anchor) => {
                let line = page.anchor(&anchor).unwrap_or(0);
                self.reading = line;
                self.top = line;
            }
        }
        self.keep_visible();
    }

    fn len(&self) -> usize {
        self.laid
            .as_ref()
            .map_or(0, |laid| laid.page.lines.len())
            .max(1)
    }

    fn max_top(&self) -> usize {
        self.len().saturating_sub(self.height())
    }

    fn clamp(&mut self) {
        self.reading = self.reading.min(self.len() - 1);
        self.top = self.top.min(self.max_top());
    }

    fn scrolloff(&self, editor: &Editor) -> usize {
        editor
            .config()
            .scrolloff
            .min(self.height().saturating_sub(1) / 2)
    }

    /// Scrolls so that the reading line is on the screen.
    fn keep_visible(&mut self) {
        self.clamp();
        let height = self.height();
        if self.reading < self.top {
            self.top = self.reading;
        } else if self.reading >= self.top + height {
            self.top = self.reading + 1 - height;
        }
        self.clamp();
    }

    /// Moves the reading line by `delta` lines, the page following with a margin.
    fn move_reading(&mut self, editor: &Editor, delta: isize) {
        self.reading = self
            .reading
            .saturating_add_signed(delta)
            .min(self.len() - 1);
        let margin = self.scrolloff(editor);
        let height = self.height();
        if self.reading < self.top + margin {
            self.top = self.reading.saturating_sub(margin);
        } else if self.reading + margin >= self.top + height {
            self.top = (self.reading + margin + 1).saturating_sub(height);
        }
        self.clamp();
    }

    /// Scrolls the page by `delta` lines, the reading line staying on the screen.
    fn scroll(&mut self, editor: &Editor, delta: isize) {
        self.top = self.top.saturating_add_signed(delta).min(self.max_top());
        let margin = self.scrolloff(editor);
        let height = self.height();
        let lowest = self.top + margin.min(self.top);
        let highest = (self.top + height).saturating_sub(1 + margin).max(lowest);
        let at_end = self.top == self.max_top();
        if self.reading < lowest {
            self.reading = lowest;
        } else if self.reading > highest && !at_end {
            self.reading = highest;
        }
        self.clamp();
    }

    /// Puts the reading line on `line`, at the top of the screen.
    pub fn go_to_line(&mut self, line: usize) {
        self.reading = line.min(self.len() - 1);
        self.top = self.reading;
        self.selected = None;
        self.clamp();
    }

    /// Handles one of its own keys: `]`/`[` (headings), `t` (the outline), `Tab`/`S-Tab` (links),
    /// `Enter` (follows a link), `?` (the keys), `Esc` (back to the source).
    pub fn handle_key(&mut self, key: KeyEvent, cx: &mut commands::Context) -> KeyResult {
        self.lay_out(cx.editor);
        self.apply_search();
        // (the count typed before the key is the editor's: the context of a key has none yet)
        let count = cx.editor.count.map_or(cx.count(), |count| count.get());
        let shift_only = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => return KeyResult::Close,
            KeyCode::Char(']') if shift_only => self.heading(count, true),
            KeyCode::Char('[') if shift_only => self.heading(count, false),
            KeyCode::Char('t') if key.modifiers.is_empty() => self.outline(cx),
            KeyCode::Char('H') if shift_only => self.scroll_code(Sideways::HalfLeft, count),
            KeyCode::Char('L') if shift_only => self.scroll_code(Sideways::HalfRight, count),
            KeyCode::Char('?') if shift_only => {
                cx.callback.push(help().push());
            }
            KeyCode::Tab if key.modifiers.is_empty() => self.next_link(true),
            KeyCode::Tab if key.modifiers == KeyModifiers::SHIFT => self.next_link(false),
            KeyCode::Enter if key.modifiers.is_empty() => self.follow(cx),
            _ => return KeyResult::Ignored,
        }
        cx.editor.count = None;
        KeyResult::Consumed
    }

    /// Runs the render mode's version of `command` of the keymap, or syncs the source for it.
    pub fn command(
        &mut self,
        command: &MappableCommand,
        cx: &mut commands::Context,
    ) -> CommandResult {
        self.lay_out(cx.editor);
        self.apply_search();
        let count = cx.count() as isize;
        let half = (self.height() / 2).max(1) as isize;
        let full = self.height().max(1) as isize;
        match command.name() {
            "move_visual_line_down" | "move_line_down" => self.move_reading(cx.editor, count),
            "move_visual_line_up" | "move_line_up" => self.move_reading(cx.editor, -count),
            "page_cursor_half_down" => self.page_move(half),
            "page_cursor_half_up" => self.page_move(-half),
            "page_down" | "page_cursor_down" => self.page_move(full),
            "page_up" | "page_cursor_up" => self.page_move(-full),
            "scroll_down" => self.scroll(cx.editor, count),
            "scroll_up" => self.scroll(cx.editor, -count),
            "goto_file_start" => {
                self.reading = cx.count.map_or(0, |n| n.get() - 1).min(self.len() - 1);
                self.top = 0;
                self.keep_visible();
            }
            "goto_last_line" | "goto_file_end" => {
                self.reading = self.len() - 1;
                self.top = self.max_top();
            }
            "goto_window_top" => self.reading = self.top + self.scrolloff(cx.editor),
            "goto_window_center" => self.reading = self.top + self.height() / 2,
            "goto_window_bottom" => {
                self.reading =
                    (self.top + self.height()).saturating_sub(1 + self.scrolloff(cx.editor))
            }
            "move_char_left" => self.scroll_code(Sideways::Left, count as usize),
            "move_char_right" => self.scroll_code(Sideways::Right, count as usize),
            "goto_line_start" | "goto_first_nonwhitespace" => self.scroll_code(Sideways::Start, 1),
            "goto_line_end" => self.scroll_code(Sideways::End, 1),
            "search" => self.search_prompt(cx),
            "search_next" => self.next_match(cx.editor, true),
            "search_prev" => self.next_match(cx.editor, false),
            "markdown_view" => return CommandResult::Close,
            _ => {
                self.sync_source(cx.editor, false);
                return CommandResult::Run;
            }
        }
        self.clamp();
        self.selected = None;
        CommandResult::Taken
    }

    /// `C-d`, `C-u`, `C-f`, `C-b`: the page and the reading line move together.
    fn page_move(&mut self, delta: isize) {
        self.top = self.top.saturating_add_signed(delta).min(self.max_top());
        self.reading = self
            .reading
            .saturating_add_signed(delta)
            .min(self.len() - 1);
        self.keep_visible();
    }

    /// The mouse: the wheel scrolls, a click puts the reading line there (and selects the link
    /// under it). Returns whether it was over the page.
    pub fn handle_mouse(&mut self, event: &MouseEvent, editor: &mut Editor) -> bool {
        let area = self.area;
        if event.row < area.y || event.row >= area.y + self.height() as u16 {
            return false;
        }
        self.lay_out(editor);
        match event.kind {
            MouseEventKind::ScrollDown => self.scroll(editor, 3),
            MouseEventKind::ScrollUp => self.scroll(editor, -3),
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                let line = self.top + (event.row - area.y) as usize;
                if let Some(block) = self.code_block_of(line) {
                    let step = if event.kind == MouseEventKind::ScrollLeft {
                        -4
                    } else {
                        4
                    };
                    self.shift_block(block, step);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let line = self.top + (event.row - area.y) as usize;
                if line < self.len() {
                    self.reading = line;
                    let columns = self.page_columns(editor);
                    let page_line = &self.laid.as_ref().unwrap().page.lines[line];
                    let column = event.column.saturating_sub(columns.line_left(page_line)) as usize;
                    let links = page_line.links();
                    self.selected = links
                        .iter()
                        .position(|(start, end, _)| (*start..*end).contains(&column))
                        .map(|i| (line, i));
                }
            }
            _ => {}
        }
        true
    }

    /// The code block the page's line `line` is in, if it scrolls.
    fn code_block_of(&self, line: usize) -> Option<usize> {
        let page = &self.laid.as_ref()?.page;
        let block = page.lines.get(line)?.code?;
        page.code_blocks[block].scrolls().then_some(block)
    }

    /// How far the code block `block` is scrolled.
    pub fn offset(&self, block: usize) -> usize {
        self.offsets.get(&block).copied().unwrap_or(0)
    }

    /// Scrolls the code block `block` by `delta` columns, within its lines.
    fn shift_block(&mut self, block: usize, delta: isize) {
        let Some(max) = self
            .laid
            .as_ref()
            .and_then(|laid| laid.page.code_blocks.get(block))
            .map(page::CodeBlock::max_offset)
        else {
            return;
        };
        let offset = self.offset(block).saturating_add_signed(delta).min(max);
        if offset == 0 {
            self.offsets.remove(&block);
        } else {
            self.offsets.insert(block, offset);
        }
    }

    /// `h`/`l`, `H`/`L`, `gh`/`gl`: scrolls the code block of the reading line sideways (only
    /// that block: the rest of the page stays). Nothing outside a code block whose lines are cut.
    fn scroll_code(&mut self, way: Sideways, count: usize) {
        let Some(block) = self.code_block_of(self.reading) else {
            return;
        };
        let code = &self.laid.as_ref().unwrap().page.code_blocks[block];
        let half = (code.visible / 2).max(1) as isize;
        let count = count.max(1) as isize;
        let delta = match way {
            Sideways::Left => -count,
            Sideways::Right => count,
            Sideways::HalfLeft => -half * count,
            Sideways::HalfRight => half * count,
            Sideways::Start => isize::MIN / 2,
            Sideways::End => isize::MAX / 2,
        };
        self.shift_block(block, delta);
    }

    /// Scrolls the code block of `line` so that the columns `start..end` of the page show.
    fn reveal(&mut self, line: usize, start: usize, end: usize) {
        let Some(block) = self.code_block_of(line) else {
            return;
        };
        let code = &self.laid.as_ref().unwrap().page.code_blocks[block];
        let (column, visible) = (code.column, code.visible);
        let offset = self.offset(block);
        let (start, end) = (start.saturating_sub(column), end.saturating_sub(column));
        if start < offset {
            self.shift_block(block, start as isize - offset as isize);
        } else if end > offset + visible {
            self.shift_block(block, (end - visible) as isize - offset as isize);
        }
    }

    /// `]`/`[`: the next or previous heading, at the top of the screen.
    fn heading(&mut self, count: usize, forward: bool) {
        let page = &self.laid.as_ref().unwrap().page;
        let lines: Vec<usize> = page.headings.iter().map(|heading| heading.line).collect();
        let target = if forward {
            lines
                .iter()
                .filter(|&&line| line > self.reading)
                .nth(count - 1)
        } else {
            lines
                .iter()
                .rev()
                .filter(|&&line| line < self.reading)
                .nth(count - 1)
        };
        if let Some(&line) = target {
            self.go_to_line(line);
        }
    }

    /// `t`: the outline, a picker of the headings.
    fn outline(&mut self, cx: &mut commands::Context) {
        let page = &self.laid.as_ref().unwrap().page;
        if page.headings.is_empty() {
            cx.editor.set_error("No headings");
            return;
        }
        let current = page
            .headings
            .iter()
            .rposition(|heading| heading.line <= self.reading)
            .unwrap_or(0) as u32;
        let columns = [PickerColumn::new(
            "heading",
            |heading: &page::Heading, _| {
                format!("{}{}", "  ".repeat(heading.level - 1), heading.text).into()
            },
        )];
        let picker = Picker::new(
            columns,
            0,
            page.headings.clone(),
            (),
            |cx, heading: &page::Heading, _action| {
                let line = heading.line;
                cx.jobs.callback(async move {
                    let call: crate::job::Callback = crate::job::Callback::EditorCompositor(
                        Box::new(move |editor: &mut Editor, _: &mut Compositor| {
                            if let Some(markdown) = markdown_of(editor) {
                                markdown.go_to_line(line);
                            }
                        }),
                    );
                    Ok(call)
                });
            },
        )
        .without_jumps()
        .with_initial_cursor(current);
        cx.callback.push(Box::new(move |compositor, _| {
            compositor.push(Box::new(overlaid(picker)))
        }));
    }

    /// `Tab`/`S-Tab`: the next or previous link from the reading line, selected.
    fn next_link(&mut self, forward: bool) {
        let page = &self.laid.as_ref().unwrap().page;
        let all: Vec<(usize, usize)> = page
            .lines
            .iter()
            .enumerate()
            .flat_map(|(line, l)| (0..l.links().len()).map(move |i| (line, i)))
            .collect();
        if all.is_empty() {
            return;
        }
        let here = self.selected.filter(|(line, _)| *line == self.reading);
        let next = if forward {
            match here {
                Some(at) => all.iter().find(|&&link| link > at),
                None => all.iter().find(|&&(line, _)| line >= self.reading),
            }
            .or(all.first())
        } else {
            match here {
                Some(at) => all.iter().rev().find(|&&link| link < at),
                None => all.iter().rev().find(|&&(line, _)| line < self.reading),
            }
            .or(all.last())
        };
        if let Some(&(line, i)) = next {
            self.reading = line;
            self.selected = Some((line, i));
            self.keep_visible();
        }
    }

    /// `Enter`: follows the selected link, or the reading line's first.
    fn follow(&mut self, cx: &mut commands::Context) {
        let page = &self.laid.as_ref().unwrap().page;
        let links = page.lines[self.reading].links();
        let index = match self.selected {
            Some((line, i)) if line == self.reading => links.get(i),
            _ => links.first(),
        };
        let Some(&(_, _, link)) = index else {
            cx.editor.set_error("No link on this line");
            return;
        };
        let target = page.links[link].clone();
        if let Some(line) = page.internal_target(&target) {
            self.go_to_line(line);
            return;
        }
        let Target::Url(url) = target else {
            return;
        };
        if url.starts_with('#') {
            cx.editor.set_error(format!("No heading {url}"));
            return;
        }
        if is_web(&url) {
            // never opened: no network
            crate::yank_place::yank(cx.editor, vec![url]);
            return;
        }
        let base = cx
            .editor
            .document(self.doc)
            .and_then(|doc| doc.path())
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(vortex_stdx::env::current_working_dir);
        let (path, fragment) = local_target(&url, &base);
        if !path.is_file() {
            cx.editor.set_error(format!("No file {}", path.display()));
            return;
        }
        cx.callback.push(Box::new(move |compositor, cx| {
            open_link(compositor, cx.editor, path, fragment);
        }));
    }

    /// `/`: a search of the page's text (a regex, case-insensitive unless it has capitals).
    fn search_prompt(&mut self, cx: &mut commands::Context) {
        {
            let mut search = self.search.borrow_mut();
            search.previous = search.query.clone();
            search.from = self.reading;
        }
        let shared = self.search.clone();
        let prompt = ui::Prompt::new(
            "search:".into(),
            None,
            |_editor, _input| Vec::new(),
            move |cx, input: &str, event| {
                let mut search = shared.borrow_mut();
                match event {
                    PromptEvent::Update | PromptEvent::Validate => {
                        search.query = input.to_string();
                        search.changed = true;
                        if event == PromptEvent::Validate && compile(input).is_err() {
                            cx.editor.set_error("Invalid regex");
                        }
                    }
                    PromptEvent::Abort => {
                        search.query = search.previous.clone();
                        search.changed = true;
                        search.aborted = true;
                    }
                }
            },
        );
        cx.callback.push(Box::new(move |compositor, _| {
            compositor.push(Box::new(prompt))
        }));
    }

    /// Applies what the search prompt changed: the matches, and the reading line on the first
    /// one from where the search started.
    fn apply_search(&mut self) {
        let (changed, aborted, from) = {
            let mut search = self.search.borrow_mut();
            let state = (search.changed, search.aborted, search.from);
            search.changed = false;
            search.aborted = false;
            state
        };
        if !changed {
            return;
        }
        let query = self.search.borrow().query.clone();
        self.regex = (!query.is_empty()).then(|| compile(&query).ok()).flatten();
        self.find_matches();
        // no match (anymore): back where the search started, as `Esc` does
        if aborted || self.matches.is_empty() {
            self.reading = from.min(self.len() - 1);
        } else if let Some(&(line, _, _)) = self
            .matches
            .iter()
            .find(|(line, _, _)| *line >= from)
            .or(self.matches.first())
        {
            self.reading = line;
            self.reveal_match();
        }
        self.selected = None;
        self.keep_visible();
    }

    /// Scrolls the reading line's code block to its first match, if it is out of sight.
    fn reveal_match(&mut self) {
        if let Some(&(line, start, end)) = self
            .matches
            .iter()
            .find(|(line, _, _)| *line == self.reading)
        {
            self.reveal(line, start, end);
        }
    }

    fn find_matches(&mut self) {
        self.matches.clear();
        let (Some(regex), Some(laid)) = (&self.regex, &self.laid) else {
            return;
        };
        for (i, line) in laid.page.lines.iter().enumerate() {
            let text = line.text();
            for found in regex.find_iter(&text) {
                if found.as_str().is_empty() {
                    continue;
                }
                let start = text[..found.start()].width();
                let end = start + found.as_str().width();
                self.matches.push((i, start, end));
            }
        }
    }

    /// `n`/`N`: the next or previous line with a match.
    fn next_match(&mut self, editor: &mut Editor, forward: bool) {
        if self.regex.is_none() {
            editor.set_error("No search");
            return;
        }
        let lines: Vec<usize> = self.matches.iter().map(|(line, _, _)| *line).collect();
        let next = if forward {
            lines
                .iter()
                .find(|&&line| line > self.reading)
                .or(lines.first())
        } else {
            lines
                .iter()
                .rev()
                .find(|&&line| line < self.reading)
                .or(lines.last())
        };
        match next {
            Some(&line) => {
                let wrapped =
                    (forward && line <= self.reading) || (!forward && line >= self.reading);
                self.reading = line;
                self.reveal_match();
                self.keep_visible();
                if wrapped {
                    editor.set_status("Wrapped around the page");
                }
            }
            None => editor.set_error("No matches"),
        }
    }

    /// Puts the source's cursor on the line under the reading line (on its first character that
    /// isn't a space) and its view's top at the line under the page's top, unless that was
    /// already done for this reading line (so that `x` then `d` delete the line).
    pub fn sync_source(&mut self, editor: &mut Editor, force: bool) {
        self.lay_out(editor);
        if !force && self.synced == Some(self.reading) {
            return;
        }
        let Some(laid) = &self.laid else {
            return;
        };
        let line_of = |i: usize| laid.page.lines.get(i).map_or(0, |line| line.source);
        let (reading, top) = (line_of(self.reading), line_of(self.top));
        let Some(doc) = editor.documents.get_mut(&self.doc) else {
            return;
        };
        let (cursor, anchor) = {
            let text = doc.text().slice(..);
            let last = text.len_lines().saturating_sub(1);
            let (reading, top) = (reading.min(last), top.min(last).min(reading.min(last)));
            let start = text.line_to_char(reading);
            let first = text
                .line(reading)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .count();
            let line_end = start + text.line(reading).len_chars();
            let cursor = (start + first).min(line_end.saturating_sub(1).max(start));
            (cursor, text.line_to_char(top))
        };
        doc.set_selection(self.view, Selection::point(cursor));
        doc.set_view_offset(
            self.view,
            ViewPosition {
                anchor,
                horizontal_offset: 0,
                vertical_offset: 0,
            },
        );
        self.synced = Some(self.reading);
    }

    /// Draws the page over `area` (its window), its last row the status row.
    pub fn render(&mut self, editor: &mut Editor, area: Rect, surface: &mut Surface, focused: bool) {
        self.area = area;
        self.lay_out(editor);
        self.apply_search();
        self.clamp();
        let theme = &editor.theme;
        let background = theme.get("ui.background");
        surface.clear_with(area, background);
        let columns = self.page_columns(editor);
        let reading_style = theme.get_or("ui.markdown.reading", "ui.cursorline.primary");
        let match_style = theme.get_or("ui.markdown.match", "ui.selection");
        let link_style = theme.get_or("ui.markdown.link.selected", "ui.selection.primary");
        let border = theme.get_or("ui.markdown.border", "ui.virtual");
        let right = columns.inner_right.min(area.right());
        let prose_right = (columns.left + columns.widths.prose as u16).min(right);
        let page = &self.laid.as_ref().unwrap().page;
        for row in 0..self.height() {
            let index = self.top + row;
            let Some(line) = page.lines.get(index) else {
                break;
            };
            let y = area.y + row as u16;
            // a table or a code block wider than the prose breaks out to the left
            let left = columns.line_left(line);
            if index == self.reading {
                let from = left.min(columns.left);
                let to = (left + line.extent() as u16).clamp(prose_right, right);
                surface.set_style(Rect::new(from, y, to - from, 1), reading_style);
                if from > area.x {
                    surface.set_string(from - 1, y, "│", border);
                }
            }
            if let Some((start, end, style)) = line.fill {
                let x = (left + start as u16).min(right);
                let to = (left + end as u16).min(right);
                if x < to {
                    let fill = Style {
                        bg: style.bg,
                        ..Default::default()
                    };
                    surface.set_style(Rect::new(x, y, to - x, 1), fill);
                }
            }
            let shift = line
                .code
                .and_then(|block| Some((page.code_blocks.get(block)?, block)))
                .filter(|(code, _)| code.scrolls())
                .map(|(code, block)| Shift {
                    column: code.column,
                    offset: self.offsets.get(&block).copied().unwrap_or(0),
                    visible: code.visible,
                });
            match shift {
                None => {
                    let mut x = left;
                    for piece in &line.pieces {
                        if x >= right {
                            break;
                        }
                        let (end, _) = surface.set_stringn(
                            x,
                            y,
                            &piece.text,
                            (right - x) as usize,
                            piece.style,
                        );
                        x = end;
                    }
                }
                Some(shift) => draw_code_line(surface, line, left, right, y, shift, border),
            }
            let column = |c: usize| shift.map_or(c, |shift| shift.screen(c));
            for &(_, start, end) in self.matches.iter().filter(|(line, _, _)| *line == index) {
                highlight(
                    surface,
                    left,
                    right,
                    y,
                    column(start),
                    column(end),
                    match_style,
                );
            }
            if let Some((line_index, i)) = self.selected {
                if line_index == index {
                    if let Some(&(start, end, _)) = line.links().get(i) {
                        highlight(surface, left, right, y, start, end, link_style);
                    }
                }
            }
        }
        self.render_status(editor, area, surface, focused);
    }

    fn render_status(&self, editor: &Editor, area: Rect, surface: &mut Surface, focused: bool) {
        if area.height < 2 {
            return;
        }
        let theme = &editor.theme;
        let y = area.bottom() - 1;
        let base = if focused {
            theme.get("ui.statusline")
        } else {
            theme.get("ui.statusline.inactive")
        };
        surface.set_style(Rect::new(area.x, y, area.width, 1), base);
        let badge = if focused {
            theme.get_or("ui.statusline.normal", "ui.statusline")
        } else {
            base
        };
        let (x, _) = surface.set_stringn(area.x, y, " RENDER ", area.width as usize, badge);
        let doc = editor.document(self.doc);
        let name = doc
            .map(|doc| doc.display_name().into_owned())
            .unwrap_or_default();
        let modified = doc.is_some_and(|doc| doc.is_modified());
        let left = format!(" {name}{}", if modified { " [+]" } else { "" });
        surface.set_stringn(x, y, &left, area.right().saturating_sub(x) as usize, base);
        let page = &self.laid.as_ref().unwrap().page;
        let section = page
            .section(self.reading)
            .map(|heading| format!("{}  ", heading.text))
            .unwrap_or_default();
        let position = format!("{}/{} ", self.reading + 1, page.lines.len().max(1));
        let right = format!("{section}{position}");
        let width = right.width() as u16;
        let min_x = x + left.width() as u16 + 2;
        let start = area.right().saturating_sub(width).max(min_x);
        if start < area.right() {
            surface.set_stringn(start, y, &right, (area.right() - start) as usize, base);
        }
    }
}

/// Which way `h`, `l`, `H`, `L`, `gh` and `gl` scroll a code block.
#[derive(Debug, Clone, Copy)]
enum Sideways {
    Left,
    Right,
    HalfLeft,
    HalfRight,
    Start,
    End,
}

/// How a line of a code block that scrolls is drawn: its code from `column`, `offset` columns
/// of it scrolled out on the left, `visible` columns shown.
#[derive(Debug, Clone, Copy)]
struct Shift {
    column: usize,
    offset: usize,
    visible: usize,
}

impl Shift {
    /// Where the page's column `c` of the line is drawn, clamped to what shows.
    fn screen(&self, c: usize) -> usize {
        if c < self.column {
            c
        } else {
            self.column
                + (c - self.column)
                    .saturating_sub(self.offset)
                    .min(self.visible)
        }
    }
}

/// Draws a line of a code block that scrolls: what is before its code as it is, its code
/// scrolled, `<` in the padding on its left when some of it is scrolled out there and `>` in the
/// last cell when more is on the right.
fn draw_code_line(
    surface: &mut Surface,
    line: &page::Line,
    left: u16,
    right: u16,
    y: u16,
    shift: Shift,
    marker: Style,
) {
    use vortex_core::unicode::segmentation::UnicodeSegmentation;
    let Shift {
        column,
        offset,
        visible,
    } = shift;
    let mut c = 0;
    let mut code_width = 0;
    for piece in &line.pieces {
        for grapheme in piece.text.graphemes(true) {
            let w = grapheme.width();
            let at = if c < column {
                Some(c)
            } else {
                let inner = c - column;
                code_width = inner + w;
                (inner >= offset && inner + w <= offset + visible).then(|| column + inner - offset)
            };
            if let Some(at) = at {
                let x = left as usize + at;
                if x + w <= right as usize {
                    surface.set_string(x as u16, y, grapheme, piece.style);
                }
            }
            c += w;
        }
    }
    let marker_at = |at: usize, text: &str, surface: &mut Surface| {
        let x = left as usize + at;
        if x < right as usize {
            surface.set_string(x as u16, y, text, marker);
        }
    };
    if offset > 0 && code_width > 0 {
        marker_at(column - 1, "<", surface);
    }
    if code_width > offset + visible {
        marker_at(column + visible, ">", surface);
    }
}

/// Patches `style` over the columns `start..end` of the page on row `y`.
fn highlight(
    surface: &mut Surface,
    left: u16,
    right: u16,
    y: u16,
    start: usize,
    end: usize,
    style: Style,
) {
    let x = (left as usize + start).min(right as usize) as u16;
    let to = (left as usize + end).min(right as usize) as u16;
    if to > x {
        surface.set_style(Rect::new(x, y, to - x, 1), style);
    }
}

fn compile(query: &str) -> Result<Regex, vortex_core::regex::Error> {
    let insensitive = !query.chars().any(char::is_uppercase);
    RegexBuilder::new(query)
        .case_insensitive(insensitive)
        .build()
}

/// Whether a link goes to the web (or anywhere with a scheme but `file:`): yanked, never opened.
fn is_web(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        // a Windows drive is not a scheme (and `vx` doesn't run there anyway)
        && scheme.len() > 1
        && !scheme.eq_ignore_ascii_case("file")
}

/// A local link's file (relative to `base`, the page's folder) and its `#fragment`.
fn local_target(url: &str, base: &Path) -> (PathBuf, Option<String>) {
    let url = url.strip_prefix("file://").unwrap_or(url);
    let (path, fragment) = match url.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment.to_string())),
        None => (url, None),
    };
    let path = PathBuf::from(page::percent_decode(path));
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    (vortex_stdx::path::normalize(path), fragment)
}

/// The line a GitHub-style `#L12` or `#L12-L20` fragment points to (from 1).
fn fragment_line(fragment: &str) -> Option<usize> {
    let first = fragment.split('-').next()?;
    first.strip_prefix('L')?.parse().ok().filter(|&n| n > 0)
}

/// Opens a local link's file: a Markdown file rendered (at its `#heading`), any other file in
/// the editor (at its `#L12` line). The page it leaves is a place history comes back to.
fn open_link(
    _compositor: &mut Compositor,
    editor: &mut Editor,
    path: PathBuf,
    fragment: Option<String>,
) {
    let opened = editor.teleport(Jump::Switch, |editor| editor.open(&path, Action::Replace));
    if let Err(err) = opened {
        editor.set_error(format!("Failed to open {}: {err}", path.display()));
        return;
    }
    let markdown = is_markdown(vortex_view::doc!(editor));
    if markdown {
        let start = match fragment {
            Some(anchor) if !anchor.is_empty() => Start::Anchor(anchor),
            _ => Start::Source,
        };
        open(editor, start);
    } else if let Some(line) = fragment.as_deref().and_then(fragment_line) {
        let (view, doc) = vortex_view::current!(editor);
        let text = doc.text().slice(..);
        let line = (line - 1).min(text.len_lines().saturating_sub(1));
        doc.set_selection(view.id, Selection::point(text.line_to_char(line)));
        vortex_view::align_view(doc, view, vortex_view::Align::Center);
    }
}

/// The render mode up in the editor, if any.
pub fn markdown_of(editor: &mut Editor) -> Option<&mut MarkdownView> {
    let screen = editor.tree.focused_screen()?;
    editor.screens.state_mut::<MarkdownView>(screen)
}

/// The render mode of the focused text window's buffer (this fork): its page, a screen over the
/// window (`Esc` and `V` show the source again), at `start`. Says why not when it can't.
pub fn open(editor: &mut Editor, start: Start) -> bool {
    match MarkdownView::open(editor, start) {
        Ok(markdown) => {
            let name = editor
                .document(markdown.doc)
                .map(|doc| format!("{}, rendered", doc.display_name()))
                .unwrap_or_default();
            let source = markdown.doc;
            let screen = editor.add_screen(
                Screen::new(ScreenKind::Markdown, name)
                    .with_source(Some(source))
                    .with_state(markdown),
            );
            editor.open_screen(screen, Placement::Cover);
            true
        }
        Err(err) => {
            editor.set_error(err);
            false
        }
    }
}

/// Back to the source from the focused page: `sync` puts its cursor at the reading line and its
/// top at the page's top (the toggle, `Esc`), not when history shows another place. History
/// keeps the page, `ga` shows it again.
pub fn close(editor: &mut Editor, sync: bool) {
    let view = editor.tree.focus;
    let Some(screen) = editor.tree.focused_screen() else {
        return;
    };
    if editor.screens.kind(screen) != Some(ScreenKind::Markdown) {
        return;
    }
    if sync {
        if let Some(mut state) = editor.screens.take_state(screen) {
            if let Some(markdown) = state.downcast_mut::<MarkdownView>() {
                if markdown.covers(editor) {
                    markdown.sync_source(editor, true);
                }
            }
            editor.screens.put_state(screen, state);
        }
    }
    crate::screens::close(editor, view);
}

/// A command of the keymap on the focused page (this fork): the page runs its own version (or
/// goes, for `V`), or the command runs: a command about windows or the editor with the page
/// staying, any other on the source at the reading line, the page gone. Returns whether the
/// command is to run.
pub fn through(command: &MappableCommand, cx: &mut commands::Context) -> bool {
    let Some(screen) = cx.editor.tree.focused_screen() else {
        return true;
    };
    if cx.editor.screens.kind(screen) != Some(ScreenKind::Markdown) {
        return true;
    }
    let Some(mut state) = cx.editor.screens.take_state(screen) else {
        return true;
    };
    let result = state
        .downcast_mut::<MarkdownView>()
        .map(|markdown| markdown.command(command, cx));
    cx.editor.screens.put_state(screen, state);
    match result {
        Some(CommandResult::Taken) => false,
        Some(CommandResult::Close) => {
            close(cx.editor, true);
            false
        }
        Some(CommandResult::Run) => {
            if !crate::screens::allows(cx.editor, command.name()) {
                // a text command: on the source, synced to the reading line
                close(cx.editor, false);
            }
            true
        }
        None => true,
    }
}

/// `V`, `:markdown-view` (this fork): the render mode of the focused Markdown buffer, or back to
/// its source.
pub fn toggle(editor: &mut Editor) {
    if markdown_of(editor).is_some() {
        close(editor, true);
    } else {
        open(editor, Start::Source);
    }
}

/// The keys of the render mode, for `?`.
pub fn help() -> KeyHelp {
    KeyHelp::new(
        "Markdown",
        &[
            (
                "read",
                vec![
                    ("j / k", "the reading line down / up"),
                    ("C-d / C-u", "half a page down / up"),
                    ("gg / ge", "the top / the end"),
                    ("] / [", "next / previous heading"),
                    ("t", "the outline: jump to a heading"),
                    ("/  n / N", "search the page, next / previous"),
                ],
            ),
            (
                "code blocks",
                vec![
                    ("h / l", "scroll the reading line's block sideways"),
                    ("H / L", "half its width"),
                    ("gh / gl", "its start / its end"),
                ],
            ),
            (
                "links",
                vec![
                    ("Tab / S-Tab", "next / previous link"),
                    (
                        "Enter",
                        "follow: a file opens, #heading jumps, a web link is yanked",
                    ),
                ],
            ),
            (
                "edit",
                vec![
                    ("V / Esc", "back to the source, at the reading line"),
                    ("i a o c ...", "edit the source at the reading line"),
                    ("ga / gA", "history"),
                ],
            ),
        ],
    )
}

/// The positions the render mode had in each buffer, for history to show it there again.
pub type Positions = HashMap<DocumentId, (usize, usize)>;
