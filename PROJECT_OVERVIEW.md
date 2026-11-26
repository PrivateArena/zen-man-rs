<!-- codegraph-file-count: 73, last-commit: 84b9074d690d9956d22f923d0898d9af73eb2948 -->

# zen-man-rs v0.1.0 — Rust 2024 + egui (rev-pinned fork) workspace

## Purpose

Native Rust port of the `zen-man` file manager (originally Go + browser UI). There is no web server and no REST layer: the domain logic is a headless library, the UI is an immediate-mode egui shell, and the binary is a thin bootstrap. The primary output is a single release binary `zenman` (~31 MB) that opens a dockable multi-pane, multi-tab file manager, and that reuses an existing Go data directory because the config JSON, the `ZMLG` undo log and the SQLite index are format-compatible. Language stack: Rust (edition 2024, MSRV 1.95) with two Python fixture scripts and shell build/golden harnesses.

## Scope Map

| Scope | Files | Overview File | One-line purpose |
|---|---|---|---|
| crates/zenman-core | 68 | PROJECT_OVERVIEW.crates-zenman-core.md | Headless engine: dirs, ops, archives, index, undo log, jobs, platform port |
| root (this file) | 73 | (this file) | egui shell (`zenman-ui`), binary bootstrap (`apps/zenman`), CI gates (`xtask`), fixtures/goldens |

## Architecture

```
argv / file:// URI  ->  apps/zenman (Cli, single_instance, gpu_startup, config_boot)
  -> eframe + wgpu frame loop
    -> zenman_ui::app::ZenApp  (dock, tabs, panes, dialogs, keymap, theme)
      -> zenman_core::session::Session  (facade -> services / ops / index / history / platform)
    -> craft-ui widgets + egui (SVG icons from assets/icons)
```

## File Tree

```
zen-man-rs/
  Cargo.toml                    workspace table + [patch.crates-io] egui/winit revs, lints, profiles
  .cargo/config.toml            `cargo xtask` alias
  apps/zenman/src/              main app app_dirs app_icon config_boot gpu_startup single_instance crash_guard i18n
                                  (+ desktop_entry, linux-only `--install-desktop-entry`)
assets/app-icon/                generated application icon set (master PNG, runtime PNG, .ico, .icns, hicolor ladder)
  apps/zenman/                  + build.rs (i18n catalog), i18n/en.json, tests/gpu_fallback.rs
  crates/zenman-ui/src/         app state dock dock_content keymap theme persist jobs_ui drag_drop icons
  crates/zenman-ui/src/panels/  file_pane side_panel breadcrumb nav_bar header tab_bar status_bar action_log
  crates/zenman-ui/src/dialogs/ search_manager quick_find batch_rename action_editor extract properties confirm prompt
  crates/zenman-ui/src/menus/   mod.rs (declarative menu specs)
  crates/zenman-ui/src/widgets/ virtual_list entry_row toast disk_meter
  crates/zenman-ui/tests/       19 kittest suites + common/mod.rs harness
  crates/zenman-core/           -> PROJECT_OVERVIEW.crates-zenman-core.md
  xtask/src/                    main parity contracts source_guards assets png fixture_tree cross_target
  scripts/                      build.sh capture-golden.sh make-fixtures.py normalise-golden.py
  tests/                        fixtures/ (tree, archives)  golden/ (134 Go-parity goldens)
  docs/ assets/                 contracts.md tab-groups-design.md parity/ windows-platform.md crash-reports.md | icons/ (SVG set)
```

## Tools

| Tool / Binary | File | Purpose |
|---|---|---|
| `zenman` (binary) | apps/zenman/src/main.rs | The file manager; `--smoke`, `--safe-gpu`, `--gl-only`, `--disable-gpu`, `--no-daemon`, path/URI targets |
| `cargo xtask ci` | xtask/src/main.rs | Aggregate gate runner (fmt/clippy/test/licences/parity/snapshots/artefact gates) |
| `cargo xtask parity` | xtask/src/parity.rs | Reports which Go reference files/screens are ported, from `docs/parity` baselines |
| `cargo xtask contracts` / `golden-index` | xtask/src/contracts.rs | Validates engine output against `tests/golden` (incl. `action.log` walk + CRC32) |
| `cargo xtask scratch` | xtask/src/main.rs (`scratch_guard`) + `crates/zenman-core/src/scratch.rs` | Reaps the test scratch trees left in the temp dir by a run whose process is gone, and fails while a reapable one survives (a `tmpfs` `/tmp` makes a leak cost RAM); runs twice inside `cargo xtask ci` |
| `cargo xtask deps` / `licenses` | xtask/src/main.rs | Reads `cargo metadata`: rev-pinning, licence allowlist, `zenman-core` UI-free boundary |
| `cargo xtask snapshots` | xtask/src/main.rs + crates/zenman-ui/tests/snapshots.rs | Rasterised kittest/wgpu snapshot rendering |
| `cargo xtask fixture-tree` | xtask/src/fixture_tree.rs | Regenerates `tests/fixtures` and its manifest via scripts/make-fixtures.py |
| `cargo xtask assets` | xtask/src/assets.rs | Checks `assets/icons` files against the attribution/licence table |
| `cargo xtask app-icon` | xtask/src/app_icon.rs + scripts/make-app-icon.py | Checks the application-icon set: valid containers, right sizes, embedded paths, and pixels re-derived from the prototype |
| `scripts/build.sh` | scripts/build.sh | Preflight + `--locked` release build, `--check`, `--smoke`, `--install`, `--dist` |
| `scripts/capture-golden.sh` | scripts/capture-golden.sh | Captures Go reference output for the golden set |

## Component Roles

| File / Module | Role | LOC | Key Exports |
|---|---|---|---|
| Cargo.toml | Workspace root: members, rev-pinned `[patch.crates-io]` egui/winit table, workspace lints (`forbid(unsafe_code)`), profiles | ~135 | `[workspace.dependencies]`, `[workspace.lints]`, `[profile.*]` |
| apps/zenman/src/main.rs | CLI parse, path overrides, single-instance hand-off, GPU bless/read, engine boot, eframe launch, exit codes | ~678 | `struct Cli` (clap), `Cli::targets(home) -> Vec<FsPath>`, `run_with_context(&Cli, &AppPaths) -> Result<u8, BootError>`, `uri_scheme(raw) -> Option<String>` |
| apps/zenman/src/app.rs | `eframe::App` adapter: owns `ZenApp`, restores the saved session when no folder was named, polls the instance socket, flushes both stores on `save`, wraps frame logic in the panic guard | ~280 | `pub struct ShellApp`, `ShellApp::new(paths, boot, smoke_after, session, notes, targets, instance) -> Self`, `logic/ui(&mut self, ctx, frame)`, `save` → `flush_state()` |
| apps/zenman/src/app_dirs.rs | XDG/app dir resolution with CLI+env overrides, **portable mode** (`portable.dat`/`zenman.portable` beside the binary → `<exe>/data`), plus the Go legacy config location | ~690 | `AppPaths::resolve(&Overrides) -> Result<Self, AppPathsError>`, `AppPaths::resolve_in(&Overrides, exe_dir)`, `enum Layout::{Override, Portable, Platform}`, `PORTABLE_MARKERS`, `PORTABLE_DATA_DIR`, `exe_dir()`, `gpu_dir()`, `run_dir()`, `zenman_go_legacy_config()` |
| apps/zenman/src/config_boot.rs | Boot the config: load, repair-or-preserve corrupt file, legacy migration, startup notes | ~483 | `boot(&AppPaths) -> Result<BootContext, BootError>`, `migrate_from(&LegacySource, apply) -> Result<String, BootError>`, `report_lines() -> Vec<String>` |
| apps/zenman/src/gpu_startup.rs | GPU backend ladder (hardware -> legacy GL -> software), blessed-profile file, `egui_wgpu` setup hook | ~514 | `fallback_ladder() -> Vec<GpuProfile>`, `select_ladder(safe_gpu, gl_only, disable_gpu)`, `WgpuSetup::apply(&mut egui_wgpu::WgpuSetup)` |
| apps/zenman/src/single_instance.rs | Unix socket/PID claim-or-focus so a second launch forwards its targets to the first window | ~718 | `enum Claim`, `SingleInstanceGuard`, `take_requests() -> Vec<Request>`, `socket_path()` |
| apps/zenman/src/crash_guard.rs | Panic hook writing timestamped reports + per-frame guard turning panics into reported errors | ~154 | `install_hook(dir)`, `guard<T, E>(what, f) -> Result<Result<T, E>, String>`, `report_dir()` |
| apps/zenman/src/i18n.rs | Reader for the build-script-generated catalog; English passthrough when feature off | ~73 | `embedded_locales() -> Vec<&'static str>`, `embedded_locale_count() -> usize`, `translations_available()` |
| apps/zenman/build.rs | Compiles `i18n/*.json` into a Rust source catalog (`CRAFT_I18N_DIR`), and the Windows `RT_GROUP_ICON` from `assets/app-icon/zenman.ico` (`winresource`; warn-not-fail) | ~360 | `collect(dir) -> Result<Vec<LocaleCatalog>, String>`, `parse_json_catalog(path)`, `render(rows) -> String`, `windows_icon(target)` |
| apps/zenman/src/app_icon.rs | The application icon: the embedded 256px PNG, decoded once into `egui::IconData` for the window/taskbar/Dock, plus the hicolor ladder the Linux installer writes | ~75 | `window_icon() -> Option<IconData>`, `HICOLOR: [(u32, &[u8]); 5]`, `HICOLOR_FILE` |
| apps/zenman/src/desktop_entry.rs | Linux only (`--install-desktop-entry`): writes `$XDG_DATA_HOME/applications/<app_id>.desktop` + the hicolor PNGs; never touches a file it did not generate | ~340 | `install(app_id, title, home) -> Result<Report, InstallError>`, `render(app_id, title, exe) -> String` |
| scripts/make-app-icon.py | Derives the whole `assets/app-icon/` set from the prototype artwork (matte → crop → square canvas → ladder), and checks the committed set | ~380 | `build_master`, `matte_from_field`, `generate`, `check`, `--check`/`--preview` |
| scripts/make-app-bundle.sh | macOS only: builds `ZenMan.app` (Info.plist + `zenman.icns`) around the built binary and re-registers it with Launch Services | ~100 | shell, no exports |
| apps/zenman/tests/gpu_fallback.rs | Drives the compiled binary (`CARGO_BIN_EXE_zenman`) through `--safe-gpu --smoke` | ~205 | no exports |
| apps/zenman/tests/session_boot.rs | Drives the same binary twice over one settings tree: the session that was saved is the session that reloads, and a folder argument is not reported as a restore | ~415 | no exports |
| crates/zenman-ui/src/lib.rs | UI crate module surface; re-exports `craft_ui` so the binary needs no direct egui-widget dep | ~68 | `pub mod app..widgets`, `pub use craft_ui` |
| crates/zenman-ui/src/app.rs | `ZenApp`: the whole shell state machine — tabs per pane, command dispatch, dialogs, workspaces, events, the frame's clipboard projection (the dimmed rows and the bar's queue count), and the two tab-group overlays (the hover flyout's timer state machine, the strip drag) | ~3463 | `struct ZenApp`, `ZenApp::new(session, boot_lines)` / `with_persist(..., persist)`, `run_command(slot, PaneCommand)`, `open_tab(slot, FsPath) -> TabId`, `list_at(slot, ix, append, all)`, `selection_fs(slot) -> Vec<FsPath>` (the queue/drag currency; `selection_abs` is for host-only verbs), `drop_paste(&DragPayload, slot, dest, mode)`, `run_paste_with(slot, dest, mode, &[FsPath], policy)`, `clip_status() -> Option<ClipStatus>`, `run_group_command(slot, GroupTarget, GroupVerb)`, `close_tabs(slot, &[TabId])`, `tick_group_hover`/`show_group_hover`, `cut_clip() -> Option<Arc<HashSet<AbsPath>>>`, `struct TabDrag`, `struct TabGroupHover` |
| crates/zenman-ui/src/state.rs | UI-side model: tabs, selection, per-pane view/flat mode, paging state, clipboard of rows — **and the tab-group algebra**, which is the only way a group changes (`docs/tab-groups-design.md` §2, its four invariants and `debug_assert_invariants`) | ~1100 | `struct TabId`, `Selection`, `enum FlatMode` (`to_core() -> core FlatMode`), `struct Tab`, `Tab::{row_key,row_abs,row_fs}` (`row_fs` keeps an archive member, `row_abs` answers its container), `struct GroupId`, `struct TabGroup`, `enum GroupVerb`/`GroupTarget`/`DropTarget`/`TabSelKind`, `Pane::{create_group, join_group, leave_group, ungroup, rename_group, toggle_collapsed, set_group_color, close_tabs, members, move_tabs, select_tab, right_click_selection, compact, prune_groups}` |
| crates/zenman-ui/src/dock.rs | Docking tree built on craft-ui: 2/3/4-pane layouts, leaf/split builders | ~381 | `type Dock = Workspace<DockId>`, `layout_for_slots(&[PaneSlot]) -> Layout<DockId>`, `default_workspace(mode)`, `quad_workspace()` |
| crates/zenman-ui/src/dock_content.rs | Renders one pane inside the dock: drop targets, modal frame styling, window key forwarding | ~434 | `show(app: &mut ZenApp, ui: &mut Ui)`, `struct PaneDock<'a>`, `forward_window_keys(ctx, &DockOutput<DockId>)`, `enum DropIntent` |
| crates/zenman-ui/src/keymap.rs | Command enum + chord model, parse/display/match, default table | ~817 | `enum Command` (~60 variants, `label()`), `struct Chord` (`ctrl/alt/ctrl_shift/...`, `parse(text)`, `matches(key, modifiers)`) |
| crates/zenman-ui/src/theme.rs | Design token set + egui stylesheet; Go palette parity incl. group colours | ~832 | `struct Tokens`, `Tokens::dark()` / `light()`, `apply(ctx, ThemeMode)`, `palette_from(&Tokens) -> craft_ui::theme::WidgetPalette` |
| crates/zenman-ui/src/persist.rs | Shell persistence over craft-settings: dock layout + snapshot, throttled autosave | ~259 | `open(location) -> (Persisted, LoadReport)`, `Persisted::restore(&mut ZenApp, registered) -> ApplyReport`, `save_dock(&Dock)`, `autosave(&ShellSnapshot, now) -> bool` |
| crates/zenman-ui/src/jobs_ui.rs | Routes engine job events to toasts/progress slots and decides repaint cadence | ~500 | `JobsUi::track(info, pane, tab)`, `route(&mut JobsUi, &mut AppModel, EngineEvent) -> Routed`, `tick(app, ctx)`, `keep_polling(app, ctx) -> bool` |
| crates/zenman-ui/src/drag_drop.rs | Internal drag payload, mode from modifiers, ghost/target painting | ~152 | `enum DragMode` (`paste_mode()`), `struct DragPayload` (`paths: Vec<FsPath>` — a grab out of an archive carries the member), `grab_mode(clip)`, `drop_mode(grab, modifiers)`, `paint_ghost(...)` |
| crates/zenman-ui/src/icons.rs | SVG icon registry loaded from `assets/icons` via `craft_ui::icons::SvgIconSet` | ~336 | `set() -> &'static SvgIconSet`, `image(name, size, tint)`, `paint(ui, rect, name, size, tint)`, `icon_for(&FileEntry) -> &'static str` |
| crates/zenman-ui/src/panels/file_pane.rs | The file list: visual row model (headers/rows), selection, keyboard nav, inline commands, and the row's **cut-clipboard** membership (`PaneModel::cut`, Go's `.cut-state`) | ~1282 | `enum PaneCommand` (~40 variants), `struct RowOrder` (`build(&Tab)`, `selected_items(&Selection, limit)`), `enum VisualRow`, `row_state(Option<&AbsPath>, bool, Option<&HashSet<AbsPath>>) -> SelectionState`, `show(...)` |
| crates/zenman-ui/src/panels/side_panel.rs | Left/right drawers: places, favourites, disks, terminal/log sections, drawer commands | ~789 | `show_left(app, ui)`, `show_right(app, ui)`, `enum SidePanelCommand`, `struct RowSpec` |
| crates/zenman-ui/src/panels/breadcrumb.rs | The address strip inside the navigation bar: width-aware crumb planning for host and archive paths | ~590 | `show(ui, &mut BreadcrumbModel, &Tokens, &mut Vec<PaneCommand>)`, `trail(&FsPath) -> Vec<Crumb>` |
| crates/zenman-ui/src/panels/header.rs | The workspace control of a pane's tab row: one Edge-style popup button (list + `New` + `Edit`/`Delete` on its right edge), the autosave state machine and its `Saved` clock (the pill is drawn by the status bar, so the row reserves only the name's width) — and the shell's whole session vocabulary (save, restore, boot reload, exit flush) | ~880 | `struct WorkspaceBar`, `WorkspaceBar::show(ui, &Tokens, disk, room) -> Vec<HeaderCommand>`, `button_width(ui, &Tokens, label)`, `enum HeaderCommand` (workspace verbs + the ribbon's four toggles), `take_due(now)`, `ZenApp::workspace_session/apply_workspace(&Workspace) -> Applied/restore_workspace/save_workspace_as/rename_workspace` (both carry the per-pane tab groups: labels, colours, folds and `groupId`s adopted from the file), `restore_last_session() -> SessionRestore`, `flush_session()` |
| crates/zenman-ui/src/panels/nav_bar.rs | A pane's navigation bar: the address strip plus the icon ribbon (view, refresh, recursive-listing dropdown, select all/none, action history, the two drawers, the two layout switches), folded behind one overflow button when the pane is narrow | ~430 | `show(ui, &mut NavBarModel, &Tokens, &mut Vec<PaneCommand>, &mut Vec<HeaderCommand>)`, `struct NavBarModel`/`NavBarHints`, `icon_button(ui, &Tokens, icon, tooltip)`, `ribbon_width(&Tokens, count)` |
| crates/zenman-ui/src/panels/tab_bar.rs | Tab strip, laid out as **items** (a tab, or a group's chip): the chip folds and unfolds its run, a fold never hides the tab on screen, the strip reads the modifier keys into a selection, and a drag reports where it let go. `layout` is a function of its own because a chip must never be evicted by `fit` and its `minimum_width` must never shrink one | ~700 | `show(ui, id, strip, &mut TabBarModel, &Tokens, &mut Vec<TabCommand>)`, `enum TabCommand`, `items(&[Tab], &[TabGroup], active) -> Vec<StripItem>`, `layout(…) -> StripLayout` (rects + `drop_target`), `struct TabBarModel` (`groups`, `selected`, `dragging`, out: `layout`, `hovered`) |
| crates/zenman-ui/src/panels/status_bar.rs | Selection/size/offset summary line, the `Saved` write-report pill in its right corner, and Go's second indicator — the amber underlined `N items in action` of the queued cut/copy, either indicator hovering open a titled item list | ~585 | `show(ui, &StatusModel, &Tokens)`, `enum StatusInfo`, `struct StatusItem`, `struct ClipStatus` (`total` + a `DISPLAY_CAP`-capped head), `StatusModel::{selected, items}`, `StatusModel::saved`, `SAVED_LABEL`, `selection_tooltip(&StatusModel)` |
| crates/zenman-ui/src/panels/action_log.rs | Undo-log drawer with query throttle and expandable records | ~449 | `struct LogPanel`, `LogPanel::apply(LogCommand) -> Option<LogId>`, `tick(app)`, `show(app, ui)`, `enum LogCommand` |
| crates/zenman-ui/src/menus/mod.rs | Declarative context/menu specs (listing / folder / **tab** / **tab-group** / **search-result** targets), per-target enablement, chord hints | ~1560 | `enum MenuTarget` (`Tab{id, group, count}`, `TabGroup{id, collapsed}`, `Result`), `struct MenuContext` (`bits()`, `in_group`, `tab_count`), `struct MenuSpec` (`command()`, `enabled(&MenuContext)`, `chord(&KeyMap)`), `enum MenuAction` (`Command`/`Custom`/`Group(GroupVerb)`/`Result(ResultVerb)`), `tab_menu(tab_count)`, `group_menu(collapsed)`, `search_menu()` |
| crates/zenman-ui/src/widgets/virtual_list.rs | Virtualized row scrolling: metrics, hit testing, load-more, selection kinds | ~591 | `struct RowMetrics`, `ScrollState`, `enum RowEvent`, `SelKind::from_modifiers(modifiers)`, `struct RowHit<'a>` |
| crates/zenman-ui/src/widgets/entry_row.rs | One row's visual state machine (selected/cut/hovered, size cells, colours) — every colour the row paints is a `SelectionState` answer, so a queued row dims wholly | ~730 | `enum SelectionState` (`of(selected, cut)`, `fill(hovered)`, `border/text/icon/muted(tokens)`), `size_cell(&FileEntry, Option<FileSizes>)`, `UNKNOWN_SIZE` |
| crates/zenman-ui/src/widgets/toast.rs | Notice queue with expiry, dedupe-by-substring, level colours | ~189 | `State::post(level, text)`, `post_at(now, ...)`, `expire(now) -> usize`, `dismiss_containing(needle)`, `show(ctx, &Tokens)` |
| crates/zenman-ui/src/widgets/disk_meter.rs | Free/used bar and byte formatting | ~142 | `show(ui, &Tokens, label, used, total) -> Response`, `format_size(bytes)`, `used_fraction(used, total)` |
| crates/zenman-ui/src/dialogs/mod.rs | Modal stack: open/close, answer routing, one `DialogKind` per dialog, the card's ✕ | ~390 | `enum DialogKind`, `enum Answer` (`choice()`, `accepted()`), `Dialogs::open(Dialog) -> bool`, `is_open()`, `close_button(ui, &Tokens) -> bool` |
| crates/zenman-ui/src/dialogs/result_menu.rs | The context menu of one search-result row, drawn by whichever dialog owns the row (both the find dialog and the Search Manager) | ~150 | `struct ResultMenu` (`new(row, at)`, `row()`, `rect()`, `show(&KeyMap, &Tokens, &Context) -> MenuShot`), `enum MenuShot::{Open, Picked(ResultVerb), Dismissed}`, `pointer_at(&Ui, &Response) -> Pos2`, `dismiss_by_click(&Ui, card, menu, opening) -> bool` |
| crates/zenman-ui/src/dialogs/search_manager.rs | Search Manager: query pills, scope/hidden policy, live vs index, rebuild controls, the result row's menu | ~1520 | `scan_pills(query) -> Vec<Pill>`, `enum ManagerRequest`, `enum IndexOutcome` (`Activate`/`Reveal`/`RevealInNewTab`/`Closed`, `from_verb`), `struct SearchManager` (`hit_target`, `open_menu`, `menu_target`, `draw_menu`), `drain(app, ctx)` |
| crates/zenman-ui/src/dialogs/quick_find.rs | Live quick-find overlay, uncapped walk, footer batch verbs: Copy/Cut Found, Copy Path/Name, Delete Found; right-click reveal | ~960 | `QuickFind::new(slot, root)`, `show(ui, &Tokens) -> Option<Answer>`, `tick`/`deliver`, `drain(app, ctx)`, `enum QuickRequest`, `enum QuickOutcome::{Activate, Closed, DeleteFound, Reveal}`, `found_paths()`, `actions_enabled()`, `row_cells(rect, &Tokens)` |
| crates/zenman-ui/src/dialogs/batch_rename.rs | Live preview table over the core batch-rename engine | ~496 | `BatchRename::new(slot, rows, rules, existing)`, `recompute()`, `table() -> Vec<PreviewCell>`, `enum BatchOutcome` |
| crates/zenman-ui/src/dialogs/action_editor.rs | Edit custom external actions with live match count | ~411 | `ActionEditor::new(slot, draft: CustomAction, ctx: ActionContext)`, `show(...) -> Option<Answer>`, `matching_rows()`, `open_action_editor(slot, Option<CustomAction>)` |
| crates/zenman-ui/src/dialogs/extract.rs | Extraction progress windows driven by core `ExtractJob` | ~348 | `start(app, slot, path, sources, policy)`, `tick(app)`, `cancel(app, id)`, `show(app, ui, &Tokens)`, `struct ExtractProgress` |
| crates/zenman-ui/src/dialogs/properties.rs | Properties for host entries and archive members, with warm size | ~423 | `PropertiesDialog::local(slot, abs, entry)`, `archive(slot, path, entry, props)`, `warm_size(&mut ZenApp)`, `show(...)` |
| crates/zenman-ui/src/dialogs/confirm.rs | Generic confirm/choice dialog incl. conflict "apply to all" | ~209 | `Confirm::new(kind, title, question, detail, buttons)`, `enum Choice`, `ConfirmButton::danger(label, choice)`, `cancel_choice()` |
| crates/zenman-ui/src/dialogs/prompt.rs | Single-line text prompt with validation | ~233 | `Prompt::new(kind, title, label, initial, ok_label, Validation)`, `rejection() -> Option<String>`, `open_prompt(Prompt)` |
| crates/zenman-ui/tests/common/mod.rs | Shared kittest harness: temp dirs, fixture panes, scripted input, snapshot helpers | ~489 | `build_harness` / `pane_harness` helpers (per-suite) |
| crates/zenman-ui/tests/{shell,file_pane,clipboard,side_panel,menus,tab_groups,quick_find,search_manager,action_editor,action_log,archives,batch_rename,dialogs,drag_drop,keymap,workspaces,parity_defects}.rs | Behavioural kittest suites over `ZenApp` — the shell's regression net (`tab_groups.rs` is the feature walk of `docs/tab-groups-design.md` §1: chip, fold, flyout, menus, drag, selection, restart) | ~160–1090 each | no exports |
| crates/zenman-ui/tests/snapshots.rs | Rasterised widget-tree/PNG snapshots (wgpu adapter probe + kittest renderer) | ~1405 | no exports |
| crates/zenman-ui/tests/perf_frame.rs | Frame-time guard for large directories | ~267 | no exports |
| xtask/src/main.rs | Gate dispatcher and composite `ci` task; snapshot artefact gate | ~1106 | `main()`, `run() -> Result<(), Box<dyn Error>>`, `ci()`, `check_snapshot_artefacts(root)`, `harness_snapshot_names(root)` |
| xtask/src/parity.rs | Go-reference porting ledger: scan + report vs `docs/parity` baselines | ~658 | `scan(root, baselines) -> Result<Report, _>`, `reference_map(root) -> Vec<MapRow>`, `run(root, baselines)`, `Report::summary()` |
| xtask/src/contracts.rs | Golden-file contract runner incl. `action.log` binary walk + CRC32 check | ~329 | `contracts()`, `golden_index(golden)`, `walk_log(raw)`, `crc32_ieee(data)` |
| xtask/src/source_guards.rs | AST-lite line scanner: bans direct `std::fs` in op code, counts logical lines, test-item lines | ~626 | `code_lines(text) -> Vec<String>`, `test_item_lines(text) -> BTreeSet<usize>`, `mentions_std_fs(line) -> bool` |
| xtask/src/assets.rs | Icon/asset attribution table cross-check | ~333 | `assets()`, `check(root) -> Result<Report, _>`, `table_entries(text)`, `claimed_paths(text)` |
| xtask/src/png.rs | Minimal PNG header/CRC reader for snapshot dimension checks | ~216 | `inspect(bytes) -> Result<PngInfo, String>`, `check_dimensions(path, bytes, want)` |
| xtask/src/fixture_tree.rs | Regenerates and diff-checks `tests/fixtures` via the Python generator | ~285 | `fixture_tree()`, `manifest_entries(path) -> (usize, Tree)`, `read_tree(root)` |
| xtask/src/cross_target.rs | Cross-target compile check for the Windows path | ~136 | `check() -> Result<(), Box<dyn Error>>` |
| scripts/build.sh | Preflight toolchain, `fetch --locked`, release build, smoke/install/dist modes | ~330 | shell, no exports |
| scripts/capture-golden.sh | Captures Go-application reference output for `tests/golden` | ~1000+ | shell, no exports |
| scripts/make-fixtures.py | Builds `tests/fixtures` (tree + archives) deterministically | ~336 | python, no exports |
| scripts/normalise-golden.py | Normalises captured golden JSON before comparison | ~201 | python, no exports |
| docs/contracts.md + docs/parity/ | Wire/format contracts (SQLite schema, `action.log`, JSON) and the Go reference screenshots | — | not code |

## Cross-References

| File | Called by / calls | Why it's central |
|---|---|---|
| crates/zenman-ui/src/app.rs | Called by `apps/zenman/src/app.rs` (`new`, `open_folder`) and by ~14 `crates/zenman-ui/tests/*` harnesses; calls `panels::*`, `dialogs::*`, `dock_content::show`, `jobs_ui`, `state` | Holds `ZenApp` — the only place UI state and engine commands meet |
| crates/zenman-ui/src/panels/file_pane.rs | Feeds `ZenApp::run_command`; consumed by `dock_content.rs`, `state.rs`, tests (`file_pane.rs`, `parity_defects.rs`) | `PaneCommand` is the shell's command alphabet |
| crates/zenman-ui/src/state.rs | Used by every panel/dialog and by `apps/zenman/src/main.rs` targets path; maps UI view modes to core enums | Tab/selection model shared across all panes |
| apps/zenman/src/main.rs | Calls `app_dirs::AppPaths::resolve`, `config_boot::boot`, `gpu_startup::select_ladder`, `single_instance`, `zenman_core::session::Session::open`, then `eframe` | The only boot path; wires config, GPU and instance hand-off before any UI exists |
| crates/zenman-ui/src/dock.rs + dock_content.rs | `dock::layout_for_slots` used by `persist.rs` restore and `app.rs`; `dock_content::show` drives `ZenApp` | Turns config `LayoutMode`/`PaneSlot` into real pane topology |
| xtask/src/main.rs | Calls `parity::run`, `contracts::golden_index`, `source_guards`, `assets::check`, `cross_target::check`, and reads UI snapshot tests | Single gate surface that every change must satisfy |

## Data Flow

```
launch : argv/URI -> Cli::targets -> single_instance claim|forward -> Session::open -> eframe
         (no targets) -> workspaces.default -> apply_workspace: panes, dock, tabs, active, group, view
frame  : eframe::App::logic -> crash_guard::guard( ZenApp::run_command -> Session::* )
                        <- Session::poll_at(now) : Vec<EngineEvent>  (jobs, DirChanged, notices)
jobs   : Session::spawn_job -> worker thread -> Progress/CancelToken -> JobEvent -> EngineEvent
undo   : ops::* -> history::codec::encode -> action.log (ZMLG)   ;  revert(record) -> ops::*
browse : Session::page_dir -> DirService(FsSource)  |  archive/vfs ManifestCache for archive stores
search : SearchManager -> Session::search_live (budget+throttle) | Session::index_search (FTS5)
persist: craft-settings (dock/tabs) + store::config (ZenConfig: workspaces.default = the session, incl. PaneSession.groups)
         + rusqlite (index); autosave 1.5 s, flushed by eframe::App::save on a clean exit
```

## Key Architectural Patterns

1. **Thin binary, fat crates**: `apps/zenman` only boots and adapts — `ShellApp` is an `eframe::App` wrapper whose whole job is `poll_instance`, the panic guard and the smoke timer; all behaviour lives in `zenman-ui`/`zenman-core`.
2. **Command enums, not callbacks**: panes emit `PaneCommand` / `TabCommand` / `SidePanelCommand` / `HeaderCommand` / `LogCommand` vectors and `ZenApp::run_command` dispatches them, so widgets stay pure and kittest can assert on emitted commands.
3. **Event-poll frame loop**: no subscriptions; `Session::poll_at` drains a queue per frame and `jobs_ui::keep_polling` decides the next repaint, which is what makes the UI a pure function of engine state.
4. **UI->core value mapping**: `zenman_ui::state::FlatMode::to_core()`, `theme::apply(ctx, ThemeMode)` and `persist` keep UI-side mirrors that convert to core enums instead of leaking core types into widgets.
5. **Rev-pinned dependency control**: `[patch.crates-io]` pins the whole egui family + winit to forked revisions declared once in `[workspace.dependencies]`, and `cargo xtask deps` fails mixed-rev graphs — no version drift is possible.
6. **Graceful degradation ladder**: `gpu_startup::select_ladder` (hardware -> legacy GL -> software, with a blessed-profile file) plus `crash_guard::guard` on every logic pass implement the project's never-crash rule.
7. **Gate-as-code**: `xtask` gates are Rust (`source_guards.rs` scanning for `std::fs`, `png.rs` verifying snapshot dimensions, `parity.rs` reporting the porting ledger), so `cargo xtask ci` is the single acceptance surface.
8. **Golden/fixture determinism**: `scripts/make-fixtures.py` + `xtask fixture-tree` + `normalise-golden.py` produce the 134 files in `tests/golden` that `xtask contracts` compares against.

## Read Triggers

| If you need to... | Open these files |
|---|---|
| Add a command (keyboard chord + context-menu entry) | `crates/zenman-ui/src/keymap.rs` (`enum Command`, `Chord`), `crates/zenman-ui/src/menus/mod.rs` (`MenuSpec`/`MenuContext`), `crates/zenman-ui/src/app.rs` (`run_command`, `OpenMenu`), `tests/keymap.rs` + `tests/menus.rs` |
| Add or change a pane command (listing, nav, view mode) | `crates/zenman-ui/src/panels/file_pane.rs` (`enum PaneCommand`), `crates/zenman-ui/src/app.rs` (`run_command`, `list_at`), `crates/zenman-ui/src/state.rs` |
| Reveal a row in its folder, or scroll the pane onto one | `ZenApp::reveal_in_parent` / `reveal_in_new_tab` / `apply_reveal` (the pending-`Reveal` promise in `app.rs`), the `Reveal` / `RevealInNewTab` outcomes of `QuickOutcome` / `IndexOutcome`, `ScrollState::jump_to_row` read by `show_list`/`show_grid` (`widgets/virtual_list.rs`), `Selection::select_key` (`state.rs`), `tests/quick_find.rs` + `tests/file_pane.rs` |
| Change a dialog's context menu, or how a card is dismissed | `dialogs/result_menu.rs` (`ResultMenu`, `dismiss_by_click`), `menus::search_menu` / `ResultVerb` (`menus/mod.rs`), the `drain` of `dialogs/quick_find.rs` + `dialogs/search_manager.rs` (which draw it and answer its rows), `dialogs::close_button` (`dialogs/mod.rs`), `tests/{quick_find,search_manager,menus}.rs` |
| Add a dialog | `crates/zenman-ui/src/dialogs/mod.rs` (`DialogKind`/`Answer`/`Dialogs::open`), a sibling such as `dialogs/prompt.rs` as template, `crates/zenman-ui/src/app.rs`, `tests/dialogs.rs` |
| Change pane/dock layout or restart persistence | `crates/zenman-ui/src/dock.rs`, `dock_content.rs`, `crates/zenman-ui/src/persist.rs`, `crates/zenman-core/src/store/config.rs` (`LayoutMode`, `PaneSlot`) |
| Change colours, fonts or the Go palette parity | `crates/zenman-ui/src/theme.rs` (`Tokens`), `crates/zenman-ui/tests/snapshots.rs`, `docs/parity/` |
| Add an icon or fix icon mapping | `crates/zenman-ui/src/icons.rs` (`icon_for`), `assets/icons/`, `xtask/src/assets.rs`, `ATTRIBUTION.md` |
| Change the **application** icon (window/taskbar/Dock/Explorer), its sizes, or the Linux/Wayland entry | `docs/app-icon.md`, `scripts/make-app-icon.py`, `assets/app-icon/`, `apps/zenman/src/app_icon.rs`, `apps/zenman/src/desktop_entry.rs`, `apps/zenman/build.rs`, `xtask/src/app_icon.rs` |
| Change CLI flags, startup, GPU fallback or single-instance hand-off | `apps/zenman/src/main.rs`, `apps/zenman/src/gpu_startup.rs`, `single_instance.rs`, `config_boot.rs`, `app_dirs.rs` (portable layout), `apps/zenman/tests/gpu_fallback.rs`, `apps/zenman/tests/session_boot.rs` |
| Add or change a tab group (chip, fold, colour, menu verb, drag) | `crates/zenman-ui/src/state.rs` (the algebra + invariants), `crates/zenman-ui/src/panels/tab_bar.rs` (`items`/`layout`/chips), `crates/zenman-ui/src/app.rs` (`run_group_command`, `end_tab_drag`, `tick_group_hover`), `crates/zenman-ui/src/menus/mod.rs` (`tab_menu`/`group_menu`), `docs/tab-groups-design.md`, `tests/tab_groups.rs` + `tests/menus.rs` |
| Change what a queued copy/cut **looks** like (dimmed rows, the bar's `N items in action`, its hover list) | `crates/zenman-ui/src/app.rs` (`ClipView`, `refresh_clip`, `cut_clip`, `clip_status`), `crates/zenman-ui/src/panels/file_pane.rs` (`row_state`, `PaneModel::cut`), `crates/zenman-ui/src/widgets/entry_row.rs` (`SelectionState`), `crates/zenman-ui/src/panels/status_bar.rs` (`StatusItem`/`ClipStatus`/`items_popup`), `crates/zenman-core/src/ops/clipboard.rs` (`ClipItem`, whose `path` is a `FsPath` so a member's own name and size reach the chip), `crates/zenman-ui/tests/clipboard.rs` |
| Copy or paste across the archive boundary (queue a member, extract it out, refuse a drop in) | `crates/zenman-ui/src/state.rs` (`Tab::row_fs`), `crates/zenman-ui/src/app.rs` (`selection_fs`, `clipboard_store`, `run_paste_with`, `begin_drag`, `drop_into_row`/`drop_into_pane`, `READ_ONLY_REFUSED`), `crates/zenman-core/src/ops/{clipboard,paste}.rs`, `crates/zenman-core/src/session.rs` (`paste_members`), `docs/contracts.md` ledger #15, `crates/zenman-ui/tests/{archives,drag_drop,clipboard}.rs` |
| Change what the session saves or restores | `crates/zenman-ui/src/panels/header.rs` (`workspace_session`, `apply_workspace`, `restore_last_session`, `flush_session`), `crates/zenman-core/src/services/workspaces.rs` (`Autosave`, `RESERVED`), `crates/zenman-core/src/store/config.rs` (`Workspace`/`TabSession`), `crates/zenman-ui/tests/workspaces.rs`, `docs/contracts.md` → "Session save and restore" |
| Change where the app's files live | `apps/zenman/src/app_dirs.rs` (`Layout`, `PORTABLE_MARKERS`, `resolve_in`), `docs/contracts.md` → "Where the files live", `apps/zenman/src/config_boot.rs` |
| Add or change a CI gate | `xtask/src/main.rs` (`run` dispatch + `ci`), the relevant gate module (`source_guards.rs`, `parity.rs`, `contracts.rs`), `.cargo/config.toml` alias |
| Add a test that needs a directory on disk | `crates/zenman-core/src/scratch.rs` (`create` for a plain path, `Scratch` for one that must be deleted on unwind — never a hand-rolled `env::temp_dir().join(..)`), then the nearest existing helper for the naming (`src/index/walker.rs` → `zenman-walker-<case>`) |
| Regenerate fixtures, goldens or snapshots | `scripts/make-fixtures.py`, `xtask/src/fixture_tree.rs`, `xtask/src/contracts.rs`, `crates/zenman-ui/tests/snapshots.rs`, `docs/fixtures.md` |
| Add an engine feature and surface it | `crates/zenman-core/src/session.rs` (facade method), `crates/zenman-ui/src/app.rs` (command), see `PROJECT_OVERVIEW.crates-zenman-core.md` |

## Dependencies

### Framework / UI (rev-pinned, root `Cargo.toml`)
| Package / Module | Role | Version |
|---|---|---|
| egui, eframe, egui_extras, egui_kittest, ecolor, emath, epaint | Immediate-mode GUI + widget harness; all patched to the `storytold/egui` fork | 0.37 @ rev `6d518fc` |
| winit | Windowing (patched fork) | rev `a517b38` |
| craft-ui, craft-settings, craft-i18n, craft-atomic | Docking/widgets, settings store, i18n runtime | `storytold/craft-libs` rev `6e03f65` |
| wgpu, pollster, image | Snapshot renderer + PNG artefacts (dev-only for UI tests) | 30 / 1 / 0.25 |

### Application layer (`apps/zenman/Cargo.toml`)
| clap, directories, log | CLI, XDG dirs, logging | 4 / 6 / 0.4 |
| rustix | `FD_CLOEXEC` on the instance socket (unix target) | 1.1.6 |
| serde_json | build-script catalog parsing | 1 |

### Engine (`crates/zenman-core/Cargo.toml`) — see scope file
| rusqlite (`bundled`), zip, sevenz-rust, thiserror, bitflags, chrono, trash, open, rustix, serde | DB, archives, errors, time, FS effects | see PROJECT_OVERVIEW.crates-zenman-core.md |

### Internal
| zenman-core -> zenman-ui -> zenman (binary); xtask standalone | One-directional layering; `xtask deps` forbids UI deps in core | workspace |

## Build & Run

| Command | Purpose |
|---|---|
| `./scripts/build.sh` | Preflight + `--locked` release build (also `--check`, `--smoke`, `--install`, `--dist`) |
| `cargo build -p zenman` / `cargo run -p zenman` | Debug build / run with real GPU fallback ladder |
| `cargo test --workspace` | All engine contracts + UI kittest suites |
| `cargo xtask ci` | Full gate suite (fmt, clippy, tests, licences, deps, parity, snapshots) |
| `cargo xtask parity` / `contracts` / `snapshots` | Porting ledger, golden contracts, rendered snapshots |
| `cargo test -p zenman-ui --test file_pane` | One UI suite |
