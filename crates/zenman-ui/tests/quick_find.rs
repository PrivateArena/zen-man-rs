//! The step-11b quick-find dialog, driven through the shell's real seams: a live `Session`, a
//! real directory, real `Event::Text`/`Event::Key`s, and the real throttle.
//!
//! What this file owns (plan §7 row 11's UI half):
//!
//! * **`Ctrl+F` opens the dialog** over the active pane's directory (the step-10a toast is gone);
//! * **≤1 walk per 250 ms of typing** — asserted by the dialog's *admission counters* (a
//!   `Running` is a walk, a `Deferred` is not), never by timing;
//! * **an invalid regex surfaces inline** and starts nothing (`CoreError::BadRegex` is not
//!   `is_expected` — no toast, no job);
//! * rows carry Go's `rel_path`, the arrows move the cursor the way `quick-find.js:329-343` does,
//!   <kbd>Enter</kbd> on a directory navigates and on a file takes the open path (the **D6**
//!   branch — where the platform decides the outcome, the test asserts the branch and prints);
//! * <kbd>Escape</kbd> closes, cancels and resets the lane so the *next* opening walks at once;
//! * a `SearchReady` whose origin is not the dialog's current one is dropped (the stale-answer
//!   rule of steps 9a/11a).

mod common;

use std::path::{Path, PathBuf};

use egui::Key;
use egui_kittest::Harness;

use zenman_core::entry::{DirEntryLike, EntryBuilder, EntryKind, EntryStat, InfoEntry};
use zenman_core::jobs::JobId;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::search_live::{SearchResults, SearchStatus};
use zenman_core::session::Origin;
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::Dialog;
use zenman_ui::dialogs::quick_find::{QuickFind, deliver};
use zenman_ui::keymap::{Chord, Command};
use zenman_ui::state::{Tab, panes_for};

// --------------------------------------------------------------------- harness plumbing

/// A fixture tree with a directory, nested content, and a file whose open *must* fail loudly:
///
/// ```text
/// <scratch>/zenman-ui-fixture-<tag>/
///   docs/          readme.md
///   alpha.txt
///   beta.txt
///   notes.zzzq
/// ```
fn tree(tag: &str) -> PathBuf {
    let root = scratch(tag);
    common::remove_tree(&root); // the trees persist between runs (plan §9.2 step 9)
    std::fs::create_dir_all(root.join("docs")).expect("fixture directory");
    for name in ["docs/readme.md", "alpha.txt", "beta.txt", "notes.zzzq"] {
        std::fs::write(root.join(name), format!("{name} original\n")).expect("fixture file");
    }
    root
}

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn build_harness(app: ZenApp) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        // Dialogs, jobs and the coalesced `DirChanged` window stretch the frame loop (plan §9.2).
        .with_max_steps(64)
        // A seeded `TextEdit` never surfaces an a11y name in this fork (plan §9.2 step 10 trap).
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
    for _ in 0..6 {
        harness.run();
    }
    harness
}

/// A single-pane shell listing `dir`.
fn pane_harness(tag: &str, dir: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, FsPath::from_abs(abs(dir)));
    build_harness(app)
}

// --------------------------------------------------------------------- driving the dialog

/// `Ctrl+F` — the step-10a chord (the dispatch seam is the same one the keyboard reaches).
fn press_ctrl_f(harness: &mut Harness<'static, ZenApp>) {
    let chord = Chord::ctrl(Key::F);
    harness.input_mut().events.push(chord.event());
    harness.run();
    harness.run();
}

/// The dispatch seam the chord routes through, used where a test only needs the command.
fn open_find(harness: &mut Harness<'static, ZenApp>) {
    let ctx = harness.ctx.clone();
    harness
        .state_mut()
        .dispatch(PaneSlot::Left, Command::QuickFind, &ctx);
    harness.run();
    harness.run();
}

/// One real keystroke into the focused field (the field took focus on the open frame).
fn type_char(harness: &mut Harness<'static, ZenApp>, c: char) {
    harness
        .input_mut()
        .events
        .push(egui::Event::Text(c.to_string()));
    harness.run();
}

fn type_word(harness: &mut Harness<'static, ZenApp>, word: &str) {
    for c in word.chars() {
        type_char(harness, c);
    }
}

/// One real key press (arrows / <kbd>Enter</kbd> / <kbd>Escape</kbd>).
fn press_key(harness: &mut Harness<'static, ZenApp>, key: Key) {
    harness.input_mut().events.push(egui::Event::Key {
        modifiers: Default::default(),
        physical_key: None,
        key,
        pressed: true,
        repeat: false,
    });
    harness.run();
}

fn quick_find(harness: &Harness<'static, ZenApp>) -> QuickFind {
    match harness.state().dialogs.active() {
        Some(Dialog::QuickFind(find)) => find.clone(),
        other => panic!("expected the quick-find dialog, found {other:?}"),
    }
}

/// The test's stand-in for clicking a toggle (the *state* is the toggle; `edit_prompt` in the
/// step-10c suite is the same convention).
fn edit_quick_find(harness: &mut Harness<'static, ZenApp>, edit: impl FnOnce(&mut QuickFind)) {
    match harness.state_mut().dialogs.active_mut() {
        Some(Dialog::QuickFind(find)) => edit(find),
        other => panic!("expected the quick-find dialog, found {other:?}"),
    }
}

fn shown<'h>(harness: &'h Harness<'static, ZenApp>) -> &'h Tab {
    harness
        .state()
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .expect("the pane has a shown tab")
}

fn names(hits: &[zenman_core::services::search_live::SearchHit]) -> Vec<String> {
    hits.iter().map(|h| h.rel_path.clone()).collect()
}

/// A hand-built row for the stale-answer test (the only test-side search construction; the
/// engine's own answers are never synthesized).
fn stale_hit(root: &AbsPath, name: &str) -> zenman_core::services::search_live::SearchHit {
    let path = root.join(name).expect("fixture name joins");
    let stat = EntryStat::in_listing(
        FsPath::from_abs(path),
        FsPath::from_abs(root.clone()),
        DirEntryLike::new(name, EntryKind::File),
        Some(InfoEntry::new(
            name,
            EntryKind::File,
            3,
            Some(1_700_000_000),
            Some(0o100_644),
        )),
    );
    zenman_core::services::search_live::SearchHit {
        entry: EntryBuilder::from_stat(&stat)
            .depth(0)
            .files_count(None)
            .build(),
        rel_path: name.to_string(),
    }
}

// ------------------------------------------------------------------------------- the tests

#[test]
fn the_chord_opens_quick_find_over_the_active_pane_s_directory() {
    let root = tree("qf-open");
    let mut harness = pane_harness("qf-open", &root);
    press_ctrl_f(&mut harness);
    let find = quick_find(&harness);
    assert_eq!(
        find.root,
        abs(&root),
        "Go scopes the search to `getActiveTab().currentPath` (`quick-find.js:26`)"
    );
    assert!(find.query.is_empty() && find.results.is_empty());
    assert!(
        !find.fresh,
        "the open frame takes the focus (Go's `input.focus()`)"
    );
    assert_eq!(
        harness.state().session.live_search(),
        None,
        "opening walks nothing — the empty field is not a query (Go's `:218-230`)"
    );
}

#[test]
fn a_three_keystroke_burst_admits_exactly_one_walk_and_the_newest_query_wins() {
    let root = tree("qf-burst");
    let mut harness = pane_harness("qf-burst", &root);
    open_find(&mut harness);
    // The window starts at the burst, so `started == 1 / deferred == 2` is an admission fact and
    // never a race against the ~250 ms a kittest frame takes (they are milliseconds apart).
    harness.state().session.reset_search_throttle();
    type_word(&mut harness, "alp"); // one frame per keystroke
    let find = quick_find(&harness);
    assert_eq!(find.query, "alp");
    // The criterion, counted in *admissions*, not in seconds: 3 dispatches inside one 250 ms
    // window produced exactly one walk and two honest deferrals.
    assert_eq!(
        find.started, 1,
        "≤1 walk per 250 ms of typing (plan §7 row 11) — the throttle is core's, the keystrokes are ours"
    );
    assert_eq!(
        find.deferred, 2,
        "every keystroke still called `search_live`; the refusals only kept the spinner up"
    );
    assert!(
        find.busy,
        "a deferral keeps the busy indicator (the `after` is the wait)"
    );
    // The newest keystroke wins: core re-issues the deferred request when the window closes.
    common::settle_until(&mut harness, "the newest query's rows", |h| {
        quick_find(h).total.is_some()
    });
    let find = quick_find(&harness);
    assert_eq!(
        find.requested().map(|q| q.query.as_str()),
        Some("alp"),
        "the request on the wire is the *full* word, not a prefix"
    );
    let rows = names(&find.results);
    assert!(
        rows.iter().any(|r| r == "alpha.txt"),
        "the deferred walk ran: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r == "beta.txt"),
        "the prefix 'a' never repainted the dialog: {rows:?}"
    );
    assert_eq!(find.total, Some(find.results.len()));
}

#[test]
fn an_invalid_regex_is_refused_inline_and_starts_no_walk() {
    let root = tree("qf-regex");
    let mut harness = pane_harness("qf-regex", &root);
    open_find(&mut harness);
    edit_quick_find(&mut harness, |f| f.regex = true);
    type_char(&mut harness, '(');
    common::settle_until(&mut harness, "the inline refusal", |h| {
        quick_find(h).error.is_some()
    });
    let find = quick_find(&harness);
    let error = find.error.clone().unwrap_or_default();
    assert!(
        error.starts_with("Invalid regex"),
        "Go's own words, inline in the field's error colour: {error}"
    );
    assert_eq!(
        find.started, 0,
        "a pattern that does not compile never reaches the job table"
    );
    assert_eq!(harness.state().session.live_search(), None);
    assert_eq!(harness.state().session.pending_search(), None);
    assert!(
        find.results.is_empty() && find.total.is_none(),
        "no rows, no `0 matches` lie"
    );
    // Recovering the pattern resumes searching — the refusal is per-keystroke, not per-opening.
    // The field still holds the `(` that was refused, so recovery starts by emptying it — a
    // substring `(alp` would legitimately match nothing and test nothing.
    edit_quick_find(&mut harness, |f| {
        f.regex = false;
        f.query.clear();
    });
    type_word(&mut harness, "alp");
    common::settle_until(&mut harness, "the substring fallback", |h| {
        quick_find(h).total.is_some()
    });
    assert!(
        quick_find(&harness)
            .results
            .iter()
            .any(|h| h.name() == "alpha.txt")
    );
}

#[test]
fn rows_carry_rel_paths_and_the_arrows_wrap_like_go() {
    let root = tree("qf-rel");
    let mut harness = pane_harness("qf-rel", &root);
    open_find(&mut harness);
    edit_quick_find(&mut harness, |f| f.recursive = true);
    type_word(&mut harness, "readme");
    common::settle_until(&mut harness, "the nested row", |h| {
        quick_find(h).total.is_some()
    });
    let find = quick_find(&harness);
    let rows = names(&find.results);
    assert_eq!(
        rows,
        vec!["docs/readme.md".to_string()],
        "Go's `rel_path` rows"
    );
    assert_eq!(
        find.cursor,
        Some(0),
        "Go selects the first row when rows arrive"
    );
    drop(find);
    // One row: Down and Up both wrap onto it (`(i ± 1) % len`, `quick-find.js:329-343`).
    press_key(&mut harness, Key::ArrowDown);
    assert_eq!(quick_find(&harness).cursor, Some(0));
    press_key(&mut harness, Key::ArrowUp);
    assert_eq!(quick_find(&harness).cursor, Some(0));
    // Emptying the field walks nothing and shows nothing (`quick-find.js:218-230`).
    edit_quick_find(&mut harness, |f| f.query.clear());
    common::settle_until(&mut harness, "the cleared list", |h| {
        quick_find(h).results.is_empty()
    });
    let find = quick_find(&harness);
    assert!(find.total.is_none() && find.cursor.is_none());
    assert_eq!(
        find.requested().map(|q| q.query.clone()),
        Some(String::new())
    );
}

#[test]
fn enter_on_a_directory_hit_navigates_the_pane_and_closes_the_dialog() {
    let root = tree("qf-dir");
    let mut harness = pane_harness("qf-dir", &root);
    open_find(&mut harness);
    type_word(&mut harness, "docs");
    common::settle_until(&mut harness, "the directory row", |h| {
        quick_find(h).total.is_some()
    });
    let find = quick_find(&harness);
    assert!(find.results[0].is_dir(), "`docs` is the directory row");
    let (target, is_dir) = find.activate_target().expect("the cursor is on a row");
    assert_eq!(target, abs(&root).join("docs").expect("joins"));
    assert!(
        is_dir,
        "directories take the *navigate* branch (Go's `executeItemAction` open)"
    );
    drop(find);
    press_key(&mut harness, Key::Enter);
    common::settle_until(&mut harness, "the navigation", |h| {
        !h.state().dialogs.is_open()
    });
    assert_eq!(
        shown(&harness).path,
        FsPath::from_abs(abs(&root).join("docs").expect("joins")),
        "Go: `navigateToCallback(itemPath, …)` for a directory row"
    );
}

#[test]
fn enter_on_a_file_hit_takes_the_open_path_and_leaves_the_pane_alone() {
    let root = tree("qf-file");
    let mut harness = pane_harness("qf-file", &root);
    open_find(&mut harness);
    type_word(&mut harness, "notes");
    common::settle_until(&mut harness, "the file row", |h| {
        quick_find(h).total.is_some()
    });
    let find = quick_find(&harness);
    let (target, is_dir) = find.activate_target().expect("the cursor is on a row");
    assert_eq!(target, abs(&root).join("notes.zzzq").expect("joins"));
    assert!(
        !is_dir,
        "files take the *desktop open* branch, never a navigation"
    );
    drop(find);
    press_key(&mut harness, Key::Enter);
    common::settle_until(&mut harness, "the dismissal", |h| {
        !h.state().dialogs.is_open()
    });
    // D6 branch-and-print: the branch is asserted above; the *outcome* of the desktop open is
    // platform's (no handler for `.zzzq` here, so the report's `failed` toasts) — print it.
    assert_eq!(
        shown(&harness).path,
        FsPath::from_abs(abs(&root)),
        "a file open never moves the pane"
    );
    let notices: Vec<String> = harness
        .state()
        .notices
        .queued()
        .iter()
        .map(|n| n.text.clone())
        .collect();
    println!("open-path notices: {notices:?}");
}

#[test]
fn escape_closes_and_the_next_open_walks_without_waiting_out_the_window() {
    let root = tree("qf-esc");
    let mut harness = pane_harness("qf-esc", &root);
    open_find(&mut harness);
    type_word(&mut harness, "al");
    common::settle_until(&mut harness, "the admitted walk", |h| {
        // The *admission* is the fact; `job` is transient (a walk over four files finishes inside
        // one settle frame), so waiting on it would be a race rather than an assertion.
        quick_find(h).started == 1
    });
    let first = harness.state().session.live_search();
    assert!(first.is_some(), "the first keystroke runs");
    press_key(&mut harness, Key::Escape);
    common::settle_until(&mut harness, "the close", |h| !h.state().dialogs.is_open());
    assert_eq!(
        harness.state().session.pending_search(),
        None,
        "Escape leaves no deferred request behind to walk after the dialog is gone"
    );
    // Teardown honesty: the *next* opening is not throttled by the previous session's typing.
    press_ctrl_f(&mut harness);
    type_char(&mut harness, 'b');
    common::settle_until(&mut harness, "the immediate walk of the new opening", |h| {
        quick_find(h).started == 1
    });
    let find = quick_find(&harness);
    assert_eq!(
        find.started, 1,
        "the reset window admits the new query at once"
    );
    assert_eq!(find.deferred, 0, "the old session's window was forgotten");
    assert_ne!(
        harness.state().session.live_search(),
        first.or(find.job),
        "a new job id walks"
    );
    // The pane was never touched by any of this.
    assert_eq!(shown(&harness).path, FsPath::from_abs(abs(&root)));
}

#[test]
fn a_stale_answer_is_dropped_and_the_current_origin_is_displayed() {
    let root = tree("qf-stale");
    let mut harness = pane_harness("qf-stale", &root);
    open_find(&mut harness);
    type_word(&mut harness, "alp");
    common::settle_until(&mut harness, "the real rows", |h| {
        quick_find(h).total.is_some()
    });
    let find = quick_find(&harness);
    let origin = find.origin.expect("the dispatch carried its origin");
    let query = find.requested().cloned().expect("a request was made");
    let real = names(&find.results);
    drop(find);
    let stale = std::sync::Arc::new(SearchResults {
        query,
        hits: vec![stale_hit(&abs(&root), "STALE-ROW")],
        status: SearchStatus {
            total_matched: 1,
            ..SearchStatus::default()
        },
    });
    // An answer for any *other* origin is a keystroke this dialog has already overwritten.
    deliver(
        harness.state_mut(),
        Origin(origin.0 + 7),
        JobId(9_999),
        std::sync::Arc::clone(&stale),
    );
    harness.run();
    assert_eq!(
        names(&quick_find(&harness).results),
        real,
        "the stale-answer rule: an origin that is not the dialog's current one paints nothing"
    );
    // The dialog's own origin paints — the check filters staleness, not answers.
    let job = quick_find(&harness).job.unwrap_or(JobId(1));
    deliver(harness.state_mut(), origin, job, stale);
    harness.run();
    let rows = names(&quick_find(&harness).results);
    assert_eq!(
        rows,
        vec!["STALE-ROW".to_string()],
        "the current origin is displayed"
    );
}
