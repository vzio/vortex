//! A scripted fake language server for the integration tests (`vortex-fake-lsp <script.json>`).
//!
//! It speaks LSP over stdio (`Content-Length` framed JSON-RPC), so the editor starts it like any
//! real server (command + args). Everything it answers comes from the JSON script; tests build
//! the script and read the log with `tests/test/fake_lsp.rs`. Nothing but protocol messages goes
//! to stdout; stderr is for debugging. Every key of the script is optional.
//!
//! # Script format
//!
//! ```json
//! {
//!   "capabilities": { ... },
//!   "workspace_symbols": [ { "query": "::", "result": [ ... ] } ],
//!   "document_symbols": [ { "uri_suffix": "src/lib.rs", "result": [ ... ], "needs_open": true } ],
//!   "completions": [ { "before": "m.", "uri_contains": "lib", "result": { ... } } ],
//!   "resolve": [ { "label": "x", "result": { ... } } ],
//!   "hover": [ { "uri_suffix": "lib.rs", "uri_contains": "src", "line": 3, "character": 4,
//!                "before": "let x", "result": "markdown text" } ],
//!   "definition": [ { "uri_suffix": "..", "uri_contains": "..", "line": 3, "character": 4,
//!                     "before": "..", "result": { "uri": "file:///a.rs", "range": { ... } } } ],
//!   "diagnostics_on_open": false,
//!   "delays_ms": { "workspace/symbol": 500 },
//!   "pull_configuration": false,
//!   "log": "/path/to/log.jsonl"
//! }
//! ```
//!
//! # Rules
//!
//! * `initialize` answers `{"capabilities": <capabilities, verbatim; {} when absent>,
//!   "serverInfo": {"name": "vortex-fake-lsp"}}`. `shutdown` answers `null`. `exit` ends the
//!   process (status 0). Other requests that are not listed below are answered with the JSON-RPC
//!   error `-32601` ("method not found"). Notifications that are not listed are ignored (but
//!   logged). Responses of the client to the server's own requests are only logged.
//! * Documents: `textDocument/didOpen` stores the text of the document, `didChange` keeps it up
//!   to date (full changes without `range`, incremental ones with `range`; the changes of one
//!   notification apply in order; positions count characters, ASCII is assumed), `didClose`
//!   forgets it. "Open" below means didOpen'd and not didClose'd.
//! * `workspace/symbol`: the `result` (verbatim JSON: `SymbolInformation` or `WorkspaceSymbol`
//!   objects) of the `workspace_symbols` entry whose `query` equals the request's `query`
//!   exactly; otherwise `[]`.
//! * `textDocument/documentSymbol`: the `result` of the first `document_symbols` entry whose
//!   `uri_suffix` the request's uri ends with; `[]` when there is none. With `needs_open` true
//!   (the default) the answer is `[]` unless that document is open.
//! * `textDocument/completion`: needs the document open (else empty). The "line text" is the
//!   text of the request's line before the request's character. The first `completions` entry
//!   whose `before` the line text ends with (and whose `uri_contains`, when given, is in the
//!   uri) answers its `result` verbatim (a CompletionList or an array). No match:
//!   `{"isIncomplete": false, "items": []}`.
//! * `completionItem/resolve`: the `result` of the first `resolve` entry whose `label` equals
//!   the item's `label`; no match: the item is echoed back unchanged.
//! * `textDocument/hover`: the first `hover` entry that matches answers
//!   `{"contents": {"kind": "markdown", "value": <result>}}`; no match: `null`.
//! * `textDocument/definition`: the first `definition` entry that matches answers its `result`
//!   verbatim (a Location, or an array); no match: `null`.
//! * Matching of `hover`/`definition` entries: every given field must hold. `uri_suffix` /
//!   `uri_contains`: the request's uri ends with / contains it. `line` / `character`: equal to
//!   the request's position. `before`: the line text (see completion) ends with it; this needs
//!   the document open, so an entry with `before` never matches a closed document.
//! * `diagnostics_on_open`: after each `didOpen` the server sends `textDocument/publishDiagnostics`
//!   for that uri with one warning at 0:0-0:1 (message `"fake diagnostic"`, source
//!   `"vortex-fake-lsp"`), and after each `didClose` one with no diagnostics.
//! * `delays_ms`: method -> milliseconds to wait before answering a request of that method (the
//!   answer is computed on arrival, from the state at that time; waiting never blocks other
//!   messages). A `$/cancelRequest` for a request that still waits is answered at once with
//!   the error `-32800` ("cancelled"), the normal answer is not sent, and the log gets a
//!   `sent_cancelled` line. Cancelling a request that is not waiting does nothing.
//! * `pull_configuration`: after each `workspace/didChangeConfiguration` the server sends a
//!   `workspace/configuration` request with `items: [{"section": "fake"}]`; the client's answer
//!   is logged like every received message.
//! * `log`: a file path (truncated at start). Every received message (requests, notifications,
//!   responses to the server's own requests) is appended as one line
//!   `{"t_ms": <ms since start>, "message": <the raw JSON-RPC message>}`, flushed at once. When
//!   a request is answered as cancelled the line `{"t_ms": .., "sent_cancelled": <request id>}`
//!   is appended. Nothing the server sends is logged.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

struct Server {
    script: Value,
    start: Instant,
    out: Mutex<std::io::Stdout>,
    log: Mutex<Option<File>>,
    /// Requests that wait for their delay: id -> () (removed when answered or cancelled).
    waiting: Mutex<HashMap<String, ()>>,
    next_id: Mutex<u64>,
}

impl Server {
    fn send(&self, message: &Value) {
        let body = message.to_string();
        let mut out = self.out.lock().unwrap();
        let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = out.flush();
    }

    fn log(&self, mut entry: Value) {
        entry["t_ms"] = json!(self.start.elapsed().as_millis() as u64);
        if let Some(file) = self.log.lock().unwrap().as_mut() {
            let _ = writeln!(file, "{entry}");
            let _ = file.flush();
        }
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        );
    }

    fn list(&self, key: &str) -> Vec<Value> {
        self.script
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn flag(&self, key: &str) -> bool {
        self.script
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// The offset in chars of `line`/`character` in `text` (clamped to the end of the line/text).
fn offset_of(text: &str, line: usize, character: usize) -> usize {
    let mut offset = 0;
    for (n, l) in text.split_inclusive('\n').enumerate() {
        if n == line {
            let content = l.trim_end_matches(['\n', '\r']).chars().count();
            return offset + character.min(content);
        }
        offset += l.chars().count();
    }
    text.chars().count()
}

fn apply_change(text: &mut String, change: &Value) {
    let new = change.get("text").and_then(Value::as_str).unwrap_or("");
    let Some(range) = change.get("range").filter(|r| !r.is_null()) else {
        *text = new.to_string();
        return;
    };
    let pos = |p: &Value| {
        offset_of(
            text,
            p["line"].as_u64().unwrap_or(0) as usize,
            p["character"].as_u64().unwrap_or(0) as usize,
        )
    };
    let (start, end) = (pos(&range["start"]), pos(&range["end"]));
    let chars: Vec<char> = text.chars().collect();
    let (start, end) = (start.min(chars.len()), end.max(start).min(chars.len()));
    let mut result: String = chars[..start].iter().collect();
    result.push_str(new);
    result.extend(&chars[end..]);
    *text = result;
}

/// The text of `line` before `character`.
fn line_before(text: &str, line: usize, character: usize) -> String {
    text.split('\n')
        .nth(line)
        .unwrap_or("")
        .trim_end_matches('\r')
        .chars()
        .take(character)
        .collect()
}

/// Whether a hover/definition entry matches the request (`before` is the line text before the
/// position when the document is open).
fn entry_matches(
    entry: &Value,
    uri: &str,
    line: u64,
    character: u64,
    before: Option<&str>,
) -> bool {
    let str_field = |key: &str| entry.get(key).and_then(Value::as_str);
    if str_field("uri_suffix").is_some_and(|s| !uri.ends_with(s)) {
        return false;
    }
    if str_field("uri_contains").is_some_and(|s| !uri.contains(s)) {
        return false;
    }
    if entry
        .get("line")
        .and_then(Value::as_u64)
        .is_some_and(|l| l != line)
    {
        return false;
    }
    if entry
        .get("character")
        .and_then(Value::as_u64)
        .is_some_and(|c| c != character)
    {
        return false;
    }
    if let Some(want) = str_field("before") {
        return before.is_some_and(|b| b.ends_with(want));
    }
    true
}

fn read_message(input: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let mut body = vec![0; length?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: vortex-fake-lsp <script.json>");
    let script: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("cannot read the script"))
            .expect("the script is not JSON");
    let log = script
        .get("log")
        .and_then(Value::as_str)
        .map(|p| File::create(p).expect("cannot create the log"));
    let server = Arc::new(Server {
        script,
        start: Instant::now(),
        out: Mutex::new(std::io::stdout()),
        log: Mutex::new(log),
        waiting: Mutex::new(HashMap::new()),
        next_id: Mutex::new(1000),
    });

    let mut documents: HashMap<String, String> = HashMap::new();
    let mut input = BufReader::new(std::io::stdin().lock());
    while let Some(message) = read_message(&mut input) {
        server.log(json!({ "message": message }));
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, message.get("id")) {
            // A response to one of our requests: only logged.
            (None, _) => {}
            (Some(method), Some(id)) => {
                handle_request(&server, &documents, method, id.clone(), &params)
            }
            (Some(method), None) => {
                if handle_notification(&server, &mut documents, method, &params) {
                    std::process::exit(0);
                }
            }
        }
    }
}

fn handle_notification(
    server: &Arc<Server>,
    documents: &mut HashMap<String, String>,
    method: &str,
    params: &Value,
) -> bool {
    let uri = params["textDocument"]["uri"]
        .as_str()
        .unwrap_or("")
        .to_string();
    match method {
        "exit" => return true,
        "textDocument/didOpen" => {
            let text = params["textDocument"]["text"].as_str().unwrap_or("");
            documents.insert(uri.clone(), text.to_string());
            if server.flag("diagnostics_on_open") {
                server.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {"uri": uri, "diagnostics": [{
                        "range": {"start": {"line": 0, "character": 0},
                                  "end": {"line": 0, "character": 1}},
                        "severity": 2,
                        "source": "vortex-fake-lsp",
                        "message": "fake diagnostic",
                    }]},
                }));
            }
        }
        "textDocument/didChange" => {
            if let Some(text) = documents.get_mut(&uri) {
                for change in params["contentChanges"].as_array().into_iter().flatten() {
                    apply_change(text, change);
                }
            }
        }
        "textDocument/didClose" => {
            documents.remove(&uri);
            if server.flag("diagnostics_on_open") {
                server.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {"uri": uri, "diagnostics": []},
                }));
            }
        }
        "workspace/didChangeConfiguration" => {
            if server.flag("pull_configuration") {
                let id = {
                    let mut next = server.next_id.lock().unwrap();
                    *next += 1;
                    *next
                };
                server.send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "workspace/configuration",
                    "params": {"items": [{"section": "fake"}]},
                }));
            }
        }
        "$/cancelRequest" => {
            let id = params.get("id").cloned().unwrap_or(Value::Null);
            let was_waiting = server
                .waiting
                .lock()
                .unwrap()
                .remove(&id.to_string())
                .is_some();
            if was_waiting {
                server.error(&id, -32800, "cancelled");
                server.log(json!({ "sent_cancelled": id }));
            }
        }
        _ => {}
    }
    false
}

fn handle_request(
    server: &Arc<Server>,
    documents: &HashMap<String, String>,
    method: &str,
    id: Value,
    params: &Value,
) {
    let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
    let line = params["position"]["line"].as_u64().unwrap_or(0);
    let character = params["position"]["character"].as_u64().unwrap_or(0);
    let open_text = documents.get(uri);
    let before = open_text.map(|t| line_before(t, line as usize, character as usize));

    let answer: Result<Value, (i64, &str)> = match method {
        "initialize" => Ok(json!({
            "capabilities": server.script.get("capabilities").cloned().unwrap_or(json!({})),
            "serverInfo": {"name": "vortex-fake-lsp"},
        })),
        "shutdown" => Ok(Value::Null),
        "workspace/symbol" => {
            let query = params["query"].as_str().unwrap_or("");
            Ok(server
                .list("workspace_symbols")
                .into_iter()
                .find(|e| e["query"].as_str() == Some(query))
                .map_or(json!([]), |e| e["result"].clone()))
        }
        "textDocument/documentSymbol" => Ok(server
            .list("document_symbols")
            .into_iter()
            .find(|e| e["uri_suffix"].as_str().is_some_and(|s| uri.ends_with(s)))
            .filter(|e| !e["needs_open"].as_bool().unwrap_or(true) || open_text.is_some())
            .map_or(json!([]), |e| e["result"].clone())),
        "textDocument/completion" => {
            let empty = json!({"isIncomplete": false, "items": []});
            Ok(match &before {
                None => empty,
                Some(before) => server
                    .list("completions")
                    .into_iter()
                    .find(|e| {
                        before.ends_with(e["before"].as_str().unwrap_or(""))
                            && e["uri_contains"].as_str().is_none_or(|s| uri.contains(s))
                    })
                    .map_or(empty, |e| e["result"].clone()),
            })
        }
        "completionItem/resolve" => Ok(server
            .list("resolve")
            .into_iter()
            .find(|e| e["label"] == params["label"])
            .map_or(params.clone(), |e| e["result"].clone())),
        "textDocument/hover" => Ok(server
            .list("hover")
            .into_iter()
            .find(|e| entry_matches(e, uri, line, character, before.as_deref()))
            .map_or(
                Value::Null,
                |e| json!({"contents": {"kind": "markdown", "value": e["result"]}}),
            )),
        "textDocument/definition" => Ok(server
            .list("definition")
            .into_iter()
            .find(|e| entry_matches(e, uri, line, character, before.as_deref()))
            .map_or(Value::Null, |e| e["result"].clone())),
        _ => Err((-32601, "method not found")),
    };

    let delay = server
        .script
        .get("delays_ms")
        .and_then(|d| d.get(method))
        .and_then(Value::as_u64)
        .filter(|&ms| ms > 0);
    let reply = {
        let server = Arc::clone(server);
        let id = id.clone();
        move || match answer {
            Ok(result) => server.respond(&id, result),
            Err((code, message)) => server.error(&id, code, message),
        }
    };
    match delay {
        None => reply(),
        Some(ms) => {
            let key = id.to_string();
            server.waiting.lock().unwrap().insert(key.clone(), ());
            let server = Arc::clone(server);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                // Still waiting (not cancelled meanwhile): answer.
                if server.waiting.lock().unwrap().remove(&key).is_some() {
                    reply();
                }
            });
        }
    }
}
