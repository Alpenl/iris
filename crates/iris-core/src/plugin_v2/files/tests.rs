use super::*;
use crate::vfs::HttpCancel;
use iris_api::v2::{enhance::ExportFormat, task::FileHash};
use std::time::Instant;
fn directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!("iris-files-{}", random_name().unwrap()));
    std::fs::create_dir(&path).unwrap();
    path
}
use std::path::PathBuf;
fn idle(root: &TempRoot) {
    let end = Instant::now() + Duration::from_secs(3);
    while !root.is_quiescent() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn input_is_single_use_owner_action_and_generation_bound_and_pins_identity() {
    let dir = directory();
    let path = dir.join("input.bin");
    std::fs::write(&path, b"abc").unwrap();
    let ledger = Ledger::default();
    let grants = FileGrants::new("one".into(), ledger.clone());
    let generation = Generation::default();
    let id = grants
        .approve_input(&path, Id(9), generation, InputAction::ExtractAudio)
        .unwrap();
    assert!(
        grants
            .consume_input("two", id, generation, InputAction::ExtractAudio)
            .is_err()
    );
    assert!(
        grants
            .consume_input("one", id, generation, InputAction::Slice)
            .is_err()
    );
    assert!(
        grants
            .consume_input(
                "one",
                id,
                Generation {
                    seek: Id(1),
                    ..generation
                },
                InputAction::ExtractAudio
            )
            .is_err()
    );
    let input = grants
        .consume_input("one", id, generation, InputAction::ExtractAudio)
        .unwrap();
    assert!(
        grants
            .consume_input("one", id, generation, InputAction::ExtractAudio)
            .is_err()
    );
    #[cfg(windows)]
    {
        assert!(std::fs::write(&path, b"tampered").is_err());
        assert!(std::fs::remove_file(&path).is_err());
    }
    assert_eq!(
        hash_media(
            &input.file,
            FileHash::FullMd5,
            &HttpCancel::default(),
            |_, _| {}
        )
        .unwrap()
        .digest,
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(
        hash_media(
            &input.file,
            FileHash::FullSha256,
            &HttpCancel::default(),
            |_, _| {}
        )
        .unwrap()
        .digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let sparse = hash_media(
        &input.file,
        FileHash::SparseSha256V1,
        &HttpCancel::default(),
        |_, _| {},
    )
    .unwrap();
    let mut vector = b"iris-sparse-sha256-v1\0".to_vec();
    vector.extend_from_slice(&3u64.to_le_bytes());
    vector.extend_from_slice(&0u64.to_le_bytes());
    vector.extend_from_slice(&3u64.to_le_bytes());
    vector.extend_from_slice(b"abc");
    let expected: String = ring::digest::digest(&ring::digest::SHA256, &vector)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(sparse.digest, expected);
    let cancel = HttpCancel::default();
    cancel.cancel();
    assert!(matches!(
        hash_media(&input.file, FileHash::FullSha256, &cancel, |_, _| {}),
        Err(ErrorCode::Cancelled)
    ));
    drop(input);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    std::fs::hard_link(&path, dir.join("alias.bin")).unwrap();
    assert!(PinnedFile::open(&path).is_err());
    std::fs::remove_file(dir.join("alias.bin")).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn artifacts_keep_disk_quota_until_cleanup_and_export_never_overwrites_a_racing_target() {
    let dir = directory();
    let ledger = Ledger::default();
    let root = TempRoot::new(&dir, "one".into(), ledger.clone()).unwrap();
    let cancel = HttpCancel::default();
    let generation = Generation::default();
    let mut writer = root.writer("txt").unwrap();
    writer.write(b"result", &cancel).unwrap();
    let artifact = writer.finish(&cancel).unwrap();
    assert_eq!(ledger.used(Some("one"), Resource::TempBytes), 6);
    let target = dir.join("result.txt");
    let export = ExportGrant::approve(
        &target,
        ExportFormat::Text,
        "one".into(),
        generation,
        &ledger,
    )
    .unwrap();
    std::fs::write(&target, b"racing user data").unwrap();
    assert!(
        export
            .commit(&artifact, "one", generation, &ledger, &cancel)
            .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"racing user data");
    assert!(
        ExportGrant::approve(
            &target,
            ExportFormat::Text,
            "one".into(),
            generation,
            &ledger
        )
        .is_err()
    );
    let output = dir.join("new.txt");
    ExportGrant::approve(
        &output,
        ExportFormat::Text,
        "one".into(),
        generation,
        &ledger,
    )
    .unwrap()
    .commit(&artifact, "one", generation, &ledger, &cancel)
    .unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), b"result");
    for name in [
        "bad.exe",
        "bad.txt:stream",
        "CON.txt",
        "trailing.txt ",
        "..txt",
    ] {
        assert!(
            ExportGrant::approve(
                &dir.join(name),
                ExportFormat::Text,
                "one".into(),
                generation,
                &ledger
            )
            .is_err()
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while ledger.used(Some("one"), Resource::TempBytes) != 6 {
        assert!(
            std::time::Instant::now() < deadline,
            "export staging cleanup"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let hold = ledger
        .reserve("one", &[(Resource::TempBytes, (1 << 30) - 6)])
        .unwrap();
    let mut rejected = root.writer("txt").unwrap();
    assert_eq!(
        rejected.write(b"over quota", &cancel),
        Err(ErrorCode::QuotaExceeded)
    );
    drop(rejected);
    drop(hold);
    drop(artifact);
    idle(&root);
    assert_eq!(ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    drop(root);
    std::fs::remove_file(target).unwrap();
    std::fs::remove_file(output).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
