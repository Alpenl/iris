//! Headless media core: viewport sessions, semantic commands and state,
//! coordinate geometry, input policy, folder scanning, photo metadata,
//! unified media URIs, credential vault, catalog & play queue, workspace presets,
//! keymaps, settings, and URI-keyed records. No GPUI or native windowing types.

pub mod bookmarks;
pub mod bounded_json;
pub mod catalog;
pub mod fs_util;
pub mod geometry;
pub mod input;
pub mod keymap;
pub mod legacy;
pub mod library;
pub mod logging;
pub mod media_uri;
pub mod photo;
pub mod player;
pub mod plugin_ids;
pub mod preset;
pub mod settings;
pub mod store;
pub mod vault;

#[cfg(test)]
mod test_util;
