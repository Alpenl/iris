mod persistence;
mod support;

use std::path::PathBuf;
use std::time::Instant;

use iris_api::v2::{
    Capability, HostCommand, Id, Reply, WireU64,
    source::{
        PlaylistEntry, ResolvedMedia, SourceDescriptor, SourceEntry, SourceFilter, SourcePage,
    },
    stream::{RemoteResource, StreamSource, StreamSpec},
};
use iris_api::{CommandSpec, SelectOption, SourceEntryKind, StatusBadge, StreamVariant};
use iris_core::bookmarks::LocationStore;
use iris_core::catalog::{CatalogItem, LocalDirectory, MediaFilter, PlayQueue, QueueItem};
use iris_core::library::{self, FolderEntry, Kind, ScanResult, SortBy, SortOrder};
use iris_core::media_uri::MediaUri;
use iris_core::preset::SidebarMode;
use iris_core::settings::Settings;
use iris_core::store::Store;
use iris_core::vault::CredentialVault;
use iris_plugin_host::{PluginHost, PluginStatus};

use self::support::{
    FakePlayer, LoopbackMediaServer, TempDir, build_routed_wat, package_zip, wait_until,
};
use super::{
    CatalogController, CatalogCounts, DirRow, DirectStreamTarget, OpenMediaBatchAction, StepAction,
    can_step_queue, parse_direct_stream_input, plan_open_paths, resolve_queue_paths,
};
use crate::plugins::{ActiveSource, PluginBatchOutcome, PluginContext, PluginController};

struct CatalogRig {
    dir: TempDir,
    catalog: CatalogController,
    plugins: PluginController,
    player: FakePlayer,
    settings: Settings,
}

impl CatalogRig {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let data_dir = dir.0.join("app-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let host = PluginHost::open_in(&data_dir, CredentialVault::in_memory(), &[], false)
            .expect("open test PluginHost");
        let now = Instant::now();
        let (plugins, _wake_rx) = PluginController::open_window(host, &[], "graphite", vec![], now);
        let locations = LocationStore::load_from(&data_dir.join("locations.json"));
        let store = Store::load_from_dir(&data_dir);
        let catalog = CatalogController::with_stores(
            locations,
            store,
            SidebarMode::Files,
            SortBy::Name,
            SortOrder::Asc,
            now,
        );
        Self {
            dir,
            catalog,
            plugins,
            player: FakePlayer::default(),
            settings: Settings::default(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.0.join("app-data")
    }

    fn locations_file(&self) -> PathBuf {
        self.data_dir().join("locations.json")
    }

    fn poll(&mut self, now: Instant) -> PluginBatchOutcome {
        let plugins = &mut self.plugins;
        let player = &mut self.player;
        let settings = &self.settings;
        let out = self.catalog.with_plugin_parts(|store, active_source| {
            plugins.poll(
                PluginContext {
                    player,
                    store,
                    settings,
                    active_source,
                },
                now,
            )
        });
        let cur_uri = self.player.uri.clone();
        let cur_local = self.player.local_path.clone();
        self.catalog.apply_plugin_domain_changes(
            out.domain,
            &self.plugins,
            cur_uri.as_ref(),
            cur_local.as_deref(),
        );
        out
    }

    fn install_plugin(
        &mut self,
        id: &str,
        perms: &[Capability],
        commands: &[CommandSpec],
        wat_src: &str,
    ) {
        let bytes = package_zip(id, perms, commands, &[], None, wat_src);
        let pkg_path = self.dir.0.join(format!("{id}.irisplugin"));
        std::fs::write(&pkg_path, bytes).unwrap();
        let _ = self.plugins.review_package_path(pkg_path);
        wait_until("review summary ready", || {
            let _ = self.poll(Instant::now());
            self.plugins.review_summary().is_some()
        });
        let _ = self.plugins.approve_host_consent();
        wait_until("plugin ready", || {
            let ready = self.plugins.is_ready(id);
            let _ = self.poll(Instant::now());
            ready
                && !self.plugins.has_host_consent()
                && self
                    .plugins
                    .list()
                    .iter()
                    .any(|p| p.id == id && p.status == PluginStatus::Ready)
        });
    }

    fn wait_for_source(&mut self, plugin_id: &str, source_id: &str) {
        wait_until("media source registered", || {
            let _ = self.poll(Instant::now());
            self.plugins
                .media_sources()
                .iter()
                .any(|(pid, _, desc)| *pid == plugin_id && desc.id == source_id)
        });
    }
}

impl Drop for CatalogRig {
    fn drop(&mut self) {
        self.plugins.shutdown();
    }
}

fn page_json_with_req_echo(page: &SourcePage) -> String {
    serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ProvideSourcePage {
            request_id: Id(999_999_991),
            page: Box::new(page.clone()),
        }],
    })
    .unwrap()
    .replace("999999991", "{{REQ}}")
}

#[test]
fn local_directory_browsing_sorting_filtering_filmstrip_and_cached_parent_scan() {
    let mut rig = CatalogRig::new("catalog-local-browse");
    let media_root = rig.dir.0.join("Library").join("SummerTrip");
    std::fs::create_dir_all(media_root.join("SubB")).unwrap();
    std::fs::create_dir_all(media_root.join("SubA")).unwrap();

    let clip = media_root.join("b_clip.mp4");
    let song = media_root.join("a_song.mp3");
    let pic_z = media_root.join("z_pic.jpg");
    let pic_m = media_root.join("m_pic.png");
    let pic_y = media_root.join("y_pic.webp");
    std::fs::write(&clip, vec![b'v'; 4096]).unwrap();
    std::fs::write(&song, vec![b'a'; 1024]).unwrap();
    std::fs::write(&pic_z, vec![b'p'; 512]).unwrap();
    std::fs::write(&pic_m, vec![b'p'; 2048]).unwrap();
    std::fs::write(&pic_y, vec![b'p'; 256]).unwrap();

    let mut scan = library::scan_local_or_share(&media_root);
    for entry in &mut scan.entries {
        entry.modified_secs = match entry.name.as_str() {
            "a_song.mp3" => 100,
            "b_clip.mp4" => 500,
            "m_pic.png" => 300,
            "y_pic.webp" => 400,
            "z_pic.jpg" => 200,
            _ => 1,
        };
    }
    let local_dir = LocalDirectory {
        folder: Some(media_root.clone()),
        entries: scan.entries,
        subfolders: scan.subfolders,
    };

    let current_pic_uri = MediaUri::Local(pic_m.clone());
    rig.catalog.set_local_directory(
        local_dir,
        &rig.plugins,
        Some(&current_pic_uri),
        Some(pic_m.as_path()),
    );

    assert_eq!(
        rig.catalog.counts(),
        CatalogCounts {
            video: 1,
            audio: 1,
            photo: 3,
            favorite: 0,
        }
    );
    assert_eq!(rig.catalog.counts().total_media(), 5);
    assert_eq!(rig.catalog.playable_count(), 5);
    assert_eq!(rig.catalog.current_playable_number(), Some(3));
    assert_eq!(rig.catalog.sidebar_header_title(), "SummerTrip");
    assert_eq!(rig.catalog.titlebar_folder_label(false), "SummerTrip");
    assert_eq!(
        rig.catalog
            .photo_counter_strings(Some(&current_pic_uri), true, true),
        ("3".to_owned(), "5".to_owned())
    );

    assert_eq!(
        rig.catalog.rows().first(),
        Some(&DirRow::Up {
            parent_local: media_root.parent().map(PathBuf::from),
            parent_plugin: None,
        })
    );
    let item_names = |c: &CatalogController| -> Vec<String> {
        c.items().iter().map(|i| i.name.clone()).collect()
    };
    assert_eq!(
        item_names(&rig.catalog),
        vec![
            "SubA",
            "SubB",
            "a_song.mp3",
            "b_clip.mp4",
            "m_pic.png",
            "y_pic.webp",
            "z_pic.jpg"
        ]
    );
    assert_eq!(
        rig.catalog.current_row_index(Some(&current_pic_uri)),
        Some(5)
    );

    assert_eq!(
        rig.catalog.photo_paths(),
        &[pic_m.clone(), pic_y.clone(), pic_z.clone()]
    );
    assert_eq!(rig.catalog.filmstrip_index(), 0);
    assert_eq!(
        rig.catalog.prioritized_filmstrip_window(true, true),
        vec![pic_m.clone(), pic_y.clone(), pic_z.clone()]
    );
    assert!(
        rig.catalog
            .prioritized_filmstrip_window(false, true)
            .is_empty()
    );
    assert!(
        rig.catalog
            .prioritized_filmstrip_window(true, false)
            .is_empty()
    );

    // Cycle through all 6 workspace sort states; folders remain pinned first in every mode.
    let (by1, ord1) = rig
        .catalog
        .cycle_sort(&rig.plugins, Some(&current_pic_uri), Some(&pic_m));
    assert_eq!((by1, ord1), (SortBy::Name, SortOrder::Desc));
    assert_eq!(
        &item_names(&rig.catalog)[..3],
        &["SubB", "SubA", "z_pic.jpg"]
    );

    let (by2, ord2) = rig
        .catalog
        .cycle_sort(&rig.plugins, Some(&current_pic_uri), Some(&pic_m));
    assert_eq!((by2, ord2), (SortBy::Modified, SortOrder::Desc));
    assert_eq!(item_names(&rig.catalog)[2], "b_clip.mp4");

    let (by3, ord3) = rig
        .catalog
        .cycle_sort(&rig.plugins, Some(&current_pic_uri), Some(&pic_m));
    assert_eq!((by3, ord3), (SortBy::Modified, SortOrder::Asc));
    assert_eq!(item_names(&rig.catalog)[2], "a_song.mp3");

    let (by4, ord4) = rig
        .catalog
        .cycle_sort(&rig.plugins, Some(&current_pic_uri), Some(&pic_m));
    assert_eq!((by4, ord4), (SortBy::Size, SortOrder::Desc));
    assert_eq!(item_names(&rig.catalog)[2], "b_clip.mp4");

    let (by5, ord5) = rig
        .catalog
        .cycle_sort(&rig.plugins, Some(&current_pic_uri), Some(&pic_m));
    assert_eq!((by5, ord5), (SortBy::Size, SortOrder::Asc));
    assert_eq!(item_names(&rig.catalog)[2], "y_pic.webp");

    let (by6, ord6) = rig
        .catalog
        .cycle_sort(&rig.plugins, Some(&current_pic_uri), Some(&pic_m));
    assert_eq!((by6, ord6), (SortBy::Name, SortOrder::Asc));

    rig.catalog
        .set_filter(MediaFilter::Photo, Some(&current_pic_uri));
    assert!(!matches!(
        rig.catalog.rows().first(),
        Some(DirRow::Up { .. })
    ));
    assert_eq!(
        item_names(&rig.catalog),
        vec!["m_pic.png", "y_pic.webp", "z_pic.jpg"]
    );

    rig.catalog
        .set_filter(MediaFilter::All, Some(&current_pic_uri));
    rig.catalog.set_search_query("clip", Some(&current_pic_uri));
    assert_eq!(item_names(&rig.catalog), vec!["b_clip.mp4"]);
    assert_eq!(rig.catalog.current_playable_number(), None);

    rig.catalog.clear_search_query(Some(&current_pic_uri));
    assert!(matches!(
        rig.catalog.rows().first(),
        Some(DirRow::Up { .. })
    ));
    assert_eq!(rig.catalog.current_playable_number(), Some(3));

    let cached = rig
        .catalog
        .cached_parent_scan(&clip)
        .expect("sibling reuses cached scan");
    assert_eq!(cached.0, media_root);
    assert_eq!(cached.1.entries.len(), 5);
    assert!(
        rig.catalog
            .cached_parent_scan(&media_root.join("SubA").join("other.mp4"))
            .is_none()
    );
}

#[test]
fn plugin_source_switching_pagination_filters_remote_search_and_return_to_local() {
    let mut rig = CatalogRig::new("catalog-plugin-browse");
    let local_folder = rig.dir.0.join("LocalVideos");
    std::fs::create_dir_all(&local_folder).unwrap();
    let local_file = local_folder.join("local_ep.mkv");
    std::fs::write(&local_file, b"local").unwrap();
    let scan = library::scan_local_or_share(&local_folder);
    rig.catalog.set_local_directory(
        LocalDirectory {
            folder: Some(local_folder.clone()),
            entries: scan.entries,
            subfolders: scan.subfolders,
        },
        &rig.plugins,
        None,
        None,
    );

    let init_sources = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetSources {
            sources: vec![SourceDescriptor {
                id: "drive".into(),
                name: "Cloud Drive".into(),
                subtitle: Some("dav.example".into()),
                badge: Some("CloudBadge".into()),
                icon: Some("cloud".into()),
                supports_search: true,
                requires_auth: false,
                auth_command_id: None,
                configurable: false,
                removable: true,
            }],
        }],
    })
    .unwrap();

    let search_page = page_json_with_req_echo(&SourcePage {
        source_id: "drive".into(),
        folder_id: None,
        parent_id: None,
        title: "Search: eva".into(),
        entries: vec![SourceEntry {
            id: "eva01".into(),
            name: "EVA Episode 01".into(),
            kind: SourceEntryKind::Video,
            size: Some(WireU64(1_000)),
            modified_secs: None,
            duration: Some(1440.0),
            subtitle: None,
            variants: Vec::new(),
            thumbnail: None,
        }],
        next_page: None,
        filters: Vec::new(),
        authenticated: true,
        auth_hint: None,
    });

    let filtered_page = page_json_with_req_echo(&SourcePage {
        source_id: "drive".into(),
        folder_id: None,
        parent_id: None,
        title: "Movies Only".into(),
        entries: vec![SourceEntry {
            id: "mov1".into(),
            name: "Feature Film".into(),
            kind: SourceEntryKind::Video,
            size: None,
            modified_secs: None,
            duration: Some(7200.0),
            subtitle: None,
            variants: Vec::new(),
            thumbnail: None,
        }],
        next_page: None,
        filters: vec![SourceFilter {
            id: "genre".into(),
            label: "类型".into(),
            selected: Some("movie".into()),
            options: vec![SelectOption {
                value: "movie".into(),
                label: "电影".into(),
            }],
        }],
        authenticated: true,
        auth_hint: None,
    });

    let sub_page = page_json_with_req_echo(&SourcePage {
        source_id: "drive".into(),
        folder_id: Some("sub1".into()),
        parent_id: Some("root".into()),
        title: "Season 1".into(),
        entries: vec![SourceEntry {
            id: "s1e1".into(),
            name: "S01E01.mkv".into(),
            kind: SourceEntryKind::Video,
            size: None,
            modified_secs: None,
            duration: Some(1200.0),
            subtitle: None,
            variants: Vec::new(),
            thumbnail: None,
        }],
        next_page: None,
        filters: Vec::new(),
        authenticated: true,
        auth_hint: None,
    });

    let page_two = page_json_with_req_echo(&SourcePage {
        source_id: "drive".into(),
        folder_id: None,
        parent_id: None,
        title: "Cloud Root".into(),
        entries: vec![SourceEntry {
            id: "vid2".into(),
            name: "Part Two.mp4".into(),
            kind: SourceEntryKind::Video,
            size: None,
            modified_secs: None,
            duration: Some(600.0),
            subtitle: None,
            variants: Vec::new(),
            thumbnail: None,
        }],
        next_page: None,
        filters: Vec::new(),
        authenticated: true,
        auth_hint: None,
    });

    let page_one = page_json_with_req_echo(&SourcePage {
        source_id: "drive".into(),
        folder_id: None,
        parent_id: None,
        title: "Cloud Root".into(),
        entries: vec![
            SourceEntry {
                id: "sub1".into(),
                name: "Season 1".into(),
                kind: SourceEntryKind::Folder,
                size: None,
                modified_secs: None,
                duration: None,
                subtitle: Some("1 item".into()),
                variants: Vec::new(),
                thumbnail: None,
            },
            SourceEntry {
                id: "vid1".into(),
                name: "Part One.mp4".into(),
                kind: SourceEntryKind::Video,
                size: None,
                modified_secs: None,
                duration: Some(300.0),
                subtitle: None,
                variants: vec![StreamVariant {
                    id: "1080p".into(),
                    label: "1080P".into(),
                }],
                thumbnail: None,
            },
        ],
        next_page: Some("token_p2".into()),
        filters: vec![SourceFilter {
            id: "genre".into(),
            label: "类型".into(),
            selected: None,
            options: vec![SelectOption {
                value: "movie".into(),
                label: "电影".into(),
            }],
        }],
        authenticated: true,
        auth_hint: None,
    });

    let wat = build_routed_wat(
        &[
            (b"w_opened", &init_sources),
            (b"\"eva 4k\"", &search_page),
            (b"\"movie\"}", &filtered_page),
            (b":\"sub1\",", &sub_page),
            (b"token_p2", &page_two),
            (b"ce_brows", &page_one),
        ],
        r#"{"ok":[]}"#,
    );
    rig.install_plugin("demo.cloud", &[Capability::MediaSource], &[], &wat);
    rig.wait_for_source("demo.cloud", "drive");

    rig.catalog.set_sidebar_mode(SidebarMode::Sources);
    let (started, _) = rig.catalog.browse_plugin_source(
        &mut rig.plugins,
        "demo.cloud",
        "drive",
        None,
        None,
        None,
        Instant::now(),
    );
    assert!(started);
    assert_eq!(rig.catalog.sidebar_mode(), SidebarMode::Files);

    wait_until("page 1 loaded", || {
        let _ = rig.poll(Instant::now());
        matches!(
            rig.catalog.active_source(),
            ActiveSource::Plugin(ps) if !ps.loading && ps.next_page.as_deref() == Some("token_p2")
        )
    });
    assert_eq!(rig.catalog.sidebar_header_title(), "Cloud Root");
    assert!(
        rig.catalog
            .titlebar_folder_label(false)
            .contains("Cloud Drive")
    );
    assert!(matches!(
        rig.catalog.rows().last(),
        Some(DirRow::LoadMore { next_page, .. }) if next_page == "token_p2"
    ));

    // Paginate (`page = Some("token_p2")`): appends `Part Two.mp4` and clears `LoadMore`.
    let gen_before_page2 = rig.catalog.dir_generation();
    let (started_p2, _) = rig.catalog.browse_plugin_source(
        &mut rig.plugins,
        "demo.cloud",
        "drive",
        None,
        Some("token_p2".into()),
        None,
        Instant::now(),
    );
    assert!(started_p2);
    assert_eq!(rig.catalog.dir_generation(), gen_before_page2);
    wait_until("page 2 appended", || {
        let _ = rig.poll(Instant::now());
        rig.catalog.items().len() == 3
    });
    assert!(!matches!(
        rig.catalog.rows().last(),
        Some(DirRow::LoadMore { .. })
    ));

    // Navigate into plugin subfolder: row 0 becomes `DirRow::Up` with `parent_plugin`.
    let _ = rig.catalog.browse_plugin_source(
        &mut rig.plugins,
        "demo.cloud",
        "drive",
        Some("sub1".into()),
        None,
        None,
        Instant::now(),
    );
    wait_until("subfolder loaded", || {
        let _ = rig.poll(Instant::now());
        matches!(
            rig.catalog.active_source(),
            ActiveSource::Plugin(ps) if !ps.loading && ps.folder_id.as_deref() == Some("sub1")
        )
    });
    assert_eq!(
        rig.catalog.rows().first(),
        Some(&DirRow::Up {
            parent_local: None,
            parent_plugin: Some(("demo.cloud".into(), "drive".into(), Some("root".into()))),
        })
    );

    // Source filter selection triggers remote browse with `"genre":"movie"`.
    let _ = rig.catalog.browse_plugin_source(
        &mut rig.plugins,
        "demo.cloud",
        "drive",
        None,
        None,
        None,
        Instant::now(),
    );
    wait_until("back at root", || {
        let _ = rig.poll(Instant::now());
        !rig.plugins.source_filters().is_empty()
    });
    assert!(
        rig.catalog
            .select_plugin_source_filter(
                &mut rig.plugins,
                "genre",
                Some("movie".into()),
                false,
                Instant::now(),
            )
            .is_some()
    );
    wait_until("filtered page loaded", || {
        let _ = rig.poll(Instant::now());
        rig.catalog.sidebar_header_title() == "Movies Only"
    });
    assert_eq!(rig.catalog.items()[0].name, "Feature Film");

    // Remote search submission trims query and dispatches server-side `browse`.
    rig.catalog.set_search_query("  eva 4k  ", None);
    assert!(
        rig.catalog
            .submit_directory_search(&mut rig.plugins, None, Instant::now())
            .is_some()
    );
    wait_until("remote search page loaded", || {
        let _ = rig.poll(Instant::now());
        rig.catalog.sidebar_header_title() == "Search: eva"
    });
    rig.catalog.clear_search_query(None);
    assert_eq!(rig.catalog.items()[0].name, "EVA Episode 01");

    // Returning to local source closes plugin browse view and restores local rows immediately.
    let _ = rig
        .catalog
        .browse_local_source(&mut rig.plugins, None, None);
    assert!(rig.catalog.active_source().is_local());
    assert!(rig.plugins.active_browse_view().is_none());
    assert_eq!(rig.catalog.items().len(), 1);
    assert_eq!(rig.catalog.items()[0].name, "local_ep.mkv");
}

#[test]
fn async_generation_fencing_discards_stale_local_refresh_plugin_page_and_favorite_scan() {
    let mut rig = CatalogRig::new("catalog-fencing");
    let folder_a = rig.dir.0.join("FolderA");
    let folder_b = rig.dir.0.join("FolderB");
    std::fs::create_dir_all(&folder_a).unwrap();
    std::fs::create_dir_all(&folder_b).unwrap();
    let file_a1 = folder_a.join("a1.mp4");
    let file_a2 = folder_a.join("a2.mp4");
    let file_b1 = folder_b.join("b1.mp4");
    std::fs::write(&file_a1, b"a1").unwrap();
    std::fs::write(&file_b1, b"b1").unwrap();

    let scan_a = library::scan_local_or_share(&folder_a);
    let scan_b = library::scan_local_or_share(&folder_b);
    rig.catalog.set_local_directory(
        LocalDirectory {
            folder: Some(folder_a.clone()),
            entries: scan_a.entries.clone(),
            subfolders: Vec::new(),
        },
        &rig.plugins,
        None,
        None,
    );

    // 1a. Refresh ticket superseded by switching to `folder_b` must be discarded.
    let stale_switch_ticket = rig.catalog.begin_local_directory_refresh().unwrap();
    std::fs::write(&file_a2, b"a2").unwrap();
    let stale_scan_a = stale_switch_ticket.run_scan();
    rig.catalog.set_local_directory(
        LocalDirectory {
            folder: Some(folder_b.clone()),
            entries: scan_b.entries,
            subfolders: Vec::new(),
        },
        &rig.plugins,
        None,
        None,
    );
    assert!(!rig.catalog.finish_local_directory_refresh(
        &stale_switch_ticket,
        stale_scan_a.clone(),
        false,
        &rig.plugins,
        None,
        None,
    ));
    assert_eq!(rig.catalog.items().len(), 1);
    assert_eq!(rig.catalog.items()[0].name, "b1.mp4");

    // 1b. Two overlapping refreshes on `folder_a`: older ticket rejected without clobbering newer ticket!
    rig.catalog.set_local_directory(
        LocalDirectory {
            folder: Some(folder_a.clone()),
            entries: scan_a.entries,
            subfolders: Vec::new(),
        },
        &rig.plugins,
        None,
        None,
    );
    let older_ticket = rig.catalog.begin_local_directory_refresh().unwrap();
    let newer_ticket = rig.catalog.begin_local_directory_refresh().unwrap();
    assert_ne!(older_ticket.generation, newer_ticket.generation);
    assert!(!rig.catalog.finish_local_directory_refresh(
        &older_ticket,
        ScanResult::default(),
        false,
        &rig.plugins,
        None,
        None,
    ));
    assert!(!rig.catalog.finish_local_directory_refresh(
        &newer_ticket,
        stale_scan_a.clone(),
        true,
        &rig.plugins,
        None,
        None,
    ));
    assert!(rig.catalog.finish_local_directory_refresh(
        &newer_ticket,
        stale_scan_a,
        false,
        &rig.plugins,
        None,
        None,
    ));
    assert_eq!(rig.catalog.items().len(), 2);

    // 2. Late plugin browse page arriving AFTER user returned to `Local` is fenced out.
    let init_sources = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetSources {
            sources: vec![SourceDescriptor {
                id: "slow".into(),
                name: "Slow Cloud".into(),
                subtitle: None,
                badge: None,
                icon: None,
                supports_search: false,
                requires_auth: false,
                auth_command_id: None,
                configurable: false,
                removable: false,
            }],
        }],
    })
    .unwrap();
    let late_page = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ProvideSourcePage {
            request_id: Id(999_999_991),
            page: Box::new(SourcePage {
                source_id: "slow".into(),
                folder_id: None,
                parent_id: None,
                title: "Stale Cloud Page".into(),
                entries: vec![SourceEntry {
                    id: "ghost".into(),
                    name: "GhostEntry.mp4".into(),
                    kind: SourceEntryKind::Video,
                    size: None,
                    modified_secs: None,
                    duration: None,
                    subtitle: None,
                    variants: Vec::new(),
                    thumbnail: None,
                }],
                next_page: None,
                filters: Vec::new(),
                authenticated: true,
                auth_hint: None,
            }),
        }],
    })
    .unwrap()
    .replace("999999991", "{{SAVED_REQ}}");
    let save_browse_req = format!(
        "{{{{SAVE_REQ}}}}{}",
        serde_json::to_string(&Reply::Ok {
            ok: vec![HostCommand::SetStatus {
                badge: Some(Box::new(StatusBadge {
                    id: "late".into(),
                    text: "browse_seen".into(),
                    tooltip: None,
                    accent: false,
                    action_id: None,
                })),
            }],
        })
        .unwrap()
    );
    let ack_barrier = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetStatus {
            badge: Some(Box::new(StatusBadge {
                id: "late".into(),
                text: "late_flushed".into(),
                tooltip: None,
                accent: false,
                action_id: None,
            })),
        }],
    })
    .unwrap();
    let wat = build_routed_wat(
        &[
            (b"w_opened", &init_sources),
            (b"ce_brows", &save_browse_req),
            (b"emit_old", &late_page),
            (b"ack_fifo", &ack_barrier),
        ],
        r#"{"ok":[]}"#,
    );
    rig.install_plugin(
        "demo.slow",
        &[Capability::MediaSource, Capability::UiStatus],
        &[
            CommandSpec {
                id: "emit_old".into(),
                title: "Emit Stale".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
            CommandSpec {
                id: "ack_fifo".into(),
                title: "Ack Barrier".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
        ],
        &wat,
    );
    rig.wait_for_source("demo.slow", "slow");

    let (started, _) = rig.catalog.browse_plugin_source(
        &mut rig.plugins,
        "demo.slow",
        "slow",
        None,
        None,
        None,
        Instant::now(),
    );
    assert!(started);
    wait_until("guest received browse request_id", || {
        let _ = rig.poll(Instant::now());
        rig.plugins
            .status_badges()
            .iter()
            .any(|(_, b)| b.text == "browse_seen")
    });
    let _ = rig
        .catalog
        .browse_local_source(&mut rig.plugins, None, None);
    assert!(rig.catalog.active_source().is_local());

    for cmd_id in ["emit_old", "ack_fifo"] {
        let plugins = &mut rig.plugins;
        let player = &mut rig.player;
        let settings = &rig.settings;
        let _ = rig.catalog.with_plugin_parts(|store, active_source| {
            plugins.invoke_command(
                PluginContext {
                    player,
                    store,
                    settings,
                    active_source,
                },
                "demo.slow",
                cmd_id,
                Instant::now(),
            )
        });
    }
    wait_until("late page reply flushed through FIFO actor", || {
        let _ = rig.poll(Instant::now());
        rig.plugins
            .status_badges()
            .iter()
            .any(|(_, b)| b.text == "late_flushed")
    });
    assert!(rig.catalog.active_source().is_local());
    assert!(
        !rig.catalog
            .items()
            .iter()
            .any(|i| i.name == "GhostEntry.mp4"),
        "late plugin browse page must never overwrite active local catalog"
    );

    // 3. Favorite background scan fencing when favorites mutate mid-scan.
    let uri_a1 = MediaUri::Local(file_a1.clone());
    let uri_b1 = MediaUri::Local(file_b1.clone());
    assert!(
        rig.catalog
            .toggle_favorite(&uri_a1, &rig.plugins, None, None)
    );
    rig.catalog.set_filter(MediaFilter::FavoritesOnly, None);
    let fav_job_1 = rig
        .catalog
        .begin_favorite_scan()
        .expect("scan #1 scheduled");
    assert!(rig.catalog.is_favorite_scan_in_flight());
    assert!(
        rig.catalog.begin_favorite_scan().is_none(),
        "duplicate scan blocked while in flight"
    );

    assert!(
        rig.catalog
            .toggle_favorite(&uri_b1, &rig.plugins, None, None)
    );
    assert!(!rig.catalog.is_favorite_scan_in_flight());
    let fav_job_2 = rig
        .catalog
        .begin_favorite_scan()
        .expect("scan #2 scheduled with bumped generation");
    assert_ne!(fav_job_1.generation(), fav_job_2.generation());

    assert!(!rig.catalog.finish_favorite_scan(fav_job_1.run(), None));
    assert!(rig.catalog.is_favorite_scan_in_flight());
    assert!(rig.catalog.rows().is_empty());

    assert!(rig.catalog.finish_favorite_scan(fav_job_2.run(), None));
    assert!(!rig.catalog.is_favorite_scan_in_flight());
    assert_eq!(rig.catalog.items().len(), 2);

    // 4. `begin_remote_load` cancels superseded token & shares serialized `remote_work_slot`.
    let (cancel_1, slot_1) = rig.catalog.begin_remote_load();
    assert!(rig.catalog.has_remote_load());
    assert!(!cancel_1.is_cancelled());
    let guard = slot_1.try_lock().expect("first probe acquires slot");
    let (cancel_2, slot_2) = rig.catalog.begin_remote_load();
    assert!(cancel_1.is_cancelled());
    assert!(!cancel_2.is_cancelled());
    assert!(slot_2.try_lock().is_none());
    drop(guard);
    assert!(slot_2.try_lock().is_some());
    assert!(rig.catalog.cancel_remote_load());
    assert!(cancel_2.is_cancelled());
    assert!(!rig.catalog.has_remote_load());
}

#[test]
fn cross_source_play_queue_ipc_batch_planning_and_step_priority() {
    let server = LoopbackMediaServer::start(b"FAKEVIDEO".to_vec());
    let mut rig = CatalogRig::new("catalog-queue-step");
    let media_dir = rig.dir.0.join("QueueMedia");
    std::fs::create_dir_all(&media_dir).unwrap();
    let local_a = media_dir.join("01_intro.mp4");
    let local_b = media_dir.join("02_main.mkv");
    let sub_a = media_dir.join("01_intro.srt");
    let sub_b = media_dir.join("02_main.ASS");
    let pkg = media_dir.join("addon.IRISPLUGIN");
    std::fs::write(&local_a, b"v1").unwrap();
    std::fs::write(&local_b, b"v2").unwrap();
    std::fs::write(&sub_a, b"1").unwrap();
    std::fs::write(&sub_b, b"2").unwrap();
    std::fs::write(&pkg, b"zip").unwrap();

    let batch = plan_open_paths(vec![
        PathBuf::new(),
        sub_a.clone(),
        pkg.clone(),
        local_a.clone(),
        sub_b.clone(),
        local_b.clone(),
    ]);
    assert_eq!(batch.package_to_review, Some(pkg));
    assert_eq!(
        batch.media_action,
        OpenMediaBatchAction::MultiQueue(vec![local_a.clone(), local_b.clone()])
    );
    assert_eq!(batch.subtitles, vec![sub_a, sub_b]);

    let single_plan = plan_open_paths(vec![local_a.clone()]);
    assert_eq!(
        single_plan.media_action,
        OpenMediaBatchAction::SinglePath(local_a.clone())
    );

    assert!(matches!(
        parse_direct_stream_input("   ", None),
        DirectStreamTarget::Empty
    ));
    let local_str = local_a.to_string_lossy().into_owned();
    assert!(matches!(
        parse_direct_stream_input(&local_str, None),
        DirectStreamTarget::LocalPath(p) if p == local_a
    ));
    assert!(matches!(
        parse_direct_stream_input("ftp://files.example.com/a.mp4", None),
        DirectStreamTarget::InvalidScheme
    ));
    match parse_direct_stream_input("https://stream.example.com/live/index.m3u8", None) {
        DirectStreamTarget::Remote(req) => {
            assert!(req.is_live);
            assert_eq!(req.kind, Kind::Video);
            assert_eq!(req.title, "index.m3u8");
        }
        other => panic!("expected Remote request, got {other:?}"),
    }

    let mut standalone_q = PlayQueue::new();
    let u1 = MediaUri::Local(local_a.clone());
    let u2 = MediaUri::Local(local_b.clone());
    assert!(!can_step_queue(&standalone_q, None, 1));
    standalone_q.push(QueueItem::new(u2.clone(), "Main", Kind::Video));
    assert!(can_step_queue(&standalone_q, None, 1));
    assert!(can_step_queue(&standalone_q, Some(&u1), 1));
    assert!(!can_step_queue(&standalone_q, Some(&u2), 1));

    let resolved = resolve_queue_paths(vec![local_a.clone()]);
    assert_eq!(resolved.len(), 1);
    let (added, auto_open) = rig.catalog.extend_queue_with_idle_check(resolved, true);
    assert_eq!(added, 1);
    assert_eq!(auto_open.as_ref().map(|q| &q.uri), Some(&u1));

    let plugin_uri = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: "nas".into(),
        entry_id: "/Anime/ep2.mkv".into(),
    };
    let http_uri = MediaUri::HttpUrl("https://cdn.example.com/ep3.mp4".into());
    assert!(
        rig.catalog
            .push_queue_item(QueueItem::new(plugin_uri.clone(), "EP2", Kind::Video))
    );
    assert!(
        !rig.catalog
            .push_queue_item(QueueItem::new(plugin_uri.clone(), "EP2 dup", Kind::Video)),
        "duplicate URI rejected"
    );
    assert!(
        rig.catalog
            .push_queue_item(QueueItem::new(http_uri.clone(), "EP3", Kind::Video))
    );
    assert_eq!(rig.catalog.queue().len(), 3);

    // Deletion-gap anchor stepping: removing middle `plugin_uri` steps `plugin_uri + 1 -> http_uri`!
    assert!(rig.catalog.remove_queue_uri(&plugin_uri));
    match rig.catalog.step(
        &mut rig.plugins,
        &mut rig.player,
        &rig.settings,
        Some(&plugin_uri),
        1,
        false,
        Instant::now(),
    ) {
        StepAction::OpenMediaUri { uri, title } => {
            assert_eq!(uri, http_uri);
            assert_eq!(title.as_deref(), Some("EP3"));
        }
        other => panic!("expected OpenMediaUri(http_uri), got {other:?}"),
    }
    match rig.catalog.step(
        &mut rig.plugins,
        &mut rig.player,
        &rig.settings,
        Some(&http_uri),
        1,
        false,
        Instant::now(),
    ) {
        StepAction::EndOfQueue => {}
        other => panic!("expected EndOfQueue without wrap, got {other:?}"),
    }

    rig.catalog.clear_queue();
    let scan = library::scan_local_or_share(&media_dir);
    rig.catalog.set_local_directory(
        LocalDirectory {
            folder: Some(media_dir.clone()),
            entries: scan.entries,
            subfolders: vec![FolderEntry {
                name: "Extras".into(),
                path: media_dir.join("Extras"),
            }],
        },
        &rig.plugins,
        Some(&u1),
        Some(local_a.as_path()),
    );
    match rig.catalog.step(
        &mut rig.plugins,
        &mut rig.player,
        &rig.settings,
        Some(&u1),
        1,
        false,
        Instant::now(),
    ) {
        StepAction::OpenCatalogItem(CatalogItem { uri, .. }) => assert_eq!(uri, u2),
        other => panic!("expected OpenCatalogItem(u2), got {other:?}"),
    }
    match rig.catalog.step(
        &mut rig.plugins,
        &mut rig.player,
        &rig.settings,
        Some(&u2),
        1,
        false,
        Instant::now(),
    ) {
        StepAction::EndOfList => {}
        other => panic!("expected EndOfList, got {other:?}"),
    }
    assert_eq!(
        rig.catalog.extend_queue_from_catalog(),
        2,
        "folders skipped when extending queue from catalog"
    );
    assert_eq!(rig.catalog.remove_queue_at(0).map(|q| q.uri), Some(u1));

    // Plugin playlist takes precedence over local catalog when queue is empty!
    rig.catalog.clear_queue();
    let init_feed = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetSources {
            sources: vec![SourceDescriptor {
                id: "feed".into(),
                name: "Feed".into(),
                subtitle: None,
                badge: None,
                icon: None,
                supports_search: false,
                requires_auth: false,
                auth_command_id: None,
                configurable: false,
                removable: false,
            }],
        }],
    })
    .unwrap();
    let req_origin = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::RequestOrigin {
            request_id: Id(1),
            origin: server.url.clone(),
        }],
    })
    .unwrap();
    let make_resolve = |entry_id: &str| {
        serde_json::to_string(&Reply::Ok {
            ok: vec![HostCommand::ResolveStream {
                request_id: Id(999_999_991),
                media: Box::new(ResolvedMedia {
                    stream: StreamSpec {
                        source_id: Some("feed".into()),
                        entry_id: Some(entry_id.into()),
                        title: Some(format!("Playlist {entry_id}")),
                        kind: Some(SourceEntryKind::Video),
                        is_live: false,
                        source: StreamSource::DirectHttp {
                            primary: RemoteResource {
                                url: format!("{}/{entry_id}.mp4", server.url),
                                headers: Vec::new(),
                                origin_grant: Some(Id(999_999_992)),
                            },
                            fallback_urls: Vec::new(),
                            identity: None,
                        },
                        variants: Vec::new(),
                        active_variant: None,
                        audio_tracks: Vec::new(),
                        subtitles: Vec::new(),
                    },
                    playlist: vec![
                        PlaylistEntry {
                            source_id: "feed".into(),
                            entry_id: "ep1".into(),
                            title: "EP1".into(),
                        },
                        PlaylistEntry {
                            source_id: "feed".into(),
                            entry_id: "ep2".into(),
                            title: "EP2".into(),
                        },
                    ],
                }),
            }],
        })
        .unwrap()
        .replace("999999991", "{{REQ}}")
        .replace("999999992", "{{GRANT}}")
    };
    let resolve_ep1 = make_resolve("ep1");
    let resolve_ep2 = make_resolve("ep2");
    let resolve_wat = build_routed_wat(
        &[
            (b"w_opened", &init_feed),
            (b"auth_org", &req_origin),
            (b"id\":\"ep1", &resolve_ep1),
            (b"id\":\"ep2", &resolve_ep2),
        ],
        r#"{"ok":[]}"#,
    );
    rig.install_plugin(
        "demo.playlist",
        &[
            Capability::MediaSource,
            Capability::NetHttp,
            Capability::UiModal,
        ],
        &[CommandSpec {
            id: "auth_org".into(),
            title: "Auth".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &resolve_wat,
    );
    rig.wait_for_source("demo.playlist", "feed");

    let plugins = &mut rig.plugins;
    let player = &mut rig.player;
    let settings = &rig.settings;
    let _ = rig.catalog.with_plugin_parts(|store, active_source| {
        plugins.invoke_command(
            PluginContext {
                player,
                store,
                settings,
                active_source,
            },
            "demo.playlist",
            "auth_org",
            Instant::now(),
        )
    });
    wait_until("origin sheet", || {
        let _ = rig.poll(Instant::now());
        rig.plugins.origin_summary().is_some()
    });
    let origin_id = rig.plugins.origin_summary().unwrap().id;
    assert!(rig.plugins.toggle_insecure_auth(origin_id));
    let _ = rig.plugins.approve_host_consent();
    wait_until("origin granted acked", || {
        let _ = rig.poll(Instant::now());
        !rig.plugins.has_host_consent()
            && rig
                .plugins
                .active_modal()
                .is_some_and(|(p, m)| p == "demo.playlist" && m.id == "origin_ack")
    });
    let plugins = &mut rig.plugins;
    let player = &mut rig.player;
    let settings = &rig.settings;
    let _ = rig.catalog.with_plugin_parts(|store, active_source| {
        plugins.cancel_active_modal(
            PluginContext {
                player,
                store,
                settings,
                active_source,
            },
            Instant::now(),
        )
    });

    let plugins = &mut rig.plugins;
    let player = &mut rig.player;
    let settings = &rig.settings;
    let _ = rig.catalog.with_plugin_parts(|store, active_source| {
        plugins.resolve_entry(
            PluginContext {
                player,
                store,
                settings,
                active_source,
            },
            "demo.playlist",
            "feed",
            "ep2",
            Some("EP2".into()),
            None,
            Instant::now(),
        )
    });
    wait_until("playlist committed on resolve", || {
        let _ = rig.poll(Instant::now());
        rig.plugins.playlist().len() == 2
    });
    let ep2_uri = MediaUri::Plugin {
        plugin_id: "demo.playlist".into(),
        source_id: "feed".into(),
        entry_id: "ep2".into(),
    };
    assert!(rig.catalog.can_step_media(&rig.plugins, Some(&ep2_uri), -1));
    assert!(matches!(
        rig.catalog.step(
            &mut rig.plugins,
            &mut rig.player,
            &rig.settings,
            Some(&ep2_uri),
            -1,
            false,
            Instant::now(),
        ),
        StepAction::PluginPlaylist(_)
    ));
}
