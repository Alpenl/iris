//! Semantic validation before any returned command changes host state.
mod ui;
use iris_api::v2::{ErrorCode, HostCommand, source::SourcePage};
use std::collections::BTreeSet;
pub(crate) use ui::overlay_with_limit;
pub use ui::{overlay, theme, widgets};

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}
pub fn text(value: &str, limit: usize) -> Result<(), ErrorCode> {
    if value.len() > limit {
        return Err(ErrorCode::QuotaExceeded);
    }
    if value
        .chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_bidi_control(c))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
pub fn single_line(value: &str, limit: usize) -> Result<(), ErrorCode> {
    if value.len() > limit {
        return Err(ErrorCode::QuotaExceeded);
    }
    if value.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
pub fn chrome(chrome: &iris_api::ChromeLayoutSpec) -> Result<(), ErrorCode> {
    if chrome.hidden_dock_buttons.len() > 64 {
        return Err(ErrorCode::QuotaExceeded);
    }
    for value in &chrome.hidden_dock_buttons {
        identifier(value)?;
    }
    Ok(())
}
pub fn manifest(manifest: &iris_api::v2::Manifest) -> Result<(), ErrorCode> {
    single_line(&manifest.name, 256)?;
    single_line(&manifest.version, 64)?;
    text(&manifest.author, 256)?;
    text(&manifest.description, 2048)?;
    for cmd in &manifest.commands {
        single_line(&cmd.title, 256)?;
        if let Some(sub) = &cmd.subtitle {
            single_line(sub, 512)?;
        }
        if let Some(hint) = &cmd.shortcut_hint {
            single_line(hint, 64)?;
        }
    }
    if let Some(provider) = &manifest.source_provider {
        single_line(&provider.label, 256)?;
        identifier(&provider.add_command_id)?;
        if provider.label.trim().is_empty()
            || !manifest
                .commands
                .iter()
                .any(|c| c.id == provider.add_command_id)
        {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    if let Some(value) = &manifest.theme {
        theme(value)?;
    }
    if let Some(value) = &manifest.chrome {
        chrome(value)?;
    }
    super::network::NetworkGrant::new(
        "manifest-check".into(),
        manifest.permissions.iter().copied().collect(),
        manifest.network.clone(),
        &super::ledger::Ledger::default(),
    )?;
    for pattern in &manifest.url_patterns {
        if !super::url_resolver::valid_pattern(pattern) {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    Ok(())
}
pub fn identifier(value: &str) -> Result<(), ErrorCode> {
    if !iris_api::is_valid_id(value) || value.len() > 256 {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
fn finite(value: f64, min: f64, max: f64) -> Result<(), ErrorCode> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
pub fn page(page: &SourcePage) -> Result<(), ErrorCode> {
    identifier(&page.source_id)?;
    text(&page.title, 1024)?;
    for value in page
        .folder_id
        .iter()
        .chain(page.parent_id.iter())
        .chain(page.next_page.iter())
    {
        text(value, 4096)?;
    }
    if page.next_page.as_ref().is_some_and(String::is_empty) {
        return Err(ErrorCode::InvalidArgument);
    }
    if let Some(hint) = &page.auth_hint {
        text(hint, 1024)?;
    }
    if page.entries.len() > 2000 || page.filters.len() > 16 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut ids = BTreeSet::new();
    for entry in &page.entries {
        text(&entry.id, 4096)?;
        text(&entry.name, 1024)?;
        if entry.id.is_empty() || entry.name.trim().is_empty() || !ids.insert(&entry.id) {
            return Err(ErrorCode::InvalidArgument);
        }
        if let Some(value) = entry.duration {
            finite(value, 0., 7. * 86400.)?;
        }
        if let Some(value) = &entry.subtitle {
            text(value, 1024)?;
        }
        variants(&entry.variants)?;
        if let Some(resource) = &entry.thumbnail {
            remote(resource)?;
        }
    }
    let mut ids = BTreeSet::new();
    for filter in &page.filters {
        identifier(&filter.id)?;
        text(&filter.label, 256)?;
        if !ids.insert(&filter.id) || filter.options.len() > 64 {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut values = BTreeSet::new();
        for option in &filter.options {
            text(&option.value, 1024)?;
            text(&option.label, 256)?;
            if !values.insert(&option.value) {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        if filter
            .selected
            .as_ref()
            .is_some_and(|s| !values.contains(s))
        {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    Ok(())
}
fn variants(values: &[iris_api::StreamVariant]) -> Result<(), ErrorCode> {
    if values.len() > 32 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut ids = BTreeSet::new();
    for value in values {
        text(&value.id, 256)?;
        text(&value.label, 256)?;
        if value.id.is_empty() || !ids.insert(&value.id) {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    Ok(())
}
pub fn remote(resource: &iris_api::v2::stream::RemoteResource) -> Result<(), ErrorCode> {
    super::network::normalize_url(&resource.url, super::network::TransportKind::Http)?;
    crate::vfs::http_client::raw::validate_headers(&resource.headers)
}
pub fn command(command: &HostCommand) -> Result<(), ErrorCode> {
    use HostCommand::*;
    match command {
        RequestOrigin { request_id, origin } => {
            if request_id.0 == 0 || request_id.0 > i64::MAX as u64 {
                return Err(ErrorCode::InvalidArgument);
            }
            super::network::normalize_url(origin, super::network::TransportKind::Http).map(|_| ())
        }
        SeekRelative { seconds } => finite(*seconds, -86400., 86400.),
        SeekAbsolute { seconds } => finite(*seconds, 0., 7. * 86400.),
        SetSpeed { speed } => finite(*speed, crate::player::MIN_SPEED, crate::player::MAX_SPEED),
        SetVolume { volume } => finite(*volume, 0., 100.),
        SetAbLoop { start, end } => {
            finite(*start, 0., 7. * 86400.)?;
            finite(*end, 0., 7. * 86400.)?;
            if start >= end {
                Err(ErrorCode::InvalidArgument)
            } else {
                Ok(())
            }
        }
        ShowToast { message } => text(message, 512),
        SetOverlay { nodes } => overlay(nodes, true),
        SetSidebar { panel } => {
            if let Some(panel) = panel {
                text(&panel.title, 256)?;
                if panel.sections.len() > 32
                    || panel
                        .sections
                        .iter()
                        .map(|s| s.widgets.len())
                        .sum::<usize>()
                        > 256
                {
                    return Err(ErrorCode::QuotaExceeded);
                }
                for section in &panel.sections {
                    text(&section.heading, 256)?;
                }
                // IDs and expanded button/options counts apply to the whole
                // panel, not independently to each section.
                let all: Vec<_> = panel
                    .sections
                    .iter()
                    .flat_map(|s| s.widgets.iter().cloned())
                    .collect();
                widgets(&all)?;
            }
            Ok(())
        }
        SetStatus { badge } => {
            if let Some(badge) = badge {
                identifier(&badge.id)?;
                single_line(&badge.text, 512)?;
                if let Some(tip) = &badge.tooltip {
                    text(tip, 512)?;
                }
                if let Some(action) = &badge.action_id {
                    identifier(action)?;
                }
            }
            Ok(())
        }
        SetToolbar { buttons } => {
            if buttons.len() > 32 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let mut ids = BTreeSet::new();
            for button in buttons {
                identifier(&button.id)?;
                identifier(&button.action_id)?;
                single_line(&button.label, 256)?;
                if !ids.insert(&button.id) {
                    return Err(ErrorCode::InvalidArgument);
                }
                if let Some(tip) = &button.tooltip {
                    text(tip, 512)?;
                }
                if let Some(icon) = &button.icon {
                    identifier(icon)?;
                }
            }
            Ok(())
        }
        OpenModal { modal } => {
            identifier(&modal.id)?;
            single_line(&modal.title, 256)?;
            if let Some(subtitle) = &modal.subtitle {
                text(subtitle, 512)?;
            }
            if let Some(submit) = &modal.submit_label {
                single_line(submit, 128)?;
            }
            if let Some(cancel) = &modal.cancel_label {
                single_line(cancel, 128)?;
            }
            widgets(&modal.widgets)
        }
        CloseModal { id } => {
            if let Some(id) = id {
                identifier(id)?;
            }
            Ok(())
        }
        SetTheme {
            theme: value,
            chrome: layout,
        } => {
            if let Some(value) = value {
                theme(value)?;
            }
            if let Some(layout) = layout {
                chrome(layout)?;
            }
            Ok(())
        }
        SetSources { sources } => iris_api::v2::source::SourceDescriptor::validate_all(sources)
            .map_err(|_| ErrorCode::InvalidArgument),
        RefreshSource { source_id } => identifier(source_id),
        ProvideSourcePage { page: value, .. } => page(value),
        SourceError { message, .. } => text(message, 1024),
        ResolveStream { media, .. } => {
            let stream = &media.stream;
            for value in stream.source_id.iter() {
                identifier(value)?;
            }
            for value in stream.entry_id.iter() {
                text(value, 4096)?;
            }
            for value in stream.title.iter() {
                text(value, 1024)?;
            }
            variants(&stream.variants)?;
            if stream
                .active_variant
                .as_ref()
                .is_some_and(|id| !stream.variants.iter().any(|v| &v.id == id))
            {
                return Err(ErrorCode::InvalidArgument);
            }
            if stream.audio_tracks.len() > 16
                || stream.subtitles.len() > 32
                || media.playlist.len() > 1000
            {
                return Err(ErrorCode::QuotaExceeded);
            }
            for attachments in [&stream.audio_tracks, &stream.subtitles] {
                let mut ids = BTreeSet::new();
                for attachment in attachments {
                    identifier(&attachment.id)?;
                    single_line(&attachment.label, 256)?;
                    if !ids.insert(&attachment.id) {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    if let Some(lang) = &attachment.lang
                        && (lang.len() > 48
                            || !lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                    {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    remote(&attachment.resource)?;
                }
            }
            for entry in &media.playlist {
                identifier(&entry.source_id)?;
                text(&entry.entry_id, 4096)?;
                single_line(&entry.title, 1024)?;
            }
            Ok(())
        }
        AttachSubtitle {
            id,
            label,
            lang,
            resource,
            ..
        }
        | AttachAudioTrack {
            id,
            label,
            lang,
            resource,
            ..
        } => {
            identifier(id)?;
            single_line(label, 256)?;
            if let Some(lang) = lang
                && (lang.len() > 48
                    || !lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            {
                return Err(ErrorCode::InvalidArgument);
            }
            if let iris_api::v2::enhance::EnhancementResource::Remote { request } = resource {
                remote(request)?;
            }
            Ok(())
        }
        SetTimelineMeta {
            chapters,
            heat,
            skip,
            ..
        } => {
            if chapters.len() > 1000 || heat.len() > 256 || skip.len() > 64 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let mut previous = None;
            for chapter in chapters {
                if previous.is_some_and(|n| chapter.start_ms <= n) {
                    return Err(ErrorCode::InvalidArgument);
                }
                text(&chapter.title, 256)?;
                previous = Some(chapter.start_ms);
            }
            for range in heat {
                if range.intensity > 100 {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            for range in skip {
                text(&range.label, 128)?;
            }
            timeline_ranges(heat.iter().map(|r| (r.start_ms.0, r.end_ms.0)))?;
            timeline_ranges(skip.iter().map(|r| (r.start_ms.0, r.end_ms.0)))?;
            Ok(())
        }
        SetTimedOverlay { track, .. }
        | AppendTimedCues { track, .. }
        | ClearTimedOverlay { track, .. } => identifier(track),
        EditAnnotation { edit, .. } => annotation(edit),
        ReadAnnotation {
            index,
            marker_offset,
            ..
        } => {
            if *index >= 1000 || *marker_offset > 1000 {
                Err(ErrorCode::InvalidArgument)
            } else {
                Ok(())
            }
        }
        RequestExport { suggested_name, .. } => {
            single_line(suggested_name, 240)?;
            if suggested_name.is_empty() || suggested_name.contains(['/', '\\', ':']) {
                return Err(ErrorCode::InvalidArgument);
            }
            Ok(())
        }
        // These commands carry opaque capabilities; consuming services check
        // instance/session ownership and current generation before mutation.
        TogglePlayback
        | RequestMediaInput { .. }
        | ToggleMute
        | OpenNext
        | OpenPrevious
        | ToggleFavorite
        | ClearAbLoop
        | UpdateStreamContext { .. }
        | UpdateManifest { .. }
        | SetMonitorLut { .. } => Ok(()),
    }
}

fn timeline_ranges(ranges: impl Iterator<Item = (u64, u64)>) -> Result<(), ErrorCode> {
    let mut previous_end = 0;
    for (start, end) in ranges {
        if start < previous_end || start >= end || end > 7 * 24 * 60 * 60 * 1000 {
            return Err(ErrorCode::InvalidArgument);
        }
        previous_end = end;
    }
    Ok(())
}
pub fn annotation(edit: &iris_api::v2::enhance::AnnotationEdit) -> Result<(), ErrorCode> {
    use iris_api::v2::enhance::AnnotationEdit::*;
    match edit {
        Rating { rating } if *rating > 5 => Err(ErrorCode::InvalidArgument),
        UpsertMarker { marker } => {
            identifier(&marker.id)?;
            text(&marker.label, 256)
        }
        RemoveMarker { id } => identifier(id),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeline_ranges_reject_overlap_reversal_and_excessive_horizon() {
        assert_eq!(timeline_ranges([(0, 10), (10, 20)].into_iter()), Ok(()));
        for ranges in [
            vec![(3, 3)],
            vec![(4, 2)],
            vec![(0, 10), (9, 20)],
            vec![(0, u64::MAX)],
        ] {
            assert_eq!(
                timeline_ranges(ranges.into_iter()),
                Err(ErrorCode::InvalidArgument)
            );
        }
        let valid = br#"{"command":"set_timeline_meta","media":"1","generation":{"media":"1","seek":"0","context":"0"},"chapters":[],"heat":[{"start_ms":"0","end_ms":"10","intensity":101}],"skip":[]}"#;
        let parsed: HostCommand = serde_json::from_slice(valid).unwrap();
        assert_eq!(command(&parsed), Err(ErrorCode::InvalidArgument));
    }
    #[test]
    fn untrusted_ui_cannot_inject_links_coordinates_or_playback_values() {
        let bad = iris_api::Widget::Link {
            label: "open".into(),
            url: "file:///C:/secret.txt".into(),
            button_text: None,
        };
        assert!(widgets(&[bad]).is_err());
        assert!(
            command(&HostCommand::SetSpeed {
                speed: f64::INFINITY
            })
            .is_err()
        );
        assert!(command(&HostCommand::SetAbLoop { start: 2., end: 1. }).is_err());
        let bad = iris_api::OverlayNode::Crosshair {
            x: f32::NAN,
            y: 0.5,
            size: 0.1,
            color: Default::default(),
            opacity: 1.,
        };
        assert!(overlay(&[bad], true).is_err());
        let button = iris_api::OverlayNode::Button {
            id: "x".into(),
            x: 0.,
            y: 0.,
            label: "x".into(),
            icon: None,
            accent: false,
            action_id: "x".into(),
        };
        assert_eq!(overlay(&[button], false), Err(ErrorCode::Denied));
    }
}
