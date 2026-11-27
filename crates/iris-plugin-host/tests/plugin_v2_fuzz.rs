//! Deterministic corpus replay, HLS parser differential baseline, outer watchdog,
//! and seeded bounded mutation regression for host-facing parsers.
//! This is deterministic replay + bounded PRNG mutation, not coverage-guided fuzzing
//! or a substitute for sanitizer campaigns.

use iris_api::v2::{
    ErrorCode, HostCommand, MESSAGE_LIMIT, Manifest,
    stream::{ManifestFormat, TrackKind},
    task::Task,
};
use iris_core::bounded_json::decode_exact;
use iris_plugin_host::{
    package::{Package, Provenance},
    stream::{manifest, prepare, remote_manifest},
    validation,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const HLS_DIFF_BASE_URL: &str = "https://media.example/hls/index.m3u8";
const MUTATION_TAG: u64 = 1u64 << 63;
const INITIALIZING_CASE: u64 = u64::MAX;

struct CorpusSeed {
    id: String,
    bytes: Vec<u8>,
}

/// Single outer watchdog for an entire replay/mutation campaign.
/// Each case updates an `AtomicU64` before running (zero per-case thread spawns).
/// If a parser enters an infinite loop or hangs past `timeout`, the watchdog prints
/// the offending case identifier and calls `std::process::abort()`.
struct Watchdog {
    done: Arc<(Mutex<bool>, Condvar)>,
    current: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(case_ids: Arc<Vec<String>>, seed: u64, timeout: Duration) -> Self {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let current = Arc::new(AtomicU64::new(INITIALIZING_CASE));
        let done_bg = Arc::clone(&done);
        let current_bg = Arc::clone(&current);
        let handle = std::thread::Builder::new()
            .name("plugin-v2-fuzz-watchdog".into())
            .spawn(move || {
                let (lock, cv) = &*done_bg;
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                let (guard, _) = cv
                    .wait_timeout_while(guard, timeout, |finished| !*finished)
                    .unwrap_or_else(PoisonError::into_inner);
                if *guard {
                    return;
                }
                let raw = current_bg.load(Ordering::Acquire);
                let label = format_case_label(raw, &case_ids, seed);
                eprintln!(
                    "plugin_v2_fuzz watchdog timeout ({timeout:?}) while executing case: {label}"
                );
                let _ = std::io::stderr().flush();
                std::process::abort();
            })
            .expect("spawn outer fuzz watchdog");
        Self {
            done,
            current,
            handle: Some(handle),
        }
    }

    fn set_corpus_case(&self, index: usize) {
        self.current.store(index as u64, Ordering::Release);
    }

    fn set_mutation_case(&self, iteration: u64) {
        self.current
            .store(MUTATION_TAG | iteration, Ordering::Release);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let (lock, cv) = &*self.done;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cv.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn format_case_label(raw: u64, case_ids: &[String], seed: u64) -> String {
    if raw == INITIALIZING_CASE {
        "initializing".into()
    } else if (raw & MUTATION_TAG) != 0 {
        let iteration = raw & !MUTATION_TAG;
        format!("mutation:seed={seed}:iteration={iteration}")
    } else {
        let idx = raw as usize;
        case_ids
            .get(idx)
            .cloned()
            .unwrap_or_else(|| format!("corpus_index:{idx}"))
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("iris-plugin-host sits under <workspace>/crates/")
        .to_path_buf()
}

fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Single discovery entry traversing both `crates/iris-plugin-host/tests/corpus/plugin-v2`
/// and `<workspace>/fuzz/corpus/{hls,range,http_head,chunked,doh}`.
fn discover_corpus() -> Vec<CorpusSeed> {
    let root = workspace_root();
    let plugin_v2_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/plugin-v2");
    let fuzz_dir = root.join("fuzz/corpus");

    for required in ["hls", "range", "http_head", "chunked", "doh"] {
        let category = fuzz_dir.join(required);
        assert!(
            category.is_dir(),
            "missing fuzz corpus directory: {}",
            category.display()
        );
        let has_seed = std::fs::read_dir(&category)
            .unwrap()
            .any(|e| e.unwrap().path().is_file());
        assert!(
            has_seed,
            "empty fuzz corpus directory: {}",
            category.display()
        );
    }

    let mut legacy_paths = Vec::new();
    collect_files_recursive(&plugin_v2_dir, &mut legacy_paths);
    let mut fuzz_paths = Vec::new();
    collect_files_recursive(&fuzz_dir, &mut fuzz_paths);

    let mut legacy_payloads = BTreeSet::new();
    let mut seeds = Vec::with_capacity(legacy_paths.len() + fuzz_paths.len());
    for path in legacy_paths {
        let bytes = std::fs::read(&path).unwrap();
        legacy_payloads.insert(bytes.clone());
        let id = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        seeds.push(CorpusSeed { id, bytes });
    }
    for path in fuzz_paths {
        let bytes = std::fs::read(&path).unwrap();
        let id = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            !legacy_payloads.contains(&bytes),
            "seed {id} duplicates an existing file in tests/corpus/plugin-v2"
        );
        seeds.push(CorpusSeed { id, bytes });
    }
    seeds
}

fn exercise(input: &[u8]) {
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = iris_plugin_host::network::normalize_url(
            text,
            iris_plugin_host::network::TransportKind::Http,
        );
        let _ = iris_plugin_host::network::parse_consent_origin_url(text);
        let _ = iris_plugin_host::package::validate_path(text);
        for size in [None, Some(0), Some(1024), Some(u64::MAX)] {
            let trimmed = text.trim();
            let base_range = iris_base::net::range::HttpRange::parse(Some(trimmed), size);
            let host_range = iris_plugin_host::stream::range::Range::parse(Some(trimmed), size);
            assert_eq!(base_range.is_ok(), host_range.is_ok());
            if let Ok(range) = host_range {
                let base = base_range.unwrap();
                assert_eq!(
                    (range.offset, range.len, range.partial, range.total),
                    (base.offset, base.len, base.partial, base.total)
                );
                if let Some(len) = range.len {
                    assert!(len > 0);
                    assert!(range.offset.checked_add(len - 1).is_some());
                    if let Some(size) = size {
                        assert!(range.offset + len - 1 < size);
                    }
                }
                assert_eq!(range.content_range(), base.content_range());
            }
        }
        let _ = remote_manifest::parse_hls_attributes(text);
        let mut bound = 0usize;
        let _ = remote_manifest::parse_hls_media(text, TrackKind::Video, |_, _, _| {
            bound += 1;
            Ok(format!("seg_{bound}"))
        });
        let _ = iris_core::player::fuzz_hls::parse_and_extract_uris(text, HLS_DIFF_BASE_URL);
    }
    let _ = prepare::subtitle_format(input);
    iris_base::net::fuzz_net::exercise(input);
    if let Ok(command) = decode_exact::<HostCommand>(input, MESSAGE_LIMIT) {
        let _ = validation::command(&command);
    }
    let _ = decode_exact::<Task>(input, MESSAGE_LIMIT);
    if let Ok(manifest) = decode_exact::<Manifest>(input, MESSAGE_LIMIT) {
        let _ = manifest.validate();
    }
    let _ = Package::from_zip(input, Provenance::User);
    if let Ok(value) = manifest::compile(ManifestFormat::VirtualHls, input, &|id, _| {
        Ok(format!("http://127.0.0.1:1234/{id}"))
    }) {
        for document in value.documents {
            assert!(document.bytes.len() <= manifest::OUTPUT_LIMIT);
            std::str::from_utf8(&document.bytes).unwrap();
        }
    }
}

fn parse_host_hls_uris(input: &str, base_url: &str) -> Result<Vec<String>, ErrorCode> {
    let base = url::Url::parse(base_url).unwrap();
    let mut uris = Vec::new();
    let mut ids = BTreeMap::<(String, String), String>::new();
    remote_manifest::parse_hls_media(input, TrackKind::Video, |raw_uri, kind, ext| {
        let resolved = base
            .join(raw_uri)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| raw_uri.to_owned());
        uris.push(resolved.clone());
        let next_id = format!("res_{}", ids.len() + 1);
        let id = ids
            .entry((format!("{kind:?}:{ext:?}"), resolved))
            .or_insert(next_id)
            .clone();
        Ok(id)
    })?;
    Ok(uris)
}

#[test]
fn seeded_plugin_parser_mutations() {
    let plugin_v2_corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/plugin-v2");
    let fuzz_corpus = workspace_root().join("fuzz/corpus");
    // Valid seeds must actually reach the renderers and parsers; malformed-input-only
    // mutation campaigns would miss most conversion logic.
    manifest::compile(
        ManifestFormat::VirtualHls,
        &std::fs::read(plugin_v2_corpus.join("virtual-hls.json")).unwrap(),
        &|id, _| Ok(format!("http://127.0.0.1:1234/{id}")),
    )
    .expect("valid renderer seed");
    Package::from_zip(
        &std::fs::read(plugin_v2_corpus.join("theme.irisplugin")).unwrap(),
        Provenance::User,
    )
    .expect("valid package seed");
    let hls_media_seed =
        std::fs::read_to_string(fuzz_corpus.join("hls/valid_fmp4_aes128.m3u8")).unwrap();
    remote_manifest::parse_hls_media(&hls_media_seed, TrackKind::Video, |_, _, _| {
        Ok("bound_res".into())
    })
    .expect("valid hls media seed");
    let http_206 =
        std::fs::read(fuzz_corpus.join("http_head/valid_100_continue_206.http")).unwrap();
    assert_eq!(
        iris_base::net::http_client::parse_response_head(&http_206)
            .expect("valid http 206 seed")
            .map(|(status, _, _)| status),
        Some(206)
    );
    let loopback_req =
        std::fs::read(fuzz_corpus.join("http_head/valid_loopback_get_range.http")).unwrap();
    iris_base::net::loopback_http::parse_http_request(&loopback_req, "127.0.0.1:18080")
        .expect("valid loopback request seed");
    let chunked =
        std::fs::read(fuzz_corpus.join("chunked/valid_extensions_and_trailers.chunked")).unwrap();
    let mut decoded_chunked = Vec::new();
    std::io::Read::read_to_end(
        &mut iris_base::net::http_client::ChunkedReader::new(std::io::Cursor::new(&chunked)),
        &mut decoded_chunked,
    )
    .expect("valid chunked seed");
    assert_eq!(decoded_chunked, b"Wikipedia");
    let doh_resp = std::fs::read(fuzz_corpus.join("doh/valid_http_dns_a_response.doh")).unwrap();
    iris_base::net::http_client::doh::parse_doh_response_bytes(&doh_resp).expect("valid doh seed");
    let open_range =
        std::fs::read_to_string(fuzz_corpus.join("range/valid_open_ended.txt")).unwrap();
    iris_plugin_host::stream::range::Range::parse(Some(open_range.trim()), Some(8192))
        .expect("valid range seed");
    iris_base::net::hls::parse_lines(
        &hls_media_seed,
        iris_base::net::hls::HlsLineOptions::CORE_REWRITE,
    )
    .expect("valid base hls lines")
    .collect::<Result<Vec<_>, _>>()
    .expect("valid base hls line items");
    assert_eq!(
        prepare::subtitle_format(b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n"),
        Ok("vtt")
    );
    assert_eq!(
        prepare::subtitle_format(b"1\n00:00:00,000 --> 00:00:01,500\nhello\n"),
        Ok("srt")
    );
    assert_eq!(
        prepare::subtitle_format(
            b"[Script Info]\nTitle: test\n[Events]\nDialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,hi\n"
        ),
        Ok("ass")
    );

    let corpus = discover_corpus();
    let iterations = std::env::var("IRIS_FUZZ_ITERATIONS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(2000);
    let initial = std::env::var("IRIS_FUZZ_SEED")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(0x4952_4953_5632);
    let watchdog_ms = std::env::var("IRIS_FUZZ_WATCHDOG_MS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(60_000);
    assert!(initial != 0 && iterations <= 20_000_000 && watchdog_ms > 0);

    let case_ids = Arc::new(corpus.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
    let watchdog = Watchdog::start(case_ids, initial, Duration::from_millis(watchdog_ms));
    let start = Instant::now();

    for (idx, seed) in corpus.iter().enumerate() {
        watchdog.set_corpus_case(idx);
        let case_start = Instant::now();
        exercise(&seed.bytes);
        // Loose pathology ceiling (>= 1s in debug builds) avoids shared-CI scheduler flake.
        assert!(
            case_start.elapsed() < Duration::from_secs(5),
            "corpus seed {} exceeded loose pathology bound ({:?})",
            seed.id,
            case_start.elapsed()
        );
    }

    let mut random = initial;
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random as usize
    };
    for index in 0..iterations {
        watchdog.set_mutation_case(index);
        let mut input = corpus[next() % corpus.len()].bytes.clone();
        for _ in 0..(1 + next() % 8) {
            let pos = next() % (input.len() + 1);
            match next() % 5 {
                0 if !input.is_empty() => {
                    let n = pos.min(input.len() - 1);
                    input[n] ^= (next() & 255) as u8;
                }
                1 => input.truncate(pos),
                2 if input.len() < 8192 => input.insert(pos, (next() & 255) as u8),
                3 if input.len() < 8192 => {
                    let piece = corpus[next() % corpus.len()].bytes.as_slice();
                    input.splice(pos..pos, piece.iter().copied().take(512));
                }
                _ => {}
            }
        }
        if std::panic::catch_unwind(|| exercise(&input)).is_err() {
            let path = std::env::temp_dir().join(format!("iris-v2-fuzz-{initial}-{index}.bin"));
            std::fs::write(&path, &input).unwrap();
            panic!(
                "parser mutation failed; seed={initial}, iteration={index}, input={}",
                path.display()
            );
        }
    }
    drop(watchdog);
    println!(
        "mutation receipt: corpus={} seed={initial} iterations={iterations} elapsed_ms={}",
        corpus.len(),
        start.elapsed().as_millis()
    );
}

#[test]
fn hls_corpus_differential_baseline_between_core_and_plugin_host() {
    type Expected = (
        Result<&'static [&'static str], ()>,
        Result<&'static [&'static str], ErrorCode>,
    );

    // Locked R-01 differential table between `iris-core/src/player/relay/playlist.rs`
    // and `iris-plugin-host/src/stream/remote_manifest/hls_parse.rs` for R-06 grammar unification.
    let entries: [(&str, Expected); 13] = [
        (
            "valid_fmp4_aes128.m3u8",
            (
                Ok(&[
                    "https://media.example/hls/init.mp4",
                    "https://media.example/hls/key.bin",
                    "https://media.example/hls/seg0.m4s",
                ]),
                Ok(&[
                    "https://media.example/hls/init.mp4",
                    "https://media.example/hls/key.bin",
                    "https://media.example/hls/seg0.m4s",
                ]),
            ),
        ),
        (
            "valid_byterange_discontinuity.m3u8",
            (
                Ok(&[
                    "https://media.example/hls/main.ts",
                    "https://media.example/hls/main.ts",
                ]),
                Ok(&[
                    "https://media.example/hls/main.ts",
                    "https://media.example/hls/main.ts",
                ]),
            ),
        ),
        (
            "diff_master_multivariant.m3u8",
            (
                Ok(&[
                    "https://media.example/hls/audio/zh.m3u8",
                    "https://media.example/hls/video/720p.m3u8",
                ]),
                Err(ErrorCode::Unsupported),
            ),
        ),
        (
            "diff_unknown_tag_and_comment.m3u8",
            (
                Ok(&["https://media.example/hls/seg0.ts"]),
                Err(ErrorCode::Unsupported),
            ),
        ),
        (
            "diff_surrounding_whitespace.m3u8",
            (
                Ok(&["https://media.example/hls/seg_ws.ts"]),
                Err(ErrorCode::InvalidArgument),
            ),
        ),
        (
            "diff_missing_targetduration.m3u8",
            (
                Ok(&["https://media.example/hls/seg_bare.ts"]),
                Err(ErrorCode::InvalidArgument),
            ),
        ),
        (
            "diff_ll_hls_part_preload.m3u8",
            (
                Ok(&[
                    "https://media.example/hls/part0.m4s",
                    "https://media.example/hls/part1.m4s",
                    "https://media.example/hls/seg0.m4s",
                ]),
                Err(ErrorCode::Unsupported),
            ),
        ),
        (
            "diff_attr_value_charset.m3u8",
            (
                Ok(&["https://media.example/hls/seg0.ts"]),
                Err(ErrorCode::Unsupported),
            ),
        ),
        (
            "diff_long_extinf_title.m3u8",
            (
                Ok(&["https://media.example/hls/seg0.ts"]),
                Err(ErrorCode::QuotaExceeded),
            ),
        ),
        (
            "diff_file_scheme_uri.m3u8",
            (Err(()), Ok(&["file:///etc/passwd"])),
        ),
        (
            "malformed_indirection_define.m3u8",
            (Err(()), Err(ErrorCode::Unsupported)),
        ),
        (
            "malformed_attr_syntax.m3u8",
            (Err(()), Err(ErrorCode::InvalidArgument)),
        ),
        (
            "truncated_unclosed_quote.m3u8",
            (Err(()), Err(ErrorCode::InvalidArgument)),
        ),
    ];
    let expected = BTreeMap::from(entries);

    // Attribute-level divergence: `core` permits empty quoted non-URI values and unquoted
    // punctuation such as `:`, whereas `hls_parse::attributes` rejects both.
    assert_eq!(
        remote_manifest::parse_hls_attributes("ID=\"ad-1\",CLASS=\"\""),
        Err(ErrorCode::InvalidArgument)
    );
    assert_eq!(
        remote_manifest::parse_hls_attributes("CODECS=avc1:640028"),
        Err(ErrorCode::InvalidArgument)
    );

    let hls_seeds: Vec<_> = discover_corpus()
        .into_iter()
        .filter(|s| s.id.starts_with("fuzz/corpus/hls/"))
        .collect();
    assert_eq!(
        hls_seeds.len(),
        expected.len(),
        "every seed in fuzz/corpus/hls/ must be covered in the differential baseline table"
    );

    for seed in hls_seeds {
        let name = seed.id.strip_prefix("fuzz/corpus/hls/").unwrap();
        let (exp_core, exp_host) = expected
            .get(name)
            .unwrap_or_else(|| panic!("untracked HLS seed in differential table: {name}"));
        let text = std::str::from_utf8(&seed.bytes).expect("utf-8 HLS seed");

        let core_res = iris_core::player::fuzz_hls::parse_and_extract_uris(text, HLS_DIFF_BASE_URL)
            .map_err(|_| ());
        let host_res = parse_host_hls_uris(text, HLS_DIFF_BASE_URL);

        let exp_core_owned = exp_core.map(|s| s.iter().map(|u| (*u).to_owned()).collect());
        let exp_host_owned = exp_host.map(|s| s.iter().map(|u| (*u).to_owned()).collect());
        assert_eq!(core_res, exp_core_owned, "core HLS mismatch on {name}");
        assert_eq!(host_res, exp_host_owned, "host HLS mismatch on {name}");
    }
}

#[test]
fn watchdog_aborts_and_prints_case_id_on_simulated_hang() {
    const HANG_CASE_ID: &str = "fuzz/corpus/synthetic/simulated_infinite_loop.bin";
    if std::env::var_os("IRIS_FUZZ_HANG_CHILD").is_some() {
        let ids = Arc::new(vec![HANG_CASE_ID.to_owned()]);
        let watchdog = Watchdog::start(ids, 1, Duration::from_millis(120));
        watchdog.set_corpus_case(0);
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let start = Instant::now();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "watchdog_aborts_and_prints_case_id_on_simulated_hang",
            "--nocapture",
        ])
        .env("IRIS_FUZZ_HANG_CHILD", "1")
        .output()
        .expect("spawn child watchdog test process");

    assert!(
        !output.status.success(),
        "hanging child must exit with non-zero abort status, got {:?}",
        output.status
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "watchdog took too long to terminate hanging child: {:?}",
        start.elapsed()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(HANG_CASE_ID),
        "watchdog stderr did not report offending case id {HANG_CASE_ID:?}; stderr={stderr:?}"
    );
}
