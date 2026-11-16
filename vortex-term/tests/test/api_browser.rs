//! Integration tests of the API browser (`vortex_term::api`) and `:api-browse` against the
//! scripted fake language server (`tests/fake_lsp/main.rs`): which provider loads a level, that
//! nothing is asked before it is wanted, that closing the browser cancels what is in flight,
//! that temporary documents stay out of the user's way, and the picker end to end.
//!
//! Each test builds a workspace in a temporary directory (with a `.git` so that the workspace
//! root is found), makes the `rust` language use the fake server (plus the `api-*` keys the test
//! needs), and reads the server's log to see what the browser asked.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;
use vortex_core::Position;
use vortex_term::api::{Browser, Kind, Level, NodeId, Provider, State};
use vortex_term::application::Application;
use vortex_view::doc;

use super::debug::harness::Harness;
use super::fake_lsp::{Log, Script, SERVER};
use super::helpers::{test_config, test_syntax_loader, AppBuilder};

// ------------------------------------------------------------------------------------------------
// The workspace.

const LIB_RS: &str =
    "pub mod shapes;\n\npub fn answer() -> u32 {\n    let total = 40 + 2;\n    total\n}\n";

const SHAPES_RS: &str = "pub struct Circle {\n    pub radius: f64,\n}\n\npub trait Shape {\n    fn area(&self) -> f64;\n}\n\nimpl Shape for Circle {\n    fn area(&self) -> f64 {\n        3.14 * self.radius * self.radius\n    }\n}\n\nimpl Square {\n    fn side(&self) -> f64 {\n        1.0\n    }\n}\n";

/// A workspace on disk, and a directory of its own for the script and the log of the server (so
/// that neither is a file of the workspace).
struct Workspace {
    dir: TempDir,
    aux: TempDir,
}

fn range(line: u32, from: u32, to: u32) -> Value {
    json!({"start": {"line": line, "character": from}, "end": {"line": line, "character": to}})
}

/// A nested document symbol: `selection` is the name, `lines` the lines it spans.
fn doc_symbol(
    name: &str,
    kind: u32,
    lines: (u32, u32),
    selection: Value,
    children: Vec<Value>,
) -> Value {
    json!({
        "name": name,
        "kind": kind,
        "range": {"start": {"line": lines.0, "character": 0}, "end": {"line": lines.1, "character": 1}},
        "selectionRange": selection,
        "children": children,
    })
}

/// The document symbols of `src/shapes.rs`: a struct with a field, a trait with a method, an
/// `impl Shape for Circle` block with a method, and an unrelated `impl Square`.
fn shapes_symbols() -> Value {
    json!([
        doc_symbol(
            "Circle",
            23,
            (0, 2),
            range(0, 11, 17),
            vec![doc_symbol("radius", 8, (1, 1), range(1, 8, 14), vec![])],
        ),
        doc_symbol(
            "Shape",
            11,
            (4, 6),
            range(4, 10, 15),
            vec![doc_symbol("area", 6, (5, 5), range(5, 7, 11), vec![])],
        ),
        doc_symbol(
            "impl Shape for Circle",
            19,
            (8, 12),
            range(8, 0, 4),
            vec![doc_symbol("area", 6, (9, 11), range(9, 7, 11), vec![])],
        ),
        doc_symbol(
            "impl Square",
            19,
            (14, 18),
            range(14, 0, 4),
            vec![doc_symbol("side", 6, (15, 17), range(15, 7, 11), vec![])],
        ),
    ])
}

/// The document symbols of `src/lib.rs`.
fn lib_symbols() -> Value {
    let mut answer = doc_symbol("answer", 12, (2, 5), range(2, 7, 13), vec![]);
    answer["detail"] = json!("fn() -> u32");
    json!([
        doc_symbol("shapes", 2, (0, 0), range(0, 8, 14), vec![]),
        answer,
    ])
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(root.join("src/lib.rs"), LIB_RS).unwrap();
        std::fs::write(root.join("src/shapes.rs"), SHAPES_RS).unwrap();
        // Files of no language the fake server serves: not listed by the Codebase.
        std::fs::write(root.join("src/notes.txt"), "not code\n").unwrap();
        std::fs::write(root.join("README.md"), "# readme\n").unwrap();
        Self {
            dir,
            aux: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    fn uri(&self, relative: &str) -> String {
        format!("file://{}", self.path(relative).display())
    }

    /// A `Location` in a file of the workspace.
    fn location(&self, relative: &str, line: u32, from: u32, to: u32) -> Value {
        json!({"uri": self.uri(relative), "range": range(line, from, to)})
    }

    /// A workspace symbol (`SymbolInformation`).
    fn symbol(&self, name: &str, kind: u32, container: Option<&str>, location: Value) -> Value {
        let mut symbol = json!({"name": name, "kind": kind, "location": location});
        if let Some(container) = container {
            symbol["containerName"] = json!(container);
        }
        symbol
    }

    fn log_path(&self) -> PathBuf {
        Script::log_path(self.aux.path())
    }

    /// The trace of the log (see [`trace`]).
    fn trace(&self) -> Vec<String> {
        trace(&Log::load(&self.log_path()), self.root())
    }
}

/// The `api-*` keys of the language and the server, and what the editor opens.
struct Options {
    /// Extra keys of `[language-server.fake]`.
    server: String,
    /// Extra keys of the `rust` language.
    language: String,
    /// The file the editor opens, relative to the workspace.
    open: &'static str,
    at: Option<Position>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server: String::new(),
            language: String::new(),
            open: "src/lib.rs",
            at: None,
        }
    }
}

impl Options {
    fn language(language: &str) -> Self {
        Self {
            language: language.to_string(),
            ..Self::default()
        }
    }

    fn server(server: &str) -> Self {
        Self {
            server: server.to_string(),
            ..Self::default()
        }
    }

    fn probe(packages: &str, members: &str) -> Self {
        Self::language(&format!(
            "api-probe = {{ packages = {}, members = {} }}",
            Value::from(packages),
            Value::from(members)
        ))
    }
}

fn languages_toml(script: &Path, options: &Options) -> String {
    format!(
        "[language-server.fake]\ncommand = {}\nargs = [{}]\n{}\n\n\
         [[language]]\nname = \"rust\"\nlanguage-servers = [\"fake\"]\n{}\n",
        Value::from(SERVER),
        Value::from(script.to_string_lossy().into_owned()),
        options.server,
        options.language,
    )
}

// ------------------------------------------------------------------------------------------------
// The editor with the fake server, and the log.

struct Fixture {
    ws: Workspace,
    harness: Harness,
}

impl Workspace {
    /// Starts the editor on `options.open` with the fake server, and waits until the server is
    /// initialized, has the file and is attached to its document.
    async fn start(self, script: Script, options: Options) -> anyhow::Result<Fixture> {
        let script_path = script.write(self.aux.path());
        let mut config = test_config();
        config.editor.lsp.enable = true;
        let loader = test_syntax_loader(Some(languages_toml(&script_path, &options)));
        let builder = AppBuilder::new()
            .with_config(config)
            .with_lang_loader(loader)
            .with_file(self.path(options.open), options.at);
        let harness = Harness::new(builder)?;
        let mut fixture = Fixture { ws: self, harness };
        fixture
            .harness
            .until("the fake server is initialized and attached", |app| {
                app.editor
                    .language_servers
                    .iter_clients()
                    .any(|client| client.is_initialized())
                    && doc!(app.editor).language_servers().next().is_some()
            })
            .await;
        fixture
            .until_log("the editor opened its file", |log| {
                log.count("textDocument/didOpen") > 0
            })
            .await;
        Ok(fixture)
    }
}

impl Fixture {
    fn log(&self) -> Log {
        Log::load(&self.ws.log_path())
    }

    /// Pumps the editor until the log satisfies `cond`.
    async fn until_log(&mut self, what: &str, cond: impl Fn(&Log) -> bool) {
        let start = Instant::now();
        loop {
            let log = self.log();
            if cond(&log) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out waiting for: {what}\ntrace: {:#?}",
                trace(&log, self.ws.root())
            );
            self.harness.step(Duration::from_millis(50)).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn browser(&self) -> Browser {
        Browser::new(&self.harness.app.editor).expect("a browser")
    }

    /// Pumps the editor and takes in the browser's results until no task is in flight.
    async fn settle(&mut self, browser: &mut Browser) {
        let start = Instant::now();
        loop {
            browser.update();
            if browser.in_flight() == 0 {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "{} requests still in flight\ntrace: {:#?}",
                browser.in_flight(),
                self.ws.trace()
            );
            self.harness.step(Duration::from_millis(20)).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Opens a node and waits for its level.
    async fn open(&mut self, browser: &mut Browser, id: NodeId) {
        browser.open(id);
        self.settle(browser).await;
    }

    /// Opens the nodes along `path`, starting at a root named `path[0]`, and returns the last.
    async fn open_path(&mut self, browser: &mut Browser, path: &[&str]) -> NodeId {
        let mut id = root(browser, path[0]);
        self.open(browser, id).await;
        for name in &path[1..] {
            id = child(browser, id, name);
            self.open(browser, id).await;
        }
        id
    }

    async fn finish(self) -> anyhow::Result<()> {
        self.harness.finish().await
    }
}

/// The messages of the log that matter, in order, as `method detail`: the uri relative to the
/// workspace (the process id of the probes' names is `PID`), the query of a workspace symbol
/// request, the id of a cancelled request.
fn trace(log: &Log, workspace: &Path) -> Vec<String> {
    const METHODS: [&str; 10] = [
        "textDocument/didOpen",
        "textDocument/didClose",
        "textDocument/didChange",
        "textDocument/documentSymbol",
        "textDocument/completion",
        "textDocument/hover",
        "textDocument/definition",
        "completionItem/resolve",
        "workspace/symbol",
        "$/cancelRequest",
    ];
    let prefix = format!("file://{}/", workspace.display());
    let pid = std::process::id().to_string();
    log.0
        .iter()
        .filter_map(|line| line.get("message"))
        .filter_map(|message| {
            let method = message.get("method")?.as_str()?;
            if !METHODS.contains(&method) {
                return None;
            }
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let detail = match method {
                "workspace/symbol" => params["query"].as_str()?.to_string(),
                "$/cancelRequest" => params["id"].to_string(),
                "completionItem/resolve" => params["label"].as_str()?.to_string(),
                _ => params["textDocument"]["uri"].as_str()?.to_string(),
            };
            Some(
                format!("{method} {detail}")
                    .replace(&prefix, "")
                    .replace(&pid, "PID"),
            )
        })
        .collect()
}

/// How many entries of the trace start with `prefix`.
fn count(trace: &[String], prefix: &str) -> usize {
    trace
        .iter()
        .filter(|entry| entry.starts_with(prefix))
        .count()
}

/// The `query` of each `workspace/symbol` request.
fn queries(log: &Log) -> Vec<String> {
    log.requests("workspace/symbol")
        .iter()
        .map(|params| params["query"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The ids of the requests of a method.
fn request_ids(log: &Log, method: &str) -> Vec<Value> {
    log.0
        .iter()
        .filter_map(|line| line.get("message"))
        .filter(|message| message["method"] == method && message.get("id").is_some())
        .map(|message| message["id"].clone())
        .collect()
}

/// The ids named by the `$/cancelRequest` notifications.
fn cancel_notifications(log: &Log) -> Vec<Value> {
    log.notifications("$/cancelRequest")
        .iter()
        .map(|params| params["id"].clone())
        .collect()
}

/// Every request about a document came while the server had it open (a didOpen without a
/// didClose before): the browser never asks about a document it didn't open or the editor
/// didn't have.
fn assert_requests_in_open_documents(log: &Log) {
    let mut open = std::collections::HashSet::new();
    for message in log.0.iter().filter_map(|line| line.get("message")) {
        let Some(method) = message["method"].as_str() else {
            continue;
        };
        let uri = message["params"]["textDocument"]["uri"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        match method {
            "textDocument/didOpen" => {
                assert!(open.insert(uri.clone()), "{uri} opened twice");
            }
            "textDocument/didClose" => {
                assert!(open.remove(&uri), "{uri} closed without being open");
            }
            "textDocument/documentSymbol"
            | "textDocument/completion"
            | "textDocument/hover"
            | "textDocument/definition" => {
                assert!(open.contains(&uri), "{method} for {uri}, which is not open");
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Looking at the tree.

fn root(browser: &Browser, name: &str) -> NodeId {
    browser
        .roots()
        .iter()
        .copied()
        .find(|&id| browser.node(id).name == name)
        .unwrap_or_else(|| panic!("no root {name}"))
}

fn level(browser: &Browser, id: NodeId) -> &Level {
    match browser.children(id) {
        State::Ready(level) => level,
        other => panic!("{} is not loaded: {other:?}", browser.node(id).name),
    }
}

fn names(browser: &Browser, id: NodeId) -> Vec<String> {
    level(browser, id)
        .children
        .iter()
        .map(|&child| browser.node(child).name.clone())
        .collect()
}

fn child(browser: &Browser, id: NodeId, name: &str) -> NodeId {
    level(browser, id)
        .children
        .iter()
        .copied()
        .find(|&child| browser.node(child).name == name)
        .unwrap_or_else(|| panic!("no {name} in {:?}", names(browser, id)))
}

fn kinds(browser: &Browser, id: NodeId) -> Vec<Kind> {
    level(browser, id)
        .children
        .iter()
        .map(|&child| browser.node(child).kind)
        .collect()
}

/// The providers of a level's items, without repeats.
fn providers(browser: &Browser, id: NodeId) -> Vec<Provider> {
    let mut providers = Vec::new();
    for &child in &level(browser, id).children {
        let provider = browser.node(child).provider;
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    providers
}

fn failure(browser: &Browser, id: NodeId) -> String {
    match browser.children(id) {
        State::Failed(reason) => reason.clone(),
        other => panic!("{} should have failed: {other:?}", browser.node(id).name),
    }
}

// ------------------------------------------------------------------------------------------------
// Scripts.

fn capabilities() -> Value {
    json!({
        "textDocumentSync": 1,
        "workspaceSymbolProvider": true,
        "documentSymbolProvider": true,
        "completionProvider": {},
        "hoverProvider": true,
        "definitionProvider": true,
    })
}

/// A server that lists packages and their items with path queries (like rust-analyzer):
/// `geo` and `serde`, and in `geo` a module, a struct and a trait (and a method of the struct,
/// which belongs to it). Document symbols for the two files, hovers for the places.
fn path_query_script(ws: &Workspace) -> Script {
    Script::new()
        .capabilities(capabilities())
        .workspace_symbol(
            "::",
            json!([
                ws.symbol("geo", 2, None, ws.location("src/shapes.rs", 0, 0, 0)),
                ws.symbol("serde", 2, None, ws.location("src/lib.rs", 0, 8, 14)),
            ]),
        )
        .workspace_symbol(
            "geo::",
            json!([
                ws.symbol("Circle", 23, None, ws.location("src/shapes.rs", 0, 11, 17)),
                ws.symbol("Shape", 11, None, ws.location("src/shapes.rs", 4, 10, 15)),
                ws.symbol(
                    "area",
                    6,
                    Some("Circle"),
                    ws.location("src/shapes.rs", 9, 7, 11)
                ),
                ws.symbol("util", 2, None, ws.location("src/lib.rs", 2, 7, 13)),
            ]),
        )
        .document_symbols("src/lib.rs", lib_symbols())
        .document_symbols("src/shapes.rs", shapes_symbols())
        .hover(
            json!({"uri_suffix": "src/shapes.rs", "line": 0, "character": 0}),
            "The geo crate.",
        )
        .hover(
            json!({"uri_suffix": "src/lib.rs", "line": 0, "character": 8}),
            "Serialization.",
        )
        .hover(
            json!({"uri_suffix": "src/shapes.rs", "line": 0, "character": 11}),
            "A circle.",
        )
        .hover(
            json!({"uri_suffix": "src/shapes.rs", "line": 4, "character": 10}),
            "A shape.",
        )
        .hover(
            json!({"uri_suffix": "src/shapes.rs", "line": 9, "character": 7}),
            "The area of the circle.",
        )
}

/// A completion list of items `(label, kind)`.
fn completion_list(items: &[(&str, u32)]) -> Value {
    json!({
        "isIncomplete": false,
        "items": items.iter().map(|(label, kind)| json!({"label": label, "kind": kind})).collect::<Vec<_>>(),
    })
}

/// A server without workspace symbols that completes package names after `use ` and a
/// package's members after `use serde::`.
fn probe_script() -> Script {
    Script::new()
        .capabilities(json!({
            "textDocumentSync": 1,
            "completionProvider": {},
            "documentSymbolProvider": true,
        }))
        .completion("use ", completion_list(&[("serde", 9), ("regex", 9)]))
        .completion(
            "use serde::",
            json!({"isIncomplete": false, "items": [
                {"label": "to_string", "kind": 3, "detail": "fn(&T) -> String"},
                {"label": "de", "kind": 9},
            ]}),
        )
}

fn probe_options() -> Options {
    Options::probe("use {module}{cursor}", "use {module}::{cursor}")
}

// ================================================================================================
// 1. Which provider loads a level.

#[tokio::test(flavor = "multi_thread")]
async fn packages_come_from_path_queries_when_the_server_answers_the_separator_with_modules(
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws);
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();

    let names_of_roots: Vec<_> = browser
        .roots()
        .iter()
        .map(|&id| browser.node(id).name.clone())
        .collect();
    assert_eq!(names_of_roots, ["Codebase", "Scope", "Packages", "Std"]);
    assert_eq!(
        browser.path_queries(),
        None,
        "not found out before it is asked"
    );

    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    assert_eq!(names(&browser, packages), ["geo", "serde"]);
    assert_eq!(kinds(&browser, packages), [Kind::Package, Kind::Package]);
    assert_eq!(providers(&browser, packages), [Provider::PathQuery]);
    assert_eq!(level(&browser, packages).requests, 1);
    assert_eq!(browser.path_queries(), Some(true));

    // Opening a package sends `name::`; the method of the struct has a container, so it is
    // left to the struct.
    let geo = child(&browser, packages, "geo");
    fx.open(&mut browser, geo).await;
    assert_eq!(names(&browser, geo), ["util", "Circle", "Shape"]);
    assert_eq!(
        kinds(&browser, geo),
        [Kind::Module, Kind::Struct, Kind::Interface]
    );
    assert_eq!(providers(&browser, geo), [Provider::PathQuery]);
    assert_eq!(browser.display_path(geo), "geo");
    assert_eq!(
        browser.display_path(child(&browser, geo, "Circle")),
        "geo::Circle"
    );

    let log = fx.log();
    assert_eq!(queries(&log), ["::", "geo::"]);
    assert_eq!(
        log.count("textDocument/completion"),
        0,
        "no probe was needed"
    );
    assert_eq!(
        fx.ws.trace(),
        [
            "textDocument/didOpen src/lib.rs",
            "workspace/symbol ::",
            "workspace/symbol geo::"
        ]
    );

    drop(browser);
    fx.finish().await
}

/// Opens Packages with a server whose answer to `::` is not a list of modules (or that has no
/// workspace symbols at all): the probe's completion lists the packages, and its members.
async fn packages_through_the_probe(script: Script, asks_separator: bool) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut fx = ws.start(script, probe_options()).await?;
    let mut browser = fx.browser();

    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    assert_eq!(names(&browser, packages), ["regex", "serde"]);
    assert_eq!(providers(&browser, packages), [Provider::Probe]);
    assert_eq!(browser.path_queries(), Some(false));
    // The separator query (when the server could answer it) and the completion.
    assert_eq!(
        level(&browser, packages).requests,
        1 + usize::from(asks_separator)
    );

    fx.until_log("the probe was closed", |log| {
        log.count("textDocument/didClose") == 1
    })
    .await;
    let mut expected = vec!["textDocument/didOpen src/lib.rs".to_string()];
    if asks_separator {
        expected.push("workspace/symbol ::".into());
    }
    expected.extend([
        "textDocument/didOpen vortex_api_probe_PID_0.rs".to_string(),
        "textDocument/completion vortex_api_probe_PID_0.rs".to_string(),
        "textDocument/didClose vortex_api_probe_PID_0.rs".to_string(),
    ]);
    assert_eq!(fx.ws.trace(), expected);

    // The members template of the package.
    let serde = child(&browser, packages, "serde");
    fx.open(&mut browser, serde).await;
    assert_eq!(names(&browser, serde), ["de", "to_string"]);
    assert_eq!(providers(&browser, serde), [Provider::Probe]);
    assert_eq!(
        browser
            .node(child(&browser, serde, "to_string"))
            .detail
            .as_deref(),
        Some("fn(&T) -> String")
    );

    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn packages_come_from_the_probe_when_the_separator_query_lists_no_modules(
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    // A function is no package: the server doesn't do path queries.
    let script = probe_script()
        .capabilities(json!({
            "textDocumentSync": 1,
            "workspaceSymbolProvider": true,
            "completionProvider": {},
        }))
        .workspace_symbol(
            "::",
            json!([ws.symbol("helper", 12, None, ws.location("src/lib.rs", 2, 7, 13))]),
        );
    drop(ws);
    packages_through_the_probe(script, true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn packages_come_from_the_probe_when_the_server_has_no_workspace_symbols(
) -> anyhow::Result<()> {
    packages_through_the_probe(probe_script(), false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn packages_and_std_fail_with_a_reason_without_path_queries_or_a_probe() -> anyhow::Result<()>
{
    let ws = Workspace::new();
    let script = Script::new().capabilities(json!({
        "textDocumentSync": 1,
        "completionProvider": {},
    }));
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();

    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    let reason = failure(&browser, packages);
    assert!(reason.contains("api-probe"), "{reason}");

    let std = root(&browser, "Std");
    fx.open(&mut browser, std).await;
    let reason = failure(&browser, std);
    assert!(reason.contains("api-probe"), "{reason}");

    assert_eq!(
        fx.ws.trace(),
        ["textDocument/didOpen src/lib.rs"],
        "nothing was asked"
    );
    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn packages_fail_when_the_separator_query_lists_no_modules_and_there_is_no_probe(
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = Script::new()
        .capabilities(json!({"textDocumentSync": 1, "workspaceSymbolProvider": true}))
        .workspace_symbol(
            "::",
            json!([ws.symbol("helper", 12, None, ws.location("src/lib.rs", 2, 7, 13))]),
        );
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();
    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    let reason = failure(&browser, packages);
    assert!(reason.contains("api-probe"), "{reason}");
    assert_eq!(browser.path_queries(), Some(false));
    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_is_the_completion_at_the_cursor() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = Script::new().capabilities(capabilities()).completion(
        "let total = ",
        json!({"isIncomplete": false, "items": [
            {"label": "answer", "kind": 3, "detail": "fn() -> u32",
             "documentation": "The answer."},
            {"label": "total", "kind": 6},
        ]}),
    );
    // Line 4, column 17 (1-based): right after `let total = `.
    let options = Options {
        at: Some(Position::new(3, 16)),
        ..Options::default()
    };
    let mut fx = ws.start(script, options).await?;
    let mut browser = fx.browser();

    let scope = root(&browser, "Scope");
    fx.open(&mut browser, scope).await;
    assert_eq!(names(&browser, scope), ["answer", "total"]);
    assert_eq!(kinds(&browser, scope), [Kind::Function, Kind::Variable]);
    assert_eq!(providers(&browser, scope), [Provider::Completion]);

    let log = fx.log();
    let requests = log.requests("textDocument/completion");
    assert_eq!(requests.len(), 1, "{:?}", fx.ws.trace());
    assert!(requests[0]["textDocument"]["uri"]
        .as_str()
        .is_some_and(|uri| uri.ends_with("src/lib.rs")));
    assert_eq!(requests[0]["position"], json!({"line": 3, "character": 16}));
    assert_eq!(requests[0]["context"]["triggerKind"], 1, "invoked");

    // The docs came with the completion item: nothing more is asked.
    let answer = child(&browser, scope, "answer");
    browser.ask_docs(answer);
    fx.settle(&mut browser).await;
    let State::Ready(docs) = browser.docs(answer) else {
        panic!("{:?}", browser.docs(answer));
    };
    assert_eq!(docs.text.as_deref(), Some("The answer."));
    assert_eq!(docs.signature.as_deref(), Some("fn() -> u32"));
    assert_eq!(fx.log().count("textDocument/hover"), 0);
    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_fails_when_the_server_does_not_complete() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = Script::new().capabilities(json!({
        "textDocumentSync": 1,
        "documentSymbolProvider": true,
    }));
    let options = Options {
        at: Some(Position::new(3, 16)),
        ..Options::default()
    };
    let mut fx = ws.start(script, options).await?;
    let mut browser = fx.browser();
    let scope = root(&browser, "Scope");
    fx.open(&mut browser, scope).await;
    let reason = failure(&browser, scope);
    assert!(reason.contains("doesn't complete"), "{reason}");
    assert_eq!(fx.ws.trace(), ["textDocument/didOpen src/lib.rs"]);
    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_codebase_lists_directories_and_files_and_files_come_from_document_symbols(
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws);
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();

    let codebase = root(&browser, "Codebase");
    fx.open(&mut browser, codebase).await;
    // Directories of the workspace; the `.git`, the readme and the notes are not code.
    assert_eq!(names(&browser, codebase), ["docs", "src"]);
    assert_eq!(
        kinds(&browser, codebase),
        [Kind::Directory, Kind::Directory]
    );
    assert_eq!(providers(&browser, codebase), [Provider::Files]);
    assert_eq!(level(&browser, codebase).requests, 0);

    let src = child(&browser, codebase, "src");
    fx.open(&mut browser, src).await;
    assert_eq!(names(&browser, src), ["lib.rs", "shapes.rs"]);
    assert_eq!(kinds(&browser, src), [Kind::File, Kind::File]);
    assert_eq!(
        browser.display_path(child(&browser, src, "lib.rs")),
        "src/lib.rs"
    );

    // A file the editor has open: its symbols, from the server's copy of the buffer.
    let lib = child(&browser, src, "lib.rs");
    fx.open(&mut browser, lib).await;
    assert_eq!(names(&browser, lib), ["shapes", "answer"]);
    assert_eq!(providers(&browser, lib), [Provider::DocumentSymbols]);
    assert!(!level(&browser, lib).skeleton);
    assert_eq!(level(&browser, lib).requests, 1);
    assert_eq!(
        browser
            .node(child(&browser, lib, "answer"))
            .detail
            .as_deref(),
        Some("fn() -> u32")
    );

    // Another file: opened on the server for the request, closed after.
    let shapes = child(&browser, src, "shapes.rs");
    fx.open(&mut browser, shapes).await;
    assert_eq!(
        names(&browser, shapes),
        ["Circle", "Shape", "impl Shape for Circle", "impl Square"]
    );
    assert_eq!(providers(&browser, shapes), [Provider::DocumentSymbols]);
    assert!(!level(&browser, shapes).skeleton);
    assert_eq!(
        kinds(&browser, shapes),
        [Kind::Struct, Kind::Interface, Kind::Object, Kind::Object]
    );
    let requests_so_far = fx.log().count("textDocument/documentSymbol");
    assert_eq!(requests_so_far, 2);

    // Nested children come with their parent: no request.
    let circle = child(&browser, shapes, "Circle");
    fx.open(&mut browser, circle).await;
    assert_eq!(names(&browser, circle), ["radius"]);
    assert_eq!(kinds(&browser, circle), [Kind::Field]);
    assert_eq!(providers(&browser, circle), [Provider::DocumentSymbols]);
    assert_eq!(level(&browser, circle).requests, 0);
    let implementation = child(&browser, shapes, "impl Shape for Circle");
    fx.open(&mut browser, implementation).await;
    assert_eq!(names(&browser, implementation), ["area"]);
    assert_eq!(
        browser.display_path(child(&browser, implementation, "area")),
        "src/shapes.rs: impl Shape for Circle::area"
    );

    fx.until_log("shapes.rs was closed", |log| {
        log.count("textDocument/didClose") == 1
    })
    .await;
    assert_eq!(
        fx.ws.trace(),
        [
            "textDocument/didOpen src/lib.rs",
            "textDocument/documentSymbol src/lib.rs",
            "textDocument/didOpen src/shapes.rs",
            "textDocument/documentSymbol src/shapes.rs",
            "textDocument/didClose src/shapes.rs",
        ]
    );
    drop(browser);
    fx.finish().await
}

// ================================================================================================
// 2. Lazy loading.

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_asked_until_a_level_is_opened_and_each_level_asks_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script =
        path_query_script(&ws).completion("let total = ", completion_list(&[("answer", 3)]));
    let options = Options {
        at: Some(Position::new(3, 16)),
        ..Options::default()
    };
    let mut fx = ws.start(script, options).await?;
    let mut browser = fx.browser();
    fx.harness.pump_for(Duration::from_millis(300)).await;
    // Creating the browser asked nothing: only the editor's own didOpen.
    assert_eq!(fx.ws.trace(), ["textDocument/didOpen src/lib.rs"]);

    let counts = |fx: &Fixture| {
        let log = fx.log();
        (
            log.count("workspace/symbol"),
            log.count("textDocument/completion"),
            log.count("textDocument/documentSymbol"),
        )
    };

    // Local levels ask nothing.
    let codebase = root(&browser, "Codebase");
    fx.open(&mut browser, codebase).await;
    let src = child(&browser, codebase, "src");
    fx.open(&mut browser, src).await;
    assert_eq!(counts(&fx), (0, 0, 0));

    // One request per level; the same node again asks nothing.
    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    assert_eq!(counts(&fx), (1, 0, 0));
    fx.open(&mut browser, packages).await;
    assert_eq!(counts(&fx), (1, 0, 0));

    // Twice while it loads: still one request.
    let geo = child(&browser, packages, "geo");
    browser.open(geo);
    assert!(matches!(browser.children(geo), State::Loading));
    browser.open(geo);
    fx.settle(&mut browser).await;
    fx.open(&mut browser, geo).await;
    assert_eq!(counts(&fx), (2, 0, 0));
    assert_eq!(queries(&fx.log()), ["::", "geo::"]);

    // The roots of the standard library are the language's `api-roots`: no request, the
    // separator query was answered already. Each one asks for its items when opened.
    let std = root(&browser, "Std");
    fx.open(&mut browser, std).await;
    assert_eq!(names(&browser, std), ["std", "core", "alloc"]);
    assert_eq!(kinds(&browser, std), [Kind::Package; 3]);
    assert_eq!(level(&browser, std).requests, 0);
    assert_eq!(counts(&fx), (2, 0, 0));
    let std_std = child(&browser, std, "std");
    fx.open(&mut browser, std_std).await;
    assert_eq!(counts(&fx), (3, 0, 0));
    assert_eq!(queries(&fx.log()), ["::", "geo::", "std::"]);

    let scope = root(&browser, "Scope");
    fx.open(&mut browser, scope).await;
    assert_eq!(counts(&fx), (3, 1, 0));
    fx.open(&mut browser, scope).await;
    assert_eq!(counts(&fx), (3, 1, 0));

    let shapes = child(&browser, src, "shapes.rs");
    fx.open(&mut browser, shapes).await;
    assert_eq!(counts(&fx), (3, 1, 1));
    // The nested children of a symbol come with it.
    let circle = child(&browser, shapes, "Circle");
    fx.open(&mut browser, circle).await;
    assert_eq!(counts(&fx), (3, 1, 1));

    // Docs: only for the node asked, one hover.
    assert_eq!(fx.log().count("textDocument/hover"), 0);
    let geo_circle = child(&browser, geo, "Circle");
    let geo_shape = child(&browser, geo, "Shape");
    browser.ask_docs(geo_circle);
    fx.settle(&mut browser).await;
    let State::Ready(docs) = browser.docs(geo_circle) else {
        panic!("{:?}", browser.docs(geo_circle));
    };
    assert_eq!(docs.text.as_deref(), Some("A circle."));
    assert_eq!(docs.source, "hover");
    assert!(matches!(browser.docs(geo_shape), State::NotAsked));
    assert!(matches!(browser.docs(geo), State::NotAsked));
    assert_eq!(fx.log().count("textDocument/hover"), 1);
    // Asked again: nothing more.
    browser.ask_docs(geo_circle);
    fx.settle(&mut browser).await;
    assert_eq!(fx.log().count("textDocument/hover"), 1);

    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_docs_for_another_node_cancels_the_docs_in_flight() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws).delay("textDocument/hover", 800);
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();

    let geo = fx.open_path(&mut browser, &["Packages", "geo"]).await;
    let circle = child(&browser, geo, "Circle");
    let shape = child(&browser, geo, "Shape");

    browser.ask_docs(circle);
    assert!(matches!(browser.docs(circle), State::Loading));
    fx.until_log("the first hover was sent", |log| {
        log.count("textDocument/hover") == 1
    })
    .await;
    let first = request_ids(&fx.log(), "textDocument/hover")[0].clone();

    browser.ask_docs(shape);
    // The first is no longer wanted.
    assert!(matches!(browser.docs(circle), State::NotAsked));
    assert!(matches!(browser.docs(shape), State::Loading));
    assert_eq!(browser.in_flight(), 1);
    fx.until_log("the first hover was cancelled", |log| {
        log.cancelled() == [first.clone()]
    })
    .await;
    assert_eq!(cancel_notifications(&fx.log()), [first]);

    fx.settle(&mut browser).await;
    let State::Ready(docs) = browser.docs(shape) else {
        panic!("{:?}", browser.docs(shape));
    };
    assert_eq!(docs.text.as_deref(), Some("A shape."));
    assert!(matches!(browser.docs(circle), State::NotAsked));
    assert_eq!(fx.log().count("textDocument/hover"), 2);
    // Both temporary documents of shapes.rs are closed.
    fx.until_log("both documents were closed", |log| {
        log.count("textDocument/didClose") == 2
    })
    .await;
    assert_requests_in_open_documents(&fx.log());

    drop(browser);
    fx.finish().await
}

// ================================================================================================
// 3. Cancellation on close.

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_browser_cancels_a_workspace_symbol_request() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws).delay("workspace/symbol", 3000);
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();

    let packages = root(&browser, "Packages");
    browser.open(packages);
    fx.until_log("the request is in flight", |log| {
        log.count("workspace/symbol") == 1
    })
    .await;
    assert_eq!(browser.in_flight(), 1);
    let id = request_ids(&fx.log(), "workspace/symbol")[0].clone();

    drop(browser);
    fx.until_log("the request was cancelled", |log| {
        log.cancelled() == [id.clone()]
    })
    .await;
    assert_eq!(cancel_notifications(&fx.log()), [id]);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_browser_cancels_a_probe_completion_and_closes_its_document(
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = probe_script().delay("textDocument/completion", 3000);
    let mut fx = ws.start(script, probe_options()).await?;
    let mut browser = fx.browser();

    let packages = root(&browser, "Packages");
    browser.open(packages);
    fx.until_log("the completion is in flight", |log| {
        log.count("textDocument/completion") == 1
    })
    .await;
    let id = request_ids(&fx.log(), "textDocument/completion")[0].clone();
    // The temporary document is open on the server.
    assert_eq!(
        fx.ws.trace(),
        [
            "textDocument/didOpen src/lib.rs",
            "textDocument/didOpen vortex_api_probe_PID_0.rs",
            "textDocument/completion vortex_api_probe_PID_0.rs",
        ]
    );

    drop(browser);
    fx.until_log("the request was cancelled and the document closed", |log| {
        log.cancelled() == [id.clone()] && log.count("textDocument/didClose") == 1
    })
    .await;
    assert_eq!(cancel_notifications(&fx.log()), [id]);
    assert_eq!(
        fx.ws.trace().last().map(String::as_str),
        Some("textDocument/didClose vortex_api_probe_PID_0.rs")
    );
    assert_requests_in_open_documents(&fx.log());
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_the_picker_with_escape_cancels_what_is_in_flight() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = probe_script().delay("textDocument/completion", 3000);
    let mut fx = ws.start(script, probe_options()).await?;

    fx.harness.keys(":api-browse<ret>").await?;
    fx.harness
        .until("the picker lists the roots", |app| {
            screen_contains(app, "Packages")
        })
        .await;
    // Packages is the third root: open it, and its completion is in flight.
    fx.harness.keys("jjl").await?;
    fx.until_log("the completion is in flight", |log| {
        log.count("textDocument/completion") == 1
    })
    .await;
    let id = request_ids(&fx.log(), "textDocument/completion")[0].clone();

    fx.harness.keys("<esc>").await?;
    fx.until_log("the request was cancelled and the document closed", |log| {
        log.cancelled() == [id.clone()] && log.count("textDocument/didClose") == 1
    })
    .await;
    assert_eq!(cancel_notifications(&fx.log()), [id]);
    assert!(
        !screen_contains(&fx.harness.app, "Codebase"),
        "the picker is closed"
    );
    assert_requests_in_open_documents(&fx.log());
    fx.finish().await
}

// ================================================================================================
// 4. Temporary documents.

/// A probe level: didOpen of a `vortex_api_probe_` document (in `dir`, when set), a completion
/// in it, then didClose.
async fn a_probe_level_opens_asks_and_closes(dir: Option<&str>) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let dir_key = dir.map_or(String::new(), |dir| format!("dir = {}, ", Value::from(dir)));
    let options = Options::language(&format!(
        "api-probe = {{ {dir_key}packages = \"use {{module}}{{cursor}}\" }}"
    ));
    let mut fx = ws.start(probe_script(), options).await?;
    let mut browser = fx.browser();
    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    assert_eq!(names(&browser, packages), ["regex", "serde"]);
    fx.until_log("the document was closed", |log| {
        log.count("textDocument/didClose") == 1
    })
    .await;

    let prefix = dir.map_or(String::new(), |dir| format!("{dir}/"));
    let temporary = format!("{prefix}vortex_api_probe_PID_0.rs");
    assert_eq!(
        fx.ws.trace(),
        [
            "textDocument/didOpen src/lib.rs".to_string(),
            format!("textDocument/didOpen {temporary}"),
            format!("textDocument/completion {temporary}"),
            format!("textDocument/didClose {temporary}"),
        ]
    );
    // What it opened: the template, with the language of the buffer; the position is at the
    // cursor marker.
    let log = fx.log();
    let opened = &log.notifications("textDocument/didOpen")[1];
    assert_eq!(opened["textDocument"]["text"], "use ");
    assert_eq!(opened["textDocument"]["languageId"], "rust");
    let asked = &log.requests("textDocument/completion")[0];
    assert_eq!(asked["position"], json!({"line": 0, "character": 4}));
    // Nothing of it is on disk.
    assert!(std::fs::read_dir(fx.ws.root())?
        .flatten()
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .contains("vortex_api_probe")));
    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_probe_document_is_opened_asked_and_closed() -> anyhow::Result<()> {
    a_probe_level_opens_asks_and_closes(None).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_probe_document_goes_under_the_probe_dir() -> anyhow::Result<()> {
    a_probe_level_opens_asks_and_closes(Some("vortex_api_probe")).await
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_of_temporary_documents_never_reach_the_editor() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = probe_script().diagnostics_on_open(true);
    let options = Options::language("api-probe = { packages = \"use {module}{cursor}\" }");
    let mut fx = ws.start(script, options).await?;

    // The diagnostics of a real buffer do arrive.
    fx.harness
        .until("the diagnostics of src/lib.rs arrived", |app| {
            app.editor
                .diagnostics
                .keys()
                .any(|uri| uri.to_string().ends_with("src/lib.rs"))
        })
        .await;

    let mut browser = fx.browser();
    let packages = root(&browser, "Packages");
    fx.open(&mut browser, packages).await;
    assert_eq!(names(&browser, packages), ["regex", "serde"]);
    fx.until_log("the document was closed", |log| {
        log.count("textDocument/didClose") == 1
    })
    .await;
    // The server published diagnostics on the opening and on the closing; give them time.
    fx.harness.pump_for(Duration::from_millis(500)).await;

    let leftover: Vec<String> = fx
        .harness
        .app
        .editor
        .diagnostics
        .keys()
        .map(|uri| uri.to_string())
        .filter(|uri| uri.contains("vortex_api_probe"))
        .collect();
    assert!(leftover.is_empty(), "{leftover:?}");
    assert!(
        fx.harness
            .app
            .editor
            .diagnostics
            .keys()
            .any(|uri| uri.to_string().ends_with("src/lib.rs")),
        "the buffer's diagnostics are still there"
    );
    // The editor knows the document: still hidden a while after it was closed.
    let log = fx.log();
    let uri = log.notifications("textDocument/didClose")[0]["textDocument"]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = vortex_lsp::lsp::Url::parse(&uri)?;
    assert!(fx
        .harness
        .app
        .editor
        .language_servers
        .hidden_documents
        .contains(&uri));

    drop(browser);
    fx.finish().await
}

// ================================================================================================
// 5. The user's buffers are never changed.

#[tokio::test(flavor = "multi_thread")]
async fn browsing_never_changes_or_reopens_the_buffer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script =
        path_query_script(&ws).completion("let total = ", completion_list(&[("answer", 3)]));
    let options = Options {
        at: Some(Position::new(3, 16)),
        ..Options::default()
    };
    let mut fx = ws.start(script, options).await?;
    let (text, version) = {
        let doc = doc!(fx.harness.app.editor);
        (doc.text().to_string(), doc.version())
    };
    assert_eq!(text, LIB_RS);

    let mut browser = fx.browser();
    // Every root and some levels: the symbols of the open file and of one that is not open,
    // a type's members (the same other file again), the docs of the open file's item.
    for name in ["Codebase", "Scope", "Packages", "Std"] {
        let id = root(&browser, name);
        fx.open(&mut browser, id).await;
    }
    let lib = fx
        .open_path(&mut browser, &["Codebase", "src", "lib.rs"])
        .await;
    let shapes = fx
        .open_path(&mut browser, &["Codebase", "src", "shapes.rs"])
        .await;
    let circle = child(&browser, shapes, "Circle");
    fx.open(&mut browser, circle).await;
    let geo = fx.open_path(&mut browser, &["Packages", "geo"]).await;
    let geo_circle = child(&browser, geo, "Circle");
    fx.open(&mut browser, geo_circle).await;
    assert_eq!(names(&browser, geo_circle), ["radius", "area"]);
    let serde = child(&browser, root(&browser, "Packages"), "serde");
    browser.ask_docs(serde);
    browser.ask_definition(serde);
    fx.settle(&mut browser).await;
    browser.ask_docs(child(&browser, lib, "answer"));
    fx.settle(&mut browser).await;
    let std = fx.open_path(&mut browser, &["Std", "std"]).await;
    let _ = std;
    fx.harness.pump_for(Duration::from_millis(200)).await;
    fx.until_log("every temporary document is closed", |log| {
        log.count("textDocument/didOpen") == log.count("textDocument/didClose") + 1
    })
    .await;

    {
        let doc = doc!(fx.harness.app.editor);
        assert_eq!(doc.text().to_string(), text);
        assert_eq!(doc.version(), version);
        assert!(!doc.is_modified());
    }
    let log = fx.log();
    let trace = trace(&log, fx.ws.root());
    assert_eq!(count(&trace, "textDocument/didChange"), 0, "{trace:#?}");
    assert_eq!(
        count(&trace, "textDocument/didOpen src/lib.rs"),
        1,
        "only the editor's: {trace:#?}"
    );
    assert_eq!(
        count(&trace, "textDocument/didClose src/lib.rs"),
        0,
        "{trace:#?}"
    );
    // The other file is opened for each of its requests and closed after: its symbols for the
    // Codebase, and again for the members of `geo::Circle`.
    let opened = count(&trace, "textDocument/didOpen src/shapes.rs");
    assert_eq!(opened, 2, "{trace:#?}");
    assert_eq!(opened, count(&trace, "textDocument/didClose src/shapes.rs"));
    assert_eq!(
        count(&trace, "textDocument/documentSymbol src/shapes.rs"),
        2
    );
    assert_eq!(count(&trace, "textDocument/documentSymbol src/lib.rs"), 1);
    assert_requests_in_open_documents(&log);

    drop(browser);
    fx.finish().await
}

// ================================================================================================
// 6. `api-params` and `api-config`.

#[tokio::test(flavor = "multi_thread")]
async fn api_params_are_merged_into_workspace_symbol_and_completion_requests() -> anyhow::Result<()>
{
    let ws = Workspace::new();
    let script =
        path_query_script(&ws).completion("let total = ", completion_list(&[("answer", 3)]));
    let options = Options {
        server: "api-params = { limit = 0 }".into(),
        at: Some(Position::new(3, 16)),
        ..Options::default()
    };
    let mut fx = ws.start(script, options).await?;
    let mut browser = fx.browser();
    fx.open_path(&mut browser, &["Packages", "geo"]).await;
    fx.open_path(&mut browser, &["Scope"]).await;
    fx.open_path(&mut browser, &["Codebase", "src", "shapes.rs"])
        .await;

    let log = fx.log();
    let symbol_requests = log.requests("workspace/symbol");
    assert_eq!(symbol_requests.len(), 2);
    for params in &symbol_requests {
        assert_eq!(params["limit"], 0, "{params}");
        assert!(
            params["query"].is_string(),
            "the request's own fields stay: {params}"
        );
    }
    let completion_requests = log.requests("textDocument/completion");
    assert_eq!(completion_requests.len(), 1);
    assert_eq!(completion_requests[0]["limit"], 0);
    assert!(completion_requests[0]["textDocument"].is_object());
    assert!(completion_requests[0]["position"].is_object());
    // Only those two requests get them.
    for params in log.requests("textDocument/documentSymbol") {
        assert!(params.get("limit").is_none(), "{params}");
    }
    drop(browser);
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn api_params_reach_the_completions_of_probe_documents() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let options = Options {
        server: "api-params = { limit = 0 }".into(),
        language: "api-probe = { packages = \"use {module}{cursor}\" }".into(),
        ..Options::default()
    };
    let mut fx = ws.start(probe_script(), options).await?;
    let mut browser = fx.browser();
    fx.open_path(&mut browser, &["Packages"]).await;
    let log = fx.log();
    let requests = log.requests("textDocument/completion");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["limit"], 0);
    drop(browser);
    fx.finish().await
}

/// The answers of the editor to the server's `workspace/configuration` requests (the server asks
/// after each `workspace/didChangeConfiguration`), in order: its `fake` section.
fn configuration_answers(log: &Log) -> Vec<Value> {
    log.0
        .iter()
        .filter_map(|line| line.get("message"))
        .filter(|message| message.get("method").is_none() && message["id"].as_u64() >= Some(1000))
        .filter_map(|message| message["result"].as_array())
        .map(|result| result[0].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn api_config_is_the_servers_configuration_while_the_browser_is_open() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws).pull_configuration(true);
    let options = Options::server(
        "config = { fake = { base = true, limit = 10 } }\n\
         api-config = { fake = { limit = 1000 } }",
    );
    let mut fx = ws.start(script, options).await?;
    // The editor tells the server its configuration once it is initialized, and answers its
    // question: without the overlay.
    fx.until_log("the base configuration was asked", |log| {
        !configuration_answers(log).is_empty()
    })
    .await;
    assert_eq!(
        configuration_answers(&fx.log()),
        [json!({"base": true, "limit": 10})]
    );

    let browser = fx.browser();
    fx.until_log("the overlay was answered", |log| {
        configuration_answers(log).last() == Some(&json!({"base": true, "limit": 1000}))
    })
    .await;
    let changes_while_open = fx.log().count("workspace/didChangeConfiguration");

    drop(browser);
    fx.until_log("the overlay was taken away", |log| {
        configuration_answers(log).last() == Some(&json!({"base": true, "limit": 10}))
    })
    .await;
    let log = fx.log();
    assert_eq!(
        log.count("workspace/didChangeConfiguration"),
        changes_while_open + 1
    );
    assert_eq!(
        configuration_answers(&log),
        [
            json!({"base": true, "limit": 10}),
            json!({"base": true, "limit": 1000}),
            json!({"base": true, "limit": 10}),
        ]
    );
    fx.finish().await
}

// ================================================================================================
// 7. The members of a type.

#[tokio::test(flavor = "multi_thread")]
async fn a_type_from_a_path_query_lists_its_fields_and_the_methods_of_its_impl_blocks(
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws);
    let mut fx = ws.start(script, Options::default()).await?;
    let mut browser = fx.browser();
    let geo = fx.open_path(&mut browser, &["Packages", "geo"]).await;
    let circle = child(&browser, geo, "Circle");
    assert!(browser.node(circle).has_children());

    // Its place is known from the workspace symbol: nothing is asked for it.
    browser.ask_definition(circle);
    let State::Ready(Some(place)) = browser.definition(circle) else {
        panic!("{:?}", browser.definition(circle));
    };
    assert!(place.uri.path().ends_with("src/shapes.rs"));
    assert_eq!(place.range.start.line, 0);
    assert_eq!(place.range.start.character, 11);
    assert_eq!(browser.in_flight(), 0);
    assert_eq!(fx.log().count("textDocument/definition"), 0);

    // Its members: the document symbols of its file; the field, and the method of the
    // `impl Shape for Circle` block. The trait's method and `impl Square` are not its.
    fx.open(&mut browser, circle).await;
    assert_eq!(names(&browser, circle), ["radius", "area"]);
    assert_eq!(kinds(&browser, circle), [Kind::Field, Kind::Method]);
    assert_eq!(providers(&browser, circle), [Provider::Definition]);
    assert_eq!(level(&browser, circle).requests, 1);
    let area = child(&browser, circle, "area");
    let place = browser.node(area).place.as_ref().expect("a place");
    assert_eq!(place.range.start.line, 9, "the method of the impl block");
    assert_eq!(browser.display_path(area), "geo::Circle::area");

    // Their docs are hovers at their places.
    browser.ask_docs(area);
    fx.settle(&mut browser).await;
    let State::Ready(docs) = browser.docs(area) else {
        panic!("{:?}", browser.docs(area));
    };
    assert_eq!(docs.text.as_deref(), Some("The area of the circle."));

    fx.until_log("the files are closed", |log| {
        log.count("textDocument/didClose") == 2
    })
    .await;
    let trace = fx.ws.trace();
    assert_eq!(
        count(&trace, "textDocument/documentSymbol src/shapes.rs"),
        1
    );
    assert_eq!(count(&trace, "textDocument/hover src/shapes.rs"), 1);
    assert_requests_in_open_documents(&fx.log());
    drop(browser);
    fx.finish().await
}

// ================================================================================================
// 8. `:api-browse` end to end.

/// The screen's text, one string per row.
fn screen_rows(app: &Application) -> Vec<String> {
    let screen = app.screen();
    (0..screen.area.height)
        .map(|y| {
            (0..screen.area.width)
                .map(|x| screen.get(x, y).map_or(" ", |cell| cell.symbol.as_str()))
                .collect()
        })
        .collect()
}

fn screen_contains(app: &Application, text: &str) -> bool {
    screen_rows(app).iter().any(|row| row.contains(text))
}

/// The first row with `text`.
fn row_with(app: &Application, text: &str) -> Option<String> {
    screen_rows(app).into_iter().find(|row| row.contains(text))
}

#[tokio::test(flavor = "multi_thread")]
async fn api_browse_picker_end_to_end() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let script = path_query_script(&ws);
    let mut fx = ws.start(script, Options::default()).await?;

    // The roots.
    fx.harness.keys(":api-browse<ret>").await?;
    fx.harness
        .until("the roots are listed", |app| {
            ["Codebase", "Scope", "Packages", "Std"]
                .iter()
                .all(|root| screen_contains(app, root))
        })
        .await;
    assert!(
        screen_contains(&fx.harness.app, "rust"),
        "the info row names the language: {:#?}",
        screen_rows(&fx.harness.app)
    );

    // `l` on Packages lists them, the info row says how many and where from.
    fx.harness.keys("jj").await?;
    fx.harness.keys("l").await?;
    fx.harness
        .until("the packages are listed", |app| {
            screen_contains(app, "geo") && screen_contains(app, "serde")
        })
        .await;
    // The prompt row: the level's path, the provider and how long it took; the picker counts
    // the items.
    let info = row_with(&fx.harness.app, "Packages · ").unwrap_or_else(|| {
        panic!("no info row: {:#?}", screen_rows(&fx.harness.app));
    });
    assert!(info.contains("Packages · path query · "), "{info}");
    assert!(info.contains(" ms"), "{info}");
    assert!(info.contains("2/2"), "{info}");

    // `j` moves; the preview shows the docs of the selected item (a hover at its place).
    fx.harness.keys("j").await?;
    fx.harness
        .until("the docs of serde are previewed", |app| {
            screen_contains(app, "Serialization.")
        })
        .await;
    let rows = screen_rows(&fx.harness.app);
    assert!(rows.iter().any(|row| row.contains("serde")), "{rows:#?}");
    assert!(
        rows.iter().any(|row| row.contains("package")),
        "the preview shows the kind: {rows:#?}"
    );
    assert!(rows
        .iter()
        .any(|row| row.contains("workspace symbols (path query)")));
    // And back: `k`.
    fx.harness.keys("k").await?;
    fx.harness
        .until("the docs of geo are previewed", |app| {
            screen_contains(app, "The geo crate.")
        })
        .await;
    assert!(!screen_contains(&fx.harness.app, "Serialization."));

    // `l` again: the items of geo; `h`: back to the packages, and `h` again to the roots.
    fx.harness.keys("l").await?;
    fx.harness
        .until("geo's items are listed", |app| {
            screen_contains(app, "Circle") && screen_contains(app, "Shape")
        })
        .await;
    fx.harness.keys("h").await?;
    fx.harness
        .until("the packages are back", |app| {
            screen_contains(app, "serde") && !screen_contains(app, "Circle")
        })
        .await;
    fx.harness.keys("h").await?;
    fx.harness
        .until("the roots are back", |app| {
            screen_contains(app, "Codebase") && !screen_contains(app, "serde")
        })
        .await;

    // `?`: the key help; `Esc` closes it.
    fx.harness.keys("?").await?;
    fx.harness
        .until("the key help is shown", |app| {
            screen_contains(app, "go to the definition")
        })
        .await;
    fx.harness.keys("<esc>").await?;
    fx.harness
        .until("the key help is closed", |app| {
            !screen_contains(app, "go to the definition") && screen_contains(app, "Codebase")
        })
        .await;

    // `Enter` on a node without a place opens it. Going back selected Packages, the root we
    // came from.
    fx.harness.keys("<ret>").await?;
    fx.harness
        .until("Enter opened Packages", |app| screen_contains(app, "serde"))
        .await;
    // `Enter` on geo (a module with a place) goes to its definition: src/shapes.rs, line 1.
    fx.harness.keys("<ret>").await?;
    fx.harness
        .until("the file of geo is open", |app| {
            doc!(app.editor)
                .path()
                .is_some_and(|path| path.ends_with("src/shapes.rs"))
        })
        .await;
    assert!(
        !screen_contains(&fx.harness.app, "serde"),
        "the picker is closed"
    );
    {
        let doc = doc!(fx.harness.app.editor);
        let view = vortex_view::view!(fx.harness.app.editor);
        let text = doc.text();
        let cursor = doc.selection(view.id).primary().cursor(text.slice(..));
        assert_eq!(text.char_to_line(cursor), 0);
    }

    // Again, to an item deeper in the file: Circle's sibling Shape is on line 5 of the file.
    fx.harness.keys(":api-browse<ret>").await?;
    fx.harness
        .until("the picker is back", |app| screen_contains(app, "Packages"))
        .await;
    fx.harness.keys("jjl").await?;
    fx.harness
        .until("the packages are listed", |app| {
            screen_contains(app, "serde")
        })
        .await;
    fx.harness.keys("l").await?;
    fx.harness
        .until("geo's items are listed", |app| {
            screen_contains(app, "Shape")
        })
        .await;
    // `util` (a module of src/lib.rs), `Circle`, `Shape`: the third.
    fx.harness.keys("jj<ret>").await?;
    fx.harness
        .until("Shape's definition is open", |app| {
            let view = vortex_view::view!(app.editor);
            let doc = doc!(app.editor);
            let text = doc.text();
            let cursor = doc.selection(view.id).primary().cursor(text.slice(..));
            doc.path()
                .is_some_and(|path| path.ends_with("src/shapes.rs"))
                && text.char_to_line(cursor) == 4
        })
        .await;
    {
        let doc = doc!(fx.harness.app.editor);
        let view = vortex_view::view!(fx.harness.app.editor);
        let range = doc.selection(view.id).primary();
        let selected: String = range.fragment(doc.text().slice(..)).into();
        assert_eq!(selected, "Shape");
    }
    assert!(
        !screen_contains(&fx.harness.app, "Packages"),
        "the picker is closed"
    );

    // `Esc` closes the picker.
    fx.harness.keys(":api-browse<ret>").await?;
    fx.harness
        .until("the picker is back", |app| screen_contains(app, "Codebase"))
        .await;
    fx.harness.keys("<esc>").await?;
    fx.harness
        .until("the picker is closed", |app| {
            !screen_contains(app, "Codebase")
        })
        .await;
    fx.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn api_browse_in_a_scratch_buffer_shows_an_error() -> anyhow::Result<()> {
    // A scratch buffer has no language.
    let mut harness = Harness::new(AppBuilder::new().with_config(test_config()))?;
    harness.keys(":api-browse<ret>").await?;
    harness
        .until("the error is shown", |app| {
            app.editor
                .get_status()
                .is_some_and(|(message, _)| message.starts_with("API browser:"))
        })
        .await;
    assert!(
        harness
            .errors
            .iter()
            .any(|error| error.starts_with("API browser:")),
        "{:?}",
        harness.errors
    );
    assert!(!screen_contains(&harness.app, "Codebase"));
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn api_browse_without_a_running_language_server_shows_an_error() -> anyhow::Result<()> {
    // A Rust file whose language server is not running (the tests' default turns them off).
    let ws = Workspace::new();
    let mut harness = Harness::new(
        AppBuilder::new()
            .with_config(test_config())
            .with_file(ws.path("src/lib.rs"), None),
    )?;
    harness.keys(":api-browse<ret>").await?;
    harness
        .until("the error is shown", |app| {
            app.editor
                .get_status()
                .is_some_and(|(message, _)| message.starts_with("API browser: no language server"))
        })
        .await;
    assert!(
        harness
            .errors
            .iter()
            .any(|error| error.starts_with("API browser: no language server")),
        "{:?}",
        harness.errors
    );
    assert!(!screen_contains(&harness.app, "Codebase"));
    harness.finish().await
}
