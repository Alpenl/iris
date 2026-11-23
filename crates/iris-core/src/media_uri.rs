//! Three-primitive unified media addressing (`Local`, `HttpUrl`, `Plugin`).

use std::fmt;
use std::path::{Path, PathBuf};

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

/// Strict component encoding set (encodes `/`, `?`, `#`, `%`, `:`, `@`, spaces, etc.).
const COMPONENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Unified identifier for local files, direct HTTP(S) streams, and `MediaSource` plugin items.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MediaUri {
    Local(PathBuf),
    HttpUrl(String),
    Plugin {
        plugin_id: String,
        source_id: String,
        entry_id: String,
    },
}

impl MediaUri {
    /// Serializes this [`MediaUri`] into a canonical string representation that roundtrips via [`MediaUri::parse`].
    pub fn to_uri_string(&self) -> String {
        match self {
            MediaUri::Local(path) => {
                if path.is_absolute()
                    && let Ok(url) = Url::from_file_path(path)
                {
                    return url.into();
                }
                let raw = path.to_string_lossy();
                if starts_with_known_scheme(&raw) {
                    format!(
                        "iris-local:{}",
                        utf8_percent_encode(&raw, COMPONENT_ENCODE_SET)
                    )
                } else {
                    raw.into_owned()
                }
            }
            MediaUri::HttpUrl(url) => url.trim().to_string(),
            MediaUri::Plugin {
                plugin_id,
                source_id,
                entry_id,
            } => format!(
                "iris-plugin://{}/{}/{}",
                utf8_percent_encode(plugin_id, COMPONENT_ENCODE_SET),
                utf8_percent_encode(source_id, COMPONENT_ENCODE_SET),
                utf8_percent_encode(entry_id, COMPONENT_ENCODE_SET),
            ),
        }
    }

    /// Parses a URI string or filesystem path into a [`MediaUri`].
    ///
    /// Legacy `"webdav://<conn>/<path>"` URIs are losslessly upgraded to
    /// `MediaUri::Plugin { plugin_id: "iris.webdav", source_id: conn, entry_id: path }`.
    pub fn parse(s: &str) -> Self {
        let trimmed = s.trim();

        if let Some(rest) = strip_prefix_ci(trimmed, "iris-plugin://")
            .or_else(|| strip_prefix_ci(trimmed, "plugin://"))
        {
            let mut parts = rest.splitn(3, '/');
            let plugin_id = parts
                .next()
                .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
                .unwrap_or_default();
            let source_id = parts
                .next()
                .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
                .unwrap_or_default();
            let entry_id = parts
                .next()
                .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
                .unwrap_or_default();
            return MediaUri::Plugin {
                plugin_id,
                source_id,
                entry_id,
            };
        }

        if let Some(rest) = strip_prefix_ci(trimmed, "webdav://")
            .or_else(|| strip_prefix_ci(trimmed, "iris-webdav://"))
        {
            if let Some((enc_conn, query_rel)) = rest.split_once("?rel=")
                && !enc_conn.contains('/')
            {
                let source_id = percent_decode_str(enc_conn)
                    .decode_utf8_lossy()
                    .into_owned();
                let entry_id = percent_decode_str(query_rel)
                    .decode_utf8_lossy()
                    .into_owned();
                return MediaUri::Plugin {
                    plugin_id: "iris.webdav".into(),
                    source_id,
                    entry_id,
                };
            }
            let (source_id, entry_id) = match rest.split_once('/') {
                Some((enc_conn, enc_path)) => {
                    let conn = percent_decode_str(enc_conn)
                        .decode_utf8_lossy()
                        .into_owned();
                    let decoded_segs: Vec<String> = enc_path
                        .split('/')
                        .map(|seg| percent_decode_str(seg).decode_utf8_lossy().into_owned())
                        .collect();
                    (conn, format!("/{}", decoded_segs.join("/")))
                }
                None => (
                    percent_decode_str(rest).decode_utf8_lossy().into_owned(),
                    "/".into(),
                ),
            };
            return MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id,
                entry_id,
            };
        }

        if strip_prefix_ci(trimmed, "http://").is_some()
            || strip_prefix_ci(trimmed, "https://").is_some()
        {
            return MediaUri::HttpUrl(trimmed.to_string());
        }

        if let Some(enc_local) = strip_prefix_ci(trimmed, "iris-local:") {
            let decoded = percent_decode_str(enc_local)
                .decode_utf8_lossy()
                .into_owned();
            return MediaUri::Local(PathBuf::from(decoded));
        }

        if strip_prefix_ci(trimmed, "file://").is_some() {
            if let Ok(url) = Url::parse(trimmed)
                && let Ok(p) = url.to_file_path()
            {
                return MediaUri::Local(p);
            }
            if let Some(raw_path) = strip_prefix_ci(trimmed, "file://") {
                let decoded = percent_decode_str(raw_path)
                    .decode_utf8_lossy()
                    .into_owned();
                return MediaUri::Local(PathBuf::from(decoded));
            }
        }

        MediaUri::Local(PathBuf::from(s))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, MediaUri::Local(_))
    }

    pub fn as_local_path(&self) -> Option<&Path> {
        match self {
            MediaUri::Local(p) => Some(p.as_path()),
            _ => None,
        }
    }

    /// Human-readable display label (basename / last path segment) for recent lists and titles.
    pub fn display_name(&self) -> String {
        match self {
            MediaUri::Local(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
            MediaUri::Plugin {
                source_id,
                entry_id,
                ..
            } => entry_id
                .trim_end_matches('/')
                .rsplit('/')
                .find(|seg| !seg.is_empty())
                .map(String::from)
                .unwrap_or_else(|| source_id.clone()),
            MediaUri::HttpUrl(u) => Url::parse(u)
                .ok()
                .and_then(|parsed| {
                    parsed
                        .path_segments()
                        .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
                        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| u.clone()),
        }
    }
}

impl From<PathBuf> for MediaUri {
    fn from(path: PathBuf) -> Self {
        MediaUri::Local(crate::library::strip_windows_verbatim_disk(path))
    }
}

impl From<&Path> for MediaUri {
    fn from(path: &Path) -> Self {
        MediaUri::Local(crate::library::strip_windows_verbatim_disk(
            path.to_path_buf(),
        ))
    }
}

impl fmt::Display for MediaUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri_string())
    }
}

impl Serialize for MediaUri {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_uri_string())
    }
}

impl<'de> Deserialize<'de> for MediaUri {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(MediaUri::parse(&raw))
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn starts_with_known_scheme(s: &str) -> bool {
    [
        "file://",
        "http://",
        "https://",
        "webdav://",
        "iris-webdav://",
        "plugin://",
        "iris-plugin://",
        "iris-local:",
    ]
    .iter()
    .any(|p| strip_prefix_ci(s, p).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_primitives_roundtrip_and_local_emits_file_uri() {
        #[cfg(windows)]
        let abs_path = Path::new(r"C:\Iris Media\测试 #1 %20.mkv");
        #[cfg(not(windows))]
        let abs_path = Path::new("/tmp/Iris Media/测试 #1 %20.mkv");

        let local_abs = MediaUri::from(abs_path);
        let local_str = local_abs.to_uri_string();
        assert!(
            local_str.starts_with("file:///"),
            "expected file:// URI, got {local_str}"
        );
        assert_eq!(local_abs.as_local_path(), Some(abs_path));

        #[cfg(windows)]
        {
            let verbatim = MediaUri::from(PathBuf::from(r"\\?\C:\Iris Media\测试 #1 %20.mkv"));
            assert_eq!(verbatim, local_abs);
            assert_eq!(MediaUri::parse(&verbatim.to_uri_string()), local_abs);
        }

        let cases = [
            local_abs,
            MediaUri::Local(PathBuf::from("/tmp/Iris Media/测试 #1 %20.mkv")),
            MediaUri::Local(PathBuf::from("relative/folder/clip.mp4")),
            MediaUri::HttpUrl("https://cdn.example.com/stream/live.m3u8?k=1".into()),
            MediaUri::Plugin {
                plugin_id: "acme.cloud".into(),
                source_id: "drive-1".into(),
                entry_id: "/folder/sub/item?token=abc&sig=1#part".into(),
            },
        ];

        for uri in cases {
            let encoded = uri.to_uri_string();
            assert_eq!(MediaUri::parse(&encoded), uri);
            let json = serde_json::to_string(&uri).unwrap();
            let from_json: MediaUri = serde_json::from_str(&json).unwrap();
            assert_eq!(from_json, uri);
        }
    }

    #[test]
    fn legacy_webdav_uris_upgrade_losslessly_to_iris_webdav_plugin() {
        let parsed = MediaUri::parse("webdav://home-nas/Movies/Sci%20Fi%20%231/ep%2002.mkv");
        assert_eq!(
            parsed,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: "home-nas".into(),
                entry_id: "/Movies/Sci Fi #1/ep 02.mkv".into(),
            }
        );
        assert_eq!(parsed.display_name(), "ep 02.mkv");
        assert_eq!(parsed.as_local_path(), None);

        let root = MediaUri::parse("webdav://home-nas");
        assert_eq!(
            root,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: "home-nas".into(),
                entry_id: "/".into(),
            }
        );
    }
}
