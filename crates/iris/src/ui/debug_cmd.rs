//! Handler for the local `IRIS_CONTROL` debug / devtools pipe commands.

use std::path::Path;

use gpui::{Context, Window, px};

use super::state::DirectoryFilter;
use super::*;
use crate::platform;

pub(super) fn parse_mouse_command(line: &str) -> Option<(&str, f32, f32)> {
    let (cmd, arg) = line.split_once(' ')?;
    if cmd != "move" && cmd != "click" {
        return None;
    }
    let mut it = arg.split_whitespace().filter_map(|s| s.parse::<f32>().ok());
    Some((cmd, it.next()?, it.next()?))
}

impl IrisWindow {
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
                    window.resize(gpui::size(px(w), px(h)));
                    eprintln!("[iris/control] resize {w}x{h}");
                }
            }
            "open" => self.open_path(Path::new(arg), cx),
            "filter" => {
                self.filter = match arg {
                    "video" => DirectoryFilter::Videos,
                    "photo" => DirectoryFilter::Photos,
                    "favorites" => DirectoryFilter::Favorites,
                    _ => DirectoryFilter::All,
                };
                self.rebuild_directory_rows();
                eprintln!("[iris/control] filter {:?}", self.filter);
                cx.notify();
            }
            "state" => {
                let session = &self.active().session;
                let state = &session.state;
                eprintln!(
                    "[iris/control] state pos={:.2} dur={:.2} paused={} vol={} muted={} speed={:.2} loop={} aspect={:?} sub_delay={:.1} video={:?} hwdec={} dir={} dir_w={:.0} side={:?} popup={} settings={} info={} popover={:?} chrome={} fullscreen={} video_rect={:?} dock={:?} filter={:?} favorite={} renders={}",
                    state.time_pos,
                    state.duration,
                    state.paused,
                    state.volume,
                    state.muted,
                    state.speed,
                    state.loop_file,
                    state.aspect,
                    state.sub_delay,
                    state.video_size,
                    state.hwdec,
                    self.settings.directory_open,
                    self.settings.directory_width,
                    self.settings.directory_side,
                    self.overlays.dir_popup,
                    self.overlays.settings_open,
                    self.overlays.info_open,
                    self.overlays.popover,
                    self.chrome_visible,
                    window.is_fullscreen(),
                    self.active().rect.get().map(|r| r.0),
                    self.dock_bounds.get().map(|b| (
                        b.origin.x,
                        b.origin.y,
                        b.size.width,
                        b.size.height
                    )),
                    self.filter,
                    session
                        .current
                        .as_deref()
                        .is_some_and(|p| self.store.favorite(p)),
                    self.render_count,
                );
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            "get" => match self.active().session.debug_property(arg) {
                Ok(v) => eprintln!("[iris/control] get {arg} = {v}"),
                Err(e) => eprintln!("[iris/control] get {arg} failed: {e}"),
            },
            #[cfg(any(debug_assertions, feature = "devtools"))]
            "mpv" => {
                let args: Vec<&str> = arg.split_whitespace().collect();
                match self.active().session.debug_command(&args) {
                    Ok(()) => eprintln!("[iris/control] mpv {arg} ok"),
                    Err(e) => eprintln!("[iris/control] mpv {arg} failed: {e}"),
                }
            }
            "quit" => cx.quit(),
            _ => eprintln!("[iris/control] unknown command: {line}"),
        }
    }
}
