//! AOT-path fuzz target coverage: the artifact loader/verifier and native
//! dispatcher must survive the adversarial corpus (and mutated/tampered
//! artifacts) without panicking. Enabled only with `security-test` + `aot`.

#![cfg(all(feature = "security-test", feature = "aot"))]

use std::fs;

use wasmtiny::security_test::{fuzz_execute_aot, fuzz_load_aot};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

#[test]
fn aot_fuzz_targets_survive_tampered_artifacts() {
    let some = fixture_artifacts();
    assert!(!some.is_empty(), "expected at least one compilable fixture");
    for artifact in some {
        // Flip bytes across the header and code; every refusal must be clean.
        for &i in &[0usize, 3, 40, artifact.len() / 2, artifact.len() - 1] {
            let mut copy = artifact.clone();
            copy[i] ^= 0xFF;
            fuzz_load_aot(&copy);
        }
        // Truncations must also be clean.
        for len in [0usize, artifact.len() / 2, artifact.len() - 1] {
            fuzz_load_aot(&artifact[..len]);
        }
    }
}

#[test]
fn aot_fuzz_targets_survive_the_corpus() {
    for artifact in fixture_artifacts() {
        fuzz_load_aot(&artifact);
        fuzz_execute_aot(&artifact);
    }
}

fn fixture_artifacts() -> Vec<Vec<u8>> {
    let mut artifacts = Vec::new();
    for entry in fs::read_dir("tests/corpus").expect("corpus dir is present") {
        let name = entry.expect("fixture entry").file_name();
        let name = name.to_string_lossy().to_string();
        // `exhaust-*` fixtures are designed to run forever (or exhaust a
        // resource); the subprocess harness classifies those with a budget
        // timer, but they must not run in-process here.
        if name.starts_with("exhaust-") {
            continue;
        }
        let wat_path = format!("tests/corpus/{name}/fixture.wat");
        let Ok(source) = fs::read_to_string(&wat_path) else {
            continue;
        };
        let Ok(wasm) = wat::parse_str(&source) else {
            continue;
        };
        let Ok(artifact) = compile_artifact(&wasm, &CompilerConfig::host()) else {
            // Rejected modules are fine; the loader refusing is the outcome.
            continue;
        };
        artifacts.push(artifact);
    }
    artifacts
}
