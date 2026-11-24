//! Windows that aren't text documents (this fork): a window of the tree like any other, whose
//! document is a *screen document* that draws and drives something else (a terminal now; the
//! hex view, the diff, the Markdown page and the debug panel can become screens the same way).
//!
//! The mechanism is the editor's:
//!
//! - A screen is a [`Document`](crate::Document) with [`Screen`] set: it has a name (the buffer
//!   picker, the statusline), no path, no gutter, no soft wrap, and its text can't be edited
//!   (transactions are refused); its owner may put text there for its own use with
//!   [`Document::set_screen_text`](crate::Document::set_screen_text) (a terminal's view mode).
//! - A screen is shown in one window at a time. Switching to its buffer (the buffer picker) goes
//!   to its window, or puts it back where it was if it is hidden; opening a file from a screen's
//!   window opens it in the window for documents ([`Editor::document_view`]) and focuses that.
//! - A screen's window can leave the tree and come back where it was ([`Editor::hide_screen`],
//!   [`Editor::show_screen`]), the document staying open meanwhile.
//! - Things that move through files on their own (watch mode following, the debugger's stops)
//!   run in the window for documents without taking the focus from a screen
//!   ([`Editor::in_document_window`]).
//!
//! `vortex-term`'s `screens` module draws a screen and routes keys and the mouse to it, by kind.

use crate::{
    editor::Action,
    tree::{Layout, Place},
    DocumentId, Editor, View, ViewId,
};

/// What a screen document shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKind {
    /// A shell (`crate::terminal`).
    Terminal,
}

/// A screen document's screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub kind: ScreenKind,
    /// Its name in the buffer picker and the statusline.
    pub name: String,
}

/// Where a screen's window goes when it is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Along an edge of the whole editor, taking `share` percent of it: `Horizontal` at the
    /// bottom (`at_end`) or the top, `Vertical` at the right or the left.
    Edge {
        layout: Layout,
        at_end: bool,
        share: u16,
    },
    /// A split of the focused window.
    Split(Layout),
    /// Where it was ([`crate::tree::Tree::place`]), else `fallback`.
    Back { place: Place, fallback: Layout },
}

impl Editor {
    /// Whether `view` shows a screen.
    pub fn is_screen_view(&self, view: ViewId) -> bool {
        self.tree
            .try_get(view)
            .and_then(|view| self.document(view.doc))
            .is_some_and(|doc| doc.is_screen())
    }

    /// The window that shows `doc`, if any.
    pub fn view_of(&self, doc: DocumentId) -> Option<ViewId> {
        self.tree
            .views()
            .find(|(view, _)| view.doc == doc)
            .map(|(view, _)| view.id)
    }

    /// The window for documents: the focused one when it shows a text document, else the one
    /// that last did, else any that does.
    pub fn document_view(&self) -> Option<ViewId> {
        let focus = self.tree.focus;
        if self.tree.contains(focus) && !self.is_screen_view(focus) {
            return Some(focus);
        }
        self.last_document_view
            .filter(|&view| self.tree.contains(view) && !self.is_screen_view(view))
            .or_else(|| {
                self.tree
                    .traverse()
                    .map(|(id, _)| id)
                    .find(|&id| !self.is_screen_view(id))
            })
    }

    /// Runs `f` with the focus in the window for documents, and gives the focus back to the
    /// screen that had it: watch mode follows and the debugger stops in the window next to the
    /// terminal you type in. With no window for documents, `f` runs in a new split of the screen.
    /// The editor's mode is kept.
    pub fn in_document_window<R>(&mut self, f: impl FnOnce(&mut Editor) -> R) -> R {
        let focus = self.tree.focus;
        if !self.is_screen_view(focus) {
            return f(self);
        }
        let mode = self.mode;
        match self.document_view() {
            Some(view) => self.tree.focus = view,
            None => {
                self.new_file(Action::VerticalSplit);
            }
        }
        let result = f(self);
        if self.tree.contains(focus) && self.tree.focus != focus {
            self.note_document_view();
            self.tree.focus = focus;
        }
        self.mode = mode;
        result
    }

    /// Remembers the focused window as the window for documents, if it shows one.
    pub(crate) fn note_document_view(&mut self) {
        let focus = self.tree.focus;
        if self.tree.contains(focus) && !self.is_screen_view(focus) {
            self.last_document_view = Some(focus);
        }
    }

    /// Opens a window for the screen `doc` at `placement`, and focuses it. A screen shown
    /// already is only focused.
    pub fn open_screen_window(&mut self, doc: DocumentId, placement: Placement) -> ViewId {
        if let Some(view) = self.view_of(doc) {
            self.focus(view);
            return view;
        }
        self.enter_normal_mode();
        self.note_document_view();
        let from = self.tree.focus;
        let view = View::new(doc, self.config().gutters.clone());
        let id = match placement {
            Placement::Edge {
                layout,
                at_end,
                share,
            } => self.tree.split_root(view, layout, at_end, Some(share)),
            Placement::Split(layout) if self.tree.contains(from) => self.tree.split(view, layout),
            Placement::Split(layout) => self.tree.split_root(view, layout, true, None),
            Placement::Back { place, fallback } => match self.tree.insert_at(view, &place) {
                Some(id) => id,
                None => {
                    let view = View::new(doc, self.config().gutters.clone());
                    self.tree.split_root(view, fallback, true, place.share)
                }
            },
        };
        self.documents
            .get_mut(&doc)
            .expect("a screen's document")
            .ensure_view_init(id);
        self.screen_focused(from);
        self._refresh();
        id
    }

    /// Shows the screen `doc`: focuses its window, or puts it back where it was if it is hidden
    /// (the scratch terminal), or where it goes the first time.
    pub fn show_screen(&mut self, doc: DocumentId) -> Option<ViewId> {
        if let Some(view) = self.view_of(doc) {
            self.focus(view);
            return Some(view);
        }
        let placement = match self.document(doc)?.screen_kind()? {
            ScreenKind::Terminal => self.terminal_placement(doc),
        };
        Some(self.open_screen_window(doc, placement))
    }

    /// Takes the screen's window `view` out of the tree, keeping its document, and gives back
    /// where it was, to [`Self::open_screen_window`] it there again. The focus goes to the window
    /// for documents.
    pub fn hide_screen(&mut self, view: ViewId) -> Place {
        let place = self.tree.place(view);
        if self.tree.focus == view {
            if let Some(next) = self.document_view() {
                self.focus(next);
            }
        }
        for doc in self.documents.values_mut() {
            doc.remove_view(view);
        }
        let focused = self.tree.focus == view;
        self.tree.remove(view);
        if focused {
            self.screen_focused(view);
        }
        self._refresh();
        place
    }

    /// The focus moved from `from` (`Editor::focus` and the screen's windows): entering a
    /// screen lands in its insert mode (a terminal types), leaving one tells it.
    pub(crate) fn screen_focused(&mut self, from: ViewId) {
        let to = self.tree.focus;
        if from == to {
            return;
        }
        let doc_of = |editor: &Editor, view: ViewId| editor.tree.try_get(view).map(|v| v.doc);
        if let Some(doc) = doc_of(self, from) {
            if let Some(terminal) = self.terminals.get_mut(doc) {
                terminal.focus_out();
            }
        }
        self.note_document_view();
        if let Some(doc) = doc_of(self, to) {
            if let Some(terminal) = self.terminals.get_mut(doc) {
                terminal.snapshot = None;
                terminal.focus_in();
                self.mode = crate::document::Mode::Insert;
            }
        }
    }
}
