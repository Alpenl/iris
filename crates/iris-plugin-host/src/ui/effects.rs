//! Typed [`WindowAction`] classification and ownership-complete [`ReadyEffect`] handles
//! surfaced by [`super::WindowSession`].
use super::{Event, PollReport};
use crate::{
    actor::Update, enhancement::Attachment, hub::Binding, ledger::Reservation, lut::MonitorLut,
    overlay::OverlayFrame,
};
use iris_api::v2::{
    ErrorCode, Generation, HostCommand,
    enhance::{HeatRange, SkipRange},
};
use iris_core::{
    player::{Chapter, Command as PlayerCommand, ExternalTrack, MonitorLutFilter},
    store::AnnotationSnapshot,
};
use std::{sync::Arc, time::Duration};

pub const POLL_NOTICE_LIMIT: usize = 16;
pub const POLL_ACTION_LIMIT: usize = 32;
pub const POLL_EFFECT_LIMIT: usize = 16;
pub const REMOTE_THUMB_SCRATCH_BYTES: u64 = 4 << 20;
pub const REMOTE_THUMB_LEASE_BYTES: u64 = 128 << 10;

/// Opaque enhancement command handle retaining the decoded `Update` and `Blob`
/// reservations until [`super::WindowSession::execute_enhancement`] runs.
pub struct EnhancementRequest {
    pub(super) binding: Arc<Binding>,
    pub(super) update: Arc<Update>,
    pub(super) index: usize,
}

impl EnhancementRequest {
    pub(super) fn command(&self) -> &HostCommand {
        &self.update.commands[self.index]
    }
}

/// Application-facing typed plugin action surfaced by [`super::WindowSession::poll`].
pub enum ActionKind {
    Player(PlayerCommand),
    StepQueue { delta: isize },
    ToggleFavorite,
    ShowToast { message: String },
    RefreshSource { source_id: String },
    Enhancement(EnhancementRequest),
}

/// Scoped action bound to a live plugin installation and window [`Generation`].
pub struct WindowAction {
    plugin_id: String,
    owner: String,
    generation: Generation,
    window_scoped: bool,
    binding: Arc<Binding>,
    pub kind: ActionKind,
}

impl WindowAction {
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn is_live(&self, current: Generation) -> bool {
        self.binding.is_live() && (!self.window_scoped || self.generation == current)
    }
}

pub(super) fn classify_command(
    binding: &Arc<Binding>,
    update: &Arc<Update>,
    index: usize,
) -> Option<WindowAction> {
    let kind = match &update.commands[index] {
        HostCommand::TogglePlayback => ActionKind::Player(PlayerCommand::TogglePlayback),
        HostCommand::SeekRelative { seconds } => {
            ActionKind::Player(PlayerCommand::SeekRelative(*seconds))
        }
        HostCommand::SeekAbsolute { seconds } => {
            ActionKind::Player(PlayerCommand::SeekAbsolute(*seconds))
        }
        HostCommand::SetSpeed { speed } => ActionKind::Player(PlayerCommand::SetSpeed(*speed)),
        HostCommand::SetVolume { volume } => ActionKind::Player(PlayerCommand::SetVolume(*volume)),
        HostCommand::ToggleMute => ActionKind::Player(PlayerCommand::ToggleMute),
        HostCommand::SetAbLoop { start, end } => ActionKind::Player(PlayerCommand::SetAbLoop {
            start: *start,
            end: *end,
        }),
        HostCommand::ClearAbLoop => ActionKind::Player(PlayerCommand::ClearAbLoop),
        HostCommand::OpenNext => ActionKind::StepQueue { delta: 1 },
        HostCommand::OpenPrevious => ActionKind::StepQueue { delta: -1 },
        HostCommand::ToggleFavorite => ActionKind::ToggleFavorite,
        HostCommand::ShowToast { message } => ActionKind::ShowToast {
            message: message.clone(),
        },
        HostCommand::RefreshSource { source_id } => ActionKind::RefreshSource {
            source_id: source_id.clone(),
        },
        HostCommand::SetTimelineMeta { .. }
        | HostCommand::SetMonitorLut { .. }
        | HostCommand::AttachSubtitle { .. }
        | HostCommand::AttachAudioTrack { .. }
        | HostCommand::SetTimedOverlay { .. }
        | HostCommand::AppendTimedCues { .. }
        | HostCommand::ClearTimedOverlay { .. }
        | HostCommand::UpdateManifest { .. }
        | HostCommand::UpdateStreamContext { .. }
        | HostCommand::EditAnnotation { .. }
        | HostCommand::ReadAnnotation { .. } => ActionKind::Enhancement(EnhancementRequest {
            binding: binding.clone(),
            update: update.clone(),
            index,
        }),
        HostCommand::SetSidebar { .. }
        | HostCommand::SetStatus { .. }
        | HostCommand::SetOverlay { .. }
        | HostCommand::SetToolbar { .. }
        | HostCommand::OpenModal { .. }
        | HostCommand::CloseModal { .. }
        | HostCommand::SetTheme { .. }
        | HostCommand::SetSources { .. }
        | HostCommand::ProvideSourcePage { .. }
        | HostCommand::ResolveStream { .. }
        | HostCommand::SourceError { .. }
        | HostCommand::RequestOrigin { .. }
        | HostCommand::Log { .. } => return None,
    };
    Some(WindowAction {
        plugin_id: binding.installed.plugin_id.clone(),
        owner: binding.installed.installation_id.clone(),
        generation: update.generation,
        window_scoped: update.window.is_some(),
        binding: binding.clone(),
        kind,
    })
}

/// Ownership-preserving handle for plugin timeline chapters, heat, and skip ranges.
pub struct TimelineLease {
    pub(super) binding: Arc<Binding>,
    pub(super) update: Arc<Update>,
    pub(super) index: usize,
}

impl TimelineLease {
    pub fn owner(&self) -> &str {
        &self.binding.installed.installation_id
    }

    pub fn plugin_id(&self) -> &str {
        &self.binding.installed.plugin_id
    }

    pub fn is_live(&self) -> bool {
        self.binding.is_live() && !self.update.is_cancelled()
    }

    pub fn chapters(&self) -> Vec<Chapter> {
        match &self.update.commands[self.index] {
            HostCommand::SetTimelineMeta { chapters, .. } => chapters
                .iter()
                .map(|c| Chapter {
                    time: c.start_ms.0 as f64 / 1000.,
                    title: c.title.clone(),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn ranges(&self) -> Option<(&[HeatRange], &[SkipRange])> {
        if !self.is_live() {
            return None;
        }
        match &self.update.commands[self.index] {
            HostCommand::SetTimelineMeta { heat, skip, .. } => Some((heat, skip)),
            _ => None,
        }
    }
}

/// Ownership-preserving handle for a prepared subtitle or audio attachment.
/// Dropping without applying unregisters the loopback route and frees `Ledger` quotas.
pub struct ReadyAttachment {
    pub(super) track: Arc<Attachment>,
    pub(super) _worker_lease: Reservation,
}

impl ReadyAttachment {
    pub fn into_external_track(self) -> Arc<dyn ExternalTrack> {
        self.track
    }
}

/// Ownership-preserving handle for a validated `.cube` monitor LUT artifact.
/// Dropping without applying releases the `Artifact` and schedules temp-file cleanup.
pub struct ReadyMonitorLut {
    pub(super) owner: String,
    pub(super) lut: Arc<MonitorLut>,
    pub(super) _worker_lease: Reservation,
}

impl ReadyMonitorLut {
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn into_filter(self) -> (String, Arc<dyn MonitorLutFilter>) {
        (self.owner, self.lut)
    }
}

/// Ownership-preserving handle for a completed timed-overlay bank mutation.
pub struct ReadyOverlayUpdate {
    pub(super) _worker_lease: Reservation,
}

/// Ownership-preserving handle for a completed `StreamSession` manifest/context update.
pub struct ReadyStreamUpdate {
    pub plugin_id: String,
    pub(super) _worker_lease: Reservation,
}

/// Ownership-preserving handle for a completed plugin annotation read/edit transaction.
pub struct ReadyAnnotations {
    pub plugin_id: String,
    pub snapshot: Option<AnnotationSnapshot>,
    pub event: Event,
    pub(super) _worker_lease: Reservation,
}

/// Ownership-complete background/immediate effect surfaced by [`super::WindowSession`].
/// Never degrades capabilities into raw file paths or raw URL strings.
pub enum ReadyEffect {
    Timeline(TimelineLease),
    Attachment(ReadyAttachment),
    MonitorLut(ReadyMonitorLut),
    ClearMonitorLut { owner: String },
    TimedOverlay(ReadyOverlayUpdate),
    StreamUpdated(ReadyStreamUpdate),
    Annotations(Box<ReadyAnnotations>),
    Failed { plugin_id: String, code: ErrorCode },
}

/// Decoded remote thumbnail paired with its `HostBytes` cache reservation (`None` images
/// release the reservation immediately upon construction).
pub struct RetainedThumbnail<T> {
    pub image: Option<T>,
    _lease: Option<Reservation>,
}

impl<T> RetainedThumbnail<T> {
    pub(crate) fn new(image: Option<T>, lease: Reservation) -> Self {
        let lease = image.is_some().then_some(lease);
        Self {
            image,
            _lease: lease,
        }
    }
}

/// Synchronous result of [`super::WindowSession::execute_enhancement`].
pub enum EnhancementOutcome {
    Queued,
    Immediate(ReadyEffect),
    Noop,
    Rejected {
        code: ErrorCode,
        superseded: bool,
        report: PollReport,
    },
}

/// Summary of revoked dead-plugin enhancements returned by [`super::WindowSession::reconcile_enhancements`].
#[derive(Default)]
pub struct RevokedEnhancements {
    pub revoked_owners: Vec<String>,
    pub clear_timeline: bool,
    pub clear_lut: bool,
}

/// Outcome of advancing timed overlays in [`super::WindowSession::step_overlay_frame`].
pub enum OverlayStep {
    Empty,
    Busy {
        retry_after: Option<Duration>,
    },
    Updated {
        frame: Option<Arc<OverlayFrame>>,
        reports: Vec<PollReport>,
    },
}

pub(super) fn is_superseded_error(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::StaleHandle | ErrorCode::Cancelled | ErrorCode::OutOfWindow
    )
}
