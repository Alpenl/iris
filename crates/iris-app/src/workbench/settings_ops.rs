//! Semantic appearance, layout, playback, subtitle/audio, view, and DoH settings
//! mutations on [`super::WorkbenchController`].

use iris_core::library::{SortBy, SortOrder};
use iris_core::settings::{
    DEFAULT_DIRECTORY_WIDTH, DEFAULT_UI_FONT_SCALE, Hwdec, MAX_DIRECTORY_WIDTH, MAX_UI_FONT_SCALE,
    MIN_DIRECTORY_WIDTH, MIN_UI_FONT_SCALE, OnEof, PhotoDefault, Side, SubLang, VideoWheel,
};

use super::{WorkbenchController, WorkbenchDomainChanges};

pub const DOH_PRESET_DOMESTIC_FIRST: u8 = 0;
pub const DOH_PRESET_DOMESTIC_ONLY: u8 = 1;
pub const DOH_PRESET_INTERNATIONAL: u8 = 2;
pub const DOH_PRESET_CUSTOM: u8 = 3;

/// Identifies which built-in DoH resolver preset matches `resolvers` (`3` = custom).
pub fn doh_resolver_preset_for(resolvers: &[String]) -> u8 {
    match resolvers {
        [a, b, c] if a == "223.5.5.5" && b == "1.12.12.12" && c == "1.1.1.1" => {
            DOH_PRESET_DOMESTIC_FIRST
        }
        [a, b] if a == "223.5.5.5" && b == "1.12.12.12" => DOH_PRESET_DOMESTIC_ONLY,
        [a, b, c] if a == "1.1.1.1" && b == "8.8.8.8" && c == "9.9.9.9" => DOH_PRESET_INTERNATIONAL,
        _ => DOH_PRESET_CUSTOM,
    }
}

/// Returns the canonical resolver list for a selectable built-in DoH preset.
pub fn doh_resolvers_for_preset(preset: u8) -> Option<Vec<String>> {
    match preset {
        DOH_PRESET_DOMESTIC_FIRST => Some(vec![
            "223.5.5.5".into(),
            "1.12.12.12".into(),
            "1.1.1.1".into(),
        ]),
        DOH_PRESET_DOMESTIC_ONLY => Some(vec!["223.5.5.5".into(), "1.12.12.12".into()]),
        DOH_PRESET_INTERNATIONAL => {
            Some(vec!["1.1.1.1".into(), "8.8.8.8".into(), "9.9.9.9".into()])
        }
        _ => None,
    }
}

impl WorkbenchController {
    pub fn set_theme_id(&mut self, theme_id: &str) -> WorkbenchDomainChanges {
        let clean = theme_id.trim();
        let next = if clean.is_empty() { "graphite" } else { clean };
        self.settings.theme_id = next.to_owned();
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            theme_changed: true,
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn step_ui_font_scale(&mut self, delta: f32) -> WorkbenchDomainChanges {
        self.settings.ui_font_scale = (((self.settings.ui_font_scale + delta) * 100.).round()
            / 100.)
            .clamp(MIN_UI_FONT_SCALE, MAX_UI_FONT_SCALE);
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            theme_changed: true,
            settings_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn reset_ui_font_scale(&mut self) -> WorkbenchDomainChanges {
        self.settings.ui_font_scale = DEFAULT_UI_FONT_SCALE;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            theme_changed: true,
            settings_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn set_directory_side(&mut self, side: Side) -> Option<WorkbenchDomainChanges> {
        if self.settings.directory_side == side {
            return None;
        }
        self.settings.directory_side = side;
        self.preset_rt.record_user_edit(&self.settings);
        Some(WorkbenchDomainChanges {
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            viewport_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        })
    }

    pub fn step_directory_width(&mut self, delta: f32) -> WorkbenchDomainChanges {
        self.settings.directory_width =
            (self.settings.directory_width + delta).clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn reset_directory_width(&mut self) -> WorkbenchDomainChanges {
        self.settings.directory_width = DEFAULT_DIRECTORY_WIDTH;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    /// Updates `directory_width` interactively during drag without disk I/O; call
    /// [`Self::commit_directory_width`] on pointer release.
    pub fn drag_sidebar_width(
        &mut self,
        pointer_x: f32,
        viewport_w: f32,
        start_x: f32,
        start_w: f32,
    ) -> WorkbenchDomainChanges {
        let dx = pointer_x - start_x;
        let signed = if self.settings.directory_side == Side::Left {
            dx
        } else {
            -dx
        };
        let max = if viewport_w > 0. {
            (viewport_w * 0.45).clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)
        } else {
            MAX_DIRECTORY_WIDTH
        };
        self.settings.directory_width = (start_w + signed).clamp(MIN_DIRECTORY_WIDTH, max);
        WorkbenchDomainChanges {
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn commit_directory_width(&mut self) -> WorkbenchDomainChanges {
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn toggle_docked_directory(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.settings.directory_open = !self.settings.directory_open;
        self.preset_rt.record_user_edit(&self.settings);
        (
            self.settings.directory_open,
            WorkbenchDomainChanges {
                settings_changed: true,
                root_layout_changed: true,
                sidebar_changed: true,
                dock_changed: true,
                titlebar_changed: true,
                ..WorkbenchDomainChanges::empty()
            },
        )
    }

    pub fn ensure_docked_directory_open(&mut self) -> Option<WorkbenchDomainChanges> {
        if self.settings.directory_open {
            return None;
        }
        self.settings.directory_open = true;
        self.preset_rt.record_user_edit(&self.settings);
        Some(WorkbenchDomainChanges {
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        })
    }

    pub fn toggle_floating_dock(&mut self) -> WorkbenchDomainChanges {
        self.settings.floating_dock = !self.settings.floating_dock;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            root_layout_changed: true,
            viewport_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn toggle_compact_controls(&mut self) -> WorkbenchDomainChanges {
        self.settings.compact_controls = !self.settings.compact_controls;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn toggle_show_titlebar_meta(&mut self) -> WorkbenchDomainChanges {
        self.settings.show_titlebar_meta = !self.settings.show_titlebar_meta;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn toggle_remember_window_size(&mut self) -> WorkbenchDomainChanges {
        self.settings.remember_window_size = !self.settings.remember_window_size;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn toggle_always_on_top(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.settings.always_on_top = !self.settings.always_on_top;
        self.preset_rt.record_user_edit(&self.settings);
        let pinned = self.settings.always_on_top;
        (
            pinned,
            WorkbenchDomainChanges {
                settings_changed: true,
                titlebar_changed: true,
                always_on_top_changed: Some(pinned),
                ..WorkbenchDomainChanges::empty()
            },
        )
    }

    pub fn toggle_dock_button_visibility(&mut self, button_id: &str) -> WorkbenchDomainChanges {
        if let Some(pos) = self
            .settings
            .hidden_dock_buttons
            .iter()
            .position(|b| b.eq_ignore_ascii_case(button_id))
        {
            self.settings.hidden_dock_buttons.remove(pos);
        } else {
            self.settings.hidden_dock_buttons.push(button_id.to_owned());
        }
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn toggle_fake_ip_doh_fallback(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.settings.fake_ip_doh_fallback = !self.settings.fake_ip_doh_fallback;
        self.apply_doh_sync();
        self.preset_rt.record_user_edit(&self.settings);
        (
            self.settings.fake_ip_doh_fallback,
            WorkbenchDomainChanges::settings_only(),
        )
    }

    pub fn set_doh_resolver_preset(&mut self, preset: u8) -> Option<WorkbenchDomainChanges> {
        let resolvers = doh_resolvers_for_preset(preset)?;
        self.settings.doh_resolvers = resolvers;
        self.apply_doh_sync();
        self.preset_rt.record_user_edit(&self.settings);
        Some(WorkbenchDomainChanges::settings_only())
    }

    pub fn set_on_eof(&mut self, val: OnEof) -> WorkbenchDomainChanges {
        self.settings.on_eof = val;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn set_seek_step(&mut self, secs: f64) -> WorkbenchDomainChanges {
        self.settings.seek_step = secs.clamp(1.0, 60.0);
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn toggle_single_click_pause(&mut self) -> WorkbenchDomainChanges {
        self.settings.single_click_pause = !self.settings.single_click_pause;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn set_hwdec(&mut self, val: Hwdec) -> WorkbenchDomainChanges {
        self.settings.hwdec = val;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn toggle_resume_playback(&mut self) -> WorkbenchDomainChanges {
        self.settings.resume_playback = !self.settings.resume_playback;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn toggle_remember_speed(&mut self) -> WorkbenchDomainChanges {
        self.settings.remember_speed = !self.settings.remember_speed;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn toggle_screenshot_subs(&mut self) -> WorkbenchDomainChanges {
        self.settings.screenshot_subs = !self.settings.screenshot_subs;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn set_sub_lang(&mut self, val: SubLang) -> WorkbenchDomainChanges {
        self.settings.sub_lang = val;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn step_sub_scale(&mut self, delta: f64) -> (f64, WorkbenchDomainChanges) {
        let scale = (((self.settings.sub_scale + delta) * 100.).round() / 100.).clamp(0.5, 2.5);
        self.settings.sub_scale = scale;
        self.preset_rt.record_user_edit(&self.settings);
        (
            scale,
            WorkbenchDomainChanges {
                settings_changed: true,
                titlebar_changed: true,
                ..WorkbenchDomainChanges::empty()
            },
        )
    }

    pub fn reset_sub_scale(&mut self) -> (f64, WorkbenchDomainChanges) {
        self.settings.sub_scale = 1.0;
        self.preset_rt.record_user_edit(&self.settings);
        (
            1.0,
            WorkbenchDomainChanges {
                settings_changed: true,
                titlebar_changed: true,
                ..WorkbenchDomainChanges::empty()
            },
        )
    }

    pub fn set_video_wheel(&mut self, val: VideoWheel) -> WorkbenchDomainChanges {
        self.settings.video_wheel = val;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn set_audio_device(&mut self, dev_name: String) -> WorkbenchDomainChanges {
        self.settings.audio_device = dev_name;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            dock_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn set_fullscreen_hide_ms(&mut self, ms: u64) -> WorkbenchDomainChanges {
        self.settings.fullscreen_hide_ms = ms.clamp(1000, 10000);
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn set_photo_default(&mut self, val: PhotoDefault) -> WorkbenchDomainChanges {
        self.settings.photo_default = val;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn set_photo_double_click_actual(&mut self, val: bool) -> WorkbenchDomainChanges {
        self.settings.photo_double_click_actual = val;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges::settings_only()
    }

    pub fn toggle_photo_filmstrip(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.settings.photo_filmstrip = !self.settings.photo_filmstrip;
        self.preset_rt.record_user_edit(&self.settings);
        (
            self.settings.photo_filmstrip,
            WorkbenchDomainChanges {
                settings_changed: true,
                viewport_changed: true,
                dock_changed: true,
                titlebar_changed: true,
                ..WorkbenchDomainChanges::empty()
            },
        )
    }

    pub fn set_sort_preference(
        &mut self,
        sort_by: SortBy,
        sort_order: SortOrder,
    ) -> WorkbenchDomainChanges {
        self.settings.sort_by = sort_by;
        self.settings.sort_order = sort_order;
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            settings_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            viewport_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }

    pub fn set_plugin_enabled_preference(&mut self, id: &str, enabled: bool) {
        self.preset_rt
            .set_plugin_enabled(&mut self.settings, id, enabled);
    }

    /// Clears a plugin's disabled entry and falls back to `"graphite"` if that plugin owned `theme_id`.
    pub fn on_plugin_uninstalled(&mut self, id: &str) -> WorkbenchDomainChanges {
        self.preset_rt
            .set_plugin_enabled(&mut self.settings, id, true);
        if self.settings.theme_id == format!("plugin:{id}") {
            self.settings.theme_id = "graphite".into();
            self.preset_rt.record_user_edit(&self.settings);
            return WorkbenchDomainChanges {
                theme_changed: true,
                settings_changed: true,
                titlebar_changed: true,
                ..WorkbenchDomainChanges::empty()
            };
        }
        WorkbenchDomainChanges::settings_only()
    }

    /// Ensures `settings.theme_id` falls back to `"graphite"` if its backing `plugin:<id>` is missing.
    pub fn ensure_valid_plugin_theme(&mut self, has_plugin: impl FnOnce(&str) -> bool) -> bool {
        if let Some(pid) = self.settings.theme_id.strip_prefix("plugin:")
            && !has_plugin(pid)
        {
            self.settings.theme_id = "graphite".into();
            self.preset_rt.record_user_edit(&self.settings);
            return true;
        }
        false
    }
}
