//! Panes and chrome (plan §5.8). Steps 8 owns `file_pane`, `tab_bar`, `breadcrumb`, `status_bar`;
//! `header` came with step 7's shell, `nav_bar` is step 8's address strip and toolbar grown into
//! one row, `app_menu` is that bar's second level, `action_log` came with step 14, `side_panel`
//! grew its real content (places, favourites, disks) in step 15b, and `switcher` is the recent
//! tab / tab-group panel of `docs/tab-switcher-design.md`.

pub mod action_log;
pub mod app_menu;
pub mod breadcrumb;
pub mod file_pane;
pub mod header;
pub mod nav_bar;
pub mod side_panel;
pub mod status_bar;
pub mod switcher;
pub mod tab_bar;
