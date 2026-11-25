use super::*;
impl Control {
    /// Directory setup is background-only. The browser hook only queues UI work.
    pub fn open_window(
        &mut self,
        window: Id,
        generation: Generation,
        snapshot: Option<PlaybackSnapshot>,
        browser: BrowserOpen,
    ) -> Result<(), ErrorCode> {
        let plan = self.window_preparation(window, generation, snapshot, browser)?;
        self.attach_window(plan.prepare()?)
    }
    pub fn advance_window(&mut self, window: Id, generation: Generation) -> Result<(), ErrorCode> {
        self.live()?;
        let state = self
            .windows
            .get_mut(&window)
            .ok_or(ErrorCode::StaleHandle)?;
        let old = state.generation;
        if generation == old {
            return Ok(());
        }
        if generation.media < old.media
            || (generation.media == old.media
                && (generation.seek < old.seek || generation.context < old.context))
        {
            return Err(ErrorCode::StaleHandle);
        }
        if let Some(actor) = &self.actor {
            actor.advance_window(window, generation)?;
        } else {
            self.services.window_changed(window, Some(generation));
        }
        if generation.media != old.media {
            std::mem::take(&mut state.media).cancel();
            state.attachments = Arc::new(super::super::enhancement::AttachmentRegistry::default());
        }
        state.generation = generation;
        Ok(())
    }
    pub fn close_window(&mut self, window: Id) -> Result<(), ErrorCode> {
        let state = self.windows.remove(&window).ok_or(ErrorCode::StaleHandle)?;
        state.media.cancel();
        if let Some(actor) = &self.actor {
            actor.close_window(window)?;
        } else {
            self.services.window_changed(window, None);
        }
        Ok(())
    }
    pub fn send(&self, window: Id, event: HostEvent<serde_json::Value>) -> Result<(), ErrorCode> {
        self.send_scoped(window, event, None)
    }
    /// SourceRequests owns this cancellation token independently of playback.
    pub fn send_cancellable(
        &self,
        window: Id,
        event: HostEvent<serde_json::Value>,
        cancel: HttpCancel,
    ) -> Result<(), ErrorCode> {
        self.send_scoped(window, event, Some(cancel))
    }
    fn send_scoped(
        &self,
        window: Id,
        event: HostEvent<serde_json::Value>,
        cancel: Option<HttpCancel>,
    ) -> Result<(), ErrorCode> {
        self.live()?;
        let state = self.windows.get(&window).ok_or(ErrorCode::StaleHandle)?;
        let stale = event_generation_stale(&event, state.generation);
        if stale {
            return Err(ErrorCode::StaleHandle);
        }
        let Some(event) = events::filter(event, &self.installed.grants, &self.sources)? else {
            return Ok(());
        };
        let actor = self.actor.as_ref().ok_or(ErrorCode::Unsupported)?;
        let bytes = iris_plugin_sdk::json::encode(&event, iris_api::v2::MESSAGE_LIMIT - 512)?;
        if let Some(cancel) = cancel {
            actor.send_cancellable(window, state.generation, bytes, cancel)
        } else {
            actor.send_window(window, state.generation, bytes)
        }
    }
    pub fn status(&self) -> Status {
        if let Some(error) = self.quarantined {
            return Status::Failed(error);
        }
        if self.stopped {
            return if self.is_quiescent() {
                Status::Stopped
            } else {
                Status::Stopping
            };
        }
        self.actor
            .as_ref()
            .map_or(Status::Ready, |actor| match actor.status() {
                Status::Ready if !actor.initialized() => Status::Initializing,
                status => status,
            })
    }
    pub fn poll(&mut self) -> Option<Update> {
        if self.stopped {
            return None;
        }
        if let Some(Status::Failed(
            error @ (ErrorCode::Denied | ErrorCode::InvalidArgument | ErrorCode::BadState),
        )) = self.actor.as_ref().map(Actor::status)
        {
            self.quarantine(error);
            return None;
        }
        let update = self.actor.as_ref()?.poll()?;
        if update.window.is_none()
            && update
                .commands
                .iter()
                .any(|command| !events::global_command(command))
        {
            self.quarantine(ErrorCode::Denied);
            return None;
        }
        for command in &update.commands {
            if let HostCommand::SetSources { sources } = command {
                self.sources = sources.clone();
            }
        }
        Some(update)
    }
    fn quarantine(&mut self, error: ErrorCode) {
        self.quarantined = Some(error);
        self.stop();
    }
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.network.revoke();
        for state in self.windows.values() {
            state.media.cancel();
        }
        self.windows.clear();
        if let Some(actor) = &self.actor {
            actor.stop();
        }
        self.services.stop();
        self.sources.clear();
        (self.setup.wake)();
    }
    pub fn is_quiescent(&self) -> bool {
        self.actor.as_ref().is_none_or(Actor::is_quiescent) && self.services.is_quiescent()
    }
}

fn event_generation_stale(event: &HostEvent<serde_json::Value>, current: Generation) -> bool {
    match event {
        HostEvent::MediaOpened { generation, .. }
        | HostEvent::StateChanged { generation, .. }
        | HostEvent::Tick { generation, .. }
        | HostEvent::StreamRefreshRequested { generation, .. } => *generation != current,
        // Timed overlays carry an independent inner seek generation so seeking
        // refreshes overlay windows without revoking unrelated ControlStore work.
        HostEvent::OverlayWindowChanged { generation, .. } => {
            generation.media != current.media
                || generation.context != current.context
                || generation.seek < current.seek
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_api::v2::enhance::OverlayWindow;

    #[test]
    fn overlay_window_accepts_inner_seek_advance_without_stale_handle() {
        let current = Generation {
            media: Id(2),
            seek: Id(0),
            context: Id(1),
        };
        let win = OverlayWindow {
            revision: Id(2),
            start_ms: Id(0),
            end_ms: Id(120_000),
        };
        let sought = HostEvent::OverlayWindowChanged {
            media: Id(2),
            generation: Generation {
                seek: Id(3),
                ..current
            },
            window: win.clone(),
        };
        assert!(!event_generation_stale(&sought, current));

        let wrong_media = HostEvent::OverlayWindowChanged {
            media: Id(1),
            generation: Generation {
                media: Id(1),
                seek: Id(3),
                context: Id(1),
            },
            window: win.clone(),
        };
        assert!(event_generation_stale(&wrong_media, current));

        let stale_tick = HostEvent::Tick {
            generation: Generation {
                seek: Id(1),
                ..current
            },
            position: 0.0,
            duration: 10.0,
            paused: false,
        };
        assert!(event_generation_stale(&stale_tick, current));
    }
}
