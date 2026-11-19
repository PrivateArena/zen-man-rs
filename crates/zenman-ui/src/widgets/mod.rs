//! Shell widgets (plan §5.8): the two step-7 ones (the toast stack, the disk meter) and the two
//! step-8 ones (the virtual scroller, the directory row). `modals` arrives with step 10.

pub mod disk_meter;
pub mod entry_row;
pub mod toast;
pub mod virtual_list;

use egui::{Layout, Rect, Ui, UiBuilder};

/// Run `body` in a child ui whose box is exactly `rect`, leaving the parent's cursor alone (15c
/// D2/D7/D12: the band and table layouts place their groups into reserved cells).
///
/// This egui (0.37) has no `Ui::allocate_ui_at_rect`; `UiBuilder::max_rect` + `scope_builder` is the
/// same thing, and the caller reserves the band itself with [`Ui::advance_cursor_after_rect`] — the
/// step-8 discipline of reserving the height *before* the container that fills it.
pub(crate) fn in_rect(ui: &mut Ui, rect: Rect, layout: Layout, body: impl FnOnce(&mut Ui)) {
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(layout), |ui| {
        body(ui);
    });
}
