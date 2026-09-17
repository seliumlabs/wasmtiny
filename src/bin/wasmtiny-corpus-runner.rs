//! Corpus fixture runner (security-test build).
//!
//! Executes one adversarial guest binary and reports a verdict via the
//! `VERDICT:` protocol described in `src/security_test.rs`. Judgement
//! of escape attempts is made by the harness (tests/corpus.rs) from
//! process state; this binary only runs the guest and reports what the
//! runtime did with it.

use std::{path::PathBuf, process::exit};

use wasmtiny::security_test::FixtureOptions;

const USAGE: &str = "usage: wasmtiny-corpus-runner <module.wasm> [--budget-ms N]
       [--memory-mb N] [--entry NAME] [--i32-arg N]... [--host-abuse]
       [--region-pages N] [--selftest-crash] [--selftest-hang]";

fn main() {
    let mut module: Option<PathBuf> = None;
    let mut budget_ms: u64 = 2000;
    let mut memory_mb: u64 = 1024;
    let mut opts = FixtureOptions::default();
    let mut selftest_crash = false;
    let mut selftest_hang = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next().unwrap_or_else(|| {
                eprintln!("{USAGE}");
                exit(2);
            })
        };
        match arg.as_str() {
            "--budget-ms" => {
                budget_ms = value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                })
            }
            "--memory-mb" => {
                memory_mb = value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                })
            }
            "--entry" => opts.entry = value(),
            "--i32-arg" => {
                let v: i32 = value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                });
                opts.i32_args.push(v);
            }
            "--host-abuse" => opts.host_abuse = true,
            "--region-pages" => {
                opts.region_pages = value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                })
            }
            // Harness-plumbing self-tests: they exist so the watchdog
            // classification can be verified end-to-end. No real
            // fixture uses them.
            "--selftest-crash" => selftest_crash = true,
            "--selftest-hang" => selftest_hang = true,
            other if other.starts_with('-') => {
                eprintln!("unknown flag: {other}\n{USAGE}");
                exit(2);
            }
            other => {
                if module.is_some() {
                    eprintln!("{USAGE}");
                    exit(2);
                }
                module = Some(PathBuf::from(other));
            }
        }
    }

    let module = module.unwrap_or_else(|| {
        eprintln!("{USAGE}");
        exit(2);
    });

    if let Err(e) = wasmtiny::security_test::apply_resource_caps(memory_mb) {
        eprintln!("failed to apply resource caps: {e}");
        exit(2);
    }

    if selftest_crash {
        // Simulates a runtime defect: an uncaught panic. The harness
        // must classify the exit code as `crashed`.
        panic!("selftest: deliberate crash");
    }
    if selftest_hang {
        // Simulates total enforcement failure: the guest ignores its
        // budget AND the budget timer never fires. Only the harness
        // deadline can catch this — it must classify it as `hung`.
        // (The budget timer is deliberately NOT installed for this
        // selftest; with it installed, a hanging guest is correctly
        // terminated at the budget and reports `trapped`.)
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    wasmtiny::security_test::install_budget_timer(budget_ms);

    exit(wasmtiny::security_test::run_fixture(&module, &opts));
}
