use super::*;
#[test]
fn binding_reviews_pin_bytes_persist_and_revoke_running_tasks() {
    let dir = std::env::temp_dir().join(format!(
        "iris-tool-catalog-{}",
        crate::plugin_v2::files::random_name().unwrap()
    ));
    std::fs::create_dir(&dir).unwrap();
    let model = dir.join("model.bin");
    std::fs::write(&model, b"approved model").unwrap();
    let config = dir.join("bindings.json");
    let catalog = ToolCatalog::new(Some(config.clone()));
    let receipt = catalog
        .inspect(Selection::WhisperModel, &model, &HttpCancel::default())
        .unwrap();
    assert!(!config.exists());
    assert!(catalog.availability().1.is_empty());
    std::fs::write(&model, b"unapproved replacement").unwrap();
    assert_eq!(
        catalog.approve(receipt, &dir, &HttpCancel::default()),
        Err(ErrorCode::Denied)
    );
    assert!(!config.exists());
    let receipt = catalog
        .inspect(Selection::WhisperModel, &model, &HttpCancel::default())
        .unwrap();
    catalog
        .approve(receipt, &dir, &HttpCancel::default())
        .unwrap();
    let id = catalog.availability().1[0];
    let cancel = HttpCancel::default();
    let held = catalog.model(id, &cancel).unwrap();
    catalog.remove(Selection::WhisperModel).unwrap();
    assert!(cancel.is_cancelled());
    assert!(catalog.model(id, &HttpCancel::default()).is_err());
    drop(held);
    let receipt = catalog
        .inspect(Selection::WhisperModel, &model, &HttpCancel::default())
        .unwrap();
    catalog
        .approve(receipt, &dir, &HttpCancel::default())
        .unwrap();
    drop(catalog);
    let restored = ToolCatalog::new(Some(config.clone()));
    restored.restore(&dir).unwrap();
    assert_eq!(restored.availability().1.len(), 1);
    drop(restored);
    std::fs::write(&model, b"changed after restart").unwrap();
    let restored = ToolCatalog::new(Some(config.clone()));
    let saved = std::fs::read(&config).unwrap();
    assert_eq!(restored.restore(&dir), Err(ErrorCode::Denied));
    assert_eq!(std::fs::read(&config).unwrap(), saved);
    assert!(restored.availability().1.is_empty());
    assert_eq!(restored.summary().1, Some(ErrorCode::Denied));
    drop(restored);
    std::fs::remove_file(model).unwrap();
    std::fs::remove_file(config).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn inspection_never_executes_a_candidate_and_records_only_bounded_helpers() {
    let dir = std::env::temp_dir().join(format!(
        "iris-tool-inspect-{}",
        crate::plugin_v2::files::random_name().unwrap()
    ));
    std::fs::create_dir(&dir).unwrap();
    let program = dir.join("not-a-program.exe");
    let helper = dir.join("helper.dll");
    std::fs::write(&program, b"not executable").unwrap();
    std::fs::write(&helper, b"not a library").unwrap();
    let catalog = ToolCatalog::default();
    let receipt = catalog
        .inspect(
            Selection::Tool(ToolKind::Ffmpeg),
            &program,
            &HttpCancel::default(),
        )
        .unwrap();
    assert_eq!(receipt.components.len(), 1);
    assert!(catalog.availability().0.is_empty());
    let cancel = HttpCancel::default();
    cancel.cancel();
    assert!(
        catalog
            .inspect(Selection::Tool(ToolKind::Ffmpeg), &program, &cancel)
            .is_err()
    );
    std::fs::remove_file(program).unwrap();
    std::fs::remove_file(helper).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
