//! Fuzz burst tests (security-test build): short, deterministic fuzz
//! bursts over the trust-boundary entry points, plus end-to-end
//! verification that crash discovery works (a finding fails the run,
//! the failing input is preserved, and removing it goes green).
//!
//! Seeds are assembled at test time from the corpus fixtures and the
//! committed malformed binaries — offline, deterministic, honoring
//! the self-contained test constraint (no external fuzzer binaries).

#![cfg(feature = "security-test")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Fixed burst parameters: reproducible by construction.
const ITERATIONS: u32 = 400;
const PRNG_SEED: u64 = 0x0C0F_FEE0_0D15_EA5E;

/// Assembles the seed corpus: corpus fixture modules (valid-ish and
/// loadable) plus every committed malformed binary. Deterministic and
/// offline.
fn assemble_seeds(out: &Path) {
    fs::create_dir_all(out).expect("create fuzz seed dir");
    let mut count = 0usize;

    // Corpus fixture .wat files, assembled via the vendored wat crate.
    let corpus = manifest_root().join("tests/corpus");
    let mut dirs: Vec<_> = fs::read_dir(&corpus)
        .expect("corpus dir readable")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let wat_path = dir.join("fixture.wat");
        let Ok(text) = fs::read_to_string(&wat_path) else {
            continue;
        };
        if let Ok(wasm) = wat::parse_str(&text) {
            count += 1;
            fs::write(out.join(format!("seed-corpus-{count:03}.wasm")), wasm).expect("write seed");
        }
    }

    // Committed malformed binaries (already part of the repo's test
    // assets; no new unaccounted binaries).
    let malformed = manifest_root().join("tests/malformed");
    let mut files: Vec<_> = fs::read_dir(&malformed)
        .expect("malformed dir readable")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    for file in files {
        count += 1;
        let _ = fs::copy(&file, out.join(format!("seed-malformed-{count:03}.wasm")));
    }

    // AOT seeds: corpus fixtures compiled ahead of time (in-test, via the
    // compiler's library — the fuzz binary itself never links it), so the
    // artifact loader/verifier and native-dispatch targets get valid
    // seeds to mutate. Fixtures the compiler rejects (malformed,
    // unsupported) are skipped — their `.wasm` seeds already cover the
    // interpreter loader targets.
    #[cfg(feature = "aot")]
    {
        let corpus = manifest_root().join("tests/corpus");
        let mut dirs: Vec<_> = fs::read_dir(&corpus)
            .expect("corpus dir readable")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let wat_path = dir.join("fixture.wat");
            let Ok(text) = fs::read_to_string(&wat_path) else {
                continue;
            };
            let Ok(wasm) = wat::parse_str(&text) else {
                continue;
            };
            if let Ok(artifact) =
                wasmtiny_aotc::compile_artifact(&wasm, &wasmtiny_aotc::CompilerConfig::host())
            {
                count += 1;
                fs::write(out.join(format!("seed-aot-{count:03}.aot")), artifact)
                    .expect("write aot seed");
            }
        }
    }

    assert!(count > 0, "seed corpus must not be empty");
}

fn fuzz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wasmtiny-fuzz"))
}

/// The CI burst: a fixed-seed mutation run over the seed corpus must
/// complete with no findings.
#[test]
fn fuzz_burst_is_clean() {
    let seeds = manifest_root().join("target/fuzz-seeds-clean");
    let _ = fs::remove_dir_all(&seeds);
    assemble_seeds(&seeds);

    let output = run_burst(&seeds, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fuzz burst found a crash (or failed to run):\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.starts_with("fuzz burst clean"),
        "unexpected burst output: {stdout}"
    );
}

/// End-to-end crash-discovery verification: a finding (simulated
/// panicking input via --inject-panic-at) must fail the run, preserve
/// the failing input as an artifact, and a clean rerun must go green.
#[test]
fn fuzz_crash_discovery_works_end_to_end() {
    let seeds = manifest_root().join("target/fuzz-seeds-crash");
    let _ = fs::remove_dir_all(&seeds);
    assemble_seeds(&seeds);

    // Sweep artifacts from any earlier run.
    for entry in fs::read_dir(manifest_root().join("target"))
        .expect("target dir readable")
        .map_while(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("fuzz-crash-"))
    {
        let _ = fs::remove_file(entry.path());
    }

    // 1. Injected finding: run fails and preserves the input.
    let output = run_burst(&seeds, &["--inject-panic-at", "50"]);
    assert!(
        !output.status.success(),
        "a finding must fail the run, got success"
    );
    let crash_artifacts: Vec<_> = fs::read_dir(manifest_root().join("target"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("fuzz-crash-"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !crash_artifacts.is_empty(),
        "the failing input must be preserved as a crash artifact"
    );
    let preserved = fs::read(&crash_artifacts[0]).expect("artifact readable");
    assert!(!preserved.is_empty(), "artifact must contain the input");

    // 2. Remove the finding, rerun: green.
    for artifact in &crash_artifacts {
        let _ = fs::remove_file(artifact);
    }
    let clean = run_burst(&seeds, &[]);
    assert!(
        clean.status.success(),
        "clean rerun must go green: {}",
        String::from_utf8_lossy(&clean.stdout)
    );
}

fn manifest_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_burst(seeds: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(fuzz_bin());
    cmd.arg("--seeds")
        .arg(seeds)
        .arg("--iterations")
        .arg(ITERATIONS.to_string())
        .arg("--seed")
        .arg(PRNG_SEED.to_string())
        .arg("--crash-dir")
        .arg(manifest_root().join("target"));
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.output().expect("run fuzz burst")
}
