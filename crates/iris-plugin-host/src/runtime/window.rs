use super::{Control, WindowState};
use crate::actor::Executor;
use crate::auth::BrowserOpen;
use crate::json_error;
use iris_api::v2::{ErrorCode, Generation, HostEvent, Id, source::PlaybackSnapshot};
use iris_base::net::http_client::HttpCancel;
use std::sync::Arc;

/// Lightweight window-open plan; actual artifact directories are created lazily.
pub struct WindowPreparation {
    control: Id,
    window: Id,
    generation: Generation,
    event: Vec<u8>,
    browser: BrowserOpen,
}
pub struct PreparedWindow {
    control: Id,
    window: Id,
    generation: Generation,
    event: Vec<u8>,
    browser: BrowserOpen,
}
impl WindowPreparation {
    pub fn prepare(self) -> Result<PreparedWindow, ErrorCode> {
        Ok(PreparedWindow {
            control: self.control,
            window: self.window,
            generation: self.generation,
            event: self.event,
            browser: self.browser,
        })
    }
}
impl Control {
    pub fn window_preparation(
        &self,
        window: Id,
        generation: Generation,
        snapshot: Option<PlaybackSnapshot>,
        browser: BrowserOpen,
    ) -> Result<WindowPreparation, ErrorCode> {
        self.live()?;
        if let Some((active, _)) = &self.window {
            return Err(if *active == window {
                ErrorCode::BadState
            } else {
                ErrorCode::Backpressure
            });
        }
        let snapshot = snapshot.filter(|_| {
            self.installed
                .grants
                .contains(&iris_api::v2::Capability::ReadState)
        });
        let event = iris_core::bounded_json::encode(
            &HostEvent::<serde_json::Value>::WindowOpened { snapshot },
            iris_api::v2::MESSAGE_LIMIT - 512,
        )
        .map_err(json_error)?;
        Ok(WindowPreparation {
            control: self.control_id,
            window,
            generation,
            event,
            browser,
        })
    }
    /// No filesystem, compilation or network waits in this commit step.
    pub fn attach_window(&mut self, prepared: PreparedWindow) -> Result<(), ErrorCode> {
        self.live()?;
        if prepared.control != self.control_id {
            return Err(ErrorCode::StaleHandle);
        }
        let PreparedWindow {
            window,
            generation,
            event,
            browser,
            ..
        } = prepared;
        if let Some((active, _)) = &self.window {
            return Err(if *active == window {
                ErrorCode::BadState
            } else {
                ErrorCode::Backpressure
            });
        }
        let files = self.services.open_window(window, generation, browser)?;
        if let Some(actor) = &self.actor
            && let Err(error) = actor.open_window_with_event(window, generation, event)
        {
            self.services.window_changed(window, None);
            return Err(error);
        }
        self.window = Some((
            window,
            WindowState {
                generation,
                files,
                media: HttpCancel::default(),
                attachments: Arc::new(super::super::enhancement::AttachmentRegistry::default()),
            },
        ));
        Ok(())
    }
}
