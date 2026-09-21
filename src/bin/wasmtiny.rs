use anyhow::{Context, Result};
use clap::Parser;
use wasmtiny::{WasmApplication, WasmValue};

#[derive(Parser, Debug)]
struct Args {
    #[arg(help = "Path to WASM module or .aot artifact")]
    module: String,

    #[arg(long, help = "Load the input as a compiled .aot artifact")]
    aot: bool,

    #[arg(short, long, help = "Function to call")]
    function: Option<String>,

    #[arg(value_name = "ARGS", help = "Arguments to pass to the function (i32)")]
    args: Vec<i32>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let use_aot = args.aot || args.module.ends_with(".aot");

    if use_aot {
        run_aot(&args)
    } else {
        run_interpreter(&args)
    }
}

fn run_interpreter(args: &Args) -> Result<()> {
    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_file(&args.module)?;
    app.instantiate(module_idx)?;

    println!("Loaded WASM module from {}", args.module);

    match &args.function {
        Some(func) => {
            let wasm_args: Vec<WasmValue> = args.args.iter().map(|&i| WasmValue::I32(i)).collect();
            match app.call_function(module_idx, func, &wasm_args) {
                Ok(results) => println!("Function '{func}' returned: {results:?}"),
                Err(err) => {
                    eprintln!("Error calling function '{func}': {err}");
                    std::process::exit(1);
                }
            }
        }
        None => match app.execute_start(module_idx) {
            Ok(()) => println!("Module executed successfully"),
            Err(err) => {
                eprintln!("Error executing module: {err}");
                std::process::exit(1);
            }
        },
    }

    Ok(())
}

#[cfg(feature = "aot")]
fn run_aot(args: &Args) -> Result<()> {
    use wasmtiny::aot::{AotInstance, AotLoader};

    let bytes =
        std::fs::read(&args.module).with_context(|| format!("failed to read {}", args.module))?;
    let module = AotLoader::new()
        .load(&bytes)
        .context("artifact load failed")?;
    // Instantiation verifies integrity and runs the start function, if any.
    let mut instance = AotInstance::new(&module).context("instantiation failed")?;

    println!("Loaded AOT artifact from {}", args.module);

    match &args.function {
        Some(func) => {
            let wasm_args: Vec<WasmValue> = args.args.iter().map(|&i| WasmValue::I32(i)).collect();
            match instance.invoke_export(func, &wasm_args) {
                Ok(results) => println!("Function '{func}' returned: {results:?}"),
                Err(err) => {
                    eprintln!("Error calling function '{func}': {err}");
                    std::process::exit(1);
                }
            }
        }
        None => println!("Artifact instantiated (start function executed, if present)"),
    }

    Ok(())
}

#[cfg(not(feature = "aot"))]
fn run_aot(args: &Args) -> Result<()> {
    let _ = args;
    anyhow::bail!("the `.aot` path requires the `aot` feature (this build has it disabled)");
}
