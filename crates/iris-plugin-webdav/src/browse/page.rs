use super::super::{
    media::Kind,
    protocol::{self, WebDavConnection, WebDavEntry, WebDavFolder, WebDavListResult},
};
use iris_plugin_sdk::api::{
    SourceEntryKind,
    v2::{
        ErrorCode, HostCommand, Id, WireU64,
        source::{PlaylistEntry, ResolvedMedia, SourceEntry, SourcePage},
        stream::{Attachment, RemoteResource, StreamSource, StreamSpec},
    },
};
use std::rc::Rc;

pub(crate) struct Cache {
    pub conn: WebDavConnection,
    pub listing: Rc<WebDavListResult>,
    serial: Id,
    query: Option<String>,
}
impl Cache {
    pub fn new(
        conn: WebDavConnection,
        listing: Rc<WebDavListResult>,
        serial: Id,
        query: Option<String>,
    ) -> Result<Self, ErrorCode> {
        let bytes = listing
            .subfolders
            .iter()
            .map(|e| e.path.len() + e.name.len() + 128)
            .sum::<usize>()
            + listing
                .entries
                .iter()
                .map(|e| e.path.len() + e.name.len() + 128)
                .sum::<usize>()
            + listing
                .subtitles
                .iter()
                .map(|e| e.path.len() + e.name.len() + 128)
                .sum::<usize>();
        if bytes > 2 * 1024 * 1024 {
            return Err(ErrorCode::QuotaExceeded);
        }
        Ok(Self {
            conn,
            listing,
            serial,
            query,
        })
    }
    pub fn page(
        &self,
        request: Id,
        source: &str,
        folder_id: Option<&str>,
        query: Option<&str>,
        cursor: &str,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let path = protocol::normalize_dav_path(folder_id.unwrap_or("/"));
        if source != self.conn.id || path != self.listing.path || query != self.query.as_deref() {
            return Err(ErrorCode::StaleHandle);
        }
        let (serial, offset) = cursor.split_once(':').ok_or(ErrorCode::InvalidArgument)?;
        if serial.parse::<u64>().ok() != Some(self.serial.0) {
            return Err(ErrorCode::StaleHandle);
        }
        let offset = offset
            .parse::<usize>()
            .map_err(|_| ErrorCode::InvalidArgument)?;
        if offset == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        self.page_at(request, folder_id, offset)
    }
    pub fn first_page(
        &self,
        request: Id,
        folder_id: Option<&str>,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        self.page_at(request, folder_id, 0)
    }
    fn page_at(
        &self,
        request: Id,
        folder_id: Option<&str>,
        offset: usize,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        enum PageItem<'a> {
            Folder(&'a WebDavFolder),
            Entry(&'a WebDavEntry),
        }
        impl PageItem<'_> {
            fn name(&self) -> &str {
                match self {
                    Self::Folder(f) => &f.name,
                    Self::Entry(e) => &e.name,
                }
            }
            fn into_source_entry(self) -> SourceEntry {
                match self {
                    Self::Folder(f) => SourceEntry {
                        id: f.path.clone(),
                        name: f.name.clone(),
                        kind: SourceEntryKind::Folder,
                        size: None,
                        modified_secs: (f.modified_secs > 0).then_some(WireU64(f.modified_secs)),
                        duration: None,
                        subtitle: None,
                        variants: Vec::new(),
                        thumbnail: None,
                    },
                    Self::Entry(e) => SourceEntry {
                        id: e.path.clone(),
                        name: e.name.clone(),
                        kind: kind(e.kind),
                        size: Some(WireU64(e.size)),
                        modified_secs: (e.modified_secs > 0).then_some(WireU64(e.modified_secs)),
                        duration: None,
                        subtitle: None,
                        variants: Vec::new(),
                        thumbnail: None,
                    },
                }
            }
        }
        let query = self.query.as_deref().unwrap_or("").trim().to_lowercase();
        let mut items = self
            .listing
            .subfolders
            .iter()
            .map(PageItem::Folder)
            .chain(self.listing.entries.iter().map(PageItem::Entry))
            .filter(|item| query.is_empty() || item.name().to_lowercase().contains(&query));
        for _ in 0..offset {
            items.next().ok_or(ErrorCode::StaleHandle)?;
        }
        let mut entries = Vec::new();
        // Leave envelope, path, title and cursor headroom below the 64 KiB ABI.
        let mut bytes = 0;
        let mut more = false;
        for item in items {
            if entries.len() >= 128 {
                more = true;
                break;
            }
            let entry = item.into_source_entry();
            let len = serde_json::to_vec(&entry)
                .map_err(|_| ErrorCode::Internal)?
                .len();
            if bytes + len > 40 * 1024 {
                more = true;
                break;
            }
            bytes += len + 1;
            entries.push(entry);
        }
        if more && entries.is_empty() {
            return Err(ErrorCode::QuotaExceeded);
        }
        let next_page = more.then(|| format!("{}:{}", self.serial.0, offset + entries.len()));
        let page = SourcePage {
            source_id: self.conn.id.clone(),
            // The host binds replies to the exact requested identity. None and
            // Some("/") share a DAV path, but remain distinct browse targets.
            folder_id: folder_id.map(str::to_owned),
            parent_id: self.listing.parent_path.clone(),
            title: if self.listing.path == "/" {
                self.conn.name.clone()
            } else {
                self.listing
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&self.conn.name)
                    .into()
            },
            entries,
            next_page,
            authenticated: true,
            auth_hint: None,
            filters: Vec::new(),
        };
        Ok(vec![HostCommand::ProvideSourcePage {
            request_id: request,
            page: Box::new(page),
        }])
    }
    pub fn resolve(
        &self,
        request: Id,
        entry: &str,
        headers: &[(String, String)],
        grant: Option<Id>,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let found = self
            .listing
            .entries
            .iter()
            .find(|e| e.path == entry)
            .ok_or(ErrorCode::InvalidArgument)?;
        let resource = |path: &str| -> Result<RemoteResource, ErrorCode> {
            Ok(RemoteResource {
                url: self
                    .conn
                    .build_url(path, false)
                    .map_err(|_| ErrorCode::InvalidArgument)?
                    .into(),
                headers: headers.to_vec(),
                origin_grant: grant,
            })
        };
        let mut media = ResolvedMedia {
            stream: StreamSpec {
                source_id: Some(self.conn.id.clone()),
                entry_id: Some(entry.into()),
                title: Some(found.name.clone()),
                kind: Some(kind(found.kind)),
                is_live: false,
                source: StreamSource::DirectHttp {
                    primary: resource(entry)?,
                    fallback_urls: Vec::new(),
                    identity: None,
                },
                variants: Vec::new(),
                active_variant: None,
                audio_tracks: Vec::new(),
                subtitles: Vec::new(),
            },
            playlist: Vec::new(),
        };
        if found.kind == Kind::Video {
            for (i, sub) in protocol::matching_subtitles(&self.listing.subtitles, &found.name)
                .into_iter()
                .take(16)
                .enumerate()
            {
                let attachment = Attachment {
                    id: format!("subtitle-{i}"),
                    label: sub.name.clone(),
                    lang: None,
                    resource: resource(&sub.path)?,
                    default: i == 0,
                };
                media.stream.subtitles.push(attachment);
                if serde_json::to_vec(&media)
                    .map_err(|_| ErrorCode::Internal)?
                    .len()
                    > 48 * 1024
                {
                    media.stream.subtitles.pop();
                    break;
                }
            }
        }
        // A bounded contiguous playlist includes the selected item and nearby
        // siblings. Larger directories remain available through source paging.
        let siblings: Vec<_> = self
            .listing
            .entries
            .iter()
            .filter(|e| e.kind == found.kind)
            .collect();
        let index = siblings.iter().position(|e| e.path == entry).unwrap();
        let start = index.saturating_sub(16);
        for item in siblings.into_iter().skip(start).take(64) {
            media.playlist.push(PlaylistEntry {
                source_id: self.conn.id.clone(),
                entry_id: item.path.clone(),
                title: item.name.clone(),
            });
            if serde_json::to_vec(&media)
                .map_err(|_| ErrorCode::Internal)?
                .len()
                > 56 * 1024
            {
                media.playlist.pop();
                break;
            }
        }
        Ok(vec![HostCommand::ResolveStream {
            request_id: request,
            media: Box::new(media),
        }])
    }
}
fn kind(value: Kind) -> SourceEntryKind {
    match value {
        Kind::Video => SourceEntryKind::Video,
        Kind::Photo => SourceEntryKind::Photo,
        Kind::Audio => SourceEntryKind::Audio,
    }
}
