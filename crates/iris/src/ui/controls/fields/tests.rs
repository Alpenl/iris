use super::can_step_queue;
use iris_core::{
    catalog::{PlayQueue, QueueItem},
    library::Kind,
    media_uri::MediaUri,
};

#[test]
fn single_item_queue_can_be_entered_from_another_media_or_idle() {
    let current = MediaUri::Local("a.mp4".into());
    let next = MediaUri::Local("b.mp4".into());
    let mut queue = PlayQueue::new();
    for delta in [-1, 1] {
        assert!(!can_step_queue(&queue, Some(&current), delta));
        assert!(!can_step_queue(&queue, None, delta));
    }
    queue.push(QueueItem::new(next.clone(), "B", Kind::Video));
    for delta in [-1, 1] {
        assert!(can_step_queue(&queue, Some(&current), delta));
        assert_eq!(&queue.step(&current, delta, true).unwrap().uri, &next);
        assert!(can_step_queue(&queue, None, delta));
        assert!(!can_step_queue(&queue, Some(&next), delta));
    }
}

#[test]
fn removing_current_item_keeps_navigation_to_the_remaining_item_enabled() {
    let current = MediaUri::Local("a.mp4".into());
    let next = MediaUri::Local("b.mp4".into());
    let mut queue = PlayQueue::new();
    queue.extend([
        QueueItem::new(current.clone(), "A", Kind::Video),
        QueueItem::new(next.clone(), "B", Kind::Video),
    ]);
    assert!(can_step_queue(&queue, Some(&current), 1));
    assert!(queue.remove(&current));
    for delta in [-1, 1] {
        assert!(can_step_queue(&queue, Some(&current), delta));
        assert_eq!(&queue.step(&current, delta, true).unwrap().uri, &next);
    }
}
