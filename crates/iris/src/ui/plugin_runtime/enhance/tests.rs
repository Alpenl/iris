use super::EnhancementView;
use iris_api::v2::Id;

#[test]
fn clearing_lut_rejects_late_preparation_with_or_without_an_applied_filter() {
    for applied_owner in [None, Some("plugin"), Some("other")] {
        let mut view = EnhancementView {
            lut_owner: applied_owner.map(str::to_owned),
            ..Default::default()
        };
        let key = ("plugin".into(), "lut".into());
        let other = ("other".into(), "lut".into());
        let track = ("plugin".into(), "track:subtitle".into());
        view.pending.insert(key.clone(), Id(1));
        view.pending.insert(other.clone(), Id(2));
        view.pending.insert(track.clone(), Id(3));

        assert_eq!(view.cancel_lut("plugin"), applied_owner == Some("plugin"));
        assert!(
            !view.finish_job(&key, Id(1)),
            "cleared LUT must never apply"
        );
        // Keep native ownership until the caller successfully clears the filter.
        assert_eq!(view.lut_owner.as_deref(), applied_owner);
        assert!(view.finish_job(&other, Id(2)));
        assert!(view.finish_job(&track, Id(3)));
    }
}

#[test]
fn a_late_cleared_lut_cannot_consume_a_newer_preparation() {
    let mut view = EnhancementView::default();
    let key = ("plugin".into(), "lut".into());
    view.pending.insert(key.clone(), Id(1));
    assert!(!view.cancel_lut("plugin"));
    view.pending.insert(key.clone(), Id(2));
    assert!(!view.finish_job(&key, Id(1)));
    assert!(view.finish_job(&key, Id(2)));
    assert!(!view.finish_job(&key, Id(2)));
}
