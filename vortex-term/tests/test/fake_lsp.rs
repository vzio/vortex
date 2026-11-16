//! Helpers for the tests that need a language server: a [`Script`] for the scripted fake server
//! `vortex-fake-lsp` (`tests/fake_lsp/main.rs`, whose module doc has the script format), the
//! [`Log`] of the messages it received, and [`languages_toml`] to make the editor use it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The path of the fake server's binary.
pub const SERVER: &str = env!("CARGO_BIN_EXE_vortex-fake-lsp");

/// Builds the JSON script of the fake language server.
#[derive(Default, Clone)]
pub struct Script {
    capabilities: Value,
    workspace_symbols: Vec<Value>,
    document_symbols: Vec<Value>,
    completions: Vec<Value>,
    resolve: Vec<Value>,
    hover: Vec<Value>,
    definition: Vec<Value>,
    diagnostics_on_open: bool,
    delays_ms: Map<String, Value>,
    pull_configuration: bool,
}

impl Script {
    pub fn new() -> Self {
        Self {
            capabilities: json!({}),
            ..Default::default()
        }
    }

    /// The `ServerCapabilities` answered by `initialize`.
    pub fn capabilities(mut self, capabilities: Value) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// `workspace/symbol` with exactly this query answers `result`.
    pub fn workspace_symbol(mut self, query: &str, result: Value) -> Self {
        self.workspace_symbols
            .push(json!({"query": query, "result": result}));
        self
    }

    /// `textDocument/documentSymbol` of a uri ending in `uri_suffix` (only while the document is
    /// open, as clangd does).
    pub fn document_symbols(mut self, uri_suffix: &str, result: Value) -> Self {
        self.document_symbols
            .push(json!({"uri_suffix": uri_suffix, "result": result, "needs_open": true}));
        self
    }

    /// Like [`Script::document_symbols`], answered whether the document is open or not.
    pub fn document_symbols_closed(mut self, uri_suffix: &str, result: Value) -> Self {
        self.document_symbols
            .push(json!({"uri_suffix": uri_suffix, "result": result, "needs_open": false}));
        self
    }

    /// Completion when the text of the line before the cursor ends with `before`.
    pub fn completion(mut self, before: &str, result: Value) -> Self {
        self.completions
            .push(json!({"before": before, "result": result}));
        self
    }

    /// Like [`Script::completion`], only for uris containing `uri_contains`.
    pub fn completion_in(mut self, before: &str, uri_contains: &str, result: Value) -> Self {
        self.completions
            .push(json!({"before": before, "uri_contains": uri_contains, "result": result}));
        self
    }

    /// `completionItem/resolve` of the item with this label.
    pub fn resolve(mut self, label: &str, result: Value) -> Self {
        self.resolve.push(json!({"label": label, "result": result}));
        self
    }

    /// A hover entry; `matcher` holds the matching fields (`uri_suffix`, `uri_contains`, `line`,
    /// `character`, `before`), `markdown` is the text answered.
    pub fn hover(mut self, matcher: Value, markdown: &str) -> Self {
        self.hover.push(entry(matcher, Value::from(markdown)));
        self
    }

    /// A definition entry; `matcher` as for [`Script::hover`], `location` the answer.
    pub fn definition(mut self, matcher: Value, location: Value) -> Self {
        self.definition.push(entry(matcher, location));
        self
    }

    pub fn diagnostics_on_open(mut self, on: bool) -> Self {
        self.diagnostics_on_open = on;
        self
    }

    /// Waits `ms` before answering requests of `method`.
    pub fn delay(mut self, method: &str, ms: u64) -> Self {
        self.delays_ms.insert(method.to_string(), json!(ms));
        self
    }

    /// Asks the client for `workspace/configuration` after each `didChangeConfiguration`.
    pub fn pull_configuration(mut self, on: bool) -> Self {
        self.pull_configuration = on;
        self
    }

    /// The path of the log written next to the script by [`Script::write`].
    pub fn log_path(dir: &Path) -> PathBuf {
        dir.join("fake-lsp-log.jsonl")
    }

    /// Writes the script (`fake-lsp-script.json`) into `dir`, with its log path set to
    /// [`Script::log_path`], and returns the script's path.
    pub fn write(&self, dir: &Path) -> PathBuf {
        let script = json!({
            "capabilities": self.capabilities,
            "workspace_symbols": self.workspace_symbols,
            "document_symbols": self.document_symbols,
            "completions": self.completions,
            "resolve": self.resolve,
            "hover": self.hover,
            "definition": self.definition,
            "diagnostics_on_open": self.diagnostics_on_open,
            "delays_ms": self.delays_ms,
            "pull_configuration": self.pull_configuration,
            "log": Self::log_path(dir),
        });
        let path = dir.join("fake-lsp-script.json");
        std::fs::write(&path, serde_json::to_string_pretty(&script).unwrap()).unwrap();
        path
    }
}

/// `matcher`'s fields plus the `result`.
fn entry(mut matcher: Value, result: Value) -> Value {
    matcher["result"] = result;
    matcher
}

/// The messages the fake server received (the lines of its log).
pub struct Log(pub Vec<Value>);

impl Log {
    /// Reads the log (empty when the server has not written it yet). Lines that are cut off by a
    /// concurrent write are skipped.
    pub fn read(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    pub fn load(path: &Path) -> Self {
        Self(Self::read(path))
    }

    fn messages(&self) -> impl Iterator<Item = &Value> {
        self.0.iter().filter_map(|line| line.get("message"))
    }

    /// The params of every request or notification with this method, in order of arrival.
    fn params_of(&self, method: &str, request: bool) -> Vec<Value> {
        self.messages()
            .filter(|m| m["method"] == method && m.get("id").is_some() == request)
            .map(|m| m.get("params").cloned().unwrap_or(Value::Null))
            .collect()
    }

    /// The params of each request with that method.
    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.params_of(method, true)
    }

    /// The params of each notification with that method.
    pub fn notifications(&self, method: &str) -> Vec<Value> {
        self.params_of(method, false)
    }

    /// How many requests and notifications with that method arrived.
    pub fn count(&self, method: &str) -> usize {
        self.messages().filter(|m| m["method"] == method).count()
    }

    /// The ids of the requests the server answered as cancelled.
    pub fn cancelled(&self) -> Vec<Value> {
        self.0
            .iter()
            .filter_map(|line| line.get("sent_cancelled").cloned())
            .collect()
    }
}

/// A `languages.toml` override (for `test_syntax_loader(Some(..))`) that defines the language
/// server `fake` (this binary with `script`) and makes `language` use only it. The default
/// languages merge by name, so the rest of the language stays as it is.
pub fn languages_toml(server_command: &str, script: &Path, language: &str) -> String {
    let command = Value::from(server_command);
    let script = Value::from(script.to_string_lossy().into_owned());
    format!(
        "[language-server.fake]\ncommand = {command}\nargs = [{script}]\n\n\
         [[language]]\nname = {}\nlanguage-servers = [\"fake\"]\n",
        Value::from(language),
    )
}

/// [`languages_toml`] with the fake server binary of the test build.
pub fn fake_languages_toml(script: &Path, language: &str) -> String {
    languages_toml(SERVER, script, language)
}

mod smoke {
    use std::time::Duration;

    use vortex_term::config::Config;

    use super::*;
    use crate::test::{
        debug::harness::Harness,
        helpers::{test_config, test_syntax_loader, AppBuilder},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn the_editor_starts_the_fake_language_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("src"))?;
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"fake\"\n",
        )?;
        let file = dir.path().join("src/lib.rs");
        std::fs::write(&file, "pub fn answer() -> u32 {\n    42\n}\n")?;

        let script = Script::new()
            .capabilities(json!({"documentSymbolProvider": true}))
            .document_symbols(
                "src/lib.rs",
                json!([{
                    "name": "answer",
                    "kind": 12,
                    "location": {
                        "uri": "file:///src/lib.rs",
                        "range": {"start": {"line": 0, "character": 0},
                                  "end": {"line": 2, "character": 1}},
                    },
                }]),
            );
        let script_path = script.write(dir.path());
        let log_path = Script::log_path(dir.path());

        let mut config: Config = test_config();
        config.editor.lsp.enable = true;
        let loader = test_syntax_loader(Some(fake_languages_toml(&script_path, "rust")));
        let builder = AppBuilder::new()
            .with_config(config)
            .with_lang_loader(loader)
            .with_file(&file, None);
        let mut harness = Harness::new(builder)?;

        harness
            .until("the fake language server is initialized", |app| {
                app.editor
                    .language_servers
                    .iter_clients()
                    .any(|client| client.is_initialized())
            })
            .await;
        // `initialized` and `didOpen` follow the answer to `initialize`.
        let mut log = Log::load(&log_path);
        for _ in 0..100 {
            if log.count("textDocument/didOpen") > 0 {
                break;
            }
            harness.pump_for(Duration::from_millis(50)).await;
            log = Log::load(&log_path);
        }

        assert_eq!(log.requests("initialize").len(), 1, "{:?}", log.0);
        assert_eq!(log.count("initialized"), 1, "{:?}", log.0);
        let opened = log.notifications("textDocument/didOpen");
        assert_eq!(opened.len(), 1, "{:?}", log.0);
        assert!(opened[0]["textDocument"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("src/lib.rs")));
        assert!(log.cancelled().is_empty());

        harness.finish().await
    }
}
