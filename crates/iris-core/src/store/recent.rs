//! Persistent recent-media list (`recent.tsv`) with optional sanitized display titles.
//!
//! Row format is 100% backwards-compatible with legacy 1-column `recent.tsv` / `recent_uris.tsv`:
//! - `<hex_uri>` (legacy or untitled entry)
//! - `<hex_uri>\t<hex_title>` (entry with remembered human-readable title)

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::Path;

use crate::fs_util;
use crate::media_uri::MediaUri;

use super::{
    MAX_RECORD_KEY, Store, canonical_hex_matches, decode_any_media_uri, push_hex, read_tsv,
};

pub(super) const MAX_RECENT: usize = 20;
const MAX_RECENT_TITLE_BYTES: usize = 256;

/// Strips control characters and Unicode bidi overrides, collapses whitespace, and
/// bounds the UTF-8 byte length so each serialized TSV row fits well inside `MAX_LINE`.
pub(super) fn sanitize_recent_title(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_RECENT_TITLE_BYTES));
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_control() || is_bidi_format(ch) {
            if matches!(ch, '\t' | '\n' | '\r') && !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        let extra = usize::from(pending_space) + ch.len_utf8();
        if out.len() + extra > MAX_RECENT_TITLE_BYTES {
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    (!out.is_empty()).then_some(out)
}

fn is_bidi_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn decode_hex_utf8(hex_text: &str, max_bytes: usize) -> Option<String> {
    if hex_text.is_empty()
        || !hex_text.len().is_multiple_of(2)
        || hex_text.len() > max_bytes * 2
        || !hex_text.is_ascii()
    {
        return None;
    }
    let bytes: Vec<u8> = (0..hex_text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_text[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub(super) fn read_recent(
    path: &Path,
    out: &mut Vec<MediaUri>,
    titles: &mut BTreeMap<String, String>,
) -> io::Result<(bool, bool)> {
    read_tsv(path, |line| {
        let mut fields = line.split('\t');
        let (Some(raw_uri), opt_title, extra) = (fields.next(), fields.next(), fields.next())
        else {
            return true;
        };
        let raw_uri = raw_uri.trim();
        let Some(uri) = decode_any_media_uri(raw_uri) else {
            return true;
        };
        let key = uri.to_uri_string();
        if !uri.may_share_identity()
            || key.is_empty()
            || key.len() > MAX_RECORD_KEY
            || out.len() >= MAX_RECENT
            || out.contains(&uri)
        {
            return true;
        }
        let mut scrub = extra.is_some() || !canonical_hex_matches(raw_uri, &key);
        if let Some(raw_title) = opt_title {
            let raw_title = raw_title.trim();
            if let Some(decoded) = decode_hex_utf8(raw_title, MAX_RECENT_TITLE_BYTES)
                .and_then(|s| sanitize_recent_title(&s))
            {
                if !canonical_hex_matches(raw_title, &decoded) {
                    scrub = true;
                }
                titles.entry(key).or_insert(decoded);
            } else {
                scrub = true;
            }
        }
        out.push(uri);
        scrub
    })
}

pub(super) fn write_recent(
    path: &Path,
    recent: &[MediaUri],
    titles: &BTreeMap<String, String>,
) -> io::Result<()> {
    let mut text = String::with_capacity(recent.len() * 96);
    for uri in recent {
        let key = uri.to_uri_string();
        push_hex(key.as_bytes(), &mut text);
        if let Some(title) = titles.get(&key)
            && !title.is_empty()
        {
            text.push('\t');
            push_hex(title.as_bytes(), &mut text);
        }
        text.push('\n');
    }
    fs_util::atomic_write(path, text)
}

pub(super) fn promote_recent(
    recent: &mut Vec<MediaUri>,
    titles: &mut BTreeMap<String, String>,
    uri: &MediaUri,
    title: Option<&str>,
) -> bool {
    let mut changed = false;
    let key = uri.to_uri_string();
    if let Some(clean) = title.and_then(sanitize_recent_title)
        && titles.get(&key).map(String::as_str) != Some(clean.as_str())
    {
        titles.insert(key, clean);
        changed = true;
    }
    if recent.first() != Some(uri) {
        recent.retain(|existing| existing != uri);
        recent.insert(0, uri.clone());
        recent.truncate(MAX_RECENT);
        changed = true;
    }
    if changed {
        prune_recent_titles(recent, titles);
    }
    changed
}

pub(super) fn set_existing_recent_title(
    recent: &[MediaUri],
    titles: &mut BTreeMap<String, String>,
    key: &str,
    title: &str,
    only_if_missing: bool,
) -> bool {
    let Some(clean) = sanitize_recent_title(title) else {
        return false;
    };
    if !recent.iter().any(|u| u.to_uri_string() == key) {
        return false;
    }
    if only_if_missing && titles.contains_key(key) {
        return false;
    }
    if titles.get(key).map(String::as_str) == Some(clean.as_str()) {
        return false;
    }
    titles.insert(key.to_owned(), clean);
    true
}

pub(super) fn prune_recent_titles(recent: &[MediaUri], titles: &mut BTreeMap<String, String>) {
    if titles.is_empty() {
        return;
    }
    let live: HashSet<String> = recent.iter().map(MediaUri::to_uri_string).collect();
    titles.retain(|k, _| live.contains(k));
}

pub(super) fn backfill_plugin_titles<'a>(
    recent: &[MediaUri],
    titles: &mut BTreeMap<String, String>,
    updates: &mut BTreeMap<String, (String, bool)>,
    plugin_id: &str,
    source_id: &str,
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> bool {
    if !recent
        .iter()
        .any(|u| matches!(u, MediaUri::Plugin { plugin_id: pid, .. } if pid == plugin_id))
    {
        return false;
    }
    let mut by_entry: BTreeMap<&str, &str> = BTreeMap::new();
    for (entry_id, name) in entries {
        if !entry_id.is_empty() && !name.trim().is_empty() {
            by_entry.entry(entry_id).or_insert(name);
        }
    }
    if by_entry.is_empty() {
        return false;
    }
    let mut changed = false;
    for uri in recent {
        let MediaUri::Plugin {
            plugin_id: pid,
            source_id: sid,
            entry_id,
        } = uri
        else {
            continue;
        };
        if pid != plugin_id {
            continue;
        }
        let Some(&name) = by_entry.get(entry_id.as_str()) else {
            continue;
        };
        let exact_source = sid == source_id;
        // Cross-source matching only backfills legacy untitled opaque IDs (e.g. BV/av IDs).
        if !exact_source && entry_id.starts_with('/') {
            continue;
        }
        let key = uri.to_uri_string();
        let only_if_missing = !exact_source;
        if set_existing_recent_title(recent, titles, &key, name, only_if_missing)
            && let Some(clean) = titles.get(&key).cloned()
        {
            updates.insert(key, (clean, only_if_missing));
            changed = true;
        }
    }
    changed
}

pub(super) fn sync_recent_files(
    dir: &Path,
    clear_all_recent: bool,
    pushes: &[(MediaUri, Option<String>)],
    title_updates: &BTreeMap<String, (String, bool)>,
) -> io::Result<(Vec<MediaUri>, BTreeMap<String, String>)> {
    let path = dir.join("recent.tsv");
    let legacy_path = dir.join("recent_uris.tsv");
    let mut recent = Vec::new();
    let mut titles = BTreeMap::new();
    let (_, scrub) = read_recent(&path, &mut recent, &mut titles)?;
    let (legacy, _) = read_recent(&legacy_path, &mut recent, &mut titles)?;
    if legacy {
        write_recent(&path, &recent, &titles)?;
        fs_util::retry_sharing_violation(|| std::fs::remove_file(&legacy_path))?;
    }
    let mut changed = scrub && !legacy;
    if clear_all_recent && (!recent.is_empty() || !titles.is_empty()) {
        recent.clear();
        titles.clear();
        changed = true;
    }
    for (uri, title) in pushes.iter().rev() {
        changed |= promote_recent(&mut recent, &mut titles, uri, title.as_deref());
    }
    for (key, (title, only_if_missing)) in title_updates {
        changed |= set_existing_recent_title(&recent, &mut titles, key, title, *only_if_missing);
    }
    prune_recent_titles(&recent, &mut titles);
    if changed {
        write_recent(&path, &recent, &titles)?;
    }
    Ok((recent, titles))
}

impl Store {
    pub fn recent(&self) -> &[MediaUri] {
        &self.recent
    }

    pub fn recent_title(&self, uri: &MediaUri) -> Option<&str> {
        self.recent_titles
            .get(&uri.to_uri_string())
            .map(String::as_str)
    }

    pub fn recent_display_name(&self, uri: &MediaUri) -> String {
        self.recent_title(uri)
            .map(str::to_owned)
            .unwrap_or_else(|| uri.display_name())
    }

    pub(super) fn sync_recent(&mut self, dir: &Path) -> io::Result<()> {
        let (recent, titles) = sync_recent_files(
            dir,
            self.clear_all_recent,
            &self.recent_pushes,
            &self.recent_title_updates,
        )?;
        self.recent = recent;
        self.recent_titles = titles;
        self.recent_pushes.clear();
        self.recent_title_updates.clear();
        self.clear_all_recent = false;
        self.refresh_recent = false;
        Ok(())
    }

    pub fn push_recent(&mut self, uri: &MediaUri) {
        self.push_recent_with_title(uri, None);
    }

    pub fn push_recent_with_title(&mut self, uri: &MediaUri, title: Option<&str>) {
        if uri.may_share_identity() && uri.to_uri_string().len() <= MAX_RECORD_KEY {
            let clean = title.and_then(sanitize_recent_title);
            promote_recent(
                &mut self.recent,
                &mut self.recent_titles,
                uri,
                clean.as_deref(),
            );
            if let Some(pos) = self.recent_pushes.iter().position(|(u, _)| u == uri) {
                let (_, prev_title) = self.recent_pushes.remove(pos);
                self.recent_pushes
                    .insert(0, (uri.clone(), clean.or(prev_title)));
            } else {
                self.recent_pushes.insert(0, (uri.clone(), clean));
                self.recent_pushes.truncate(MAX_RECENT);
            }
            self.save();
        }
    }

    pub fn push_recent_path(&mut self, path: &Path) {
        self.push_recent(&MediaUri::from(path));
    }

    /// Backfills display titles for existing recent plugin entries when a catalog page loads
    /// (without reordering recency or overwriting titles from unrelated sources).
    pub fn backfill_plugin_recent_titles<'a>(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> bool {
        let changed = backfill_plugin_titles(
            &self.recent,
            &mut self.recent_titles,
            &mut self.recent_title_updates,
            plugin_id,
            source_id,
            entries,
        );
        if changed {
            self.save();
        }
        changed
    }

    pub fn clear_recent(&mut self) {
        self.recent.clear();
        self.recent_titles.clear();
        self.recent_pushes.clear();
        self.recent_title_updates.clear();
        self.clear_all_recent = true;
        self.save();
    }
}
