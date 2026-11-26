//! The step-10c dialogs, driven through the shell's real seams: a live `Session`, a real directory,
//! real `Event::Key`s and real clicks on real buttons.
//!
//! What this file owns (plan §7 row 10's dialog half, and its two named criteria):
//!
//! * **F2 rename is in-cell** — the editor is the row's own (`Tab::rename_editor`), Enter commits it,
//!   Escape cancels it, and a taken name is refused *in the cell* instead of clobbering (Go asked in a
//!   `window.prompt`, `context-menu.js:435`, and then in a second `alert()`);
//! * **toasts stack** — N notices render as N cards, in posting order, newest last, each expiring on its
//!   own clock, capped;
//! * the three prompts that already had a home: `NewFolder` → `Session::mkdir`, the `Chmod` menu row →
//!   `Session::chmod`, `PermanentDelete` → `Session::delete` behind plan **D6**'s answer;
//! * the paste-conflict question, asked *before* anything is written, and each of its three answers.
//!
//! The safety property is asserted as an on-disk fact, not as a toast: a cancelled confirm leaves the
//! file where it was.

mod common;

use std::path::{Path, PathBuf};

use egui::{Key, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::ops::clipboard::{ClipOp, ClipScope, ClipboardStore};
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::{Dialog, Prompt};
use zenman_ui::keymap::{Chord, Command};
use zenman_ui::state::{Tab, panes_for};
use zenman_ui::widgets::toast;

const SETTLE: usize = 6;

// --------------------------------------------------------------------- harness plumbing

/// A fixture directory with the named files (leftovers from an earlier run of this target are
/// removed — the trees persist across runs, plan §9.2 step 9).
fn tree(tag: &str, files: &[&str]) -> PathBuf {
    // The trees live in `CARGO_TARGET_TMPDIR` and persist between runs (plan §9.2 step 9), so every
    // fixture starts by being removed: a rename test that left its result behind would otherwise make
    // the next run refuse its own target name.
    let root = clean(tag);
    std::fs::create_dir_all(&root).expect("fixture directory");
    for name in files {
        let path = root.join(name);
        std::fs::write(&path, format!("{name} original\n")).expect("fixture file");
    }
    root
}

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

fn clean(tag: &str) -> PathBuf {
    let root = scratch(tag);
    common::remove_tree(&root);
    root
}

fn fs_path(path: &Path) -> FsPath {
    FsPath::from_abs(abs(path))
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn build_harness(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_size(size)
        // Dialogs, jobs and the coalesced `DirChanged` window stretch the frame loop (plan §9.2 10a/10b).
        .with_max_steps(64)
        // The fork's `TextEdit` never surfaces an accessible name (its `hint_text` placeholder does not
        // reach the a11y node once the field holds text), so every frame with an open prompt trips the
        // harness's unnamed-input assertion. The fields do carry hover and hint text; the check stays on
        // everywhere else.
        .with_accessibility_check(false)
        .with_context_setup(|ctx| {
            egui_extras::install_image_loaders(ctx);
            zenman_ui::theme::fonts::install(ctx);
            zenman_ui::theme::apply(ctx, zenman_core::store::config::ThemeMode::Dark);
        })
        .build_ui_state(
            |ui, app| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
    settle(&mut harness);
    harness
}

/// A single-pane shell listing `dir`.
fn pane_harness(tag: &str, dir: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fs_path(dir));
    build_harness(app, egui::vec2(1200.0, 800.0))
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

fn shown<'h>(harness: &'h Harness<'static, ZenApp>, slot: PaneSlot) -> &'h Tab {
    harness
        .state()
        .model
        .panes
        .get(&slot)
        .and_then(|p| p.active_tab())
        .unwrap_or_else(|| panic!("the harness has a shown tab in {slot:?}"))
}

fn names(tab: &Tab) -> Vec<String> {
    tab.entries
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect()
}

// ------------------------------------------------------------------------- the driving seams

/// Run one command through the shell's single seam (`ZenApp::dispatch`, the same call a menu row makes).
fn dispatch(harness: &mut Harness<'static, ZenApp>, slot: PaneSlot, command: Command) {
    let ctx = harness.ctx.clone();
    harness.state_mut().dispatch(slot, command, &ctx);
    settle(harness);
}

fn press(harness: &mut Harness<'static, ZenApp>, chord: Chord) {
    harness.input_mut().events.push(chord.event());
    harness.run();
    harness.run();
}

fn click(harness: &mut Harness<'static, ZenApp>, label: &str) {
    harness.get_by_label(label).click();
    settle(harness);
}

/// The open prompt, mutably — the test's stand-in for the user's keystrokes in the field.
fn edit_prompt(harness: &mut Harness<'static, ZenApp>, text: &str) {
    let found = {
        let app = harness.state_mut();
        match app.dialogs.active_mut() {
            Some(Dialog::Prompt(prompt)) => {
                prompt.buffer = text.to_string();
                prompt.error = None;
                true
            }
            other => {
                panic!("no prompt is open, only {other:?}")
            }
        }
    };
    assert!(found);
    harness.run();
}

fn prompt(harness: &Harness<'static, ZenApp>) -> Prompt {
    match harness.state().dialogs.active() {
        Some(Dialog::Prompt(prompt)) => prompt.clone(),
        other => panic!("expected a prompt, found {other:?}"),
    }
}

fn dialog_open(harness: &Harness<'static, ZenApp>) -> bool {
    harness.state().dialogs.is_open()
}

// ------------------------------------------------------------------------ in-cell rename

#[test]
fn f2_rename_is_in_the_row_and_enter_commits_it() {
    let dir = tree("dlg-rename", &["gamma-source.txt"]);
    let mut harness = pane_harness("dlg-rename", &dir);
    harness.get_by_label("gamma-source.txt").click();
    settle(&mut harness);
    press(&mut harness, Chord::key(Key::F2));
    settle(&mut harness);

    // The editor is the *row's*, not a modal: no dialog is open, and the tab carries the field.
    assert!(!dialog_open(&harness), "rename is in-cell, not a dialog");
    let editor = shown(&harness, PaneSlot::Left)
        .rename_editor
        .clone()
        .expect("F2 opened the in-cell editor");
    assert_eq!(editor.buf, "gamma-source.txt");
    assert_eq!(
        editor.select, 12,
        "the base name is selected, `.txt` is not"
    );

    edit_editor(&mut harness, "gamma-renamed.txt");
    press(&mut harness, Chord::key(Key::Enter));
    common::settle_until(
        &mut harness,
        "the rename lands and the listing follows",
        |h| {
            dir.join("gamma-renamed.txt").is_file()
                && !dir.join("gamma-source.txt").exists()
                && names(shown(h, PaneSlot::Left)).contains(&"gamma-renamed.txt".to_string())
        },
    );
    assert!(
        shown(&harness, PaneSlot::Left).rename_editor.is_none(),
        "a committed rename closes its editor"
    );
    assert!(
        names(shown(&harness, PaneSlot::Left)).contains(&"gamma-renamed.txt".to_string()),
        "the listing reloaded onto the new name"
    );
}

fn edit_editor(harness: &mut Harness<'static, ZenApp>, text: &str) {
    {
        let app = harness.state_mut();
        let tab = app
            .model
            .panes
            .get_mut(&PaneSlot::Left)
            .and_then(|p| p.active_tab_mut())
            .expect("a shown tab");
        let editor = tab.rename_editor.as_mut().expect("an open in-cell editor");
        editor.buf = text.to_string();
    }
    harness.run();
}

#[test]
fn escape_in_the_cell_leaves_the_name_alone() {
    let dir = tree("dlg-rename-esc", &["keepme.txt"]);
    let mut harness = pane_harness("dlg-rename-esc", &dir);
    harness.get_by_label("keepme.txt").click();
    settle(&mut harness);
    press(&mut harness, Chord::key(Key::F2));
    settle(&mut harness);
    edit_editor(&mut harness, "not-this-name.txt");
    press(&mut harness, Chord::key(Key::Escape));
    settle(&mut harness);
    assert!(
        dir.join("keepme.txt").is_file() && !dir.join("not-this-name.txt").exists(),
        "Escape is a cancel, not a partial write"
    );
    assert!(
        shown(&harness, PaneSlot::Left).rename_editor.is_none(),
        "the editor is gone"
    );
    assert_eq!(names(shown(&harness, PaneSlot::Left)), vec!["keepme.txt"]);
}

#[test]
fn a_taken_name_is_refused_in_the_cell_and_overwrites_nothing() {
    let dir = tree("dlg-rename-taken", &["one.txt", "two.txt"]);
    let mut harness = pane_harness("dlg-rename-taken", &dir);
    harness.get_by_label("one.txt").click();
    settle(&mut harness);
    press(&mut harness, Chord::key(Key::F2));
    settle(&mut harness);
    edit_editor(&mut harness, "two.txt");
    press(&mut harness, Chord::key(Key::Enter));
    settle(&mut harness);
    let editor = shown(&harness, PaneSlot::Left)
        .rename_editor
        .clone()
        .expect("the refusal keeps the editor open");
    assert!(
        editor.error.is_some(),
        "the reason is shown where the name was typed"
    );
    assert!(
        dir.join("one.txt").is_file() && dir.join("two.txt").is_file(),
        "both files are exactly where they were"
    );
    assert_eq!(
        std::fs::read(dir.join("two.txt")).expect("readable"),
        b"two.txt original\n"
    );
}

// ---------------------------------------------------------------------------- new folder

#[test]
fn new_folder_prompt_creates_the_directory() {
    let dir = tree("dlg-mkdir", &["neighbour.txt"]);
    let mut harness = pane_harness("dlg-mkdir", &dir);
    dispatch(&mut harness, PaneSlot::Left, Command::NewFolder);
    let opened = prompt(&harness);
    assert_eq!(
        opened.label, "Enter new folder name:",
        "Go's own copy (`context-menu.js:548`)"
    );
    assert_eq!(opened.buffer, "New Folder", "Go's own default");
    edit_prompt(&mut harness, "fresh-dir");
    click(&mut harness, "Create");
    common::settle_until(&mut harness, "the new directory exists", |_| {
        dir.join("fresh-dir").is_dir()
    });
    assert!(!dialog_open(&harness), "the prompt closed on its answer");
    assert!(harness.state().new_folder_request.is_none());
}

#[test]
fn new_folder_prompt_refuses_a_duplicate_name_inline() {
    let root = clean("dlg-mkdir-dupe");
    std::fs::create_dir_all(root.join("taken")).expect("a directory fixture");
    let mut harness = pane_harness("dlg-mkdir-dupe", &root);
    dispatch(&mut harness, PaneSlot::Left, Command::NewFolder);
    edit_prompt(&mut harness, "taken");
    click(&mut harness, "Create");
    assert!(
        dialog_open(&harness),
        "a taken name does not close the prompt"
    );
    assert!(
        prompt(&harness)
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("already exists"),
        "the refusal is inline, in the dialog that asked"
    );
    // Only one directory is in the fixture, so nothing new appeared.
    assert_eq!(std::fs::read_dir(&root).expect("readable").count(), 1);
}

#[test]
fn new_folder_prompt_refuses_an_empty_name_inline() {
    let dir = tree("dlg-mkdir-empty", &["only.txt"]);
    let mut harness = pane_harness("dlg-mkdir-empty", &dir);
    dispatch(&mut harness, PaneSlot::Left, Command::NewFolder);
    edit_prompt(&mut harness, "   ");
    click(&mut harness, "Create");
    assert!(
        dialog_open(&harness),
        "an empty name is refused, not created"
    );
    assert!(prompt(&harness).error.is_some());
    assert_eq!(names(shown(&harness, PaneSlot::Left)), vec!["only.txt"]);
}

#[test]
fn new_folder_prompt_cancel_creates_nothing() {
    let dir = tree("dlg-mkdir-cancel", &["only.txt"]);
    let mut harness = pane_harness("dlg-mkdir-cancel", &dir);
    dispatch(&mut harness, PaneSlot::Left, Command::NewFolder);
    edit_prompt(&mut harness, "never-created");
    click(&mut harness, "Cancel");
    settle(&mut harness);
    assert!(!dialog_open(&harness));
    assert!(
        !dir.join("never-created").exists(),
        "Cancel is not a silent Enter"
    );
}

// ------------------------------------------------------------------------------- chmod

#[test]
fn chmod_prompt_seeds_the_current_mode_and_applies_the_new_one() {
    let dir = tree("dlg-chmod", &["script.sh"]);
    set_mode(&dir.join("script.sh"), 0o640);
    let mut harness = pane_harness("dlg-chmod", &dir);
    harness.get_by_label("script.sh").click();
    settle(&mut harness);
    dispatch(&mut harness, PaneSlot::Left, Command::Chmod);
    let opened = prompt(&harness);
    assert_eq!(
        opened.buffer, "0640",
        "the field opens on the row's own mode, in the four-digit octal spelling the log carries"
    );
    assert!(
        opened.label.contains("Changing permissions for 1 item(s)."),
        "Go's own prompt copy (`context-menu.js:476-484`)"
    );
    assert!(
        opened
            .label
            .contains("Enter 3 or 4-digit octal permissions:")
    );
    edit_prompt(&mut harness, "600");
    click(&mut harness, "Apply");
    settle(&mut harness);
    assert_eq!(
        mode_of(&dir.join("script.sh")),
        0o600,
        "the mode is really set"
    );
}

#[test]
fn chmod_prompt_refuses_a_bad_octal_inline() {
    let dir = tree("dlg-chmod-bad", &["script.sh"]);
    set_mode(&dir.join("script.sh"), 0o644);
    let mut harness = pane_harness("dlg-chmod-bad", &dir);
    harness.get_by_label("script.sh").click();
    settle(&mut harness);
    dispatch(&mut harness, PaneSlot::Left, Command::Chmod);
    edit_prompt(&mut harness, "999");
    click(&mut harness, "Apply");
    assert!(
        dialog_open(&harness),
        "Go re-asked with a second `alert`; we stay put"
    );
    assert_eq!(
        prompt(&harness).error.as_deref(),
        // `context-menu.js:489`, verbatim.
        Some(
            "Invalid permission format. Please enter a 3 or 4-digit octal number (e.g., 755 or 644)."
        ),
        "the refusal is the one Go printed"
    );
    assert_eq!(
        mode_of(&dir.join("script.sh")),
        0o644,
        "nothing was changed"
    );
}

// --------------------------------------------------------------------- permanent delete

#[test]
fn permanent_delete_asks_then_destroys_and_says_what_it_cost() {
    let dir = tree("dlg-perm-del", &["gone.txt"]);
    let mut harness = pane_harness("dlg-perm-del", &dir);
    harness.get_by_label("gone.txt").click();
    settle(&mut harness);
    press(&mut harness, Chord::shift(Key::Delete));
    settle(&mut harness);
    assert!(
        dir.join("gone.txt").is_file(),
        "the chord alone destroys nothing"
    );
    // Go's copy (`context-menu.js:507`), plus the part a browser `confirm` could not carry.
    assert!(dialog_open(&harness));
    click(&mut harness, "Delete permanently");
    common::settle_until(&mut harness, "the file is gone", |_| {
        !dir.join("gone.txt").exists()
    });
    // The delete is a *job* (plan §4.1), so it reports itself by the row disappearing rather than by
    // a synchronous receipt toast; an error would have been one, and none was posted.
    common::settle_until(&mut harness, "the listing drops the row", |h| {
        !names(shown(h, PaneSlot::Left)).contains(&"gone.txt".to_string())
    });
    let errors: Vec<String> = harness
        .state()
        .notices
        .queued()
        .iter()
        .filter(|notice| notice.level == toast::Level::Error)
        .map(|notice| notice.text.clone())
        .collect();
    assert!(
        errors.is_empty(),
        "a successful delete is not an error: {errors:?}"
    );
}

#[test]
fn permanent_delete_cancelled_leaves_the_bytes_on_disk() {
    // Plan D6's safety property, asserted on the filesystem rather than in a toast.
    let dir = tree("dlg-perm-keep", &["lives.txt"]);
    let mut harness = pane_harness("dlg-perm-keep", &dir);
    harness.get_by_label("lives.txt").click();
    settle(&mut harness);
    press(&mut harness, Chord::shift(Key::Delete));
    settle(&mut harness);
    click(&mut harness, "Cancel");
    settle(&mut harness);
    assert!(!dialog_open(&harness));
    assert!(
        dir.join("lives.txt").is_file(),
        "a cancelled confirmation deletes nothing at all"
    );
    assert_eq!(names(shown(&harness, PaneSlot::Left)), vec!["lives.txt"]);
}

// -------------------------------------------------------------------------- paste conflicts

/// `src/dup.txt` onto `dst/dup.txt`, then each of the three answers.
fn conflict_scene(tag: &str) -> (Harness<'static, ZenApp>, PathBuf, PathBuf) {
    let root = clean(tag);
    let src = root.join("src");
    let dst = root.join("dst");
    std::fs::create_dir_all(&src).expect("src");
    std::fs::create_dir_all(&dst).expect("dst");
    std::fs::write(src.join("dup.txt"), b"incoming bytes\n").expect("src file");
    std::fs::write(dst.join("dup.txt"), b"existing bytes\n").expect("dst file");
    let session = common::session(tag);
    session
        .clipboard_store(&ClipboardStore {
            op: ClipOp::Copy,
            sources: vec![fs_path(&src.join("dup.txt"))],
            scope: ClipScope::Selected,
        })
        .expect("the clipboard takes the copy");
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fs_path(&dst));
    (build_harness(app, egui::vec2(1200.0, 800.0)), src, dst)
}

#[test]
fn a_colliding_paste_asks_first_and_overwrites_only_when_told() {
    let (mut harness, src, dst) = conflict_scene("dlg-paste-over");
    dispatch(&mut harness, PaneSlot::Left, Command::Paste);
    assert!(
        dialog_open(&harness),
        "the collision is asked about before a byte moves"
    );
    assert_eq!(
        std::fs::read(dst.join("dup.txt")).expect("readable"),
        b"existing bytes\n",
        "opening the question wrote nothing"
    );
    click(&mut harness, "Overwrite");
    common::settle_until(&mut harness, "the overwrite lands", |_| {
        std::fs::read(dst.join("dup.txt")).is_ok_and(|bytes| bytes == b"incoming bytes\n")
    });
    assert!(src.join("dup.txt").is_file(), "a copy leaves its source");
}

#[test]
fn a_colliding_paste_answered_skip_keeps_the_destination_bytes() {
    let (mut harness, _src, dst) = conflict_scene("dlg-paste-skip");
    dispatch(&mut harness, PaneSlot::Left, Command::Paste);
    click(&mut harness, "Skip");
    settle(&mut harness);
    // `SkipExisting` is core's `ConflictPolicy::DEFAULT` — Go's silent skip. The answer makes it a
    // *decision* rather than an accident: the destination keeps its bytes, and nothing is added.
    assert!(!dialog_open(&harness), "the question was answered");
    assert_eq!(
        std::fs::read(dst.join("dup.txt")).expect("readable"),
        b"existing bytes\n",
        "Skip never overwrites"
    );
    assert_eq!(
        std::fs::read_dir(&dst).expect("readable").count(),
        1,
        "and never adds a second name"
    );
}

#[test]
fn a_colliding_paste_answered_rename_keeps_both_files() {
    let (mut harness, _src, dst) = conflict_scene("dlg-paste-rename");
    dispatch(&mut harness, PaneSlot::Left, Command::Paste);
    click(&mut harness, "Rename");
    common::settle_until(&mut harness, "the second copy appears", |_| {
        std::fs::read(dst.join("dup.txt")).is_ok_and(|bytes| bytes == b"existing bytes\n")
            && std::fs::read_dir(&dst).is_ok_and(|entries| entries.count() > 1)
    });
    let names: Vec<String> = std::fs::read_dir(&dst)
        .expect("readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"dup.txt".to_string()), "{names:?}");
    assert!(
        names.iter().any(|name| name != "dup.txt"),
        "both names are there: {names:?}"
    );
}

#[test]
fn a_paste_with_no_collision_runs_without_a_question() {
    let root = clean("dlg-paste-clean");
    let src = root.join("src");
    let dst = root.join("dst");
    std::fs::create_dir_all(&src).expect("src");
    std::fs::create_dir_all(&dst).expect("dst");
    std::fs::write(src.join("unique.txt"), b"copy me\n").expect("src file");
    let session = common::session("dlg-paste-clean");
    session
        .clipboard_store(&ClipboardStore {
            op: ClipOp::Copy,
            sources: vec![fs_path(&src.join("unique.txt"))],
            scope: ClipScope::Selected,
        })
        .expect("the clipboard takes the copy");
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fs_path(&dst));
    let mut harness = build_harness(app, egui::vec2(1200.0, 800.0));
    dispatch(&mut harness, PaneSlot::Left, Command::Paste);
    assert!(!dialog_open(&harness), "no collision, no question");
    common::settle_until(&mut harness, "the copy lands", |_| {
        dst.join("unique.txt").is_file()
    });
}

// ----------------------------------------------------------------------------- toast stack

#[test]
fn five_notices_render_as_five_stacked_toasts_in_order() {
    let dir = tree("dlg-toasts", &["one.txt"]);
    let mut harness = pane_harness("dlg-toasts", &dir);
    let texts: Vec<String> = (0..5).map(|ix| format!("stacked notice {ix}")).collect();
    {
        let app = harness.state_mut();
        for text in &texts {
            app.notices.post(toast::Level::Info, text.clone());
        }
    }
    harness.run();
    let queue: Vec<String> = harness
        .state()
        .notices
        .queued()
        .iter()
        .map(|notice| notice.text.clone())
        .collect();
    assert_eq!(queue, texts, "the queue is posting order, oldest first");
    for text in &texts {
        assert_eq!(
            harness.query_all_by_label(text.as_str()).count(),
            1,
            "{text} is drawn exactly once, as its own card"
        );
    }
}

#[test]
fn toasts_expire_on_their_own_clock_and_the_stack_caps() {
    let t0 = std::time::Instant::now();
    let mut state = toast::State::default();
    // Three at once, then one much later: the late one must not drag the early two down with it.
    state.post_at(t0, toast::Level::Info, "early a");
    state.post_at(t0, toast::Level::Warning, "early b");
    state.post_at(t0, toast::Level::Error, "early c");
    let late = t0 + toast::NOTICE_TTL / 2;
    state.post_at(late, toast::Level::Info, "late d");
    assert_eq!(state.queued().len(), 4, "they stack rather than replace");
    assert_eq!(
        state.expire(t0 + toast::NOTICE_TTL),
        3,
        "only the three old ones go"
    );
    assert_eq!(
        state
            .queued()
            .iter()
            .map(|n| n.text.as_str())
            .collect::<Vec<_>>(),
        vec!["late d"],
        "the young notice survives its elders"
    );
    for ix in 0..toast::NOTICE_CAP + 5 {
        state.post_at(late, toast::Level::Info, format!("flood {ix}"));
    }
    assert_eq!(
        state.queued().len(),
        toast::NOTICE_CAP,
        "a flood is capped, not stacked forever"
    );
    assert_eq!(
        state.queued().back().map(|n| n.text.clone()),
        Some(format!("flood {}", toast::NOTICE_CAP + 4)),
        "and the newest is the one that survives"
    );
}

// --------------------------------------------------------------------------- modal safety

#[test]
fn a_second_dialog_is_refused_while_one_is_open_and_keys_do_not_reach_the_panes() {
    let dir = tree("dlg-modal", &["only.txt"]);
    let mut harness = pane_harness("dlg-modal", &dir);
    dispatch(&mut harness, PaneSlot::Left, Command::NewFolder);
    let first = prompt(&harness).title;
    // The same command again: refused, and the first dialog is untouched.
    dispatch(&mut harness, PaneSlot::Left, Command::NewFolder);
    assert_eq!(prompt(&harness).title, first, "still the first dialog");
    // While a modal is up, the panes must not eat its keys: `Select all` is a chord-backed command,
    // and its effect (every row selected) would be visible.
    let before = shown(&harness, PaneSlot::Left).sel.len();
    press(&mut harness, Chord::ctrl(Key::A));
    settle(&mut harness);
    assert_eq!(
        shown(&harness, PaneSlot::Left).sel.len(),
        before,
        "the modal owns the keyboard (plan F15)"
    );
    click(&mut harness, "Cancel");
    assert!(!dialog_open(&harness));
}

// ---------------------------------------------------------------------------------- unix bits

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(not(unix))]
fn mode_of(_path: &Path) -> u32 {
    0
}
