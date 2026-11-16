//! The API browser's data layer (this fork): a tree of everything a language server knows,
//! loaded lazily, one level at a time.
//!
//! Four roots: **Codebase** (the workspace's directories and files, each file's symbols),
//! **Scope** (what completes at the cursor), **Packages** (the dependencies, their modules and
//! items) and **Std** (the roots of the standard library, `api-roots` in `languages.toml`).
//! Each level is loaded by a provider (see [`load`]), chosen by what the server offers and how
//! it behaves, never by its name: document symbols, workspace symbols with path queries,
//! completion at the cursor, completion in temporary documents (`api-probe`), definitions, and
//! tree-sitter tags for an instant skeleton.
//!
//! A [`Browser`] lives as long as the view that shows it. Opening a node starts one task (one
//! request, mostly), asking for an item's docs another; their results come back through a
//! channel that [`Browser::update`] drains. Dropping the browser aborts every task in flight,
//! which cancels their requests (`$/cancelRequest`) and closes their temporary documents, and
//! takes the server's `api-config` overlay away. Nothing outlives it: no cache, nothing on disk,
//! and the user's buffers are never touched (a file the editor has open is read from the server
//! as it is; other files are opened on the server with their text on disk, then closed).

mod load;
mod node;
pub mod picker;
mod tags;
mod temp;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;
use vortex_lsp::{util::pos_to_lsp_pos, Client};
use vortex_view::{doc, view, Editor};

pub use load::Docs;
pub use node::{Kind, Node, Place, Provider, Root};

use load::{Ctx, Loaded, ScopeAt};
use node::Load;

/// How long a temporary document waits for the server to parse it before its request.
const PARSE_WAIT: Duration = Duration::from_millis(1500);

/// An index into the browser's nodes.
pub type NodeId = usize;

/// Something loaded on request.
#[derive(Debug, Clone, Default)]
pub enum State<T> {
    #[default]
    NotAsked,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> State<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// A node's children as loaded.
#[derive(Debug, Clone, Default)]
pub struct Level {
    pub children: Vec<NodeId>,
    /// The server said there is more than it gave.
    pub incomplete: bool,
    /// The language server requests the level took.
    pub requests: usize,
    /// From the opening to the answer.
    pub took: Duration,
    /// These are the tags skeleton; the document symbols are on their way.
    pub skeleton: bool,
}

struct Entry {
    node: Node,
    parent: Option<NodeId>,
    children: State<Level>,
    docs: State<Docs>,
    definition: State<Option<Place>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum What {
    Children,
    Docs,
    Definition,
}

enum Outcome {
    Children(anyhow::Result<Loaded>),
    Docs(anyhow::Result<Docs>),
    Definition(anyhow::Result<Option<Place>>),
}

struct Done {
    id: NodeId,
    outcome: Outcome,
    took: Duration,
}

pub struct Browser {
    ctx: Arc<Ctx>,
    entries: Vec<Entry>,
    roots: Vec<NodeId>,
    sender: UnboundedSender<Done>,
    receiver: UnboundedReceiver<Done>,
    tasks: HashMap<(NodeId, What), (AbortHandle, Instant)>,
    /// The language whose server is browsed.
    pub language: String,
}

impl Browser {
    /// A browser of the current buffer's language server: the first that lists symbols or
    /// completes.
    pub fn new(editor: &Editor) -> anyhow::Result<Self> {
        let (view, doc) = (view!(editor), doc!(editor));
        let config = doc
            .language_config()
            .ok_or_else(|| anyhow!("the buffer has no language"))?;
        let client = doc
            .language_servers()
            .find(|client| {
                let capabilities = client.capabilities();
                capabilities.document_symbol_provider.is_some()
                    || capabilities.workspace_symbol_provider.is_some()
                    || capabilities.completion_provider.is_some()
            })
            .and_then(|client| editor.language_servers.get_by_id(client.id()))
            .cloned()
            .ok_or_else(|| anyhow!("no language server of this buffer lists symbols"))?;
        let loader = editor.syn_loader.load_full();
        let server_config = loader.language_server_configs().get(client.name());
        let api_config = server_config.and_then(|config| config.api_config.clone());
        let params = server_config.and_then(|config| config.api_params.clone());

        let workspace = doc
            .path()
            .map(|path| vortex_loader::find_workspace_in(path).0)
            .unwrap_or_else(|| vortex_loader::find_workspace().0);
        let encoding = client.offset_encoding();
        let scope = doc.uri().map(|_| {
            let text = doc.text();
            let cursor = doc.selection(view.id).primary().cursor(text.slice(..));
            let before = cursor
                .checked_sub(1)
                .map(|at| text.char(at))
                .map(String::from);
            ScopeAt {
                document: doc.identifier(),
                position: pos_to_lsp_pos(text, cursor, encoding),
                trigger: before,
            }
        });
        let extension = config
            .file_types
            .iter()
            .find_map(|file_type| match file_type {
                vortex_core::syntax::config::FileType::Extension(extension) => {
                    Some(extension.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| "txt".to_string());
        let open_in_editor: HashSet<PathBuf> = editor
            .documents()
            .filter_map(|doc| doc.path().map(Path::to_path_buf))
            .collect();

        let ctx = Ctx {
            encoding,
            language_id: doc.language_id().unwrap_or("plaintext").to_string(),
            extension,
            separator: config
                .api_separator
                .clone()
                .unwrap_or_else(|| ".".to_string()),
            roots: config.api_roots.clone(),
            probe: config.api_probe.clone(),
            params,
            workspace,
            hidden: editor.language_servers.hidden_documents.clone(),
            loader,
            open_in_editor,
            scope,
            path_queries: tokio::sync::OnceCell::new(),
            counter: AtomicUsize::new(0),
            parse_wait: PARSE_WAIT,
            client,
        };
        if api_config.is_some() {
            ctx.client.set_config_overlay(api_config);
        }
        Ok(Self::with_ctx(ctx, config.language_id.clone()))
    }

    fn with_ctx(ctx: Ctx, language: String) -> Self {
        let (sender, receiver) = unbounded_channel();
        let mut browser = Self {
            ctx: Arc::new(ctx),
            entries: Vec::new(),
            roots: Vec::new(),
            sender,
            receiver,
            tasks: HashMap::new(),
            language,
        };
        browser.roots = load::roots()
            .into_iter()
            .map(|node| browser.insert(node, None))
            .collect();
        browser
    }

    fn insert(&mut self, node: Node, parent: Option<NodeId>) -> NodeId {
        self.entries.push(Entry {
            node,
            parent,
            children: State::NotAsked,
            docs: State::NotAsked,
            definition: State::NotAsked,
        });
        self.entries.len() - 1
    }

    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.entries[id].node
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.entries[id].parent
    }

    pub fn children(&self, id: NodeId) -> &State<Level> {
        &self.entries[id].children
    }

    pub fn docs(&self, id: NodeId) -> &State<Docs> {
        &self.entries[id].docs
    }

    pub fn definition(&self, id: NodeId) -> &State<Option<Place>> {
        &self.entries[id].definition
    }

    pub fn server(&self) -> &str {
        self.ctx.client.name()
    }

    pub fn separator(&self) -> &str {
        &self.ctx.separator
    }

    /// The requests in flight.
    pub fn in_flight(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the server answers path queries, once a level found out.
    pub fn path_queries(&self) -> Option<bool> {
        self.ctx.path_queries.get().copied()
    }

    /// A node's path for people: directories and files with `/`, the items in a file or a
    /// package with the language's separator (`std::collections::HashMap::insert`).
    pub fn display_path(&self, id: NodeId) -> String {
        let mut chain = Vec::new();
        let mut at = Some(id);
        while let Some(id) = at {
            let entry = &self.entries[id];
            if entry.node.kind != Kind::Root {
                chain.push(&entry.node);
            }
            at = entry.parent;
        }
        chain.reverse();
        let mut path = String::new();
        for (index, node) in chain.iter().enumerate() {
            if index > 0 {
                let previous = chain[index - 1];
                path.push_str(match (previous.kind, previous.provider) {
                    (Kind::Directory, Provider::Files) => "/",
                    // A directory of packages ends with its `/`.
                    (Kind::Directory, _) => "",
                    (Kind::File, Provider::Files) => ": ",
                    _ => &self.ctx.separator,
                });
            }
            path.push_str(&node.name);
        }
        path
    }

    /// Opens a node: loads its children unless they are loaded or loading. Children known with
    /// the node, a directory's entries and the roots come at once; a file shows its tags
    /// skeleton at once while its document symbols load; everything else is one task.
    pub fn open(&mut self, id: NodeId) {
        if matches!(self.entries[id].children, State::Loading | State::Ready(_)) {
            return;
        }
        let node = self.entries[id].node.clone();
        match &node.load {
            Load::Nothing => self.set_children(id, Loaded::default(), Duration::ZERO, false),
            Load::Given(children) => {
                let loaded = Loaded {
                    nodes: children.clone(),
                    ..Default::default()
                };
                self.set_children(id, loaded, Duration::ZERO, false);
            }
            Load::Root(Root::Codebase) | Load::Dir(_) => {
                let started = Instant::now();
                let dir = match &node.load {
                    Load::Dir(dir) => dir.clone(),
                    _ => self.ctx.workspace.clone(),
                };
                match load::directory(&self.ctx, &dir) {
                    Ok(loaded) => self.set_children(id, loaded, started.elapsed(), false),
                    Err(err) => self.entries[id].children = State::Failed(err.to_string()),
                }
            }
            Load::File(path) => {
                let started = Instant::now();
                let skeleton = tags::skeleton(&self.ctx.loader, path, None, self.ctx.encoding);
                if !self.ctx.supports_document_symbols() {
                    let loaded = Loaded {
                        nodes: skeleton.unwrap_or_default(),
                        ..Default::default()
                    };
                    self.set_children(id, loaded, started.elapsed(), false);
                    return;
                }
                match skeleton {
                    Some(nodes) if !nodes.is_empty() => {
                        let loaded = Loaded {
                            nodes,
                            ..Default::default()
                        };
                        self.set_children(id, loaded, started.elapsed(), true);
                    }
                    _ => self.entries[id].children = State::Loading,
                }
                self.spawn(id, What::Children, load::children(self.ctx.clone(), node));
            }
            _ => {
                self.entries[id].children = State::Loading;
                self.spawn(id, What::Children, load::children(self.ctx.clone(), node));
            }
        }
    }

    /// Asks for a node's docs, and cancels the docs asked for any other node: only the
    /// selected item's are wanted.
    pub fn ask_docs(&mut self, id: NodeId) {
        let others: Vec<_> = self
            .tasks
            .keys()
            .filter(|(node, what)| *what == What::Docs && *node != id)
            .copied()
            .collect();
        for key in others {
            if let Some((task, _)) = self.tasks.remove(&key) {
                task.abort();
            }
            self.entries[key.0].docs = State::NotAsked;
        }
        if !matches!(self.entries[id].docs, State::NotAsked) {
            return;
        }
        let node = self.entries[id].node.clone();
        if node.kind == Kind::Root || node.kind == Kind::Directory {
            self.entries[id].docs = State::Ready(Docs::default());
            return;
        }
        self.entries[id].docs = State::Loading;
        self.spawn(id, What::Docs, load::docs(self.ctx.clone(), node));
    }

    /// Asks where a node is defined (known at once for most).
    pub fn ask_definition(&mut self, id: NodeId) {
        if !matches!(self.entries[id].definition, State::NotAsked) {
            return;
        }
        let node = self.entries[id].node.clone();
        if let Some(place) = node.place.clone() {
            self.entries[id].definition = State::Ready(Some(place));
            return;
        }
        if node.probe.is_none() {
            self.entries[id].definition = State::Ready(None);
            return;
        }
        self.entries[id].definition = State::Loading;
        self.spawn(
            id,
            What::Definition,
            load::definition(self.ctx.clone(), node),
        );
    }

    fn spawn<T: Send + 'static>(
        &mut self,
        id: NodeId,
        what: What,
        future: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    ) where
        Outcome: From<(What, anyhow::Result<T>)>,
    {
        let sender = self.sender.clone();
        let started = Instant::now();
        let task = tokio::spawn(async move {
            let result = future.await;
            let _ = sender.send(Done {
                id,
                outcome: Outcome::from((what, result)),
                took: started.elapsed(),
            });
            vortex_event::request_redraw();
        });
        if let Some((previous, _)) = self
            .tasks
            .insert((id, what), (task.abort_handle(), started))
        {
            previous.abort();
        }
    }

    /// Takes in what the tasks finished. Returns whether anything changed.
    pub fn update(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.receiver.try_recv() {
            changed = true;
            match done.outcome {
                Outcome::Children(result) => {
                    self.tasks.remove(&(done.id, What::Children));
                    match result {
                        Ok(loaded) => self.set_children(done.id, loaded, done.took, false),
                        Err(err) => {
                            // A skeleton stays when the symbols fail.
                            let skeleton = matches!(
                                &self.entries[done.id].children,
                                State::Ready(level) if level.skeleton
                            );
                            if skeleton {
                                if let State::Ready(level) = &mut self.entries[done.id].children {
                                    level.skeleton = false;
                                }
                            } else {
                                self.entries[done.id].children = State::Failed(format!("{err:#}"));
                            }
                        }
                    }
                }
                Outcome::Docs(result) => {
                    self.tasks.remove(&(done.id, What::Docs));
                    self.entries[done.id].docs = match result {
                        Ok(docs) => State::Ready(docs),
                        Err(err) => State::Failed(format!("{err:#}")),
                    };
                }
                Outcome::Definition(result) => {
                    self.tasks.remove(&(done.id, What::Definition));
                    self.entries[done.id].definition = match result {
                        Ok(place) => State::Ready(place),
                        Err(err) => State::Failed(format!("{err:#}")),
                    };
                }
            }
        }
        changed
    }

    fn set_children(&mut self, id: NodeId, loaded: Loaded, took: Duration, skeleton: bool) {
        let parent_path = self.entries[id].node.path.clone();
        let children = loaded
            .nodes
            .into_iter()
            .map(|mut node| {
                node.path = parent_path
                    .iter()
                    .cloned()
                    .chain([node.name.clone()])
                    .collect();
                self.insert(node, Some(id))
            })
            .collect();
        self.entries[id].children = State::Ready(Level {
            children,
            incomplete: loaded.incomplete,
            requests: loaded.requests,
            took,
            skeleton,
        });
    }

    /// Where the workspace is.
    pub fn workspace(&self) -> &Path {
        &self.ctx.workspace
    }

    /// Waits for the tasks in flight, for tests and the measurements of the report.
    #[doc(hidden)]
    pub async fn settle(&mut self, limit: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + limit;
        while !self.tasks.is_empty() {
            if Instant::now() > deadline {
                bail!("{} requests still in flight", self.tasks.len());
            }
            match tokio::time::timeout(Duration::from_millis(20), self.receiver.recv()).await {
                Ok(Some(done)) => {
                    // Put it back through `update`.
                    let _ = self.sender.send(done);
                    self.update();
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        self.update();
        Ok(())
    }

    /// The client browsed, for tests.
    #[doc(hidden)]
    pub fn client(&self) -> &Arc<Client> {
        &self.ctx.client
    }
}

impl From<(What, anyhow::Result<Loaded>)> for Outcome {
    fn from((_, result): (What, anyhow::Result<Loaded>)) -> Self {
        Outcome::Children(result)
    }
}

impl From<(What, anyhow::Result<Docs>)> for Outcome {
    fn from((_, result): (What, anyhow::Result<Docs>)) -> Self {
        Outcome::Docs(result)
    }
}

impl From<(What, anyhow::Result<Option<Place>>)> for Outcome {
    fn from((_, result): (What, anyhow::Result<Option<Place>>)) -> Self {
        Outcome::Definition(result)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Aborting drops each task's future: its request is cancelled and its temporary
        // document closed (`TempDocument`'s drop).
        for (_, (task, _)) in self.tasks.drain() {
            task.abort();
        }
        self.ctx.client.set_config_overlay(None);
    }
}
