use walkdir::WalkDir;
use wasmtiny::WasmApplication;

#[test]
fn malformed_wasms_should_fail() {
    let mut app = WasmApplication::new();

    WalkDir::new("tests/malformed")
        .into_iter()
        .filter_map(|e| e.ok())
        .for_each(|entry| {
            let result = app.load_module_from_file(entry.path());
            assert!(
                result.is_err(),
                "Expected failure for malformed file: {}",
                entry.path().display()
            );
        });
}
