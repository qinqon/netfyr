//! WebAssembly execution adapter for the neutral netfyr observer API.
//!
//! Only declared Unix sockets and bounded logging are linked. The host supplies
//! no WASI imports. Socket grants restrict endpoints, not protocol operations.

mod bindings;
mod host;
pub mod instances;
pub mod metadata;

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use netfyr_plugin_api::{
    Contract, Device, Observation, Observer, ObserverBackend, ObserverFactory, ValidatedObserver,
};
use netfyr_state::{Match, SchemaRegistry};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use bindings::Observer as ComponentObserver;
use bindings::exports::netfyr::plugin::observation as wire;
use host::Host;

// Compatibility exports are neutral SDK types, never generated WIT types.
pub use netfyr_plugin_api::{
    Diagnostic, Error, ErrorKind, FailureKind, Level, LogRecord, Policy, Snapshot,
};

/// Shared by the native precompiler and runtime adapter on the local machine.
pub fn engine_config() -> Config {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .epoch_interruption(true);
    config
}

pub(crate) fn registry(policy: &Policy) -> Result<SchemaRegistry, Error> {
    Contract::registry(policy)
}

/// The Wasm adapter. The engine-facing contract is `netfyr_plugin_api::Observer`.
pub struct Plugin {
    observer: ValidatedObserver<WasmBackend>,
}

impl Plugin {
    /// Read static metadata and verify the artifact before deserialization.
    ///
    /// # Safety
    /// Descriptor, artifact and containing directory must be trusted local
    /// precompiler output. The digest does not authenticate either file.
    pub unsafe fn load_package(
        path: impl AsRef<Path>,
        mut policy: Policy,
    ) -> Result<(String, Self), Error> {
        let path = path.as_ref();
        let metadata = metadata::InstalledMetadata::load(path)?;
        let bytes = metadata.artifact(path, &mut policy)?;
        registry(&policy)?;
        // SAFETY: the caller guarantees trust in the exact verified bytes.
        let mut plugin = unsafe { Self::from_precompiled_bytes(&bytes, policy)? };
        plugin
            .observer
            .set_config_schema(metadata.config_schema())?;
        Ok((metadata.id, plugin))
    }

    #[cfg(feature = "compile")]
    pub fn load_provider(
        path: impl AsRef<Path>,
        descriptor: impl AsRef<Path>,
        mut policy: Policy,
    ) -> Result<(String, Self), Error> {
        let (metadata, schemas) = metadata::ProviderMetadata::load(descriptor)?;
        policy.schemas = schemas
            .into_iter()
            .map(|(name, schema)| (name, schema.to_string()))
            .collect();
        let mut plugin = Self::load(path, policy)?;
        plugin.observer.set_config_schema(&metadata.config_schema)?;
        Ok((metadata.id, plugin))
    }

    #[cfg(feature = "compile")]
    pub fn load(path: impl AsRef<Path>, policy: Policy) -> Result<Self, Error> {
        let engine = Engine::new(&engine_config()).map_err(load_error)?;
        let component = Component::from_file(&engine, path).map_err(load_error)?;
        Self::new(engine, component, policy)
    }

    /// Load trusted precompiled bytes without package metadata (low-level API).
    ///
    /// # Safety
    /// The file must contain trusted, unmodified output from the matching
    /// precompiler; arbitrary native artifacts can execute code in the host.
    pub unsafe fn load_precompiled(path: impl AsRef<Path>, policy: Policy) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(load_error)?;
        // SAFETY: the caller guarantees trusted compiler output.
        unsafe { Self::from_precompiled_bytes(&bytes, policy) }
    }

    unsafe fn from_precompiled_bytes(bytes: &[u8], policy: Policy) -> Result<Self, Error> {
        let engine = Engine::new(&engine_config()).map_err(load_error)?;
        // SAFETY: trust in these bytes is required by the caller's contract.
        let component = unsafe { Component::deserialize(&engine, bytes) }.map_err(load_error)?;
        Self::new(engine, component, policy)
    }

    fn new(engine: Engine, component: Component, policy: Policy) -> Result<Self, Error> {
        let backend = WasmBackend {
            engine,
            component,
            policy: policy.clone(),
        };
        Ok(Self {
            observer: ValidatedObserver::new(backend, policy, None)?,
        })
    }

    pub fn schema_registry(&self) -> &SchemaRegistry {
        self.observer.schema_registry()
    }

    /// Synchronous query; use outside an async runtime or on a blocking worker.
    pub fn query(&mut self, config: &str, selector: &Match) -> Result<Snapshot, Error> {
        self.observer.query(config, selector)
    }
}

impl Observer for Plugin {
    fn query(&mut self, config: &str, selector: &Match) -> Result<Snapshot, Error> {
        self.query(config, selector)
    }
    fn schemas(&self) -> Arc<SchemaRegistry> {
        self.observer.schemas()
    }
}

/// Binds a trusted installed Wasm package to the neutral factory contract.
/// The registry has no native library/path loading mechanism.
pub struct WasmFactory {
    id: String,
    descriptor: PathBuf,
}

impl WasmFactory {
    /// # Safety
    /// The descriptor and its artifact must remain trusted compiler output for
    /// the lifetime of this factory, including when a registry calls `create`.
    pub unsafe fn new(id: String, descriptor: PathBuf) -> Self {
        Self { id, descriptor }
    }
}

impl ObserverFactory for WasmFactory {
    fn create(&self, policy: Policy) -> Result<Box<dyn Observer>, Error> {
        // SAFETY: construction requires trusted installation paths to remain so.
        let (id, plugin) = unsafe { Plugin::load_package(&self.descriptor, policy)? };
        if id != self.id {
            return Err(Error::new(
                ErrorKind::InvalidMetadata,
                "installed plugin identity changed",
            ));
        }
        Ok(Box::new(plugin))
    }
}

fn load_error(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Load, error)
}

struct WasmBackend {
    engine: Engine,
    component: Component,
    policy: Policy,
}

impl ObserverBackend for WasmBackend {
    fn observe(&mut self, config: &str) -> Result<Observation, Error> {
        let deadline = Instant::now() + self.policy.timeout;
        let host = Host::new(&self.policy, deadline).map_err(load_error)?;
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store.set_fuel(self.policy.fuel).map_err(load_error)?;
        store.set_epoch_deadline(1);
        let mut linker = Linker::new(&self.engine);
        ComponentObserver::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |host| host,
        )
        .map_err(load_error)?;
        // Fuel bounds work; epoch interruption also bounds CPU-bound guests by
        // wall time. Every host socket call uses this same absolute deadline.
        let result = std::thread::scope(|scope| {
            let (done, receiver) = mpsc::channel::<()>();
            let engine = &self.engine;
            scope.spawn(move || {
                if matches!(
                    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    engine.increment_epoch();
                }
            });
            let result = (|| {
                let observer =
                    ComponentObserver::instantiate(&mut store, &self.component, &linker)?;
                observer
                    .netfyr_plugin_observation()
                    .call_query(&mut store, config)
            })();
            let _ = done.send(());
            result
        });
        if Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::DeadlineExceeded,
                "plugin query deadline exceeded",
            ));
        }
        let snapshot = result
            .map_err(|error| Error::new(ErrorKind::Trap, format!("{error:#}")))?
            .map_err(|failure| {
                Error::new(
                    ErrorKind::Plugin(failure_kind(failure.kind)),
                    failure.message,
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
            logs: std::mem::take(&mut store.data_mut().logs),
        })
    }
}

fn failure_kind(kind: wire::FailureKind) -> FailureKind {
    match kind {
        wire::FailureKind::PermissionDenied => FailureKind::PermissionDenied,
        wire::FailureKind::Unreachable => FailureKind::Unreachable,
        wire::FailureKind::DeadlineExceeded => FailureKind::DeadlineExceeded,
        wire::FailureKind::Unsupported => FailureKind::Unsupported,
        wire::FailureKind::Undecodable => FailureKind::Undecodable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_denied_socket_is_rejected_before_io() {
        use bindings::netfyr::plugin::host_unix_socket::{ConnectError, Host as _};
        let mut host = Host::new(
            &Policy::default(),
            Instant::now() + std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            host.connect("/this/path/does/not/exist".into()).unwrap(),
            Err(ConnectError::NotDeclared)
        ));
    }
}
