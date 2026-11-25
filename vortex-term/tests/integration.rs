#[cfg(feature = "integration")]
mod test {
    mod helpers;

    use vortex_core::{syntax::config::AutoPairConfig, Selection};
    use vortex_term::config::Config;

    use indoc::indoc;

    use self::helpers::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn hello_world() -> anyhow::Result<()> {
        test(("#[\n|]#", "ihello world<esc>", "hello world#[|\n]#")).await?;
        Ok(())
    }

    mod api_browser;
    mod api_report;
    mod auto_pairs;
    mod change_lists;
    mod change_lists_live;
    mod command_line;
    mod commands;
    mod conflicts;
    mod dashboard;
    mod debug;
    mod editing_cost;
    mod explorer;
    #[allow(dead_code)]
    mod fake_lsp;
    mod git_log;
    mod goto_word;
    mod hex_view;
    mod history;
    mod hunks;
    mod inline_blame;
    mod language_theme;
    mod line_history;
    mod markdown_view;
    mod menus;
    mod modes;
    mod move_lines;
    mod movement;
    mod palette;
    mod picker_modes;
    mod practice;
    mod practice_panel;
    mod repo_gen;
    mod selective_wrap;
    mod soft_wrap;
    mod splits;
    mod stale_buffers;
    mod terminal;
    mod uncommitted;
    mod watch;
    mod worktrees;
    mod x_empty_line;
    mod yank_place;
}
