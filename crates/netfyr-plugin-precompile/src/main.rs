use std::path::PathBuf;
use std::process::ExitCode;

use netfyr_plugin_runtime::{Target, engine_config};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = None;
    let mut output = None;
    let mut target = Target::Native;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = Some(PathBuf::from(args.next().ok_or("--output needs a path")?)),
            "--pulley" => target = Target::Pulley,
            "--help" | "-h" => {
                println!(
                    "Usage: netfyr-plugin-precompile INPUT.wasm --output OUTPUT.cwasm [--pulley]"
                );
                return Ok(());
            }
            _ if !arg.starts_with('-') && input.is_none() => input = Some(PathBuf::from(arg)),
            _ => return Err(format!("unknown argument {arg:?}").into()),
        }
    }
    let input = input.ok_or("pass an input WebAssembly component")?;
    let output = output.ok_or("pass --output OUTPUT.cwasm")?;
    let bytes = std::fs::read(input)?;
    let engine = wasmtime::Engine::new(&engine_config(target)?)?;
    let compiled = engine.precompile_component(&bytes)?;
    // create_new avoids overwriting a source or following an existing symlink.
    // Recompilation uses a new output path or explicitly removes the old file.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    file.write_all(&compiled)?;
    println!("{} ({} bytes)", output.display(), compiled.len());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("precompile failed: {error}");
            ExitCode::FAILURE
        }
    }
}
