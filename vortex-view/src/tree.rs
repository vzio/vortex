use crate::{graphics::Rect, screen::ScreenId, View, ViewId};
use slotmap::SlotMap;

// the dimensions are recomputed on window resize/tree change.
//
#[derive(Debug)]
pub struct Tree {
    root: ViewId,
    // (container, index inside the container)
    pub focus: ViewId,
    // fullscreen: bool,
    area: Rect,

    nodes: SlotMap<ViewId, Node>,

    // used for traversals
    stack: Vec<(ViewId, Rect)>,

    /// The text window that last had the focus (this fork): what text commands act on while a
    /// screen's window has the focus (see [`Self::text_focus`]).
    last_text: Option<ViewId>,
}

#[derive(Debug)]
pub struct Node {
    parent: ViewId,
    content: Content,
    /// The node's size in percent of its container, along the container's layout (this fork):
    /// a terminal's window takes `[editor.terminal] size`. `None` shares what the sized nodes
    /// leave equally with the other unsized ones.
    share: Option<u16>,
    /// The window this one covers (this fork): a screen opened in a window (a diff, the Markdown
    /// page, the hex view) or a terminal's lines in view mode cover what the window showed, which
    /// is parked meanwhile and takes its place again when the cover closes ([`Tree::uncover`]).
    under: Option<ViewId>,
    /// Out of the layout (this fork): covered by another window, or the last text window, closed
    /// while screens' windows stay ([`Tree::park`]). Kept, so that text commands always have a
    /// text window to act on.
    parked: bool,
}

/// Where a node was in the tree (this fork), to put a window back there after it left the tree
/// (the scratch terminal hidden with `C-t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// The container it was in.
    pub parent: ViewId,
    /// Its index among the container's children.
    pub index: usize,
    /// The container's layout then: the place is gone if it changed.
    pub layout: Layout,
    pub share: Option<u16>,
}

/// What a node holds (this fork: a window holds a text document or a screen).
#[derive(Debug)]
pub enum Content {
    /// A text window: a [`View`] of a text document.
    View(Box<View>),
    /// A screen's window (`crate::screen`): something that isn't a text document.
    Screen(Box<ScreenView>),
    Container(Box<Container>),
}

/// A window that shows a screen (this fork): no text, no cursor of the editor's; the screen
/// draws itself in `area` and takes its own keys (`vortex-term`'s `screens`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    pub id: ViewId,
    pub area: Rect,
    pub screen: ScreenId,
}

impl ScreenView {
    pub fn new(screen: ScreenId) -> Self {
        Self {
            id: ViewId::default(),
            area: Rect::default(),
            screen,
        }
    }

    /// The area the screen draws in: its window less the statusline, the last row.
    pub fn inner_area(&self) -> Rect {
        self.area.clip_bottom(1)
    }
}

/// A new window (this fork): a text window or a screen's.
// only passed by value to the tree's insertions, which box it: no need to box it here too
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Leaf {
    Text(View),
    Screen(ScreenView),
}

impl From<View> for Leaf {
    fn from(view: View) -> Self {
        Leaf::Text(view)
    }
}

impl From<ScreenView> for Leaf {
    fn from(view: ScreenView) -> Self {
        Leaf::Screen(view)
    }
}

/// A window of the tree (this fork): a text window or a screen's. Text code takes [`View`]s
/// ([`Tree::get`], [`Tree::views`]), so it never meets a screen; code about windows takes these.
#[derive(Debug, Clone, Copy)]
pub enum Window<'a> {
    Text(&'a View),
    Screen(&'a ScreenView),
}

impl<'a> Window<'a> {
    pub fn id(&self) -> ViewId {
        match self {
            Window::Text(view) => view.id,
            Window::Screen(view) => view.id,
        }
    }

    pub fn area(&self) -> Rect {
        match self {
            Window::Text(view) => view.area,
            Window::Screen(view) => view.area,
        }
    }

    /// The text window, if it is one.
    pub fn text(&self) -> Option<&'a View> {
        match self {
            Window::Text(view) => Some(view),
            Window::Screen(_) => None,
        }
    }

    /// The screen it shows, if it is a screen's window.
    pub fn screen(&self) -> Option<ScreenId> {
        match self {
            Window::Text(_) => None,
            Window::Screen(view) => Some(view.screen),
        }
    }
}

impl Node {
    pub fn container(layout: Layout) -> Self {
        Self {
            parent: ViewId::default(),
            content: Content::Container(Box::new(Container::new(layout))),
            share: None,
            under: None,
            parked: false,
        }
    }

    pub fn view(view: View) -> Self {
        Self::leaf(Leaf::Text(view))
    }

    fn leaf(leaf: Leaf) -> Self {
        Self {
            parent: ViewId::default(),
            content: match leaf {
                Leaf::Text(view) => Content::View(Box::new(view)),
                Leaf::Screen(view) => Content::Screen(Box::new(view)),
            },
            share: None,
            under: None,
            parked: false,
        }
    }

    /// The window's area, if it is one.
    fn leaf_area(&self) -> Option<Rect> {
        match &self.content {
            Content::View(view) => Some(view.area),
            Content::Screen(view) => Some(view.area),
            Content::Container(_) => None,
        }
    }

    /// The window it is, if it is one.
    fn window(&self) -> Option<Window<'_>> {
        match &self.content {
            Content::View(view) => Some(Window::Text(view)),
            Content::Screen(view) => Some(Window::Screen(view)),
            Content::Container(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Horizontal,
    Vertical,
    // could explore stacked/tabbed
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug)]
pub struct Container {
    layout: Layout,
    children: Vec<ViewId>,
    area: Rect,
}

impl Container {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            children: Vec::new(),
            area: Rect::default(),
        }
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new(Layout::Vertical)
    }
}

impl Tree {
    pub fn new(area: Rect) -> Self {
        let root = Node::container(Layout::Vertical);

        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(root);

        // root is it's own parent
        nodes[root].parent = root;

        Self {
            root,
            focus: root,
            // fullscreen: false,
            area,
            nodes,
            stack: Vec::new(),
            last_text: None,
        }
    }

    /// Inserts a new window node, its id set.
    fn insert_leaf(&mut self, node: Node) -> ViewId {
        let node = self.nodes.insert(node);
        match &mut self.nodes[node].content {
            Content::View(view) => view.id = node,
            Content::Screen(view) => view.id = node,
            Content::Container(_) => {}
        }
        node
    }

    /// Focuses `id` (the tree's own moves), noting it when it is a text window.
    fn set_focus(&mut self, id: ViewId) {
        self.focus = id;
        self.note_focus();
    }

    /// The text window that last had the focus, if it is still there (shown or parked).
    pub fn last_text(&self) -> Option<ViewId> {
        self.last_text.filter(|&id| self.try_get(id).is_some())
    }

    /// Notes the focused window as the last text window to have it, if it is one (this fork):
    /// called by whoever sets `focus`, see [`Self::text_focus`].
    pub fn note_focus(&mut self) {
        if matches!(
            self.nodes.get(self.focus),
            Some(Node {
                content: Content::View(_),
                parked: false,
                ..
            })
        ) {
            self.last_text = Some(self.focus);
        }
    }

    pub fn insert(&mut self, view: impl Into<Leaf>) -> ViewId {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;
        let mut node = Node::leaf(view.into());
        node.parent = parent;
        let node = self.insert_leaf(node);

        let container = match &mut self.nodes[parent] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        };

        // insert node after the current item if there is children already
        let pos = if container.children.is_empty() {
            0
        } else {
            let pos = container
                .children
                .iter()
                .position(|&child| child == focus)
                .unwrap();
            pos + 1
        };

        container.children.insert(pos, node);
        // focus the new node
        self.set_focus(node);

        // recalculate all the sizes
        self.recalculate();

        node
    }

    pub fn split(&mut self, view: impl Into<Leaf>, layout: Layout) -> ViewId {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;

        let node = Node::leaf(view.into());
        let node = self.insert_leaf(node);

        let container = match &mut self.nodes[parent] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        };
        if container.layout == layout {
            // insert node after the current item if there is children already
            let pos = if container.children.is_empty() {
                0
            } else {
                let pos = container
                    .children
                    .iter()
                    .position(|&child| child == focus)
                    .unwrap();
                pos + 1
            };
            container.children.insert(pos, node);
            self.nodes[node].parent = parent;
        } else {
            let mut split = Node::container(layout);
            split.parent = parent;
            let split = self.nodes.insert(split);

            let container = match &mut self.nodes[split] {
                Node {
                    content: Content::Container(container),
                    ..
                } => container,
                _ => unreachable!(),
            };
            container.children.push(focus);
            container.children.push(node);
            self.nodes[focus].parent = split;
            self.nodes[node].parent = split;
            // the new split takes the place of the focused node, and its size
            let share = self.nodes[focus].share.take();
            self.nodes[split].share = share;

            let container = match &mut self.nodes[parent] {
                Node {
                    content: Content::Container(container),
                    ..
                } => container,
                _ => unreachable!(),
            };

            let pos = container
                .children
                .iter()
                .position(|&child| child == focus)
                .unwrap();

            // replace focus on parent with split
            container.children[pos] = split;
        }

        // focus the new node
        self.set_focus(node);

        // recalculate all the sizes
        self.recalculate();

        node
    }

    /// Get a mutable reference to a [Container] by index.
    /// # Panics
    /// Panics if `index` is not in self.nodes, or if the node's content is not a [Content::Container].
    fn container_mut(&mut self, index: ViewId) -> &mut Container {
        match &mut self.nodes[index] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        }
    }

    fn remove_or_replace(&mut self, child: ViewId, replacement: Option<ViewId>) {
        let parent = self.nodes[child].parent;

        self.nodes.remove(child);

        let container = self.container_mut(parent);
        let pos = container
            .children
            .iter()
            .position(|&item| item == child)
            .unwrap();

        if let Some(new) = replacement {
            container.children[pos] = new;
            self.nodes[new].parent = parent;
        } else {
            container.children.remove(pos);
        }
    }

    /// Removes the window `index` from the tree, with the windows it covers (this fork: see
    /// [`Self::chain`]). A parked window just goes.
    pub fn remove(&mut self, index: ViewId) {
        // what it covers goes with it
        let mut under = self.nodes[index].under;
        while let Some(id) = under {
            under = self.nodes[id].under;
            self.nodes.remove(id);
        }
        if self.nodes[index].parked {
            // the window that covers it covers nothing now
            if let Some(cover) = self.cover_of(index) {
                self.nodes[cover].under = None;
            }
            self.nodes.remove(index);
            return;
        }
        if self.focus == index {
            // focus on something else
            let prev = self.prev();
            self.set_focus(prev);
        }

        let parent = self.nodes[index].parent;
        let parent_is_root = parent == self.root;

        self.remove_or_replace(index, None);

        let parent_container = self.container_mut(parent);
        if parent_container.children.len() == 1 && !parent_is_root {
            // Lets merge the only child back to its grandparent so that Views
            // are equally spaced.
            let sibling = parent_container.children.pop().unwrap();
            // the child takes the container's place, and its size
            self.nodes[sibling].share = self.nodes[parent].share;
            self.remove_or_replace(parent, Some(sibling));
        }

        self.recalculate()
    }

    /// The text windows shown (this fork: not the screens' windows, nor the windows parked
    /// under a cover), with whether each has the focus.
    pub fn views(&self) -> impl Iterator<Item = (&View, bool)> {
        let focus = self.focus;
        self.nodes.iter().filter_map(move |(key, node)| match node {
            Node {
                content: Content::View(view),
                parked: false,
                ..
            } => Some((view.as_ref(), focus == key)),
            _ => None,
        })
    }

    /// The text windows shown, see [`Self::views`].
    pub fn views_mut(&mut self) -> impl Iterator<Item = (&mut View, bool)> {
        let focus = self.focus;
        self.nodes
            .iter_mut()
            .filter_map(move |(key, node)| match node {
                Node {
                    content: Content::View(view),
                    parked: false,
                    ..
                } => Some((view.as_mut(), focus == key)),
                _ => None,
            })
    }

    /// Every text window, the parked ones too (this fork): what must follow a document (its
    /// closing, its changes) whether or not it is shown.
    pub fn all_views(&self) -> impl Iterator<Item = &View> {
        self.nodes.values().filter_map(|node| match &node.content {
            Content::View(view) => Some(view.as_ref()),
            _ => None,
        })
    }

    /// Every text window, the parked ones too, see [`Self::all_views`].
    pub fn all_views_mut(&mut self) -> impl Iterator<Item = &mut View> {
        self.nodes
            .values_mut()
            .filter_map(|node| match &mut node.content {
                Content::View(view) => Some(view.as_mut()),
                _ => None,
            })
    }

    /// The windows shown, text and screens' (this fork), with whether each has the focus.
    pub fn windows(&self) -> impl Iterator<Item = (Window<'_>, bool)> {
        let focus = self.focus;
        self.nodes.iter().filter_map(move |(key, node)| {
            if node.parked {
                return None;
            }
            node.window().map(|window| (window, focus == key))
        })
    }

    /// Every screen's window, the parked ones too (this fork).
    pub fn all_screen_views(&self) -> impl Iterator<Item = &ScreenView> {
        self.nodes.values().filter_map(|node| match &node.content {
            Content::Screen(view) => Some(view.as_ref()),
            _ => None,
        })
    }

    /// The window `id`, text or a screen's, shown or parked (this fork).
    pub fn window(&self, id: ViewId) -> Option<Window<'_>> {
        self.nodes.get(id)?.window()
    }

    /// The screen's window `id`, shown or parked (this fork); `None` for any other node.
    pub fn screen_view(&self, id: ViewId) -> Option<&ScreenView> {
        match self.nodes.get(id) {
            Some(Node {
                content: Content::Screen(view),
                ..
            }) => Some(view),
            _ => None,
        }
    }

    /// The screen shown in the focused window, if it shows one (this fork).
    pub fn focused_screen(&self) -> Option<ScreenId> {
        self.screen_view(self.focus).map(|view| view.screen)
    }

    /// Whether `id` is a window shown in the layout (this fork): not parked, not gone.
    pub fn is_visible(&self, id: ViewId) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|node| !node.parked && node.window().is_some())
    }

    /// Whether the window `id` is parked under a cover (or an orphan text window).
    pub fn is_parked(&self, id: ViewId) -> bool {
        self.nodes.get(id).is_some_and(|node| node.parked)
    }

    /// The window that `id` covers, if any (this fork).
    pub fn under(&self, id: ViewId) -> Option<ViewId> {
        self.nodes.get(id)?.under
    }

    /// The window that covers the parked window `id`, if any (this fork).
    pub fn cover_of(&self, id: ViewId) -> Option<ViewId> {
        self.nodes
            .iter()
            .find(|(_, node)| node.under == Some(id))
            .map(|(key, _)| key)
    }

    /// The window shown in the layout that `id` is (or lies under, through covers).
    pub fn top_of(&self, mut id: ViewId) -> ViewId {
        while let Some(cover) = self.cover_of(id) {
            id = cover;
        }
        id
    }

    /// `id` and the windows it covers, top first (this fork).
    pub fn chain(&self, id: ViewId) -> Vec<ViewId> {
        let mut chain = vec![id];
        let mut under = self.under(id);
        while let Some(next) = under {
            chain.push(next);
            under = self.under(next);
        }
        chain
    }

    /// The text window text commands act on (this fork): the focused window when it is one; else
    /// (a screen's window has the focus) the text window the focused screen covers, else the
    /// text window that last had the focus, else any shown, else one parked. The macros
    /// `current!`, `view!` and `doc!` go through here, so they always give a text window.
    pub fn text_focus(&self) -> ViewId {
        let is_text = |id: ViewId| {
            matches!(
                self.nodes.get(id),
                Some(Node {
                    content: Content::View(_),
                    ..
                })
            )
        };
        let focus = self.focus;
        if is_text(focus) {
            return focus;
        }
        if let Some(id) = self
            .chain(focus)
            .into_iter()
            .skip(1)
            .find(|&id| is_text(id))
        {
            return id;
        }
        if let Some(id) = self.last_text.filter(|&id| is_text(id)) {
            return id;
        }
        if let Some((id, _)) = self.traverse().find(|(_, window)| window.text().is_some()) {
            return id;
        }
        self.nodes
            .iter()
            .find(|(_, node)| matches!(node.content, Content::View(_)))
            .map_or(focus, |(key, _)| key)
    }

    /// Get reference to a [View] by index.
    /// # Panics
    ///
    /// Panics if `index` is not in self.nodes, or if the node's content is not [Content::View]. This can be checked with [Self::contains].
    pub fn get(&self, index: ViewId) -> &View {
        self.try_get(index).unwrap()
    }

    /// Try to get reference to a [View] by index. Returns `None` if node content is not a [`Content::View`].
    ///
    /// Does not panic if the view does not exists anymore.
    pub fn try_get(&self, index: ViewId) -> Option<&View> {
        match self.nodes.get(index) {
            Some(Node {
                content: Content::View(view),
                ..
            }) => Some(view),
            _ => None,
        }
    }

    /// Get a mutable reference to a [View] by index.
    /// # Panics
    ///
    /// Panics if `index` is not in self.nodes, or if the node's content is not [Content::View]. This can be checked with [Self::contains].
    pub fn get_mut(&mut self, index: ViewId) -> &mut View {
        match &mut self.nodes[index] {
            Node {
                content: Content::View(view),
                ..
            } => view,
            _ => unreachable!(),
        }
    }

    /// The text window `index`, mutably, if it is one (this fork).
    pub fn try_get_mut(&mut self, index: ViewId) -> Option<&mut View> {
        match self.nodes.get_mut(index) {
            Some(Node {
                content: Content::View(view),
                ..
            }) => Some(view),
            _ => None,
        }
    }

    /// Check if tree contains a [Node] with a given index (this fork: shown or parked; see
    /// [`Self::is_visible`]).
    pub fn contains(&self, index: ViewId) -> bool {
        self.nodes.contains_key(index)
    }

    /// Puts the new window `leaf` in the place of the window `id` in the layout, `id` parked
    /// under it until the new one closes with [`Self::uncover`] (this fork). The new window takes
    /// the focus if `id` had it. Returns its id.
    pub fn cover(&mut self, id: ViewId, leaf: impl Into<Leaf>) -> ViewId {
        let parent = self.nodes[id].parent;
        let share = self.nodes[id].share;
        let mut node = Node::leaf(leaf.into());
        node.parent = parent;
        node.share = share;
        node.under = Some(id);
        let new = self.insert_leaf(node);
        if let Content::Container(container) = &mut self.nodes[parent].content {
            if let Some(pos) = container.children.iter().position(|&child| child == id) {
                container.children[pos] = new;
            }
        }
        let covered = &mut self.nodes[id];
        covered.parked = true;
        covered.parent = new;
        if self.focus == id {
            self.set_focus(new);
        }
        self.recalculate();
        new
    }

    /// Takes the cover `id` out of the tree (this fork): the window it covers takes its place
    /// again, with the focus if `id` had it. Returns the window shown again; `None` (and nothing
    /// changes) when `id` covers nothing.
    pub fn uncover(&mut self, id: ViewId) -> Option<ViewId> {
        let under = self.nodes.get(id)?.under?;
        let (parent, share, parked) = {
            let node = &self.nodes[id];
            (node.parent, node.share, node.parked)
        };
        {
            let node = &mut self.nodes[under];
            node.parked = parked;
            node.parent = parent;
            node.share = share;
        }
        if let Content::Container(container) = &mut self.nodes[parent].content {
            if let Some(pos) = container.children.iter().position(|&child| child == id) {
                container.children[pos] = under;
            }
        }
        // a parked cover (under another one) hands its place in the chain down
        if parked {
            if let Some(node) = self.nodes.get_mut(parent) {
                if node.under == Some(id) {
                    node.under = Some(under);
                }
            }
        }
        self.nodes.remove(id);
        if self.focus == id {
            self.set_focus(under);
        }
        self.recalculate();
        Some(under)
    }

    /// Takes the text window `id` out of the layout but keeps it, parked (this fork): the last
    /// text window, closed while screens' windows stay, so that text commands still have one to
    /// act on (see [`Self::text_focus`]). Its next shown text window drops it.
    pub fn park(&mut self, id: ViewId) {
        if self.focus == id {
            let prev = self.prev();
            self.set_focus(prev);
        }
        let parent = self.nodes[id].parent;
        let parent_is_root = parent == self.root;
        let container = self.container_mut(parent);
        if let Some(pos) = container.children.iter().position(|&item| item == id) {
            container.children.remove(pos);
        }
        let node = &mut self.nodes[id];
        node.parked = true;
        node.parent = self.root;
        let parent_container = self.container_mut(parent);
        if parent_container.children.len() == 1 && !parent_is_root {
            let sibling = parent_container.children.pop().unwrap();
            self.nodes[sibling].share = self.nodes[parent].share;
            self.remove_or_replace(parent, Some(sibling));
        }
        self.recalculate();
    }

    /// Puts the parked window `id` back in the layout as the only window, focused (this fork:
    /// the last text window, kept by [`Self::park`], once its screens closed).
    pub fn unpark(&mut self, id: ViewId) {
        let root = self.root;
        let node = &mut self.nodes[id];
        node.parked = false;
        node.parent = root;
        node.share = None;
        self.container_mut(root).children.push(id);
        self.set_focus(id);
        self.recalculate();
    }

    /// The parked text windows that cover nothing and lie under nothing (this fork): the last
    /// text window kept by [`Self::park`].
    pub fn orphans(&self) -> Vec<ViewId> {
        self.nodes
            .iter()
            .filter(|(key, node)| {
                node.parked
                    && matches!(node.content, Content::View(_))
                    && !self.nodes.values().any(|other| other.under == Some(*key))
            })
            .map(|(key, _)| key)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        match &self.nodes[self.root] {
            Node {
                content: Content::Container(container),
                ..
            } => container.children.is_empty(),
            _ => unreachable!(),
        }
    }

    pub fn resize(&mut self, area: Rect) -> bool {
        if self.area != area {
            self.area = area;
            self.recalculate();
            return true;
        }
        false
    }

    pub fn recalculate(&mut self) {
        if self.is_empty() {
            // There are no more views, so the tree should focus itself again.
            self.focus = self.root;

            return;
        }

        self.stack.push((self.root, self.area));

        // take the area
        // fetch the node
        // a) node is view, give it whole area
        // b) node is container, calculate areas for each child and push them on the stack

        while let Some((key, area)) = self.stack.pop() {
            // the sizes of a container's children (this fork), read before borrowing it
            let shares: Vec<Option<u16>> = match &self.nodes[key].content {
                Content::Container(container) => container
                    .children
                    .iter()
                    .map(|&child| self.nodes[child].share)
                    .collect(),
                Content::View(_) | Content::Screen(_) => Vec::new(),
            };
            let node = &mut self.nodes[key];

            match &mut node.content {
                Content::View(view) => {
                    // debug!!("setting view area {:?}", area);
                    view.area = area;
                } // TODO: call f()
                Content::Screen(view) => view.area = area,
                Content::Container(container) => {
                    // debug!!("setting container area {:?}", area);
                    container.area = area;

                    match container.layout {
                        Layout::Horizontal => {
                            let len = container.children.len();

                            let heights = sizes(&shares, area.height, area.height / len as u16);

                            let mut child_y = area.y;

                            for (i, child) in container.children.iter().enumerate() {
                                let height = heights[i];
                                let mut area = Rect::new(
                                    container.area.x,
                                    child_y,
                                    container.area.width,
                                    height,
                                );
                                child_y += height;

                                // last child takes the remaining width because we can get uneven
                                // space from rounding
                                if i == len - 1 {
                                    area.height = container.area.y + container.area.height - area.y;
                                }

                                self.stack.push((*child, area));
                            }
                        }
                        Layout::Vertical => {
                            let len = container.children.len();
                            let len_u16 = len as u16;

                            let inner_gap = 1u16;
                            let total_gap = inner_gap * len_u16.saturating_sub(2);

                            let used_area = area.width.saturating_sub(total_gap);
                            let widths = sizes(&shares, used_area, used_area / len_u16);

                            let mut child_x = area.x;

                            for (i, child) in container.children.iter().enumerate() {
                                let width = widths[i];
                                let mut area = Rect::new(
                                    child_x,
                                    container.area.y,
                                    width,
                                    container.area.height,
                                );
                                child_x += width + inner_gap;

                                // last child takes the remaining width because we can get uneven
                                // space from rounding
                                if i == len - 1 {
                                    area.width = container.area.x + container.area.width - area.x;
                                }

                                self.stack.push((*child, area));
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn traverse(&self) -> Traverse<'_> {
        Traverse::new(self)
    }

    // Finds the split in the given direction if it exists
    pub fn find_split_in_direction(&self, id: ViewId, direction: Direction) -> Option<ViewId> {
        let parent = self.nodes[id].parent;
        // Base case, we found the root of the tree
        if parent == id {
            return None;
        }
        // Parent must always be a container
        let parent_container = match &self.nodes[parent].content {
            Content::Container(container) => container,
            Content::View(_) | Content::Screen(_) => unreachable!(),
        };

        match (direction, parent_container.layout) {
            (Direction::Up, Layout::Vertical)
            | (Direction::Left, Layout::Horizontal)
            | (Direction::Right, Layout::Horizontal)
            | (Direction::Down, Layout::Vertical) => {
                // The desired direction of movement is not possible within
                // the parent container so the search must continue closer to
                // the root of the split tree.
                self.find_split_in_direction(parent, direction)
            }
            (Direction::Up, Layout::Horizontal)
            | (Direction::Down, Layout::Horizontal)
            | (Direction::Left, Layout::Vertical)
            | (Direction::Right, Layout::Vertical) => {
                // It's possible to move in the desired direction within
                // the parent container so an attempt is made to find the
                // correct child.
                match self.find_child(id, &parent_container.children, direction) {
                    // Child is found, search is ended
                    Some(id) => Some(id),
                    // A child is not found. This could be because of either two scenarios
                    // 1. Its not possible to move in the desired direction, and search should end
                    // 2. A layout like the following with focus at X and desired direction Right
                    // | _ | x |   |
                    // | _ _ _ |   |
                    // | _ _ _ |   |
                    // The container containing X ends at X so no rightward movement is possible
                    // however there still exists another view/container to the right that hasn't
                    // been explored. Thus another search is done here in the parent container
                    // before concluding it's not possible to move in the desired direction.
                    None => self.find_split_in_direction(parent, direction),
                }
            }
        }
    }

    fn find_child(&self, id: ViewId, children: &[ViewId], direction: Direction) -> Option<ViewId> {
        let mut child_id = match direction {
            // index wise in the child list the Up and Left represents a -1
            // thus reversed iterator.
            Direction::Up | Direction::Left => children
                .iter()
                .rev()
                .skip_while(|i| **i != id)
                .copied()
                .nth(1)?,
            // Down and Right => +1 index wise in the child list
            Direction::Down | Direction::Right => {
                children.iter().skip_while(|i| **i != id).copied().nth(1)?
            }
        };
        let (current_x, current_y) = match self.nodes[self.focus].leaf_area() {
            Some(area) => (area.left(), area.top()),
            None => unreachable!(),
        };

        // If the child is a container the search finds the closest container child
        // visually based on screen location.
        while let Content::Container(container) = &self.nodes[child_id].content {
            match (direction, container.layout) {
                (_, Layout::Vertical) => {
                    // find closest split based on x because y is irrelevant
                    // in a vertical container (and already correct based on previous search)
                    child_id = *container.children.iter().min_by_key(|id| {
                        let x = match &self.nodes[**id].content {
                            Content::View(view) => view.area.left(),
                            Content::Screen(view) => view.area.left(),
                            Content::Container(container) => container.area.left(),
                        };
                        (current_x as i16 - x as i16).abs()
                    })?;
                }
                (_, Layout::Horizontal) => {
                    // find closest split based on y because x is irrelevant
                    // in a horizontal container (and already correct based on previous search)
                    child_id = *container.children.iter().min_by_key(|id| {
                        let y = match &self.nodes[**id].content {
                            Content::View(view) => view.area.top(),
                            Content::Screen(view) => view.area.top(),
                            Content::Container(container) => container.area.top(),
                        };
                        (current_y as i16 - y as i16).abs()
                    })?;
                }
            }
        }
        Some(child_id)
    }

    pub fn prev(&self) -> ViewId {
        // This function is very dumb, but that's because we don't store any parent links.
        // (we'd be able to go parent.prev_sibling() recursively until we find something)
        // For now that's okay though, since it's unlikely you'll be able to open a large enough
        // number of splits to notice.

        let mut views = self
            .traverse()
            .rev()
            .skip_while(|&(id, _view)| id != self.focus)
            .skip(1); // Skip focused value
        if let Some((id, _)) = views.next() {
            id
        } else {
            // extremely crude, take the last item
            let (key, _) = self.traverse().next_back().unwrap();
            key
        }
    }

    pub fn next(&self) -> ViewId {
        // This function is very dumb, but that's because we don't store any parent links.
        // (we'd be able to go parent.next_sibling() recursively until we find something)
        // For now that's okay though, since it's unlikely you'll be able to open a large enough
        // number of splits to notice.

        let mut views = self
            .traverse()
            .skip_while(|&(id, _view)| id != self.focus)
            .skip(1); // Skip focused value
        if let Some((id, _)) = views.next() {
            id
        } else {
            // extremely crude, take the first item again
            let (key, _) = self.traverse().next().unwrap();
            key
        }
    }

    pub fn transpose(&mut self) {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;
        if let Content::Container(container) = &mut self.nodes[parent].content {
            container.layout = match container.layout {
                Layout::Vertical => Layout::Horizontal,
                Layout::Horizontal => Layout::Vertical,
            };
            self.recalculate();
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: Direction) -> Option<()> {
        let focus = self.focus;
        let target = self.find_split_in_direction(focus, direction)?;
        // windows only, text or screens' (this fork)
        self.nodes[target].leaf_area()?;
        let focus_parent = self.nodes[focus].parent;
        let target_parent = self.nodes[target].parent;
        let focus_pos = match &self.nodes[focus_parent].content {
            Content::Container(container) => {
                container.children.iter().position(|&id| id == focus)?
            }
            _ => return None,
        };
        let target_pos = match &self.nodes[target_parent].content {
            Content::Container(container) => {
                container.children.iter().position(|&id| id == target)?
            }
            _ => return None,
        };
        // swap node positions so that traversal order is kept, and the areas with them
        self.container_mut(focus_parent).children[focus_pos] = target;
        self.container_mut(target_parent).children[target_pos] = focus;
        self.nodes[focus].parent = target_parent;
        self.nodes[target].parent = focus_parent;
        self.recalculate();
        Some(())
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    /// Puts `view` along an edge of the whole tree (this fork): at the bottom (`Horizontal`,
    /// `at_end`), top, left (`Vertical`) or right, across everything else, taking `share`
    /// percent of the tree in that direction. Focuses it.
    pub fn split_root(
        &mut self,
        view: impl Into<Leaf>,
        layout: Layout,
        at_end: bool,
        share: Option<u16>,
    ) -> ViewId {
        let root = self.root;
        let mut node = Node::leaf(view.into());
        node.parent = root;
        node.share = share;
        let node = self.insert_leaf(node);

        let children = std::mem::take(&mut self.container_mut(root).children);
        let old_layout = self.container_mut(root).layout;
        let mut kept = if children.len() > 1 && old_layout != layout {
            // what was there goes into a container of its own, which takes its place
            let mut inner = Node::container(old_layout);
            inner.parent = root;
            let inner = self.nodes.insert(inner);
            for &child in &children {
                self.nodes[child].parent = inner;
            }
            self.container_mut(inner).children = children;
            vec![inner]
        } else {
            children
        };
        if at_end {
            kept.push(node);
        } else {
            kept.insert(0, node);
        }
        let container = self.container_mut(root);
        container.layout = layout;
        container.children = kept;
        self.set_focus(node);
        self.recalculate();
        node
    }

    /// Where the node `id` is, to put a window back there with [`Self::insert_at`].
    pub fn place(&self, id: ViewId) -> Place {
        let node = &self.nodes[id];
        let parent = node.parent;
        let (index, layout) = match &self.nodes[parent].content {
            Content::Container(container) => (
                container
                    .children
                    .iter()
                    .position(|&child| child == id)
                    .unwrap_or(0),
                container.layout,
            ),
            Content::View(_) | Content::Screen(_) => (0, Layout::Vertical),
        };
        Place {
            parent,
            index,
            layout,
            share: node.share,
        }
    }

    /// Puts `view` back at `place` and focuses it, if its container is still there with the
    /// same layout; else `None`, and the tree doesn't change.
    pub fn insert_at(&mut self, view: impl Into<Leaf>, place: &Place) -> Option<ViewId> {
        match self.nodes.get(place.parent) {
            Some(Node {
                content: Content::Container(container),
                ..
            }) if container.layout == place.layout => {}
            _ => return None,
        }
        let mut node = Node::leaf(view.into());
        node.parent = place.parent;
        node.share = place.share;
        let node = self.insert_leaf(node);
        let container = self.container_mut(place.parent);
        let index = place.index.min(container.children.len());
        container.children.insert(index, node);
        self.set_focus(node);
        self.recalculate();
        Some(node)
    }

    /// Sets the size of the node `id` in percent of its container (`None`: an equal part).
    pub fn set_share(&mut self, id: ViewId, share: Option<u16>) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.share = share;
            self.recalculate();
        }
    }

    /// The size of the node `id` in percent of its container, if it has one.
    pub fn share(&self, id: ViewId) -> Option<u16> {
        self.nodes.get(id)?.share
    }
}

/// The sizes of children with `shares` in `total` cells: each sized one takes its share, the others what is
/// left in equal parts (`equal` each when none is sized, as upstream). The last one is given
/// what rounding leaves by the caller.
fn sizes(shares: &[Option<u16>], total: u16, equal: u16) -> Vec<u16> {
    if shares.iter().all(Option::is_none) {
        return vec![equal; shares.len()];
    }
    let fixed: Vec<Option<u16>> = shares
        .iter()
        .map(|share| {
            share.map(|percent| ((total as u32 * percent.min(100) as u32 + 50) / 100) as u16)
        })
        .collect();
    let used: u16 = fixed.iter().flatten().sum();
    let flexible = fixed.iter().filter(|size| size.is_none()).count() as u16;
    let rest = total.saturating_sub(used);
    let each = rest.checked_div(flexible).unwrap_or(0);
    fixed
        .into_iter()
        .map(|size| size.unwrap_or(each).max(1))
        .collect()
}

#[derive(Debug)]
pub struct Traverse<'a> {
    tree: &'a Tree,
    stack: Vec<ViewId>, // TODO: reuse the one we use on update
}

impl<'a> Traverse<'a> {
    fn new(tree: &'a Tree) -> Self {
        Self {
            tree,
            stack: vec![tree.root],
        }
    }
}

/// The windows shown, in the order of the layout (this fork: text and screens' windows).
impl<'a> Iterator for Traverse<'a> {
    type Item = (ViewId, Window<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.stack.pop()?;

            let node = &self.tree.nodes[key];

            match &node.content {
                Content::View(view) => return Some((key, Window::Text(view))),
                Content::Screen(view) => return Some((key, Window::Screen(view))),
                Content::Container(container) => {
                    self.stack.extend(container.children.iter().rev());
                }
            }
        }
    }
}

impl DoubleEndedIterator for Traverse<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.stack.pop()?;

            let node = &self.tree.nodes[key];

            match &node.content {
                Content::View(view) => return Some((key, Window::Text(view))),
                Content::Screen(view) => return Some((key, Window::Screen(view))),
                Content::Container(container) => {
                    self.stack.extend(container.children.iter());
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::editor::GutterConfig;
    use crate::DocumentId;

    #[test]
    fn find_split_in_direction() {
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 80,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let l0 = tree.focus;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let r0 = tree.focus;

        tree.focus = l0;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Horizontal);
        let l1 = tree.focus;

        tree.focus = l0;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        // Tree in test
        // | L0  | L2 |    |
        // |    L1    | R0 |
        let l2 = tree.focus;
        assert_eq!(Some(l0), tree.find_split_in_direction(l2, Direction::Left));
        assert_eq!(Some(l1), tree.find_split_in_direction(l2, Direction::Down));
        assert_eq!(Some(r0), tree.find_split_in_direction(l2, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(l2, Direction::Up));

        tree.focus = l1;
        assert_eq!(None, tree.find_split_in_direction(l1, Direction::Left));
        assert_eq!(None, tree.find_split_in_direction(l1, Direction::Down));
        assert_eq!(Some(r0), tree.find_split_in_direction(l1, Direction::Right));
        assert_eq!(Some(l0), tree.find_split_in_direction(l1, Direction::Up));

        tree.focus = l0;
        assert_eq!(None, tree.find_split_in_direction(l0, Direction::Left));
        assert_eq!(Some(l1), tree.find_split_in_direction(l0, Direction::Down));
        assert_eq!(Some(l2), tree.find_split_in_direction(l0, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(l0, Direction::Up));

        tree.focus = r0;
        assert_eq!(Some(l2), tree.find_split_in_direction(r0, Direction::Left));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Down));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Up));
    }

    #[test]
    fn swap_split_in_direction() {
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 80,
        });

        let doc_l0 = DocumentId::default();
        let mut view = View::new(doc_l0, GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let l0 = tree.focus;

        let doc_r0 = DocumentId::default();
        let view = View::new(doc_r0, GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let r0 = tree.focus;

        tree.focus = l0;

        let doc_l1 = DocumentId::default();
        let view = View::new(doc_l1, GutterConfig::default());
        tree.split(view, Layout::Horizontal);
        let l1 = tree.focus;

        tree.focus = l0;

        let doc_l2 = DocumentId::default();
        let view = View::new(doc_l2, GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let l2 = tree.focus;

        // Views in test
        // | L0  | L2 |    |
        // |    L1    | R0 |

        // Document IDs in test
        // | l0  | l2 |    |
        // |    l1    | r0 |

        fn doc_id(tree: &Tree, view_id: ViewId) -> Option<DocumentId> {
            if let Content::View(view) = &tree.nodes[view_id].content {
                Some(view.doc)
            } else {
                None
            }
        }

        tree.focus = l0;
        // `*` marks the view in focus from view table (here L0)
        // | l0*  | l2 |    |
        // |    l1     | r0 |
        tree.swap_split_in_direction(Direction::Down);
        // | l1   | l2 |    |
        // |    l0*    | r0 |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_l0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_r0));

        tree.swap_split_in_direction(Direction::Right);

        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        // cannot swap, nothing changes
        tree.swap_split_in_direction(Direction::Up);
        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        // cannot swap, nothing changes
        tree.swap_split_in_direction(Direction::Down);
        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        tree.focus = l2;
        // | l1  | l2* |    |
        // |    r0     | l0 |

        tree.swap_split_in_direction(Direction::Down);
        // | l1  | r0  |    |
        // |    l2*    | l0 |
        assert_eq!(tree.focus, l2);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_l2));
        assert_eq!(doc_id(&tree, l2), Some(doc_r0));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        tree.swap_split_in_direction(Direction::Up);
        // | l2* | r0 |    |
        // |    l1    | l0 |
        assert_eq!(tree.focus, l2);
        assert_eq!(doc_id(&tree, l0), Some(doc_l2));
        assert_eq!(doc_id(&tree, l1), Some(doc_l1));
        assert_eq!(doc_id(&tree, l2), Some(doc_r0));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));
    }

    #[test]
    fn all_vertical_views_have_same_width() {
        let tree_area_width = 180;
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: tree_area_width,
            height: 80,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Horizontal);

        tree.remove(tree.focus);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        // Make sure that we only have one level in the tree.
        assert_eq!(3, tree.views().count());
        assert_eq!(
            vec![
                tree_area_width / 3 - 1, // gap here
                tree_area_width / 3 - 1, // gap here
                tree_area_width / 3
            ],
            tree.views()
                .map(|(view, _)| view.area.width)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn vsplit_gap_rounding() {
        let (tree_area_width, tree_area_height) = (80, 24);
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: tree_area_width,
            height: tree_area_height,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, tree_area_width, tree_area_height);
        tree.insert(view);

        for _ in 0..9 {
            let view = View::new(DocumentId::default(), GutterConfig::default());
            tree.split(view, Layout::Vertical);
        }

        assert_eq!(10, tree.views().count());
        assert_eq!(
            std::iter::repeat_n(7, 9)
                .chain(Some(8)) // Rounding in `recalculate`.
                .collect::<Vec<_>>(),
            tree.views()
                .map(|(view, _)| view.area.width)
                .collect::<Vec<_>>()
        );
    }

    fn view() -> View {
        View::new(DocumentId::default(), GutterConfig::default())
    }

    fn tree(width: u16, height: u16) -> Tree {
        let mut tree = Tree::new(Rect::new(0, 0, width, height));
        tree.insert(view());
        tree
    }

    #[test]
    fn a_sized_window_at_the_bottom_across_side_by_side_splits() {
        let mut tree = tree(100, 30);
        let left = tree.focus;
        let right = tree.split(view(), Layout::Vertical);
        let bottom = tree.split_root(view(), Layout::Horizontal, true, Some(33));
        assert_eq!(tree.focus, bottom);
        assert_eq!(tree.get(bottom).area, Rect::new(0, 20, 100, 10));
        assert_eq!(tree.get(left).area.height, 20);
        assert_eq!(tree.get(right).area.height, 20);
        assert_eq!(tree.get(left).area.y, 0);
        // a split of the windows above shares what is left
        tree.focus = left;
        let third = tree.split(view(), Layout::Horizontal);
        assert_eq!(tree.get(bottom).area, Rect::new(0, 20, 100, 10));
        assert_eq!(tree.get(left).area.height + tree.get(third).area.height, 20);
    }

    #[test]
    fn sized_windows_on_each_side() {
        let mut tree = tree(100, 30);
        let main = tree.focus;
        let top = tree.split_root(view(), Layout::Horizontal, false, Some(20));
        assert_eq!(tree.get(top).area, Rect::new(0, 0, 100, 6));
        assert_eq!(tree.get(main).area, Rect::new(0, 6, 100, 24));
        let right = tree.split_root(view(), Layout::Vertical, true, Some(25));
        // what was there (two rows) is a container on the left
        assert_eq!(tree.get(right).area.x + tree.get(right).area.width, 100);
        assert_eq!(tree.get(right).area.height, 30);
        // the separator column between them comes out of the last one
        assert_eq!(tree.get(right).area.width, 24);
        assert_eq!(tree.get(top).area.width, 75);
        let left = tree.split_root(view(), Layout::Vertical, false, Some(10));
        assert_eq!(tree.get(left).area.x, 0);
        assert_eq!(tree.get(left).area.width, 10);
    }

    #[test]
    fn a_cover_takes_the_window_place_and_gives_it_back() {
        let mut tree = tree(100, 30);
        let main = tree.focus;
        let other = tree.split(view(), Layout::Vertical);
        tree.focus = main;
        let area = tree.get(main).area;
        let screen = tree.cover(main, ScreenView::new(ScreenId::default()));
        // the cover has the window's place, size and focus; the text window is parked
        assert_eq!(tree.focus, screen);
        assert_eq!(tree.screen_view(screen).unwrap().area, area);
        assert!(tree.is_parked(main));
        assert!(!tree.is_visible(main));
        assert_eq!(tree.views().count(), 1);
        assert_eq!(tree.windows().count(), 2);
        assert_eq!(tree.under(screen), Some(main));
        assert_eq!(tree.top_of(main), screen);
        // text commands act on the window the screen covers
        assert_eq!(tree.text_focus(), main);
        // moving to the other window and back goes through the cover
        assert_eq!(tree.next(), other);
        assert_eq!(tree.uncover(screen), Some(main));
        assert_eq!(tree.focus, main);
        assert!(tree.is_visible(main));
        assert_eq!(tree.get(main).area, area);
        assert!(!tree.contains(screen));
        assert_eq!(tree.uncover(main), None);
    }

    #[test]
    fn covers_stack_and_go_together() {
        let mut tree = tree(100, 30);
        let main = tree.focus;
        let page = tree.cover(main, ScreenView::new(ScreenId::default()));
        let diff = tree.cover(page, ScreenView::new(ScreenId::default()));
        assert_eq!(tree.chain(diff), [diff, page, main]);
        assert_eq!(tree.text_focus(), main);
        assert_eq!(tree.uncover(diff), Some(page));
        assert_eq!(tree.focus, page);
        assert_eq!(tree.chain(page), [page, main]);
        // removing a window removes what it covers
        let other = tree.split(view(), Layout::Vertical);
        tree.remove(page);
        assert!(!tree.contains(page));
        assert!(!tree.contains(main));
        assert_eq!(tree.focus, other);
    }

    #[test]
    fn the_last_text_window_stays_for_text_commands() {
        let mut tree = tree(100, 30);
        let main = tree.focus;
        let terminal = tree.split(ScreenView::new(ScreenId::default()), Layout::Horizontal);
        // the screen has the focus: text commands act on the text window that had it last
        assert_eq!(tree.focus, terminal);
        assert_eq!(tree.text_focus(), main);
        tree.park(main);
        assert_eq!(tree.windows().count(), 1);
        assert_eq!(tree.views().count(), 0);
        assert_eq!(tree.text_focus(), main);
        assert_eq!(tree.orphans(), [main]);
        assert_eq!(
            tree.screen_view(terminal).unwrap().area,
            Rect::new(0, 0, 100, 30)
        );
        // a screen's window draws above its statusline
        assert_eq!(
            tree.screen_view(terminal).unwrap().inner_area(),
            Rect::new(0, 0, 100, 29)
        );
    }

    #[test]
    fn windows_of_both_kinds_swap() {
        let mut tree = tree(100, 30);
        let main = tree.focus;
        let screen = tree.split(ScreenView::new(ScreenId::default()), Layout::Vertical);
        let (left, right) = (tree.get(main).area, tree.screen_view(screen).unwrap().area);
        tree.swap_split_in_direction(Direction::Left).unwrap();
        assert_eq!(tree.get(main).area, right);
        assert_eq!(tree.screen_view(screen).unwrap().area, left);
        assert_eq!(tree.focus, screen);
    }

    #[test]
    fn a_window_goes_back_where_it_was() {
        let mut tree = tree(100, 30);
        let main = tree.focus;
        let bottom = tree.split_root(view(), Layout::Horizontal, true, Some(33));
        let place = tree.place(bottom);
        tree.remove(bottom);
        assert_eq!(tree.get(main).area, Rect::new(0, 0, 100, 30));
        let again = tree.insert_at(view(), &place).unwrap();
        assert_eq!(tree.focus, again);
        assert_eq!(tree.get(again).area, Rect::new(0, 20, 100, 10));
        assert_eq!(tree.get(main).area.height, 20);
    }

    #[test]
    fn a_place_that_is_gone() {
        let mut tree = tree(100, 30);
        let a = tree.focus;
        let b = tree.split(view(), Layout::Vertical);
        tree.focus = b;
        let c = tree.split(view(), Layout::Horizontal);
        // c is in a container of its own (b over c)
        let place = tree.place(c);
        tree.remove(c);
        // the container went with it: b took its place
        assert!(tree.insert_at(view(), &place).is_none());
        assert_eq!(tree.views().count(), 2);
        assert!(tree.contains(a));
    }

    #[test]
    fn a_split_window_keeps_its_size_through_its_new_container() {
        let mut tree = tree(100, 30);
        let bottom = tree.split_root(view(), Layout::Horizontal, true, Some(33));
        // splitting the sized window side by side: the pair keeps the size
        let next = tree.split(view(), Layout::Vertical);
        assert_eq!(tree.get(bottom).area.height, 10);
        assert_eq!(tree.get(next).area.height, 10);
        tree.remove(next);
        assert_eq!(tree.share(bottom), Some(33));
        assert_eq!(tree.get(bottom).area.height, 10);
    }
}
