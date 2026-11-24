//! Build script: the craft-i18n catalog code-gen (plan §5.4/§5.9 `apps/zenman/build.rs` row), and the
//! Windows executable icon.
//!
//! # The convention this copies, and where it is pinned
//!
//! `storytold/photocraft` generates its translations in `crates/ui-egui/build.rs`. The lines that matter
//! are **`crates/ui-egui/build.rs:36-41`**, verified against the checkout in step 1:
//!
//! ```text
//! 36  }
//! 37
//! 38  /// The catalog this app reads in each locale: `locales/<locale>/photocraft.tsv`.
//! 39  const APP_CATALOG: &str = "photocraft.tsv";
//! 40
//! 41  /// `CRAFT_I18N` from `CRAFT_I18N_DIR`: the `Embedded` list the craft-i18n crate reads.
//! ```
//!
//! so the imported rule is: the app's own catalog is one file per locale named after the *app*, found
//! under `$CRAFT_I18N_DIR/locales/<locale>/`, and the generated item is `pub static CRAFT_I18N:
//! &craft_i18n::Embedded` — one `(locale, shared, app)` triple per locale, the catalog text `include_str!`-ed
//! rather than copied into the source. `CRAFT_I18N_DIR` unset means no catalogs: the interface is English
//! only, and that is not an error.
//!
//! Pinned revisions, because a generated file must be reproducible:
//!
//! * the `craft-i18n` crate this workspace links is the one in `storytold/craft-libs` at rev
//!   **`6e03f65bf45181db77ba6229716e6624db9eb25b`** (the workspace `[workspace.dependencies]` rev, and
//!   the value of photocraft's own `craft-i18n.rev`-style pin);
//! * photocraft's `craft-i18n.rev` file pins the standalone `storytold/craft-i18n` checkout that
//!   `CRAFT_I18N_DIR` may point at: **`e02939ef009218e81c44e8ec001fbb7603461de4`**. Its `docs/integration.md`
//!   is the recipe the layout above follows.
//!
//! # What differs here, and why that is deliberate
//!
//! * The app catalog is `zenman.json` under `apps/zenman/i18n/` (source-controlled), not a `.tsv` inside
//!   somebody else's checkout. `CRAFT_I18N_DIR` therefore selects *translation input*, and the directory
//!   may be either the upstream `locales/<locale>/zenman.tsv` layout **or** our own `<locale>.json`
//!   layout; both are read, JSON first.
//! * English is not embedded at all. `craft_i18n` looks up *English source text* and returns it unchanged
//!   when there is no translation (`Lang::EN`), so `en.json` is the source-of-truth document, not a
//!   catalog — see `apps/zenman/i18n/en.json`.
//!
//! # Why an optional dependency behind a feature
//!
//! `craft-i18n` is an *optional* dependency (`--features i18n`), so the default build of `zenman` links
//! no new crate at all and `cargo check -p zenman` stays exactly as fast as it was in step 1. The
//! generated `$OUT_DIR/craft_i18n.rs` names the `craft_i18n::Embedded` type, so it is `include!`-d only
//! under `#[cfg(feature = "i18n")]` (`src/i18n.rs`); it is still *written* in both modes, which is what
//! makes the "with and without `CRAFT_I18N_DIR`" acceptance criterion a test of this script rather than
//! of the feature.
//!
//! No bash, no python: the script is Rust over `serde_json::Value`.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The app's own catalog file name in the upstream layout (`locales/<locale>/zenman.tsv`).
const APP_CATALOG: &str = "zenman.tsv";

/// The shared catalog beside it, when a locale has one.
const SHARED_CATALOG: &str = "common.tsv";

/// The locale whose translations are the source text, so it is never embedded.
const SOURCE_LOCALE: &str = "en";

fn main() {
    println!("cargo::rerun-if-env-changed=CRAFT_I18N_DIR");
    println!("cargo::rerun-if-env-changed=CRAFT_I18N_REQUIRED");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    // The script itself and our own catalog directory always matter, `CRAFT_I18N_DIR` or not: `en.json`
    // is the document the other locales are checked against.
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={}", default_i18n_dir().display());
    let target = std::env::var("TARGET").unwrap_or_default();
    windows_icon(&target);
    let body = generate();
    if let Err(e) = std::fs::write(out_dir.join("craft_i18n.rs"), body.text) {
        println!("cargo::error=writing {}: {e}", out_dir.display());
    }
    for (name, text) in body.catalogs {
        if let Err(e) = std::fs::write(out_dir.join(&name), text) {
            println!("cargo::error=writing {name}: {e}");
        }
    }
}

/// The Windows executable resource: the app icon as `RT_GROUP_ICON`.
///
/// # Why a build-time resource at all
///
/// The runtime icon (`apps/zenman/src/app_icon.rs`, `WM_SETICON`) only exists once a window does.
/// Explorer, a desktop shortcut, "open file location", the taskbar's *pinned* tile and every
/// thumbnail Windows draws from the binary itself all read the icon out of the executable's resource
/// table, before anything runs — so without this the shipped `.exe` is the generic app tile no matter
/// how good the in-window icon is.
///
/// # Why a warning and never an error
///
/// A missing icon is cosmetic; a build that stops is not. So a failure here is a `cargo:warning`, and
/// the `*-pc-windows-gnu` case (which needs `windres`, unlike the MSVC case, whose `.res` writer is
/// pure Rust and therefore works from any host) still produces a working binary. See
/// `docs/app-icon.md` for what each platform then shows.
fn windows_icon(target: &str) {
    if !target.contains("windows") {
        return;
    }
    let ico = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/app-icon/zenman.ico");
    println!("cargo::rerun-if-changed={}", ico.display());
    if !ico.is_file() {
        println!(
            "cargo::warning={}: run `scripts/make-app-icon.py` — the .exe would ship without an icon",
            ico.display()
        );
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    // Relative to this crate, which is how `winresource` resolves it and how `cargo` reports it.
    resource.set_icon("../../assets/app-icon/zenman.ico");
    if let Err(e) = resource.compile() {
        println!(
            "cargo::warning=the Windows icon resource could not be compiled ({e}): the .exe ships without an icon"
        );
    }
}

/// `apps/zenman/i18n`, from the crate directory the script runs in.
fn default_i18n_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n")
}

/// Everything the generated file needs: the Rust source, plus the catalog texts it `include_str!`s.
struct Generated {
    text: String,
    catalogs: Vec<(String, String)>,
}

/// Build `$OUT_DIR/craft_i18n.rs`.
fn generate() -> Generated {
    let required = std::env::var("CRAFT_I18N_REQUIRED").is_ok_and(|v| v.trim() == "1");
    let dir = std::env::var_os("CRAFT_I18N_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let entries = match dir {
        Some(dir) => match collect(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                println!(
                    "cargo::warning=CRAFT_I18N_DIR={}: {e}; the interface is English only",
                    dir.display()
                );
                if required {
                    println!(
                        "cargo::error=CRAFT_I18N_REQUIRED=1 but the catalogs could not be read"
                    );
                }
                Vec::new()
            }
        },
        None => {
            // The passthrough case, and the one every default `cargo check`/`build` takes: no
            // `CRAFT_I18N_DIR` is not an error, it is "ship English" (photocraft's own rule).
            Vec::new()
        }
    };

    let mut catalogs = Vec::new();
    let mut rows = String::new();
    for entry in entries {
        let shared_name = format!("craft_i18n-{}-shared.tsv", entry.locale);
        let app_name = format!("craft_i18n-{}-app.tsv", entry.locale);
        catalogs.push((shared_name.clone(), entry.shared));
        catalogs.push((app_name.clone(), entry.app));
        // `{:?}` escapes the path the way photocraft's does, so a Windows `OUT_DIR` stays a legal literal.
        rows.push_str(&format!(
            "    ({:?}, include_str!({shared_name:?}), include_str!({app_name:?})),\n",
            entry.locale
        ));
    }

    let mut text = String::new();
    text.push_str("// Generated by `build.rs`; do not edit. See that file for the photocraft\n");
    text.push_str(
        "// convention this follows (crates/ui-egui/build.rs:36-41) and the pinned revs.\n",
    );
    text.push_str("//\n");
    text.push_str(
        "/// `(locale, shared catalog, zenman catalog)`, one entry per locale; `\"\"` for a\n",
    );
    text.push_str("/// missing file. Empty means English only: `craft_i18n::Lang::EN` returns the source text.\n");
    text.push_str("pub static CRAFT_I18N: &craft_i18n::Embedded = &[\n");
    text.push_str(&rows);
    text.push_str("];\n");
    Generated { text, catalogs }
}

/// One locale's two catalogs, already rendered as craft-i18n TSV.
struct LocaleCatalog {
    locale: String,
    shared: String,
    app: String,
}

/// Read every locale in `dir`, in either supported layout.
fn collect(dir: &Path) -> Result<Vec<LocaleCatalog>, String> {
    if !dir.is_absolute() {
        // photocraft's reason, verbatim: build scripts run in the crate directory and `include_str!`
        // resolves against the generated file, so a relative path would mean two different places.
        return Err(format!("{} is not an absolute path", dir.display()));
    }
    println!("cargo::rerun-if-changed={}", dir.display());
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let read =
        std::fs::read_dir(dir).map_err(|e| format!("{} cannot be read ({e})", dir.display()))?;
    let mut names: Vec<PathBuf> = read.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    names.sort();
    for path in names {
        if path.is_dir() {
            // The upstream layout: `<dir>/<locale>/<APP_CATALOG>`, plus the optional shared catalog.
            let Some(locale) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let app = path.join(APP_CATALOG);
            if !app.is_file() {
                continue;
            }
            println!("cargo::rerun-if-changed={}", app.display());
            let shared = path.join(SHARED_CATALOG);
            if shared.is_file() {
                println!("cargo::rerun-if-changed={}", shared.display());
            }
            out.push(LocaleCatalog {
                locale: locale.to_string(),
                shared: std::fs::read_to_string(&shared).unwrap_or_default(),
                app: std::fs::read_to_string(&app)
                    .map_err(|e| format!("{} cannot be read ({e})", app.display()))?,
            });
            continue;
        }
        // Our layout: `<dir>/<locale>.json`.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(locale) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        println!("cargo::rerun-if-changed={}", path.display());
        if locale == SOURCE_LOCALE {
            // The source text is not a translation; read it only to catch a malformed file.
            if let Err(e) = parse_json_catalog(&path) {
                problems.push(format!("{}: {e}", path.display()));
            }
            continue;
        }
        let catalog = parse_json_catalog(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        out.push(LocaleCatalog {
            locale: locale.to_string(),
            shared: render(&catalog.shared),
            app: render(&catalog.app),
        });
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    out.sort_by(|a, b| a.locale.cmp(&b.locale));
    Ok(out)
}

/// A parsed `i18n/<locale>.json`.
struct JsonCatalog {
    shared: Vec<(String, String, String)>,
    app: Vec<(String, String, String)>,
}

/// Parse our JSON catalog form:
///
/// ```json
/// { "locale": "pt", "shared": { "menu": { "Open": "Abrir" } }, "app": { "@id": { "file.open": "…" } } }
/// ```
///
/// `locale` must match the file name's stem, and every value must be a string; anything else is an
/// error rather than a silently empty catalog, because a build that quietly loses translations is the
/// failure mode this whole file is written against.
fn parse_json_catalog(path: &Path) -> Result<JsonCatalog, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "the top level must be an object".to_string())?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    match object.get("locale") {
        Some(Value::String(locale)) if *locale == stem || stem == "en" => {}
        Some(Value::String(locale)) => {
            return Err(format!(
                "`locale` is {locale:?} but the file is named {stem}.json"
            ));
        }
        Some(_) => return Err("`locale` must be a string".to_string()),
        None => return Err("missing `locale`".to_string()),
    }
    let rows = |key: &str| -> Result<Vec<(String, String, String)>, String> {
        let mut out = Vec::new();
        let Some(Value::Object(contexts)) = object.get(key) else {
            return Ok(out);
        };
        let mut names: Vec<String> = contexts.keys().cloned().collect();
        names.sort();
        for context in names {
            let Value::Object(pairs) = &contexts[&context] else {
                return Err(format!("`{key}.{context}` must be an object"));
            };
            let mut sources: Vec<String> = pairs.keys().cloned().collect();
            sources.sort();
            for source in sources {
                let Value::String(target) = &pairs[&source] else {
                    return Err(format!("`{key}.{context}.{source}` must be a string"));
                };
                if source.contains('\t') || source.contains('\n') || context.contains('\t') {
                    return Err(format!(
                        "`{key}.{context}.{source}`: a context or source string contains a tab or newline"
                    ));
                }
                let _ = &target;
                out.push((context.clone(), source, target.clone()));
            }
        }
        Ok(out)
    };
    Ok(JsonCatalog {
        shared: rows("shared")?,
        app: rows("app")?,
    })
}

/// Render rows as craft-i18n's TSV: `context\tsource\ttranslation` per line (`@id` is the framework's
/// own context for keyed lookups; a normal row is English source text → translation).
fn render(rows: &[(String, String, String)]) -> String {
    let mut out = String::new();
    for (context, source, target) in rows {
        out.push_str(context);
        out.push('\t');
        out.push_str(source);
        out.push('\t');
        out.push_str(&target.replace(['\n', '\r'], " "));
        out.push('\n');
    }
    out
}
