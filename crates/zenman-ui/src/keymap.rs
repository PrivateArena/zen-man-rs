//! The keymap (plan §5.8 `keymap.rs`, decision **D7**).
//!
//! One table is the single source of truth for **both** sides of the keyboard story: the dispatch
//! side (`ZenApp::handle_keys` → [`Command`], run against the focused pane) and the display side
//! (the menu hints step 10b renders — a menu item prints its chord by asking [`KeyMap::hint`], so a
//! chord can never be re-labelled in one place and left stale in the other).
//!
//! # Where the chords come from
//!
//! The Go app keeps its registry in **`frontend/js/shortcuts.js:7-31`** (`SHORTCUTS`), matched by
//! `matchesShortcut` (`shortcuts.js:63-82`), routed by `handleKeyboardShortcuts`
//! (`keyboard.js:148-253`). Three bindings live outside that table and are enumerated here because
//! the registry is not the whole truth:
//!
//! * `Ctrl+Tab` / `Ctrl+Shift+Tab` — tab cycling (`keyboard.js:203-207`),
//! * `Shift+Tab` and `Alt+Z` — the *recent-tabs overlay* (`keyboard.js:171-183`, `triggerTabCycling`
//!   at `:110`), which Go draws as a modifier-held MRU popup,
//! * bare <kbd>H</kbd> — the action-log toggle (`action-log-init.js:16`), and <kbd>Escape</kbd>,
//!   which Go spends on closing the action log (`action-log.js:231`), the address bar
//!   (`app.js:276`) and its overlays (`keyboard.js:158,166`, `quick-find.js:353`) but **never** on
//!   the file list.
//!
//! Go has **no** arrow/Home/End/PageUp/PageDown bindings at all (plan §2.3's "no arrow navigation"
//! bug list) and no <kbd>Shift+Delete</kbd>; those rows below are the plan's own additions —
//! §6.2's "Arrow/Home/End/PageUp/PageDown/Esc added" row and decision **D6** for the trash/permanent
//! split. Each deviation names its source in the table.
//!
//! # Matching rules
//!
//! Matching is **exact on the modifier set** (`Chord::matches`), not egui's
//! `InputState::consume_key`, whose `Modifiers::matches_logically` ignores extra Shift/Alt and would
//! let `Delete` swallow `Shift+Delete`. Key events carry the modifier state *in the event*
//! (`Event::Key { modifiers, .. }`), so a chord and its shifted twin can never collide.
//!
//! Keypad <kbd>Enter</kbd> needs no special case: egui maps `NumpadEnter` to `Key::Enter` while
//! parsing the key name (`egui::Key::from_name`, `data/key.rs:513`), so both physical keys arrive as
//! one logical key and one chord.
//!
//! Printable-character chords: Go has exactly one, bare <kbd>H</kbd> (see above); it is bound, and
//! [`crate::app::ZenApp::handle_keys`] drops the whole pass whenever a text field owns the keyboard
//! (plan F15), which is what makes a bare letter safe.
//!
//! # The clipboard chords arrive as *events*, not as keys
//!
//! <kbd>Ctrl</kbd>+<kbd>X</kbd>/<kbd>C</kbd>/<kbd>V</kbd> are **not** deliverable as
//! [`Event::Key`] on any real backend: the pinned `egui-winit` translates the platform clipboard
//! chords — <kbd>Cmd/Ctrl</kbd>+<kbd>X</kbd>/<kbd>C</kbd>/<kbd>V</kbd>, the dedicated Cut/Copy/Paste
//! keys, and on Windows also <kbd>Shift</kbd>+<kbd>Del</kbd>, <kbd>Ctrl</kbd>+<kbd>Ins</kbd> and
//! <kbd>Shift</kbd>+<kbd>Ins</kbd> — into [`Event::Cut`], [`Event::Copy`] and [`Event::Paste`],
//! pushes that event and **returns without emitting the key press at all**
//! (`egui-winit/src/lib.rs:3112-3140,3213-3218`, whose `set_clipboard_shortcuts` switch is off only
//! at the cost of `TextEdit`'s own copy/paste). The key *release* is silenced too: the eaten press
//! leaves a `None` marker in `HeldKeys`, so lifting the key emits nothing either
//! (`egui-winit/src/held_keys.rs:26-33,83-93`). So the table keeps the chords — they are what the
//! menus *print*, and what a synthetic press answers — and [`KeyMap::lookup_events`] answers the
//! clipboard events in their place.
//!
//! Two consequences, both handled there:
//!
//! * **A clipboard event names no chord**, so its modifier strip is read out of the events that
//!   *precede it* ([`strips`]) — never from the frame's end-of-queue `InputState::modifiers`, which a
//!   quick tap has already cleared, turning Windows' <kbd>Shift</kbd>+<kbd>Del</kbd> into a plain cut.
//! * **The strip is the only evidence**, so an eaten chord that is not this shell's clipboard is
//!   answered by *nothing* ([`cut_command`]) rather than guessed, and Windows' <kbd>Shift</kbd>+
//!   <kbd>Del</kbd> — eaten as [`Event::Cut`] — keeps its own answer, [`Command::PermanentDelete`].
//!
//! The one keystroke nothing can see: [`Event::Paste`] is pushed only when the system clipboard
//! yielded text and [`Event::PasteImage`] only when it yielded an image, so a <kbd>Ctrl</kbd>+
//! <kbd>V</kbd> over an *empty* system clipboard arrives as no event at all, and `eframe` exposes no
//! raw-event hook to catch it upstream. Cut and Copy are therefore the reliable half of the trio —
//! their events are pushed before the clipboard is ever read.

use egui::{Event, Key, Modifiers};

/// Every action the shell can take, by the name of the `Session` / model call that performs it
/// (plan §5.8's `CommandId`). Dispatch and menus both speak this enum; nothing else names an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Command {
    /// Enter: open the selection (`Session::open_paths`, dirs navigate — Go's `open`).
    Open,
    /// `Ctrl+C` — `Session::clipboard_store` with `ClipOp::Copy` (Go's `copy`).
    Copy,
    /// `Ctrl+X` — the same with `ClipOp::Cut` (Go's `cut`).
    Cut,
    /// `Alt+T` — open the selected directory in a new tab (Go's `open-in-new-tab`).
    OpenInNewTab,
    /// `Ctrl+V` — `Session::paste_clipboard` into the shown directory (Go's `paste`).
    Paste,
    /// `Ctrl+Alt+V` — `Session::paste` in `PasteMode::Link` (Go's `paste-link`).
    PasteLink,
    /// `Alt+Shift+C` — the selection's paths as text on the system clipboard (Go's `copy-path`).
    CopyPath,
    /// `Ctrl+Alt+C` — the selection's names as text (Go's `copy-name`).
    CopyName,
    /// `Alt+C` — queue a directory's *children* (Go's `copy-inside`, `ClipScope::Inside`).
    CopyInside,
    /// `Alt+X` — the cut form of the same (Go's `cut-inside`).
    CutInside,
    /// `Alt+V` — paste into the selected directory instead of the shown one (Go's `paste-inside`).
    PasteInside,
    /// `F2` — start the in-cell rename (Go's `rename`; the editor itself is step 10c).
    Rename,
    /// `Delete` — `DeleteReq::to_trash`, plan **D6** (Go's `delete`, which destroyed bytes).
    Delete,
    /// `Shift+Delete` — `DeleteReq::permanent` (D6). **No Go binding**: Go's Delete was permanent.
    PermanentDelete,
    /// Go's `Change Permissions` row (`context-menu.js:105-107`) — **no chord**: Go's
    /// `shortcuts.js` registry has none for it either.
    Chmod,
    /// `Ctrl+Shift+N` — `Session::mkdir` (Go's `create-folder`; the name prompt is step 10c).
    NewFolder,
    /// `Ctrl+F` — quick-find overlay (Go's `quick-find`; the dialog is step 11).
    QuickFind,
    /// `Ctrl+Shift+F` — the indexed search manager (Go's `search-manager`; step 12).
    SearchManager,
    /// `F3` — the grid's split shape (Go's `split-view`, which *toggles*).
    SplitLayout,
    /// `F4` — the quad grid (Go's `quad-view`, which *toggles*).
    QuadLayout,
    /// One pane. **No Go binding** — Go's `F3` off-state was the single pane; menu-only here.
    SingleLayout,
    /// List ⇄ grid. **No Go binding** — Go's was a toolbar button.
    ToggleView,
    /// `Ctrl+B` — the places drawer (Go's `toggle-sidebar`).
    ToggleLeftDrawer,
    /// `Ctrl+J` — the disk drawer. **No Go binding**; plan §5.8's `side_panel` row names the chord.
    ToggleRightDrawer,
    /// `Ctrl+T` — a new tab in the focused pane (Go's `new-tab`).
    NewTab,
    /// Copy the shown tab beside itself. Go's `duplicate-tab` (`tabs.js:365-367`) — **no chord**: Go's
    /// registry (`shortcuts.js:7-31`) has none for it, only its tab-menu row did.
    DuplicateTab,
    /// `Ctrl+W` — close the shown tab (Go's `close-tab`).
    CloseTab,
    /// `Ctrl+Tab` — the next tab (Go's `cycleTabs(1)`, `keyboard.js:205`).
    NextTab,
    /// `Ctrl+Shift+Tab` — the previous tab (Go's `cycleTabs(-1)`, `keyboard.js:205`).
    PrevTab,
    /// `Shift+Tab` / `Alt+Z` — walk the MRU ring (Go's recent-tabs overlay, `keyboard.js:110-183`).
    CycleTabsMru,
    /// `Backspace` — the parent directory (Go's `navigate-up`).
    GoUp,
    /// `Ctrl+A` — every loaded row (Go's `select-all`).
    SelectAll,
    /// `Escape` — clear the selection / close what is open. **No Go binding on the file list**
    /// (plan §2.3's bug list); Go's `Escape` closed its overlays.
    SelectNone,
    /// Flip the loaded rows' selection. **No Go binding** — menu-only until a chord is agreed.
    InvertSelection,
    /// `F5` — re-read the directory, bypassing the cache (plan F13; Go's was a toolbar button).
    Refresh,
    /// `Alt+Enter` — the properties dialog (plan §5.8's `properties` row names the chord; Go had
    /// neither a key nor a dialog — its `/api/props` was never called).
    Properties,
    /// <kbd>Ctrl+E</kbd> — extract the archive the pane is listing into the directory that holds it
    /// (step 13b). **No Go binding**: Go shipped `/api/archive/extract` and never wired a caller.
    Extract,
    /// Bookmark the shown directory. **No Go binding** — menu-only (the places service is step 14).
    AddToFavorites,
    /// Focus the address bar. **No Go binding** — Go's address bar was clicked.
    FocusSearch,
    /// Open the pane's context menu at the pointer. **No Go binding** — Go's was right-click only.
    OpenContextMenu,
    /// Bare `H` — toggle the action log (Go's `action-log-init.js:16`).
    ActionLog,
    /// Cancel the newest live job (`Session::cancel`). **No Go binding**; the status strip's button
    /// is the entry point.
    CancelJob,
    /// Close the window (`ViewportCommand::Close`). **No Go binding** — Go was a web page.
    Quit,
    /// `Up` — move the cursor one row up. **No Go binding** (plan §6.2's added-navigation row).
    MoveUp,
    /// `Down`. **No Go binding.**
    MoveDown,
    /// `Home` — the first row. **No Go binding.**
    MoveTop,
    /// `End` — the last row. **No Go binding.**
    MoveBottom,
    /// `PageUp` — one screenful up (`ScrollState::first..last` is the screenful). **No Go binding.**
    PageUp,
    /// `PageDown`. **No Go binding.**
    PageDown,
}

impl Command {
    /// Every command, in table order — the iteration the "every chord tested" test walks.
    pub const ALL: &'static [Self] = &[
        Self::Open,
        Self::Copy,
        Self::Cut,
        Self::OpenInNewTab,
        Self::Paste,
        Self::PasteLink,
        Self::CopyPath,
        Self::CopyName,
        Self::CopyInside,
        Self::CutInside,
        Self::PasteInside,
        Self::Rename,
        Self::Delete,
        Self::PermanentDelete,
        Self::Chmod,
        Self::NewFolder,
        Self::QuickFind,
        Self::SearchManager,
        Self::SplitLayout,
        Self::QuadLayout,
        Self::SingleLayout,
        Self::ToggleView,
        Self::ToggleLeftDrawer,
        Self::ToggleRightDrawer,
        Self::NewTab,
        Self::DuplicateTab,
        Self::CloseTab,
        Self::NextTab,
        Self::PrevTab,
        Self::CycleTabsMru,
        Self::GoUp,
        Self::SelectAll,
        Self::SelectNone,
        Self::InvertSelection,
        Self::Refresh,
        Self::Properties,
        Self::Extract,
        Self::AddToFavorites,
        Self::FocusSearch,
        Self::OpenContextMenu,
        Self::ActionLog,
        Self::CancelJob,
        Self::Quit,
        Self::MoveUp,
        Self::MoveDown,
        Self::MoveTop,
        Self::MoveBottom,
        Self::PageUp,
        Self::PageDown,
    ];

    /// The menu text (plan §5.8's `MenuSpec::label`). Kept free of punctuation so 10b can add the
    /// danger/icon decoration without re-wording anything.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::OpenInNewTab => "Open in new tab",
            Self::Copy => "Copy",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::PasteLink => "Paste link",
            Self::CopyPath => "Copy path",
            Self::CopyName => "Copy name",
            Self::CopyInside => "Copy inside",
            Self::CutInside => "Cut inside",
            Self::PasteInside => "Paste inside",
            Self::Rename => "Rename",
            Self::Delete => "Move to trash",
            Self::PermanentDelete => "Delete permanently",
            Self::Chmod => "Change permissions",
            Self::NewFolder => "New folder",
            Self::QuickFind => "Quick find",
            Self::SearchManager => "Search manager",
            Self::SplitLayout => "Split view",
            Self::QuadLayout => "Quad view",
            Self::SingleLayout => "Single view",
            Self::ToggleView => "Toggle grid view",
            Self::ToggleLeftDrawer => "Toggle places",
            Self::ToggleRightDrawer => "Toggle disks",
            Self::NewTab => "New tab",
            Self::DuplicateTab => "Duplicate tab",
            Self::CloseTab => "Close tab",
            Self::NextTab => "Next tab",
            Self::PrevTab => "Previous tab",
            Self::CycleTabsMru => "Recent tabs",
            Self::GoUp => "Go up",
            Self::SelectAll => "Select all",
            Self::SelectNone => "Clear selection",
            Self::InvertSelection => "Invert selection",
            Self::Refresh => "Refresh",
            Self::Properties => "Properties",
            Self::AddToFavorites => "Add to favorites",
            Self::FocusSearch => "Edit address",
            Self::OpenContextMenu => "Context menu",
            Self::ActionLog => "Action log",
            Self::CancelJob => "Cancel job",
            Self::Quit => "Quit",
            Self::MoveUp => "Move up",
            Self::MoveDown => "Move down",
            Self::MoveTop => "First row",
            Self::MoveBottom => "Last row",
            Self::PageUp => "Page up",
            Self::PageDown => "Page down",
            Self::Extract => "Extract here",
        }
    }
}

/// One key press with its modifier set. `Copy + Eq + Hash` so a test can put chords in a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    /// <kbd>Ctrl</kbd> (and, on macOS, the Command key: egui sets `ctrl` for the primary modifier on
    /// every platform but macOS, and `mac_cmd` there — see [`Chord::matches`]).
    pub ctrl: bool,
    /// <kbd>Alt</kbd>.
    pub alt: bool,
    /// <kbd>Shift</kbd>. Part of the identity: `Shift+Delete` and `Delete` are two chords.
    pub shift: bool,
    /// The logical key.
    pub key: Key,
}

impl Chord {
    /// The bare key, no modifiers (Go's `Enter`, `Del`, `F2`, `Backspace`, `H`).
    #[must_use]
    pub const fn key(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            key,
        }
    }

    /// `Ctrl+key`.
    #[must_use]
    pub const fn ctrl(key: Key) -> Self {
        Self {
            ctrl: true,
            ..Self::key(key)
        }
    }

    /// `Alt+key`.
    #[must_use]
    pub const fn alt(key: Key) -> Self {
        Self {
            alt: true,
            ..Self::key(key)
        }
    }

    /// `Shift+key`.
    #[must_use]
    pub const fn shift(key: Key) -> Self {
        Self {
            shift: true,
            ..Self::key(key)
        }
    }

    /// `Ctrl+Alt+key`.
    #[must_use]
    pub const fn ctrl_alt(key: Key) -> Self {
        Self {
            ctrl: true,
            alt: true,
            ..Self::key(key)
        }
    }

    /// `Ctrl+Shift+key`.
    #[must_use]
    pub const fn ctrl_shift(key: Key) -> Self {
        Self {
            ctrl: true,
            shift: true,
            ..Self::key(key)
        }
    }

    /// `Alt+Shift+key`.
    #[must_use]
    pub const fn alt_shift(key: Key) -> Self {
        Self {
            alt: true,
            shift: true,
            ..Self::key(key)
        }
    }

    /// `Ctrl+Alt+Shift+key`.
    #[must_use]
    pub const fn ctrl_alt_shift(key: Key) -> Self {
        Self {
            ctrl: true,
            alt: true,
            shift: true,
            ..Self::key(key)
        }
    }

    /// Parse the table's own spelling — `"Ctrl+Shift+Delete"`, `"Del"`, `"Enter"`, `"Alt+Z"`.
    ///
    /// Modifiers may come in any order and any case; the key is Go's spelling as well as ours, so
    /// `shortcuts.js` can be read straight against this parser in a test. Aliases Go uses and egui
    /// does not (`Del`, `Esc`, `PgUp`, `PgDn`, `Return`, `NumpadEnter`) are folded in; the keypad
    /// Enter folds away because egui already maps `NumpadEnter` to the logical `Enter`
    /// (`Key::from_name`), which is the whole "keypad vs main Enter" story.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (&key_part, mods) = parts.split_last()?;
        if key_part.is_empty() {
            return None;
        }
        for part in mods {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" => alt = true,
                "shift" => shift = true,
                _ => return None,
            }
        }
        let key = parse_key(key_part)?;
        Some(Self {
            ctrl,
            alt,
            shift,
            key,
        })
    }

    /// The table's canonical spelling, in the fixed order `Ctrl+Alt+Shift+Key`. Round-trips through
    /// [`Self::parse`] for every chord in the map (tested).
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        out.push_str(self.key.name());
        out
    }

    /// Exact match against one key event's modifier set.
    ///
    /// Deliberately *not* `Modifiers::matches_logically` (which ignores extra Shift/Alt, so `Delete`
    /// would eat `Shift+Delete`). macOS: a chord's `ctrl` also accepts the Command key, because
    /// egui's macOS path leaves `ctrl` false and sets `mac_cmd`/`command` for what a user calls "the
    /// Ctrl key" on every other platform.
    #[must_use]
    pub fn matches(&self, key: Key, modifiers: &Modifiers) -> bool {
        if key != self.key {
            return false;
        }
        // `command` mirrors `ctrl` on Windows and Linux and carries ⌘ on macOS, where `ctrl` stays
        // false for what the label calls Ctrl — so accepting it (and `mac_cmd`) is what makes a
        // `Ctrl+T` chord reachable on a Mac at all (`egui::Modifiers`, `data/input/modifiers.rs:33-37`).
        let ctrl =
            self.ctrl == modifiers.ctrl || (self.ctrl && (modifiers.command || modifiers.mac_cmd));
        let alt = self.alt == modifiers.alt;
        let shift = self.shift == modifiers.shift;
        ctrl && alt && shift
    }

    /// The key-press event this chord is (synthesised by the dispatch tests; `modifiers` is what a
    /// real press of this chord carries).
    #[must_use]
    pub const fn event(self) -> Event {
        Event::Key {
            key: self.key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers {
                alt: self.alt,
                ctrl: self.ctrl,
                shift: self.shift,
                mac_cmd: false,
                command: self.ctrl,
            },
        }
    }
}

/// Which chord an [`Event::Cut`] was.
///
/// `on_windows` is a parameter rather than a `cfg` so both arms are testable from any host; the one
/// call site passes [`cfg!`]'s answer.
///
/// * Windows eats <kbd>Shift</kbd>+<kbd>Del</kbd> into this very event (`is_cut_command`,
///   `egui-winit/src/lib.rs:3753-3757`), and that chord is this shell's [`Command::PermanentDelete`].
/// * The same clause eats <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Del</kbd>, which no chord of ours
///   binds — answered by nothing, because guessing here would queue a move the user never asked for.
/// * Everything else — <kbd>Cmd/Ctrl</kbd>+<kbd>X</kbd>, the dedicated Cut key, or a Cut whose strip
///   was never reported (an accessibility-driven one, or a harness injecting one bare event) — is the
///   cut chord's own spelling.
#[must_use]
fn cut_command(modifiers: &Modifiers, on_windows: bool) -> Option<Command> {
    if on_windows && modifiers.shift && !modifiers.ctrl && !modifiers.alt {
        return Some(Command::PermanentDelete);
    }
    if modifiers.ctrl && modifiers.shift {
        return None;
    }
    Some(Command::Cut)
}

/// `true` for the events `egui-winit` synthesises *instead of* a key press.
#[must_use]
fn synthesised(event: &Event) -> bool {
    matches!(
        event,
        Event::Cut | Event::Copy | Event::Paste(_) | Event::PasteImage(_)
    )
}

/// The modifier strip in force at each event of a frame. `egui-winit` reports a modifier press as its
/// own [`Event::ModifiersChanged`] (`egui-winit/src/lib.rs:2367-2387`), and a clipboard event belongs
/// to the strip held when it was pushed — see the module docs.
fn strips(events: &[Event]) -> Vec<Modifiers> {
    let mut current = Modifiers::NONE;
    events
        .iter()
        .map(|event| {
            match event {
                Event::ModifiersChanged(held) => current = *held,
                Event::Key { modifiers, .. } => current = *modifiers,
                _ => {}
            }
            current
        })
        .collect()
}

/// The key part of a chord: egui's own name table plus the spellings Go's registry uses.
fn parse_key(part: &str) -> Option<Key> {
    let alias = match part.to_ascii_lowercase().as_str() {
        "del" => "Delete",
        "esc" => "Escape",
        "return" | "numpadenter" => "Enter",
        "pgup" | "prior" => "PageUp",
        "pgdn" | "next" => "PageDown",
        "ins" => "Insert",
        "spacebar" => "Space",
        " " => "Space",
        _ => return Key::from_name(part).or_else(|| Key::from_name(&capitalise(part))),
    };
    Key::from_name(alias).or_else(|| Key::from_name(part))
}

/// `f2` → `F2`, `a` → `A`: egui's name table is case-sensitive for function keys and digits.
fn capitalise(part: &str) -> String {
    let mut out = part.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// One command with its (possibly empty) chord list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    command: Command,
    chords: Vec<Chord>,
}

/// The whole map. Built from [`TABLE`]; immutable in practice (nothing here mutates it — user-editable
/// bindings are not on the plan's menu, and this type would grow a reload if they ever were).
pub struct KeyMap {
    table: Vec<Binding>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self {
            table: TABLE
                .iter()
                .map(|(command, chords)| Binding {
                    command: *command,
                    chords: chords.to_vec(),
                })
                .collect(),
        }
    }
}

impl KeyMap {
    /// The command one key event asks for, exact-match (`None` for an unbound key).
    #[must_use]
    pub fn lookup(&self, key: Key, modifiers: &Modifiers) -> Option<Command> {
        self.table.iter().find_map(|binding| {
            binding
                .chords
                .iter()
                .any(|chord| chord.matches(key, modifiers))
                .then_some(binding.command)
        })
    }

    /// [`Self::lookup`] for one raw event: only a key *press* answers, and a clipboard event answers
    /// with an empty modifier set — see [`Self::lookup_event_with`] for what a live frame supplies.
    #[must_use]
    pub fn lookup_event(&self, event: &Event) -> Option<Command> {
        self.lookup_event_with(event, &Modifiers::NONE)
    }

    /// [`Self::lookup_event`] with the modifier strip that was held when the event was pushed — the
    /// only evidence a synthesised clipboard event carries about which chord it was (module docs,
    /// "the clipboard chords arrive as events").
    #[must_use]
    pub fn lookup_event_with(&self, event: &Event, modifiers: &Modifiers) -> Option<Command> {
        match event {
            Event::Key {
                key,
                pressed: true,
                modifiers: pressed_with,
                ..
            } => self.lookup(*key, pressed_with),
            Event::Cut => cut_command(modifiers, cfg!(target_os = "windows")),
            Event::Copy => Some(Command::Copy),
            // Both Paste forms are the paste chord: the fork reads the clipboard first and pushes the
            // text form for a text clipboard, the image form for an image one. The payload is egui's
            // own, and the file manager's clipboard is the engine's, so neither string matters here.
            Event::Paste(_) | Event::PasteImage(_) => Some(Command::Paste),
            _ => None,
        }
    }

    /// Every command this frame's events ask for, in event order (plan F15's guard lives at the call
    /// site, [`crate::app::ZenApp::handle_keys`]). Each event is read against the modifier strip it
    /// was pushed under ([`strips`]).
    #[must_use]
    pub fn lookup_events(&self, events: &[Event]) -> Vec<Command> {
        let strips = strips(events);
        let hits: Vec<(Command, bool)> = events
            .iter()
            .zip(&strips)
            .filter_map(|(event, modifiers)| {
                let command = self.lookup_event_with(event, modifiers)?;
                Some((command, synthesised(event)))
            })
            .collect();
        // A clipboard event is an *echo* of a chord press whenever that press is also in the frame —
        // and a doubled Paste is a duplicated file batch, not a no-op. This fork emits one or the
        // other, so the rule costs a comparison and never fires today; it fires the day egui's
        // clipboard translation is turned off.
        hits.iter()
            .filter(|(command, is_synthesised)| {
                !is_synthesised
                    || !hits
                        .iter()
                        .any(|(other, pressed)| !pressed && other == command)
            })
            .map(|(command, _)| *command)
            .collect()
    }

    /// The chords bound to `command` — empty for a menu-only command, never `None`, so a caller can
    /// loop without a branch.
    #[must_use]
    pub fn bindings_for(&self, command: Command) -> &[Chord] {
        self.table
            .iter()
            .find(|binding| binding.command == command)
            .map(|binding| binding.chords.as_slice())
            .unwrap_or_default()
    }

    /// Every `(chord, command)` pair, table order.
    pub fn iterate(&self) -> impl Iterator<Item = (Chord, Command)> + '_ {
        self.table
            .iter()
            .flat_map(|binding| binding.chords.iter().map(move |c| (*c, binding.command)))
    }

    /// Every command, table order (the same order [`Command::ALL`] walks).
    pub fn commands(&self) -> impl Iterator<Item = Command> + '_ {
        self.table.iter().map(|binding| binding.command)
    }

    /// The hint a menu item prints: its first chord, or `None` when the command is menu-only —
    /// which is exactly the distinction plan §5.8's `MenuSpec::chord: Option<Chord>` is drawn for.
    #[must_use]
    pub fn hint(&self, command: Command) -> Option<String> {
        self.bindings_for(command).first().map(Chord::display)
    }

    /// [`Self::hint`] with the placeholder a two-column menu wants, so 10b can print the hint cell
    /// unconditionally and keep the labels aligned.
    #[must_use]
    pub fn hint_text(&self, command: Command) -> String {
        self.hint(command).unwrap_or_else(|| "—".to_string())
    }

    /// `true` when the command has at least one chord.
    #[must_use]
    pub fn is_bound(&self, command: Command) -> bool {
        !self.bindings_for(command).is_empty()
    }
}

/// The table itself: one row per [`Command::ALL`] entry, in that order, chords in the spelling
/// [`Chord::parse`] accepts. The source of each row is the comment on its command above.
const TABLE: &[(Command, &[Chord])] = &[
    // --- shortcuts.js:7-31, verbatim -----------------------------------------------------------
    (Command::Open, &[Chord::key(Key::Enter)]),
    (Command::Cut, &[Chord::ctrl(Key::X)]),
    (Command::Copy, &[Chord::ctrl(Key::C)]),
    (Command::CopyPath, &[Chord::alt_shift(Key::C)]),
    (Command::CopyName, &[Chord::ctrl_alt(Key::C)]),
    (Command::OpenInNewTab, &[Chord::alt(Key::T)]),
    (Command::CutInside, &[Chord::alt(Key::X)]),
    (Command::CopyInside, &[Chord::alt(Key::C)]),
    (Command::PasteInside, &[Chord::alt(Key::V)]),
    (Command::Rename, &[Chord::key(Key::F2)]),
    (Command::Delete, &[Chord::key(Key::Delete)]),
    (Command::Paste, &[Chord::ctrl(Key::V)]),
    (Command::PasteLink, &[Chord::ctrl_alt(Key::V)]),
    (Command::NewFolder, &[Chord::ctrl_shift(Key::N)]),
    (Command::SearchManager, &[Chord::ctrl_shift(Key::F)]),
    (Command::QuickFind, &[Chord::ctrl(Key::F)]),
    (Command::SplitLayout, &[Chord::key(Key::F3)]),
    (Command::QuadLayout, &[Chord::key(Key::F4)]),
    (Command::ToggleLeftDrawer, &[Chord::ctrl(Key::B)]),
    (Command::NewTab, &[Chord::ctrl(Key::T)]),
    (Command::DuplicateTab, &[]),
    (Command::CloseTab, &[Chord::ctrl(Key::W)]),
    (Command::GoUp, &[Chord::key(Key::Backspace)]),
    (Command::SelectAll, &[Chord::ctrl(Key::A)]),
    // --- keyboard.js ----------------------------------------------------------------------------
    (Command::NextTab, &[Chord::ctrl(Key::Tab)]),
    (Command::PrevTab, &[Chord::ctrl_shift(Key::Tab)]),
    (
        Command::CycleTabsMru,
        &[Chord::shift(Key::Tab), Chord::alt(Key::Z)],
    ),
    // --- action-log-init.js:16 ------------------------------------------------------------------
    (Command::ActionLog, &[Chord::key(Key::H)]),
    // --- the plan's own additions, each named in the command doc --------------------------------
    (Command::PermanentDelete, &[Chord::shift(Key::Delete)]),
    (Command::Chmod, &[]),
    (Command::SelectNone, &[Chord::key(Key::Escape)]),
    (Command::Refresh, &[Chord::key(Key::F5)]),
    (Command::ToggleRightDrawer, &[Chord::ctrl(Key::J)]),
    (Command::Properties, &[Chord::alt(Key::Enter)]),
    (Command::Extract, &[Chord::ctrl(Key::E)]),
    (Command::MoveUp, &[Chord::key(Key::ArrowUp)]),
    (Command::MoveDown, &[Chord::key(Key::ArrowDown)]),
    (Command::MoveTop, &[Chord::key(Key::Home)]),
    (Command::MoveBottom, &[Chord::key(Key::End)]),
    (Command::PageUp, &[Chord::key(Key::PageUp)]),
    (Command::PageDown, &[Chord::key(Key::PageDown)]),
    // --- menu-only rows (10b draws these; no chord until one is agreed) --------------------------
    (Command::SingleLayout, &[]),
    (Command::ToggleView, &[]),
    (Command::InvertSelection, &[]),
    (Command::AddToFavorites, &[]),
    (Command::FocusSearch, &[]),
    (Command::OpenContextMenu, &[]),
    (Command::CancelJob, &[]),
    (Command::Quit, &[]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_row_is_a_command_and_every_command_has_a_row() {
        let map = KeyMap::default();
        let mut listed: Vec<Command> = map.commands().collect();
        let mut all = Command::ALL.to_vec();
        listed.sort();
        all.sort();
        assert_eq!(
            listed, all,
            "the table and ALL cover exactly the same commands"
        );
        for command in Command::ALL {
            // Either a chord or an explicit menu-only row; never a missing row.
            let _ = map.bindings_for(*command);
        }
    }

    #[test]
    fn every_chord_round_trips_through_parse_and_display() {
        let map = KeyMap::default();
        for (chord, command) in map.iterate() {
            let text = chord.display();
            assert_eq!(
                Chord::parse(&text),
                Some(chord),
                "{command:?} hint {text:?} must parse back to itself"
            );
        }
    }

    #[test]
    fn no_two_commands_answer_the_same_chord() {
        let map = KeyMap::default();
        let mut seen: Vec<(Chord, Command)> = map.iterate().collect();
        seen.sort_by_key(|(chord, _)| *chord);
        let pairs = seen.windows(2);
        for pair in pairs {
            assert_ne!(
                pair[0].0,
                pair[1].0,
                "{} and {} both answer {}",
                pair[0].1.label(),
                pair[1].1.label(),
                pair[0].0.display()
            );
        }
    }

    #[test]
    fn shift_delete_is_not_shadowed_by_delete() {
        let map = KeyMap::default();
        let plain = map.lookup(Key::Delete, &Modifiers::NONE);
        let shifted = map.lookup(
            Key::Delete,
            &Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(plain, Some(Command::Delete));
        assert_eq!(shifted, Some(Command::PermanentDelete));
        // …and the same trap for the two-tab pair and the three `C` chords.
        let ctrl_shift_c = map.lookup(
            Key::C,
            &Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(ctrl_shift_c, None, "Ctrl+Shift+C belongs to nobody");
        assert_eq!(
            map.lookup(
                Key::C,
                &Modifiers {
                    alt: true,
                    shift: true,
                    ..Modifiers::NONE
                }
            ),
            Some(Command::CopyPath)
        );
        assert_eq!(
            map.lookup(
                Key::C,
                &Modifiers {
                    ctrl: true,
                    alt: true,
                    ..Modifiers::NONE
                }
            ),
            Some(Command::CopyName)
        );
    }

    #[test]
    fn go_spellings_parse_to_the_same_chords_we_display() {
        // The registry text of `frontend/js/shortcuts.js`, read through our parser.
        for (text, expected) in [
            ("Enter", Chord::key(Key::Enter)),
            ("Ctrl+X", Chord::ctrl(Key::X)),
            ("Ctrl+C", Chord::ctrl(Key::C)),
            ("Alt+Shift+C", Chord::alt_shift(Key::C)),
            ("Ctrl+Alt+C", Chord::ctrl_alt(Key::C)),
            ("Alt+T", Chord::alt(Key::T)),
            ("Alt+X", Chord::alt(Key::X)),
            ("Alt+V", Chord::alt(Key::V)),
            ("F2", Chord::key(Key::F2)),
            ("Del", Chord::key(Key::Delete)),
            ("Ctrl+V", Chord::ctrl(Key::V)),
            ("Ctrl+Alt+V", Chord::ctrl_alt(Key::V)),
            ("Ctrl+Shift+N", Chord::ctrl_shift(Key::N)),
            ("Ctrl+Shift+F", Chord::ctrl_shift(Key::F)),
            ("Ctrl+F", Chord::ctrl(Key::F)),
            ("F3", Chord::key(Key::F3)),
            ("F4", Chord::key(Key::F4)),
            ("Ctrl+B", Chord::ctrl(Key::B)),
            ("Ctrl+T", Chord::ctrl(Key::T)),
            ("Ctrl+W", Chord::ctrl(Key::W)),
            ("Backspace", Chord::key(Key::Backspace)),
            ("Ctrl+A", Chord::ctrl(Key::A)),
        ] {
            assert_eq!(Chord::parse(text), Some(expected), "{text}");
        }
        // Keypad Enter *is* Enter to egui, so one chord covers both physical keys.
        assert_eq!(Chord::parse("NumpadEnter"), Chord::parse("Enter"));
        assert_eq!(Chord::parse("Return"), Chord::parse("Enter"));
        assert_eq!(Chord::parse("Esc"), Chord::parse("Escape"));
        assert_eq!(
            Chord::parse("ctrl+shift+delete"),
            Chord::parse("Ctrl+Shift+Delete")
        );
        assert_eq!(Chord::parse("f2"), Chord::parse("F2"));
        assert_eq!(Chord::parse("bad+x"), None);
        assert_eq!(Chord::parse(""), None, "an empty key part is not a chord");
    }

    #[test]
    fn only_presses_answer_and_the_event_form_agrees_with_the_pair_form() {
        let map = KeyMap::default();
        let chord = Chord::ctrl(Key::C);
        assert_eq!(map.lookup_event(&chord.event()), Some(Command::Copy));
        let release = Event::Key {
            key: Key::C,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        assert_eq!(map.lookup_event(&release), None);
        assert_eq!(
            map.lookup_events(&[chord.event(), release]),
            vec![Command::Copy]
        );
    }

    #[test]
    fn the_clipboard_events_answer_the_clipboard_commands() {
        let map = KeyMap::default();
        // What a real window gets for Ctrl+X / Ctrl+C / Ctrl+V — the key press is retired by
        // `egui-winit`, so these events are the only route the three commands have.
        assert_eq!(map.lookup_event(&Event::Cut), Some(Command::Cut));
        assert_eq!(map.lookup_event(&Event::Copy), Some(Command::Copy));
        assert_eq!(
            map.lookup_event(&Event::Paste("a\nb".to_string())),
            Some(Command::Paste),
            "a paste arrives with the clipboard text attached, which the file manager ignores"
        );
        // A file manager's clipboard is the engine's, so an image clipboard is the same command. The
        // pixels are irrelevant to a chord, so the image is egui's own placeholder rather than a
        // palette colour: `theme.rs` owns every colour this crate paints, and this one paints none.
        assert_eq!(
            map.lookup_event(&Event::PasteImage(std::sync::Arc::new(
                egui::ColorImage::example()
            ))),
            Some(Command::Paste),
            "a Ctrl+V over an image clipboard is still the paste chord"
        );
    }

    #[test]
    fn the_clipboard_events_are_read_against_the_strip_held_when_they_were_pushed() {
        let shift = Modifiers::SHIFT;
        // The frame a Windows Shift+Del tap really produces: the modifier press, the Cut that ate the
        // Del press, then the strip clearing again. End-of-queue modifiers are NONE here — reading
        // them is what turned a permanent delete into a cut.
        let frame = [
            Event::Key {
                key: Key::ShiftLeft,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: shift,
            },
            Event::ModifiersChanged(shift),
            Event::Cut,
            Event::ModifiersChanged(Modifiers::NONE),
        ];
        let held = strips(&frame);
        assert_eq!(
            held[2], shift,
            "the Cut is read under the strip it was pushed with"
        );
        assert_eq!(
            cut_command(&held[2], true),
            Some(Command::PermanentDelete),
            "and on Windows that strip is the permanent-delete chord"
        );
        assert_eq!(
            cut_command(&held[2], false),
            Some(Command::Cut),
            "off Windows Shift+Del is never eaten, so a Cut stays a Cut"
        );
    }

    #[test]
    fn an_eaten_chord_that_is_not_a_clipboard_command_answers_nothing() {
        let both = Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..Modifiers::NONE
        };
        // Windows' `is_cut_command` checks Shift alone, so it eats Ctrl+Shift+Del too. Nothing is
        // bound to that chord, so it must not become a cut.
        assert_eq!(cut_command(&both, true), None);
        assert_eq!(
            cut_command(&both, false),
            None,
            "and the same strip is unbound wherever it comes from"
        );
        let cut_mods = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            cut_command(&cut_mods, true),
            Some(Command::Cut),
            "the cut chord itself is the one strip that must survive"
        );
    }

    #[test]
    fn a_chord_and_its_clipboard_event_in_one_frame_answer_once() {
        let map = KeyMap::default();
        let chord = Chord::ctrl(Key::V).event();
        assert_eq!(
            map.lookup_events(&[chord.clone(), Event::Paste("x".into())]),
            vec![Command::Paste],
            "a doubled Paste is a duplicated file batch, not a no-op"
        );
        assert_eq!(
            map.lookup_events(&[Event::Paste("x".into()), chord]),
            vec![Command::Paste],
            "and the guard does not depend on which of the two arrives first"
        );
    }

    #[test]
    fn menu_only_commands_have_no_hint_and_bound_ones_do() {
        let map = KeyMap::default();
        assert_eq!(map.hint(Command::Copy).as_deref(), Some("Ctrl+C"));
        assert!(!map.is_bound(Command::Quit), "Quit is menu-only");
        assert_eq!(map.hint_text(Command::Quit), "—");
        assert_eq!(
            map.bindings_for(Command::CycleTabsMru),
            &[Chord::shift(Key::Tab), Chord::alt(Key::Z)],
            "one command may answer two chords, in menu order"
        );
    }
}
