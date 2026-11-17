//! Host-local observer instances. Low-to-high tiers replace whole files by
//! instance name; configuration and grants never merge across tiers or instances.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use netfyr_plugin_api::ObserverRegistry;
use netfyr_state::{Match, SchemaRegistry};
use serde::Deserialize;
use serde_json::Value as Json;

use crate::metadata::InstalledMetadata;
use crate::{Error, ErrorKind, Policy, Snapshot, WasmFactory};

/// Directories are processed low to high, matching netfyr's policy tier order.
pub const DEFAULT_CONFIG_DIRS: [&str; 3] = [
    "/usr/lib/netfyr/plugin.d",
    "/etc/netfyr/plugin.d",
    "/run/netfyr/plugin.d",
];
pub const DEFAULT_PLUGIN_DIR: &str = "/usr/lib/netfyr/plugins";
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Grants {
    #[serde(default)]
    pub unix_sockets: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct FileConfig {
    plugin: Option<String>,
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default = "empty_config")]
    config: Json,
    #[serde(default)]
    grants: Grants,
    #[serde(default = "timeout_ms")]
    timeout_ms: u64,
}

fn enabled() -> bool {
    true
}
fn timeout_ms() -> u64 {
    5000
}
fn empty_config() -> Json {
    serde_json::json!({})
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && !matches!(text, "." | "..")
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn invalid(path: &Path, message: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::InvalidConfig,
        format!("{}: {message}", path.display()),
    )
}

/// Resolved host configuration for one enabled instance.
pub struct Instance {
    name: String,
    plugin: String,
    origin: PathBuf,
    config: Json,
    grants: Grants,
    timeout: Duration,
}

/// Collect file paths first, then parse only winning files. A disabled higher-tier
/// file can replace a broken lower-tier definition without opening it.
pub fn load(directories: &[PathBuf]) -> Result<Vec<Instance>, Error> {
    let mut winners = BTreeMap::new();
    for directory in directories {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(invalid(directory, error)),
        };
        let mut tier = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|error| invalid(directory, error))?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.')
                || !matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yaml" | "yml")
                )
            {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !identifier(name) {
                return Err(invalid(&path, "invalid instance filename"));
            }
            if !entry
                .file_type()
                .map_err(|error| invalid(&path, error))?
                .is_file()
            {
                return Err(invalid(
                    &path,
                    "instance drop-ins must be regular files, not symlinks or directories",
                ));
            }
            if tier.insert(name.to_string(), path.clone()).is_some() {
                return Err(invalid(&path, "duplicate instance name within one tier"));
            }
        }
        winners.extend(tier);
    }
    winners
        .into_iter()
        .filter_map(|(name, path)| match read_instance(name, &path) {
            Ok(Some(instance)) => Some(Ok(instance)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn read_instance(name: String, path: &Path) -> Result<Option<Instance>, Error> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|error| invalid(path, error))?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(path, error))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid(path, "instance file exceeds 64 KiB"));
    }
    // Do not echo malformed configuration values (which may contain secrets).
    let config: FileConfig = serde_yaml::from_slice(&bytes).map_err(|_| {
        invalid(
            path,
            "invalid instance YAML, field type, duplicate or unknown field",
        )
    })?;
    if !config.enabled {
        return Ok(None);
    }
    let plugin = config
        .plugin
        .filter(|id| identifier(id))
        .ok_or_else(|| invalid(path, "enabled instance needs a valid plugin identifier"))?;
    if !config.config.is_object() {
        return Err(invalid(path, "config must be an object"));
    }
    if config.config.to_string().len() > 64 * 1024 {
        return Err(invalid(path, "serialized config exceeds 64 KiB"));
    }
    if config.timeout_ms == 0 || config.timeout_ms > 60_000 {
        return Err(invalid(path, "timeout-ms must be in 1..=60000"));
    }
    let mut seen = BTreeSet::new();
    for socket in &config.grants.unix_sockets {
        if !Path::new(socket).is_absolute() || socket.contains('\0') || !seen.insert(socket) {
            return Err(invalid(
                path,
                "unix-sockets grants must be unique absolute paths",
            ));
        }
    }
    Ok(Some(Instance {
        name,
        plugin,
        origin: path.to_path_buf(),
        config: config.config,
        grants: config.grants,
        timeout: Duration::from_millis(config.timeout_ms),
    }))
}

/// Validated against installed metadata, but not yet deserialized or executed.
pub struct PreparedInstance {
    instance: Instance,
    descriptor: PathBuf,
}

/// Validate every enabled instance before any guest executes. This reads static
/// metadata and schemas only. plugin_dir must contain trusted precompiler output.
pub fn prepare(
    instances: Vec<Instance>,
    plugin_dir: &Path,
) -> Result<Vec<PreparedInstance>, Error> {
    instances
        .into_iter()
        .map(|instance| {
            let descriptor = plugin_dir.join(format!("{}.cwasm.json", instance.plugin));
            let check = || {
                let metadata = InstalledMetadata::load(&descriptor)?;
                if metadata.id != instance.plugin {
                    return Err(invalid(
                        &descriptor,
                        "installed identity does not match configured plugin",
                    ));
                }
                metadata.validate_config(&instance.config)
            };
            check().map_err(|error| {
                invalid(
                    &instance.origin,
                    format!("instance {:?}: {error}", instance.name),
                )
            })?;
            Ok(PreparedInstance {
                instance,
                descriptor,
            })
        })
        .collect()
}

pub struct InstanceObservation {
    pub snapshot: Snapshot,
    pub schema_registry: Arc<SchemaRegistry>,
}

pub struct InstanceReport {
    pub name: String,
    pub plugin: String,
    pub result: Result<InstanceObservation, Error>,
}

impl PreparedInstance {
    /// Execute with a new plugin/store and this instance's policy only.
    ///
    /// The registry binds trusted implementation factories. This orchestration
    /// uses only the common observer contract, not Wasmtime types or WIT calls.
    pub fn query(self, registry: &ObserverRegistry, selector: &Match) -> InstanceReport {
        let result = (|| {
            let policy = Policy {
                socket_paths: self.instance.grants.unix_sockets,
                timeout: self.instance.timeout,
                ..Policy::default()
            };
            let mut plugin = registry.create(&self.instance.plugin, policy)?;
            let snapshot = plugin.query(&self.instance.config.to_string(), selector)?;
            Ok(InstanceObservation {
                snapshot,
                schema_registry: plugin.schemas(),
            })
        })();
        InstanceReport {
            name: self.instance.name,
            plugin: self.instance.plugin,
            result,
        }
    }
}

/// Register each installed plugin once. WebAssembly remains the only production
/// loader; a future approved native factory can satisfy the same registry API.
///
/// # Safety
/// Descriptors/artifacts must remain trusted compiler output for the lifetime
/// of the returned registry. Drop-ins must be controlled by authorized operators.
pub unsafe fn observer_registry(instances: &[PreparedInstance]) -> Result<ObserverRegistry, Error> {
    let mut registry = ObserverRegistry::default();
    for prepared in instances {
        let id = &prepared.instance.plugin;
        if !registry.contains(id) {
            // SAFETY: this method requires trust in the installed artifacts.
            let factory = unsafe { WasmFactory::new(id.clone(), prepared.descriptor.clone()) };
            registry.register(id.clone(), factory)?;
        }
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_do_not_grant_capabilities() {
        let config: FileConfig = serde_yaml::from_str("plugin: example").unwrap();
        assert!(config.enabled);
        assert_eq!(config.config, serde_json::json!({}));
        assert!(config.grants.unix_sockets.is_empty());
        assert_eq!(config.timeout_ms, 5000);
    }

    #[test]
    fn unknown_and_duplicate_control_fields_are_rejected() {
        for yaml in [
            "plugin: a\nplugin: b",
            "plugin: a\ngrant: {}",
            "enabled: nope",
            "grants: {unknown: []}",
        ] {
            assert!(serde_yaml::from_str::<FileConfig>(yaml).is_err());
        }
    }

    #[test]
    fn plugin_settings_and_grants_are_separate_values() {
        let config: FileConfig = serde_yaml::from_str("plugin: example\nconfig: {endpoint: /tmp/a.sock}\ngrants: {unix-sockets: [/tmp/b.sock]}").unwrap();
        assert_eq!(config.config["endpoint"], "/tmp/a.sock");
        assert_eq!(config.grants.unix_sockets, ["/tmp/b.sock"]);
    }
}
