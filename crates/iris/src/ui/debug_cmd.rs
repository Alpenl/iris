//! Handler for the local `IRIS_CONTROL` debug / devtools pipe commands,
//! delegating headless diagnostic snapshots & debug commands to [`iris_app::AppModel`].

use std::path::Path;

use gpui::{Context, Window, px, size};

use super::WindowShell;
use super::views::DirtyRegions;
use crate::platform;

pub(super) fn parse_mouse_command(line: &str) -> Option<(&str, f32, f32)> {
    let (cmd, arg) = line.split_once(' ')?;
    if cmd != "move" && cmd != "click" {
        return None;
    }
    let mut it = arg.split_whitespace().filter_map(|s| s.parse::<f32>().ok());
    Some((cmd, it.next()?, it.next()?))
}

impl WindowShell {
    pub(super) fn debug_command(
        &mut self,
        line: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut parts = line.splitn(2, ' ');
        let (cmd, arg) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or("").trim(),
        );
        match cmd {
            "snap" => match platform::snapshot_window(window, Path::new(arg)) {
                Ok(()) => eprintln!("[iris/control] snap {arg} ok"),
                Err(e) => eprintln!("[iris/control] snap failed: {e:#}"),
            },
            "resize" => {
                let mut it = arg.split_whitespace().filter_map(|s| s.parse::<f32>().ok());
                if let (Some(w), Some(h)) = (it.next(), it.next()) {
                    window.resize(size(px(w), px(h)));
                    eprintln!("[iris/control] resize {w}x{h}");
                }
            }
            "open" => self.open_path(Path::new(arg), cx),
            "filter" => {
                let applied = self.app.debug_apply_filter(arg);
                self.rebuild_directory_rows();
                self.scan_visible_favorites(cx);
                eprintln!("[iris/control] filter {:?}", applied);
                self.notify_regions(
                    DirtyRegions {
                        sidebar: true,
                        viewport: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            "state" => {
                let snap = self.app.debug_state_snapshot();
                let vp_size = window.viewport_size();
                eprintln!(
                    "[iris/control] state pos={:.2} dur={:.2} paused={} vol={} muted={} speed={:.2} loop={} aspect={:?} sub_delay={:.1} video={:?} hwdec={} dir={} dir_w={:.0} side={:?} popup={} settings={} info={} popover={:?} chrome={} fullscreen={} video_rect={:?} dock={:?} filter={:?} favorite={} renders={} win=({:.1},{:.1})",
                    snap.time_pos,
                    snap.duration,
                    snap.paused,
                    snap.volume,
                    snap.muted,
                    snap.speed,
                    snap.loop_file,
                    snap.aspect,
                    snap.sub_delay,
                    snap.video_size,
                    snap.hwdec,
                    snap.directory_open,
                    snap.directory_width,
                    snap.directory_side,
                    self.overlays.dir_popup.is_open(),
                    self.overlays.settings_open,
                    self.overlays.info_open,
                    self.overlays.popover,
                    self.chrome.chrome_visible,
                    window.is_fullscreen(),
                    self.active().rect.get().map(|r| r.0),
                    self.chrome.probes.dock_bounds.get().map(|b| (
                        b.origin.x,
                        b.origin.y,
                        b.size.width,
                        b.size.height
                    )),
                    snap.filter,
                    snap.favorite,
                    self.chrome.render_count.get(),
                    f32::from(vp_size.width),
                    f32::from(vp_size.height),
                );
            }
            "ui" => {
                let snap = self.app.debug_ui_snapshot();
                eprintln!(
                    "[iris/control] ui preset={} seek_step={:.1} top={} filmstrip={} thumbs={} sb_mode={:?} rows={} queue={} search_open={} search_q={:?} palette={} palette_q={:?} scheme={:?} theme={} hide_tb={} tb_hover={} ctx_menu={} modal={} sources={} osd={:?} toast={:?}",
                    snap.active_preset,
                    snap.seek_step,
                    snap.always_on_top,
                    snap.photo_filmstrip,
                    self.catalog_ui.thumbs.len(),
                    snap.sidebar_mode,
                    snap.rows_len,
                    snap.queue_len,
                    self.text_inputs.search_open,
                    self.text_inputs.search_input.text(),
                    self.overlays.palette_open,
                    self.text_inputs.palette_input.text(),
                    snap.keymap_scheme,
                    snap.theme_id,
                    snap.hide_titlebar,
                    self.chrome.titlebar_hover,
                    self.overlays.context_menu.is_some(),
                    snap.modal_open,
                    snap.sources_len,
                    self.chrome.osd,
                    self.chrome.toast,
                );
            }
            "get" => match self.app.debug_mpv_property(arg) {
                Ok(v) => eprintln!("[iris/control] get {arg} = {v}"),
                Err(e) => eprintln!("[iris/control] get {arg} failed: {e}"),
            },
            "mpv" => {
                let args: Vec<&str> = arg.split_whitespace().collect();
                match self.app.debug_mpv_command(&args) {
                    Ok(()) => eprintln!("[iris/control] mpv {arg} ok"),
                    Err(e) => eprintln!("[iris/control] mpv {arg} failed: {e}"),
                }
            }
            "sim_device_reset" => {
                if let Some(v) = self.active().video.borrow().as_ref() {
                    if v.debug_force_device_reset() {
                        eprintln!("[iris/control] sim_device_reset ok");
                    } else {
                        eprintln!("[iris/control] sim_device_reset unsupported");
                    }
                }
            }
            "quit" => cx.quit(),
            _ => eprintln!("[iris/control] unknown command: {line}"),
        }
    }
}
