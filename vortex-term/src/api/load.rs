//! The providers: the requests that load a level of the tree, an item's docs and its definition.
//! Each runs as a task of the browser ([`super::Browser`]); dropping it (closing the browser)
//! cancels its request (`$/cancelRequest`) and closes its temporary document.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use serde_json::Value;
use vortex_core::syntax::config::ApiProbe;
use vortex_core::syntax::Loader;
use vortex_core::Rope;
use vortex_lsp::{lsp, util::pos_to_lsp_pos, Client, HiddenDocuments, OffsetEncoding};

use super::node::{completion_name, documentation_text, Kind, Load, Node, Place, Provider, Root};
use super::temp::TempDocument;

/// What the browser knows about its server and language, shared with its tasks.
pub(crate) struct Ctx {
    pub client: Arc<Client>,
    pub encoding: OffsetEncoding,
    /// The language id sent with temporary documents.
    pub language_id: String,
    /// The extension of temporary documents.
    pub extension: String,
    pub separator: String,
    pub roots: Vec<String>,
    pub probe: Option<ApiProbe>,
    /// Fields merged into workspace symbol and completion requests (`api-params`).
    pub params: Option<Value>,
    pub workspace: PathBuf,
    pub hidden: HiddenDocuments,
    pub loader: Arc<Loader>,
    /// The files open in the editor: never opened again by the browser, their text is the
    /// buffer's.
    pub open_in_editor: HashSet<PathBuf>,
    /// The cursor when the browser opened, for the scope view.
    pub scope: Option<ScopeAt>,
    /// Whether the server lists a module's items for a path query, found out with the first
    /// level that needs it.
    pub path_queries: tokio::sync::OnceCell<bool>,
    /// Numbers the temporary documents.
    pub counter: AtomicUsize,
    /// How long a temporary document waits for its first diagnostics (the server has parsed it)
    /// before its request.
    pub parse_wait: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct ScopeAt {
    pub document: lsp::TextDocumentIdentifier,
    pub position: lsp::Position,
    /// The character before the cursor, when it is one of the server's trigger characters.
    pub trigger: Option<String>,
}

/// A level of the tree as loaded.
#[derive(Debug, Default)]
pub(crate) struct Loaded {
    pub nodes: Vec<Node>,
    /// The server said its list is incomplete (`isIncomplete`), or it reached a known limit.
    pub incomplete: bool,
    /// The language server requests it took.
    pub requests: usize,
}

/// The docs of an item.
#[derive(Debug, Clone, Default)]
pub struct Docs {
    pub signature: Option<String>,
    /// Markdown.
    pub text: Option<String>,
    /// The request that gave them.
    pub source: &'static str,
}

// ------------------------------------------------------------------------------------------------
// Raw requests, so that `api-params` can add fields.

enum RawCompletion {}
impl lsp::request::Request for RawCompletion {
    type Params = Value;
    type Result = Option<lsp::CompletionResponse>;
    const METHOD: &'static str = "textDocument/completion";
}

enum RawWorkspaceSymbol {}
impl lsp::request::Request for RawWorkspaceSymbol {
    type Params = Value;
    type Result = Option<lsp::WorkspaceSymbolResponse>;
    const METHOD: &'static str = "workspace/symbol";
}

/// Merges `extra` into the object `params`.
fn with_params(mut params: Value, extra: Option<&Value>) -> Value {
    if let (Value::Object(params), Some(Value::Object(extra))) = (&mut params, extra) {
        for (key, value) in extra {
            params.insert(key.clone(), value.clone());
        }
    }
    params
}

impl Ctx {
    fn capabilities(&self) -> &lsp::ServerCapabilities {
        self.client.capabilities()
    }

    pub fn supports_document_symbols(&self) -> bool {
        matches!(
            self.capabilities().document_symbol_provider,
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_))
        )
    }

    pub fn supports_workspace_symbols(&self) -> bool {
        matches!(
            self.capabilities().workspace_symbol_provider,
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_))
        )
    }

    pub fn supports_completion(&self) -> bool {
        self.capabilities().completion_provider.is_some()
    }

    pub fn supports_resolve(&self) -> bool {
        self.capabilities()
            .completion_provider
            .as_ref()
            .and_then(|options| options.resolve_provider)
            .unwrap_or(false)
    }

    pub fn supports_hover(&self) -> bool {
        matches!(
            self.capabilities().hover_provider,
            Some(
                lsp::HoverProviderCapability::Simple(true)
                    | lsp::HoverProviderCapability::Options(_)
            )
        )
    }

    pub fn supports_definition(&self) -> bool {
        matches!(
            self.capabilities().definition_provider,
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_))
        )
    }

    /// The trigger kind to send for a completion right after `before`: a trigger character when
    /// the server lists it as one, or, in a temporary document (`probe`), when it is
    /// punctuation (gopls completes import paths after `"` only so, without listing `"`).
    fn context(&self, before: Option<char>, probe: bool) -> lsp::CompletionContext {
        let trigger = before
            .filter(|before| !probe || before.is_ascii_punctuation())
            .map(String::from)
            .filter(|before| {
                probe
                    || self
                        .capabilities()
                        .completion_provider
                        .as_ref()
                        .and_then(|options| options.trigger_characters.as_ref())
                        .is_some_and(|triggers| triggers.contains(before))
            });
        match trigger {
            Some(character) => lsp::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(character),
            },
            None => lsp::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
        }
    }

    async fn completion(
        &self,
        document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        context: lsp::CompletionContext,
    ) -> anyhow::Result<(Vec<lsp::CompletionItem>, bool)> {
        let params = serde_json::to_value(lsp::CompletionParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document: document,
                position,
            },
            context: Some(context),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })?;
        let params = with_params(params, self.params.as_ref());
        let response = self
            .client
            .cancellable_call::<RawCompletion>(&params)
            .await?;
        Ok(match response {
            None => (Vec::new(), false),
            Some(lsp::CompletionResponse::Array(items)) => (items, false),
            Some(lsp::CompletionResponse::List(list)) => (list.items, list.is_incomplete),
        })
    }

    async fn workspace_symbols(&self, query: &str) -> anyhow::Result<Vec<Symbol>> {
        let params = serde_json::to_value(lsp::WorkspaceSymbolParams {
            query: query.to_string(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })?;
        let params = with_params(params, self.params.as_ref());
        let response = self
            .client
            .cancellable_call::<RawWorkspaceSymbol>(&params)
            .await?;
        Ok(match response {
            None => Vec::new(),
            Some(lsp::WorkspaceSymbolResponse::Flat(symbols)) => symbols
                .into_iter()
                .map(|symbol| Symbol {
                    name: symbol.name,
                    kind: symbol.kind,
                    container: symbol.container_name,
                    location: Some(symbol.location),
                })
                .collect(),
            Some(lsp::WorkspaceSymbolResponse::Nested(symbols)) => symbols
                .into_iter()
                .map(|symbol| Symbol {
                    name: symbol.name,
                    kind: symbol.kind,
                    container: symbol.container_name,
                    location: match symbol.location {
                        lsp::OneOf::Left(location) => Some(location),
                        lsp::OneOf::Right(_) => None,
                    },
                })
                .collect(),
        })
    }

    async fn document_symbols(
        &self,
        uri: &lsp::Url,
    ) -> anyhow::Result<lsp::DocumentSymbolResponse> {
        let params = lsp::DocumentSymbolParams {
            text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let response = self
            .client
            .cancellable_call::<lsp::request::DocumentSymbolRequest>(&params)
            .await?;
        Ok(response.unwrap_or(lsp::DocumentSymbolResponse::Flat(Vec::new())))
    }

    async fn definition(
        &self,
        uri: &lsp::Url,
        position: lsp::Position,
    ) -> anyhow::Result<Option<lsp::Location>> {
        let params = lsp::GotoDefinitionParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let response = self
            .client
            .cancellable_call::<lsp::request::GotoDefinition>(&params)
            .await?;
        Ok(match response {
            None => None,
            Some(lsp::GotoDefinitionResponse::Scalar(location)) => Some(location),
            Some(lsp::GotoDefinitionResponse::Array(locations)) => locations.into_iter().next(),
            Some(lsp::GotoDefinitionResponse::Link(links)) => {
                links.into_iter().next().map(|link| lsp::Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
            }
        })
    }

    async fn hover(
        &self,
        uri: &lsp::Url,
        position: lsp::Position,
    ) -> anyhow::Result<Option<String>> {
        let params = lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: Default::default(),
        };
        let response = self
            .client
            .cancellable_call::<lsp::request::HoverRequest>(&params)
            .await?;
        Ok(response.map(|hover| match hover.contents {
            lsp::HoverContents::Scalar(text) => marked_string(text),
            lsp::HoverContents::Array(texts) => texts
                .into_iter()
                .map(marked_string)
                .collect::<Vec<_>>()
                .join("\n\n"),
            lsp::HoverContents::Markup(content) => content.value,
        }))
    }

    /// Opens a real file on the server, unless the editor has it open: then the server already
    /// has the buffer, which is never touched.
    async fn open_real(&self, path: &Path) -> anyhow::Result<(lsp::Url, Option<TempDocument>)> {
        let uri = lsp::Url::from_file_path(path).map_err(|_| anyhow!("not a file path"))?;
        if self.open_in_editor.contains(path) {
            return Ok((uri, None));
        }
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let language_id = self
            .loader
            .language_for_filename(path)
            .map(|language| {
                let config = self.loader.language(language).config();
                config
                    .language_server_language_id
                    .clone()
                    .unwrap_or_else(|| config.language_id.clone())
            })
            .unwrap_or_else(|| self.language_id.clone());
        let document =
            TempDocument::open(&self.client, &self.hidden, uri.clone(), language_id, &text);
        Ok((uri, Some(document)))
    }

    /// Opens a temporary document from a template, and gives its position at the marker
    /// (`{cursor}` or `{definition}`), with the character before it.
    fn open_probe(&self, template: &str, module: &str) -> anyhow::Result<Probe> {
        let filled = template.replace("{module}", module);
        let (marker, at) = ["{cursor}", "{definition}"]
            .iter()
            .find_map(|marker| filled.find(marker).map(|at| (*marker, at)))
            .ok_or_else(|| anyhow!("the template has no {{cursor}} or {{definition}}"))?;
        let text = format!("{}{}", &filled[..at], &filled[at + marker.len()..]);
        let rope = Rope::from(text.as_str());
        let position = pos_to_lsp_pos(&rope, rope.byte_to_char(at), self.encoding);
        let before = text[..at].chars().next_back();

        let number = self.counter.fetch_add(1, Ordering::Relaxed);
        let dir = match self.probe.as_ref().and_then(|probe| probe.dir.as_deref()) {
            Some(dir) => self.workspace.join(dir),
            None => self.workspace.clone(),
        };
        let path = dir.join(format!(
            "vortex_api_probe_{}_{number}.{}",
            std::process::id(),
            self.extension
        ));
        let uri = lsp::Url::from_file_path(&path).map_err(|_| anyhow!("not a file path"))?;
        let document = TempDocument::open(
            &self.client,
            &self.hidden,
            uri.clone(),
            self.language_id.clone(),
            &text,
        );
        Ok(Probe {
            document,
            uri,
            position,
            before,
            definition: marker == "{definition}",
        })
    }

    /// Waits (a little) for the server to parse a temporary document, and says whether it was
    /// still to be parsed.
    async fn parsed(&self, uri: &lsp::Url) -> bool {
        if self.hidden.is_parsed(uri) {
            return false;
        }
        let _ = tokio::time::timeout(self.parse_wait, self.hidden.parsed(uri)).await;
        true
    }

    /// Completes in a temporary document. Asked at once; when the answer is empty or said to be
    /// incomplete before the server parsed the document (clangd answers from its index until
    /// then), asked again once it has (its first diagnostics), or after [`Ctx::parse_wait`].
    ///
    /// With `packages`, an answer without a single module, file or folder is not one either
    /// (clangd's index gives identifiers where `#include <` wants headers).
    async fn probe_completion(
        &self,
        probe: &Probe,
        packages: bool,
    ) -> anyhow::Result<(Vec<lsp::CompletionItem>, bool, usize)> {
        let ask = || {
            self.completion(
                lsp::TextDocumentIdentifier {
                    uri: probe.uri.clone(),
                },
                probe.position,
                self.context(probe.before, true),
            )
        };
        let wrong = |items: &[lsp::CompletionItem]| {
            packages
                && !items.iter().any(|item| {
                    matches!(
                        item.kind,
                        Some(
                            lsp::CompletionItemKind::MODULE
                                | lsp::CompletionItemKind::FILE
                                | lsp::CompletionItemKind::FOLDER
                        )
                    )
                })
        };
        let parsed_before = self.hidden.is_parsed(&probe.uri);
        let (mut items, mut incomplete) = ask().await?;
        let mut requests = 1;
        for round in 0..3 {
            let unsure = incomplete && round == 0 && !parsed_before;
            if !(items.is_empty() || wrong(&items) || unsure) {
                break;
            }
            // An empty or wrong answer right after the document opened can also be a race
            // (gopls, clangd): asked again in a moment, parsed or not.
            if !self.parsed(&probe.uri).await {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            (items, incomplete) = ask().await?;
            requests += 1;
        }
        Ok((items, incomplete, requests))
    }

    /// A definition in a temporary document, asked again once it is parsed when there was none.
    async fn probe_definition(
        &self,
        probe: &Probe,
    ) -> anyhow::Result<(Option<lsp::Location>, usize)> {
        let location = self.definition(&probe.uri, probe.position).await?;
        if location.is_none() && self.parsed(&probe.uri).await {
            return Ok((self.definition(&probe.uri, probe.position).await?, 2));
        }
        Ok((location, 1))
    }

    /// A hover in a temporary document, asked again once it is parsed when there was none.
    async fn probe_hover(&self, probe: &Probe) -> anyhow::Result<Option<String>> {
        let text = self.hover(&probe.uri, probe.position).await?;
        if text.as_deref().is_none_or(str::is_empty) && self.parsed(&probe.uri).await {
            return self.hover(&probe.uri, probe.position).await;
        }
        Ok(text)
    }

    fn template(&self, std: bool) -> Option<&str> {
        let probe = self.probe.as_ref()?;
        if std {
            probe.std.as_deref().or(probe.members.as_deref())
        } else {
            probe.members.as_deref()
        }
    }

    /// The members template with `chain` (the members the listing is in) before the cursor
    /// marker (`m.Router.{cursor}`), and `last` after it: the marker then sits on the name of
    /// that member (`m.{cursor}Router`), to ask for its definition or its hover.
    fn member_template(&self, std: bool, chain: &[String], last: Option<&str>) -> Option<String> {
        let template = self.template(std)?;
        let mut prefix = String::new();
        for member in chain {
            prefix.push_str(member);
            prefix.push_str(&self.separator);
        }
        let marker = if template.contains("{cursor}") {
            "{cursor}"
        } else {
            "{definition}"
        };
        let last = last.unwrap_or_default();
        Some(template.replace(marker, &format!("{prefix}{marker}{last}")))
    }
}

fn marked_string(text: lsp::MarkedString) -> String {
    match text {
        lsp::MarkedString::String(text) => text,
        lsp::MarkedString::LanguageString(code) => {
            format!("```{}\n{}\n```", code.language, code.value)
        }
    }
}

/// A workspace symbol, of either shape.
struct Symbol {
    name: String,
    kind: lsp::SymbolKind,
    container: Option<String>,
    location: Option<lsp::Location>,
}

/// An open temporary document and where to ask in it.
struct Probe {
    document: TempDocument,
    uri: lsp::Url,
    position: lsp::Position,
    before: Option<char>,
    /// Go to the definition at the marker instead of completing.
    definition: bool,
}

// ------------------------------------------------------------------------------------------------
// Levels.

/// Loads the children of a node.
pub(crate) async fn children(ctx: Arc<Ctx>, node: Node) -> anyhow::Result<Loaded> {
    Ok(match node.load.clone() {
        Load::Nothing => Loaded::default(),
        Load::Given(nodes) => Loaded {
            nodes,
            ..Default::default()
        },
        Load::Root(Root::Codebase) => directory(&ctx, &ctx.workspace.clone())?,
        Load::Dir(dir) => directory(&ctx, &dir)?,
        Load::Root(Root::Scope) => scope(&ctx).await?,
        Load::Root(Root::Packages) => packages(&ctx).await?,
        Load::Root(Root::Std) => std_roots(&ctx).await?,
        Load::File(file) => file_symbols(&ctx, &file).await?,
        Load::PathQuery(query) => path_query(&ctx, &query).await?,
        Load::Packages(prefix) => probe_packages(&ctx, &prefix).await?,
        Load::Members { module, chain, std } => members(&ctx, &module, &chain, std).await?,
        Load::TypeMembers => type_members(&ctx, &node).await?,
    })
}

/// The roots of the tree.
pub(crate) fn roots() -> Vec<Node> {
    Root::ALL
        .into_iter()
        .map(|root| {
            let mut node = Node::new(root.name(), Kind::Root, Provider::Browser);
            node.load = Load::Root(root);
            node
        })
        .collect()
}

/// The directories and the files of the server's languages in `dir`, with the ignore files
/// applied like the file picker.
pub(crate) fn directory(ctx: &Ctx, dir: &Path) -> anyhow::Result<Loaded> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let walk = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(true)
        .require_git(false)
        .build();
    let server = ctx.client.name();
    for entry in walk.flatten() {
        let path = entry.path();
        if path == dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            let mut node = Node::new(name, Kind::Directory, Provider::Files);
            node.load = Load::Dir(path.to_path_buf());
            dirs.push(node);
            continue;
        }
        let served = ctx
            .loader
            .language_for_filename(path)
            .is_some_and(|language| {
                ctx.loader
                    .language(language)
                    .config()
                    .language_servers
                    .iter()
                    .any(|features| features.name == server)
            });
        if served {
            let mut node = Node::new(name, Kind::File, Provider::Files);
            node.load = Load::File(path.to_path_buf());
            node.place = lsp::Url::from_file_path(path).ok().map(|uri| Place {
                uri,
                range: lsp::Range::default(),
                encoding: ctx.encoding,
            });
            files.push(node);
        }
    }
    dirs.sort_by_key(|node| node.name.to_lowercase());
    files.sort_by_key(|node| node.name.to_lowercase());
    dirs.extend(files);
    Ok(Loaded {
        nodes: dirs,
        ..Default::default()
    })
}

/// Everything usable at the cursor: a completion at its position, with nothing typed and no
/// edit.
async fn scope(ctx: &Ctx) -> anyhow::Result<Loaded> {
    let Some(at) = ctx.scope.clone() else {
        bail!("the buffer has no file the server knows");
    };
    if !ctx.supports_completion() {
        bail!("the server doesn't complete");
    }
    let before = at.trigger.as_deref().and_then(|text| text.chars().next());
    let (items, incomplete) = ctx
        .completion(at.document, at.position, ctx.context(before, false))
        .await?;
    let nodes = items
        .into_iter()
        .map(|item| completion_node(item, None))
        .collect();
    Ok(Loaded {
        nodes,
        incomplete,
        requests: 1,
    })
}

/// Whether path queries list a module's items, found out with the query of the separator alone
/// (`::`), which such a server answers with the packages, all modules. Gives those packages.
async fn detect_path_queries(ctx: &Ctx) -> anyhow::Result<(bool, Option<Vec<Symbol>>)> {
    if let Some(known) = ctx.path_queries.get() {
        return Ok((*known, None));
    }
    if !ctx.supports_workspace_symbols() || ctx.separator.is_empty() {
        let _ = ctx.path_queries.set(false);
        return Ok((false, None));
    }
    let symbols = ctx.workspace_symbols(&ctx.separator).await?;
    // No answer at all may be a server still loading the workspace: asked again next time.
    if symbols.is_empty() {
        return Ok((false, Some(symbols)));
    }
    let works = symbols
        .iter()
        .all(|symbol| Kind::from_symbol(symbol.kind).is_module());
    let _ = ctx.path_queries.set(works);
    Ok((works, Some(symbols)))
}

async fn packages(ctx: &Ctx) -> anyhow::Result<Loaded> {
    let (works, symbols) = detect_path_queries(ctx).await?;
    let asked = usize::from(symbols.is_some());
    if works {
        let symbols = match symbols {
            Some(symbols) => symbols,
            None => ctx.workspace_symbols(&ctx.separator).await?,
        };
        let mut nodes: Vec<Node> = symbols
            .into_iter()
            .map(|symbol| {
                let mut node = symbol_node(ctx, symbol, Provider::PathQuery);
                node.kind = Kind::Package;
                node.load = Load::PathQuery(format!("{}{}", node.name, ctx.separator));
                node
            })
            .collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        nodes.dedup_by(|a, b| a.name == b.name);
        return Ok(Loaded {
            nodes,
            incomplete: false,
            requests: 1,
        });
    }
    let mut loaded = probe_packages(ctx, "").await?;
    loaded.requests += asked;
    Ok(loaded)
}

/// The roots of the standard library (`api-roots`), each listed with path queries or the
/// probe's `std` template.
async fn std_roots(ctx: &Ctx) -> anyhow::Result<Loaded> {
    if ctx.roots.is_empty() {
        bail!("no api-roots for this language in languages.toml");
    }
    let (works, symbols) = detect_path_queries(ctx).await?;
    if !works && ctx.template(true).is_none() {
        bail!("the server doesn't answer path queries and the language has no api-probe");
    }
    let nodes = ctx
        .roots
        .iter()
        .map(|root| {
            let mut node = Node::new(root.clone(), Kind::Package, Provider::Browser);
            node.load = if works {
                Load::PathQuery(format!("{root}{}", ctx.separator))
            } else {
                Load::Members {
                    module: root.clone(),
                    chain: Vec::new(),
                    std: true,
                }
            };
            node
        })
        .collect();
    Ok(Loaded {
        nodes,
        incomplete: false,
        requests: usize::from(symbols.is_some()),
    })
}

/// The items of a module, from a path query. Items in a container (a type's methods, a
/// function's statics) are left to their container.
async fn path_query(ctx: &Ctx, query: &str) -> anyhow::Result<Loaded> {
    let symbols = ctx.workspace_symbols(query).await?;
    let mut nodes: Vec<Node> = symbols
        .into_iter()
        .filter(|symbol| symbol.container.as_deref().is_none_or(str::is_empty))
        .map(|symbol| {
            let mut node = symbol_node(ctx, symbol, Provider::PathQuery);
            if node.kind.is_module() {
                node.load = Load::PathQuery(format!("{query}{}{}", node.name, ctx.separator));
            }
            node
        })
        .collect();
    sort_nodes(&mut nodes);
    Ok(Loaded {
        nodes,
        incomplete: false,
        requests: 1,
    })
}

/// The symbols of a file. A file the editor doesn't have open is opened on the server for the
/// request, with its text on disk, and closed after.
async fn file_symbols(ctx: &Ctx, file: &Path) -> anyhow::Result<Loaded> {
    if !ctx.supports_document_symbols() {
        bail!("the server doesn't list document symbols");
    }
    let (uri, _document) = ctx.open_real(file).await?;
    let response = ctx.document_symbols(&uri).await?;
    Ok(Loaded {
        nodes: document_symbol_nodes(ctx, &uri, response, Provider::DocumentSymbols),
        incomplete: false,
        requests: 1,
    })
}

/// The packages under `prefix`, completed in the probe's `packages` template.
async fn probe_packages(ctx: &Ctx, prefix: &str) -> anyhow::Result<Loaded> {
    let Some(template) = ctx
        .probe
        .as_ref()
        .and_then(|probe| probe.packages.as_deref())
    else {
        bail!("the server doesn't answer path queries and the language has no api-probe packages");
    };
    if !ctx.supports_completion() {
        bail!("the server doesn't complete");
    }
    let probe = ctx.open_probe(template, prefix)?;
    let (items, incomplete, requests) = ctx.probe_completion(&probe, true).await?;
    drop(probe.document);
    let mut nodes: Vec<Node> = items
        .into_iter()
        .map(|item| {
            let name = completion_name(&item);
            let module = format!("{prefix}{name}");
            let mut node = Node::new(name.clone(), Kind::Package, Provider::Probe);
            node.detail = item
                .detail
                .clone()
                .filter(|detail| detail.trim_matches('"') != name);
            node.load = if name.ends_with('/') {
                node.kind = Kind::Directory;
                Load::Packages(module)
            } else {
                Load::Members {
                    module,
                    chain: Vec::new(),
                    std: false,
                }
            };
            node
        })
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes.dedup_by(|a, b| a.name == b.name);
    Ok(Loaded {
        nodes,
        incomplete,
        requests,
    })
}

/// The members of a package (and of its members along `chain`), completed in the probe's
/// `members` (or `std`) template; or, with a `{definition}` marker, the document symbols of the
/// file its definition leads to (a C header).
async fn members(ctx: &Ctx, module: &str, chain: &[String], std: bool) -> anyhow::Result<Loaded> {
    let Some(template) = ctx.member_template(std, chain, None) else {
        bail!("the language has no api-probe members template");
    };
    let probe = ctx.open_probe(&template, module)?;
    if probe.definition {
        let (location, asked) = ctx.probe_definition(&probe).await?;
        drop(probe.document);
        let Some(location) = location else {
            bail!("the server found no definition for {module}");
        };
        let path = location
            .uri
            .to_file_path()
            .map_err(|_| anyhow!("the definition is not a file"))?;
        let (uri, _document) = ctx.open_real(&path).await?;
        let response = ctx.document_symbols(&uri).await?;
        return Ok(Loaded {
            nodes: document_symbol_nodes(ctx, &uri, response, Provider::Definition),
            incomplete: false,
            requests: asked + 1,
        });
    }
    let (items, incomplete, requests) = ctx.probe_completion(&probe, false).await?;
    drop(probe.document);
    let mut nodes: Vec<Node> = items
        .into_iter()
        .map(|item| completion_node(item, Some((module.to_string(), chain.to_vec(), std))))
        .collect();
    sort_nodes(&mut nodes);
    Ok(Loaded {
        nodes,
        incomplete,
        requests,
    })
}

/// The members of a type, from the document symbols of the file that defines it: the type's own
/// children, and the children of the symbols named after it (`impl Shape for Circle`), or those
/// symbols themselves when they have none (Go's `(*Rect).Area`).
async fn type_members(ctx: &Ctx, node: &Node) -> anyhow::Result<Loaded> {
    let mut requests = 0;
    let mut place = node.place.clone();
    if place.is_none() {
        // A member found by completion: its definition, through a temporary document.
        place = locate(ctx, node).await?;
        requests += 1;
    }
    let Some(place) = place else {
        bail!("the server found no definition for {}", node.name);
    };
    let path = place.path().ok_or_else(|| anyhow!("not a file"))?;
    let (uri, document) = ctx.open_real(&path).await?;
    let response = ctx.document_symbols(&uri).await?;
    requests += 1;
    let symbols = nested(response, &uri);
    if let Some(members) = members_of(&symbols, &node.name) {
        return Ok(Loaded {
            nodes: members_to_nodes(ctx, &uri, members),
            incomplete: false,
            requests,
        });
    }
    // Not defined here (a re-export): follow its definition.
    drop(document);
    if !ctx.supports_definition() {
        bail!("{} is not defined in {}", node.name, path.display());
    }
    let (uri, _document) = ctx.open_real(&path).await?;
    let location = ctx.definition(&uri, place.range.start).await?;
    requests += 1;
    let Some(location) =
        location.filter(|location| location.uri != uri || location.range != place.range)
    else {
        bail!("the server found no other definition for {}", node.name);
    };
    let target = location
        .uri
        .to_file_path()
        .map_err(|_| anyhow!("not a file"))?;
    let (target_uri, _target_document) = ctx.open_real(&target).await?;
    let response = ctx.document_symbols(&target_uri).await?;
    requests += 1;
    let symbols = nested(response, &target_uri);
    let members = members_of(&symbols, &node.name).unwrap_or_default();
    Ok(Loaded {
        nodes: members_to_nodes(ctx, &target_uri, members),
        incomplete: false,
        requests,
    })
}

// ------------------------------------------------------------------------------------------------
// Docs and definitions.

/// The docs of a node: those it came with, or `completionItem/resolve`, or a hover at its
/// definition (or at its name in a temporary document).
pub(crate) async fn docs(ctx: Arc<Ctx>, node: Node) -> anyhow::Result<Docs> {
    let signature = node.detail.clone();
    if node.docs.is_some() {
        return Ok(Docs {
            signature,
            text: node.docs.clone(),
            source: "the completion item",
        });
    }
    if let Some(item) = node.item.as_deref().filter(|_| ctx.supports_resolve()) {
        let resolved = ctx
            .client
            .cancellable_call::<lsp::request::ResolveCompletionItem>(item)
            .await?;
        let text = resolved.documentation.as_ref().map(documentation_text);
        if text.is_some() || resolved.detail.is_some() {
            return Ok(Docs {
                signature: resolved.detail.or(signature),
                text,
                source: "completionItem/resolve",
            });
        }
    }
    if !ctx.supports_hover() {
        return Ok(Docs {
            signature,
            text: None,
            source: "",
        });
    }
    if let Some(place) = node.place.clone().filter(|_| node.kind != Kind::File) {
        let path = place.path().ok_or_else(|| anyhow!("not a file"))?;
        let (uri, _document) = ctx.open_real(&path).await?;
        let text = ctx.hover(&uri, place.range.start).await?;
        return Ok(Docs {
            signature,
            text,
            source: "hover",
        });
    }
    if let Some((module, chain, std)) = &node.probe {
        let Some(template) = ctx.member_template(*std, chain, Some(&node.name)) else {
            return Ok(Docs::default());
        };
        let probe = ctx.open_probe(&template, module)?;
        let text = ctx.probe_hover(&probe).await?;
        return Ok(Docs {
            signature,
            text,
            source: "hover in a temporary document",
        });
    }
    Ok(Docs {
        signature,
        text: None,
        source: "",
    })
}

/// Where a node is defined: its place, or the definition of its name in a temporary document.
pub(crate) async fn definition(ctx: Arc<Ctx>, node: Node) -> anyhow::Result<Option<Place>> {
    if node.place.is_some() {
        return Ok(node.place);
    }
    locate(&ctx, &node).await
}

/// The definition of a member found by completion, with the members template where the member
/// is written out (`m.Router` with the position on `Router`).
async fn locate(ctx: &Ctx, node: &Node) -> anyhow::Result<Option<Place>> {
    let Some((module, chain, std)) = &node.probe else {
        return Ok(None);
    };
    if !ctx.supports_definition() {
        return Ok(None);
    }
    let Some(template) = ctx.member_template(*std, chain, Some(&node.name)) else {
        return Ok(None);
    };
    let probe = ctx.open_probe(&template, module)?;
    let (location, _) = ctx.probe_definition(&probe).await?;
    Ok(location
        .filter(|location| location.uri != probe.uri)
        .map(|location| Place {
            uri: location.uri,
            range: location.range,
            encoding: ctx.encoding,
        }))
}

// ------------------------------------------------------------------------------------------------
// Conversions.

fn symbol_node(ctx: &Ctx, symbol: Symbol, provider: Provider) -> Node {
    let kind = Kind::from_symbol(symbol.kind);
    let mut node = Node::new(symbol.name, kind, provider);
    node.place = symbol.location.map(|location| Place {
        uri: location.uri,
        range: location.range,
        encoding: ctx.encoding,
    });
    if kind.is_type() {
        node.load = Load::TypeMembers;
    }
    node
}

fn completion_node(item: lsp::CompletionItem, probe: Option<(String, Vec<String>, bool)>) -> Node {
    let kind = Kind::from_completion(item.kind);
    let name = completion_name(&item);
    let mut node = Node::new(
        name.clone(),
        kind,
        if probe.is_some() {
            Provider::Probe
        } else {
            Provider::Completion
        },
    );
    let detail = match (
        &item.detail,
        item.label_details
            .as_ref()
            .and_then(|details| details.detail.as_ref()),
    ) {
        (Some(detail), Some(signature)) if !detail.contains(signature.as_str()) => {
            Some(format!("{signature} {detail}"))
        }
        (Some(detail), _) => Some(detail.clone()),
        (None, Some(signature)) => Some(signature.clone()),
        (None, None) => None,
    };
    node.detail = detail.filter(|detail| !detail.is_empty());
    node.docs = item.documentation.as_ref().map(documentation_text);
    if let Some((module, chain, std)) = &probe {
        if kind.is_module() {
            node.load = Load::Members {
                module: module.clone(),
                chain: chain.iter().cloned().chain([name]).collect(),
                std: *std,
            };
        } else if kind.is_type() {
            node.load = Load::TypeMembers;
        }
    }
    node.probe = probe;
    node.item = Some(Box::new(item));
    node
}

/// Document symbols as one nested shape (a flat answer has no children).
fn nested(response: lsp::DocumentSymbolResponse, uri: &lsp::Url) -> Vec<lsp::DocumentSymbol> {
    match response {
        lsp::DocumentSymbolResponse::Nested(symbols) => symbols,
        #[allow(deprecated)]
        lsp::DocumentSymbolResponse::Flat(symbols) => symbols
            .into_iter()
            .filter(|symbol| &symbol.location.uri == uri)
            .map(|symbol| lsp::DocumentSymbol {
                name: symbol.name,
                detail: symbol
                    .container_name
                    .map(|container| format!("in {container}")),
                kind: symbol.kind,
                tags: symbol.tags,
                deprecated: symbol.deprecated,
                range: symbol.location.range,
                selection_range: symbol.location.range,
                children: None,
            })
            .collect(),
    }
}

fn document_symbol_nodes(
    ctx: &Ctx,
    uri: &lsp::Url,
    response: lsp::DocumentSymbolResponse,
    provider: Provider,
) -> Vec<Node> {
    members_to_nodes_with(ctx, uri, nested(response, uri), provider)
}

fn members_to_nodes(ctx: &Ctx, uri: &lsp::Url, symbols: Vec<lsp::DocumentSymbol>) -> Vec<Node> {
    members_to_nodes_with(ctx, uri, symbols, Provider::Definition)
}

fn members_to_nodes_with(
    ctx: &Ctx,
    uri: &lsp::Url,
    symbols: Vec<lsp::DocumentSymbol>,
    provider: Provider,
) -> Vec<Node> {
    symbols
        .into_iter()
        .map(|symbol| {
            let mut node = Node::new(symbol.name, Kind::from_symbol(symbol.kind), provider);
            node.detail = symbol.detail.filter(|detail| !detail.is_empty());
            node.place = Some(Place {
                uri: uri.clone(),
                range: symbol.selection_range,
                encoding: ctx.encoding,
            });
            let children = symbol.children.unwrap_or_default();
            if !children.is_empty() {
                node.load = Load::Given(members_to_nodes_with(ctx, uri, children, provider));
            }
            node
        })
        .collect()
}

/// The members of the type `name` among `symbols` (searched at every depth), or `None` when
/// the type isn't there.
fn members_of(symbols: &[lsp::DocumentSymbol], name: &str) -> Option<Vec<lsp::DocumentSymbol>> {
    fn flatten<'a>(symbols: &'a [lsp::DocumentSymbol], out: &mut Vec<&'a lsp::DocumentSymbol>) {
        for symbol in symbols {
            out.push(symbol);
            flatten(symbol.children.as_deref().unwrap_or_default(), out);
        }
    }
    let mut all = Vec::new();
    flatten(symbols, &mut all);
    let defined = all
        .iter()
        .any(|symbol| symbol.name == name && Kind::from_symbol(symbol.kind).is_type());
    if !defined {
        return None;
    }
    let mut members = Vec::new();
    for symbol in all {
        if symbol.name == name {
            members.extend(symbol.children.iter().flatten().cloned());
        } else if names_word(&symbol.name, name) {
            match &symbol.children {
                Some(children) if !children.is_empty() => members.extend(children.iter().cloned()),
                _ if matches!(
                    Kind::from_symbol(symbol.kind),
                    Kind::Method | Kind::Function | Kind::Field | Kind::Property
                ) =>
                {
                    members.push(symbol.clone())
                }
                _ => {}
            }
        }
    }
    Some(members)
}

/// Whether `text` has `word` as a whole word (`impl Shape for Circle` has `Circle`, `Circles`
/// doesn't).
fn names_word(text: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// Modules first, then types, then the rest, each by name.
fn sort_nodes(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| {
        let rank = |node: &Node| {
            if node.kind.is_module() {
                0
            } else if node.kind.is_type() {
                1
            } else {
                2
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(
        name: &str,
        kind: lsp::SymbolKind,
        children: Vec<lsp::DocumentSymbol>,
    ) -> lsp::DocumentSymbol {
        #[allow(deprecated)]
        lsp::DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: Default::default(),
            selection_range: Default::default(),
            children: Some(children),
        }
    }

    #[test]
    fn a_types_members_come_from_its_impl_blocks_and_methods() {
        let method = |name: &str| symbol(name, lsp::SymbolKind::METHOD, vec![]);
        let symbols = vec![
            symbol(
                "Circle",
                lsp::SymbolKind::STRUCT,
                vec![symbol("radius", lsp::SymbolKind::FIELD, vec![])],
            ),
            symbol(
                "impl Shape for Circle",
                lsp::SymbolKind::OBJECT,
                vec![method("area")],
            ),
            symbol(
                "impl Circles",
                lsp::SymbolKind::OBJECT,
                vec![method("count")],
            ),
            symbol("(*Circle).Grow", lsp::SymbolKind::METHOD, vec![]),
        ];
        let names: Vec<_> = members_of(&symbols, "Circle")
            .unwrap()
            .into_iter()
            .map(|symbol| symbol.name)
            .collect();
        assert_eq!(names, ["radius", "area", "(*Circle).Grow"]);
        assert!(members_of(&symbols, "Rect").is_none(), "not defined there");
    }

    #[test]
    fn whole_words_only() {
        assert!(names_word("impl Shape for Circle", "Circle"));
        assert!(names_word("(*Rect).Area", "Rect"));
        assert!(!names_word("impl Circles", "Circle"));
        assert!(!names_word("RectInfo", "Rect"));
    }

    #[test]
    fn api_params_are_merged_into_the_request() {
        let params = serde_json::json!({"query": "::"});
        let merged = with_params(params, Some(&serde_json::json!({"limit": 0})));
        assert_eq!(merged, serde_json::json!({"query": "::", "limit": 0}));
    }
}
