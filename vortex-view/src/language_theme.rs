//! A theme per language (this fork): a language can name a theme in `languages.toml`
//! (`theme = "gruvbox"`), and the whole editor takes the theme of the focused buffer's language,
//! to tell at a glance which language (and which mental gear) you are in. Languages without one
//! use the main theme (the config's, or the last `:theme`).
//!
//! - An explicit `:theme` overrides the languages' themes until the feature is toggled back on
//!   (`:language-themes`) or `vx` quits.
//! - The dashboard and the other screens (the side by side diff) keep the main theme.
//! - Themes are loaded once per run and kept in memory: nothing is saved. A theme's `[effects]`
//!   come with it, since they are part of the theme.

use std::collections::HashMap;

use crate::{theme::Theme, Editor};

/// The state of the languages' themes.
#[derive(Default)]
pub struct LanguageThemes {
    /// Off with `:language-themes`: the main theme everywhere.
    pub off: bool,
    /// An explicit `:theme` overrides the languages' themes, until the feature is toggled.
    pub overridden: bool,
    /// The main theme, kept while a language's theme is shown.
    main: Option<Theme>,
    /// The name of the language theme shown, if one is.
    shown: Option<String>,
    /// The themes loaded so far, by name; `None` for one that failed to load (said once).
    loaded: HashMap<String, Option<Theme>>,
}

impl LanguageThemes {
    /// The name of the language theme shown, if one is (not the main theme).
    pub fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// The main theme set anew (`:theme`, the config): the language theme shown is forgotten.
    pub(crate) fn main_set(&mut self) {
        self.main = None;
        self.shown = None;
    }
}

impl Editor {
    /// The theme the editor should show now: the focused buffer's language's, unless the feature
    /// is off or overridden, or a screen (`screen`: the dashboard, the diff) is up, or a theme is
    /// being previewed. Swaps it in (or the main theme back) when it changed; cheap otherwise.
    pub fn sync_language_theme(&mut self, screen: bool) {
        if self.last_theme.is_some() {
            // `:theme` is previewing one
            return;
        }
        let wanted = if self.language_themes.off || self.language_themes.overridden || screen {
            None
        } else {
            let doc = crate::doc!(self);
            doc.language_config()
                .and_then(|config| config.theme.clone())
        };
        if wanted.as_deref() == self.language_themes.shown() {
            return;
        }
        let theme = wanted.as_ref().and_then(|name| self.language_theme(name));
        match theme {
            Some(theme) => {
                let main = std::mem::replace(&mut self.theme, theme);
                if self.language_themes.main.is_none() {
                    self.language_themes.main = Some(main);
                }
                self.language_themes.shown = wanted;
                self.theme_swapped();
            }
            // (a theme that failed to load: the main theme, which refreshes only if it was
            // swapped out, not on every frame)
            None => self.restore_main_theme(),
        }
    }

    /// Shows the main theme again, if a language's is shown (a `:theme` preview starts from it).
    pub fn restore_main_theme(&mut self) {
        if let Some(main) = self.language_themes.main.take() {
            self.theme = main;
            self.theme_swapped();
        }
        self.language_themes.shown = None;
    }

    /// `:language-themes`: turns the languages' themes back on when an explicit `:theme`
    /// overrides them or they are off, else off. Returns whether they are on now.
    pub fn toggle_language_themes(&mut self) -> bool {
        let themes = &mut self.language_themes;
        if themes.overridden || themes.off {
            themes.overridden = false;
            themes.off = false;
            true
        } else {
            themes.off = true;
            false
        }
    }

    /// The theme `name`, loaded once per run.
    fn language_theme(&mut self, name: &str) -> Option<Theme> {
        if let Some(theme) = self.language_themes.loaded.get(name) {
            return theme.clone();
        }
        let theme = match self.theme_loader.load(name) {
            Ok(theme) if theme.find_highlight_exact("ui.selection").is_some() => Some(theme),
            Ok(_) => {
                log::warn!("language theme `{name}` has no `ui.selection`: not used");
                None
            }
            Err(err) => {
                log::warn!("couldn't load the language theme `{name}`: {err}");
                None
            }
        };
        self.language_themes
            .loaded
            .insert(name.to_string(), theme.clone());
        theme
    }
}
