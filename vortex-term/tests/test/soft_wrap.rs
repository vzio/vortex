//! Markdown buffers wrap by default (`soft-wrap` of the markdown entry in `languages.toml`), and a
//! user's `languages.toml` can turn it off.

use super::*;
use vortex_term::application::Application;
use vortex_view::doc;

/// Whether the focused buffer soft wraps in a view 80 columns wide.
fn wraps(app: &Application) -> bool {
    let doc = doc!(app.editor);
    doc.text_format(80, None).soft_wrap
}

async fn open(
    path: &std::path::Path,
    loader: Option<vortex_core::syntax::Loader>,
) -> anyhow::Result<Application> {
    let mut builder = AppBuilder::new().with_file(path, None);
    if let Some(loader) = loader {
        builder = builder.with_lang_loader(loader);
    }
    builder.build()
}

fn temp_file(suffix: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    let file = tempfile::Builder::new().suffix(suffix).tempfile()?;
    std::fs::write(file.path(), "# Title\n\nA paragraph.\n")?;
    Ok(file)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_markdown_buffer_wraps_by_default() -> anyhow::Result<()> {
    let file = temp_file(".md")?;
    let mut app = open(file.path(), None).await?;
    assert_eq!(
        doc!(app.editor).language_name(),
        Some("markdown"),
        "the file is detected as Markdown"
    );
    test_key_sequence(&mut app, None, Some(&|app| assert!(wraps(app))), false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn other_languages_still_do_not_wrap() -> anyhow::Result<()> {
    let file = temp_file(".rs")?;
    let mut app = open(file.path(), None).await?;
    test_key_sequence(&mut app, None, Some(&|app| assert!(!wraps(app))), false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_users_languages_toml_turns_it_off() -> anyhow::Result<()> {
    let file = temp_file(".md")?;
    let overrides = r#"
[[language]]
name = "markdown"
soft-wrap = { enable = false }
"#;
    let loader = test_syntax_loader(Some(overrides.into()));
    let mut app = open(file.path(), Some(loader)).await?;
    test_key_sequence(&mut app, None, Some(&|app| assert!(!wraps(app))), false).await
}
