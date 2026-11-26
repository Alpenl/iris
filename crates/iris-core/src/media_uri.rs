//! Three-primitive unified media addressing (`Local`, `HttpUrl`, `Plugin`).

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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
                #[cfg(windows)]
                let normalized = crate::library::windows_unverbatim_path(path);
                #[cfg(windows)]
                let path = normalized.as_deref().unwrap_or(path);
                if path.is_absolute()
                    && let Ok(url) = Url::from_file_path(path)
                    && url
                        .path()
                        .strip_prefix('/')
                        .is_some_and(valid_file_url_tail)
                    && Self::decode_local_file_url(&url).as_deref() == Some(path)
                {
                    return url.into();
                }
                if let Some(raw) = path.to_str()
                    && !path.is_absolute()
                    && !starts_with_known_scheme(raw)
                {
                    return raw.to_owned();
                }
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    let encoded = percent_encoding::percent_encode(
                        path.as_os_str().as_bytes(),
                        COMPONENT_ENCODE_SET,
                    );
                    format!("iris-local:{encoded}")
                }
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStrExt;
                    if let Some(raw) = path.to_str() {
                        format!(
                            "iris-local:{}",
                            utf8_percent_encode(raw, COMPONENT_ENCODE_SET)
                        )
                    } else {
                        let bytes: Vec<u8> = path
                            .as_os_str()
                            .encode_wide()
                            .flat_map(u16::to_le_bytes)
                            .collect();
                        let mut encoded = String::from("iris-local-windows:");
                        URL_SAFE_NO_PAD.encode_string(bytes, &mut encoded);
                        encoded
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    format!(
                        "iris-local:{}",
                        utf8_percent_encode(&path.to_string_lossy(), COMPONENT_ENCODE_SET)
                    )
                }
            }
            MediaUri::HttpUrl(url) => url.trim_end_matches([' ', '\t', '\r', '\n']).to_owned(),
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

    /// Parses a canonical URI string (`file://`, `plugin://`, `http(s)://`) or local path.
    /// Legacy persisted schemes (`webdav://`) are handled by [`Self::parse_persisted`].
    pub fn parse(s: &str) -> Self {
        if let Some(rest) =
            strip_prefix_ci(s, "iris-plugin://").or_else(|| strip_prefix_ci(s, "plugin://"))
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

        if strip_prefix_ci(s, "http://").is_some() || strip_prefix_ci(s, "https://").is_some() {
            return MediaUri::HttpUrl(s.trim_end_matches([' ', '\t', '\r', '\n']).to_string());
        }

        if let Some(enc_local) = strip_prefix_ci(s, "iris-local:") {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                let bytes: Vec<u8> = percent_decode_str(enc_local).collect();
                if !bytes.is_empty() && !bytes.contains(&0) {
                    return Self::from(PathBuf::from(std::ffi::OsString::from_vec(bytes)));
                }
            }
            #[cfg(not(unix))]
            if let Ok(decoded) = percent_decode_str(enc_local).decode_utf8()
                && !decoded.is_empty()
                && !decoded.contains('\0')
            {
                return Self::from(PathBuf::from(decoded.into_owned()));
            }
            return MediaUri::Local(PathBuf::new());
        }

        if let Some(encoded) = strip_prefix_ci(s, "iris-local-windows:") {
            return Self::parse_iris_local_windows(encoded);
        }

        if let Some(path) = Self::local_file_uri_path(s) {
            return Self::from(path);
        }

        Self::from(PathBuf::from(s))
    }

    #[cfg(windows)]
    fn parse_iris_local_windows(encoded: &str) -> Self {
        if let Ok(bytes) = URL_SAFE_NO_PAD.decode(encoded)
            && !bytes.is_empty()
            && bytes.len().is_multiple_of(2)
        {
            use std::os::windows::ffi::OsStringExt;
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(u16::from_le_bytes)
                .collect();
            if !units.contains(&0) {
                return Self::from(PathBuf::from(std::ffi::OsString::from_wide(&units)));
            }
        }
        MediaUri::Local(PathBuf::new())
    }

    #[cfg(not(windows))]
    fn parse_iris_local_windows(_encoded: &str) -> Self {
        MediaUri::Local(PathBuf::new())
    }

    /// Parses a persisted URI string, upgrading legacy `webdav://` / `iris-webdav://` records
    /// from earlier Iris versions into canonical `MediaUri::Plugin` form.
    pub fn parse_persisted(raw: &str) -> Self {
        Self::upgrade_legacy_webdav(raw).unwrap_or_else(|| Self::parse(raw))
    }

    fn upgrade_legacy_webdav(raw: &str) -> Option<Self> {
        let s = raw.trim_start();
        let rest =
            strip_prefix_ci(s, "webdav://").or_else(|| strip_prefix_ci(s, "iris-webdav://"))?;
        if let Some((enc_conn, query_rel)) = rest.split_once("?rel=")
            && !enc_conn.contains('/')
        {
            let source_id = percent_decode_str(enc_conn)
                .decode_utf8_lossy()
                .into_owned();
            let entry_id = percent_decode_str(query_rel)
                .decode_utf8_lossy()
                .into_owned();
            return Some(MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id,
                entry_id,
            });
        }
        let (source_id, entry_id) = match rest.split_once('/') {
            Some((enc_conn, enc_path)) => {
                let conn = percent_decode_str(enc_conn)
                    .decode_utf8_lossy()
                    .into_owned();
                let path = percent_decode_str(enc_path).decode_utf8_lossy();
                (conn, format!("/{path}"))
            }
            None => (
                percent_decode_str(rest).decode_utf8_lossy().into_owned(),
                "/".into(),
            ),
        };
        Some(MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id,
            entry_id,
        })
    }

    /// Converts an unambiguous local file URL. Remote authorities, relative
    /// paths, generated separators and URL options cannot enter this path.
    pub fn local_file_uri_path(s: &str) -> Option<PathBuf> {
        let rest = strip_prefix_ci(s, "file://")?;
        let (authority, path) = rest.split_once('/')?;
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        if !valid_file_url_tail(path) {
            return None;
        }
        Self::decode_local_file_url(&Url::parse(s).ok()?)
    }

    fn decode_local_file_url(url: &Url) -> Option<PathBuf> {
        if url.host().is_some() || url.path().starts_with("//") {
            return None;
        }
        #[cfg(windows)]
        if !url.path().get(1..).is_some_and(|path| path.contains('/')) {
            return None;
        }
        let path = url.to_file_path().ok()?;
        #[cfg(windows)]
        if !matches!(path.components().next(), Some(std::path::Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::Disk(_)))
            || path.components().any(|component| {
                matches!(component, std::path::Component::Normal(part) if part.to_str().is_some_and(crate::library::windows_verbatim_component))
            })
        {
            return None;
        }
        path.is_absolute().then_some(path)
    }

    // Older records could serialize an explicit Windows UNC path using URL's
    // remote FILE authority. Keep this compatibility confined to stored data.
    #[cfg(windows)]
    pub(crate) fn legacy_unc_file_uri_path(s: &str) -> Option<PathBuf> {
        let rest = strip_prefix_ci(s, "file://")?;
        let (host, tail) = rest.split_once('/')?;
        if host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            || tail.is_empty()
            || !valid_file_url_tail(tail)
        {
            return None;
        }
        let url = Url::parse(s).ok()?;
        if url.path().trim_start_matches('/').is_empty() || url.path().starts_with("//") {
            return None;
        }
        let path = url.to_file_path().ok()?;
        matches!(path.components().next(), Some(std::path::Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::UNC(_, share) if !share.is_empty()))
            .then_some(path)
    }

    /// Direct URLs with userinfo, queries or fragments are session-only. Parameter
    /// names cannot reliably distinguish a resource selector from a bearer token.
    /// A plugin-provided stable identity is independent of its playback URL.
    pub fn may_share_identity(&self) -> bool {
        match self {
            Self::HttpUrl(raw) => Url::parse(raw).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            }),
            Self::Local(path) => !path.as_os_str().is_empty(),
            Self::Plugin { .. } => true,
        }
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
            MediaUri::HttpUrl(u) => {
                let Ok(parsed) = Url::parse(u) else {
                    return "HTTP stream".into();
                };
                if !matches!(parsed.scheme(), "http" | "https") {
                    return "HTTP stream".into();
                }
                parsed
                    .path_segments()
                    .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
                    .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
                    .filter(|label| {
                        !label.trim().is_empty() && !label.chars().any(char::is_control)
                    })
                    .or_else(|| parsed.host().map(|host| host.to_string()))
                    .unwrap_or_else(|| "HTTP stream".into())
            }
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
        Ok(MediaUri::parse_persisted(&raw))
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

fn valid_file_url_tail(path: &str) -> bool {
    if path.starts_with('/')
        || path.ends_with(' ')
        || path
            .bytes()
            .any(|b| b.is_ascii_control() || matches!(b, b'\\' | b'?' | b'#'))
    {
        return false;
    }
    let mut bytes = path.as_bytes();
    while let Some(index) = bytes.iter().position(|&b| b == b'%') {
        let Some(escape) = bytes.get(index + 1..index + 3) else {
            return false;
        };
        if !escape.iter().all(u8::is_ascii_hexdigit)
            || escape.eq_ignore_ascii_case(b"00")
            || escape.eq_ignore_ascii_case(b"2f")
            || (cfg!(windows) && escape.eq_ignore_ascii_case(b"5c"))
        {
            return false;
        }
        bytes = &bytes[index + 3..];
    }
    true
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
        "iris-local-windows:",
    ]
    .iter()
    .any(|p| strip_prefix_ci(s, p).is_some())
}

#[cfg(test)]
mod tests {
    use super::MediaUri;
    use std::path::{Path, PathBuf};

    #[test]
    fn malformed_iris_local_schemes_do_not_fall_back_to_literal_paths() {
        for bad in [
            "iris-local:",
            "iris-local:foo%00bar",
            "iris-local-windows:!!!invalid!!!",
            "iris-local-windows:",
            "iris-local-windows:AAA",
        ] {
            let parsed = MediaUri::parse(bad);
            assert_eq!(parsed, MediaUri::Local(PathBuf::new()), "{bad}");
            assert!(!parsed.may_share_identity(), "{bad}");
            assert_eq!(parsed.to_uri_string(), "", "{bad}");
        }
        #[cfg(windows)]
        {
            let parsed = MediaUri::parse("iris-local:%ff%fe");
            assert_eq!(parsed, MediaUri::Local(PathBuf::new()));
            assert!(!parsed.may_share_identity());
        }
        #[cfg(not(windows))]
        {
            let parsed = MediaUri::parse("iris-local-windows:QQA");
            assert_eq!(parsed, MediaUri::Local(PathBuf::new()));
            assert!(!parsed.may_share_identity());
            assert_eq!(parsed.to_uri_string(), "");
        }
    }

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
    fn legacy_webdav_uris_upgrade_losslessly_via_parse_persisted() {
        let parsed =
            MediaUri::parse_persisted("webdav://home-nas/Movies/Sci%20Fi%20%231/ep%2002.mkv");
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

        let root = MediaUri::parse_persisted("webdav://home-nas");
        assert_eq!(
            root,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: "home-nas".into(),
                entry_id: "/".into(),
            }
        );
    }

    #[test]
    fn local_urls_require_an_unambiguous_local_path() {
        for value in [
            "file://server/share/clip.mp4",
            "file:////server/share/clip.mp4",
            "file://localhost//server/share/clip.mp4",
            "file://localhost/%2fserver/share/clip.mp4",
            "file:///clip.mp4?x=1",
            "file:///clip.mp4#t=0",
            "file:///clip%00.mp4",
            "file://u@localhost/clip.mp4",
        ] {
            assert!(MediaUri::local_file_uri_path(value).is_none(), "{value}");
        }
        #[cfg(windows)]
        let value = (
            "file://LOCALHOST/C:/Iris%20Media/clip.mp4",
            r"C:\Iris Media\clip.mp4",
        );
        #[cfg(not(windows))]
        let value = (
            "file://LOCALHOST/tmp/Iris%20Media/clip.mp4",
            "/tmp/Iris Media/clip.mp4",
        );
        assert_eq!(
            MediaUri::local_file_uri_path(value.0).as_deref(),
            Some(Path::new(value.1))
        );
    }

    #[test]
    fn whitespace_and_private_stream_titles_keep_their_identity() {
        for raw in [
            " https://example.com/movie",
            "\u{3000}clip.mp4\u{3000}",
            " clip.mp4 ",
        ] {
            let expected = MediaUri::from(PathBuf::from(raw));
            assert_eq!(MediaUri::parse(raw), expected);
            assert_eq!(MediaUri::parse(&expected.to_uri_string()), expected);
        }
        assert_eq!(
            MediaUri::parse("https://user:secret@example.com/?token=secret#secret").display_name(),
            "example.com"
        );
        assert_eq!(
            MediaUri::parse("https://[invalid]/?token=secret").display_name(),
            "HTTP stream"
        );
        assert_eq!(
            MediaUri::parse("https://example.com/clip%20one.mp4?secret").display_name(),
            "clip one.mp4"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_stays_an_explicit_native_path() {
        let ordinary = MediaUri::from(Path::new(r"\\nas\media\clip.mp4"));
        let verbatim = MediaUri::from(Path::new(r"\\?\UNC\nas\media\clip.mp4"));
        assert_eq!(ordinary, verbatim);
        let encoded = ordinary.to_uri_string();
        assert!(encoded.starts_with("iris-local:"));
        assert_eq!(MediaUri::parse(&encoded), ordinary);
        assert_eq!(
            MediaUri::legacy_unc_file_uri_path("file://nas/media/clip.mp4").as_deref(),
            ordinary.as_local_path()
        );
        use std::os::windows::ffi::OsStringExt;
        let opaque = MediaUri::from(PathBuf::from(std::ffi::OsString::from_wide(&[0xd800, 65])));
        assert_eq!(MediaUri::parse(&opaque.to_uri_string()), opaque);
    }
}
