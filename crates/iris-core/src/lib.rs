//! Headless media core: viewport sessions, semantic commands and state,
//! coordinate geometry, input policy, folder scanning, photo metadata,
//! settings and path-keyed records. No GPUI or native windowing types.

pub mod geometry;
pub mod icc;
pub mod input;
pub mod library;
pub mod logging;
pub mod photo;
pub mod player;
pub mod plugin;
pub mod settings;
pub mod store;
