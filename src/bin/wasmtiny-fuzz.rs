//! Trust-boundary fuzz runner (security-test build).
//!
//! Runs short, deterministic fuzz bursts over the fuzz entry points in
//! `wasmtiny::security_test` (module loader/validator, interpreter
//! dispatch, shared-region API) using an in-repo mutation loop with a
//! fixed PRNG seed — honoring the self-contained test constraint (no
//! external fuzzer binaries). The entry points are structured so a
//! real coverage-guided fuzzer can drive them unchanged later.
//!
//! Exit codes: 0 = burst completed with no findings; 1 = finding
//! (failing input written next to the report on stdout); 2 = usage.

use std::{path::PathBuf, process::exit};

use wasmtiny::security_test::{Prng, fuzz_execute, fuzz_load, fuzz_shared_region, mutate};

const USAGE: &str = "usage: wasmtiny-fuzz --seeds <path>... [--iterations N] [--seed N]
       [--crash-dir DIR] [--inject-panic-at N]";

struct Args {
    seeds: Vec<PathBuf>,
    iterations: usize,
    seed: u64,
    crash_dir: PathBuf,
    inject_panic_at: Option<usize>,
}

fn load_seeds(paths: &[PathBuf]) -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .unwrap_or_else(|e| {
                    eprintln!("cannot read seed dir {}: {e}", path.display());
                    exit(2);
                })
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            entries.sort();
            for entry in entries {
                match std::fs::read(&entry) {
                    Ok(bytes) => seeds.push(bytes),
                    Err(e) => {
                        eprintln!("cannot read seed {}: {e}", entry.display());
                        exit(2);
                    }
                }
            }
        } else {
            match std::fs::read(path) {
                Ok(bytes) => seeds.push(bytes),
                Err(e) => {
                    eprintln!("cannot read seed {}: {e}", path.display());
                    exit(2);
                }
            }
        }
    }
    seeds
}

fn main() {
    let args = parse_args();
    let seeds = load_seeds(&args.seeds);
    if seeds.is_empty() {
        eprintln!("no seeds provided\n{USAGE}");
        exit(2);
    }

    // Panics print normally but must not take the process down before
    // the artifact is written; run_input converts them into a report.
    let mut prng = Prng::new(args.seed);
    let mut current = seeds[prng.below(seeds.len())].clone();
    let mut injection = args.inject_panic_at;

    for iteration in 0..args.iterations {
        if prng.coin() || current.is_empty() {
            current = seeds[prng.below(seeds.len())].clone();
        }
        current = mutate(&mut prng, &current, &seeds);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_input(&current, &mut injection, iteration);
        }));
        if result.is_err() {
            let path = args
                .crash_dir
                .join(format!("fuzz-crash-iter-{iteration}.bin"));
            if let Err(e) = std::fs::write(&path, &current) {
                eprintln!("FINDING at iteration {iteration} (artifact write failed: {e})");
            } else {
                println!("FINDING at iteration {iteration}: {e}", e = path.display());
            }
            exit(1);
        }
    }

    println!(
        "fuzz burst clean: {} iterations, {} seeds, seed #{:#x}",
        args.iterations,
        seeds.len(),
        args.seed
    );
    exit(0);
}

fn parse_args() -> Args {
    let mut args = Args {
        seeds: Vec::new(),
        iterations: 1000,
        seed: 0x5EED_5EED_5EED_5EED,
        crash_dir: std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target"),
        inject_panic_at: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next().unwrap_or_else(|| {
                eprintln!("{USAGE}");
                exit(2);
            })
        };
        match arg.as_str() {
            "--seeds" | "--seed-path" => args.seeds.push(PathBuf::from(value())),
            "--iterations" => {
                args.iterations = value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                })
            }
            "--seed" => {
                args.seed = value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                })
            }
            "--crash-dir" => args.crash_dir = PathBuf::from(value()),
            // Harness-plumbing self-test: simulates the fuzzer finding
            // a panicking input at iteration N, to verify crash
            // discovery and reporting end-to-end. No real burst uses
            // it.
            "--inject-panic-at" => {
                args.inject_panic_at = Some(value().parse().unwrap_or_else(|_| {
                    eprintln!("{USAGE}");
                    exit(2);
                }))
            }
            other => {
                eprintln!("unknown argument: {other}\n{USAGE}");
                exit(2);
            }
        }
    }
    args
}

/// Runs one input through every fuzz entry point, catching panics so
/// the failing input can be reported instead of silently unwinding.
fn run_input(input: &[u8], injection: &mut Option<usize>, iteration: usize) {
    if let Some(at) = *injection
        && iteration == at
    {
        panic!("inject-panic-at: simulated fuzz finding");
    }
    let closures: [&dyn Fn(); 3] = [&|| fuzz_load(input), &|| fuzz_execute(input), &|| {
        fuzz_shared_region(input)
    }];
    for closure in closures {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(closure));
        if result.is_err() {
            // Report and fail: a runtime panic under fuzz input is a
            // finding, not a shrug.
            std::panic::resume_unwind(Box::new("fuzz target panicked"));
        }
    }
}
