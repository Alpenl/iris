//! Legacy v1 `webdav://` URI string upgrade and background `Bootstrap` `source_id`
//! migration reconciliation helpers.

use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;

use crate::plugin_ids::WEBDAV_PLUGIN_ID;

/// Canonical plugin URI fields upgraded from a legacy `webdav://` or `iris-webdav://` string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradedWebdavUri {
    pub plugin_id: &'static str,
    pub source_id: String,
    pub entry_id: String,
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

/// Upgrades a persisted v1 `webdav://` or `iris-webdav://` URI string into canonical
/// `iris.webdav` plugin addressing components.
pub fn upgrade_legacy_uri(raw: &str) -> Option<UpgradedWebdavUri> {
    let s = raw.trim_start();
    let rest = strip_prefix_ci(s, "webdav://").or_else(|| strip_prefix_ci(s, "iris-webdav://"))?;
    if let Some((enc_conn, query_rel)) = rest.split_once("?rel=")
        && !enc_conn.contains('/')
    {
        let source_id = percent_decode_str(enc_conn)
            .decode_utf8_lossy()
            .into_owned();
        let entry_id = percent_decode_str(query_rel)
            .decode_utf8_lossy()
            .into_owned();
        return Some(UpgradedWebdavUri {
            plugin_id: WEBDAV_PLUGIN_ID,
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
    Some(UpgradedWebdavUri {
        plugin_id: WEBDAV_PLUGIN_ID,
        source_id,
        entry_id,
    })
}

/// Reconciles an in-memory plugin bookmark entry against its latest on-disk version
/// when background v2 `Bootstrap` migration has hashed an overlong legacy WebDAV
/// `source_id` (`> 48` bytes) into `v1-<sha256_prefix>`.
pub fn reconcile_migrated_source(
    local_plugin_id: &str,
    local_source_id: &mut String,
    local_entry_id: &str,
    disk_plugin_id: &str,
    disk_source_id: &str,
    disk_entry_id: &str,
    remapped: &mut BTreeMap<String, String>,
) {
    if local_plugin_id == WEBDAV_PLUGIN_ID
        && disk_plugin_id == WEBDAV_PLUGIN_ID
        && local_entry_id == disk_entry_id
        && local_source_id.as_str() != disk_source_id
    {
        remapped.insert(local_source_id.clone(), disk_source_id.to_owned());
        *local_source_id = disk_source_id.to_owned();
    }
}

/// Rewrites `source_id` in place if a newly added `iris.webdav` bookmark still carries
/// a pre-migration legacy `source_id` observed during [`reconcile_migrated_source`].
pub fn apply_remapped_source(
    plugin_id: &str,
    source_id: &mut String,
    remapped: &BTreeMap<String, String>,
) {
    if plugin_id == WEBDAV_PLUGIN_ID
        && let Some(migrated) = remapped.get(source_id.as_str())
    {
        *source_id = migrated.clone();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        UpgradedWebdavUri, WEBDAV_PLUGIN_ID, apply_remapped_source, reconcile_migrated_source,
        upgrade_legacy_uri,
    };

    #[test]
    fn upgrade_legacy_uri_handles_path_rel_query_root_and_rejects_other_schemes() {
        assert_eq!(
            upgrade_legacy_uri("  WEBDAV://home%20nas/Movies/Ep%2001.mkv"),
            Some(UpgradedWebdavUri {
                plugin_id: WEBDAV_PLUGIN_ID,
                source_id: "home nas".into(),
                entry_id: "/Movies/Ep 01.mkv".into(),
            })
        );
        assert_eq!(
            upgrade_legacy_uri("iris-webdav://nas-1?rel=%2FAnime%2F01.mkv"),
            Some(UpgradedWebdavUri {
                plugin_id: WEBDAV_PLUGIN_ID,
                source_id: "nas-1".into(),
                entry_id: "/Anime/01.mkv".into(),
            })
        );
        assert_eq!(
            upgrade_legacy_uri("webdav://nas-root"),
            Some(UpgradedWebdavUri {
                plugin_id: WEBDAV_PLUGIN_ID,
                source_id: "nas-root".into(),
                entry_id: "/".into(),
            })
        );
        assert_eq!(upgrade_legacy_uri("https://example.com/a.mp4"), None);
        assert_eq!(
            upgrade_legacy_uri("iris-plugin://iris.webdav/nas/%2FMovies"),
            None
        );
    }

    #[test]
    fn reconcile_and_apply_remapped_source_only_affect_matching_webdav_entries() {
        let mut remapped = BTreeMap::new();
        let mut local_src = "legacy-overlong-source".to_string();
        reconcile_migrated_source(
            WEBDAV_PLUGIN_ID,
            &mut local_src,
            "/Movies",
            WEBDAV_PLUGIN_ID,
            "v1-deadbeef",
            "/Movies",
            &mut remapped,
        );
        assert_eq!(local_src, "v1-deadbeef");
        assert_eq!(
            remapped.get("legacy-overlong-source").map(String::as_str),
            Some("v1-deadbeef")
        );

        let mut other_plugin_src = "legacy-overlong-source".to_string();
        apply_remapped_source("acme.cloud", &mut other_plugin_src, &remapped);
        assert_eq!(other_plugin_src, "legacy-overlong-source");

        let mut new_webdav_src = "legacy-overlong-source".to_string();
        apply_remapped_source(WEBDAV_PLUGIN_ID, &mut new_webdav_src, &remapped);
        assert_eq!(new_webdav_src, "v1-deadbeef");
    }
}
