use super::*;

/// The coordinator moves this plan to its bounded filesystem worker lane.
pub struct WindowPreparation {
    control: Id,
    window: Id,
    generation: Generation,
    event: Vec<u8>,
    setup: Setup,
    installed: Installed,
    network: NetworkGrant,
    storage: Arc<Storage>,
    browser: BrowserOpen,
}
pub struct PreparedWindow {
    control: Id,
    window: Id,
    generation: Generation,
    event: Vec<u8>,
    services: HostServices,
}
impl WindowPreparation {
    pub fn prepare(self) -> Result<PreparedWindow, ErrorCode> {
        let services = Control::make_services(
            &self.setup,
            &self.installed,
            &self.network,
            &self.storage,
            Some(self.window),
            self.browser,
        )?;
        Ok(PreparedWindow {
            control: self.control,
            window: self.window,
            generation: self.generation,
            event: self.event,
            services,
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
        if self.windows.contains_key(&window) {
            return Err(ErrorCode::BadState);
        }
        if self.windows.len() >= 4 {
            return Err(ErrorCode::Backpressure);
        }
        let snapshot = snapshot.filter(|_| {
            self.installed
                .grants
                .contains(&iris_api::v2::Capability::ReadState)
        });
        let event = iris_plugin_sdk::json::encode(
            &HostEvent::<serde_json::Value>::WindowOpened { snapshot },
            iris_api::v2::MESSAGE_LIMIT - 512,
        )?;
        Ok(WindowPreparation {
            control: self.control_id,
            window,
            generation,
            event,
            setup: self.setup.clone(),
            installed: self.installed.clone(),
            network: self.network.clone(),
            storage: self.storage.clone(),
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
            services,
            ..
        } = prepared;
        if self.windows.contains_key(&window) {
            return Err(ErrorCode::BadState);
        }
        if self.windows.len() >= 4 {
            return Err(ErrorCode::Backpressure);
        }
        self.services
            .register(window, generation, Arc::new(services.clone()))?;
        if let Some(actor) = &self.actor {
            if let Err(error) = actor.open_window(window, generation) {
                self.services.window_changed(window, None);
                return Err(error);
            }
            if let Err(error) = actor.send_window(window, generation, event) {
                let _ = actor.close_window(window);
                return Err(error);
            }
        }
        self.windows.insert(
            window,
            WindowState {
                generation,
                services,
                media: HttpCancel::default(),
                attachments: Arc::new(super::super::enhancement::AttachmentRegistry::default()),
            },
        );
        Ok(())
    }
}
