// Theme synchronization hook for Dioxus
//
// This module provides a reactive hook that syncs the Rust-side theme
// setting to the DOM by toggling the `dark` or `light` class on the
// `<html>` element. It also handles `System` mode by checking
// `prefers-color-scheme`.

use dioxus::prelude::*;
use crate::settings::{Settings, Theme};

/// Syncs the application theme to the DOM's `<html>` element class.
///
/// Call this once in your root `app()` component after `settings` is available.
/// When the `theme` setting changes, the DOM will be updated to reflect the new theme.
pub fn use_theme_sync(settings: Signal<Settings>) {
    use_effect(move || {
        let theme = settings.read().theme;
        spawn(async move {
            let script = match theme {
                Theme::Dark => r#"
                    document.documentElement.classList.remove('light');
                    document.documentElement.classList.add('dark');
                "#,
                Theme::Light => r#"
                    document.documentElement.classList.remove('dark');
                    document.documentElement.classList.add('light');
                "#,
                Theme::System => r#"
                    if (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) {
                        document.documentElement.classList.remove('light');
                        document.documentElement.classList.add('dark');
                    } else {
                        document.documentElement.classList.remove('dark');
                        document.documentElement.classList.add('light');
                    }
                "#,
            };
            let _ = document::eval(script);
        });
    });
}
