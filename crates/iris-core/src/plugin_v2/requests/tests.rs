use super::*;
use iris_api::v2::source::SourceEntry;
fn target() -> BrowseTarget {
    BrowseTarget {
        source: "nas".into(),
        folder: Some("/videos".into()),
        query: None,
        filters: BTreeMap::new(),
    }
}
fn page(entry: &str, next: Option<&str>) -> SourcePage {
    SourcePage {
        source_id: "nas".into(),
        folder_id: Some("/videos".into()),
        parent_id: None,
        title: "Videos".into(),
        entries: vec![SourceEntry {
            id: entry.into(),
            name: entry.into(),
            kind: iris_api::SourceEntryKind::Video,
            size: None,
            modified_secs: None,
            duration: None,
            subtitle: None,
            variants: vec![],
            thumbnail: None,
        }],
        next_page: next.map(Into::into),
        authenticated: true,
        auth_hint: None,
        filters: vec![],
    }
}
#[test]
fn pages_require_owner_view_generation_and_exact_cursor_without_overwriting_on_rejection() {
    let ledger = Ledger::default();
    let mut table = SourceRequests::new(ledger.clone());
    let (view, first) = table.open_view("install-a", Id(1), target()).unwrap();
    assert!(matches!(
        table.accept_page("install-b", first.id, Id(1), page("one", Some("p2"))),
        Err(ErrorCode::Denied)
    ));
    assert!(matches!(
        table.accept_page("install-a", first.id, Id(2), page("one", Some("p2"))),
        Err(ErrorCode::StaleHandle)
    ));
    let first_page = table
        .accept_page("install-a", first.id, Id(1), page("one", Some("p2")))
        .unwrap();
    assert!(!first_page.append);
    assert!(table.next_page("install-a", view, "wrong").is_err());
    let second = table.next_page("install-a", view, "p2").unwrap();
    assert!(table.next_page("install-a", view, "p2").is_err());
    assert!(
        table
            .accept_page("install-a", second.id, Id(1), page("two", Some("p2")))
            .is_err()
    );
    assert!(
        table
            .accept_page("install-a", second.id, Id(1), page("one", None))
            .is_err()
    );
    let mut wrong = page("two", None);
    wrong.folder_id = Some("/private".into());
    assert!(
        table
            .accept_page("install-a", second.id, Id(1), wrong)
            .is_err()
    );
    let second_page = table
        .accept_page("install-a", second.id, Id(1), page("two", None))
        .unwrap();
    assert!(second_page.append);
    assert!(
        table
            .accept_page("install-a", second.id, Id(1), page("three", None))
            .is_err()
    );
    table.close_view("install-a", view).unwrap();
    drop((first, second, first_page, second_page, table));
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
#[test]
fn closing_a_view_cancels_taken_thumbnails_and_keeps_actual_worker_reservations() {
    let ledger = Ledger::default();
    let mut table = SourceRequests::new(ledger.clone());
    let (view, request) = table.open_view("a", Id(1), target()).unwrap();
    let page = table
        .accept_page("a", request.id, Id(1), page("media", None))
        .unwrap();
    drop(request);
    let thumb = table
        .issue(
            "a",
            Id(1),
            RequestTarget::Thumbnail {
                view,
                entry: "media".into(),
            },
        )
        .unwrap();
    let worker = table.take("a", thumb.id, Id(1)).unwrap();
    drop(thumb);
    let close = table.close_view("a", view).unwrap();
    assert!(close.contains(&worker.id));
    assert!(worker.cancel.is_cancelled());
    assert_eq!(ledger.used(None, Resource::Handle), 1);
    drop((worker, page));
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    let (_, old) = table.open_view("a", Id(2), target()).unwrap();
    table.cancel_owner("a");
    assert!(old.cancel.is_cancelled());
    assert!(
        table
            .accept_page("a", old.id, Id(2), super::tests::page("late", None))
            .is_err()
    );
    drop(old);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
