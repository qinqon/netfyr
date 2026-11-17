//! Measurement-only reference, never a production native plugin/loader.
//! Both variants use the exact netfyr-ovs discovery/decoder and shared validator.

use std::path::PathBuf;
use std::time::Instant;

#[cfg(not(feature = "wasm"))]
use netfyr_plugin_api::{
    Device, Diagnostic, Error, ErrorKind, FailureKind, Level, LogRecord, Observation,
    ObserverBackend, ValidatedObserver,
};
use netfyr_plugin_api::{Observer, Policy};
use netfyr_state::Match;
#[cfg(not(feature = "wasm"))]
use serde_json::Value as Json;
use serde_json::json;
#[cfg(not(feature = "wasm"))]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(not(feature = "wasm"))]
use tokio::net::UnixStream;

#[cfg(not(feature = "wasm"))]
fn io_failure(error: impl std::fmt::Display) -> netfyr_ovs::Failure {
    netfyr_ovs::Failure {
        kind: netfyr_ovs::FailureKind::Unreachable,
        message: error.to_string(),
    }
}

#[cfg(not(feature = "wasm"))]
struct NativeSocket {
    runtime: tokio::runtime::Runtime,
    stream: UnixStream,
    deadline: Instant,
    remaining: usize,
}

#[cfg(not(feature = "wasm"))]
impl NativeSocket {
    fn connect(endpoint: &str, deadline: Instant) -> Result<Self, netfyr_ovs::Failure> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io_failure)?;
        let stream = runtime
            .block_on(async {
                tokio::time::timeout_at(deadline.into(), UnixStream::connect(endpoint)).await
            })
            .map_err(io_failure)?
            .map_err(io_failure)?;
        Ok(Self {
            runtime,
            stream,
            deadline,
            remaining: 8 * 1024 * 1024,
        })
    }

    fn charge(&mut self, count: usize) -> Result<(), netfyr_ovs::Failure> {
        self.remaining = self
            .remaining
            .checked_sub(count)
            .ok_or_else(|| io_failure("I/O budget exceeded"))?;
        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
impl netfyr_ovs::Transport for NativeSocket {
    fn write(&mut self, bytes: &[u8]) -> Result<(), netfyr_ovs::Failure> {
        self.charge(bytes.len())?;
        self.runtime
            .block_on(async {
                tokio::time::timeout_at(self.deadline.into(), self.stream.write_all(bytes)).await
            })
            .map_err(io_failure)?
            .map_err(io_failure)
    }

    fn read(&mut self) -> Result<Vec<u8>, netfyr_ovs::Failure> {
        let mut bytes = vec![0; 16 * 1024];
        let count = self
            .runtime
            .block_on(async {
                tokio::time::timeout_at(self.deadline.into(), self.stream.read(&mut bytes)).await
            })
            .map_err(io_failure)?
            .map_err(io_failure)?;
        self.charge(count)?;
        bytes.truncate(count);
        Ok(bytes)
    }
}

#[cfg(not(feature = "wasm"))]
struct NativeOvs {
    policy: Policy,
}

#[cfg(not(feature = "wasm"))]
impl ObserverBackend for NativeOvs {
    fn observe(&mut self, config: &str) -> Result<Observation, Error> {
        let read = || -> Result<_, netfyr_ovs::Failure> {
            let endpoint = netfyr_ovs::endpoint(config)?;
            if !self.policy.socket_paths.contains(&endpoint) {
                return Err(netfyr_ovs::Failure {
                    kind: netfyr_ovs::FailureKind::PermissionDenied,
                    message: "socket path is not declared".into(),
                });
            }
            let mut socket =
                NativeSocket::connect(&endpoint, Instant::now() + self.policy.timeout)?;
            netfyr_ovs::discover(&mut socket)
        };
        let snapshot = read().map_err(|error| {
            Error::new(
                ErrorKind::Plugin(match error.kind {
                    netfyr_ovs::FailureKind::PermissionDenied => FailureKind::PermissionDenied,
                    netfyr_ovs::FailureKind::Unreachable => FailureKind::Unreachable,
                    netfyr_ovs::FailureKind::Unsupported => FailureKind::Unsupported,
                    netfyr_ovs::FailureKind::Undecodable => FailureKind::Undecodable,
                }),
                error.message,
            )
        })?;
        Ok(Observation {
            devices: snapshot
                .devices
                .into_iter()
                .map(|device| Device {
                    name: device.name,
                    device_type: device.device_type,
                    fields_json: device.fields_json,
                })
                .collect(),
            diagnostics: snapshot
                .diagnostics
                .into_iter()
                .map(|diagnostic| Diagnostic {
                    device_name: diagnostic.device_name,
                    field: diagnostic.field,
                    message: diagnostic.message,
                })
                .collect(),
            logs: vec![LogRecord {
                level: Level::Debug,
                message: "reading an OVSDB snapshot".into(),
            }],
        })
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut descriptor = None;
    let mut endpoint = None;
    let mut wasm = false;
    let mut iterations = 1usize;
    let mut warmups = 0usize;
    let mut dump = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--descriptor" => {
                descriptor = Some(PathBuf::from(args.next().ok_or("descriptor needs a path")?))
            }
            "--endpoint" => endpoint = Some(args.next().ok_or("endpoint needs a path")?),
            "--iterations" => {
                iterations = args.next().ok_or("iterations needs an integer")?.parse()?
            }
            "--warmups" => warmups = args.next().ok_or("warmups needs an integer")?.parse()?,
            "--wasm" => wasm = true,
            "--dump" => dump = true,
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    if iterations == 0 || iterations > 10000 || warmups > 1000 {
        return Err("invalid iteration count".into());
    }
    let descriptor = descriptor.ok_or("pass --descriptor INSTALLED.cwasm.json")?;
    let endpoint = endpoint.ok_or("pass --endpoint PATH")?;
    let setup_start = Instant::now();
    let config = json!({"endpoint":endpoint}).to_string();
    let policy = Policy {
        socket_paths: vec![endpoint],
        ..Policy::default()
    };
    let mut observer: Box<dyn Observer> = if wasm {
        #[cfg(feature = "wasm")]
        {
            // SAFETY: this benchmark takes only trusted local precompiler output.
            let (_, plugin) =
                unsafe { netfyr_plugin_runtime::Plugin::load_package(&descriptor, policy)? };
            Box::new(plugin)
        }
        #[cfg(not(feature = "wasm"))]
        {
            return Err("rebuild the probe with --features wasm".into());
        }
    } else {
        #[cfg(not(feature = "wasm"))]
        {
            // Benchmark-only native reference reads the same installed schemas. It
            // neither loads nor deserializes the native Wasm artifact, so it needs
            // no Wasmtime or digest verification. Native code is statically linked.
            let metadata: Json = serde_json::from_slice(&std::fs::read(&descriptor)?)?;
            let mut policy = policy;
            policy.schemas = metadata["schemas"]
                .as_object()
                .ok_or("missing schemas")?
                .iter()
                .map(|(name, schema)| (name.clone(), schema.to_string()))
                .collect();
            let backend = NativeOvs {
                policy: policy.clone(),
            };
            Box::new(ValidatedObserver::new(
                backend,
                policy,
                Some(&metadata["config-schema"]),
            )?)
        }
        #[cfg(feature = "wasm")]
        {
            return Err(
                "this build supports only --wasm; use the separately built native reference".into(),
            );
        }
    };
    let setup_ms = setup_start.elapsed().as_secs_f64() * 1000.0;
    let selector = Match::default();
    let mut elapsed = Vec::new();
    let mut devices = 0;
    for iteration in 0..(iterations + warmups) {
        let start = Instant::now();
        let snapshot = observer.query(&config, &selector)?;
        let duration = start.elapsed().as_secs_f64() * 1000.0;
        devices = snapshot.states.len();
        if dump {
            print!("{}", observer.schemas().to_yaml_query(&snapshot.states)?);
            return Ok(());
        }
        if iteration >= warmups {
            elapsed.push(duration);
        }
    }
    println!(
        "{}",
        json!({"setup_ms":setup_ms,"query_samples_ms":elapsed,"devices":devices})
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run()
}
