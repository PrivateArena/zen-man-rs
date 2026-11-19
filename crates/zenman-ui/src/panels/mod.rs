//! Panes and chrome (plan §5.8). Steps 8 owns `file_pane`, `tab_bar`, `breadcrumb`, `status_bar`;
//! `header` came with step 7's shell, `action_log` with step 14, and `side_panel` grew its real
//! content (places, favourites, disks) in step 15b.

pub mod action_log;
pub mod breadcrumb;
pub mod file_pane;
pub mod header;
pub mod side_panel;
pub mod status_bar;
pub mod tab_bar;
