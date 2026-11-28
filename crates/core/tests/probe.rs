//! Module probe integration tests.

use std::fs;

use walkdir::WalkDir;

/// Every committed malformed module must be rejected by the probe without a
/// panic. The probe parses and validates with the engine loader, so it returns
/// `Err` for exactly the modules the engine refuses.
#[test]
fn malformed_corpus_is_rejected_without_panic() {
    let mut count = 0;
    for entry in WalkDir::new("tests/malformed")
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        let bytes = fs::read(entry.path()).expect("read malformed fixture");
        assert!(
            wasmtiny::probe(&bytes).is_err(),
            "probe accepted malformed fixture: {}",
            entry.path().display()
        );
        count += 1;
    }
    assert!(count > 0, "malformed corpus must not be empty");
}
