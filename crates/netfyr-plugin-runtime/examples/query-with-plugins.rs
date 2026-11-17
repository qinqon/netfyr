use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use netfyr_backend::{NetlinkBackend, NetworkBackend};
use netfyr_plugin_api::{Policy, Snapshot};
use netfyr_plugin_runtime::{Plugin, instances};
use netfyr_state::{Match, SchemaRegistry};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = None;
    let mut precompiled = false;
    let mut metadata = None;
    let mut allowed = Vec::new();
    let mut policy = Policy::default();
    let mut selector = Match::default();
    let mut kernel = true;
    let mut config = None;
    let mut config_dirs = Vec::new();
    let mut plugin_dir = PathBuf::from(instances::DEFAULT_PLUGIN_DIR);
    let mut instance_options = false;
    let mut manual_options = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plugin" | "--trusted-precompiled" => {
                if plugin.is_some() {
                    return Err("pass exactly one component path".into());
                }
                plugin = Some(PathBuf::from(
                    args.next().ok_or("component option needs a path")?,
                ));
                precompiled = arg == "--trusted-precompiled";
            }
            "--metadata" => {
                manual_options = true;
                metadata = Some(PathBuf::from(args.next().ok_or("--metadata needs a path")?))
            }
            "--allow-socket" => {
                manual_options = true;
                allowed.push(args.next().ok_or("--allow-socket needs a path")?);
            }
            "--plugin-config-dir" => {
                instance_options = true;
                config_dirs.push(PathBuf::from(
                    args.next().ok_or("--plugin-config-dir needs a path")?,
                ));
            }
            "--plugin-dir" => {
                instance_options = true;
                plugin_dir = PathBuf::from(args.next().ok_or("--plugin-dir needs a path")?);
            }
            "--timeout-ms" => {
                manual_options = true;
                policy.timeout = Duration::from_millis(
                    args.next()
                        .ok_or("--timeout-ms needs an integer")?
                        .parse()?,
                )
            }
            "--name" => selector.name = Some(args.next().ok_or("--name needs a name")?),
            "--type" => selector.r#type = Some(args.next().ok_or("--type needs a type")?),
            "--config-json" => {
                manual_options = true;
                config = Some(args.next().ok_or("--config-json needs JSON")?);
            }
            "--no-kernel" => kernel = false,
            "--help" | "-h" => {
                println!(
                    "Usage: query-with-plugins [--plugin-config-dir DIR ...] [--plugin-dir DIR]
    [--name NAME] [--type TYPE] [--no-kernel]

Default: read instance YAML from /usr/lib/netfyr/plugin.d, /etc/netfyr/plugin.d,
and /run/netfyr/plugin.d (low to high). Explicit config dirs replace these tiers.
The trusted plugin directory defaults to /usr/lib/netfyr/plugins.

Explicit single-plugin development mode:
    --trusted-precompiled INSTALLED.cwasm.json
    [--allow-socket PATH ...]
    [--name NAME] [--type TYPE] [--timeout-ms N] [--no-kernel]
    [--config-json JSON]

Installed metadata and artifacts must be trusted local precompiler output.
Identity and schemas come from installed metadata, without running the guest.
Configuration defaults to {{}} and is passed unchanged to the guest.
No sockets are granted unless explicitly allowed.
Build with the compile feature to use --plugin FILE.wasm --metadata PROVIDER.json for comparison.
Output is query-form YAML, one document per observation source."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg:?}").into()),
        }
    }
    if plugin.is_some() && instance_options {
        return Err(
            "instance directory options cannot be combined with explicit plugin paths".into(),
        );
    }
    if plugin.is_none() {
        if manual_options {
            return Err("per-plugin options require an explicit plugin path; use per-instance YAML in directory mode".into());
        }
        if config_dirs.is_empty() {
            config_dirs = instances::DEFAULT_CONFIG_DIRS
                .iter()
                .map(PathBuf::from)
                .collect();
        }
        // Validate every enabled config against static metadata before executing.
        let prepared = instances::prepare(instances::load(&config_dirs)?, &plugin_dir)?;
        // SAFETY: trust the configured installation and operator drop-ins.
        let registry = unsafe { instances::observer_registry(&prepared)? };
        let mut printed = false;
        if kernel {
            print_kernel(&selector)?;
            printed = true;
        }
        let mut failed = false;
        for instance in prepared {
            let report = instance.query(&registry, &selector);
            match report.result {
                Ok(observed) => {
                    if printed {
                        println!("---");
                    }
                    printed = true;
                    let label = format!("{} ({})", report.name, report.plugin);
                    print_snapshot(&label, &observed.schema_registry, observed.snapshot)?;
                }
                Err(error) => {
                    failed = true;
                    eprintln!(
                        "instance {:?} plugin {:?}: {error}",
                        report.name, report.plugin
                    );
                }
            }
        }
        if !printed && !failed {
            println!("[]");
        }
        return if failed {
            Err("one or more plugin instances failed; successful observations were retained".into())
        } else {
            Ok(())
        };
    }
    let path = plugin.expect("explicit plugin path checked above");
    if precompiled && metadata.is_some() {
        return Err("--metadata is only for the compile-on-load comparison; installed schemas cannot be overridden".into());
    }
    if kernel {
        print_kernel(&selector)?;
    }
    policy.socket_paths = allowed;
    let (source, mut plugin) = if precompiled {
        // SAFETY: --trusted-precompiled explicitly selects the trusted local
        // artifact loading path. This flag does not authenticate the file.
        unsafe { Plugin::load_package(path, policy)? }
    } else {
        #[cfg(feature = "compile")]
        {
            let metadata =
                metadata.ok_or("pass --metadata PROVIDER.json for compilation on load")?;
            Plugin::load_provider(path, metadata, policy)?
        }
        #[cfg(not(feature = "compile"))]
        {
            return Err(
                "this runtime-only build needs --trusted-precompiled INSTALLED.cwasm.json".into(),
            );
        }
    };
    let config = config.unwrap_or_else(|| "{}".to_string());
    let snapshot = plugin
        .query(&config, &selector)
        .map_err(|error| format!("source {source:?}: {error}"))?;
    if kernel {
        println!("---");
    }
    print_snapshot(&source, plugin.schema_registry(), snapshot)
}

fn print_kernel(selector: &Match) -> Result<(), Box<dyn std::error::Error>> {
    let backend = NetlinkBackend::open("/proc/self/ns/net")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let observation = runtime.block_on(backend.query_all())?;
    let states = observation
        .devices
        .into_iter()
        .map(|device| device.state)
        .filter(|state| selector.matches(&state.match_spec))
        .collect::<Vec<_>>();
    println!("# kernel namespace {:?}", backend.namespace());
    print!("{}", SchemaRegistry::new().to_yaml_query(&states)?);
    for diagnostic in observation.diagnostics {
        eprintln!("kernel: {}", diagnostic.message);
    }
    Ok(())
}

fn print_snapshot(
    source: &str,
    registry: &SchemaRegistry,
    snapshot: Snapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("# source {source:?}");
    print!("{}", registry.to_yaml_query(&snapshot.states)?);
    for diagnostic in snapshot.diagnostics {
        eprintln!(
            "source {source:?} {:?} {:?}: {}",
            diagnostic.device_name, diagnostic.field, diagnostic.message
        );
    }
    for record in snapshot.logs {
        eprintln!("source {source:?} {:?}: {}", record.level, record.message);
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("query failed: {error}");
            ExitCode::FAILURE
        }
    }
}
