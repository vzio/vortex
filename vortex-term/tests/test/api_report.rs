//! Measures the API browser against a real language server, for the report of the spike (not a
//! test: it is ignored, and needs the server installed and a project to open).
//!
//! ```sh
//! API_REPORT_FILE=/path/to/project/src/main.rs API_REPORT_WAIT=10 \
//! API_REPORT_PATHS='Packages>regex;Std>std>collections>HashMap;Codebase>src>main.rs' \
//!   cargo integration-test -- api_report --ignored --nocapture
//! ```
//!
//! It opens the file, waits for its language server to start and `API_REPORT_WAIT` seconds more
//! (indexing), then opens the four roots and every path of `API_REPORT_PATHS` (node names
//! separated by `>`), printing each level: its items, the provider, the requests it took, how
//! long, whether the server said it was incomplete; and the docs of its first items.

use std::time::{Duration, Instant};

use vortex_term::api::{Browser, NodeId, State};

use super::debug::harness::Harness;
use super::helpers::AppBuilder;

async fn settle(harness: &mut Harness, browser: &mut Browser, limit: Duration) {
    let start = Instant::now();
    loop {
        browser.update();
        if browser.in_flight() == 0 || start.elapsed() > limit {
            return;
        }
        harness.step(Duration::from_millis(20)).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn print_level(browser: &Browser, id: NodeId) {
    let path = browser.display_path(id);
    let path = if path.is_empty() {
        browser.node(id).name.clone()
    } else {
        path
    };
    match browser.children(id) {
        State::Ready(level) => {
            let provider = level
                .children
                .first()
                .map(|&child| browser.node(child).provider.label())
                .unwrap_or("-");
            let names: Vec<String> = level
                .children
                .iter()
                .take(10)
                .map(|&child| {
                    let node = browser.node(child);
                    let detail = node
                        .detail
                        .as_deref()
                        .map(|detail| format!(" `{}`", detail.chars().take(40).collect::<String>()))
                        .unwrap_or_default();
                    format!("{}[{}]{detail}", node.name, node.kind.label())
                })
                .collect();
            println!(
                "LEVEL {path} | {} items | {provider} | {} requests | {} ms{} | {}",
                level.children.len(),
                level.requests,
                level.took.as_millis(),
                if level.incomplete {
                    " | INCOMPLETE"
                } else {
                    ""
                },
                names.join(", ")
            );
        }
        State::Failed(err) => println!("LEVEL {path} | FAILED: {err}"),
        State::Loading => println!("LEVEL {path} | still loading"),
        State::NotAsked => println!("LEVEL {path} | not asked"),
    }
}

async fn print_docs(harness: &mut Harness, browser: &mut Browser, id: NodeId, count: usize) {
    let children: Vec<NodeId> = match browser.children(id) {
        State::Ready(level) => level.children.iter().copied().take(count).collect(),
        _ => return,
    };
    for child in children {
        let start = Instant::now();
        browser.ask_docs(child);
        settle(harness, browser, Duration::from_secs(30)).await;
        let name = browser.node(child).name.clone();
        match browser.docs(child) {
            State::Ready(docs) => println!(
                "  DOCS {name} | {} ms | source: {} | signature: {:?} | docs: {} chars{}",
                start.elapsed().as_millis(),
                if docs.source.is_empty() {
                    "none"
                } else {
                    docs.source
                },
                docs.signature
                    .as_deref()
                    .map(|text| text.chars().take(60).collect::<String>()),
                docs.text.as_deref().map_or(0, str::len),
                docs.text
                    .as_deref()
                    .map(|text| format!(" ({:?})", text.chars().take(80).collect::<String>()))
                    .unwrap_or_default()
            ),
            State::Failed(err) => println!("  DOCS {name} | FAILED: {err}"),
            _ => println!("  DOCS {name} | not ready"),
        }
        browser.ask_definition(child);
        settle(harness, browser, Duration::from_secs(30)).await;
        if let State::Ready(place) = browser.definition(child) {
            println!(
                "  DEFINITION {name} | {}",
                place.as_ref().map_or("none".to_string(), |place| format!(
                    "{}:{}",
                    place.uri.path(),
                    place.range.start.line + 1
                ))
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn api_report() -> anyhow::Result<()> {
    let Ok(file) = std::env::var("API_REPORT_FILE") else {
        return Ok(());
    };
    let wait: u64 = std::env::var("API_REPORT_WAIT")
        .ok()
        .and_then(|wait| wait.parse().ok())
        .unwrap_or(5);
    let paths = std::env::var("API_REPORT_PATHS").unwrap_or_default();
    let docs: usize = std::env::var("API_REPORT_DOCS")
        .ok()
        .and_then(|docs| docs.parse().ok())
        .unwrap_or(2);

    let mut harness = Harness::new(AppBuilder::new().with_file(&file, None))?;
    let start = Instant::now();
    loop {
        harness.step(Duration::from_millis(100)).await;
        let ready = harness
            .app
            .editor
            .language_servers
            .iter_clients()
            .any(|client| client.is_initialized());
        if ready {
            break;
        }
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(60),
            "no language server started"
        );
    }
    println!("SERVER initialized in {} ms", start.elapsed().as_millis());
    harness.pump_for(Duration::from_secs(wait)).await;

    let mut browser = Browser::new(&harness.app.editor)?;
    println!("BROWSER {} · {}", browser.language, browser.server());
    for root in browser.roots().to_vec() {
        browser.open(root);
        settle(&mut harness, &mut browser, Duration::from_secs(60)).await;
        print_level(&browser, root);
        print_docs(&mut harness, &mut browser, root, docs).await;
    }
    println!("PATH QUERIES {:?}", browser.path_queries());
    for path in paths.split(';').filter(|path| !path.is_empty()) {
        let mut at: Option<NodeId> = None;
        for name in path.split('>') {
            let candidates: Vec<NodeId> = match at {
                None => browser.roots().to_vec(),
                Some(id) => match browser.children(id) {
                    State::Ready(level) => level.children.clone(),
                    _ => Vec::new(),
                },
            };
            let Some(next) = candidates.into_iter().find(|&id| {
                browser.node(id).name.trim_end_matches('/') == name.trim_end_matches('/')
            }) else {
                println!("PATH {path}: no {name}");
                at = None;
                break;
            };
            browser.open(next);
            settle(&mut harness, &mut browser, Duration::from_secs(60)).await;
            at = Some(next);
        }
        if let Some(id) = at {
            print_level(&browser, id);
            print_docs(&mut harness, &mut browser, id, docs).await;
        }
    }
    drop(browser);
    harness.pump_for(Duration::from_millis(300)).await;
    let leftover: Vec<String> = harness
        .app
        .editor
        .diagnostics
        .keys()
        .filter(|uri| uri.to_string().contains("vortex_api_probe"))
        .map(|uri| uri.to_string())
        .collect();
    println!("PROBE DIAGNOSTICS LEFT IN THE EDITOR: {leftover:?}");
    harness.finish().await
}
