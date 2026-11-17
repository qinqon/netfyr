use std::path::PathBuf;
use std::process::ExitCode;

use netfyr_plugin_runtime::engine_config;
use netfyr_plugin_runtime::metadata::{
    InstalledMetadata, ProviderMetadata, installed_metadata_path,
};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = None;
    let mut output = None;
    let mut metadata = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = Some(PathBuf::from(args.next().ok_or("--output needs a path")?)),
            "--metadata" => {
                metadata = Some(PathBuf::from(args.next().ok_or("--metadata needs a path")?))
            }
            "--help" | "-h" => {
                println!(
                    "Usage: netfyr-plugin-precompile INPUT.wasm --metadata PROVIDER.json --output OUTPUT.cwasm\nAlso writes OUTPUT.cwasm.json with identity, schemas and artifact digest."
                );
                return Ok(());
            }
            _ if !arg.starts_with('-') && input.is_none() => input = Some(PathBuf::from(arg)),
            _ => return Err(format!("unknown argument {arg:?}").into()),
        }
    }
    let input = input.ok_or("pass an input WebAssembly component")?;
    let output = output.ok_or("pass --output OUTPUT.cwasm")?;
    let metadata_path = metadata.ok_or("pass --metadata PROVIDER.json")?;
    let (provider, schemas) = ProviderMetadata::load(metadata_path)?;
    let bytes = std::fs::read(input)?;
    let engine = wasmtime::Engine::new(&engine_config())?;
    let compiled = engine.precompile_component(&bytes)?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("output needs a UTF-8 filename")?;
    let installed = InstalledMetadata::new(provider, schemas, name, &compiled)?.to_json()?;
    // create_new avoids overwriting a source or following an existing symlink.
    // Recompilation uses a new output path or explicitly removes the old file.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    let descriptor_path = installed_metadata_path(&output);
    let mut descriptor = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&descriptor_path)?;
    file.write_all(&compiled)?;
    file.sync_all()?;
    descriptor.write_all(installed.as_bytes())?;
    descriptor.sync_all()?;
    println!("{} ({} bytes)", output.display(), compiled.len());
    println!("{} (installed metadata)", descriptor_path.display());
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
