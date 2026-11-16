//! Experimental, in-process host for versioned WebAssembly observers.
//!
//! Only declared Unix socket paths and bounded logging are linked. The host
//! supplies no WASI imports. A socket grant permits arbitrary bytes at that
//! endpoint; it does not authorize or filter application-protocol operations.
//! OVSDB read-only behavior is implemented by the OVS guest.

mod bindings;
mod host;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use netfyr_state::{Match, SchemaRegistry, Source, State, Value};
use serde_json::Value as Json;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use bindings::Observer;
use bindings::exports::netfyr::plugin::observation as wire;
use host::Host;

pub use bindings::exports::netfyr::plugin::observation::{Diagnostic, FailureKind};
pub use bindings::netfyr::plugin::host_log::Level;

/// Compilation/execution target. Native artifacts use the build host's CPU.
#[derive(Clone, Copy, Debug, Default)]
pub enum Target {
    #[default]
    Native,
    Pulley,
}

/// Shared by the runtime and the separate precompilation tool.
pub fn engine_config(target: Target) -> wasmtime::Result<Config> {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .epoch_interruption(true);
    if matches!(target, Target::Pulley) {
        if !cfg!(feature = "pulley") {
            return Err(wasmtime::Error::msg(
                "rebuild with --features pulley for the Pulley target",
            ));
        }
        let triple = match (
            cfg!(target_pointer_width = "64"),
            cfg!(target_endian = "little"),
        ) {
            (true, true) => "pulley64",
            (true, false) => "pulley64be",
            (false, true) => "pulley32",
            (false, false) => "pulley32be",
        };
        config.target(triple)?;
    }
    Ok(config)
}

/// Explicit host grants for this PoC, supplied without executing the guest.
/// Each schema defines the sub-object owned by one device type.
pub struct Policy {
    pub socket_paths: Vec<String>,
    pub schemas: BTreeMap<String, String>,
    pub timeout: Duration,
    pub fuel: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            socket_paths: Vec::new(),
            schemas: BTreeMap::new(),
            timeout: Duration::from_secs(5),
            fuel: 100_000_000,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ErrorKind {
    Load,
    InvalidPolicy,
    InvalidOutput,
    Trap,
    DeadlineExceeded,
    Plugin(FailureKind),
}

#[derive(Clone, Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    fn new(kind: ErrorKind, message: impl fmt::Display) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Debug)]
pub struct LogRecord {
    pub level: Level,
    pub message: String,
}

/// Validated observed state. Identities remain in the plugin-owned fields;
/// these are not namespace/ifindex-based backend observations or plan inputs.
#[derive(Debug)]
pub struct Snapshot {
    pub states: Vec<State>,
    pub diagnostics: Vec<Diagnostic>,
    pub logs: Vec<LogRecord>,
}

/// A loaded component. Each query gets a new store, resources and limits.
pub struct Plugin {
    engine: Engine,
    component: Component,
    policy: Policy,
    registry: SchemaRegistry,
}

impl Plugin {
    /// Validate and compile a portable component. Requires the `compile` feature.
    #[cfg(feature = "compile")]
    pub fn load(path: impl AsRef<Path>, policy: Policy, target: Target) -> Result<Self, Error> {
        let engine = Engine::new(
            &engine_config(target).map_err(|error| Error::new(ErrorKind::Load, error))?,
        )
        .map_err(|error| Error::new(ErrorKind::Load, error))?;
        let component = Component::from_file(&engine, path)
            .map_err(|error| Error::new(ErrorKind::Load, error))?;
        Self::new(engine, component, policy)
    }

    /// Load the output of the matching `netfyr-plugin-precompile` tool.
    ///
    /// # Safety
    ///
    /// The file must contain trusted, unmodified output from Wasmtime's
    /// precompiler. Deserialization is not validation of untrusted code: a
    /// tampered artifact can execute native code with the host's privileges.
    /// Keep the artifact and its containing directories under trusted control.
    pub unsafe fn load_precompiled(
        path: impl AsRef<Path>,
        policy: Policy,
        target: Target,
    ) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(|error| Error::new(ErrorKind::Load, error))?;
        let engine = Engine::new(
            &engine_config(target).map_err(|error| Error::new(ErrorKind::Load, error))?,
        )
        .map_err(|error| Error::new(ErrorKind::Load, error))?;
        // SAFETY: the caller guarantees these bytes come from trusted
        // precompilation. Reading first avoids a file-backed mapping changing.
        let component = unsafe { Component::deserialize(&engine, &bytes) }
            .map_err(|error| Error::new(ErrorKind::Load, error))?;
        Self::new(engine, component, policy)
    }

    fn new(engine: Engine, component: Component, policy: Policy) -> Result<Self, Error> {
        let registry = registry(&policy)?;
        Ok(Self {
            engine,
            component,
            policy,
            registry,
        })
    }

    pub fn schema_registry(&self) -> &SchemaRegistry {
        &self.registry
    }

    /// Run one bounded, synchronous query. Call outside an async runtime (or
    /// from a blocking worker). The deadline covers instantiation and query,
    /// not component compilation/loading. An exclusive borrow serializes
    /// queries using this engine's deadline interrupt.
    pub fn query(&mut self, config_json: &str, selector: &Match) -> Result<Snapshot, Error> {
        if config_json.len() > 64 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidPolicy,
                "configuration exceeds 64 KiB",
            ));
        }
        let deadline = Instant::now() + self.policy.timeout;
        let host = Host::new(&self.policy, deadline)
            .map_err(|error| Error::new(ErrorKind::Load, error))?;
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store
            .set_fuel(self.policy.fuel)
            .map_err(|error| Error::new(ErrorKind::Load, error))?;
        store.set_epoch_deadline(1);
        let mut linker = Linker::new(&self.engine);
        Observer::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |host| host)
            .map_err(|error| Error::new(ErrorKind::Load, error))?;

        // A sleeping receiver is cancelled immediately on every exit path.
        // Unlike fuel alone, epoch interruption also bounds CPU-bound guests
        // by wall time. Host socket calls use the same absolute deadline.
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
                let observer = Observer::instantiate(&mut store, &self.component, &linker)?;
                observer
                    .netfyr_plugin_observation()
                    .call_query(&mut store, config_json)
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
            .map_err(|failure| Error::new(ErrorKind::Plugin(failure.kind), failure.message))?;
        let mut states = validate(&snapshot, &self.policy, &self.registry)?;
        states.retain(|state| selector.matches(&state.match_spec));
        Ok(Snapshot {
            states,
            diagnostics: snapshot.diagnostics,
            logs: std::mem::take(&mut store.data_mut().logs),
        })
    }
}

fn registry(policy: &Policy) -> Result<SchemaRegistry, Error> {
    if policy.timeout.is_zero() || policy.timeout > Duration::from_secs(60) || policy.fuel == 0 {
        return Err(Error::new(
            ErrorKind::InvalidPolicy,
            "timeout must be in (0, 60s] and fuel must be nonzero",
        ));
    }
    let mut registry = SchemaRegistry::new();
    for (device_type, schema) in &policy.schemas {
        if device_type.is_empty()
            || !device_type
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Error::new(
                ErrorKind::InvalidPolicy,
                "device types must be nonempty identifiers",
            ));
        }
        registry
            .add_schema_node("base", device_type, schema)
            .map_err(|error| Error::new(ErrorKind::InvalidPolicy, error))?;
    }
    Ok(registry)
}

fn validate(
    snapshot: &wire::Snapshot,
    policy: &Policy,
    registry: &SchemaRegistry,
) -> Result<Vec<State>, Error> {
    let invalid = |message| Error::new(ErrorKind::InvalidOutput, message);
    if snapshot.devices.len() > 4096 || snapshot.diagnostics.len() > 256 {
        return Err(invalid("too many devices or diagnostics".to_string()));
    }
    for diagnostic in &snapshot.diagnostics {
        if diagnostic.message.len() > 4096
            || diagnostic
                .device_name
                .as_ref()
                .is_some_and(|s| s.len() > 1024)
            || diagnostic.field.as_ref().is_some_and(|s| s.len() > 1024)
        {
            return Err(invalid("oversized diagnostic".to_string()));
        }
    }
    let mut identities = BTreeSet::new();
    let mut states = Vec::new();
    let mut bytes = 0usize;
    for device in &snapshot.devices {
        bytes = bytes.saturating_add(device.fields_json.len());
        if bytes > 4 * 1024 * 1024 || device.name.is_empty() || device.name.len() > 1024 {
            return Err(invalid(
                "oversized result or invalid device name".to_string(),
            ));
        }
        if !policy.schemas.contains_key(&device.device_type) {
            return Err(invalid(format!(
                "undeclared device type {:?}",
                device.device_type
            )));
        }
        if !identities.insert((&device.device_type, &device.name)) {
            return Err(invalid(format!(
                "duplicate device identity {:?}/{:?}",
                device.device_type, device.name
            )));
        }
        let fields: Json = serde_json::from_str(&device.fields_json)
            .map_err(|_| invalid("fields-json is not valid JSON".to_string()))?;
        let fields = fields
            .as_object()
            .ok_or_else(|| invalid("fields-json must be an object".to_string()))?;
        if fields.len() != 1 || !fields.get(&device.device_type).is_some_and(Json::is_object) {
            return Err(invalid(
                "fields-json must contain only the owned device-type sub-object".to_string(),
            ));
        }
        let mut state = State::new(Source::Kernel);
        state.device_type = device.device_type.clone();
        state.match_spec = Match {
            name: Some(device.name.clone()),
            r#type: Some(device.device_type.clone()),
            ..Match::default()
        };
        state
            .fields
            .insert("name".to_string(), Value::String(device.name.clone()));
        for (key, value) in fields {
            state
                .fields
                .insert(key.clone(), value_from_json(value, 0).map_err(invalid)?);
        }
        let errors = registry.validate(&state);
        if !errors.is_empty() {
            return Err(invalid(format!(
                "invalid device {:?}: {}",
                device.name,
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        states.push(state);
    }
    Ok(states)
}

fn value_from_json(value: &Json, depth: usize) -> Result<Value, String> {
    if depth > 64 {
        return Err("fields-json exceeds the nesting limit".to_string());
    }
    Ok(match value {
        Json::String(text) => Value::String(text.clone()),
        Json::Bool(value) => Value::Bool(*value),
        Json::Number(value) => {
            if let Some(value) = value.as_u64() {
                Value::U64(value)
            } else if let Some(value) = value.as_i64() {
                Value::I64(value)
            } else {
                return Err("floating-point fields are not supported".to_string());
            }
        }
        Json::Array(values) => Value::List(
            values
                .iter()
                .map(|value| value_from_json(value, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Json::Object(values) => Value::Map(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), value_from_json(value, depth + 1)?)))
                .collect::<Result<_, String>>()?,
        ),
        Json::Null => return Err("null fields are not supported".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            schemas: [(
                "test_device".into(),
                r#"{"type":"object","required":["uuid"],"properties":{"uuid":{"type":"string"}}}"#
                    .into(),
            )]
            .into(),
            ..Policy::default()
        }
    }

    fn snapshot(kind: &str, fields: &str) -> wire::Snapshot {
        wire::Snapshot {
            devices: vec![wire::Device {
                name: "br0".into(),
                device_type: kind.into(),
                fields_json: fields.into(),
            }],
            diagnostics: vec![],
        }
    }

    #[test]
    fn declared_results_become_read_only_state_with_type_qualified_matches() {
        let policy = policy();
        let registry = registry(&policy).unwrap();
        let states = validate(
            &snapshot("test_device", r#"{"test_device":{"uuid":"0123"}}"#),
            &policy,
            &registry,
        )
        .unwrap();
        assert_eq!(states[0].match_spec.name.as_deref(), Some("br0"));
        assert_eq!(states[0].match_spec.r#type.as_deref(), Some("test_device"));
        assert!(states[0].match_spec.mac.is_none());
        assert!(!registry.validate_writable(&states[0]).is_empty());
        assert!(registry.to_yaml_query(&states).unwrap().contains("0123"));
    }

    #[test]
    fn undeclared_malformed_duplicate_and_out_of_schema_results_are_rejected() {
        let policy = policy();
        let registry = registry(&policy).unwrap();
        for (kind, fields) in [
            ("ethernet", r#"{"ethernet":{}}"#),
            ("test_device", "[]"),
            ("test_device", r#"{"test_device":{"uuid":12}}"#),
            (
                "test_device",
                r#"{"test_device":{"uuid":"id","extra":true}}"#,
            ),
            ("test_device", r#"{"test_device":{"uuid":null}}"#),
            ("test_device", r#"{"test_device":{},"mtu":1500}"#),
            ("test_device", r#"{"test_device":{}}"#),
        ] {
            assert!(
                validate(&snapshot(kind, fields), &policy, &registry).is_err(),
                "{fields}"
            );
        }
        let mut duplicate = snapshot("test_device", r#"{"test_device":{"uuid":"id"}}"#);
        duplicate.devices.push(duplicate.devices[0].clone());
        assert!(validate(&duplicate, &policy, &registry).is_err());
    }

    #[test]
    fn a_denied_socket_is_rejected_before_io() {
        use bindings::netfyr::plugin::host_unix_socket::{ConnectError, Host as _};
        let mut host = host::Host::new(&policy(), Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(matches!(
            host.connect("/this/path/does/not/exist".into()).unwrap(),
            Err(ConnectError::NotDeclared)
        ));
    }
}
