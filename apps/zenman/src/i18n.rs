//! The i18n table this binary was built with (plan §5.9 `build.rs` row, wired here).
//!
//! With `--features i18n` the generated `$OUT_DIR/craft_i18n.rs` is included and the embedded catalog
//! table is visible to the rest of the app; without it — the default build, which is what step 4 gates
//! — this module answers "no extra locales", which is the honest description of a build that links no
//! translation crate at all. See `build.rs` for the `CRAFT_I18N_DIR` convention and the pinned revs.

/// The generated table, when the feature is on. `craft_i18n::Embedded` is
/// `[(&'static str, &'static str, &'static str)]` — one `(locale, shared catalog, app catalog)` row per
/// locale, the catalogs `include_str!`-ed from `$OUT_DIR` (the photocraft mechanism,
/// `crates/ui-egui/build.rs:36-41`).
#[cfg(feature = "i18n")]
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/craft_i18n.rs"));
}

/// How many locales besides English are in the binary.
#[must_use]
pub fn embedded_locale_count() -> usize {
    #[cfg(feature = "i18n")]
    {
        embedded::CRAFT_I18N.len()
    }
    #[cfg(not(feature = "i18n"))]
    {
        0
    }
}

/// Their language tags, for the settings panel and the boot line.
#[must_use]
pub fn embedded_locales() -> Vec<&'static str> {
    #[cfg(feature = "i18n")]
    {
        embedded::CRAFT_I18N
            .iter()
            .map(|(code, _, _)| *code)
            .collect()
    }
    #[cfg(not(feature = "i18n"))]
    {
        Vec::new()
    }
}

/// Whether this build can show anything but the source text.
#[must_use]
pub const fn translations_available() -> bool {
    cfg!(feature = "i18n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_build_reports_no_extra_locales_and_no_build_dir_leak() {
        // The feature is off in every gate (`cargo xtask ci`), so the count must be zero and the list
        // empty; with it on, the count is whatever `CRAFT_I18N_DIR` provided (0 for an English-only
        // directory, which is what `apps/zenman/i18n/` is today).
        if translations_available() {
            assert!(
                embedded_locales().iter().all(|c| *c != "en"),
                "English is the source language, never an embedded catalog: {:?}",
                embedded_locales()
            );
            assert_eq!(embedded_locale_count(), embedded_locales().len());
        } else {
            assert_eq!(embedded_locale_count(), 0);
            assert!(embedded_locales().is_empty());
        }
    }
}
