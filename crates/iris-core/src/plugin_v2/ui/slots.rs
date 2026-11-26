use crate::plugin_v2::{actor::Update, hub::Binding};
use iris_api::{
    ChromeLayoutSpec, OverlayNode, PluginModal, SidebarPanel, StatusBadge, ToolbarButton,
    UiThemeSpec,
    v2::{Capability, HostCommand},
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtySlots {
    pub sidebar: bool,
    pub status: bool,
    pub toolbar: bool,
    pub overlay: bool,
    pub theme: bool,
    pub modal: bool,
    pub sources: bool,
}
impl DirtySlots {
    pub const fn all() -> Self {
        Self {
            sidebar: true,
            status: true,
            toolbar: true,
            overlay: true,
            theme: true,
            modal: true,
            sources: true,
        }
    }
    pub fn merge(&mut self, o: Self) {
        self.sidebar |= o.sidebar;
        self.status |= o.status;
        self.toolbar |= o.toolbar;
        self.overlay |= o.overlay;
        self.theme |= o.theme;
        self.modal |= o.modal;
        self.sources |= o.sources;
    }
    pub const fn should_notify_ui(self, fullscreen: bool, chrome: bool, sidebar: bool) -> bool {
        if fullscreen && !chrome && !sidebar {
            self.overlay || self.modal || self.theme
        } else {
            self.overlay
                || self.modal
                || self.theme
                || self.sources
                || self.status
                || self.toolbar
                || (self.sidebar && sidebar)
        }
    }
}
struct Slot {
    update: Arc<Update>,
    index: usize,
}
impl Slot {
    fn command(&self) -> &HostCommand {
        &self.update.commands[self.index]
    }
}
pub(super) struct Slots {
    pub binding: Arc<Binding>,
    sidebar: Option<Slot>,
    status: Option<Slot>,
    toolbar: Option<Slot>,
    overlay: Option<Slot>,
    modal: Option<Slot>,
    theme: Option<Slot>,
    pub modal_order: u64,
}
impl Slots {
    pub fn new(binding: Arc<Binding>) -> Self {
        Self {
            binding,
            sidebar: None,
            status: None,
            toolbar: None,
            overlay: None,
            modal: None,
            theme: None,
            modal_order: 0,
        }
    }
    pub fn sidebar(&self) -> Option<&SidebarPanel> {
        match self.sidebar.as_ref()?.command() {
            HostCommand::SetSidebar { panel } => panel.as_deref(),
            _ => None,
        }
    }
    pub fn status(&self) -> Option<&StatusBadge> {
        match self.status.as_ref()?.command() {
            HostCommand::SetStatus { badge } => badge.as_deref(),
            _ => None,
        }
    }
    pub fn toolbar(&self) -> &[ToolbarButton] {
        match self.toolbar.as_ref().map(Slot::command) {
            Some(HostCommand::SetToolbar { buttons }) => buttons,
            _ => &[],
        }
    }
    pub fn overlay(&self) -> &[OverlayNode] {
        match self.overlay.as_ref().map(Slot::command) {
            Some(HostCommand::SetOverlay { nodes }) => nodes,
            _ => &[],
        }
    }
    pub fn modal(&self) -> Option<&PluginModal> {
        match self.modal.as_ref()?.command() {
            HostCommand::OpenModal { modal } => Some(modal),
            _ => None,
        }
    }
    pub fn theme(&self) -> Option<&UiThemeSpec> {
        if !self.binding.installed.grants.contains(&Capability::UiTheme) {
            return None;
        }
        match self.theme.as_ref().map(Slot::command) {
            Some(HostCommand::SetTheme { theme, .. }) => theme.as_deref(),
            _ => self.binding.manifest.theme.as_ref(),
        }
    }
    pub fn chrome(&self) -> Option<&ChromeLayoutSpec> {
        if !self.binding.installed.grants.contains(&Capability::UiTheme) {
            return None;
        }
        match self.theme.as_ref().map(Slot::command) {
            Some(HostCommand::SetTheme { chrome, .. }) => chrome.as_ref(),
            _ => self.binding.manifest.chrome.as_ref(),
        }
    }
    pub fn dismiss_modal(&mut self) {
        self.modal = None;
    }
    pub fn apply(
        &mut self,
        update: &Arc<Update>,
        index: usize,
        order: u64,
        dirty: &mut DirtySlots,
    ) -> bool {
        let slot = || Slot {
            update: update.clone(),
            index,
        };
        match &update.commands[index] {
            HostCommand::SetSidebar { panel } => {
                if self.sidebar() != panel.as_deref() {
                    dirty.sidebar = true;
                    self.sidebar = panel.as_ref().map(|_| slot());
                }
            }
            HostCommand::SetStatus { badge } => {
                if self.status() != badge.as_deref() {
                    dirty.status = true;
                    self.status = badge.as_ref().map(|_| slot());
                }
            }
            HostCommand::SetToolbar { buttons } => {
                if self.toolbar() != buttons {
                    dirty.toolbar = true;
                    self.toolbar = (!buttons.is_empty()).then(slot);
                }
            }
            HostCommand::SetOverlay { nodes } => {
                if self.overlay() != nodes {
                    dirty.overlay = true;
                    self.overlay = (!nodes.is_empty()).then(slot);
                }
            }
            HostCommand::OpenModal { modal } => {
                if self.modal() != Some(modal) {
                    dirty.modal = true;
                    self.modal = Some(slot());
                    self.modal_order = order;
                }
            }
            HostCommand::CloseModal { id } => {
                if self
                    .modal()
                    .is_some_and(|modal| id.as_ref().is_none_or(|id| *id == modal.id))
                {
                    dirty.modal = true;
                    self.modal = None;
                }
            }
            HostCommand::SetTheme { theme, chrome } => {
                dirty.theme |= self.theme() != theme.as_deref() || self.chrome() != chrome.as_ref();
                self.theme = Some(slot());
            }
            _ => return false,
        }
        true
    }
}
