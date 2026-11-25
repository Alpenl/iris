//! Seeded mutation regression for host-facing parsers. This is bounded mutation
//! fuzzing, not coverage-guided fuzzing or a substitute for sanitizer campaigns.
use iris_api::v2::{HostCommand, MESSAGE_LIMIT, Manifest, stream::ManifestFormat, task::Task};
use iris_core::plugin_v2::{
    package::{Package, Provenance},
    stream::manifest,
    validation,
};
use iris_plugin_sdk::json::decode_exact;
use std::{path::PathBuf, time::Instant};

fn exercise(input: &[u8]) {
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = iris_core::plugin_v2::network::normalize_url(
            text,
            iris_core::plugin_v2::network::TransportKind::Http,
        );
        let _ = iris_core::plugin_v2::package::validate_path(text);
        for size in [None, Some(0), Some(1024), Some(u64::MAX)] {
            if let Ok(range) =
                iris_core::plugin_v2::stream::range::Range::parse(Some(text.trim()), size)
            {
                if let Some(len) = range.len {
                    assert!(len > 0);
                    assert!(range.offset.checked_add(len - 1).is_some());
                    if let Some(size) = size {
                        assert!(range.offset + len - 1 < size);
                    }
                }
                let _ = range.content_range();
            }
        }
    }
    if let Ok(command) = decode_exact::<HostCommand>(input, MESSAGE_LIMIT) {
        let _ = validation::command(&command);
    }
    let _ = decode_exact::<Task>(input, MESSAGE_LIMIT);
    if let Ok(manifest) = decode_exact::<Manifest>(input, MESSAGE_LIMIT) {
        let _ = manifest.validate();
    }
    let _ = Package::from_zip(input, Provenance::User);
    for format in [
        ManifestFormat::VirtualHls,
        ManifestFormat::VirtualDash,
        ManifestFormat::RestrictedHls,
    ] {
        if let Ok(value) = manifest::compile(format, input, &|id, _| {
            Ok(format!("http://127.0.0.1:1234/{id}"))
        }) {
            for document in value.documents {
                assert!(document.bytes.len() <= manifest::OUTPUT_LIMIT);
                std::str::from_utf8(&document.bytes).unwrap();
            }
        }
    }
}

#[test]
fn seeded_plugin_parser_mutations() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/plugin-v2");
    // Valid seeds must actually reach the renderers; malformed-input-only
    // mutation campaigns would miss most conversion logic.
    for (name, format) in [
        ("virtual-hls.json", ManifestFormat::VirtualHls),
        ("virtual-dash.json", ManifestFormat::VirtualDash),
        ("restricted-hls.json", ManifestFormat::RestrictedHls),
    ] {
        manifest::compile(
            format,
            &std::fs::read(corpus.join(name)).unwrap(),
            &|id, _| Ok(format!("http://127.0.0.1:1234/{id}")),
        )
        .expect("valid renderer seed");
    }
    Package::from_zip(
        &std::fs::read(corpus.join("theme.irisplugin")).unwrap(),
        Provenance::User,
    )
    .expect("valid package seed");
    let mut files: Vec<_> = std::fs::read_dir(corpus)
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect();
    files.sort();
    let seeds: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    for seed in &seeds {
        exercise(seed);
    }
    let iterations = std::env::var("IRIS_FUZZ_ITERATIONS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(2000);
    let initial = std::env::var("IRIS_FUZZ_SEED")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(0x495249535632);
    assert!(initial != 0 && iterations <= 20_000_000);
    let mut random = initial;
    let start = Instant::now();
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random as usize
    };
    for index in 0..iterations {
        let mut input = seeds[next() % seeds.len()].clone();
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
                    let piece = seeds[next() % seeds.len()].as_slice();
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
    println!(
        "mutation receipt: seed={initial} iterations={iterations} elapsed_ms={}",
        start.elapsed().as_millis()
    );
}
