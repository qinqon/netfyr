use std::collections::BTreeSet;
use std::sync::Arc;

use netfyr_state::{Match, SchemaRegistry, Source, State, Value};
use serde_json::Value as Json;

use crate::{Error, ErrorKind, Observation, Policy, Snapshot};

pub struct Contract {
    policy: Policy,
    registry: Arc<SchemaRegistry>,
    config_registry: Option<SchemaRegistry>,
}

impl Contract {
    pub fn new(policy: Policy, config_schema: Option<&Json>) -> Result<Self, Error> {
        let registry = Arc::new(Self::registry(&policy)?);
        Ok(Self {
            policy,
            registry,
            config_registry: config_schema.map(configuration_registry).transpose()?,
        })
    }

    pub fn registry(policy: &Policy) -> Result<SchemaRegistry, Error> {
        if policy.timeout.is_zero()
            || policy.timeout > std::time::Duration::from_secs(60)
            || policy.fuel == 0
        {
            return Err(Error::new(
                ErrorKind::InvalidPolicy,
                "timeout must be in (0, 60s] and fuel must be nonzero",
            ));
        }
        let mut registry = SchemaRegistry::new();
        for (kind, schema) in &policy.schemas {
            if kind.is_empty()
                || !kind
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(Error::new(
                    ErrorKind::InvalidPolicy,
                    "device types must be nonempty identifiers",
                ));
            }
            registry
                .add_schema_node("base", kind, schema)
                .map_err(|error| Error::new(ErrorKind::InvalidPolicy, error))?;
        }
        Ok(registry)
    }

    pub fn set_config_schema(&mut self, schema: &Json) -> Result<(), Error> {
        self.config_registry = Some(configuration_registry(schema)?);
        Ok(())
    }
    pub fn schema_registry(&self) -> &SchemaRegistry {
        &self.registry
    }
    pub fn schemas(&self) -> Arc<SchemaRegistry> {
        Arc::clone(&self.registry)
    }

    pub fn check_config(&self, text: &str) -> Result<(), Error> {
        if text.len() > 64 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "configuration exceeds 64 KiB",
            ));
        }
        if let Some(registry) = &self.config_registry {
            let config: Json = serde_json::from_str(text).map_err(|_| {
                Error::new(ErrorKind::InvalidConfig, "configuration is not valid JSON")
            })?;
            validate_config(registry, &config)?;
        }
        Ok(())
    }

    pub fn validate(&self, output: Observation, selector: &Match) -> Result<Snapshot, Error> {
        let invalid = |message| Error::new(ErrorKind::InvalidOutput, message);
        if output.devices.len() > 4096 || output.diagnostics.len() > 256 || output.logs.len() > 128
        {
            return Err(invalid(
                "too many devices, diagnostics or log records".to_string(),
            ));
        }
        for diagnostic in &output.diagnostics {
            if diagnostic.message.len() > 4096
                || diagnostic
                    .device_name
                    .as_ref()
                    .is_some_and(|s| s.len() > 1024)
                || diagnostic.field.as_ref().is_some_and(|s| s.len() > 1024)
            {
                return Err(invalid("oversized diagnostic".into()));
            }
        }
        if output.logs.iter().any(|log| log.message.len() > 4096) {
            return Err(invalid("oversized log record".into()));
        }
        let mut identities = BTreeSet::new();
        let mut states = Vec::new();
        let mut bytes = 0usize;
        for device in output.devices {
            bytes = bytes.saturating_add(device.fields_json.len());
            if bytes > 4 * 1024 * 1024 || device.name.is_empty() || device.name.len() > 1024 {
                return Err(invalid("oversized result or invalid device name".into()));
            }
            if !self.policy.schemas.contains_key(&device.device_type) {
                return Err(invalid(format!(
                    "undeclared device type {:?}",
                    device.device_type
                )));
            }
            if !identities.insert((device.device_type.clone(), device.name.clone())) {
                return Err(invalid("duplicate device identity".into()));
            }
            let fields: Json = serde_json::from_str(&device.fields_json)
                .map_err(|_| invalid("fields-json is not valid JSON".into()))?;
            let fields = fields
                .as_object()
                .ok_or_else(|| invalid("fields-json must be an object".into()))?;
            if fields.len() != 1 || !fields.get(&device.device_type).is_some_and(Json::is_object) {
                return Err(invalid(
                    "fields-json must contain only the owned device-type sub-object".into(),
                ));
            }
            let mut state = State::new(Source::Kernel);
            state.device_type = device.device_type;
            state.match_spec = Match {
                name: Some(device.name.clone()),
                r#type: Some(state.device_type.clone()),
                ..Match::default()
            };
            state
                .fields
                .insert("name".into(), Value::String(device.name));
            for (key, value) in fields {
                state
                    .fields
                    .insert(key.clone(), value_from_json(value, 0).map_err(invalid)?);
            }
            let errors = self.registry.validate(&state);
            if !errors.is_empty() {
                return Err(invalid(
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                ));
            }
            // Validate every row before filtering; selectors cannot hide invalid output.
            states.push(state);
        }
        states.retain(|state| selector.matches(&state.match_spec));
        Ok(Snapshot {
            states,
            diagnostics: output.diagnostics,
            logs: output.logs,
        })
    }
}

pub fn configuration_registry(schema: &Json) -> Result<SchemaRegistry, Error> {
    if schema.get("type").and_then(Json::as_str) != Some("object") {
        return Err(Error::new(
            ErrorKind::InvalidMetadata,
            "config-schema must describe an object",
        ));
    }
    let mut registry = SchemaRegistry::new();
    registry
        .add_schema_node("base", "plugin_config", &schema.to_string())
        .map_err(|error| Error::new(ErrorKind::InvalidMetadata, error))?;
    Ok(registry)
}

pub fn validate_config(registry: &SchemaRegistry, config: &Json) -> Result<(), Error> {
    let mut state = State::new(Source::Kernel);
    state.fields.insert(
        "plugin_config".into(),
        value_from_json(config, 0).map_err(|_| {
            Error::new(
                ErrorKind::InvalidConfig,
                "configuration contains unsupported values",
            )
        })?,
    );
    let errors = registry.validate(&state);
    if !errors.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    Ok(())
}

pub fn value_from_json(value: &Json, depth: usize) -> Result<Value, String> {
    if depth > 64 {
        return Err("JSON exceeds the nesting limit".into());
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
                return Err("floating-point values are not supported".into());
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
        Json::Null => return Err("null values are not supported".into()),
    })
}
