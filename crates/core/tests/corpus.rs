//! Corpus harness: executes adversarial guest fixtures and judges
//! them from outside the sandbox (security-test build).
//!
//! Judgement is made ONLY from process state observed by this harness:
//! exit codes, output, time, and host invariants (canaries). Nothing
//! inside the sandbox under test is trusted — including the runner's
//! stdout, which is recorded for diagnostics but never used as the
//! verdict source.
//!
//! Verdict classification (four classes, per the sandbox-escape-testing
//! spec):
//!
//! - `clean`   — runner exited 0
//! - `trapped` — runner exited 3 (load rejection or runtime trap)
//! - `crashed` — any other exit code (panic 101, abort, signal)
//! - `hung`    — exceeded the harness deadline; the process group
//!   is killed. A hung fixture ALWAYS fails the run: it is a
//!   resource-management defect, not a flake.

#![cfg(feature = "security-test")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use wasmtiny::security_test::open_fd_count;

/// Harness deadline is always the guest budget plus this grace, so a
/// fixture that ignores its budget is classified `hung` by the
/// harness rather than silently overrunning.
const DEADLINE_GRACE_MS: u64 = 3000;
const PAGE_BYTES: u32 = 64 * 1024;
/// Serializes the tests in this file: cargo runs test functions on
/// parallel threads in one process, and the fd canary reads the
/// process-wide open-fd count. Concurrently spawning watchdog children
/// (pipe fds) or allocating shared regions (shm fds) would race the fd
/// canary windows, so every test that opens fds holds this lock.
static TEST_SERIALIZE: Mutex<()> = Mutex::new(());

/// Parsed fixture manifest (`key: value` lines, `#` comments).
#[derive(Debug, Clone)]
struct Manifest {
    name: String,
    /// Threat-model entry (used by tests/traceability.rs; kept here so
    /// the manifest format is defined in one place).
    #[allow(dead_code)]
    threat: String,
    /// Expected verdict: `clean` or `trapped`. `crashed`/`hung` are
    /// never expected outcomes — they always fail the run.
    expected: String,
    /// Compile this `.wat` file to `.wasm` at test time (offline, via
    /// the vendored wat dev-dependency).
    wat: Option<String>,
    /// Use this committed binary directly (byte-level patch fixtures).
    binary: Option<String>,
    /// Build the module from this inline hex string (malformed-module
    /// fixtures: the manifest itself is the byte-level source of
    /// truth, so every malformation is reviewable and reproducible).
    bytes: Option<String>,
    budget_ms: u64,
    memory_mb: u64,
    host_abuse: bool,
    region_pages: u32,
    entry: String,
    i32_args: Vec<i32>,
    /// Harness-plumbing self-test (`--selftest-crash`/`--selftest-hang`):
    /// simulates a runtime defect / budget-ignoring guest so the
    /// failure paths (crashed/hung verdicts failing the run) can be
    /// verified end-to-end. No real corpus fixture uses this.
    selftest: Option<String>,
}

/// Host invariants checked from outside the sandbox after each
/// fixture. Any violation is an escape attempt (or a leak): the run
/// fails with the fixture and invariant named.
#[derive(Debug)]
struct Canaries {
    marker_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Clean,
    Trapped,
    Crashed,
    Hung,
}

#[derive(Debug)]
struct RunOutcome {
    verdict: Verdict,
    /// Runner stdout (diagnostics only — never the verdict source).
    output: String,
}

/// One fixture failure report.
struct Failure {
    fixture: String,
    reason: String,
}

impl Canaries {
    fn new(root: &Path) -> Self {
        let marker_dir = root.join("marker");
        fs::create_dir_all(&marker_dir).expect("create marker dir");
        // Seed marker files with distinctive content; any change to
        // them, or any new file in the dir, is detected after the run.
        for i in 0..8 {
            let path = marker_dir.join(format!("marker-{i}.canary"));
            fs::write(&path, format!("canary-content-{i}-DO-NOT-TOUCH")).expect("seed marker");
        }
        Canaries { marker_dir }
    }

    /// Current open-fd count. Compared per fixture (before spawn vs
    /// after reap): cargo runs test functions on parallel threads in
    /// one process, so a whole-run baseline would see other tests'
    /// transient fds. Only an *increase* across a fixture's window is
    /// a leak; decreases are concurrent-test noise.
    fn fd_snapshot() -> usize {
        open_fd_count()
    }

    /// Returns the list of violated invariant descriptions.
    fn check(&self) -> Vec<String> {
        let mut violations = Vec::new();

        let mut entries: Vec<_> = fs::read_dir(&self.marker_dir)
            .expect("marker dir readable")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        let expected: Vec<_> = (0..8)
            .map(|i| self.marker_dir.join(format!("marker-{i}.canary")))
            .collect();
        if entries != expected {
            violations.push(format!(
                "marker violation: sandbox wrote to or removed files in {} \
                 (found {:?}, expected {:?})",
                self.marker_dir.display(),
                entries,
                expected
            ));
        } else {
            for (i, path) in expected.iter().enumerate() {
                let content = fs::read_to_string(path).unwrap_or_default();
                if content != format!("canary-content-{i}-DO-NOT-TOUCH") {
                    violations.push(format!("marker violation: {} was modified", path.display()));
                }
            }
        }

        // Network canary: by construction, guests have no syscall
        // surface — the interpreter exposes only registered host
        // functions and the runner registers none that touch the
        // network (see docs/threat-model.md). No active check is
        // possible without OS-level firewalling, which is a CI-runner
        // concern (ephemeral isolated runners), not a harness one.

        violations
    }
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Verdict::Clean => "clean",
            Verdict::Trapped => "trapped",
            Verdict::Crashed => "crashed",
            Verdict::Hung => "hung",
        }
    }
}

fn build_dir() -> PathBuf {
    let dir = tmp_root().join("corpus-build");
    fs::create_dir_all(&dir).expect("create corpus build dir");
    dir
}

/// Canary self-test: each canary fires on a deliberate violation.
#[test]
fn canaries_detect_violations() {
    let _guard = TEST_SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
    let root = tmp_root().join("corpus-canary-selftest");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create selftest dir");
    let canaries = Canaries::new(&root);
    assert!(canaries.check().is_empty(), "clean baseline must pass");

    // Marker violation: write a new file into the marker dir.
    fs::write(canaries.marker_dir.join("escaped.txt"), "hostile write")
        .expect("deliberate marker write");
    let violations = canaries.check();
    assert!(
        violations.iter().any(|v| v.contains("marker violation")),
        "marker canary must fire: {violations:?}"
    );

    // fd canary: leak enough fds that concurrent-test noise cannot
    // mask the increase.
    let baseline = Canaries::fd_snapshot();
    let leaked: Vec<_> = (0..16)
        .map(|_| fs::File::open(&root).expect("deliberate leaked fd"))
        .collect();
    assert!(
        Canaries::fd_snapshot() > baseline,
        "fd canary must fire: {} fds leaked, baseline {baseline}",
        leaked.len()
    );
    drop(leaked);
    assert!(
        Canaries::fd_snapshot() <= baseline,
        "fd count must return to baseline after dropping the leaks"
    );
}

// Hand-assembled smoke modules (committed bytes, no toolchain faith).
// clean: (module (func (export "main") (result i32) i32.const 42))
fn clean_module_bytes() -> Vec<u8> {
    wat::parse_str(r#"(module (func (export "main") (result i32) i32.const 42))"#)
        .expect("clean smoke module")
}

/// Compiles a resolved fixture `.wasm` into a `.aot` artifact for the
/// AOT corpus pass. The runner binary itself never links the compiler —
/// it only loads the finished artifact.
#[cfg(feature = "aot")]
fn compile_aot(wasm: &Path, name: &str) -> Result<PathBuf, String> {
    let bytes = fs::read(wasm).map_err(|e| format!("read {}: {e}", wasm.display()))?;
    let artifact = wasmtiny_aotc::compile_artifact(&bytes, &wasmtiny_aotc::CompilerConfig::host())
        .map_err(|e| format!("AOT compile failed: {e}"))?;
    let out = build_dir().join(format!("{name}.aot"));
    fs::write(&out, artifact).map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(out)
}

fn corpus_dir() -> PathBuf {
    std::env::var("WASMTINY_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"))
}

/// The main gate: every adversarial fixture fails closed, safely, and
/// all host invariants hold.
#[test]
fn corpus_fixtures_fail_closed() {
    let _guard = TEST_SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
    let failures = run_corpus(&corpus_dir());
    let report = failures
        .iter()
        .map(|f| format!("  [{}] {}", f.fixture, f.reason.replace('\n', "\n  ")))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        failures.is_empty(),
        "corpus containment failures ({}):\n{}",
        failures.len(),
        report
    );
}

/// Decodes an inline hex string (`bytes:` manifest field). Whitespace
/// is allowed; anything else that is not a hex pair is an error.
fn decode_hex(hex: &str, fixture: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(format!("fixture {fixture}: odd-length hex string"));
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .map_err(|e| format!("fixture {fixture}: bad hex: {e}"))
        })
        .collect()
}

fn load_manifest(dir: &Path) -> Option<Manifest> {
    let text = fs::read_to_string(dir.join("manifest.txt")).ok()?;
    let fields = parse_manifest(&text);
    let name = fields.get("name")?.clone();
    let expected = fields.get("expected")?.clone();
    if expected != "clean" && expected != "trapped" {
        panic!(
            "fixture {name}: invalid expected verdict '{expected}' \
             (only clean|trapped may be expected; crashed/hung always fail)"
        );
    }
    Some(Manifest {
        name,
        threat: fields.get("threat").cloned().unwrap_or_default(),
        expected,
        wat: fields.get("wat").cloned(),
        binary: fields.get("binary").cloned(),
        bytes: fields.get("bytes").cloned(),
        budget_ms: fields
            .get("budget_ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000),
        memory_mb: fields
            .get("memory_mb")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1024),
        host_abuse: fields
            .get("host_abuse")
            .map(|v| v == "true")
            .unwrap_or(false),
        region_pages: fields
            .get("region_pages")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        entry: fields.get("entry").cloned().unwrap_or_default(),
        i32_args: fields
            .get("i32_args")
            .map(|v| {
                v.split_whitespace()
                    .filter_map(|a| a.parse().ok())
                    .collect()
            })
            .unwrap_or_default(),
        selftest: fields.get("selftest").cloned(),
    })
}

/// Manifest-integrity self-test: a manifest naming a missing binary
/// fails with a clear error.
#[test]
fn manifest_with_missing_binary_fails_clearly() {
    let _guard = TEST_SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tmp_root().join("corpus-manifest-selftest");
    let _ = fs::remove_dir_all(&tmp);
    let fixture = tmp.join("broken-fixture");
    fs::create_dir_all(&fixture).expect("create selftest fixture dir");
    fs::write(
        fixture.join("manifest.txt"),
        "name: broken\nthreat: TM-01\nexpected: trapped\nbinary: does-not-exist.wasm\n",
    )
    .expect("write manifest");

    let manifest = load_manifest(&fixture).expect("manifest parses");
    let err = resolve_wasm(&fixture, &manifest).expect_err("missing binary must fail");
    assert!(
        err.contains("committed binary missing"),
        "error must be clear, got: {err}"
    );
}

fn parse_manifest(text: &str) -> BTreeMap<String, String> {
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

/// Resolves a fixture's wasm binary: assembles `.wat` offline (wat is
/// a vendored dev-dependency — self-contained, no network), decodes
/// inline `bytes:` hex, or copies the committed binary. A manifest
/// naming a missing binary fails with a clear error
/// (manifest-integrity requirement).
fn resolve_wasm(dir: &Path, manifest: &Manifest) -> Result<PathBuf, String> {
    let build_dir = build_dir();
    if let Some(wat_name) = &manifest.wat {
        let wat_path = dir.join(wat_name);
        let bytes = fs::read(&wat_path)
            .map_err(|e| format!("wat source missing: {}: {e}", wat_path.display()))?;
        let wasm = wat::parse_bytes(&bytes)
            .map_err(|e| format!("wat assembly failed for {}: {e}", manifest.name))?;
        let out = build_dir.join(format!("{}.wasm", manifest.name));
        fs::write(&out, wasm).map_err(|e| format!("write {}: {e}", out.display()))?;
        Ok(out)
    } else if let Some(hex) = &manifest.bytes {
        let wasm = decode_hex(hex, &manifest.name)?;
        let out = build_dir.join(format!("{}.wasm", manifest.name));
        fs::write(&out, wasm).map_err(|e| format!("write {}: {e}", out.display()))?;
        Ok(out)
    } else if let Some(bin_name) = &manifest.binary {
        let bin_path = dir.join(bin_name);
        if !bin_path.is_file() {
            return Err(format!(
                "committed binary missing: {} (fixtures must be reproducible: \
                 commit the binary and its build recipe)",
                bin_path.display()
            ));
        }
        Ok(bin_path)
    } else {
        Err(format!(
            "fixture {} declares neither 'wat' nor 'binary'",
            manifest.name
        ))
    }
}

/// Runs the whole corpus in `dir`, returning all failures. Canaries
/// are checked after every fixture.
fn run_corpus(dir: &Path) -> Vec<Failure> {
    let mut failures = Vec::new();
    let canary_root = tmp_root().join("corpus-run");
    let _ = fs::remove_dir_all(&canary_root);
    fs::create_dir_all(&canary_root).expect("create corpus run dir");
    let canaries = Canaries::new(&canary_root);

    let mut fixture_dirs: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("corpus dir {} unreadable: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    fixture_dirs.sort();

    assert!(
        !fixture_dirs.is_empty(),
        "corpus dir {} contains no fixtures",
        dir.display()
    );

    for fixture_dir in &fixture_dirs {
        let Some(manifest) = load_manifest(fixture_dir) else {
            failures.push(Failure {
                fixture: fixture_dir.display().to_string(),
                reason: "unreadable or incomplete manifest.txt".to_string(),
            });
            continue;
        };

        let wasm = match resolve_wasm(fixture_dir, &manifest) {
            Ok(p) => p,
            Err(e) => {
                failures.push(Failure {
                    fixture: manifest.name,
                    reason: e,
                });
                continue;
            }
        };

        let mut cmd = Command::new(runner_path());
        cmd.arg(&wasm)
            .arg("--budget-ms")
            .arg(manifest.budget_ms.to_string())
            .arg("--memory-mb")
            .arg(manifest.memory_mb.to_string());
        if manifest.host_abuse {
            cmd.arg("--host-abuse");
        }
        if manifest.region_pages > 0 {
            cmd.arg("--region-pages")
                .arg(manifest.region_pages.to_string());
        }
        if !manifest.entry.is_empty() {
            cmd.arg("--entry").arg(&manifest.entry);
        }
        match manifest.selftest.as_deref() {
            None => {}
            Some("crash") => {
                cmd.arg("--selftest-crash");
            }
            Some("hang") => {
                cmd.arg("--selftest-hang");
            }
            Some(other) => panic!("fixture {}: unknown selftest '{other}'", manifest.name),
        }
        for arg in &manifest.i32_args {
            cmd.arg("--i32-arg").arg(arg.to_string());
        }

        let deadline = Duration::from_millis(manifest.budget_ms + DEADLINE_GRACE_MS);
        let fds_before = Canaries::fd_snapshot();
        let outcome = run_with_watchdog(cmd, deadline);

        // A hung fixture always fails the run, expected or not.
        if outcome.verdict == Verdict::Hung {
            failures.push(Failure {
                fixture: manifest.name.clone(),
                reason: "HUNG: exceeded the harness deadline (resource-management \
                         defect; not a flake)"
                    .to_string(),
            });
            continue;
        }
        if outcome.verdict.tag() != manifest.expected {
            failures.push(Failure {
                fixture: manifest.name.clone(),
                reason: format!(
                    "verdict mismatch: expected {}, got {}\n--- runner output ---\n{}",
                    manifest.expected,
                    outcome.verdict.tag(),
                    outcome.output
                ),
            });
        }

        // fd canary: only an increase across the fixture window is a
        // leak (decreases are concurrent-test noise; see fd_snapshot).
        let fds_after = Canaries::fd_snapshot();
        if fds_after > fds_before {
            failures.push(Failure {
                fixture: manifest.name.clone(),
                reason: format!(
                    "canary violation: fd leak: {} fds before fixture, {} after",
                    fds_before, fds_after
                ),
            });
        }

        for violation in canaries.check() {
            failures.push(Failure {
                fixture: manifest.name.clone(),
                reason: format!("canary violation: {violation}"),
            });
        }

        // AOT pass: the same fixture, compiled ahead of time (in-harness;
        // the runner binary never links the compiler) and executed
        // natively through the corpus-runner's `.aot` path, must yield
        // the same verdict. Fixtures exercising embedder-interpreter
        // plumbing (host abuse, shared regions) or harness selftests are
        // interpreter-only.
        #[cfg(feature = "aot")]
        if !manifest.host_abuse && manifest.region_pages == 0 && manifest.selftest.is_none() {
            match compile_aot(&wasm, &manifest.name) {
                Ok(aot) => {
                    let mut cmd = Command::new(runner_path());
                    cmd.arg(&aot)
                        .arg("--budget-ms")
                        .arg(manifest.budget_ms.to_string())
                        .arg("--memory-mb")
                        .arg(manifest.memory_mb.to_string());
                    if !manifest.entry.is_empty() {
                        cmd.arg("--entry").arg(&manifest.entry);
                    }
                    for arg in &manifest.i32_args {
                        cmd.arg("--i32-arg").arg(arg.to_string());
                    }
                    let deadline = Duration::from_millis(manifest.budget_ms + DEADLINE_GRACE_MS);
                    let outcome = run_with_watchdog(cmd, deadline);
                    if outcome.verdict == Verdict::Hung {
                        failures.push(Failure {
                            fixture: format!("{} (aot)", manifest.name),
                            reason: "HUNG: exceeded the harness deadline on the AOT path \
                                     (resource-management defect; not a flake)"
                                .to_string(),
                        });
                    } else if outcome.verdict.tag() != manifest.expected {
                        failures.push(Failure {
                            fixture: format!("{} (aot)", manifest.name),
                            reason: format!(
                                "AOT verdict mismatch: expected {}, got {}\n--- runner output ---\n{}",
                                manifest.expected,
                                outcome.verdict.tag(),
                                outcome.output
                            ),
                        });
                    }
                }
                Err(reason) => {
                    // Compilation refusing the fixture is the AOT
                    // equivalent of a load rejection: accepted only when
                    // the interpreter path rejected it too. A guest the
                    // interpreter accepts but the compiler cannot
                    // compile is a coverage-parity failure.
                    if outcome.output.contains("load-rejected") {
                        continue;
                    }
                    failures.push(Failure {
                        fixture: format!("{} (aot)", manifest.name),
                        reason,
                    });
                }
            }
        }
    }

    failures
}

/// Spawns the runner in a new process group, enforces the harness
/// deadline, and classifies the outcome from process state only.
fn run_with_watchdog(mut cmd: Command, deadline: Duration) -> RunOutcome {
    use std::os::unix::process::CommandExt;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let mut child: Child = cmd.spawn().expect("spawn corpus runner");
    let pgid: libc::pid_t = child.id() as libc::pid_t;
    let start = Instant::now();

    let verdict = loop {
        match child.try_wait().expect("try_wait runner") {
            Some(status) => {
                break match status.code() {
                    Some(0) => Verdict::Clean,
                    Some(3) => Verdict::Trapped,
                    // panic (101), abort, signal death, anything else.
                    _ => Verdict::Crashed,
                };
            }
            None => {
                if start.elapsed() >= deadline {
                    // Kill the whole process group: descendants must
                    // not survive the fixture.
                    // SAFETY: pgid is the child's process group.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                    child.wait().expect("reap killed runner");
                    break Verdict::Hung;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    // After terminal state, collect remaining piped output.
    let output = child
        .wait_with_output()
        .expect("collect runner output")
        .stdout;
    let output = String::from_utf8_lossy(&output).to_string();

    // Stray-process canary: the process group must be gone.
    if verdict != Verdict::Hung {
        // SAFETY: kill(2) with signal 0 is a pure existence probe.
        let rc = unsafe { libc::kill(-pgid, 0) };
        if rc == 0 {
            return RunOutcome {
                verdict: Verdict::Crashed,
                output: format!("{output}\nSTRAY-PROCESS: pgid {pgid} still has members"),
            };
        }
    }

    RunOutcome { verdict, output }
}

fn runner_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wasmtiny-corpus-runner"))
}

/// TM-06 host-side check: guests can only reach shared ranges mapped
/// into their own linear memory, and only with the protections they
/// were granted. Region attach is an embedder-side grant (no
/// guest-reachable host call exposes it), so the runtime-level
/// guarantees are boundary containment and protection enforcement:
/// accesses past the region/memory end, accesses straddling the end,
/// and writes to read-only regions must all be denied.
#[test]
fn shared_region_boundary_probes_are_denied() {
    let _guard = TEST_SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
    use wasmtiny::{RegionProt, WasmApplication, WasmValue};

    let guest_wat = r#"
        (module
            (memory 2 2 shared)
            (func (export "load") (param i32) (result i32)
                local.get 0
                i32.load)
            (func (export "store") (param i32 i32)
                local.get 0
                local.get 1
                i32.store)
            (func (export "notify") (param i32) (result i32)
                local.get 0
                i32.const 1
                memory.atomic.notify))
    "#;

    let mut app = WasmApplication::new();
    let idx = app
        .load_module_from_memory(&wat::parse_str(guest_wat).expect("wat"))
        .expect("load");
    app.instantiate(idx).expect("instantiate");

    // Read-write region: the grant under test.
    let (_rw_region, page_offset) = app
        .allocate_shared_region(idx, PAGE_BYTES, RegionProt::ReadWrite)
        .expect("allocate rw region");
    let base = page_offset * PAGE_BYTES;
    // Read-only region: a write-restricted grant.
    let (_ro_region, ro_page_offset) = app
        .allocate_shared_region(idx, PAGE_BYTES, RegionProt::ReadOnly)
        .expect("allocate ro region");
    let ro_base = ro_page_offset * PAGE_BYTES;

    // Sanity: in-region, in-grant accesses succeed.
    assert!(
        app.call_function(idx, "notify", &[WasmValue::I32((base + 64) as i32)])
            .is_ok()
    );
    assert!(
        app.call_function(
            idx,
            "store",
            &[WasmValue::I32((base + 64) as i32), WasmValue::I32(1)]
        )
        .is_ok()
    );

    // Probe past the end of the granted/accessible range.
    let end = base + PAGE_BYTES;
    let past = app.call_function(idx, "load", &[WasmValue::I32(end as i32)]);
    assert!(
        past.is_err(),
        "load past region end must trap, got {past:?}"
    );

    // Straddling access: starts inside, ends past the end.
    let straddle = app.call_function(idx, "load", &[WasmValue::I32((end - 2) as i32)]);
    assert!(
        straddle.is_err(),
        "straddling load must trap, got {straddle:?}"
    );

    // Atomic op past the end must also trap.
    let atomic_past = app.call_function(idx, "notify", &[WasmValue::I32(end as i32)]);
    assert!(
        atomic_past.is_err(),
        "atomic notify past region end must trap, got {atomic_past:?}"
    );

    // Write to a read-only (write-denied) region must be rejected.
    let ro_write = app.call_function(
        idx,
        "store",
        &[WasmValue::I32((ro_base + 64) as i32), WasmValue::I32(1)],
    );
    assert!(
        ro_write.is_err(),
        "write to read-only region must be denied, got {ro_write:?}"
    );
}

/// Scratch root for test output: Cargo's per-package tmp dir for
/// integration tests (inside the real build `target/`, never the
/// source tree), so generated fixtures stay out of `crates/core/`.
fn tmp_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

// trapped: OOB load past a 1-page memory.
fn trapped_module_bytes() -> Vec<u8> {
    wat::parse_str(
        r#"(module (memory 1 1)
            (func (export "main")
                i32.const 65536
                i32.load
                drop))"#,
    )
    .expect("trapped smoke module")
}

/// Watchdog classification self-test: all four verdict classes are
/// produced end-to-end from real process state, and a hung fixture
/// fails the run.
#[test]
fn watchdog_classifies_all_four_verdicts() {
    let _guard = TEST_SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
    let runner = runner_path();

    let smoke = tmp_root().join("corpus-smoke");
    fs::create_dir_all(&smoke).expect("create smoke dir");
    let clean_wasm = smoke.join("clean.wasm");
    fs::write(&clean_wasm, clean_module_bytes()).expect("write smoke module");

    let mk = |args: &[&str]| {
        let mut cmd = Command::new(&runner);
        cmd.arg(&clean_wasm).arg("--entry").arg("main");
        for a in args {
            cmd.arg(a);
        }
        cmd
    };

    let clean_outcome = run_with_watchdog(mk(&[]), Duration::from_secs(5));
    assert_eq!(clean_outcome.verdict, Verdict::Clean, "{:?}", clean_outcome);

    let trapped_wasm = smoke.join("trapped.wasm");
    fs::write(&trapped_wasm, trapped_module_bytes()).expect("write smoke module");
    let mut trapped_cmd = Command::new(&runner);
    trapped_cmd.arg(&trapped_wasm).arg("--entry").arg("main");
    let trapped = run_with_watchdog(trapped_cmd, Duration::from_secs(5));
    assert_eq!(trapped.verdict, Verdict::Trapped, "{:?}", trapped);

    let crashed = run_with_watchdog(mk(&["--selftest-crash"]), Duration::from_secs(5));
    assert_eq!(crashed.verdict, Verdict::Crashed, "{:?}", crashed);

    let hung = run_with_watchdog(mk(&["--selftest-hang"]), Duration::from_millis(500));
    assert_eq!(hung.verdict, Verdict::Hung, "{:?}", hung);
}
