use super::{Capability, Id};
use crate::{SourceEntryKind, StreamVariant};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteResource {
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub origin_grant: Option<Id>,
}

/// Immutable context bytes and the exact private-origin capabilities copied into
/// a new StreamStore. The host intersects requested transports with installation grants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamContext {
    pub version: Id,
    pub blob: Id,
    #[serde(default)]
    pub origin_grants: Vec<Id>,
    #[serde(default)]
    pub transports: Vec<Capability>,
    pub expires_unix_ms: Option<Id>,
}

/// Optional proof required before splicing bytes from fallback representations.
/// `hashes` contains consecutive 32-byte SHA-256 hashes of fixed-size plaintext
/// chunks (the last may be shorter), not plugin-provided ETag assertions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteIdentity {
    pub size: Id,
    pub chunk_bytes: u32,
    pub hashes: Id,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamSource {
    DirectHttp {
        primary: RemoteResource,
        #[serde(default)]
        fallback_urls: Vec<RemoteResource>,
        identity: Option<ByteIdentity>,
    },
    RandomAccess {
        resource_id: String,
        size: Option<Id>,
        context: StreamContext,
    },
    VirtualManifest {
        format: ManifestFormat,
        document: Id,
        context: Option<StreamContext>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFormat {
    VirtualHls,
    VirtualDash,
    RestrictedHls,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSpec {
    pub source_id: Option<String>,
    pub entry_id: Option<String>,
    pub title: Option<String>,
    pub kind: Option<SourceEntryKind>,
    #[serde(default)]
    pub is_live: bool,
    pub source: StreamSource,
    #[serde(default)]
    pub variants: Vec<StreamVariant>,
    pub active_variant: Option<String>,
    #[serde(default)]
    pub audio_tracks: Vec<Attachment>,
    #[serde(default)]
    pub subtitles: Vec<Attachment>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: String,
    pub label: String,
    pub lang: Option<String>,
    pub resource: RemoteResource,
    #[serde(default)]
    pub default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Manifest,
    Init,
    Index,
    Media,
    Subtitle,
    Key,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    pub offset: Id,
    pub len: Id,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSpan {
    pub resource_id: String,
    pub range: Option<ByteRange>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceSource {
    Remote { request: RemoteResource },
    Guest { resource_id: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceBinding {
    pub id: String,
    pub kind: ResourceKind,
    pub extension: ResourceExtension,
    pub size: Option<Id>,
    pub source: ResourceSource,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceExtension {
    Mp4,
    M4a,
    M4s,
    Ts,
    Aac,
    Webm,
    Vtt,
    Srt,
    Ass,
    Key,
    M3u8,
    Mpd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HlsKey {
    pub resource_id: String,
    /// Hexadecimal 16-byte IV, always explicit.
    pub iv: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HlsSegment {
    pub resource: ResourceSpan,
    pub duration_ms: Id,
    pub init: Option<ResourceSpan>,
    pub key: Option<HlsKey>,
    #[serde(default)]
    pub discontinuity: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HlsTrack {
    pub id: String,
    pub kind: TrackKind,
    #[serde(default)]
    pub group: Option<String>,
    pub label: String,
    pub lang: Option<String>,
    pub codec: Option<String>,
    #[serde(default)]
    pub default: bool,
    pub sequence: Id,
    #[serde(default)]
    pub discontinuity_sequence: Id,
    pub segments: Vec<HlsSegment>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HlsVariant {
    pub video: String,
    pub audio: Option<String>,
    pub bandwidth: Id,
    pub width: Option<u32>,
    pub height: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualHls {
    pub resources: Vec<ResourceBinding>,
    pub tracks: Vec<HlsTrack>,
    #[serde(default)]
    pub variants: Vec<HlsVariant>,
    #[serde(default)]
    pub live: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictedHls {
    pub resources: Vec<ResourceBinding>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashSegment {
    pub resource: ResourceSpan,
    pub time: Id,
    pub duration: Id,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashTimeline {
    pub time: Option<Id>,
    pub duration: Id,
    #[serde(default)]
    pub repeat: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "addressing", rename_all = "snake_case", deny_unknown_fields)]
pub enum DashAddressing {
    SegmentBase {
        media: ResourceSpan,
        index: ByteRange,
    },
    SegmentList {
        segments: Vec<DashSegment>,
    },
    /// Only $Number$ or $Time$ in a logical resource ID; no remote URL syntax.
    SegmentTemplate {
        resource_template: String,
        start_number: Id,
        timeline: Vec<DashTimeline>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashTrack {
    pub id: String,
    pub kind: TrackKind,
    pub codec: String,
    pub mime_type: String,
    pub bandwidth: Id,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
    pub lang: Option<String>,
    pub timescale: u32,
    #[serde(default)]
    pub presentation_time_offset: Id,
    pub init: Option<ResourceSpan>,
    pub segments: DashAddressing,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualDash {
    pub resources: Vec<ResourceBinding>,
    pub duration_ms: Id,
    pub tracks: Vec<DashTrack>,
}
