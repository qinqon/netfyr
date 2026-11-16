use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use netfyr_backend::{NetlinkBackend, NetworkBackend};
use netfyr_plugin_runtime::{Plugin, Policy, Target};
use netfyr_state::{Match, SchemaRegistry};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = None;
    let mut precompiled = false;
    let mut endpoint = "/run/openvswitch/db.sock".to_string();
    let mut schemas = PathBuf::from("crates/netfyr-ovs/schemas.json");
    let mut allowed = Vec::new();
    let mut policy = Policy::default();
    let mut target = Target::Native;
    let mut selector = Match::default();
    let mut kernel = true;
    let mut config = None;
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
            "--ovsdb" => endpoint = args.next().ok_or("--ovsdb needs a path")?,
            "--schemas" => schemas = PathBuf::from(args.next().ok_or("--schemas needs a path")?),
            "--allow-socket" => allowed.push(args.next().ok_or("--allow-socket needs a path")?),
            "--timeout-ms" => {
                policy.timeout = Duration::from_millis(
                    args.next()
                        .ok_or("--timeout-ms needs an integer")?
                        .parse()?,
                )
            }
            "--name" => selector.name = Some(args.next().ok_or("--name needs a name")?),
            "--type" => selector.r#type = Some(args.next().ok_or("--type needs a type")?),
            "--config-json" => config = Some(args.next().ok_or("--config-json needs JSON")?),
            "--pulley" => target = Target::Pulley,
            "--no-kernel" => kernel = false,
            "--help" | "-h" => {
                println!("Usage: query-with-plugins (--plugin FILE.wasm | --trusted-precompiled FILE.cwasm)
    [--ovsdb PATH] [--schemas PATH] [--allow-socket PATH ...]
    [--name NAME] [--type TYPE] [--timeout-ms N] [--pulley] [--no-kernel]
    [--config-json JSON]

Precompiled files must be trusted, unmodified local precompiler output.
Output is query-form YAML, one document per observation source.");
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg:?}").into()),
        }
    }
    let path = plugin.ok_or("pass --plugin FILE.wasm or --trusted-precompiled FILE.cwasm")?;
    if kernel {
        let backend = NetlinkBackend::open("/proc/self/ns/net")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        // Filter after query_all: an OVS-only name selector must not turn a
        // successful kernel inventory into a fatal NotFound result.
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
    }
    let schema_json: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(schemas)?)?;
    policy.schemas = schema_json
        .into_iter()
        .map(|(name, schema)| (name, schema.to_string()))
        .collect();
    policy.socket_paths = if allowed.is_empty() {
        vec![endpoint.clone()]
    } else {
        allowed
    };
    let mut plugin = if precompiled {
        // SAFETY: --trusted-precompiled explicitly selects the trusted local
        // artifact loading path. This flag does not authenticate the file.
        unsafe { Plugin::load_precompiled(path, policy, target)? }
    } else {
        #[cfg(feature = "compile")]
        {
            Plugin::load(path, policy, target)?
        }
        #[cfg(not(feature = "compile"))]
        {
            return Err("this runtime-only build needs --trusted-precompiled FILE.cwasm".into());
        }
    };
    let config = config.unwrap_or_else(|| serde_json::json!({"endpoint": endpoint}).to_string());
    let snapshot = plugin
        .query(&config, &selector)
        .map_err(|error| format!("OVSDB endpoint {endpoint:?}: {error}"))?;
    if kernel {
        println!("---");
    }
    println!("# OVSDB endpoint {endpoint:?}");
    print!(
        "{}",
        plugin.schema_registry().to_yaml_query(&snapshot.states)?
    );
    for diagnostic in snapshot.diagnostics {
        eprintln!(
            "plugin {:?} {:?}: {}",
            diagnostic.device_name, diagnostic.field, diagnostic.message
        );
    }
    for record in snapshot.logs {
        eprintln!("plugin {:?}: {}", record.level, record.message);
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
