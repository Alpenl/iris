//! Fast zero-libmpv unit tests for [`super::PlaybackOrchestrator`], [`super::PendingSubtitles`],
//! GPU reset recovery predicates, sleep/NowPlaying targets, fixed event sequences, and
//! [`super::WindowShutdownCoordinator`] two-phase teardown ordering.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::player::{
    AspectMode, Chapter, Command, MediaStats, PlaybackState, PlayerActionError, SessionEvent,
    Track, TrackKind,
};
use iris_core::settings::{OnEof, PhotoDefault, Settings};

use super::{
    EofOutcome, MediaKeyInput, MediaKeyOutcome, NowPlayingSnapshot, PendingSubtitles,
    PlaybackDomainChanges, PlaybackOrchestrator, PlaybackSessionPort, STATS_REFRESH_INTERVAL,
    SubtitleFlushOutcome, VOLUME_ANNOUNCE_WINDOW, WindowShutdownCoordinator,
    should_prevent_display_sleep, should_recover_after_gpu_reset, split_open_paths,
};

struct FakeSession {
    state: PlaybackState,
    kind: Option<Kind>,
    local_path: Option<PathBuf>,
    uri: Option<String>,
    title: Option<String>,
    share_identity: bool,
    display_size: (i64, i64),
    display_rotation: u16,
    tracks: Vec<Track>,
    stats: MediaStats,
    full_refreshes: Cell<usize>,
    flushed_deferred: usize,
    recovered: Vec<(f64, [f64; 2])>,
    commands: Vec<String>,
    fail_next: Option<PlayerActionError>,
    fail_subtitle_name: Option<String>,
    shutdowns: Cell<usize>,
}

impl Default for FakeSession {
    fn default() -> Self {
        Self {
            state: PlaybackState {
                loaded: true,
                duration: 180.0,
                time_pos: 20.0,
                volume: 80.0,
                speed: 1.0,
                subs_visible: true,
                hwdec: "d3d11va".into(),
                ..Default::default()
            },
            kind: Some(Kind::Video),
            local_path: None,
            uri: None,
            title: Some("Sample Episode".into()),
            share_identity: true,
            display_size: (1920, 1080),
            display_rotation: 0,
            tracks: Vec::new(),
            stats: MediaStats {
                video_codec: "hevc".into(),
                dropped_frames: Some(0),
                ..Default::default()
            },
            full_refreshes: Cell::new(0),
            flushed_deferred: 0,
            recovered: Vec::new(),
            commands: Vec::new(),
            fail_next: None,
            fail_subtitle_name: None,
            shutdowns: Cell::new(0),
        }
    }
}

impl PlaybackSessionPort for FakeSession {
    type Update = SessionEvent;

    fn coalesce_updates(batch: Vec<SessionEvent>) -> Vec<SessionEvent> {
        batch
    }

    fn state(&self) -> &PlaybackState {
        &self.state
    }

    fn kind(&self) -> Option<Kind> {
        self.kind
    }

    fn current(&self) -> Option<&Path> {
        self.local_path.as_deref()
    }

    fn current_uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }

    fn current_media_uri(&self) -> Option<MediaUri> {
        self.local_path.clone().map(MediaUri::Local)
    }

    fn current_title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn may_share_current_identity(&self) -> bool {
        self.share_identity
    }

    fn display_size(&self) -> (i64, i64) {
        self.display_size
    }

    fn display_rotation(&self) -> u16 {
        self.display_rotation
    }

    fn display_flips(&self) -> (bool, bool) {
        (self.state.hflip, self.state.vflip)
    }

    fn tracks_of(&self, kind: TrackKind) -> Vec<&Track> {
        self.tracks.iter().filter(|t| t.kind == kind).collect()
    }

    fn selected_track(&self, kind: TrackKind) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == kind && t.selected)
    }

    fn media_stats(&self) -> MediaStats {
        self.full_refreshes.set(self.full_refreshes.get() + 1);
        self.stats.clone()
    }

    fn refresh_dynamic_stats(&self, stats: &mut MediaStats) {
        stats.dropped_frames = Some(stats.dropped_frames.unwrap_or(0) + 1);
    }

    fn apply_update<'a>(&mut self, ev: &'a SessionEvent) -> Option<&'a SessionEvent> {
        match ev {
            SessionEvent::Loaded => {
                self.state.loaded = true;
                self.state.ended = false;
            }
            SessionEvent::Paused(p) => self.state.paused = *p,
            SessionEvent::Speed(s) => self.state.speed = *s,
            SessionEvent::Position(t) => self.state.time_pos = *t,
            SessionEvent::Duration(d) => self.state.duration = *d,
            SessionEvent::CacheDuration(c) => self.state.cache_duration = *c,
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Buffering(b) => self.state.buffering = *b,
            SessionEvent::Ended(e) => self.state.ended = *e,
            SessionEvent::Audio(raw_id) => {
                let target = raw_id.parse::<i64>().ok();
                for t in &mut self.tracks {
                    if t.kind == TrackKind::Audio {
                        t.selected = Some(t.id) == target;
                    }
                }
            }
            SessionEvent::SubtitleTrack(raw_id) => {
                let target = raw_id.parse::<i64>().ok();
                for t in &mut self.tracks {
                    if t.kind == TrackKind::Sub {
                        t.selected = Some(t.id) == target;
                    }
                }
            }
            _ => {}
        }
        Some(ev)
    }

    fn flush_deferred(&mut self) {
        self.flushed_deferred += 1;
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        if let Some(err) = self.fail_next.take() {
            return Err(anyhow::Error::new(err));
        }
        match &command {
            Command::CycleAbLoop => self.commands.push("CycleAbLoop".into()),
            Command::ClearAbLoop => self.commands.push("ClearAbLoop".into()),
            Command::TogglePlayback => {
                self.state.paused = !self.state.paused;
                self.state.ended = false;
                self.commands.push("TogglePlayback".into());
            }
            Command::SeekRelative(d) => self.commands.push(format!("SeekRelative({d})")),
            Command::SeekAbsolute(s) => self.commands.push(format!("SeekAbsolute({s})")),
            Command::FrameStep => self.commands.push("FrameStep".into()),
            Command::FrameBackStep => self.commands.push("FrameBackStep".into()),
            Command::VolumeBy(d) => {
                self.state.volume = (self.state.volume + d).clamp(0., 100.);
                self.commands.push(format!("VolumeBy({d})"));
            }
            Command::ToggleMute => {
                self.state.muted = !self.state.muted;
                self.commands.push("ToggleMute".into());
            }
            Command::SetSpeed(s) => {
                self.state.speed = *s;
                self.commands.push(format!("SetSpeed({s:.2})"));
            }
            Command::SetLoop(on) => {
                self.state.loop_file = *on;
                self.commands.push(format!("SetLoop({on})"));
            }
            Command::ToggleSubtitles => {
                self.state.subs_visible = !self.state.subs_visible;
                self.commands.push("ToggleSubtitles".into());
            }
            Command::NextSubtitle => self.commands.push("NextSubtitle".into()),
            Command::SubSeekNext => self.commands.push("SubSeekNext".into()),
            Command::SubSeekPrev => self.commands.push("SubSeekPrev".into()),
            Command::SetSubDelay(d) => {
                self.state.sub_delay = *d;
                self.commands.push(format!("SetSubDelay({d:.2})"));
            }
            Command::NextAudio => self.commands.push("NextAudio".into()),
            Command::NextChapter => self.commands.push("NextChapter".into()),
            Command::PrevChapter => self.commands.push("PrevChapter".into()),
            Command::SetAspect(a) => {
                self.state.aspect = *a;
                self.commands.push(format!("SetAspect({a:?})"));
            }
            Command::RotateMedia(deg) => {
                let next = (i64::from(self.display_rotation) + *deg).rem_euclid(360);
                self.display_rotation = next as u16;
                self.commands.push(format!("RotateMedia({deg})"));
            }
            Command::ToggleHFlip => {
                if self.display_rotation % 180 == 90 {
                    self.state.vflip = !self.state.vflip;
                } else {
                    self.state.hflip = !self.state.hflip;
                }
                self.commands.push("ToggleHFlip".into());
            }
            Command::ToggleVFlip => {
                if self.display_rotation % 180 == 90 {
                    self.state.hflip = !self.state.hflip;
                } else {
                    self.state.vflip = !self.state.vflip;
                }
                self.commands.push("ToggleVFlip".into());
            }
            Command::LoadSubtitle(path) => {
                if let Some(bad) = &self.fail_subtitle_name
                    && path.to_string_lossy().contains(bad)
                {
                    return Err(anyhow::Error::new(PlayerActionError::SubtitleFileNotFound));
                }
                self.commands.push(format!(
                    "LoadSubtitle({})",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
            _ => self.commands.push("Other".into()),
        }
        Ok(())
    }

    fn recover_unloaded_after_gpu_reset(&mut self, _settings: &Settings, zoom: f64, pan: [f64; 2]) {
        self.recovered.push((zoom, pan));
    }

    fn shutdown(&self) {
        self.shutdowns.set(self.shutdowns.get() + 1);
    }
}

#[test]
fn speed_ab_loop_seek_sub_delay_and_transform_actions_cover_success_and_failure_paths() {
    let t0 = Instant::now();
    let mut ctrl = PlaybackOrchestrator::new(FakeSession::default(), t0);

    // 1. Speed step up/down & reset
    let up = ctrl.step_speed(1);
    assert_eq!(up.osd.as_deref(), Some("倍速 · 1.25×"));
    assert!(up.error.is_none());
    let down = ctrl.step_speed(-1);
    assert_eq!(down.osd.as_deref(), Some("倍速 · 1.00×"));
    let rst = ctrl.reset_speed();
    assert_eq!(rst.osd.as_deref(), Some("倍速 · 1.00×"));

    // 2. Native A-B loop cycle & clear
    assert!(ctrl.cycle_ab_loop().error.is_none());
    assert!(ctrl.clear_ab_loop().error.is_none());
    assert!(ctrl.session().commands.contains(&"CycleAbLoop".to_owned()));
    assert!(ctrl.session().commands.contains(&"ClearAbLoop".to_owned()));

    // 3. Relative & large seek target clamping
    let back = ctrl.seek_relative_step(25.0, false);
    assert_eq!(back.osd.as_deref(), Some("-25s · 00:00"));
    let fwd_large = ctrl.seek_relative_large(30.0, true);
    assert_eq!(fwd_large.osd.as_deref(), Some("+120s · 02:20"));

    // 4. Subtitle delay centisecond rounding without float drift
    for _ in 0..3 {
        let _ = ctrl.adjust_sub_delay(0.1);
    }
    assert_eq!(ctrl.session().state().sub_delay, 0.3);
    let back_delay = ctrl.adjust_sub_delay(-0.1);
    assert_eq!(back_delay.osd.as_deref(), Some("字幕偏移 · +0.2s"));
    assert_eq!(
        ctrl.reset_sub_delay().osd.as_deref(),
        Some("字幕偏移 · 0.0s")
    );

    // 5. Empty track/chapter guards vs deferred OSD announcements
    assert_eq!(
        ctrl.cycle_subtitle_track().osd.as_deref(),
        Some("当前视频没有字幕轨")
    );
    assert_eq!(
        ctrl.seek_subtitle_cue(true).osd.as_deref(),
        Some("请先选择字幕轨道")
    );
    assert_eq!(
        ctrl.cycle_audio_track().osd.as_deref(),
        Some("当前视频没有其他音轨")
    );
    assert_eq!(
        ctrl.step_chapter(true).osd.as_deref(),
        Some("当前视频没有章节")
    );

    ctrl.session_mut().state.chapters.push(Chapter {
        time: 0.0,
        title: "Intro".into(),
    });
    assert_eq!(ctrl.step_chapter(true).osd.as_deref(), Some("下一章节"));

    // 6. Rotation resets image view; display flip accounts for 90-degree rotation!
    let rot = ctrl.rotate_media(90, "顺时针旋转 90°");
    assert!(rot.reset_image_view);
    let flip_h = ctrl.flip_display_axis(true);
    assert_eq!(flip_h.osd.as_deref(), Some("水平镜像"));
    assert_eq!(flip_h.flip_pan_axis, Some(0));
    assert_eq!(ctrl.cycle_aspect().osd.as_deref(), Some("画面比例 · 16:9"));
    assert_eq!(ctrl.session().state().aspect, AspectMode::Ratio16x9);

    // 7. Command failure path surfaces `error` cleanly without mutating deferred announce flags
    ctrl.session_mut().fail_next = Some(PlayerActionError::NotSeekable);
    assert!(ctrl.adjust_volume_by(5.0, t0).is_err());
    assert!(ctrl.announce_volume.is_none());
}

#[test]
fn fixed_playback_event_sequence_produces_exact_domain_changes_osd_and_sleep_targets() {
    let t0 = Instant::now();
    let mut session = FakeSession::default();
    session.state.loaded = false;
    session.state.paused = true;
    session.tracks = vec![
        Track {
            id: 1,
            kind: TrackKind::Audio,
            title: "Main".into(),
            lang: "jpn".into(),
            codec: "aac".into(),
            external: false,
            selected: true,
        },
        Track {
            id: 2,
            kind: TrackKind::Audio,
            title: "Commentary".into(),
            lang: "eng".into(),
            codec: "aac".into(),
            external: false,
            selected: false,
        },
        Track {
            id: 1,
            kind: TrackKind::Sub,
            title: "CHS".into(),
            lang: "zh".into(),
            codec: "ass".into(),
            external: true,
            selected: false,
        },
    ];
    let mut ctrl = PlaybackOrchestrator::new(session, t0);

    // Step 1: Load (`SessionEvent::Loaded` while paused -> sleep preventer stays false)
    let r_load =
        ctrl.process_session_updates(vec![SessionEvent::Loaded], false, false, OnEof::Pause, t0);
    assert_eq!(
        r_load.changes,
        PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            loaded_or_reconfigured: true,
            ..Default::default()
        }
    );
    assert!(r_load.should_flush_subtitles);
    assert!(!r_load.sleep_prevent_active);
    assert_eq!(
        r_load.now_playing_updated,
        Some(Some(NowPlayingSnapshot {
            title: "Sample Episode".into(),
            is_video: true,
        }))
    );

    // Step 2: Unpause (`SessionEvent::Paused(false)` -> sleep preventer activates!)
    let r_play = ctrl.process_session_updates(
        vec![SessionEvent::Paused(false)],
        false,
        false,
        OnEof::Pause,
        t0 + Duration::from_millis(100),
    );
    assert_eq!(
        r_play.changes,
        PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            pause_or_speed_changed: true,
            ..Default::default()
        }
    );
    assert!(r_play.sleep_prevent_active);
    assert!(r_play.now_playing_updated.is_some());

    // Step 3: Seek (`SessionEvent::Seek` + `Position(42.0)` within 2s -> NowPlaying throttled!)
    let r_seek = ctrl.process_session_updates(
        vec![SessionEvent::Seek, SessionEvent::Position(42.0)],
        false,
        false,
        OnEof::Pause,
        t0 + Duration::from_millis(300),
    );
    assert_eq!(
        r_seek.changes,
        PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            seeked: true,
            ..Default::default()
        }
    );
    assert!(r_seek.clock_changed);
    assert_eq!(r_seek.position, Some(42.0));
    assert!(r_seek.now_playing_updated.is_none());

    // Step 4: Speed (`SessionEvent::Speed(1.5)`)
    let r_speed = ctrl.process_session_updates(
        vec![SessionEvent::Speed(1.5)],
        false,
        false,
        OnEof::Pause,
        t0 + Duration::from_millis(400),
    );
    assert_eq!(
        r_speed.changes,
        PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            pause_or_speed_changed: true,
            ..Default::default()
        }
    );

    // Step 5: Subtitle switch + Audio switch + Volume announce (with 1s window check)
    let _ = ctrl.cycle_subtitle_track();
    let _ = ctrl.cycle_audio_track();
    ctrl.adjust_volume_by(5.0, t0 + Duration::from_millis(500))
        .unwrap();
    let r_tracks = ctrl.process_session_updates(
        vec![
            SessionEvent::SubtitleTrack("1".into()),
            SessionEvent::Audio("2".into()),
            SessionEvent::Volume(85.0),
        ],
        false,
        false,
        OnEof::Pause,
        t0 + Duration::from_millis(600),
    );
    assert!(r_tracks.changes.subtitle_track_changed);
    assert!(r_tracks.changes.audio_track_changed);
    assert!(r_tracks.changes.volume_changed);
    assert!(r_tracks.changes.affects_transport_popover());
    assert_eq!(
        r_tracks.osd_messages,
        vec![
            "音轨 · Commentary · eng · AAC",
            "字幕 · CHS · zh · ASS [外挂]",
            "音量 · 85%",
        ]
    );

    // Expired volume announcement (> 1s) must not emit stale OSD.
    ctrl.adjust_volume_by(-5.0, t0).unwrap();
    let r_late_vol = ctrl.process_session_updates(
        vec![SessionEvent::Volume(80.0)],
        false,
        false,
        OnEof::Pause,
        t0 + VOLUME_ANNOUNCE_WINDOW + Duration::from_millis(10),
    );
    assert!(r_late_vol.osd_messages.is_empty());

    // Step 6: EOF transition (`OnEof::Next` steps queue; `loop_file` settles without stepping)
    let r_eof_next = ctrl.process_session_updates(
        vec![SessionEvent::Ended(true)],
        false,
        false,
        OnEof::Next,
        t0 + Duration::from_secs(3),
    );
    assert_eq!(r_eof_next.eof_outcome, EofOutcome::SaveAndStepNext);
    assert!(!r_eof_next.sleep_prevent_active);

    // Playback errors are suppressed when `scoped_released` or `media_load_pending` is true,
    // and surfaced otherwise.
    let suppressed = ctrl.process_session_updates(
        vec![SessionEvent::Error(-13)],
        false,
        true,
        OnEof::Pause,
        t0 + Duration::from_secs(4),
    );
    assert_eq!(suppressed.error_code, None);
    let surfaced = ctrl.process_session_updates(
        vec![SessionEvent::Error(-13)],
        false,
        false,
        OnEof::Pause,
        t0 + Duration::from_secs(4),
    );
    assert_eq!(surfaced.error_code, Some(-13));
    assert!(surfaced.changes.error_surfaced);
}

#[test]
fn load_generation_fencing_pending_subtitles_and_gpu_reset_predicates_reject_stale_work() {
    let base = std::env::temp_dir().join("iris-r10-playback-unit");
    let first_mkv = base.join("first.mkv");
    let sub_a = base.join("A.srt");
    let sub_b = base.join("bad_sub.ass");
    let second_mp4 = base.join("second.mp4");

    let (media, subs) = split_open_paths(vec![
        sub_a.clone(),
        first_mkv.clone(),
        sub_b.clone(),
        second_mp4.clone(),
    ]);
    assert_eq!(media, vec![first_mkv.clone(), second_mp4.clone()]);
    assert_eq!(subs, vec![sub_a.clone(), sub_b.clone()]);

    let mut pending = PendingSubtitles::default();
    pending.add(1, vec![sub_a.clone()]);
    pending.add(2, vec![sub_a.clone()]);
    pending.add(2, vec![sub_b.clone()]);
    pending.advance(2, 3);
    assert_eq!(pending.take(3), vec![sub_a.clone(), sub_b.clone()]);
    pending.add(4, vec![sub_a.clone()]);
    assert_eq!(pending.take(5), Vec::<PathBuf>::new());

    let t0 = Instant::now();
    let mut ctrl = PlaybackOrchestrator::new(FakeSession::default(), t0);

    // Start Load #1 (`first.mkv`), attach subtitles, then supersede with Load #2 (`second.mp4`)!
    let gen1 = ctrl.begin_local_load(Some(first_mkv));
    assert!(!ctrl.enqueue_subtitles(vec![sub_a.clone()], true));
    let gen2 = ctrl.begin_local_load(Some(second_mp4));
    assert_ne!(gen1, gen2);
    // Stale 150ms loading timer and stale preparation completion for `gen1` are discarded!
    assert!(!ctrl.maybe_show_loading_indicator(gen1));
    assert!(!ctrl.accept_prepared_load(gen1));
    assert!(ctrl.maybe_show_loading_indicator(gen2));
    assert!(ctrl.loading_visible());

    // Attach `[sub_a, sub_b]` to `gen2`; `accept_prepared_load(gen2)` advances them onto committed `gen3`!
    let _ = ctrl.enqueue_subtitles(vec![sub_a, sub_b], true);
    assert!(ctrl.accept_prepared_load(gen2));
    assert!(!ctrl.loading_visible());
    assert_eq!(ctrl.pending_path(), None);

    // Simulate partial subtitle failure on `bad_sub.ass`.
    ctrl.session_mut().fail_subtitle_name = Some("bad_sub".into());
    match ctrl.flush_pending_subtitles() {
        SubtitleFlushOutcome::PartialFailure { count, failed, .. } => {
            assert_eq!(count, 2);
            assert_eq!(failed, 1);
        }
        other => panic!("expected PartialFailure, got {other:?}"),
    }

    // Photo open arms `pending_photo_actual` only when `PhotoDefault::Actual`, and stats refresh honors 1s throttle.
    ctrl.session_mut().kind = Some(Kind::Photo);
    ctrl.on_media_open_succeeded(PhotoDefault::Actual);
    assert!(ctrl.take_pending_photo_actual());
    assert!(!ctrl.take_pending_photo_actual());
    ctrl.refresh_cached_stats(true, t0);
    assert!(!ctrl.refresh_stats_if_due(t0 + Duration::from_millis(400)));
    assert!(ctrl.refresh_stats_if_due(t0 + STATS_REFRESH_INTERVAL));
    assert_eq!(ctrl.cached_stats().and_then(|s| s.dropped_frames), Some(1));

    // GPU reset recovery predicate & method:
    let cur = ctrl.load_generation();
    assert!(!should_recover_after_gpu_reset(
        cur - 1,
        cur,
        Some(Kind::Video),
        true,
        false
    ));
    assert!(!should_recover_after_gpu_reset(
        cur,
        cur,
        Some(Kind::Video),
        true,
        true
    ));
    assert!(should_recover_after_gpu_reset(
        cur,
        cur,
        Some(Kind::Video),
        false,
        true
    ));
    assert!(should_recover_after_gpu_reset(
        cur,
        cur,
        Some(Kind::Photo),
        true,
        true
    ));
    assert!(!ctrl.recover_after_gpu_reset(cur - 1, false, &Settings::default(), 2.0, [1.0, -1.0]));
    assert!(ctrl.recover_after_gpu_reset(cur, true, &Settings::default(), 2.0, [1.0, -1.0]));
    assert_eq!(ctrl.session().recovered, vec![(2.0, [1.0, -1.0])]);
}

#[test]
fn media_keys_sleep_preventer_and_media_info_badges_follow_session_state() {
    let mut ctrl = PlaybackOrchestrator::new(FakeSession::default(), Instant::now());
    assert_eq!(
        ctrl.media_info().as_deref(),
        Some("1920×1080 · 硬解 d3d11va")
    );
    assert!(should_prevent_display_sleep(
        Some(Kind::Video),
        ctrl.session().state()
    ));
    assert!(matches!(
        ctrl.handle_media_key(MediaKeyInput::Play),
        MediaKeyOutcome::Ignored
    ));
    assert!(matches!(
        ctrl.handle_media_key(MediaKeyInput::Pause),
        MediaKeyOutcome::Executed
    ));
    assert!(!ctrl.should_prevent_sleep());
    assert!(matches!(
        ctrl.handle_media_key(MediaKeyInput::Pause),
        MediaKeyOutcome::Ignored
    ));
    assert!(matches!(
        ctrl.handle_media_key(MediaKeyInput::Play),
        MediaKeyOutcome::Executed
    ));
    assert!(matches!(
        ctrl.handle_media_key(MediaKeyInput::Next),
        MediaKeyOutcome::StepQueue { delta: 1 }
    ));

    ctrl.session_mut().kind = Some(Kind::Audio);
    assert!(!ctrl.should_prevent_sleep());
    assert_eq!(ctrl.media_info().as_deref(), Some("音频"));
}

#[test]
fn window_shutdown_coordinator_enforces_video_surface_before_session_across_close_quit_and_drop() {
    let expected = vec![
        "1:sleep_preventer_off",
        "2:media_keys_released",
        "3:plugins_and_host_shutdown",
        "4:state_persisted",
        "5:video_surface_dropped",
        "6:session_shutdown",
    ];

    for triggers in [3usize, 2, 1] {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let surface_alive = Rc::new(Cell::new(true));
        let mut coord = WindowShutdownCoordinator::default();
        let ctrl = PlaybackOrchestrator::new(FakeSession::default(), Instant::now());

        let mut invoke = || {
            coord.run_once(
                || log.borrow_mut().push("1:sleep_preventer_off"),
                || log.borrow_mut().push("2:media_keys_released"),
                || log.borrow_mut().push("3:plugins_and_host_shutdown"),
                || log.borrow_mut().push("4:state_persisted"),
                || {
                    surface_alive.set(false);
                    log.borrow_mut().push("5:video_surface_dropped");
                },
                || {
                    assert!(
                        !surface_alive.get(),
                        "VideoSurface MUST be destroyed before Session::shutdown()"
                    );
                    ctrl.shutdown();
                    log.borrow_mut().push("6:session_shutdown");
                },
            )
        };

        assert!(invoke(), "first shutdown trigger must execute");
        for _ in 1..triggers {
            assert!(!invoke(), "subsequent shutdown triggers must be no-ops");
        }
        assert!(coord.is_done());
        assert_eq!(*log.borrow(), expected);
        assert_eq!(ctrl.session().shutdowns.get(), 1);
    }
}
