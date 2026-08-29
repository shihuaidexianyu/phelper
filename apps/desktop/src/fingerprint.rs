//! Shell-level fingerprint (§Phase 2): only repaints when the *sidebar /
//! shell chrome* changes (page selection, theme). Per-page repaints are
//! driven by each page entity's own observer + fingerprint inside the
//! page module — this hash exists so the shell's `cx.observe(&app_state)`
//! can passively refresh its cached snapshot without firing spurious
//! `cx.notify()`s (every page already observes the same entity).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use gpui::AppContext;
use phelper_core::app::settings::ThemePref;

use crate::shell::ShellView;

impl ShellView {
    pub(crate) fn fingerprint(&self, cx: &gpui::App) -> u64 {
        let mut h = DefaultHasher::new();
        std::mem::discriminant(&self.page).hash(&mut h);
        // Settings pref flips the shell chrome (light/dark surface).
        let theme: ThemePref = self
            .settings_page
            .read_with(cx, |s, _| s.theme());
        format!("{:?}", theme).hash(&mut h);
        h.finish()
    }
}