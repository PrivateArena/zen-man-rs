//! The dock: pane grid, persistence shape, and the layout edits (plan §5.7 `dock.rs` row).
//!
//! File panes are the **only** dock panels: `docking::Side` is `Start | End` only (§2.2 verified),
//! so the 48 px header is an `egui::TopBottomPanel` outside the `DockArea`, and the drawers and the
//! action log are panels too — the dock owns nothing but the pane grid.
//!
//! Every pane answers [`Permissions::PROTECTED`], which makes its group render **stripless**
//! (`is_protected() = !movable && !closable && !accepts_tabs`, `docking/model.rs:529-535`): no dock
//! tab strip, no drag bar, the whole rect to `DockContent::body`. That is what lets step 8's
//! `panels/tab_bar.rs` own the coloured 38 px strip. Drag-to-split between panes is deliberately not
//! a feature (§10 R8): with `movable:false` no gesture can produce `Action::Move`, so the grid only
//! ever changes shape through [`set_layout`], which edits the `Layout` directly.

use serde::{Deserialize, Serialize};

use craft_ui::docking::{Layout, Node, Permissions, Workspace};
use craft_ui::layout::{SplitAxis, SplitSize};

use zenman_core::store::config::{LayoutMode, PaneSlot};

use crate::state::{panes_for, quad_slots};

/// The dock panel identity. Serde-persisted (it rides inside the `Workspace` JSON), hence the
/// explicit `rename`-free stable representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockId {
    /// One file pane.
    FilePane(PaneSlot),
}

// `PaneSlot` (core's persisted enum) is `Eq + Ord` but not `Hash` — the docking surface needs
// `Hash`, so the slot's discriminant is the hash. (Fork correction: `#[derive(Hash)]` on `DockId`
// will not compile against core's `PaneSlot`.)
impl std::hash::Hash for DockId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let discriminant = match dock_slot_key(self) {
            0 => 0u8,
            1 => 1,
            2 => 2,
            _ => 3,
        };
        discriminant.hash(state);
    }
}

fn dock_slot_key(id: &DockId) -> u8 {
    let DockId::FilePane(slot) = id;
    match slot {
        PaneSlot::Left => 0,
        PaneSlot::Right => 1,
        PaneSlot::LeftBottom => 2,
        PaneSlot::RightBottom => 3,
    }
}

/// The dock state: `craft_ui`'s `Workspace` over [`DockId`] (the plan's `Dock` alias).
pub type Dock = Workspace<DockId>;

/// The `Id` of the single `DockArea` in the window.
#[must_use]
pub fn dock_area_id() -> egui::Id {
    egui::Id::unique("zenman-dock-area")
}

/// The permissions every file pane answers with — stripless rendering, no dock chrome, dividers
/// still draggable (resize is not permission-gated, `docking/model.rs:588,1140`).
#[must_use]
pub const fn pane_permissions() -> Permissions {
    Permissions::PROTECTED
}

/// The permission function the render/apply call sites pass (`DockContent::permissions` and these
/// must agree, or the stripless guarantee is a race).
#[must_use]
pub fn perms_of(_panel: &DockId) -> Permissions {
    pane_permissions()
}

/// The pane set of a dock id.
#[must_use]
pub fn slot_of(panel: &DockId) -> PaneSlot {
    let DockId::FilePane(slot) = panel;
    *slot
}

/// One stripless leaf: a tab group holding exactly this pane.
fn leaf(slot: PaneSlot) -> Node<DockId> {
    Node::Tabs {
        panels: vec![DockId::FilePane(slot)],
        active: 0,
    }
}

fn split(axis: SplitAxis, first: Node<DockId>, second: Node<DockId>) -> Node<DockId> {
    Node::Split {
        axis,
        // Half-and-half; the user's divider drag is what changes it, and `Action::Resize` rides in
        // the same `Layout` the app persists.
        size: SplitSize::Ratio(0.5),
        first: Box::new(first),
        second: Box::new(second),
    }
}

/// The default layout for a pane set: a pure `Node::Split` tree of single-panel leaves — one panel
/// per group, so every group is stripless (plan §5.7 `dock.rs` row).
#[must_use]
pub fn layout_for_slots(slots: &[PaneSlot]) -> Layout<DockId> {
    let root = match slots {
        [left] => Some(leaf(*left)),
        [left, right] => Some(split(SplitAxis::Horizontal, leaf(*left), leaf(*right))),
        [left, right, left_bottom, right_bottom] => Some(split(
            SplitAxis::Horizontal,
            split(SplitAxis::Vertical, leaf(*left), leaf(*left_bottom)),
            split(SplitAxis::Vertical, leaf(*right), leaf(*right_bottom)),
        )),
        // A one-slot list that is not `[Left]` (a restored oddity): still valid, one leaf.
        [first, ..] => Some(leaf(*first)),
        [] => None,
    };
    Layout {
        root,
        floating: Vec::new(),
    }
}

/// The default layout for a persisted mode (`Single`/`Split`).
#[must_use]
pub fn default_layout(mode: LayoutMode) -> Layout<DockId> {
    layout_for_slots(&panes_for(mode))
}

/// The default workspace for a mode — the `reconcile` anchor and the `F17` fallback.
#[must_use]
pub fn default_workspace(mode: LayoutMode) -> Dock {
    Dock::new(default_layout(mode))
}

/// The workspace for the four-pane grid (not a `LayoutMode`: Go's config cannot store it, plan
/// §5.7 `state.rs` correction).
#[must_use]
pub fn quad_workspace() -> Dock {
    Dock::new(layout_for_slots(&quad_slots()))
}

/// What [`apply`] did — the plan's `ApplyReport`, spelled so the caller can *say* what happened
/// (a swallowed report is exactly the "silently dropped a pane state" bug Go's F3 had).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyReport {
    /// The saved layout was installed after reconciliation against the default.
    Applied,
    /// The file carries a newer `Workspace::VERSION`; the default layout is in use, the file was
    /// kept (plan F17/F20: never overwrite a newer file, plan §2.2 `Workspace::VERSION` = 2).
    NewerVersion {
        /// The version the file asked for.
        found: u32,
    },
    /// The JSON did not parse, or failed `Layout::validate` even after reconciliation.
    Invalid,
}

/// Serialise the live dock (plan §5.7 `snapshot`; the bytes go to `persist.rs`, never to disk from
/// here — `zenman-ui` does no IO).
///
/// # Errors
/// Serialisation cannot fail for this shape today, but the signature keeps the future `?` honest.
pub fn snapshot(dock: &Dock) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(dock)
}

/// Read a persisted dock, validate it, and reconcile it against the registered panes
/// (plan §5.7 `apply`, plan F17: never let a bad file reach the first paint).
///
/// `registered` answers "is this pane part of the current grid" — a pane from a quad file on a
/// single-pane start goes to `hidden` with its location, which is exactly what `reconcile` is for.
pub fn apply(
    dock: &mut Dock,
    value: &serde_json::Value,
    registered: &dyn Fn(&DockId) -> bool,
    default: &Dock,
) -> ApplyReport {
    let Ok(saved) = serde_json::from_value::<Dock>(value.clone()) else {
        *dock = default.clone();
        return ApplyReport::Invalid;
    };
    if saved.version > Dock::VERSION {
        // Keep the file, use the default, report — the F17 sentence, in code.
        *dock = default.clone();
        return ApplyReport::NewerVersion {
            found: saved.version,
        };
    }
    if saved.layout.validate().is_err() {
        *dock = default.clone();
        return ApplyReport::Invalid;
    }
    *dock = saved;
    dock.reconcile(registered, perms_of, default);
    ApplyReport::Applied
}

/// The **only** way the grid changes shape (plan §5.7 `set_layout`): a direct `Layout` edit, since
/// `Action::Move`/`OpenAt` are permission-gated on `movable` and our panes are not movable.
///
/// Pane *contents* survive: [`crate::state::AppModel::set_grid`] keeps every pane that stays in the
/// new grid (Go's F3 dropped them; the port's whole reason `set_grid` exists).
pub fn set_layout(model: &mut crate::state::AppModel, dock: &mut Dock, slots: Vec<PaneSlot>) {
    model.set_grid(slots.clone());
    // The persisted mode follows the grid (`Quad` has no `LayoutMode` spelling — plan §5.7 state
    // correction), so `layout_mode_is_consistent` holds after every restructure, Go-F3-style.
    model.layout = if slots.len() >= 2 {
        LayoutMode::Split
    } else {
        LayoutMode::Single
    };
    let layout = layout_for_slots(&slots);
    if layout.validate().is_ok() {
        dock.layout = layout;
    }
    // An invalid tree would be a bug in `layout_for_slots`, not user data; the previous layout
    // stays, and `layout_mode_is_consistent` still holds because `set_grid` ran regardless.
}

/// The pane slots a dock tree actually holds, in grid order.
///
/// [`set_layout`] edits the model and the tree together, so they agree by construction — but a tree
/// read back from a file (`persist`, or a workspace's own `dock` value) can name panels the model does
/// not have, and that mismatch is only visible by asking the tree. A four-leaf tree from a quad run over
/// a two-pane grid is the case that matters: the extra panels draw nothing
/// ([`crate::dock_content`]), so the run looks broken and then saves itself that way.
pub fn panels(dock: &Dock) -> Vec<PaneSlot> {
    let mut slots = dock
        .layout
        .panels()
        .into_iter()
        .map(slot_of)
        .collect::<Vec<_>>();
    slots.sort();
    slots.dedup();
    slots
}

/// `true` when a persist is allowed to write *now* (plan §5.7 `persist_pending`): not while the
/// pointer is down on dock chrome, since the divider position of an in-flight drag is not final.
///
/// The dock's own drag state lives in `DockOutput::group_drag`/resize responses; step 7 has no
/// divider drag yet, so the pointer press is the honest check — a held button means "later".
#[must_use]
pub fn persist_pending(ctx: &egui::Context, now: std::time::Instant) -> bool {
    let _ = now;
    !ctx.input(|i| i.pointer.any_pressed() || i.pointer.any_down())
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_ui::docking::DockError;

    #[test]
    fn default_layouts_validate_for_every_grid() {
        for mode in [LayoutMode::Single, LayoutMode::Split] {
            let layout = default_layout(mode);
            assert!(
                layout.validate().is_ok(),
                "{mode:?} default layout must validate"
            );
            assert_eq!(layout.panels().len(), panes_for(mode).len());
        }
        assert!(
            layout_for_slots(&quad_slots()).validate().is_ok(),
            "the quad tree must validate too (plan §5.7 Done-when)"
        );
    }

    #[test]
    fn every_group_is_a_single_protected_leaf() {
        let layout = layout_for_slots(&quad_slots());
        let Some(Node::Split { first, second, .. }) = layout.root.as_ref() else {
            panic!("quad root is a split");
        };
        for side in [first.as_ref(), second.as_ref()] {
            let Node::Split { first, second, .. } = side else {
                panic!("quad column is a split");
            };
            for leaf in [first.as_ref(), second.as_ref()] {
                let Node::Tabs { panels, .. } = leaf else {
                    panic!("quad leaf must be a group");
                };
                assert_eq!(panels.len(), 1, "one panel per group ⇒ stripless");
            }
        }
        assert!(pane_permissions().is_protected());
    }

    #[test]
    fn close_is_rejected_not_swallowed() {
        // Plan §5.7 Done-when: a `Close` on a protected pane is `DockError::NotAllowed` (or the
        // close-target equivalent), never silence.
        let mut dock = default_workspace(LayoutMode::Split);
        let result = dock.apply(
            craft_ui::docking::Action::Close {
                panel: DockId::FilePane(PaneSlot::Left),
            },
            perms_of,
        );
        assert!(
            result.is_err(),
            "a protected pane cannot be closed: {result:?}"
        );
        assert!(matches!(result.unwrap_err(), DockError::NotAllowed));
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let dock = default_workspace(LayoutMode::Split);
        let value = snapshot(&dock).expect("serialisable");
        let mut restored = Dock::default();
        let report = apply(
            &mut restored,
            &value,
            &|p| matches!(slot_of(p), PaneSlot::Left | PaneSlot::Right),
            &default_workspace(LayoutMode::Split),
        );
        assert_eq!(report, ApplyReport::Applied);
        assert_eq!(restored.layout, dock.layout);
    }

    #[test]
    fn a_newer_version_keeps_the_file_and_uses_the_default() {
        let mut value = snapshot(&default_workspace(LayoutMode::Single)).expect("serialisable");
        value["version"] = serde_json::json!(Dock::VERSION + 1);
        let mut dock = Dock::default();
        let report = apply(
            &mut dock,
            &value,
            &|_| true,
            &default_workspace(LayoutMode::Split),
        );
        assert_eq!(
            report,
            ApplyReport::NewerVersion {
                found: Dock::VERSION + 1
            }
        );
        assert_eq!(dock.layout, default_layout(LayoutMode::Split));
    }

    #[test]
    fn garbage_is_reported_invalid_and_the_default_wins() {
        for garbage in [
            serde_json::json!(null),
            serde_json::json!("not a dock"),
            serde_json::json!({"version": 2, "layout": {"root": {"Tabs": {"panels": [], "active": 9}}} }),
        ] {
            let mut dock = Dock::default();
            let report = apply(
                &mut dock,
                &garbage,
                &|_| true,
                &default_workspace(LayoutMode::Single),
            );
            assert_eq!(report, ApplyReport::Invalid, "{garbage} must be reported");
            assert_eq!(dock.layout, default_layout(LayoutMode::Single));
        }
    }

    #[test]
    fn set_layout_from_quad_keeps_surviving_panes_and_validates() {
        let mut model = crate::state::AppModel::new(LayoutMode::Split);
        let mut dock = Dock::new(model_grid(&model));
        set_layout(&mut model, &mut dock, quad_slots());
        assert!(model.quad());
        assert!(dock.layout.validate().is_ok(), "quad layout is valid");
        let mut ids = dock
            .layout
            .panels()
            .into_iter()
            .copied()
            .collect::<Vec<_>>();
        ids.sort_by_key(|p| format!("{p:?}"));
        assert_eq!(ids.len(), 4);
        // Back to single: the two extra panes vanish from the grid, the survivor keeps its tabs.
        let id = model.next_tab_id();
        model
            .pane_mut(PaneSlot::Left)
            .tabs
            .push(crate::state::Tab::new(
                id,
                zenman_core::path::FsPath::ROOT,
                "/",
            ));
        set_layout(&mut model, &mut dock, panes_for(LayoutMode::Single));
        assert!(model.layout_mode_is_consistent());
        assert_eq!(model.panes[&PaneSlot::Left].tabs.len(), 1);
    }

    fn model_grid(model: &crate::state::AppModel) -> Layout<DockId> {
        layout_for_slots(&model.grid_slots())
    }
}
