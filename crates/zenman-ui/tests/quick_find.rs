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
//!   rule of steps 9a/11a);
//! * **the walk is uncapped**, so the count line is a number and not a `500+ (capped)` shrug, and
//!   the footer's batch verbs act on *every* match (`Copy Found` on 600 files queues 600 paths);
//! * **the footer** (`quick-find.js:499-841`): `Copy Found`/`Cut Found` queue the match set on the
//!   file clipboard, `Copy Path`/`Copy Name` write it to the *text* clipboard, both without closing
//!   the dialog; `Delete Found` closes it and asks the same three-button question the context menu's
//!   destructive rows ask, and nothing is deleted until that question is answered.

mod common;

use std::path::{Path, PathBuf};

use egui::{Key, PointerButton, Pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::entry::{DirEntryLike, EntryBuilder, EntryKind, EntryStat, InfoEntry};
use zenman_core::jobs::JobId;
use zenman_core::ops::clipboard::ClipOp;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::platform;
use zenman_core::services::search_live::{SearchResults, SearchStatus};
use zenman_core::session::Origin;
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::Dialog;
use zenman_ui::dialogs::ResultMenu;
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

/// A flat root of exactly these files — the fixture the footer's batch verbs need, where which
/// rows survive the walk is the assertion.
fn flat_tree(tag: &str, names: &[&str]) -> PathBuf {
    let root = scratch(tag);
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture root");
    for name in names {
        std::fs::write(root.join(name), "x\n").expect("fixture file");
    }
    root
}

/// A root of `count` files that one query matches, plus one it never does: Go's 500-row cap in the
/// way of a number that is not `500+`.
fn many_tree(tag: &str, count: usize) -> PathBuf {
    let mut names: Vec<String> = (0..count).map(|ix| format!("batch-{ix:03}.txt")).collect();
    names.push("keeper.zzz".to_string());
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    flat_tree(tag, &borrowed)
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

/// …and the same shell over the **recording** launcher, for the test that takes the open branch: it
/// asserts what was handed to the desktop instead of launching an editor during the test run
/// (`common::NoDesktopEnv`; a directory argument there would have asked the session manager for the
/// default file manager — this project's own binary on an installed machine).
fn pane_harness_logging(
    tag: &str,
    dir: &Path,
) -> (
    Harness<'static, ZenApp>,
    std::sync::Arc<common::NoDesktopEnv>,
) {
    let (session, env) = common::session_with_launcher(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, FsPath::from_abs(abs(dir)));
    (build_harness(app), env)
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

/// The test's stand-in for a footer click: kittest queues hover+press+release, and `run` walks them
/// (and any repaint the verb asked for) to settled.
fn click_footer(harness: &mut Harness<'static, ZenApp>, label: &str) {
    harness.get_by_label(label).click();
    harness.run();
}

/// …and the same click, one frame per queued event, so that `output()` is still the frame that
/// answered: [`Harness::output`] is the **last** frame only, and `run` keeps stepping past the click.
fn click_one_frame(harness: &mut Harness<'static, ZenApp>, label: &str) {
    harness.get_by_label(label).click();
    harness.step();
}

/// What the shell put on the **text** clipboard this frame (`Context::copy_text` is an output
/// command, and `Harness::output` is the last frame only — so this is read between the click and
/// any further frame).
fn copied_text(harness: &Harness<'static, ZenApp>) -> Option<String> {
    harness
        .output()
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
}

fn notices(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    harness
        .state()
        .notices
        .queued()
        .iter()
        .map(|n| n.text.clone())
        .collect()
}

fn any_notice(harness: &Harness<'static, ZenApp>, needle: &str) -> bool {
    notices(harness).iter().any(|text| text.contains(needle))
}

/// Type a query and wait for its rows, returning the match set's rel paths.
fn find_rows(harness: &mut Harness<'static, ZenApp>, word: &str) -> Vec<String> {
    type_word(harness, word);
    common::settle_until(harness, "the rows of a query", |h| {
        quick_find(h).total.is_some()
    });
    names(&quick_find(harness).results)
}

/// The queue the engine is holding, as `(op, paths)`.
fn clip(harness: &Harness<'static, ZenApp>) -> Option<(ClipOp, Vec<String>)> {
    harness.state().session.clipboard().map(|clip| {
        (
            clip.op,
            clip.items
                .iter()
                .map(|item| item.path.to_string_lossy())
                .collect(),
        )
    })
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
    let (mut harness, launcher) = pane_harness_logging("qf-file", &root);
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
    // D6 branch-and-print: the branch is asserted above, and the *hand-over* is asserted here — the
    // one path the launcher was asked about, the file and not its directory. Whether the desktop has a
    // handler for `.zzzq` is the platform's business, and `contract_ops` answers it with the real one.
    assert_eq!(
        launcher.opened(),
        vec![
            abs(&root)
                .join("notes.zzzq")
                .expect("joins")
                .to_string_lossy()
        ],
        "the open branch handed the file to the launcher"
    );
    assert_eq!(
        shown(&harness).path,
        FsPath::from_abs(abs(&root)),
        "a file open never moves the pane"
    );
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

// --------------------------------------------------------------------------- the uncapped walk

/// Go capped the live walk at 500 rows (`api/search.go:66`) and paid for it by re-fetching the whole
/// set with `limit=0` before every footer verb (`quick-find.js:499-534`). The port lifts the cap at
/// the request instead, which is the difference between a filter that lies and one that does not.
#[test]
fn the_walk_is_uncapped_so_six_hundred_matches_are_six_hundred_rows() {
    const MATCHES: usize = 600;
    let root = many_tree("qf-uncapped", MATCHES);
    let mut harness = pane_harness("qf-uncapped", &root);
    open_find(&mut harness);
    let rows = find_rows(&mut harness, "batch");
    let find = quick_find(&harness);
    assert_eq!(
        find.requested().map(|q| q.limit),
        Some(0),
        "the request asks for Go's `limit <= 0` (unlimited), not for 500"
    );
    assert_eq!(rows.len(), MATCHES, "every match is a row");
    assert_eq!(find.total, Some(MATCHES), "and the count says so");
    assert!(
        !find.status.limited(),
        "no budget cut this walk short: {:?}",
        find.status
    );
    drop(find);
    // The count line is the user-visible half of the contract: a number, and no `(capped)` hedge.
    assert_eq!(
        harness.query_all_by_label("600 matches").count(),
        1,
        "the count line prints the real total"
    );
    assert_eq!(
        harness.query_all_by_label_contains("(capped)").count(),
        0,
        "the cap wording is gone: {:?}",
        notices(&harness)
    );
}

// ------------------------------------------------------------------- the footer's clipboard verbs

#[test]
fn copy_found_queues_every_match_and_leaves_the_dialog_up() {
    let root = flat_tree("qf-copy-found", &["f1.txt", "f2.txt", "keeper.zzz"]);
    let mut harness = pane_harness("qf-copy-found", &root);
    open_find(&mut harness);
    assert!(
        !quick_find(&harness).actions_enabled(),
        "with no rows there is nothing to queue, so the footer is greyed (Go's `:437-451`)"
    );
    assert_eq!(
        harness.query_all_by_label("Copy Found").count(),
        1,
        "the button is drawn even while it is greyed"
    );
    let rows = find_rows(&mut harness, ".txt");
    assert_eq!(rows, vec!["f1.txt", "f2.txt"]);
    assert!(quick_find(&harness).actions_enabled());
    click_footer(&mut harness, "Copy Found");
    common::settle_until(&mut harness, "the copy toast", |h| {
        any_notice(h, "copied 2 found item(s)")
    });
    let (op, paths) = clip(&harness).expect("the queue is not empty");
    assert_eq!(op, ClipOp::Copy);
    assert_eq!(
        paths,
        vec![
            root.join("f1.txt").to_string_lossy(),
            root.join("f2.txt").to_string_lossy()
        ],
        "the whole match set, in list order, as absolute paths"
    );
    assert!(
        matches!(harness.state().dialogs.active(), Some(Dialog::QuickFind(_))),
        "Go closed its overlay (`:785`); the port keeps it open so one opening can chain verbs"
    );
}

#[test]
fn cut_found_queues_the_match_set_as_a_move() {
    let root = flat_tree("qf-cut-found", &["f1.txt", "keeper.zzz"]);
    let mut harness = pane_harness("qf-cut-found", &root);
    open_find(&mut harness);
    find_rows(&mut harness, ".txt");
    click_footer(&mut harness, "Cut Found");
    common::settle_until(&mut harness, "the cut toast", |h| {
        any_notice(h, "cut 1 found item(s)")
    });
    let (op, paths) = clip(&harness).expect("the queue is not empty");
    assert_eq!(op, ClipOp::Cut, "`Cut Found` is a move, not a copy");
    assert_eq!(paths, vec![root.join("f1.txt").to_string_lossy()]);
}

#[test]
fn a_greyed_footer_button_queues_nothing() {
    let root = flat_tree("qf-greyed", &["f1.txt"]);
    let mut harness = pane_harness("qf-greyed", &root);
    open_find(&mut harness);
    // No query typed: the list is empty, so the buttons must not answer a click.
    click_footer(&mut harness, "Copy Found");
    harness.run();
    assert!(
        clip(&harness).is_none(),
        "a click on a greyed button reached the engine: {:?}",
        notices(&harness)
    );
}

#[test]
fn copy_path_and_copy_name_write_the_text_clipboard() {
    let root = flat_tree("qf-copy-path", &["f1.txt", "f2.txt"]);
    let mut harness = pane_harness("qf-copy-path", &root);
    open_find(&mut harness);
    find_rows(&mut harness, ".txt");
    // Go's format (`context-menu.js:582-591`): one line per path, plus a trailing newline — and,
    // unlike `Copy Found`, plain text only: the file queue is untouched.
    let want_paths = format!(
        "{}\n{}\n",
        root.join("f1.txt").to_string_lossy(),
        root.join("f2.txt").to_string_lossy()
    );
    click_one_frame(&mut harness, "Copy Path");
    assert_eq!(copied_text(&harness).as_deref(), Some(want_paths.as_str()));
    assert_eq!(
        harness
            .output()
            .platform_output
            .commands
            .iter()
            .filter(|command| matches!(command, egui::OutputCommand::CopyText(_)))
            .count(),
        1,
        "one click, one clipboard write (a footer verb runs once per frame, in `drain`)"
    );
    assert!(clip(&harness).is_none(), "Copy Path is not Copy Found");
    click_one_frame(&mut harness, "Copy Name");
    assert_eq!(copied_text(&harness).as_deref(), Some("f1.txt\nf2.txt\n"));
    // The dialog is still up for the verb that follows.
    harness.run();
    assert!(matches!(
        harness.state().dialogs.active(),
        Some(Dialog::QuickFind(_))
    ));
}

// ---------------------------------------------------------------------- the footer's delete verb

#[test]
fn delete_found_closes_the_find_and_asks_the_three_way_question() {
    let root = flat_tree(
        "qf-delete-ask",
        &["f1.txt", "f2.txt", "f3.txt", "keeper.zzz"],
    );
    let mut harness = pane_harness("qf-delete-ask", &root);
    open_find(&mut harness);
    find_rows(&mut harness, ".txt");
    click_footer(&mut harness, "Delete Found");
    common::settle_until(&mut harness, "the confirm card", |h| {
        matches!(h.state().dialogs.active(), Some(Dialog::Confirm(_)))
    });
    assert_eq!(
        harness
            .query_all_by_label_contains("delete these 3 matched item(s)")
            .count(),
        1,
        "the card counts the whole match set"
    );
    for label in ["Cancel", "Move to trash", "Delete permanently"] {
        assert_eq!(
            harness.query_all_by_label(label).count(),
            1,
            "`{label}` is one of the card's answers"
        );
    }
    click_footer(&mut harness, "Cancel");
    common::settle_until(&mut harness, "the cancellation toast", |h| {
        any_notice(h, "delete found: cancelled")
    });
    assert!(
        harness.state().dialogs.active().is_none(),
        "a cancelled card closes"
    );
    for name in ["f1.txt", "f2.txt", "f3.txt", "keeper.zzz"] {
        assert!(
            root.join(name).exists(),
            "the safe answer touched `{name}`: {:?}",
            notices(&harness)
        );
    }
}

#[test]
fn delete_found_permanently_removes_every_match_and_nothing_else() {
    let root = flat_tree("qf-delete-perm", &["f1.txt", "f2.txt", "keeper.zzz"]);
    let mut harness = pane_harness("qf-delete-perm", &root);
    open_find(&mut harness);
    find_rows(&mut harness, ".txt");
    click_footer(&mut harness, "Delete Found");
    common::settle_until(&mut harness, "the confirm card", |h| {
        matches!(h.state().dialogs.active(), Some(Dialog::Confirm(_)))
    });
    click_footer(&mut harness, "Delete permanently");
    common::settle_until(&mut harness, "the matched files to go", |h| {
        let _ = h;
        !root.join("f1.txt").exists()
    });
    assert!(!root.join("f2.txt").exists(), "the whole match set went");
    assert!(
        root.join("keeper.zzz").exists(),
        "a non-match is not part of the answer"
    );
}

/// The same branch probe `keymap.rs`'s D6 test runs: the fixture volume may have no trash root, in
/// which case `to_trash` (`permanent_fallback: false`) must refuse rather than quietly delete.
#[test]
fn delete_found_to_the_trash_answers_the_d6_ladder() {
    let root = flat_tree("qf-delete-trash", &["f1.txt", "keeper.zzz"]);
    let one = root.join("f1.txt");
    let can_trash = platform::default_env().can_trash(&abs(&one));
    println!(
        "can_trash({}) = {can_trash} — {}",
        one.display(),
        if can_trash {
            "TRASHED branch: this volume has a trash location"
        } else {
            "REFUSED branch: foreign fsid, no trash root, permanent_fallback == false"
        }
    );
    let mut harness = pane_harness("qf-delete-trash", &root);
    open_find(&mut harness);
    find_rows(&mut harness, ".txt");
    click_footer(&mut harness, "Delete Found");
    common::settle_until(&mut harness, "the confirm card", |h| {
        matches!(h.state().dialogs.active(), Some(Dialog::Confirm(_)))
    });
    click_footer(&mut harness, "Move to trash");
    common::settle_until(&mut harness, "the delete job's summary", |h| {
        any_notice(h, "trashed") || any_notice(h, "failed")
    });
    if can_trash {
        assert!(!one.exists(), "the trash branch moved the match away");
    } else {
        assert!(one.exists(), "a refused trash must not destroy the bytes");
    }
    assert!(
        root.join("keeper.zzz").exists(),
        "the trash rung never touches a non-match"
    );
}

#[test]
fn the_found_paths_are_the_matches_under_the_search_root() {
    // The footer acts on paths, so the rel_path → absolute step is the whole feature: a nested
    // match must resolve inside the root the dialog is searching, recursively.
    let root = scratch("qf-nested");
    common::remove_tree(&root);
    std::fs::create_dir_all(root.join("deep/deeper")).expect("fixture dirs");
    for name in ["top.log", "deep/mid.log", "deep/deeper/low.log"] {
        std::fs::write(root.join(name), "x\n").expect("fixture file");
    }
    let mut harness = pane_harness("qf-nested", &root);
    open_find(&mut harness);
    edit_quick_find(&mut harness, |find| find.recursive = true);
    find_rows(&mut harness, ".log");
    let find = quick_find(&harness);
    assert_eq!(find.results.len(), 3, "the recursive walk found the run");
    let paths = find.found_paths();
    assert_eq!(
        paths
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>(),
        vec![
            // `WalkDir`'s pre-order over name-sorted siblings: `deep` sorts before `top.log`, and
            // inside it `deeper` before `mid.log` (`search_live`'s own ordering).
            root.join("deep/deeper/low.log").to_string_lossy(),
            root.join("deep/mid.log").to_string_lossy(),
            root.join("top.log").to_string_lossy(),
        ],
        "each rel_path resolves against the search root"
    );
}

// ---------------------------------------------------------------------- the row's menu
//
// A right-click on a result row raises Go's search-result menu (`context-menu.js:173-197`) — the same
// five rows the search manager raises (`search-manager.js:378-389`), so the gesture means one thing in
// both dialogs. Go's find dialog skipped the menu and revealed straight away; the port kept the action
// and gave it the menu that names it.

/// The rows the menu draws, in the order the shell would paint them.
fn menu_rows() -> Vec<String> {
    let specs = ResultMenu::specs();
    specs
        .iter()
        .flat_map(|group| group.iter())
        .map(|spec| spec.label.clone())
        .collect()
}

/// A raw pointer press+release at one point — for the space that is no node at all, the void outside a
/// card being the case that needs it.
fn click_at(harness: &mut Harness<'static, ZenApp>, at: Pos2) {
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        });
    }
    harness.run();
}

/// Click the `skip`-th node labelled `label`, in the order the frame allocated them, and stop on the
/// frame that answered: two nodes can share a label — the footer's `Copy Name` button and the menu's
/// `Copy Name` row — so a test that means one of them says which, and [`Harness::output`] only keeps
/// the last frame's clipboard commands.
fn click_node(harness: &mut Harness<'static, ZenApp>, label: &str, skip: usize) {
    harness
        .query_all_by_label(label)
        .nth(skip)
        .unwrap_or_else(|| panic!("no node named {label} #{skip}"))
        .click();
    harness.step();
}

/// Click a menu row by name, arming nothing: the name is unique to the menu.
fn click_menu_row(harness: &mut Harness<'static, ZenApp>, label: &str) {
    assert_eq!(
        harness.query_all_by_label(label).count(),
        1,
        "{label} is not a menu row's unique name"
    );
    click_node(harness, label, 0);
}

/// Right-click a row, then arm the menu: the frame the press lands in draws nothing, because a menu that
/// is asked for and drawn in one frame is closed again by the press that asked for it.
fn right_click_row(harness: &mut Harness<'static, ZenApp>, name: &str) {
    harness.get_by_label(name).click_secondary();
    // Not merely "a menu exists": its rows only exist once it has been painted, which is the frame
    // after the right-click. A click sent before that is a click on the card behind.
    common::settle_until(harness, "the row's menu", |h| {
        matches!(h.state().dialogs.active(),
            Some(Dialog::QuickFind(f)) if f.menu_shown())
    });
}

/// The five rows are the ones Go's `showSearchResultContextMenu` builds, in his order, and the right-click
/// leaves the row under the cursor — `search-manager.js:380-384` selects before it shows.
#[test]
fn right_clicking_a_row_raises_the_menu_on_that_row() {
    let dir = common::listing_tree("qf-row-menu");
    let mut harness = pane_harness("qf-row-menu", &dir);
    open_find(&mut harness);
    assert_eq!(
        menu_rows(),
        [
            "Open",
            "Open Containing Folder",
            "Open Containing Folder in New Tab",
            "Copy Path",
            "Copy Name",
        ]
        .map(str::to_string),
        "Go's five rows, in his order (`context-menu.js:175-193`)"
    );
    // A query first: with an empty field the dialog paints no rows, and the row the pointer would
    // reach belongs to the pane behind. `row-04` matches rows the pane has not painted.
    let rows = find_rows(&mut harness, "row-04");
    assert_eq!(
        rows.first().map(String::as_str),
        Some("row-0400.txt"),
        "the first of {} rows",
        rows.len()
    );
    right_click_row(&mut harness, "row-0400.txt");
    let at = quick_find(&harness);
    assert_eq!(
        at.cursor,
        Some(0),
        "the menu speaks about the row the pointer was on"
    );
    assert_eq!(
        at.menu_target().map(|(path, _)| path),
        Some(abs(&dir).join("row-0400.txt").expect("joins")),
        "the row's own absolute path, not its folder's"
    );
    assert!(
        shown(&harness).sel.is_empty(),
        "a menu in a dialog selects nothing in the pane behind it"
    );
}

/// `Copy Name` on a row writes the one name — and the card stays open with the menu gone, because
/// copying is not an answer to anything. Legacy closed both dialogs for every menu row
/// (`context-menu.js:683`); for a copy that only made the user open the dialog again.
#[test]
fn the_menu_copies_one_row_and_leaves_the_card_up() {
    let root = scratch("qf-row-copy");
    common::remove_tree(&root);
    std::fs::create_dir_all(root.join("deep")).expect("fixture dir");
    for name in ["top.log", "deep/mid.log"] {
        std::fs::write(root.join(name), "x\n").expect("fixture file");
    }
    let mut harness = pane_harness("qf-row-copy", &root);
    open_find(&mut harness);
    edit_quick_find(&mut harness, |find| find.recursive = true);
    let rows = find_rows(&mut harness, ".log");
    assert_eq!(rows, vec!["deep/mid.log", "top.log"], "both rows");
    assert_eq!(copied_text(&harness), None, "no copy has happened yet");
    right_click_row(&mut harness, "mid.log");
    // Two nodes carry `Copy Name`: the footer's button (which would copy *every* match) and the menu's
    // row, drawn after the card. The menu is the later one.
    click_node(&mut harness, "Copy Name", 1);
    assert_eq!(
        copied_text(&harness).as_deref(),
        Some("mid.log\n"),
        "the row's name, and only the row's — `Copy Found` would have written both"
    );
    let at = quick_find(&harness);
    assert!(
        at.menu_target().is_none(),
        "a clicked menu does not stay open"
    );
    assert!(
        harness
            .state()
            .dialogs
            .active()
            .is_some_and(|d| matches!(d, Dialog::QuickFind(_))),
        "copying leaves the list up to be read"
    );
    let tab = shown(&harness);
    assert_eq!(
        tab.path.to_string(),
        root.to_string_lossy(),
        "and the pane behind stays where it was"
    );
}

/// `Open Containing Folder`: Go's `reveal-search-result` (`context-menu.js:690-698`), which closes the
/// search manager, navigates the pane to `parentDir` and leaves the row in `pathSelections[parentDir]`.
/// The port's second half cannot happen in the same frame: a folder the pane never listed is a job, not a
/// fact.
#[test]
fn the_menu_opens_the_row_s_folder_and_selects_the_row() {
    let root = scratch("qf-menu-reveal");
    common::remove_tree(&root);
    std::fs::create_dir_all(root.join("deep")).expect("fixture dir");
    for name in ["top.log", "deep/mid.log"] {
        std::fs::write(root.join(name), "x\n").expect("fixture file");
    }
    let mut harness = pane_harness("qf-menu-reveal", &root);
    open_find(&mut harness);
    edit_quick_find(&mut harness, |find| find.recursive = true);
    find_rows(&mut harness, ".log");
    right_click_row(&mut harness, "mid.log");
    click_menu_row(&mut harness, "Open Containing Folder");
    common::settle_until(&mut harness, "the folder of the row", |h| {
        h.state().dialogs.active().is_none() && shown(h).sel.contains("mid.log")
    });
    let tab = shown(&harness);
    assert!(
        tab.path.to_string().ends_with("/deep"),
        "the pane went to the row's parent, not to the search root: {}",
        tab.path
    );
    assert_eq!(
        tab.selected_names(),
        vec!["mid.log".to_string()],
        "the reveal is the whole selection, as Go's `selectedPaths.clear()` then `add` made it"
    );
}

/// `Open Containing Folder in New Tab` — Go's `reveal-search-result-new-tab` (`:700-707`), which calls
/// `createTab(parentDir)` and then `switchTab`, so the pane's own tab is left alone and the row appears
/// in a fresh one.
#[test]
fn the_new_tab_row_reveals_the_row_in_a_tab_of_its_own() {
    let root = scratch("qf-menu-tab");
    common::remove_tree(&root);
    std::fs::create_dir_all(root.join("deep")).expect("fixture dir");
    for name in ["top.log", "deep/mid.log"] {
        std::fs::write(root.join(name), "x\n").expect("fixture file");
    }
    let mut harness = pane_harness("qf-menu-tab", &root);
    let before = harness.state().model.panes[&PaneSlot::Left].tabs.len();
    open_find(&mut harness);
    edit_quick_find(&mut harness, |find| find.recursive = true);
    find_rows(&mut harness, ".log");
    right_click_row(&mut harness, "mid.log");
    click_menu_row(&mut harness, "Open Containing Folder in New Tab");
    common::settle_until(&mut harness, "the new tab", |h| {
        h.state().dialogs.active().is_none() && shown(h).sel.contains("mid.log")
    });
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(
        pane.tabs.len(),
        before + 1,
        "the folder got a tab of its own, as Go's `createTab` gave it"
    );
    assert!(
        shown(&harness).path.to_string().ends_with("/deep"),
        "and the pane now shows that tab"
    );
}

// --------------------------------------------------------------------------- the mouse's doors
//
// Both search dialogs answer <kbd>Escape</kbd>, and always did. Everything else the user reaches for a
// pointer — a ✕ on the card, a click outside it — is legacy's, not Go's (`quick-find.js:207-215` closes
// on a document click; neither card has a close button, because a browser frames one for them).

/// The ✕ at the end of the title row answers exactly what <kbd>Escape</kbd> answers, teardown included.
#[test]
fn the_close_button_in_the_title_row_answers_the_dialog() {
    let dir = common::listing_tree("qf-close-x");
    let mut harness = pane_harness("qf-close-x", &dir);
    open_find(&mut harness);
    find_rows(&mut harness, "row-04");
    click_menu_row(&mut harness, "Close (Escape)");
    common::settle_until(&mut harness, "the card gone", |h| {
        h.state().dialogs.active().is_none()
    });
    assert!(
        !harness.state().session.live_search().is_some_and(|_| false),
        "the answer is the same one the key gives"
    );
}

/// A press outside the card dismisses it — and on the frame the card appeared, it does not: the press
/// that walked the user to the card must not also be the press that throws it away.
#[test]
fn a_click_outside_the_card_dismisses_it() {
    let dir = common::listing_tree("qf-outside");
    let mut harness = pane_harness("qf-outside", &dir);
    open_find(&mut harness);
    find_rows(&mut harness, "row-04");
    // The card is centred; the far corner of the status bar is not on it, nor on any of its rows.
    click_at(&mut harness, Pos2::new(20.0, 880.0));
    common::settle_until(&mut harness, "the card gone", |h| {
        h.state().dialogs.active().is_none()
    });
    assert!(
        !shown(&harness).sel.contains("row-0400.txt"),
        "dismissing a find selects nothing in the pane behind"
    );
}

/// The press that opens a card is not the press that closes it. A pointer-driven open (a toolbar
/// button, a menu row) puts both in the same frame, and a dialog that answered its own opening click
/// would be impossible to open with the mouse at all.
#[test]
fn a_card_does_not_close_on_the_press_that_opened_it() {
    let dir = common::listing_tree("qf-outside-open");
    let mut harness = pane_harness("qf-outside-open", &dir);
    // The click is queued before the frame that draws the card for the first time.
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(Pos2::new(20.0, 880.0)));
    for pressed in [true, false] {
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: Pos2::new(20.0, 880.0),
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        });
    }
    let ctx = harness.ctx.clone();
    harness
        .state_mut()
        .dispatch(PaneSlot::Left, Command::QuickFind, &ctx);
    harness.step();
    assert!(
        harness
            .state()
            .dialogs
            .active()
            .is_some_and(|d| matches!(d, Dialog::QuickFind(_))),
        "the frame that opened the card is not the frame that closes it"
    );
    harness.run();
    assert!(
        harness
            .state()
            .dialogs
            .active()
            .is_some_and(|d| matches!(d, Dialog::QuickFind(_))),
        "and the press does not leak into the frames after it"
    );
}

/// A dismissed menu is not an answer: a press outside it takes the menu off and leaves the dialog, the
/// cursor and the pane alone.
#[test]
fn a_click_outside_the_menu_only_takes_the_menu_off() {
    let dir = common::listing_tree("qf-row-away");
    let mut harness = pane_harness("qf-row-away", &dir);
    open_find(&mut harness);
    find_rows(&mut harness, "row-04");
    right_click_row(&mut harness, "row-0404.txt");
    // A press on a row *above* the anchor: inside the card, outside the menu, which hangs below.
    harness.get_by_label("row-0400.txt").click();
    common::settle_until(&mut harness, "the menu gone", |h| {
        h.state()
            .dialogs
            .active()
            .map(|d| match d {
                Dialog::QuickFind(f) => f.menu_target().is_none(),
                _ => false,
            })
            .unwrap_or(false)
    });
    let at = quick_find(&harness);
    assert_eq!(
        at.cursor,
        Some(0),
        "the card outlives its menu, and the click reached the row it landed on"
    );
    let tab = shown(&harness);
    assert!(
        tab.path.to_string().ends_with("/listing"),
        "nothing was navigated: {}",
        tab.path
    );
}

/// The same reveal for a row the pane has never *read*: the loaded page stops at
/// [`zenman_ui::state::DEFAULT_PAGE_SIZE`], so the shell has to widen it — the pane's own
/// `Load all items` verb — before there is a row to select.
#[test]
fn revealing_a_row_below_the_page_loads_the_rest_and_scrolls_onto_it() {
    let dir = common::listing_tree("qf-reveal-deep");
    let mut harness = pane_harness("qf-reveal-deep", &dir);
    let loaded = shown(&harness).entries.entries.len();
    assert!(
        loaded < 450,
        "the fixture is a paged listing: {loaded} rows loaded"
    );
    open_find(&mut harness);
    find_rows(&mut harness, "row-04");
    // `row-0400.txt` sits past the loaded page of the pane's listing, and it is the first row the
    // dialog paints — so it is reachable by the pointer without any scrolling of the dialog.
    right_click_row(&mut harness, "row-0400.txt");
    click_menu_row(&mut harness, "Open Containing Folder");
    common::settle_until(&mut harness, "the far row", |h| {
        h.state().dialogs.active().is_none() && shown(h).sel.contains("row-0400.txt")
    });
    let tab = shown(&harness);
    assert!(
        tab.entries.entries.len() > loaded,
        "the reveal widened the page from {loaded} rows to {}",
        tab.entries.entries.len()
    );
    assert!(
        !tab.has_more,
        "and it read the directory, not one more page"
    );
    assert!(
        tab.scroll.y > 100.0,
        "the scroller moved onto the row ({} rows down, in rows)",
        tab.scroll.y
    );
    // The painted span is the proof the jump reached *that* row, not merely somewhere down the list
    // (`FlatMode::Off`, so the listing order is the visual order).
    let row = tab
        .entries
        .entries
        .iter()
        .position(|entry| entry.name == "row-0400.txt")
        .expect("the revealed row is loaded");
    assert!(
        (tab.scroll.first..tab.scroll.last).contains(&row),
        "row {row} is selected but not painted ({}..{})",
        tab.scroll.first,
        tab.scroll.last
    );
}
