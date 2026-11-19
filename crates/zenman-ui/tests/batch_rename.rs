//! Step 14b: batch rename (plan §7 row 14, §5.8 `dialogs/batch_rename.rs`).
//!
//! The preview's three statuses (including the two Go behaviours 14a kept *visibly*), the "not per
//! frame" rule behind the plan's 2 ms claim, and Apply renaming on disk through `Session::batch_rename`.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use egui::Vec2;
use egui_kittest::Harness;

use zenman_core::ops::ConflictPolicy;
use zenman_core::ops::batch_rename::{Row, RowStatus, Rules};
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::Dialog;
use zenman_ui::dialogs::batch_rename::{self, BatchRename};
use zenman_ui::state::panes_for;

const SETTLE: usize = 6;

fn rows(names: &[&str]) -> Vec<Row> {
    names
        .iter()
        .map(|name| {
            Row::new(
                AbsPath::from_path(format!("/tmp/{name}")).expect("a synthetic path"),
                *name,
                false,
            )
        })
        .collect()
}

fn preview(
    names: &[&str],
    rules: &Rules,
    existing: &[&str],
) -> zenman_core::ops::batch_rename::PreviewSet {
    let set: HashSet<String> = existing.iter().map(|s| (*s).to_string()).collect();
    batch_rename::preview(&rows(names), rules, &set)
}

fn status(preview: &zenman_core::ops::batch_rename::PreviewSet, name: &str) -> (RowStatus, String) {
    let row = preview
        .rows
        .iter()
        .find(|r| r.original == name)
        .unwrap_or_else(|| panic!("no preview row for {name}"));
    (row.status, row.preview.clone())
}

#[test]
fn the_three_preview_statuses_are_gos() {
    let rules = Rules {
        find: "a".to_string(),
        replace: "o".to_string(),
        ..Rules::default()
    };
    // `cat.txt` → `cot.txt`, and `cot.txt` is already in the folder (it is in `existing`, which is
    // the listing minus the selection): Go's `File already exists in folder`.
    let set = preview(&["cat.txt"], &rules, &["cot.txt"]);
    assert_eq!(
        status(&set, "cat.txt").0,
        RowStatus::Conflict,
        "{:?} must collide",
        set.rows
    );
    assert_eq!(status(&set, "cat.txt").1, "cot.txt");
    assert!(set.has_conflicts());
    // A rule that changes nothing is Go's grey row — and Go tests *that* first, so an unchanged
    // row is never a conflict even when its name is also somebody else's target.
    let same = preview(&["cot.txt"], &rules, &[]);
    assert_eq!(status(&same, "cot.txt").0, RowStatus::Unchanged);
    assert_eq!(same.changed(), 0);
    // And the plain success case, with nothing in the way.
    let ok = preview(&["cat.txt"], &rules, &[]);
    assert_eq!(status(&ok, "cat.txt").0, RowStatus::Success);
    assert_eq!(ok.changed(), 1);
}

#[test]
fn the_counter_tokens_number_the_selection_from_start_by_step() {
    // The rules run on the *stem* by default (Go's behaviour without the `ext` box), so these
    // rows have no extension to keep out of the way.
    let rules = Rules {
        find: ".".to_string(),
        replace: "pic[n:2]".to_string(),
        regex: true,
        counter_start: 7,
        counter_step: 2,
        ..Rules::default()
    };
    let set = preview(&["a", "b", "c"], &rules, &[]);
    assert_eq!(status(&set, "a").1, "pic07", "{:?}", set.rows);
    assert_eq!(status(&set, "b").1, "pic09");
    assert_eq!(status(&set, "c").1, "pic11");
    assert_eq!(set.changed(), 3);
    // `[n]` without a width keeps Go's unpadded counter, and it is one number per *row*.
    let plain = Rules {
        replace: "x[n]".to_string(),
        ..rules.clone()
    };
    let set = preview(&["a", "b"], &plain, &[]);
    assert_eq!(status(&set, "a").1, "x7", "{:?}", set.rows);
    assert_eq!(status(&set, "b").1, "x9");
}

#[test]
fn the_rules_run_on_the_stem_unless_the_extension_box_is_ticked() {
    // The counterpart of the counter test: with the default rules a match inside the extension is
    // invisible, which is the one Go behaviour the label of the `ext` box did make clear.
    let rules = Rules {
        find: "jpg".to_string(),
        replace: "png".to_string(),
        ..Rules::default()
    };
    let set = preview(&["a.jpg"], &rules, &[]);
    assert_eq!(
        status(&set, "a.jpg").0,
        RowStatus::Unchanged,
        "{:?}",
        set.rows
    );
    let whole = Rules {
        include_extension: true,
        ..rules.clone()
    };
    let set = preview(&["a.jpg"], &whole, &[]);
    assert_eq!(status(&set, "a.jpg").1, "a", "{:?}", set.rows);
}

#[test]
fn the_extension_box_drops_the_extension_as_go_did() {
    // `batch-rename.js:281`: with `ext` ticked, `previewName` is `finalNamePart` alone. The port
    // keeps the bug, so this test says it out loud rather than letting it be "fixed" later.
    let rules = Rules {
        find: "cat".to_string(),
        replace: "dog".to_string(),
        include_extension: true,
        ..Rules::default()
    };
    let set = preview(&["cat.txt"], &rules, &[]);
    let (_, name) = status(&set, "cat.txt");
    assert_eq!(
        name, "dog",
        "the extension is no longer dropped — parity broken"
    );
}

#[test]
fn a_bad_pattern_is_one_message_and_changes_nothing() {
    let rules = Rules {
        find: "(".to_string(),
        replace: "x".to_string(),
        regex: true,
        ..Rules::default()
    };
    let set = preview(&["a.txt", "b.txt"], &rules, &[]);
    assert!(
        set.regex_error.is_some(),
        "an uncompiled pattern went unreported"
    );
    assert!(
        set.rows
            .iter()
            .all(|r| r.status == RowStatus::Unchanged && r.preview == r.original),
        "a bad pattern still proposed renames: {:?}",
        set.rows
    );
    let dialog = BatchRename::new(
        PaneSlot::Left,
        rows(&["a.txt", "b.txt"]),
        rules,
        HashSet::new(),
    );
    assert!(
        !dialog.can_apply(),
        "Apply is live while the pattern does not compile"
    );
}

#[test]
fn a_collision_gets_a_second_pass_only_when_the_policy_numbers_it() {
    // Two rows, one target: Go showed the later one as a conflict and applied nothing to it.
    let rules = Rules {
        find: ".".to_string(),
        replace: "x".to_string(),
        regex: true,
        ..Rules::default()
    };
    let set = preview(&["a.txt", "b.txt"], &rules, &[]);
    assert_eq!(
        set.conflicts().count(),
        1,
        "a duplicate target went unnoticed: {:?}",
        set.rows
    );
    assert_eq!(set.rows[0].status, RowStatus::Success, "{:?}", set.rows);
    assert!(
        set.rows[1].reason.contains("collision"),
        "{:?}",
        set.rows[1]
    );
    let mut dialog = BatchRename::new(
        PaneSlot::Left,
        rows(&["a.txt", "b.txt"]),
        rules,
        HashSet::new(),
    );
    assert!(
        !dialog.can_apply(),
        "Apply is live over unresolved conflicts"
    );
    dialog.policy = ConflictPolicy::Rename;
    assert!(dialog.can_apply(), "the numbering policy must unlock Apply");
}

#[test]
fn the_preview_is_recomputed_on_change_and_never_per_frame() {
    let root = scratch("br-frame");
    std::fs::create_dir_all(&root).expect("fixture directory");
    let mut app = app("br-frame", &root);
    let dialog = BatchRename::new(
        PaneSlot::Left,
        rows(&["a.txt", "b.txt"]),
        Rules::default(),
        HashSet::new(),
    );
    assert_eq!(
        dialog.computes, 1,
        "the first preview is the only one on open"
    );
    app.dialogs.open(Dialog::BatchRename(Box::new(dialog)));
    let mut harness = build(app, egui::vec2(1400.0, 900.0));
    let before = computes(&harness);
    for _ in 0..12 {
        harness.run();
    }
    assert_eq!(
        computes(&harness),
        before,
        "the dialog recomputed while nothing was typed — the 2 ms figure is per keystroke, not per frame"
    );
    {
        let state = harness.state_mut();
        match state.dialogs.active_mut() {
            Some(Dialog::BatchRename(b)) => {
                b.rules.find = "b".to_string();
                b.recompute();
            }
            other => panic!("the dialog is not the batch rename: {other:?}"),
        }
    }
    assert_eq!(computes(&harness), before + 1);
    harness.run();
}

#[test]
fn apply_renames_on_disk_as_one_batch_and_the_numbering_pass_clobbers_nothing() {
    let root = scratch("br-apply");
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    for name in ["shot1.jpg", "shot2.jpg"] {
        std::fs::write(root.join(name), "x\n").expect("fixture file");
    }
    let dir = AbsPath::from_path(&root).expect("absolute");
    let mut app = app("br-apply", &root);
    let selected = vec![
        Row::new(dir.join("shot1.jpg").expect("joined"), "shot1.jpg", false),
        Row::new(dir.join("shot2.jpg").expect("joined"), "shot2.jpg", false),
    ];
    let rules = Rules {
        find: "shot".to_string(),
        replace: "photo".to_string(),
        ..Rules::default()
    };
    app.apply_batch_rename(
        PaneSlot::Left,
        selected,
        rules,
        ConflictPolicy::SkipExisting,
    );
    assert!(
        root.join("photo1.jpg").is_file(),
        "the batch did not reach the disk"
    );
    assert!(
        root.join("photo2.jpg").is_file(),
        "the batch renamed only half of it"
    );
    assert!(
        !root.join("shot1.jpg").exists(),
        "the old name is still there"
    );

    // Both rows now want `photo.jpg`: 14a's second pass numbers the loser instead of clobbering.
    let clash = vec![
        Row::new(dir.join("photo1.jpg").expect("joined"), "photo1.jpg", false),
        Row::new(dir.join("photo2.jpg").expect("joined"), "photo2.jpg", false),
    ];
    let rules = Rules {
        find: "[12]".to_string(),
        replace: String::new(),
        regex: true,
        ..Rules::default()
    };
    app.apply_batch_rename(PaneSlot::Left, clash, rules, ConflictPolicy::Rename);
    assert!(
        root.join("photo.jpg").is_file(),
        "the first target was lost"
    );
    let numbered = std::fs::read_dir(&root)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("photo"))
        .collect::<Vec<_>>();
    assert_eq!(numbered.len(), 2, "the second pass produced {numbered:?}");
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("batch rename")),
        "the apply was silent"
    );
}

// ------------------------------------------------------------------ harness plumbing

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

fn app(tag: &str, dir: &Path) -> ZenApp {
    common::remove_tree(
        &std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("zenman-ui-fixtures")
            .join(format!("zenman-ui-test-{tag}")),
    );
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(
        PaneSlot::Left,
        FsPath::from_abs(AbsPath::from_path(dir).expect("an absolute fixture path")),
    );
    app
}

fn build(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_max_steps(64)
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
    for _ in 0..SETTLE {
        harness.run();
    }
    harness
}

fn computes(harness: &Harness<'static, ZenApp>) -> usize {
    match harness.state().dialogs.active() {
        Some(Dialog::BatchRename(b)) => b.computes,
        other => panic!("the batch dialog closed: {other:?}"),
    }
}
