//! Traceability matrix: every threat-model entry (`TM-xx` in
//! `docs/threat-model.md`) must map to at least one corpus fixture
//! manifest (`tests/corpus/*/manifest.txt`, `threat:` field) or fuzz
//! target declaration (`tests/fuzz/targets.txt`).
//!
//! Writes the matrix to `CARGO_TARGET_TMPDIR/traceability-matrix.md`
//! (Cargo's scratch dir for integration tests) and fails,
//! listing uncovered entries, when coverage is incomplete. This is the
//! CI gate required by the `sandbox-escape-testing` spec: a new
//! threat-model entry fails CI until a test exists.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

/// A coverage source: a corpus fixture or a fuzz target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Coverage {
    Fixture(String),
    FuzzTarget(String),
}

/// Parse the `key: value` lines of a fixture manifest.
pub fn parse_manifest(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn collect_fixture_coverage(corpus_dir: &Path, coverage: &mut BTreeMap<String, Vec<Coverage>>) {
    let Ok(entries) = fs::read_dir(corpus_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("manifest.txt");
        let Ok(text) = fs::read_to_string(&manifest) else {
            continue;
        };
        let fields = parse_manifest(&text);
        if let (Some(threat), Some(name)) = (fields.get("threat"), fields.get("name")) {
            coverage
                .entry(threat.clone())
                .or_default()
                .push(Coverage::Fixture(name.clone()));
        }
    }
}

fn collect_fuzz_coverage(targets_file: &Path, coverage: &mut BTreeMap<String, Vec<Coverage>>) {
    let Ok(text) = fs::read_to_string(targets_file) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Format: <target-name> <TM-xx> [<TM-yy> ...]
        let mut parts = line.split_whitespace();
        let Some(target) = parts.next() else { continue };
        for threat in parts {
            coverage
                .entry(threat.to_string())
                .or_default()
                .push(Coverage::FuzzTarget(target.to_string()));
        }
    }
}

#[test]
fn every_threat_model_entry_has_coverage() {
    let root = manifest_dir();
    let doc = fs::read_to_string(root.join("../../docs/threat-model.md"))
        .expect("docs/threat-model.md is the coverage source of truth");

    let entries = threat_entries(&doc);
    assert!(
        !entries.is_empty(),
        "no TM-xx entries parsed from ../../docs/threat-model.md"
    );

    let mut coverage: BTreeMap<String, Vec<Coverage>> = BTreeMap::new();
    collect_fixture_coverage(&root.join("tests/corpus"), &mut coverage);
    collect_fuzz_coverage(&root.join("tests/fuzz/targets.txt"), &mut coverage);

    // Build and emit the matrix regardless of pass/fail so it can be
    // inspected as a build artifact.
    let mut matrix =
        String::from("# Traceability Matrix\n\n| Threat | Title | Coverage |\n|---|---|---|\n");
    let mut uncovered = Vec::new();
    for (id, title) in &entries {
        let sources = coverage.get(id).cloned().unwrap_or_default();
        let cell = if sources.is_empty() {
            uncovered.push(id.clone());
            "**UNCOVERED**".to_string()
        } else {
            sources
                .iter()
                .map(|s| match s {
                    Coverage::Fixture(n) => format!("fixture `{n}`"),
                    Coverage::FuzzTarget(n) => format!("fuzz `{n}`"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(matrix, "| {id} | {title} | {cell} |");
    }

    // Cargo creates and guarantees CARGO_TARGET_TMPDIR for integration tests.
    let out_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("traceability-matrix.md");
    let _ = fs::write(&out_path, &matrix);

    assert!(
        uncovered.is_empty(),
        "threat-model entries without a corpus fixture or fuzz target: {}. \
         Matrix written to {}. Add a fixture manifest (tests/corpus/<name>/\
         manifest.txt) or a fuzz target declaration (tests/fuzz/targets.txt).",
        uncovered.join(", "),
        out_path.display()
    );
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Extract `TM-xx` entries (with titles) from the threat model doc.
fn threat_entries(doc: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in doc.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("### TM-") {
            let id = format!("TM-{}", rest.split(':').next().unwrap_or(rest).trim());
            let title = rest.split_once(':').map(|(_, t)| t.trim().to_string());
            if let Some(title) = title {
                out.push((id, title));
            }
        }
    }
    out
}
