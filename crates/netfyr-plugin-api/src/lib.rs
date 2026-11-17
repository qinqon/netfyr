//! Host-facing observer contract. This crate has no Wasmtime or generated WIT
//! dependencies. It is an in-process Rust API, NOT a stable dynamic-library ABI.
//!
//! WebAssembly is the only production loader today. A future trusted native
//! implementation can use the same contract without claiming Wasm isolation.

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;
mod validation;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use netfyr_state::{Match, SchemaRegistry, State};
use serde_json::Value as Json;

pub use validation::{Contract, configuration_registry, validate_config, value_from_json};

/// Host grants and execution budgets. Native implementations cannot be assumed
/// to enforce Wasm-specific fuel, memory or interruption guarantees.
#[derive(Clone)]
pub struct Policy {
    pub socket_paths: Vec<String>,
    pub schemas: BTreeMap<String, String>,
    pub timeout: Duration,
    pub fuel: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            socket_paths: vec![],
            schemas: BTreeMap::new(),
            timeout: Duration::from_secs(5),
            fuel: 100_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    PermissionDenied,
    Unreachable,
    DeadlineExceeded,
    Unsupported,
    Undecodable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Load,
    InvalidMetadata,
    InvalidConfig,
    InvalidPolicy,
    InvalidOutput,
    /// An execution trap, not a promise of native crash containment.
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
    pub fn new(kind: ErrorKind, message: impl fmt::Display) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    pub level: Level,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub device_name: Option<String>,
    pub field: Option<String>,
    pub message: String,
}

/// Unvalidated provider output, independent of generated component types.
#[derive(Clone, Debug)]
pub struct Device {
    pub name: String,
    pub device_type: String,
    pub fields_json: String,
}

#[derive(Default, Debug)]
pub struct Observation {
    pub devices: Vec<Device>,
    pub diagnostics: Vec<Diagnostic>,
    pub logs: Vec<LogRecord>,
}

/// Validated observed states, never kernel-ifindex identities or planner inputs.
#[derive(Debug)]
pub struct Snapshot {
    pub states: Vec<State>,
    pub diagnostics: Vec<Diagnostic>,
    pub logs: Vec<LogRecord>,
}

/// Object-safe engine-facing contract. Configuration is per instance; matching
/// and validation occur before a result is exposed. Host services are bound by
/// the factory, not passed as Wasmtime handles through this API.
pub trait Observer {
    fn query(&mut self, config_json: &str, selector: &Match) -> Result<Snapshot, Error>;
    fn schemas(&self) -> Arc<SchemaRegistry>;
}

/// Adapter/provider execution hook. The wrapper supplies common validation and
/// matching. This trait alone provides no memory, privilege or crash isolation.
pub trait ObserverBackend {
    fn observe(&mut self, config_json: &str) -> Result<Observation, Error>;
}

pub struct ValidatedObserver<B> {
    backend: B,
    contract: Contract,
}

impl<B> ValidatedObserver<B> {
    pub fn new(backend: B, policy: Policy, config_schema: Option<&Json>) -> Result<Self, Error> {
        Ok(Self {
            backend,
            contract: Contract::new(policy, config_schema)?,
        })
    }

    pub fn set_config_schema(&mut self, schema: &Json) -> Result<(), Error> {
        self.contract.set_config_schema(schema)
    }

    pub fn schema_registry(&self) -> &SchemaRegistry {
        self.contract.schema_registry()
    }
}

impl<B: ObserverBackend> Observer for ValidatedObserver<B> {
    fn query(&mut self, config_json: &str, selector: &Match) -> Result<Snapshot, Error> {
        self.contract.check_config(config_json)?;
        let output = self.backend.observe(config_json)?;
        self.contract.validate(output, selector)
    }
    fn schemas(&self) -> Arc<SchemaRegistry> {
        self.contract.schemas()
    }
}

/// Factories bind per-instance policy to an implementation. Registering a factory
/// is trusted application code, not authorization to load an arbitrary module.
pub trait ObserverFactory {
    fn create(&self, policy: Policy) -> Result<Box<dyn Observer>, Error>;
}

impl<F> ObserverFactory for F
where
    F: Fn(Policy) -> Result<Box<dyn Observer>, Error>,
{
    fn create(&self, policy: Policy) -> Result<Box<dyn Observer>, Error> {
        self(policy)
    }
}

#[derive(Default)]
pub struct ObserverRegistry {
    factories: BTreeMap<String, Box<dyn ObserverFactory>>,
}

impl ObserverRegistry {
    pub fn contains(&self, id: &str) -> bool {
        self.factories.contains_key(id)
    }

    pub fn register(
        &mut self,
        id: String,
        factory: impl ObserverFactory + 'static,
    ) -> Result<(), Error> {
        if id.is_empty() || self.factories.contains_key(&id) {
            return Err(Error::new(
                ErrorKind::InvalidPolicy,
                "empty or duplicate observer registration",
            ));
        }
        self.factories.insert(id, Box::new(factory));
        Ok(())
    }

    pub fn create(&self, id: &str, policy: Policy) -> Result<Box<dyn Observer>, Error> {
        self.factories
            .get(id)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Load,
                    format!("observer {id:?} is not registered"),
                )
            })?
            .create(policy)
    }
}
