//! The cost of plain text editing in the whole editor: a full frame (the compositor render of
//! `Application::render`, with the terminal flush) and the time per key through the real key path
//! (`Application::handle_terminal_events`, which handles the key through the compositor and then
//! renders a frame when the key asks for one).
//!
//! Ignored: run it optimized, before and after a change to the window tree, and compare:
//!
//! ```sh
//! CARGO_INCREMENTAL=0 cargo integration-test editing_cost -- --ignored --nocapture
//! ```

use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use termina::event::{Event, KeyEvent};
use vortex_term::application::Application;
use vortex_view::input::parse_macro;

use super::helpers::*;

const WIDTH: u16 = 200;
const HEIGHT: u16 = 60;
const LINES: usize = 20_000;
const FRAMES: usize = 300;

/// About `LINES` lines of realistic Rust: structs, impls, functions, comments, strings.
fn rust_source() -> String {
    let mut out = String::new();
    let mut n = 0;
    while out.lines().count() < LINES {
        write!(
            out,
            r#"/// A record of kind {n}, kept in the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Record{n} {{
    pub id: u64,
    pub name: String,
    pub tags: Vec<&'static str>,
}}

impl Record{n} {{
    /// Builds the record with the given id.
    pub fn new(id: u64) -> Self {{
        // the name is derived from the id
        let name = format!("record-{{}}-{n}", id);
        Self {{ id, name, tags: vec!["a", "b", "c"] }}
    }}

    pub fn score(&self, weights: &[f64]) -> f64 {{
        let mut total = 0.0;
        for (i, w) in weights.iter().enumerate() {{
            if self.tags.len() > i {{
                total += w * (self.id as f64 + i as f64);
            }} else {{
                total -= 1.5;
            }}
        }}
        total
    }}
}}

"#
        )
        .unwrap();
        n += 1;
    }
    out
}

fn key_events(keys: &str) -> Vec<Event> {
    parse_macro(keys)
        .unwrap()
        .into_iter()
        .map(|key| Event::Key(KeyEvent::from(key)))
        .collect()
}

/// Sends the keys through the real key path, without timing.
async fn send(app: &mut Application, keys: &str) {
    for event in key_events(keys) {
        app.handle_terminal_events(Ok(event)).await;
    }
    run_event_loop_until_idle(app).await;
}

/// Sends the keys one by one and returns the time each took, with its redraw.
async fn time_keys(app: &mut Application, keys: &str) -> Vec<Duration> {
    let events = key_events(keys);
    let mut times = Vec::with_capacity(events.len());
    for event in events {
        let start = Instant::now();
        app.handle_terminal_events(Ok(event)).await;
        times.push(start.elapsed());
    }
    run_event_loop_until_idle(app).await;
    times
}

fn stats(mut times: Vec<Duration>) -> (Duration, Duration) {
    times.sort();
    let median = times[times.len() / 2];
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    (median, mean)
}

fn us(duration: Duration) -> String {
    format!("{:>9.1}", duration.as_secs_f64() * 1e6)
}

fn row(name: &str, count: usize, times: Vec<Duration>) {
    let (median, mean) = stats(times);
    println!("{name:<44} {count:>6} {} {}", us(median), us(mean));
}

/// `FRAMES` frames, spread over a few places in the file.
async fn frames(app: &mut Application) -> Vec<Duration> {
    let places = ["gg", "5000G", "10000G", "15000G", "19900G"];
    let per_place = FRAMES / places.len();
    let mut times = Vec::with_capacity(FRAMES);
    for place in places {
        send(app, place).await;
        for _ in 0..per_place {
            let start = Instant::now();
            app.render().await;
            times.push(start.elapsed());
        }
    }
    times
}

async fn open(source: &str) -> (Application, tempfile::NamedTempFile) {
    let mut file = tempfile::Builder::new().suffix(".rs").tempfile().unwrap();
    std::io::Write::write_all(&mut file, source.as_bytes()).unwrap();
    let mut app = AppBuilder::new()
        .with_file(file.path(), None)
        .build()
        .unwrap();
    app.resize_screen(WIDTH, HEIGHT);
    // the terminal picks the size up with the next event, and the syntax tree is parsed
    send(&mut app, "<esc>").await;
    run_event_loop_until_idle(&mut app).await;
    app.render().await;
    (app, file)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn editing_cost() -> anyhow::Result<()> {
    let source = rust_source();
    println!(
        "\nediting_cost: {} lines of Rust, {WIDTH}x{HEIGHT} terminal, syntax highlighting on",
        source.lines().count()
    );
    println!("frame = Application::render (compositor render + terminal draw)");
    println!(
        "key   = Application::handle_terminal_events per key, including the redraw it asks for"
    );
    println!(
        "\n{:<44} {:>6} {:>9} {:>9}",
        "", "n", "median us", "mean us"
    );

    let (mut app, _file) = open(&source).await;
    row("frame, 1 window", FRAMES, frames(&mut app).await);

    // keys, in a single window
    send(&mut app, "gg").await;
    row(
        "key j (normal), 2000",
        2000,
        time_keys(&mut app, &"j".repeat(2000)).await,
    );
    send(&mut app, "gg").await;
    row(
        "key w (normal), 500",
        500,
        time_keys(&mut app, &"w".repeat(500)).await,
    );
    row(
        "key b (normal), 500",
        500,
        time_keys(&mut app, &"b".repeat(500)).await,
    );
    let typed: String = "abcdefghij klmnopqrst".chars().cycle().take(2000).collect();
    send(&mut app, "5000Gi").await;
    row(
        "key typed in insert mode, 2000",
        2000,
        time_keys(&mut app, &typed).await,
    );
    send(&mut app, "<esc>").await;

    // three windows on the same file
    send(&mut app, ":vsplit<ret>:vsplit<ret>").await;
    row(
        "frame, 3 windows (2 vsplits)",
        FRAMES,
        frames(&mut app).await,
    );
    send(&mut app, "gg").await;
    row(
        "key j (normal, 3 windows), 2000",
        2000,
        time_keys(&mut app, &"j".repeat(2000)).await,
    );

    let errs = app.close().await;
    anyhow::ensure!(errs.is_empty(), "errors closing the app: {errs:?}");
    Ok(())
}
