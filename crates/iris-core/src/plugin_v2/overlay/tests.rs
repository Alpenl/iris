use super::*;
use std::time::Instant;
fn cue(id: usize, start: u64) -> TimedCue {
    TimedCue {
        id: format!("cue.{id}"),
        start_ms: Id(start),
        duration_ms: Id(1000),
        layout: CueLayout::Scroll,
        content: CueContent::Text {
            text: "bounded".into(),
            size: 20,
            color: Default::default(),
            bold: false,
        },
    }
}
fn blob(ledger: &Ledger, owner: &str, cues: &[TimedCue]) -> Arc<Blob> {
    Arc::new(
        Blob::new(
            ledger,
            owner,
            iris_plugin_sdk::json::encode(&cues, iris_api::v2::CHUNK_LIMIT).unwrap(),
        )
        .unwrap(),
    )
}
#[test]
fn whole_movie_pages_can_exceed_residency_but_not_bypass_shared_quotas() {
    let ledger = Ledger::default();
    let mut bank = OverlayBank::new(ledger.clone(), Id(1), Generation::default(), 0);
    for page in 0..5 {
        let data: Vec<_> = (page * 2000..(page + 1) * 2000)
            .map(|n| cue(n, 1000))
            .collect();
        bank.submit(
            "a",
            Id(1),
            Generation::default(),
            "track",
            Id(1),
            &blob(&ledger, "a", &data),
            page != 0,
        )
        .unwrap();
    }
    assert_eq!(ledger.used(Some("a"), Resource::OverlayCues), 10_000);
    let one = blob(&ledger, "a", &[cue(10_001, 1000)]);
    assert_eq!(
        bank.submit(
            "a",
            Id(1),
            Generation::default(),
            "another",
            Id(1),
            &one,
            false
        ),
        Err(ErrorCode::QuotaExceeded)
    );
    let frame = bank.frame(1500, false, 1.).unwrap();
    assert_eq!(frame.cues.len(), 256);
    assert_eq!(frame.omitted, 9744);
    assert!(frame.animate);
    drop(frame);
    // Replacement can atomically exchange its own lease without double charging.
    bank.submit(
        "a",
        Id(1),
        Generation::default(),
        "track",
        Id(1),
        &one,
        false,
    )
    .unwrap();
    assert_eq!(ledger.used(Some("a"), Resource::OverlayCues), 1);
    bank.advance(Id(1), Generation::default(), 60_000).unwrap();
    assert_eq!(ledger.used(Some("a"), Resource::OverlayCues), 0);
    let future = blob(&ledger, "a", &[cue(20_000, 61_000)]);
    bank.submit(
        "a",
        Id(1),
        Generation::default(),
        "track",
        Id(2),
        &future,
        false,
    )
    .unwrap();
    assert_eq!(bank.frame(61_500, true, 1.).unwrap().cues.len(), 1);
    bank.revoke("a");
    drop((one, future, bank));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
#[test]
fn window_and_seek_generation_reject_stale_or_far_future_submissions_without_truncation() {
    let ledger = Ledger::default();
    let generation = Generation::default();
    let mut bank = OverlayBank::new(ledger.clone(), Id(1), generation, 0);
    let too_far = blob(&ledger, "a", &[cue(1, 120_000)]);
    assert_eq!(
        bank.submit("a", Id(1), generation, "track", Id(1), &too_far, false),
        Err(ErrorCode::OutOfWindow)
    );
    assert_eq!(ledger.used(None, Resource::OverlayCues), 0);
    let valid = blob(&ledger, "a", &[cue(2, 1000)]);
    assert_eq!(
        bank.submit("b", Id(1), generation, "track", Id(1), &valid, false),
        Err(ErrorCode::Denied)
    );
    bank.submit("a", Id(1), generation, "track", Id(1), &valid, false)
        .unwrap();
    let paused = bank.frame(1500, true, 1.).unwrap();
    assert!(!paused.animate);
    assert!(paused.next_change.is_none());
    drop(paused);
    let future = bank.frame(0, false, 2.).unwrap();
    assert!(future.cues.is_empty());
    assert!(!future.animate);
    assert_eq!(future.next_change, Some(Duration::from_millis(500)));
    drop(future);
    let seek = Generation {
        seek: Id(1),
        ..generation
    };
    bank.advance(Id(1), seek, 1000).unwrap();
    assert_eq!(ledger.used(None, Resource::OverlayCues), 0);
    assert_eq!(
        bank.submit("a", Id(1), generation, "track", Id(1), &valid, false),
        Err(ErrorCode::StaleHandle)
    );
    let empty = bank.frame(1500, false, 1.).unwrap();
    assert!(!empty.animate);
    assert!(empty.next_change.is_none());
}
#[test]
fn host_clock_interpolates_without_guest_ticks_and_freezes_when_paused_or_buffering() {
    let now = Instant::now();
    let mut clock = OverlayClock::default();
    clock.update(10., 2., false, false, 20., now);
    assert_eq!(clock.position_ms(now + Duration::from_millis(500)), 11_000);
    clock.update(11., 2., true, false, 20., now);
    assert_eq!(clock.position_ms(now + Duration::from_secs(5)), 11_000);
    clock.update(11., 2., false, true, 20., now);
    assert_eq!(clock.position_ms(now + Duration::from_secs(5)), 11_000);
    clock.update(19., 2., false, false, 20., now);
    assert_eq!(clock.position_ms(now + Duration::from_secs(5)), 20_000);
}
