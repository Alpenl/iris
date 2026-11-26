use super::{Capability, Id};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Task {
    /// Looks up an already confirmed host Origin; never creates authority.
    OriginLookup {
        origin: String,
    },
    StateRead {
        key: String,
    },
    StateWrite {
        key: String,
        value: Option<String>,
    },
    SecretRead {
        bucket: CredentialBucket,
        key: String,
    },
    SecretWrite {
        bucket: CredentialBucket,
        key: String,
        value: Option<String>,
    },
    HttpStart {
        method: String,
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
        body_handle: Option<Id>,
        #[serde(default)]
        stream_body: bool,
        #[serde(default)]
        follow_redirects: bool,
        timeout_ms: Option<u32>,
        origin_grant: Option<Id>,
    },
    HttpRead {
        handle: Id,
        len: u32,
    },
    HttpWrite {
        handle: Id,
        blob: Id,
        finish: bool,
    },
    WsConnect {
        url: String,
        headers: Vec<(String, String)>,
        origin_grant: Option<Id>,
    },
    WsRead {
        handle: Id,
    },
    WsWrite {
        handle: Id,
        blob: Id,
        text: bool,
    },
    TcpConnect {
        host: String,
        port: u16,
        tls: bool,
        origin_grant: Option<Id>,
    },
    TcpRead {
        handle: Id,
        len: u32,
    },
    TcpWrite {
        handle: Id,
        blob: Id,
    },
    BeginAuth {
        callback_path: String,
        issuer: Option<String>,
    },
    OpenAuth {
        transaction: Id,
        url: String,
        origin_grant: Option<Id>,
    },
    HashCurrentMedia {
        media: Id,
        algorithm: FileHash,
    },
    ArtifactRead {
        handle: Id,
        offset: Id,
        len: u32,
    },
    ArtifactBegin {
        format: ArtifactFormat,
    },
    ArtifactWrite {
        handle: Id,
        blob: Id,
        finish: bool,
    },
}

impl Task {
    pub fn capability(&self) -> Option<Capability> {
        Some(match self {
            Self::StateRead { .. } | Self::StateWrite { .. } => return None,
            Self::SecretRead { .. } | Self::SecretWrite { .. } => Capability::Secrets,
            Self::OriginLookup { .. }
            | Self::HttpStart { .. }
            | Self::HttpRead { .. }
            | Self::HttpWrite { .. } => Capability::NetHttp,
            Self::WsConnect { .. } | Self::WsRead { .. } | Self::WsWrite { .. } => {
                Capability::NetWs
            }
            Self::TcpConnect { .. } | Self::TcpRead { .. } | Self::TcpWrite { .. } => {
                Capability::NetTcp
            }
            Self::BeginAuth { .. } | Self::OpenAuth { .. } => Capability::UiModal,
            Self::HashCurrentMedia { .. } => Capability::ReadState,
            Self::ArtifactRead { .. } | Self::ArtifactBegin { .. } | Self::ArtifactWrite { .. } => {
                Capability::MediaEnhance
            }
        })
    }
    pub fn authorized(&self, grants: &std::collections::BTreeSet<Capability>) -> bool {
        if matches!(self, Self::SecretRead { .. } | Self::SecretWrite { .. }) {
            grants.contains(&Capability::Secrets) || grants.contains(&Capability::MediaSource)
        } else if matches!(self, Self::OriginLookup { .. }) {
            grants.contains(&Capability::NetHttp)
                || grants.contains(&Capability::NetWs)
                || grants.contains(&Capability::NetTcp)
        } else {
            self.capability().is_none_or(|cap| grants.contains(&cap))
        }
    }

    pub fn allowed_in_stream(&self) -> bool {
        !matches!(self, Self::OriginLookup { .. })
            && matches!(
                self.capability(),
                Some(Capability::NetHttp | Capability::NetWs | Capability::NetTcp)
            )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialBucket {
    Plugin,
    Origin { origin_grant: Id },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    Wav,
    Flac,
    M4a,
    Ogg,
    SubRip,
    WebVtt,
    Ass,
    Cube,
    Text,
    Csv,
    Edl,
    Xmp,
}
impl ArtifactFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Ogg => "ogg",
            Self::SubRip => "srt",
            Self::WebVtt => "vtt",
            Self::Ass => "ass",
            Self::Cube => "cube",
            Self::Text => "txt",
            Self::Csv => "csv",
            Self::Edl => "edl",
            Self::Xmp => "xmp",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileHash {
    FullMd5,
    FullSha256,
    SparseSha256V1,
}

#[cfg(test)]
mod tests {
    use super::Task;
    use crate::v2::Capability;
    use std::collections::BTreeSet;

    #[test]
    fn origin_lookup_accepts_net_http_net_ws_or_net_tcp() {
        let task = Task::OriginLookup {
            origin: "wss://chat.example".into(),
        };
        assert!(task.authorized(&BTreeSet::from([Capability::NetHttp])));
        assert!(task.authorized(&BTreeSet::from([Capability::NetWs])));
        assert!(task.authorized(&BTreeSet::from([Capability::NetTcp])));
        assert!(!task.authorized(&BTreeSet::from([Capability::UiModal])));
        assert!(!task.allowed_in_stream());
    }
}
