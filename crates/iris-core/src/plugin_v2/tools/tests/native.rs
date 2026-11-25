//! Actual BYOB CLI compatibility fixtures; the script supplies reviewed paths.
use crate::{
    plugin_v2::{
        actor::{Executor, IoOutcome, Operation, TaskContext},
        files::{ExportGrant, InputAction, identity::PinnedFile, random_name},
        ledger::{Ledger, Resource},
        network::NetworkGrant,
        tools::{
            ToolExecutor, ToolSetup,
            binding::ToolKind,
            catalog::{Receipt, Selection, ToolCatalog},
        },
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role,
    enhance::ExportFormat,
    task::{MediaContainer, SampleRate, Task, ToolTask},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
fn digest(value: &str) -> [u8; 32] {
    assert_eq!(value.len(), 64);
    let mut out = [0; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap();
    }
    out
}
fn setup() -> (ToolExecutor, Ledger, PathBuf, Value) {
    let receipt =
        std::env::var("IRIS_V2_TOOLS_RECEIPT").expect("run scripts/test-plugin-v2-tools.py");
    let receipt: Value = serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
    let directory =
        std::env::temp_dir().join(format!("iris-tool-contract-{}", random_name().unwrap()));
    std::fs::create_dir(&directory).unwrap();
    let ledger = Ledger::default();
    let catalog = ToolCatalog::default();
    for (name, kind) in [
        ("ffmpeg", ToolKind::Ffmpeg),
        ("ffprobe", ToolKind::Ffprobe),
        ("whisper", ToolKind::WhisperCpp),
        ("ytdlp", ToolKind::YtDlp),
    ] {
        let entry = &receipt[name];
        let components = entry["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    PathBuf::from(v["path"].as_str().unwrap()),
                    digest(v["sha256"].as_str().unwrap()),
                )
            })
            .collect();
        catalog
            .approve(
                Receipt {
                    selection: Selection::Tool(kind),
                    path: PathBuf::from(entry["path"].as_str().unwrap()),
                    sha256: digest(entry["sha256"].as_str().unwrap()),
                    components,
                },
                &directory,
                &HttpCancel::default(),
            )
            .unwrap_or_else(|e| panic!("{name} binding: {e:?}"));
    }
    let model = &receipt["model"];
    catalog
        .approve(
            Receipt {
                selection: Selection::WhisperModel,
                path: PathBuf::from(model["path"].as_str().unwrap()),
                sha256: digest(model["sha256"].as_str().unwrap()),
                components: Vec::new(),
            },
            &directory,
            &HttpCancel::default(),
        )
        .unwrap();
    let capabilities = BTreeSet::from([
        Capability::ExternalTool,
        Capability::ExportFiles,
        Capability::ReadState,
        Capability::NetHttp,
    ]);
    let network = NetworkGrant::new(
        "native-tests".into(),
        capabilities.clone(),
        NetworkDeclaration {
            net_public: true,
            ..Default::default()
        },
        &ledger,
    )
    .unwrap();
    let executor = ToolExecutor::new(ToolSetup {
        network,
        ledger: ledger.clone(),
        capabilities,
        temporary_directory: directory.clone(),
        tools: BTreeMap::new(),
        models: BTreeMap::new(),
        catalog,
    })
    .unwrap();
    (executor, ledger, directory, receipt)
}
fn context(ledger: &Ledger) -> TaskContext {
    TaskContext {
        instance_id: Id(91),
        window: None,
        owner: "native-tests".into(),
        role: Role::Control,
        generation: Generation::default(),
        cancel: HttpCancel::default(),
        ledger: ledger.clone(),
    }
}
fn call(executor: &ToolExecutor, ledger: &Ledger, task: Task) -> Result<IoOutcome, ErrorCode> {
    match executor.prepare(task, &BTreeMap::new(), context(ledger))? {
        Operation::Blocking(run) => run(),
        _ => panic!("expected blocking tool operation"),
    }
}
fn handle(outcome: &IoOutcome) -> Id {
    Id(outcome.value["artifact"].as_str().unwrap().parse().unwrap())
}
fn cleanup(executor: ToolExecutor, ledger: Ledger, dir: PathBuf) {
    executor.stop();
    let end = Instant::now() + Duration::from_secs(5);
    while !executor.is_quiescent() {
        assert!(Instant::now() < end, "tool cleanup not quiescent");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(ledger.used(None, Resource::Tool), 0);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    drop(executor);
    std::fs::remove_dir(dir).unwrap();
}
#[test]
#[ignore = "requires pinned native tools/model; run scripts/test-plugin-v2-tools.py"]
fn ffmpeg_extract_slice_and_whisper_use_real_semantic_adapters() {
    let (executor, ledger, dir, receipt) = setup();
    let available = call(&executor, &ledger, Task::QueryTools).unwrap();
    let model = Id(available.value["models"][0]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    assert!(
        available.value["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "transcribe")
    );
    drop(available);
    let audio = Path::new(receipt["speech"].as_str().unwrap());
    let input = executor
        .input_grants()
        .approve_input(
            audio,
            Id(8),
            Generation::default(),
            InputAction::ExtractAudio,
        )
        .unwrap();
    let extracted = call(
        &executor,
        &ledger,
        Task::InvokeTool {
            task: ToolTask::ExtractAudio {
                input,
                track: 0,
                sample_rate: SampleRate::Hz16000,
            },
        },
    )
    .expect("extract actual audio");
    let read = call(
        &executor,
        &ledger,
        Task::ArtifactRead {
            handle: handle(&extracted),
            offset: Id(0),
            len: 44,
        },
    )
    .unwrap();
    assert_eq!(&read.blob.as_ref().unwrap().bytes[..4], b"RIFF");
    drop(read);
    let subtitle = call(
        &executor,
        &ledger,
        Task::InvokeTool {
            task: ToolTask::Transcribe {
                audio: handle(&extracted),
                model,
                language: Some("en".into()),
            },
        },
    )
    .expect("transcribe actual audio");
    let read = call(
        &executor,
        &ledger,
        Task::ArtifactRead {
            handle: handle(&subtitle),
            offset: Id(0),
            len: 65536,
        },
    )
    .unwrap();
    let text = String::from_utf8(read.blob.as_ref().unwrap().bytes.clone()).unwrap();
    assert!(text.starts_with("WEBVTT\n"));
    assert!(text.to_ascii_lowercase().contains("country"), "{text}");
    drop(read);
    drop(subtitle);
    drop(extracted);
    let video = Path::new(receipt["video"].as_str().unwrap());
    let selected = Arc::new(PinnedFile::open(video).unwrap());
    let before = selected.sha256(&HttpCancel::default()).unwrap();
    let input = executor
        .input_grants()
        .approve_input(video, Id(8), Generation::default(), InputAction::Slice)
        .unwrap();
    let destination = dir.join("clip.mkv");
    let export = executor
        .add_export(
            ExportGrant::approve(
                &destination,
                ExportFormat::Matroska,
                "native-tests".into(),
                Generation::default(),
                &ledger,
            )
            .unwrap(),
        )
        .unwrap();
    let clipped = call(
        &executor,
        &ledger,
        Task::InvokeTool {
            task: ToolTask::LosslessSlice {
                input,
                export,
                start_ms: Id(1300),
                end_ms: Id(3000),
                container: MediaContainer::Matroska,
            },
        },
    )
    .expect("slice actual video");
    assert!(clipped.value["exported"].as_bool().unwrap());
    let start: i64 = clipped.value["actual_start_ms"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let end: i64 = clipped.value["actual_end_ms"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((900..=1400).contains(&start), "actual start {start}");
    assert!((2900..=3100).contains(&end), "actual end {end}");
    assert!(std::fs::metadata(&destination).unwrap().len() > 1000);
    assert_eq!(selected.sha256(&HttpCancel::default()).unwrap(), before);
    drop(clipped);
    drop(selected);
    std::fs::remove_file(destination).unwrap();
    cleanup(executor, ledger, dir);
}
#[test]
#[ignore = "requires real yt-dlp and public fixture network; run scripts/test-plugin-v2-tools.py"]
fn yt_dlp_metadata_runs_through_the_authorized_proxy_without_downloading_media() {
    let (executor, ledger, dir, receipt) = setup();
    let url = receipt["metadata_url"].as_str().unwrap();
    let metadata = call(
        &executor,
        &ledger,
        Task::InvokeTool {
            task: ToolTask::ResolveMetadata {
                url: url.into(),
                origin_grant: None,
            },
        },
    )
    .expect("resolve public fixture metadata");
    let read = call(
        &executor,
        &ledger,
        Task::ArtifactRead {
            handle: handle(&metadata),
            offset: Id(0),
            len: 1 << 20,
        },
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&read.blob.as_ref().unwrap().bytes).unwrap();
    assert!(!json["formats"].as_array().unwrap().is_empty());
    for format in json["formats"].as_array().unwrap() {
        assert!(format["url"].as_str().unwrap().starts_with("https://"));
    }
    drop(read);
    drop(metadata);
    cleanup(executor, ledger, dir);
}
