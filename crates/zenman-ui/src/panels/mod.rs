//! Panes and chrome (plan §5.8). Steps 8 owns `file_pane`, `tab_bar`, `breadcrumb`, `status_bar`;
//! `header` and `side_panel` came with step 7's shell, `action_log` arrives with step 14.

pub mod breadcrumb;
pub mod file_pane;
pub mod status_bar;
pub mod tab_bar;
