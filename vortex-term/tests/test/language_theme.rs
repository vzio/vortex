//! A theme per language (this fork): a language can name a theme in `languages.toml`, and the
//! editor shows the one of the focused buffer's language, unless `:theme` overrides it, the
//! feature is off, or a screen (the dashboard, the diff) is up.

use std::path::{Path, PathBuf};

use super::git_log::{git, on_screen, status, Repo, Session};
use super::helpers::{test_config, test_syntax_loader, AppBuilder};

/// The name of the main theme of the sessions: Vortex's own.
const MAIN: &str = "vortex";

const OVERRIDES: &str = r#"
[[language]]
name = "rust"
theme = "gruvbox"
"#;

fn file(suffix: &str, text: &str) -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
    std::fs::write(file.path(), text).unwrap();
    file
}

/// A session on `path`, with `overrides` as the user's `languages.toml`, after its first frame.
async fn session(path: &Path, overrides: &str) -> Session {
    let mut config = test_config();
    // (`:theme` refuses themes that need true color otherwise)
    config.editor.true_color = true;
    let app = AppBuilder::new()
        .with_config(config)
        .with_file(path, None)
        .with_lang_loader(test_syntax_loader(Some(overrides.into())))
        .build()
        .unwrap();
    let mut session = Session::with_app(app);
    // (a complete theme: the tests' own has no scopes for the syntax highlighting)
    session.app.editor.theme = session.app.editor.theme_loader.default_theme(true);
    session.first_frame().await;
    session
}

/// The name of the theme shown.
fn theme(session: &Session) -> String {
    session.app.editor.theme.name().to_string()
}

/// The name of the language theme shown, if one is.
fn shown(session: &Session) -> Option<String> {
    session.app.editor.language_themes.shown().map(String::from)
}

/// Asserts that the language theme `name` is the one shown.
fn assert_language_theme(session: &Session, name: &str) {
    assert_eq!(theme(session), name);
    assert_eq!(shown(session).as_deref(), Some(name));
}

/// Asserts that the main theme (Vortex's own) is shown.
fn assert_main(session: &Session) {
    assert_eq!(theme(session), MAIN);
    assert_eq!(shown(session), None);
}

/// The theme of the focused buffer's language is shown, and follows the focus and the buffer.
#[tokio::test(flavor = "multi_thread")]
async fn the_theme_follows_the_focused_buffer() {
    let rust = file(".rs", "fn main() {}\n");
    let toml = file(".toml", "a = 1\n");
    let mut session = session(rust.path(), OVERRIDES).await;
    assert_language_theme(&session, "gruvbox");

    session
        .keys(&format!(":vsplit {}<ret>", toml.path().display()))
        .await;
    assert_main(&session);
    session.keys("<C-w>w").await;
    assert_language_theme(&session, "gruvbox");
    session.keys("<C-w>w").await;
    assert_main(&session);

    // the same view, another buffer
    session.keys("<C-w>o").await;
    session.keys(":bp<ret>").await;
    assert_language_theme(&session, "gruvbox");
    session.keys(":bn<ret>").await;
    assert_main(&session);
}

/// A language without a theme keeps the main theme.
#[tokio::test(flavor = "multi_thread")]
async fn a_language_without_a_theme_keeps_the_main_theme() {
    let toml = file(".toml", "a = 1\n");
    let mut session = session(toml.path(), OVERRIDES).await;
    assert_main(&session);
    session.keys("jj").await;
    assert_main(&session);
}

/// `:theme` overrides the languages' themes, also after a change of focus, until
/// `:language-themes`.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_theme_overrides_the_languages_themes() {
    let rust = file(".rs", "fn main() {}\n");
    let toml = file(".toml", "a = 1\n");
    let mut session = session(rust.path(), OVERRIDES).await;
    assert_language_theme(&session, "gruvbox");

    session.keys(":theme nord<ret>").await;
    assert_eq!(theme(&session), "nord");
    assert_eq!(shown(&session), None);
    assert!(session.app.editor.language_themes.overridden);

    session
        .keys(&format!(":vsplit {}<ret>", toml.path().display()))
        .await;
    assert_eq!(theme(&session), "nord");
    session.keys("<C-w>w").await;
    assert_eq!(theme(&session), "nord", "the rust buffer is focused again");

    session.keys(":language-themes<ret>").await;
    assert_language_theme(&session, "gruvbox");
    assert!(!session.app.editor.language_themes.overridden);
}

/// `:language-themes` turns the feature off and on, and says so.
#[tokio::test(flavor = "multi_thread")]
async fn language_themes_toggles_the_feature() {
    let rust = file(".rs", "fn main() {}\n");
    let mut session = session(rust.path(), OVERRIDES).await;
    assert_language_theme(&session, "gruvbox");

    session.keys(":language-themes<ret>").await;
    assert_main(&session);
    assert_eq!(status(&session.app), "Language themes off");

    session.keys(":language-themes<ret>").await;
    assert_language_theme(&session, "gruvbox");
    assert_eq!(status(&session.app), "Language themes on");
}

/// The dashboard keeps the main theme, and the language's comes back with its buffer.
#[tokio::test(flavor = "multi_thread")]
async fn the_dashboard_keeps_the_main_theme() {
    let rust = file(".rs", "fn main() {}\n");
    let mut session = session(rust.path(), OVERRIDES).await;
    assert_language_theme(&session, "gruvbox");

    session.keys(":dashboard<ret>").await;
    assert_main(&session);
    // (the dashboard is over a new scratch buffer: it has no language)
    session.keys("<esc>").await;
    assert_main(&session);
    session.keys(":bp<ret>").await;
    assert_language_theme(&session, "gruvbox");
}

/// A theme's effects come with it.
#[tokio::test(flavor = "multi_thread")]
async fn the_effects_follow_the_theme() {
    let rust = file(".rs", "fn main() {}\n");
    let toml = file(".toml", "a = 1\n");
    let overrides = "[[language]]\nname = \"rust\"\ntheme = \"matrix\"\n";
    let mut session = session(rust.path(), overrides).await;
    assert_language_theme(&session, "matrix");
    assert_eq!(session.app.editor.theme.effects().glitch_diagnostics, 0.02);

    session
        .keys(&format!(":vsplit {}<ret>", toml.path().display()))
        .await;
    assert_eq!(shown(&session), None);
    assert_eq!(session.app.editor.theme.effects().glitch_diagnostics, 0.0);
}

/// A theme that doesn't exist: the main theme, and nothing changes over several frames.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_theme_falls_back_to_the_main_theme() {
    let rust = file(".rs", "fn main() {}\n");
    let overrides = "[[language]]\nname = \"rust\"\ntheme = \"no-such-theme\"\n";
    let mut session = session(rust.path(), overrides).await;
    assert_main(&session);
    session.keys("jl").await;
    assert_main(&session);
    session.keys("<C-w>s<C-w>c").await;
    assert_main(&session);
}

fn cwd() -> PathBuf {
    vortex_stdx::env::current_working_dir()
}

/// The side by side diff keeps the main theme.
#[tokio::test(flavor = "multi_thread")]
async fn the_diff_keeps_the_main_theme() {
    let Some(_) = Repo::enter_in(module_path!(), "the_diff_keeps_the_main_theme") else {
        return;
    };
    let lines = |word: &str| -> String { (1..=20).map(|i| format!("// {word} {i}\n")).collect() };
    std::fs::write(cwd().join("a.rs"), lines("line")).unwrap();
    git(&cwd(), "", &["add", "-A"]);
    git(&cwd(), "", &["commit", "-q", "-m", "Add a.rs"]);
    std::fs::write(cwd().join("a.rs"), lines("edited")).unwrap();

    let path = cwd().join("a.rs");
    let mut session = session(&path, OVERRIDES).await;
    assert_language_theme(&session, "gruvbox");

    session = Session::open_log_with(session).await;
    session
        .wait_for_screen("the changes", |rows| on_screen(rows, "(1 file)"))
        .await;
    session.open_diff(0).await;
    assert_main(&session);

    // the diff closed: the buffer's theme again
    session.keys("<esc>").await;
    session.keys("<esc>").await;
    assert_language_theme(&session, "gruvbox");
}
