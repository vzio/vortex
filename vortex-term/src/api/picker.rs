//! `:api-browse` (this fork): a plain picker over the API browser's tree, to try the data layer.
//! It lists one level at a time: the four roots, then the children of the node opened. The
//! preview shows the selected item's path, signature and docs; the prompt row the level's state
//! (how many items, which provider, how long it took).
//!
//! Like the file tree it uses the modes of the picker: in normal mode the letters are keys, `s`
//! filters the level.

use std::sync::Arc;

use parking_lot::Mutex;
use tui::buffer::Buffer as Surface;
use tui::text::Text;
use tui::widgets::{Cell, Paragraph, Widget, Wrap};
use vortex_core::Position;
use vortex_view::editor::Action;
use vortex_view::graphics::{CursorKind, Rect};
use vortex_view::input::KeyEvent;
use vortex_view::Editor;

use super::{Browser, Kind, NodeId, Place, State};
use crate::compositor::{self, Component, Compositor, Context, Event, EventResult};
use crate::job::Callback;
use crate::ui::editor::canonicalize_key;
use crate::ui::overlay::{overlaid, Overlay};
use crate::ui::picker::PickerMode;
use crate::ui::{Markdown, Picker, PickerColumn};
use crate::{ctrl, key, shift};

pub const ID: &str = "api-browser";

fn key_help() -> crate::ui::keyhelp::KeyHelp {
    crate::ui::keyhelp::KeyHelp::new(
        "API browser",
        &[
            (
                "tree",
                vec![
                    ("l →", "open"),
                    ("h ← Backspace", "back"),
                    ("Enter", "go to the definition (or open)"),
                ],
            ),
            (
                "list",
                vec![
                    ("j k", "move"),
                    ("s", "filter"),
                    ("Esc q", "close"),
                    ("?", "these keys"),
                ],
            ),
        ],
    )
}

/// A row: one node of the level shown.
pub struct Row {
    id: NodeId,
    kind: &'static str,
    name: String,
    detail: String,
}

/// What the preview shows, kept up to date by the browser view.
#[derive(Default)]
pub struct Preview {
    id: Option<NodeId>,
    text: String,
}

type Shared = Arc<Mutex<Preview>>;

pub struct ApiPicker {
    picker: Picker<Row, Shared>,
    browser: Browser,
    preview: Shared,
    /// The node whose children are shown; `None` for the roots.
    level: Option<NodeId>,
    /// What the rows were built from: the level, and its children's count and skeleton flag.
    shown: Option<(Option<NodeId>, usize, bool)>,
    /// The selection the preview was made for, and whether its docs were ready.
    previewed: Option<(NodeId, bool)>,
    /// The node to jump to once its definition is found (`Enter`).
    jump_when_found: Option<NodeId>,
}

/// Opens the browser of the current buffer's language server, or says why not.
pub fn open(editor: &mut Editor, compositor: &mut Compositor) {
    match Browser::new(editor) {
        Ok(browser) => compositor.push(Box::new(overlaid(ApiPicker::new(browser)))),
        Err(err) => editor.set_error(format!("API browser: {err}")),
    }
}

impl ApiPicker {
    pub fn new(browser: Browser) -> Self {
        let preview = Shared::default();
        let columns = [
            PickerColumn::new("kind", |row: &Row, _: &Shared| Cell::from(row.kind)),
            PickerColumn::new("name", |row: &Row, _: &Shared| {
                Cell::from(row.name.as_str())
            }),
            PickerColumn::new("detail", |row: &Row, _: &Shared| {
                Cell::from(row.detail.as_str())
            })
            .without_filtering(),
        ];
        let picker = Picker::new(columns, 1, Vec::new(), preview.clone(), |_, _, _| {})
            .with_drawn_preview(|row: &Row, preview: &Shared, editor, area, surface| {
                let preview = preview.lock();
                if preview.id != Some(row.id) {
                    return;
                }
                let markdown = Markdown::new(preview.text.clone(), editor.syn_loader.clone());
                let text: Text = markdown.parse(Some(&editor.theme));
                Paragraph::new(&text)
                    .wrap(Wrap { trim: false })
                    .render(area, surface);
            })
            .with_modes()
            .truncate_start(false);
        let mut view = Self {
            picker,
            browser,
            preview,
            level: None,
            shown: None,
            previewed: None,
            jump_when_found: None,
        };
        view.refresh();
        view
    }

    /// The browser, for tests.
    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    /// The node whose children are shown (`None`: the roots), for tests.
    pub fn level(&self) -> Option<NodeId> {
        self.level
    }

    fn selected(&self) -> Option<NodeId> {
        self.picker.selection().map(|row| row.id)
    }

    fn level_ids(&self) -> (Vec<NodeId>, bool) {
        match self.level {
            None => (self.browser.roots().to_vec(), false),
            Some(id) => match self.browser.children(id) {
                State::Ready(level) => (level.children.clone(), level.skeleton),
                _ => (Vec::new(), false),
            },
        }
    }

    /// Rebuilds the rows when the level or its children changed, and the info and the preview.
    fn refresh(&mut self) {
        let (ids, skeleton) = self.level_ids();
        let shown = Some((self.level, ids.len(), skeleton));
        if self.shown != shown {
            let selected_name = self.selected().map(|id| self.browser.node(id).name.clone());
            let rows: Vec<Row> = ids
                .iter()
                .map(|&id| {
                    let node = self.browser.node(id);
                    let mut name = node.name.clone();
                    if node.has_children() {
                        name.push_str(" ›");
                    }
                    Row {
                        id,
                        kind: node.kind.label(),
                        name,
                        detail: node.detail.clone().unwrap_or_default().replace('\n', " "),
                    }
                })
                .collect();
            // Keep the selection on the same name when the symbols replace the skeleton.
            let same_level = self.shown.is_some_and(|(level, ..)| level == self.level);
            let cursor = selected_name
                .filter(|_| same_level)
                .and_then(|name| {
                    ids.iter()
                        .position(|&id| self.browser.node(id).name == name)
                })
                .unwrap_or(0);
            self.picker.replace_items(rows, cursor as u32);
            self.shown = shown;
        }
        self.picker.set_info(Some(self.info()));
        self.refresh_preview();
    }

    /// The prompt row: where the level is and how it loaded.
    fn info(&self) -> String {
        let Some(level) = self.level else {
            return format!("{} · {}", self.browser.language, self.browser.server());
        };
        let mut path = self.browser.display_path(level);
        if path.is_empty() {
            path = self.browser.node(level).name.clone();
        }
        match self.browser.children(level) {
            State::NotAsked | State::Loading => format!("{path} · loading"),
            State::Failed(err) => format!("{path} · {err}"),
            State::Ready(loaded) => {
                let provider = loaded
                    .children
                    .first()
                    .map(|&id| self.browser.node(id).provider.short())
                    .unwrap_or_default();
                let mut facts = vec![path, provider.to_string()];
                if loaded.requests > 0 {
                    facts.push(format!("{} ms", loaded.took.as_millis()));
                }
                if loaded.incomplete {
                    facts.push("incomplete".to_string());
                }
                if loaded.skeleton {
                    facts.push("symbols loading".to_string());
                }
                facts.retain(|fact| !fact.is_empty());
                facts.join(" · ")
            }
        }
    }

    fn refresh_preview(&mut self) {
        let Some(id) = self.selected() else {
            *self.preview.lock() = Preview::default();
            self.previewed = None;
            return;
        };
        self.browser.ask_docs(id);
        let ready = !matches!(self.browser.docs(id), State::Loading);
        if self.previewed == Some((id, ready)) {
            return;
        }
        self.previewed = Some((id, ready));
        let text = self.preview_text(id);
        *self.preview.lock() = Preview { id: Some(id), text };
    }

    fn preview_text(&self, id: NodeId) -> String {
        let node = self.browser.node(id);
        let mut text = format!("**{}**\n\n", self.browser.display_path(id));
        let mut facts = vec![
            node.kind.label().to_string(),
            node.provider.label().to_string(),
        ];
        if let Some(public) = node.public {
            facts.push(if public { "public" } else { "private" }.to_string());
        }
        if let Some(place) = &node.place {
            if let Some(path) = place.path() {
                let path = vortex_stdx::path::get_relative_path(&path);
                facts.push(format!("{}:{}", path.display(), place.range.start.line + 1));
            }
        }
        facts.retain(|fact| !fact.is_empty());
        text.push_str(&facts.join(" · "));
        text.push_str("\n\n");
        match self.browser.docs(id) {
            State::Ready(docs) => {
                if let Some(signature) = docs.signature.as_ref().or(node.detail.as_ref()) {
                    text.push_str(&format!("```\n{signature}\n```\n\n"));
                }
                if let Some(docs_text) = &docs.text {
                    text.push_str(docs_text);
                    text.push_str("\n\n");
                }
                if !docs.source.is_empty() {
                    text.push_str(&format!("*docs: {}*\n", docs.source));
                }
            }
            State::Loading => {
                if let Some(detail) = &node.detail {
                    text.push_str(&format!("```\n{detail}\n```\n\n"));
                }
                text.push_str("*docs loading*\n");
            }
            State::Failed(err) => text.push_str(&format!("*docs: {err}*\n")),
            State::NotAsked => {}
        }
        text
    }

    /// Shows the children of the selected node.
    fn descend(&mut self, editor: &Editor) {
        let Some(id) = self.selected() else {
            return;
        };
        if !self.browser.node(id).has_children() {
            return;
        }
        self.browser.open(id);
        self.level = Some(id);
        self.picker.set_query_line(String::new(), editor);
        self.refresh();
    }

    /// Goes back to the parent level, with the node we came from selected.
    fn ascend(&mut self, editor: &Editor) {
        let Some(level) = self.level else {
            return;
        };
        self.level = self.browser.parent(level);
        self.picker.set_query_line(String::new(), editor);
        self.refresh();
        let (ids, _) = self.level_ids();
        if let Some(index) = ids.iter().position(|&id| id == level) {
            self.picker.set_cursor_index(index as u32);
        }
        self.refresh();
    }

    /// `Enter`: the definition when there is one, else the children.
    fn enter(&mut self, editor: &Editor) -> EventResult {
        let Some(id) = self.selected() else {
            return EventResult::Consumed(None);
        };
        let node = self.browser.node(id);
        if node.kind == Kind::Root || node.kind == Kind::Directory {
            self.descend(editor);
            return EventResult::Consumed(None);
        }
        self.browser.ask_definition(id);
        match self.browser.definition(id) {
            State::Ready(Some(place)) => jump_callback(place.clone()),
            State::Loading => {
                self.jump_when_found = Some(id);
                EventResult::Consumed(None)
            }
            _ => {
                self.descend(editor);
                EventResult::Consumed(None)
            }
        }
    }

    fn handle_key(&mut self, mut key: KeyEvent, cx: &mut Context) -> EventResult {
        canonicalize_key(&mut key);
        if self.picker.mode() == PickerMode::Filter {
            return match key {
                key!(Enter) => self.enter(cx.editor),
                ctrl!('c') => close_callback(),
                _ => {
                    self.picker.handle_event(&Event::Key(key), cx);
                    self.refresh_preview();
                    EventResult::Consumed(None)
                }
            };
        }
        if self.picker.pending_key() {
            self.picker.handle_event(&Event::Key(key), cx);
            self.refresh_preview();
            return EventResult::Consumed(None);
        }
        match key {
            key!('j')
            | key!('k')
            | key!('g')
            | key!('G')
            | key!('s')
            | key!(Up)
            | key!(Down)
            | key!(Tab)
            | shift!(Tab)
            | ctrl!('n')
            | ctrl!('p')
            | key!(PageUp)
            | key!(PageDown)
            | ctrl!('u')
            | ctrl!('d')
            | key!(Home)
            | key!(End)
            | ctrl!('t') => {
                self.picker.handle_event(&Event::Key(key), cx);
                self.refresh_preview();
            }
            key!('l') | key!(Right) => self.descend(cx.editor),
            key!('h') | key!(Left) | key!(Backspace) => self.ascend(cx.editor),
            key!(Enter) => return self.enter(cx.editor),
            key!('?') => return EventResult::Consumed(Some(key_help().push())),
            key!(Esc) if !self.picker.query_line().is_empty() => {
                self.picker.set_query_line(String::new(), cx.editor);
            }
            key!(Esc) | key!('q') | ctrl!('c') => return close_callback(),
            _ => {}
        }
        EventResult::Consumed(None)
    }
}

impl Component for ApiPicker {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => self.handle_key(*key, cx),
            _ => {
                let result = self.picker.handle_event(event, cx);
                self.refresh_preview();
                result
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        if self.browser.update() {
            self.previewed = None;
        }
        self.refresh();
        if let Some(id) = self.jump_when_found {
            match self.browser.definition(id) {
                State::Ready(Some(place)) => {
                    self.jump_when_found = None;
                    let place = place.clone();
                    cx.jobs.callback(async move {
                        Ok(Callback::EditorCompositor(Box::new(
                            move |editor, compositor| {
                                compositor.remove(ID);
                                jump(editor, &place);
                            },
                        )))
                    });
                }
                State::Loading => {}
                _ => {
                    self.jump_when_found = None;
                    self.descend(cx.editor);
                }
            }
        }
        self.picker.render(area, surface, cx);
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        self.picker.cursor(area, editor)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.picker.required_size(viewport)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}

/// Opens the file of `place` with its name selected.
fn jump(editor: &mut Editor, place: &Place) {
    let Some(path) = place.path() else {
        editor.set_error("API browser: the definition is not in a file");
        return;
    };
    let (view, doc) = vortex_view::current!(editor);
    let from = crate::commands::jump_from(view, doc);
    editor.without_history(|editor| {
        crate::commands::lsp::jump_to_position(
            editor,
            &path,
            place.range,
            place.encoding,
            Action::Replace,
        )
    });
    editor.record_jump(vortex_view::history::Jump::Goto, from);
}

fn jump_callback(place: Place) -> EventResult {
    EventResult::Consumed(Some(Box::new(
        move |compositor: &mut Compositor, cx: &mut compositor::Context| {
            compositor.remove(ID);
            jump(cx.editor, &place);
        },
    )))
}

fn close_callback() -> EventResult {
    EventResult::Consumed(Some(Box::new(|compositor: &mut Compositor, _| {
        compositor.remove(ID);
    })))
}

/// Finds the browser view in the compositor, for tests.
pub fn find(compositor: &mut Compositor) -> Option<&mut ApiPicker> {
    compositor
        .find_id::<Overlay<ApiPicker>>(ID)
        .map(|overlay| &mut overlay.content)
}
