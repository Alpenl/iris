//! Headless `Session::new_headless` integration tests for [`iris_app::PlaybackController`],
//! one-shot [`iris_app::VideoAttachSlot`] issuance, and two-phase shutdown ordering.
//!
//! Separated from `crates/iris-app/src/playback/tests.rs` so pure playback orchestration
//! unit tests run in milliseconds with zero libmpv initialization overhead.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use iris_app::{PlaybackController, WindowShutdownCoordinator};
use iris_core::player::{Command, Session, SessionUpdate};
use iris_core::settings::OnEof;

#[test]
fn headless_session_one_shot_attach_slot_and_two_phase_shutdown_contract() {
    let session = Session::new_headless(true).expect("headless mpv session");
    let now = Instant::now();
    let (mut ctrl, mut events_rx) =
        PlaybackController::open_window(session, now).expect("open playback controller");

    // 1. `take_video_attach_slot` issues the narrow binding token at most once per window.
    assert!(
        ctrl.take_video_attach_slot().is_some(),
        "initial window attach must succeed"
    );
    assert!(
        ctrl.take_video_attach_slot().is_none(),
        "second attach attempt must be rejected"
    );

    // 2. Real headless `Session` commands flow through `PlaybackController` and update state.
    let speed_out = ctrl.step_speed(1);
    assert!(speed_out.error.is_none());
    assert_eq!(speed_out.osd.as_deref(), Some("倍速 · 1.25×"));

    let delay_out = ctrl.adjust_sub_delay(0.2);
    assert!(delay_out.error.is_none());
    assert_eq!(delay_out.osd.as_deref(), Some("字幕偏移 · +0.2s"));
    ctrl.session_mut()
        .execute(Command::SetVolume(55.0))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_speed = false;
    while Instant::now() < deadline
        && !(saw_speed && (ctrl.session().state().speed - 1.25).abs() < 1e-6)
    {
        let mut batch: Vec<SessionUpdate> = Vec::new();
        while let Ok(ev) = events_rx.try_recv() {
            batch.push(ev);
        }
        if !batch.is_empty() {
            let report =
                ctrl.process_session_updates(batch, false, false, OnEof::Pause, Instant::now());
            saw_speed |= report.changes.pause_or_speed_changed;
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(saw_speed, "headless mpv session must emit Speed update");
    assert!(
        (ctrl.session().state().speed - 1.25).abs() < 1e-6,
        "headless mpv session must settle at speed 1.25, got {}",
        ctrl.session().state().speed
    );

    // 3. Two-phase shutdown coordinator drops simulated `VideoSurface` strictly before `Session::shutdown()`.
    let surface_dropped = Rc::new(Cell::new(false));
    let mut shutdown = WindowShutdownCoordinator::default();
    let ran = shutdown.run_once(
        || {},
        || {},
        || {},
        || {},
        || surface_dropped.set(true),
        || {
            assert!(surface_dropped.get());
            ctrl.shutdown();
        },
    );
    assert!(ran);
    assert!(!shutdown.run_once(|| {}, || {}, || {}, || {}, || {}, || ctrl.shutdown()));
}
