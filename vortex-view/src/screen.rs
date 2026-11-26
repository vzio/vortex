//! Screens (this fork): what a window shows when it isn't a text document. A window's content is
//! either a **text document** ([`crate::Document`], in `editor.documents`, shown by a text window,
//! [`crate::View`]) or a **screen** ([`Screen`], in [`Editor::screens`], shown by a screen's
//! window, [`ScreenView`]): a terminal, the hex view, a diff, a Markdown page, the debug panel.
//! Each is a type of its own with its own rules:
//!
//! - `editor.documents` holds text documents only, so whatever walks the documents (language
//!   servers, saving, the disk checks, git, watch mode, completion, search) never meets a screen;
//!   [`crate::tree::Tree::get`] and [`crate::tree::Tree::views`] give text windows only, and the
//!   macros `current!`, `view!` and `doc!` the text window commands act on
//!   ([`crate::tree::Tree::text_focus`]), so text code can't be handed a screen.
//! - A screen is drawn and driven by `vortex-term`'s `screens` module, by its [`ScreenKind`];
//!   its state is the kind's own ([`Screen::state`]): `vortex-view`'s for the bytes of the hex
//!   view, `vortex-term`'s for a diff or a page, none for a terminal (`crate::terminal`) or the
//!   debug panel (`crate::debug`), which keep theirs where they always were.
//! - A screen is shown in one window at a time, opened in a window of its own or covering the
//!   window it was opened from ([`Placement`]); closing a cover (`Esc`) shows what the window
//!   held before ([`Editor::close_screen_window`]), and history (`ga`) treats it as that window's
//!   ([`crate::history`]).
//! - A screen that isn't open costs nothing: nothing of it exists until it opens, and closing it
//!   drops its state.

use std::{any::Any, collections::BTreeMap, fmt, num::NonZeroUsize};

use crate::{
    editor::Action,
    tree::{Layout, Leaf, Place, ScreenView},
    DocumentId, Editor, View, ViewId,
};

/// A screen's id: an index in [`Screens`], never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScreenId(NonZeroUsize);

impl Default for ScreenId {
    fn default() -> Self {
        Self(NonZeroUsize::MIN)
    }
}

impl fmt::Display for ScreenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a screen is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenKind {
    /// A shell (`crate::terminal`).
    Terminal,
    /// The bytes of a file, or of a debugged program's memory (`crate::hex`).
    Hex,
    /// The side by side (or stacked) diff of two versions of a file.
    Diff,
    /// A Markdown buffer rendered as a page.
    Markdown,
    /// The debug panel (`crate::debug`): variables, watch, call stack, breakpoints, console.
    DebugPanel,
}

impl ScreenKind {
    /// What the buffer picker calls it.
    pub fn label(self) -> &'static str {
        match self {
            ScreenKind::Terminal => "terminal",
            ScreenKind::Hex => "hex",
            ScreenKind::Diff => "diff",
            ScreenKind::Markdown => "page",
            ScreenKind::DebugPanel => "debug",
        }
    }

    /// Whether another screen may cover a window of this kind (the diff of a file from its
    /// rendered page): the screens shown instead of a document may; terminals and the debug
    /// panel, windows of their own, may not (a screen opened from them goes to the window for
    /// documents).
    pub fn can_be_covered(self) -> bool {
        matches!(
            self,
            ScreenKind::Hex | ScreenKind::Diff | ScreenKind::Markdown
        )
    }
}

/// A screen: what a screen's window shows.
pub struct Screen {
    id: ScreenId,
    pub kind: ScreenKind,
    /// Its name in the buffer picker and the statusline.
    pub name: String,
    /// The kind's own state (the hex view's bytes, a diff, a page), out while the terminal UI
    /// draws or drives it (see [`Screens::take_state`]).
    state: Option<Box<dyn Any>>,
    /// The text document it was opened from, if any: a page's source, the file of a hex view or
    /// of a diff. Not a dependency: the screen lives on if the buffer closes.
    pub source: Option<DocumentId>,
    /// The place of history it was shown again from, refilled when it closes again, so that
    /// walking through history finds it there.
    pub place: Option<crate::history::Stash>,
    /// The window came to it by a jump (a page's link followed to another page): going back
    /// leaves it and its source together, for the place the jump left.
    pub arrived: bool,
}

impl Screen {
    pub fn new(kind: ScreenKind, name: impl Into<String>) -> Self {
        Self {
            id: ScreenId::default(),
            kind,
            name: name.into(),
            state: None,
            source: None,
            place: None,
            arrived: false,
        }
    }

    /// The screen with `state`, its kind's.
    pub fn with_state(mut self, state: impl Any) -> Self {
        self.state = Some(Box::new(state));
        self
    }

    pub fn with_source(mut self, source: Option<DocumentId>) -> Self {
        self.source = source;
        self
    }

    pub fn id(&self) -> ScreenId {
        self.id
    }

    /// Its state, if it is a `T`.
    pub fn state<T: Any>(&self) -> Option<&T> {
        self.state.as_ref()?.downcast_ref()
    }

    pub fn state_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.state.as_mut()?.downcast_mut()
    }
}

impl fmt::Debug for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Screen")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("source", &self.source)
            .finish()
    }
}

/// The open screens.
#[derive(Debug)]
pub struct Screens {
    list: BTreeMap<ScreenId, Screen>,
    next: NonZeroUsize,
    /// The focused screen lends the command running now to the text window it covers (a
    /// page's text commands run on its source at the reading line).
    pub lent: Option<ScreenId>,
}

impl Default for Screens {
    fn default() -> Self {
        Self {
            list: BTreeMap::new(),
            next: NonZeroUsize::MIN,
            lent: None,
        }
    }
}

impl Screens {
    /// Adds `screen`, with a new id, which it returns.
    pub fn add(&mut self, mut screen: Screen) -> ScreenId {
        let id = ScreenId(self.next);
        self.next = self.next.saturating_add(1);
        screen.id = id;
        self.list.insert(id, screen);
        id
    }

    pub fn get(&self, id: ScreenId) -> Option<&Screen> {
        self.list.get(&id)
    }

    pub fn get_mut(&mut self, id: ScreenId) -> Option<&mut Screen> {
        self.list.get_mut(&id)
    }

    /// Takes the screen out (it closes): what it took goes with it, unless the caller keeps it
    /// (history keeps a screen it can show again).
    pub fn remove(&mut self, id: ScreenId) -> Option<Screen> {
        self.list.remove(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Screen> {
        self.list.values()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Screen> {
        self.list.values_mut()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn kind(&self, id: ScreenId) -> Option<ScreenKind> {
        self.get(id).map(|screen| screen.kind)
    }

    /// The first open screen of `kind`.
    pub fn find(&self, kind: ScreenKind) -> Option<ScreenId> {
        self.iter()
            .find(|screen| screen.kind == kind)
            .map(Screen::id)
    }

    /// Takes the state of `id` out, to draw or drive it with the editor at hand; give it back
    /// with [`Self::put_state`].
    pub fn take_state(&mut self, id: ScreenId) -> Option<Box<dyn Any>> {
        self.get_mut(id)?.state.take()
    }

    /// Gives back a state taken with [`Self::take_state`]; dropped if the screen closed
    /// meanwhile.
    pub fn put_state(&mut self, id: ScreenId, state: Box<dyn Any>) {
        if let Some(screen) = self.get_mut(id) {
            screen.state = Some(state);
        }
    }

    pub fn state<T: Any>(&self, id: ScreenId) -> Option<&T> {
        self.get(id)?.state()
    }

    pub fn state_mut<T: Any>(&mut self, id: ScreenId) -> Option<&mut T> {
        self.get_mut(id)?.state_mut()
    }
}

/// Where a screen's window goes when it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Over the window it is opened from, which it hides until it closes: the focused window
    /// when it shows text or a screen that can be covered, else the window for documents (a
    /// split of the focused window if there is none).
    Cover,
    /// Along an edge of the whole editor, taking `share` percent of it: `Horizontal` at the
    /// bottom (`at_end`) or the top, `Vertical` at the right or the left.
    Edge {
        layout: Layout,
        at_end: bool,
        share: u16,
    },
    /// A split of the focused window.
    Split(Layout),
    /// Where it was ([`crate::tree::Tree::place`]), else along the edge `fallback`, `at_end`,
    /// taking the share it had.
    Back {
        place: Place,
        fallback: Layout,
        at_end: bool,
    },
}

/// What closing a screen's window did ([`Editor::close_screen_window`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Closed {
    /// The window shows what the screen covered again: this window.
    Uncovered(ViewId),
    /// The window is gone (the scratch terminal and the debug panel are only hidden).
    Gone,
}

impl Editor {
    /// Whether the window `view` shows a screen.
    pub fn is_screen_view(&self, view: ViewId) -> bool {
        self.tree.screen_view(view).is_some()
    }

    /// The screen of the focused window, if it shows one.
    pub fn focused_screen(&self) -> Option<&Screen> {
        self.screens.get(self.tree.focused_screen()?)
    }

    /// The window that shows the screen `screen`, shown or parked under a cover, if any.
    pub fn view_of(&self, screen: ScreenId) -> Option<ViewId> {
        self.tree
            .all_screen_views()
            .find(|view| view.screen == screen)
            .map(|view| view.id)
    }

    /// The windows shown that show the document `doc`.
    pub fn views_of(&self, doc: DocumentId) -> impl Iterator<Item = ViewId> + '_ {
        self.tree
            .views()
            .filter(move |(view, _)| view.doc == doc)
            .map(|(view, _)| view.id)
    }

    /// Whether `doc` is a buffer of its own: not the lines of a screen shown as text (a
    /// terminal's in view mode), which the buffer picker, the buffer cycling and the window for
    /// documents leave out.
    pub fn is_buffer(&self, doc: DocumentId) -> bool {
        self.document(doc)
            .is_some_and(|doc| doc.lines_of().is_none())
    }

    /// The window for documents: a text window shown, of a buffer of its own: the focused one,
    /// else the one that last had the focus, else any. Watch mode and the debugger follow in
    /// it, and files opened from a screen's window open there.
    pub fn document_view(&self) -> Option<ViewId> {
        let usable = |id: ViewId| {
            self.tree.is_visible(id)
                && self
                    .tree
                    .try_get(id)
                    .is_some_and(|view| self.is_buffer(view.doc))
        };
        let focus = self.tree.focus;
        if usable(focus) {
            return Some(focus);
        }
        self.tree.last_text().filter(|&id| usable(id)).or_else(|| {
            self.tree
                .traverse()
                .map(|(id, _)| id)
                .find(|&id| usable(id))
        })
    }

    /// Runs `f` with the focus in the window for documents, and gives the focus back to the
    /// screen that had it: watch mode follows and the debugger stops in the window next to the
    /// terminal you type in. With no window for documents, `f` runs in a new split of the screen.
    /// The editor's mode is kept.
    pub fn in_document_window<R>(&mut self, f: impl FnOnce(&mut Editor) -> R) -> R {
        let focus = self.tree.focus;
        let on_text = self
            .tree
            .try_get(focus)
            .is_some_and(|view| self.is_buffer(view.doc));
        if on_text {
            return f(self);
        }
        let mode = self.mode;
        match self.document_view() {
            Some(view) => self.tree.focus = view,
            None => {
                self.new_file(Action::VerticalSplit);
            }
        }
        self.tree.note_focus();
        let result = f(self);
        if self.tree.contains(focus) && self.tree.focus != focus {
            self.tree.note_focus();
            self.tree.focus = focus;
        }
        self.mode = mode;
        result
    }

    /// Adds `screen`, without showing it.
    pub fn add_screen(&mut self, screen: Screen) -> ScreenId {
        self.screens.add(screen)
    }

    /// The window a screen opened with [`Placement::Cover`] covers: the focused window when it
    /// shows text or a screen that can be covered, else the window for documents.
    fn window_to_cover(&self) -> Option<ViewId> {
        let focus = self.tree.focus;
        if self.tree.try_get(focus).is_some() {
            return Some(focus);
        }
        let coverable = self
            .focused_screen()
            .is_some_and(|screen| screen.kind.can_be_covered());
        if coverable {
            return Some(focus);
        }
        self.document_view()
    }

    /// Opens a window for the screen `screen` at `placement`, and focuses it. A screen shown
    /// already is only focused.
    pub fn open_screen(&mut self, screen: ScreenId, placement: Placement) -> ViewId {
        if let Some(view) = self.view_of(screen) {
            let top = self.tree.top_of(view);
            self.focus(top);
            return top;
        }
        self.enter_normal_mode();
        let from = self.tree.focus;
        let window = ScreenView::new(screen);
        let id = match placement {
            Placement::Cover => match self.window_to_cover() {
                Some(covered) => self.tree.cover(covered, window),
                None if self.tree.is_visible(from) => self.tree.split(window, Layout::Vertical),
                None => self.tree.split_root(window, Layout::Vertical, true, None),
            },
            Placement::Edge {
                layout,
                at_end,
                share,
            } => self.tree.split_root(window, layout, at_end, Some(share)),
            Placement::Split(layout) if self.tree.is_visible(from) => {
                self.tree.split(window, layout)
            }
            Placement::Split(layout) => self.tree.split_root(window, layout, true, None),
            Placement::Back {
                place,
                fallback,
                at_end,
            } => match self.tree.insert_at(window, &place) {
                Some(id) => id,
                None => {
                    let window = ScreenView::new(screen);
                    self.tree.split_root(window, fallback, at_end, place.share)
                }
            },
        };
        self.tree.focus = id;
        self.screen_focused(from);
        self._refresh();
        id
    }

    /// Covers the window `covered` with a new text window of the new document `doc` (a
    /// terminal's lines in view mode), focused if `covered` was. Returns the document and the
    /// window.
    pub fn cover_with_document(
        &mut self,
        covered: ViewId,
        doc: crate::Document,
    ) -> (DocumentId, ViewId) {
        let doc = self.new_document(doc);
        let view = View::new(doc, self.config().gutters.clone());
        let id = self.tree.cover(covered, Leaf::Text(view));
        if let Some(document) = self.documents.get_mut(&doc) {
            document.ensure_view_init(id);
        }
        self._refresh();
        (doc, id)
    }

    /// Takes away the text window of `doc` that covers another window (a terminal's lines in
    /// view mode), and closes `doc`: the window it covered is shown again, focused if it was.
    /// The focus doesn't count as moving (the terminal stays in normal mode).
    pub fn uncover_document(&mut self, doc: DocumentId) {
        let cover = self
            .tree
            .all_views()
            .find(|view| view.doc == doc && self.tree.under(view.id).is_some())
            .map(|view| view.id);
        if let Some(cover) = cover {
            if let Some(document) = self.documents.get_mut(&doc) {
                document.remove_view(cover);
            }
            self.tree.uncover(cover);
        }
        let _ = self.close_document(doc, true);
        self._refresh();
    }

    /// Shows the screen `screen`: focuses its window (the window over it, if it is covered), or
    /// puts it back where it goes if it is hidden (the scratch terminal, the debug panel).
    pub fn show_screen(&mut self, screen: ScreenId) -> Option<ViewId> {
        if let Some(view) = self.view_of(screen) {
            let top = self.tree.top_of(view);
            self.focus(top);
            return Some(top);
        }
        let placement = match self.screens.kind(screen)? {
            ScreenKind::Terminal => self.terminal_placement(screen),
            ScreenKind::DebugPanel => self.debug_panel_placement(),
            ScreenKind::Hex | ScreenKind::Diff | ScreenKind::Markdown => Placement::Cover,
        };
        Some(self.open_screen(screen, placement))
    }

    /// Takes the screen's window `view` out of the tree, keeping its screen, and gives back
    /// where it was, to [`Self::open_screen`] it there again. What it covered goes with it. The
    /// focus goes to the window for documents.
    pub fn hide_screen(&mut self, view: ViewId) -> Place {
        let place = self.tree.place(view);
        let focused = self.tree.focus == view;
        if focused {
            if let Some(next) = self.document_view() {
                self.focus(next);
            }
        }
        self.drop_windows(view);
        let still_focused = self.tree.focus == view;
        self.tree.remove(view);
        self.ensure_a_window();
        if still_focused {
            self.focus_moved(None);
        }
        self._refresh();
        place
    }

    /// After a screen's window went on its own (its shell exited, it was hidden): if it was the
    /// last window, the text window kept parked comes back, or a new one, rather than the editor
    /// quitting, which only `:q` does.
    pub(crate) fn ensure_a_window(&mut self) {
        if self.tree.windows().next().is_some() {
            return;
        }
        if let Some(&orphan) = self.tree.orphans().first() {
            self.tree.unpark(orphan);
            return;
        }
        let doc = self
            .documents
            .values()
            .find(|doc| doc.lines_of().is_none())
            .map(|doc| doc.id());
        let doc = match doc {
            Some(doc) => doc,
            None => self.new_document(crate::Document::default(
                self.config.clone(),
                self.syn_loader.clone(),
            )),
        };
        let view = View::new(doc, self.config().gutters.clone());
        let id = self.tree.insert(view);
        if let Some(doc) = self.documents.get_mut(&doc) {
            doc.ensure_view_init(id);
        }
    }

    /// Before the window `view` leaves the tree with what it covers: the documents forget the
    /// text windows among them; the screens they show that aren't kept elsewhere close, but the
    /// scratch terminal and the debug panel, which are only hidden.
    pub(crate) fn drop_windows(&mut self, view: ViewId) {
        for id in self.tree.chain(view) {
            if let Some(text) = self.tree.try_get(id) {
                let _ = text;
                for doc in self.documents.values_mut() {
                    doc.remove_view(id);
                }
            }
        }
    }

    /// `Esc` on a screen (once its own modal layers took theirs), or its own way out: the window
    /// shows what the screen covered again, or, when it covered nothing, the window closes (the
    /// scratch terminal and the debug panel are hidden). The screen closes, but the scratch
    /// terminal and the debug panel; what history keeps of it is the caller's (see
    /// `vortex-term`'s `screens::close`).
    pub fn close_screen_window(&mut self, view: ViewId) -> Closed {
        let Some(screen) = self.tree.screen_view(view).map(|view| view.screen) else {
            return Closed::Gone;
        };
        let focused = self.tree.focus == view;
        if self.tree.under(view).is_some() {
            let shown = self.tree.uncover(view).expect("a cover");
            self.screens.remove(screen);
            self.terminals.screen_closed(screen);
            if focused {
                self.screen_focused_from_screen(screen);
            }
            self._refresh();
            return Closed::Uncovered(shown);
        }
        match self.screens.kind(screen) {
            Some(ScreenKind::Terminal) if self.terminals.scratch == Some(screen) => {
                let place = self.hide_screen(view);
                self.terminals.scratch_place = Some(place);
            }
            Some(ScreenKind::DebugPanel) => {
                let place = self.hide_screen(view);
                self.debug.panel_place = Some(place);
            }
            _ => {
                self.close_window(view);
                self.screens.remove(screen);
                self.terminals.screen_closed(screen);
                self.ensure_a_window();
                self._refresh();
            }
        }
        Closed::Gone
    }

    /// Closes the screen `screen` wherever it is: its window shows what it covered, or goes.
    pub fn close_screen(&mut self, screen: ScreenId) {
        match self.view_of(screen) {
            Some(view) if self.tree.is_parked(view) => {
                // under a cover (a terminal in view mode): the whole window goes
                let top = self.tree.top_of(view);
                self.close_window(top);
            }
            Some(view) if self.tree.under(view).is_some() => {
                let focused = self.tree.focus == view;
                self.tree.uncover(view);
                if focused {
                    self.screen_focused_from_screen(screen);
                }
                self._refresh();
            }
            Some(view) => self.close_window(view),
            None => {}
        }
        self.screens.remove(screen);
        self.terminals.screen_closed(screen);
        if self.debug.panel == Some(screen) {
            self.debug.panel = None;
        }
        self.ensure_a_window();
        self._refresh();
    }

    /// The focus came back to a window from a screen that just closed.
    fn screen_focused_from_screen(&mut self, screen: ScreenId) {
        // from the screen (gone now): the mode stays as the command left it (`i` on a page)
        self.focus_moved(Some(Content::Screen(screen)));
    }

    /// The focus moved from `from` (`Editor::focus` and the screen's windows): entering a
    /// screen lands in its insert mode (a terminal types), leaving one tells it.
    pub(crate) fn screen_focused(&mut self, from: ViewId) {
        let to = self.tree.focus;
        if from == to {
            return;
        }
        let from = self.window_content(from);
        self.focus_moved(from);
    }

    /// What the window `id` shows, for [`Self::focus_moved`].
    pub fn window_content(&self, id: ViewId) -> Option<Content> {
        match self.tree.window(id)? {
            crate::tree::Window::Text(view) => Some(Content::Text(view.doc)),
            crate::tree::Window::Screen(view) => Some(Content::Screen(view.screen)),
        }
    }

    /// The focus moved away from a window that showed `from` (gone, maybe): entering a terminal
    /// lands in its insert mode (it types), leaving one tells it and gives normal mode back.
    pub(crate) fn focus_moved(&mut self, from: Option<Content>) {
        self.tree.note_focus();
        let to = self.window_content(self.tree.focus);
        if from == to {
            return;
        }
        let terminal_of = |editor: &Editor, content: Option<Content>| match content {
            Some(Content::Screen(screen)) => editor.terminals.is_terminal(screen).then_some(screen),
            // a terminal's lines in view mode are the terminal's
            Some(Content::Text(doc)) => editor
                .document(doc)
                .and_then(|doc| doc.lines_of())
                .filter(|&screen| editor.terminals.is_terminal(screen)),
            None => None,
        };
        let left = terminal_of(self, from);
        let entered = terminal_of(self, to);
        if left.is_some() && left == entered {
            return;
        }
        if let Some(terminal) = left.and_then(|screen| self.terminals.get_mut(screen)) {
            terminal.focus_out();
        }
        if let Some(Content::Screen(screen)) = from {
            if self.screens.get(screen).is_some() {
                vortex_event::dispatch(crate::events::ScreenFocusLost {
                    editor: self,
                    screen,
                });
            }
        }
        let entered_live = matches!(to, Some(Content::Screen(_))) && entered.is_some();
        let on_screen = self.is_screen_view(self.tree.focus);
        match entered.and_then(|screen| self.terminals.get_mut(screen)) {
            Some(terminal) if entered_live => {
                terminal.focus_in();
                self.mode = crate::document::Mode::Insert;
            }
            Some(terminal) => terminal.focus_in(),
            None if left.is_some() || from.is_none() || on_screen => {
                if self.mode != crate::document::Mode::Normal {
                    self.mode = crate::document::Mode::Normal;
                }
            }
            None => {}
        }
    }
}

/// What a window shows: a text document or a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    Text(DocumentId),
    Screen(ScreenId),
}
