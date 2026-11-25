//! Iris Wasm plugin protocol 2. Use `v2` for the versioned wire contract;
//! common UI and native media value types remain available at the crate root.
pub mod manifest;
pub mod overlay;
pub mod source;
pub mod v2;
pub mod workbench;
pub use manifest::{CommandSpec, SourceProviderSpec, is_valid_id};
pub use overlay::{OverlayNode, TextAlign};
pub use source::{
    MediaKind, PlaybackSnapshot, SourceDescriptor, SourceEntry, SourceEntryKind,
    SourceManageAction, SourcePage, StreamSpec, StreamVariant, SubtitleAttachment,
};
pub use workbench::{
    ButtonSpec, ChromeLayoutSpec, Color, PluginModal, SelectOption, SidebarPanel, SidebarSection,
    StatusBadge, ToolbarButton, UiThemeSpec, Widget, is_valid_hex_color,
};
/// Canonical package/control protocol. ABI has its own versioned constant.
pub const PROTOCOL_VERSION: u32 = 2;
