// SPDX-License-Identifier: GPL-3.0-or-later

mod blur;
mod browser;
mod browser_modes;
mod chooser;
mod controls;
mod entry_list_model;
mod inline_search;
mod loading_skeleton;
mod marquee;
mod motion;
mod preview;
mod scrolling;
mod search;
mod settings;
mod theme;
mod thumbnail;
mod thumbnail_cache;
mod window;

pub(crate) use chooser::{cancel_chooser, present_chooser};
pub(crate) use window::home_directory;
pub use window::{present, present_location, present_reveal};

pub(crate) fn prepare_portal_ui() {
    let _theme = theme::ThemeManager::shared();
    window::load_styles();
}
