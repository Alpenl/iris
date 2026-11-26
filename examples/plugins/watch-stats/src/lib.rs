//! Per-window session timer: no network or filesystem capability. Persisted
//! asynchronous state is demonstrated by the official Guides/WebDAV sources.
use iris_plugin_sdk::{
    api::{
        StatusBadge,
        v2::{Generation, HostCommand, HostEvent, Id},
    },
    serde_json::Value,
};
use std::collections::BTreeMap;
#[derive(Default)]
pub struct Stats {
    windows: BTreeMap<Id, Timer>,
}
#[derive(Default)]
struct Timer {
    generation: Generation,
    last: Option<u64>,
    watched: u64,
}
impl Stats {
    pub fn event(&mut self, event: HostEvent<Value>, now: u64) -> Vec<HostCommand> {
        match event {
            HostEvent::WindowClosed { window } => {
                self.windows.remove(&window);
                Vec::new()
            }
            HostEvent::Shutdown => {
                self.windows.clear();
                Vec::new()
            }
            HostEvent::Window {
                window,
                generation,
                body,
            } => {
                if !self.windows.contains_key(&window) && self.windows.len() >= 4 {
                    return Vec::new();
                }
                let timer = self.windows.entry(window).or_default();
                if timer.generation != generation {
                    timer.generation = generation;
                    timer.last = None;
                }
                match *body {
                    HostEvent::Tick { paused, .. } => {
                        if !paused && let Some(last) = timer.last {
                            timer.watched = timer
                                .watched
                                .saturating_add(now.saturating_sub(last).min(5000));
                        }
                        timer.last = (!paused).then_some(now);
                    }
                    HostEvent::StateChanged { snapshot, .. } => {
                        if snapshot.paused || (snapshot.media.is_none() && snapshot.uri.is_none()) {
                            if let Some(last) = timer.last.take() {
                                timer.watched = timer
                                    .watched
                                    .saturating_add(now.saturating_sub(last).min(5000));
                            }
                        } else if timer.last.is_none() {
                            timer.last = Some(now);
                        }
                    }
                    HostEvent::WindowOpened { .. } | HostEvent::MediaOpened { .. } => {
                        timer.last = None
                    }
                    HostEvent::Command { id, .. } | HostEvent::Action { id, .. }
                        if id == "reset" =>
                    {
                        timer.watched = 0;
                        timer.last = None;
                    }
                    _ => return Vec::new(),
                }
                vec![HostCommand::SetStatus {
                    badge: Some(Box::new(StatusBadge {
                        id: "watch-timer".into(),
                        text: format!("本窗口已看 {} 秒", timer.watched / 1000),
                        tooltip: Some("点击重置本次窗口计时".into()),
                        accent: false,
                        action_id: Some("reset".into()),
                    })),
                }]
            }
            _ => Vec::new(),
        }
    }
}
iris_plugin_sdk::export_typed_plugin!(Stats, |app, event| {
    let now = iris_plugin_sdk::guest::clock(true)?.0;
    Ok(app.event(event, now))
});

#[cfg(test)]
mod tests {
    use super::Stats;
    use iris_plugin_sdk::{
        api::v2::{Generation, HostCommand, HostEvent, Id, source::PlaybackSnapshot},
        serde_json::Value,
    };

    fn scoped(window: u64, generation: Generation, body: HostEvent<Value>) -> HostEvent<Value> {
        HostEvent::Window {
            window: Id(window),
            generation,
            body: Box::new(body),
        }
    }

    fn tick(paused: bool) -> HostEvent<Value> {
        HostEvent::Tick {
            generation: Generation::default(),
            position: 0.0,
            duration: 60.0,
            paused,
        }
    }

    fn badge_text(cmds: Vec<HostCommand>) -> String {
        match cmds.as_slice() {
            [HostCommand::SetStatus { badge: Some(badge) }] => badge.text.clone(),
            other => panic!("expected single SetStatus command, got {other:?}"),
        }
    }

    #[test]
    fn ticks_accumulate_per_window_respect_pause_and_clamp_gaps() {
        let mut stats = Stats::default();
        let gen0 = Generation::default();

        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 1_000)),
            "本窗口已看 0 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 3_000)),
            "本窗口已看 2 秒"
        );
        // Large gaps are clamped to 5 seconds per tick.
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 60_000)),
            "本窗口已看 7 秒"
        );
        // Paused ticks stop accumulation and clear `last` so resume does not count paused time.
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(true)), 62_000)),
            "本窗口已看 7 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 70_000)),
            "本窗口已看 7 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 72_500)),
            "本窗口已看 9 秒"
        );
        // Window 2 stays isolated from window 1.
        assert_eq!(
            badge_text(stats.event(scoped(2, gen0, tick(false)), 72_500)),
            "本窗口已看 0 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(2, gen0, tick(false)), 73_500)),
            "本窗口已看 1 秒"
        );
    }

    #[test]
    fn generation_reset_commands_and_window_limits_work_cleanly() {
        let mut stats = Stats::default();
        let gen0 = Generation::default();
        let gen1 = Generation {
            media: Id(1),
            ..gen0
        };

        stats.event(scoped(1, gen0, tick(false)), 1_000);
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 4_000)),
            "本窗口已看 3 秒"
        );
        // Generation bump clears `last` without resetting accumulated watched time.
        assert_eq!(
            badge_text(stats.event(scoped(1, gen1, tick(false)), 6_000)),
            "本窗口已看 3 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(1, gen1, tick(false)), 7_000)),
            "本窗口已看 4 秒"
        );
        // MediaOpened clears `last`.
        assert_eq!(
            badge_text(stats.event(
                scoped(
                    1,
                    gen1,
                    HostEvent::MediaOpened {
                        generation: gen1,
                        snapshot: PlaybackSnapshot::default(),
                    },
                ),
                8_000,
            )),
            "本窗口已看 4 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(1, gen1, tick(false)), 11_000)),
            "本窗口已看 4 秒"
        );
        // Reset command zeroes the counter.
        assert_eq!(
            badge_text(stats.event(
                scoped(
                    1,
                    gen1,
                    HostEvent::Action {
                        id: "reset".into(),
                        value: None,
                        snapshot: None,
                    },
                ),
                12_000,
            )),
            "本窗口已看 0 秒"
        );
        // At most 4 windows are tracked; closing one frees a slot and drops its timer.
        for w in 2..=4 {
            assert!(!stats.event(scoped(w, gen0, tick(false)), 12_000).is_empty());
        }
        assert!(stats.event(scoped(5, gen0, tick(false)), 12_000).is_empty());
        assert!(
            stats
                .event(HostEvent::WindowClosed { window: Id(1) }, 13_000)
                .is_empty()
        );
        assert_eq!(
            badge_text(stats.event(scoped(5, gen0, tick(false)), 13_000)),
            "本窗口已看 0 秒"
        );
    }

    #[test]
    fn state_changed_pause_stops_accumulation_when_host_omits_paused_ticks() {
        let mut stats = Stats::default();
        let gen0 = Generation::default();

        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 1_000)),
            "本窗口已看 0 秒"
        );
        // Host emits StateChanged(paused=true) at 2.5s and stops Tick completely while paused.
        assert_eq!(
            badge_text(stats.event(
                scoped(
                    1,
                    gen0,
                    HostEvent::StateChanged {
                        generation: gen0,
                        snapshot: PlaybackSnapshot {
                            media: Some(Id(1)),
                            paused: true,
                            ..Default::default()
                        },
                    },
                ),
                2_500,
            )),
            "本窗口已看 1 秒"
        );
        // Resume 30 seconds later via StateChanged(paused=false), followed by Tick(false) 1s later.
        assert_eq!(
            badge_text(stats.event(
                scoped(
                    1,
                    gen0,
                    HostEvent::StateChanged {
                        generation: gen0,
                        snapshot: PlaybackSnapshot {
                            media: Some(Id(1)),
                            paused: false,
                            ..Default::default()
                        },
                    },
                ),
                32_500,
            )),
            "本窗口已看 1 秒"
        );
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 33_500)),
            "本窗口已看 2 秒"
        );
        assert!(stats.event(HostEvent::Shutdown, 34_000).is_empty());
        assert_eq!(
            badge_text(stats.event(scoped(1, gen0, tick(false)), 35_000)),
            "本窗口已看 0 秒"
        );
    }
}
