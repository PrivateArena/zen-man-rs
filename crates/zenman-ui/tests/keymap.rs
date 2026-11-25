//! The step-10a keymap: the chord table as the single source of truth (plan **D7**), and the
//! keyboard actually reaching `ZenApp::dispatch` — real `Event::Key`s driven through the same
//! harness the pane tests use, against a real `Session` on a real directory.
//!
//! Two halves:
//!
//! * **Coverage, table-driven.** Every [`Command`] in [`Command::ALL`] round-trips through
//!   `Chord::parse(c.display())`, `hint` and `bindings_for` agree, every chord *answers* its own
//!   command when fed back as an event, no chord answers two commands, and `Shift+Delete` is not
//!   shadowed by `Delete`. This half is what makes 10b's menus safe to build on [`KeyMap::hint`].
//! * **Behaviour.** `Ctrl+A`/`Escape`/`Delete`/`Ctrl+C`+`Ctrl+V`/`Enter`/`Ctrl+W`/`F3`/`F4`/
//!   `Shift+Delete`/`F2` and the no-selection path, each asserted on the state, on **disk**, or on
//!   the toast the dispatch owes the user.
//!
//! The fixture trees live in `CARGO_TARGET_TMPDIR` (fsid-measured: see the D6 test's doc comment),
//! never in the developer's home directory.

mod common;

use std::path::{Path, PathBuf};

use egui::Key;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::ops::delete::DeleteReq;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::platform;
use zenman_core::session::Receipt;
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::keymap::{Chord, Command, KeyMap};
use zenman_ui::state::{Tab, panes_for};
use zenman_ui::widgets::toast::Level;

/// Frames to let the shell settle after a non-job action (same budget as `tests/file_pane.rs`).
const SETTLE: usize = 6;

/// The page size a fresh tab loads (`zenman_ui::state::DEFAULT_PAGE_SIZE`).
const PAGE: usize = zenman_ui::state::DEFAULT_PAGE_SIZE;

// --------------------------------------------------------------------- harness plumbing

/// A fixture directory under cargo's per-test scratch, like `common::listing_tree` but with only
/// the named files — the key tests do not need 457 rows and *do* need to know exactly what is on
/// disk. Leftovers from previous runs of the same target are removed first (the trees persist).
fn tree(tag: &str, files: &[&str]) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"));
    std::fs::create_dir_all(&root).expect("fixture directory");
    let wanted: std::collections::BTreeSet<String> =
        files.iter().map(|f| (*f).to_string()).collect();
    for entry in std::fs::read_dir(&root).expect("fixture directory is readable") {
        let entry = entry.expect("readable directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if !wanted.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    for name in files {
        std::fs::write(root.join(name), b"keymap fixture\n").expect("fixture file");
    }
    root
}

fn fixture_path(path: &Path) -> FsPath {
    FsPath::from_abs(AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn build_harness(app: ZenApp, size: egui::Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(size)
        // Jobs and the `DirChanged` coalescing window make a frame loop outlive the
        // builder's 4-step settle budget; the wait helpers, not the step guard, decide
        // when a test is done.
        .with_max_steps(64)
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

/// A single-pane shell showing `dir`.
fn pane_harness(tag: &str, dir: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fixture_path(dir));
    build_harness(app, egui::vec2(1200.0, 800.0))
}

/// A two-pane shell: `left` in [`PaneSlot::Left`], `right` in [`PaneSlot::Right`].
fn split_harness(tag: &str, left: &Path, right: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Split));
    app.open_tab(PaneSlot::Left, fixture_path(left));
    app.open_tab(PaneSlot::Right, fixture_path(right));
    build_harness(app, egui::vec2(1600.0, 800.0))
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

/// Push one real key event and run a frame — the chord arrives through egui's event queue exactly
/// as a physical press would, and `ZenApp::handle_keys` reads it out of `InputState::events`.
fn press(harness: &mut Harness<'static, ZenApp>, chord: Chord) {
    harness.input_mut().events.push(chord.event());
    harness.run();
    harness.run();
}

/// Press the chord a command's *hint* spells — the coverage↔behaviour bridge: if a hint ever stops
/// parsing or stops answering, every behavioural test that goes through the keyboard notices.
fn press_command(harness: &mut Harness<'static, ZenApp>, command: Command) {
    let map = KeyMap::default();
    let chord = map.hint(command).unwrap_or_else(|| {
        panic!("{command:?} is menu-only; a behavioural test must name a chord")
    });
    let parsed = Chord::parse(&chord).unwrap_or_else(|| panic!("hint {chord:?} does not parse"));
    press(harness, parsed);
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

fn notices(harness: &Harness<'static, ZenApp>) -> Vec<(Level, String)> {
    harness
        .state()
        .notices
        .queued()
        .iter()
        .map(|n| (n.level, n.text.clone()))
        .collect()
}

fn any_notice(harness: &Harness<'static, ZenApp>, needle: &str) -> bool {
    notices(harness)
        .iter()
        .any(|(_, text)| text.contains(needle))
}

/// Like [`common::settle_until`] but the timeout names the state it timed out on (path, loaded
/// names, notices, tab error) — the diagnostics `settle_until`'s bare message leaves out.
fn wait(
    harness: &mut Harness<'static, ZenApp>,
    slot: PaneSlot,
    what: &str,
    mut pred: impl FnMut(&Harness<'static, ZenApp>) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        harness.run();
        if pred(harness) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let tab = shown(harness, slot);
            panic!(
                "waiting for {what} timed out.\n  path: {:?}\n  names: {:?}\n  notices: {:?}\n  tab error: {:?}",
                tab.path,
                names(tab).into_iter().take(12).collect::<Vec<_>>(),
                notices(harness),
                tab.error.as_ref().map(ToString::to_string)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------- coverage half

#[test]
fn every_command_round_trips_and_answers_its_own_chords() {
    let map = KeyMap::default();
    assert_eq!(
        map.commands().count(),
        Command::ALL.len(),
        "one table row per command, none missing"
    );
    for command in Command::ALL {
        let bindings = map.bindings_for(*command);
        // `hint` is `bindings_for`'s first entry, or nothing at all — the two can never disagree.
        assert_eq!(
            map.hint(*command).as_deref(),
            bindings.first().map(Chord::display).as_deref(),
            "{command:?}: the hint a menu prints is the first chord of the table"
        );
        assert_eq!(map.is_bound(*command), map.hint(*command).is_some());
        for chord in bindings {
            let text = chord.display();
            assert_eq!(
                Chord::parse(&text),
                Some(*chord),
                "{command:?} hint {text:?} must parse back to its own chord"
            );
            // Fed back as the event a real press produces, the chord answers *this* command.
            assert_eq!(
                map.lookup_event(&chord.event()),
                Some(*command),
                "pressing {text:?} must answer {command:?}, nothing else"
            );
        }
    }
}

#[test]
fn no_two_commands_answer_one_chord() {
    let map = KeyMap::default();
    let mut pairs: Vec<(Chord, Command)> = map.iterate().collect();
    pairs.sort_by_key(|(chord, _)| *chord);
    let distinct = pairs
        .windows(2)
        .filter(|pair| pair[0].0 == pair[1].0)
        .count();
    assert_eq!(
        distinct, 0,
        "a chord that answers two commands makes one of them unreachable"
    );
}

#[test]
fn shift_del_is_its_own_answer_and_del_does_not_shadow_it() {
    let map = KeyMap::default();
    assert_eq!(
        map.lookup_event(&Chord::key(Key::Delete).event()),
        Some(Command::Delete),
        "bare Del is the trash rung (D6)"
    );
    assert_eq!(
        map.lookup_event(&Chord::shift(Key::Delete).event()),
        Some(Command::PermanentDelete),
        "Shift+Del survives its unshifted twin — `Chord::matches` is exact, not `matches_logically`"
    );
}

// ----------------------------------------------------------------------------- behaviour half

#[test]
fn ctrl_a_selects_everything() {
    let dir = common::listing_tree("keymap-select-all");
    let mut harness = pane_harness("keymap-select-all", &dir);
    press_command(&mut harness, Command::SelectAll);
    let tab = shown(&harness, PaneSlot::Left);
    assert_eq!(
        tab.sel.len(),
        PAGE,
        "Ctrl+A selects every *loaded* row — the page, not the un-fetched remainder"
    );
    assert!(
        harness.query_all_by_label_contains("selected").count() > 0,
        "the pane says how many rows are selected"
    );
}

#[test]
fn escape_clears_the_selection() {
    let dir = common::listing_tree("keymap-escape");
    let mut harness = pane_harness("keymap-escape", &dir);
    press_command(&mut harness, Command::SelectAll);
    assert_eq!(shown(&harness, PaneSlot::Left).sel.len(), PAGE);
    press_command(&mut harness, Command::SelectNone);
    let tab = shown(&harness, PaneSlot::Left);
    assert!(tab.sel.is_empty(), "Escape cleared the selection");
}

/// <kbd>Delete</kbd> against the real D6 ladder, for the volume the fixtures actually live on.
///
/// The fixture tree lives under `CARGO_TARGET_TMPDIR` (`/media/…/target/tmp`), whose fsid differs
/// from the fsid of `$HOME/.local/share/Trash` — so `Env::can_trash` answers **false** for it and,
/// with `DeleteReq::to_trash`'s `permanent_fallback == false`, the ladder's rung 2 refuses with a
/// `PermissionDenied` outcome per path and nothing is destroyed. The test *measures* that answer
/// instead of assuming it, **prints which branch ran**, and asserts the matching contract:
/// refused ⇒ summary toast says the item failed + file still on disk; trashed ⇒ file gone + row
/// gone. A second lane then drives `DeleteReq::permanent` + `permanent_fallback: true` — the
/// opt-in the 10c prompt will set — and asserts the bytes really go.
#[test]
fn del_answers_the_d6_ladder_for_this_volume() {
    let dir = tree("keymap-d6", &["one.txt", "two.txt"]);
    let one = dir.join("one.txt");
    let two = dir.join("two.txt");
    let can_trash = platform::default_env().can_trash(&abs(&one));
    println!(
        "D6 branch: can_trash({}) = {can_trash} — {}",
        one.display(),
        if can_trash {
            "TRASHED branch: this volume has a trash location"
        } else {
            "REFUSED branch: foreign fsid, no trash root, permanent_fallback == false"
        }
    );
    let mut harness = pane_harness("keymap-d6", &dir);
    harness.get_by_label("one.txt").click();
    settle(&mut harness);
    press_command(&mut harness, Command::Delete);
    common::settle_until(&mut harness, "the delete job's summary toast", |h| {
        any_notice(h, "trashed") || any_notice(h, "failed")
    });
    if can_trash {
        assert!(
            !one.exists(),
            "the trash branch moved the file off the listing"
        );
        wait(
            &mut harness,
            PaneSlot::Left,
            "the row to disappear after the trash",
            |h| !names(shown(h, PaneSlot::Left)).contains(&"one.txt".to_string()),
        );
    } else {
        assert!(one.exists(), "the refused branch must not touch the bytes");
        assert!(
            any_notice(&harness, "0 trashed, 0 deleted, 1 failed"),
            "the refusal is said out loud: {:?}",
            notices(&harness)
        );
    }

    // Second lane: the explicit permanent request with the fallback opted in — delete *deletes*,
    // and the tab reloads itself through the `DirChanged` the job emits.
    let origin = harness.state().session.next_origin();
    let mut req = DeleteReq::permanent(vec![abs(&two)]);
    req.permanent_fallback = true;
    let receipt = harness
        .state_mut()
        .session
        .delete(&req, origin)
        .expect("a permanent delete of a live path is accepted");
    assert!(
        matches!(receipt, Receipt::Running { .. }),
        "delete is a job (plan §4.1)"
    );
    common::settle_until(&mut harness, "two.txt to be gone from disk", |_| {
        !two.exists()
    });
    common::settle_until(&mut harness, "two.txt to be gone from the tab", |h| {
        !names(shown(h, PaneSlot::Left)).contains(&"two.txt".to_string())
    });
    assert!(
        any_notice(&harness, "1 deleted"),
        "the summary says what it did"
    );
}

#[test]
fn ctrl_c_then_ctrl_v_copies_into_the_other_pane() {
    let src = tree("keymap-copy-src", &["keep.txt"]);
    let dst = tree("keymap-copy-dst", &[]);
    let mut harness = split_harness("keymap-copy", &src, &dst);
    harness.get_by_label("keep.txt").click();
    settle(&mut harness);
    assert_eq!(shown(&harness, PaneSlot::Left).sel.len(), 1);
    press_command(&mut harness, Command::Copy);
    // Route the paste to the *other* pane — the way a click on it would (plan §4.3's focus).
    harness.state_mut().model.active_pane = PaneSlot::Right;
    press_command(&mut harness, Command::Paste);
    let landed = dst.join("keep.txt");
    common::settle_until(&mut harness, "keep.txt to land in the right pane", |_| {
        landed.is_file()
    });
    assert!(
        src.join("keep.txt").is_file(),
        "a copy leaves the source alone"
    );
    common::settle_until(
        &mut harness,
        "the right pane to reload and show keep.txt",
        |h| names(shown(h, PaneSlot::Right)).contains(&"keep.txt".to_string()),
    );
}

#[test]
fn enter_on_a_directory_navigates() {
    let dir = common::listing_tree("keymap-enter");
    let mut harness = pane_harness("keymap-enter", &dir);
    harness.get_by_label("docs").click();
    settle(&mut harness);
    press_command(&mut harness, Command::Open);
    wait(
        &mut harness,
        PaneSlot::Left,
        "the tab to navigate into docs",
        |h| names(shown(h, PaneSlot::Left)).contains(&"readme.md".to_string()),
    );
}

#[test]
fn ctrl_w_closes_the_shown_tab() {
    let dir = common::listing_tree("keymap-close-tab");
    let session = common::session("keymap-close-tab");
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fixture_path(&dir));
    let first = app
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .expect("the first tab is shown")
        .id;
    app.open_tab(PaneSlot::Left, fixture_path(&dir.join("docs")));
    let mut harness = build_harness(app, egui::vec2(1200.0, 800.0));
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].tabs.len(),
        2,
        "two tabs are open"
    );
    press_command(&mut harness, Command::CloseTab);
    settle(&mut harness);
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(pane.tabs.len(), 1, "Ctrl+W closed the shown tab");
    assert_eq!(
        pane.active_tab().expect("one tab remains").id,
        first,
        "and the first tab is what remains"
    );
}

#[test]
fn f3_and_f4_toggle_the_grid_and_the_quad() {
    let dir = common::listing_tree("keymap-grid");
    let mut harness = pane_harness("keymap-grid", &dir);
    assert_eq!(harness.state().model.panes.len(), 1);
    press_command(&mut harness, Command::SplitLayout);
    assert_eq!(harness.state().model.panes.len(), 2, "F3 opens the split");
    // Go's `updateLayout` seeds a new pane with the directory in view (`split-view.js:68-74`); a
    // pane without a tab has no address bar and takes no sidebar navigation.
    assert_eq!(
        shown_path(harness.state(), PaneSlot::Right),
        Some(fixture_path(&dir)),
        "the new pane opens on the directory the user was looking at"
    );
    assert_eq!(
        harness.state().model.active_pane,
        PaneSlot::Left,
        "and the seeding leaves the focus alone"
    );
    settle(&mut harness);
    assert!(
        !names(shown(&harness, PaneSlot::Right)).is_empty(),
        "the new pane reads its directory — the blank \"no directory\" panel is the bug"
    );
    press_command(&mut harness, Command::SplitLayout);
    assert_eq!(
        harness.state().model.panes.len(),
        1,
        "F3 toggles back (Go's own behaviour)"
    );
    press_command(&mut harness, Command::QuadLayout);
    assert_eq!(harness.state().model.panes.len(), 4, "F4 opens the quad");
    assert!(harness.state().model.quad());
    for slot in [PaneSlot::LeftBottom, PaneSlot::RightBottom] {
        assert_eq!(
            shown_path(harness.state(), slot),
            Some(fixture_path(&dir)),
            "{slot:?} is live too"
        );
    }
    press_command(&mut harness, Command::QuadLayout);
    assert_eq!(harness.state().model.panes.len(), 1, "F4 toggles back too");
}

/// The shown tab's path of one pane.
fn shown_path(app: &ZenApp, slot: PaneSlot) -> Option<FsPath> {
    app.model
        .panes
        .get(&slot)
        .and_then(|pane| pane.active_tab())
        .map(|tab| tab.path.clone())
}

/// <kbd>Shift+Delete</kbd> must destroy nothing *until the confirmation is answered* (plan **D6**), and
/// must ask out loud — a silent chord on the one destructive command is the failure mode this guards.
#[test]
fn shift_del_asks_before_it_destroys_anything() {
    let dir = tree("keymap-shift-del", &["doomed.txt"]);
    let mut harness = pane_harness("keymap-shift-del", &dir);
    harness.get_by_label("doomed.txt").click();
    settle(&mut harness);
    press_command(&mut harness, Command::PermanentDelete);
    settle(&mut harness);
    assert!(
        dir.join("doomed.txt").is_file(),
        "the chord alone never destroys a byte"
    );
    let open = harness.state().dialogs.active();
    assert!(
        matches!(open, Some(zenman_ui::dialogs::Dialog::Confirm(_))),
        "the chord raises the confirmation instead of deleting"
    );
    // Escape is the safe answer, and the safe answer is the *default* one.
    press(&mut harness, Chord::key(egui::Key::Escape));
    settle(&mut harness);
    assert!(
        !harness.state().dialogs.is_open(),
        "Escape closed the confirmation"
    );
    assert!(
        dir.join("doomed.txt").is_file(),
        "a cancelled confirmation leaves the file exactly where it was (D6)"
    );
    assert!(
        harness
            .query_all_by_label("Delete permanently")
            .count()
            .lt(&usize::MAX),
        "the danger row was drawn at least once"
    );
}

#[test]
fn f2_stands_the_rename_editor_up() {
    let dir = tree("keymap-f2", &["alpha.txt", "beta.txt"]);
    let mut harness = pane_harness("keymap-f2", &dir);
    harness.get_by_label("beta.txt").click();
    settle(&mut harness);
    press_command(&mut harness, Command::Rename);
    settle(&mut harness);
    let tab = shown(&harness, PaneSlot::Left);
    let armed = tab.rename_target.clone();
    assert_eq!(
        armed,
        tab.sel.keys().iter().next().cloned(),
        "F2 arms exactly the selected row"
    );
    // 10c: arming *is* opening the editor — in the row, not in a modal (Go used `window.prompt`;
    // plan §7 row 10's criterion is that the port does not).
    let editor = tab
        .rename_editor
        .clone()
        .expect("the armed row grew an in-cell editor");
    assert_eq!(editor.key, armed.unwrap_or_default());
    assert_eq!(
        editor.buf, "beta.txt",
        "the field starts as the current name"
    );
    assert_eq!(
        editor.select, 4,
        "the selection covers the base name, not the extension"
    );
    assert!(editor.taken.contains(&"alpha.txt".to_string()));
    assert!(
        dir.join("beta.txt").is_file() && dir.join("alpha.txt").is_file(),
        "nothing renamed yet"
    );
    assert_eq!(names(tab), vec!["alpha.txt", "beta.txt"]);
}

#[test]
fn a_command_with_no_selection_toasts_rather_than_panicking() {
    let dir = common::listing_tree("keymap-no-sel");
    let mut harness = pane_harness("keymap-no-sel", &dir);
    // `harness.run()` after the chord is the assertion: a panic surfaces as a failed frame.
    press_command(&mut harness, Command::Cut);
    settle(&mut harness);
    let warning = notices(&harness)
        .into_iter()
        .find(|(level, _)| *level == Level::Warning)
        .expect("a command with nothing to act on says so");
    assert!(warning.1.contains("nothing selected"), "{:?}", warning.1);
}
