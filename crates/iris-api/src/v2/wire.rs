use super::{
    Capability, Generation, Id, Role,
    enhance::{AnnotationEdit, Chapter, EnhancementResource, OverlayWindow, SubtitleFormat},
    source::{PlaybackSnapshot, ResolvedMedia, SourceDescriptor, SourcePage},
    stream::{ManifestFormat, StreamContext},
};
use crate::{
    ChromeLayoutSpec, OverlayNode, PluginModal, SidebarPanel, SourceManageAction, StatusBadge,
    ToolbarButton, UiThemeSpec,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// T is the versioned completed-task result. Hosts serialize only results defined
/// by the submitted operation; guests may use task_result::TaskResult.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostEvent<T> {
    /// ControlStore multiplexes windows; replies and asynchronous tasks inherit
    /// this host scope. StreamStore events remain unwrapped.
    Window {
        window: Id,
        generation: Generation,
        body: Box<HostEvent<T>>,
    },
    WindowOpened {
        snapshot: Option<PlaybackSnapshot>,
    },
    WindowClosed {
        window: Id,
    },
    Init {
        protocol: u32,
        abi: u32,
        host_version: String,
        role: Role,
        generation: Generation,
        capabilities: BTreeSet<Capability>,
        features: BTreeSet<String>,
        snapshot: Option<PlaybackSnapshot>,
    },
    MediaOpened {
        generation: Generation,
        snapshot: PlaybackSnapshot,
    },
    StateChanged {
        generation: Generation,
        snapshot: PlaybackSnapshot,
    },
    Tick {
        generation: Generation,
        position: f64,
        duration: f64,
        paused: bool,
    },
    Command {
        id: String,
        snapshot: Option<PlaybackSnapshot>,
    },
    Action {
        id: String,
        value: Option<String>,
        snapshot: Option<PlaybackSnapshot>,
    },
    ModalSubmit {
        id: String,
        values: BTreeMap<String, String>,
    },
    ModalCancel {
        id: String,
    },
    SourceBrowse {
        request_id: Id,
        source_id: String,
        folder_id: Option<String>,
        page: Option<String>,
        query: Option<String>,
        #[serde(default)]
        filters: BTreeMap<String, String>,
    },
    SourceResolve {
        request_id: Id,
        source_id: String,
        folder_id: Option<String>,
        entry_id: String,
        variant: Option<String>,
    },
    SourceManage {
        source_id: String,
        action: SourceManageAction,
    },
    ResolveUrl {
        request_id: Id,
        url: String,
    },
    RequestCancelled {
        request_id: Id,
    },
    Completed {
        operation_id: Id,
        generation: Generation,
        result: T,
    },
    StreamInit {
        session: Id,
        generation: Generation,
        context_blob: Id,
    },
    StreamRead {
        request_id: Id,
        generation: Generation,
        resource_id: String,
        offset: Id,
        len: u32,
    },
    StreamReadCancelled {
        request_id: Id,
        generation: Generation,
    },
    StreamContextChanged {
        session: Id,
        generation: Generation,
        context_blob: Id,
    },
    StreamRefreshRequested {
        session: Id,
        generation: Generation,
        reason: String,
    },
    /// Receipt for host admission or an atomic stream update. The outer Window
    /// scope uses the control generation; this is the independent stream epoch.
    StreamStatus {
        request_id: Option<Id>,
        session: Id,
        generation: Generation,
        manifest_revision: Id,
        context_version: Id,
    },
    OverlayWindowChanged {
        media: Id,
        generation: Generation,
        window: OverlayWindow,
    },
    SelectionGranted {
        selection: Id,
        media: Id,
        count: u32,
    },
    AnnotationResult {
        request_id: Id,
        selection: Id,
        result: super::Reply<super::enhance::AnnotationPage>,
    },
    OriginGranted {
        request_id: Id,
        origin: String,
        grant: Option<Id>,
        insecure_auth: bool,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostCommand {
    TogglePlayback,
    SeekRelative {
        seconds: f64,
    },
    SeekAbsolute {
        seconds: f64,
    },
    SetSpeed {
        speed: f64,
    },
    SetVolume {
        volume: f64,
    },
    ToggleMute,
    OpenNext,
    OpenPrevious,
    ToggleFavorite,
    SetAbLoop {
        start: f64,
        end: f64,
    },
    ClearAbLoop,
    ShowToast {
        message: String,
    },
    SetSidebar {
        panel: Option<Box<SidebarPanel>>,
    },
    SetStatus {
        badge: Option<Box<StatusBadge>>,
    },
    SetOverlay {
        #[serde(default)]
        nodes: Vec<OverlayNode>,
    },
    SetToolbar {
        #[serde(default)]
        buttons: Vec<ToolbarButton>,
    },
    OpenModal {
        modal: Box<PluginModal>,
    },
    CloseModal {
        id: Option<String>,
    },
    SetTheme {
        theme: Option<Box<UiThemeSpec>>,
        chrome: Option<ChromeLayoutSpec>,
    },
    SetSources {
        #[serde(default)]
        sources: Vec<SourceDescriptor>,
    },
    RefreshSource {
        source_id: String,
    },
    ProvideSourcePage {
        request_id: Id,
        page: Box<SourcePage>,
    },
    /// A resolution always answers a live host request. Opening an unsolicited
    /// remote URI cannot be disguised as a source response.
    ResolveStream {
        request_id: Id,
        media: Box<ResolvedMedia>,
    },
    SourceError {
        request_id: Id,
        message: String,
    },
    UpdateStreamContext {
        session: Id,
        expected_version: Id,
        context: StreamContext,
    },
    UpdateManifest {
        session: Id,
        expected_revision: Id,
        format: ManifestFormat,
        document: Id,
    },
    AttachSubtitle {
        media: Id,
        generation: Generation,
        id: String,
        label: String,
        lang: Option<String>,
        format: SubtitleFormat,
        resource: EnhancementResource,
    },
    AttachAudioTrack {
        media: Id,
        generation: Generation,
        id: String,
        label: String,
        lang: Option<String>,
        resource: EnhancementResource,
    },
    SetMonitorLut {
        media: Id,
        generation: Generation,
        artifact: Option<Id>,
    },
    SetTimelineMeta {
        media: Id,
        generation: Generation,
        chapters: Vec<Chapter>,
        #[serde(default)]
        heat: Vec<super::enhance::HeatRange>,
        #[serde(default)]
        skip: Vec<super::enhance::SkipRange>,
    },
    /// `cues` is a Blob containing strict JSON Vec<TimedCue>. Each transfer is
    /// bounded by CHUNK_LIMIT; total residency is independently limited.
    SetTimedOverlay {
        media: Id,
        generation: Generation,
        track: String,
        window_revision: Id,
        cues: Id,
    },
    AppendTimedCues {
        media: Id,
        generation: Generation,
        track: String,
        window_revision: Id,
        cues: Id,
    },
    ClearTimedOverlay {
        media: Id,
        generation: Generation,
        track: String,
    },
    EditAnnotation {
        request_id: Id,
        selection: Id,
        edit: AnnotationEdit,
    },
    ReadAnnotation {
        request_id: Id,
        selection: Id,
        index: u32,
        marker_offset: u32,
    },
    /// A separate host-owned confirmation sheet, never a plugin-provided form.
    RequestOrigin {
        request_id: Id,
        origin: String,
    },
    /// Structured diagnostic log emitted to host logs (redacted and rate-limited).
    Log {
        level: LogLevel,
        message: String,
    },
}
impl HostCommand {
    pub fn authorized(&self, grants: &BTreeSet<Capability>) -> bool {
        match self {
            Self::RequestOrigin { .. } => {
                grants.contains(&Capability::UiModal) && grants.contains(&Capability::NetHttp)
            }
            Self::ResolveStream { .. }
            | Self::SourceError { .. }
            | Self::UpdateStreamContext { .. }
            | Self::UpdateManifest { .. } => {
                grants.contains(&Capability::MediaSource)
                    || grants.contains(&Capability::UrlResolver)
            }
            _ => self
                .required_capability()
                .is_none_or(|cap| grants.contains(&cap)),
        }
    }
    pub fn required_capability(&self) -> Option<Capability> {
        Some(match self {
            Self::TogglePlayback
            | Self::SeekRelative { .. }
            | Self::SeekAbsolute { .. }
            | Self::SetSpeed { .. }
            | Self::SetVolume { .. }
            | Self::ToggleMute
            | Self::OpenNext
            | Self::OpenPrevious
            | Self::ToggleFavorite
            | Self::SetAbLoop { .. }
            | Self::ClearAbLoop => Capability::ControlPlayback,
            Self::ShowToast { .. } => Capability::Notifications,
            Self::SetSidebar { .. } => Capability::UiSidebar,
            Self::SetStatus { .. } => Capability::UiStatus,
            Self::SetOverlay { .. }
            | Self::SetTimedOverlay { .. }
            | Self::AppendTimedCues { .. }
            | Self::ClearTimedOverlay { .. } => Capability::UiOverlay,
            Self::SetToolbar { .. } => Capability::UiToolbar,
            Self::OpenModal { .. } | Self::CloseModal { .. } | Self::RequestOrigin { .. } => {
                Capability::UiModal
            }
            Self::SetTheme { .. } => Capability::UiTheme,
            Self::SetSources { .. }
            | Self::RefreshSource { .. }
            | Self::ProvideSourcePage { .. }
            | Self::ResolveStream { .. }
            | Self::SourceError { .. }
            | Self::UpdateStreamContext { .. }
            | Self::UpdateManifest { .. } => Capability::MediaSource,
            Self::AttachSubtitle { .. }
            | Self::AttachAudioTrack { .. }
            | Self::SetMonitorLut { .. }
            | Self::SetTimelineMeta { .. } => Capability::MediaEnhance,
            Self::EditAnnotation { .. } | Self::ReadAnnotation { .. } => Capability::Annotations,
            Self::Log { .. } => return None,
        })
    }

    /// The host transfers only explicitly named Blob capabilities out of the
    /// returning Store; referenced files/artifacts/session handles are separate.
    pub fn blobs(&self) -> Vec<Id> {
        use super::stream::StreamSource;
        let mut out = Vec::new();
        match self {
            Self::ResolveStream { media, .. } => match &media.stream.source {
                StreamSource::DirectHttp { identity, .. } => {
                    if let Some(identity) = identity {
                        out.push(identity.hashes);
                    }
                }
                StreamSource::RandomAccess { context, .. } => out.push(context.blob),
                StreamSource::VirtualManifest {
                    document, context, ..
                } => {
                    out.push(*document);
                    if let Some(context) = context {
                        out.push(context.blob);
                    }
                }
            },
            Self::UpdateStreamContext { context, .. } => out.push(context.blob),
            Self::UpdateManifest { document, .. } => out.push(*document),
            Self::SetTimedOverlay { cues, .. } | Self::AppendTimedCues { cues, .. } => {
                out.push(*cues)
            }
            _ => {}
        }
        out
    }
}
