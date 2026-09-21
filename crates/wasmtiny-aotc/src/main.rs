//! The `wasmtiny-aotc` command-line compiler: `.wasm` in, `.aot` out.

use std::path::PathBuf;

use clap::Parser;
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

/// Ahead-of-time compiler for the wasmtiny runtime.
///
/// Compiles a WebAssembly binary into a finish-linked `.aot` artifact for a
/// chosen target. The default target is the host triple with baseline CPU
/// features; use `--target` to compile for another ISA.
#[derive(Parser)]
#[command(name = "wasmtiny-aotc", version, about)]
struct Cli {
    /// Input `.wasm` file.
    input: PathBuf,

    /// Output `.aot` file. Defaults to the input path with a `.aot` extension.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Target triple to compile for (e.g. `x86_64-unknown-linux-gnu`,
    /// `aarch64-apple-darwin`). Defaults to the host triple with baseline
    /// CPU features.
    #[arg(long)]
    target: Option<String>,

    /// Enable a compiler feature flag (currently fixed to the v1 feature set).
    #[arg(long = "feature", value_name = "FEATURE")]
    features: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    let config = match &cli.target {
        Some(target) => match CompilerConfig::for_target(target) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        None => CompilerConfig::host(),
    };

    let no_features = cli.features.is_empty();
    if !no_features {
        eprintln!(
            "error: --feature is reserved for future feature toggles (v1 feature set is fixed)"
        );
        std::process::exit(1);
    }

    let wasm = match std::fs::read(&cli.input) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("error: failed to read {}: {err}", cli.input.display());
            std::process::exit(1);
        }
    };

    let compiled = match compile_artifact(&wasm, &config) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    let output = cli
        .output
        .unwrap_or_else(|| cli.input.with_extension("aot"));

    if let Err(err) = std::fs::write(&output, &compiled) {
        eprintln!("error: failed to write {}: {err}", output.display());
        std::process::exit(1);
    }
}
