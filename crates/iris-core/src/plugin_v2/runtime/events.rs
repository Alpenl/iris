use super::*;
use iris_api::v2::Capability;
pub(super) fn filter(
    mut event: HostEvent<serde_json::Value>,
    grants: &BTreeSet<Capability>,
    sources: &[SourceDescriptor],
) -> Result<Option<HostEvent<serde_json::Value>>, ErrorCode> {
    let read = grants.contains(&Capability::ReadState);
    let required = match &mut event {
        HostEvent::Init { .. }
        | HostEvent::Window { .. }
        | HostEvent::WindowOpened { .. }
        | HostEvent::WindowClosed { .. }
        | HostEvent::Completed { .. }
        | HostEvent::Shutdown
        | HostEvent::StreamInit { .. }
        | HostEvent::StreamRead { .. }
        | HostEvent::StreamReadCancelled { .. }
        | HostEvent::StreamContextChanged { .. } => return Err(ErrorCode::Denied),
        HostEvent::MediaOpened { .. } | HostEvent::StateChanged { .. } if !read => return Ok(None),
        HostEvent::MediaOpened { .. } | HostEvent::StateChanged { .. } => None,
        HostEvent::Command { snapshot, .. } | HostEvent::Action { snapshot, .. } => {
            if !read {
                *snapshot = None;
            }
            None
        }
        HostEvent::Tick { .. } => {
            if !read
                && !grants.contains(&Capability::UiOverlay)
                && !grants.contains(&Capability::MediaEnhance)
            {
                return Ok(None);
            }
            None
        }
        HostEvent::ModalSubmit { .. } | HostEvent::ModalCancel { .. } => Some(Capability::UiModal),
        HostEvent::SourceBrowse { source_id, .. } | HostEvent::SourceResolve { source_id, .. } => {
            if !sources.iter().any(|source| source.id == *source_id) {
                return Err(ErrorCode::StaleHandle);
            }
            Some(Capability::MediaSource)
        }
        HostEvent::SourceManage { .. } => Some(Capability::MediaSource),
        HostEvent::ResolveUrl { .. } => Some(Capability::UrlResolver),
        HostEvent::OverlayWindowChanged { .. } => Some(Capability::UiOverlay),
        HostEvent::ExportGranted { .. } => Some(Capability::ExportFiles),
        HostEvent::MediaInputGranted { .. } => Some(Capability::ExternalTool),
        HostEvent::SelectionGranted { .. } | HostEvent::AnnotationResult { .. } => {
            Some(Capability::Annotations)
        }
        HostEvent::OriginGranted { .. } => {
            if !grants.contains(&Capability::NetHttp) {
                return Err(ErrorCode::Denied);
            }
            Some(Capability::UiModal)
        }
        HostEvent::StreamRefreshRequested { .. }
        | HostEvent::StreamStatus { .. }
        | HostEvent::RequestCancelled { .. } => {
            if !grants.contains(&Capability::MediaSource)
                && !grants.contains(&Capability::UrlResolver)
            {
                return Err(ErrorCode::Denied);
            }
            None
        }
    };
    if required.is_some_and(|cap| !grants.contains(&cap)) {
        return Err(ErrorCode::Denied);
    }
    Ok(Some(event))
}
pub(super) fn global_command(command: &HostCommand) -> bool {
    matches!(
        command,
        HostCommand::SetSources { .. }
            | HostCommand::SetSidebar { .. }
            | HostCommand::SetToolbar { .. }
            | HostCommand::SetStatus { .. }
            | HostCommand::SetOverlay { .. }
            | HostCommand::SetTheme { .. }
    )
}
