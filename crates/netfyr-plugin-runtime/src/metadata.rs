//! Static metadata for the PoC, not the final netfyr manifest contract.
//!
//! Provider paths are relative to the provider descriptor. Installed metadata
//! embeds schemas and names a sibling native artifact by SHA-256. No guest code
//! executes during metadata loading. Digests detect mismatches, not authenticity:
//! the installed descriptor and artifact directory must both be trusted.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::{Error, ErrorKind, Policy};
pub(crate) use netfyr_plugin_api::{configuration_registry, validate_config};

const INTERFACE: &str = "netfyr:plugin/observer@0.1.0";
const MAX_METADATA: u64 = 4 * 1024 * 1024;

/// A provider ships this descriptor alongside its portable component and schemas.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderMetadata {
    format_version: u32,
    pub id: String,
    interface: String,
    schemas_file: String,
    pub(crate) config_schema: Json,
}

/// Self-contained metadata emitted locally by the precompiler. The native
/// artifact is the only sibling file needed when querying an installed plugin.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct InstalledMetadata {
    format_version: u32,
    pub id: String,
    interface: String,
    artifact: String,
    artifact_sha256: String,
    schemas: BTreeMap<String, Json>,
    config_schema: Json,
}

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::InvalidMetadata, message)
}

fn check_identity(version: u32, id: &str, interface: &str) -> Result<(), Error> {
    if version != 1 {
        return Err(invalid(format!(
            "unsupported metadata format-version {version}"
        )));
    }
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid(
            "plugin id must be a nonempty identifier of at most 128 bytes",
        ));
    }
    if interface != INTERFACE {
        return Err(invalid(format!(
            "unsupported plugin interface {interface:?}"
        )));
    }
    Ok(())
}

fn relative_path(text: &str) -> Result<&Path, Error> {
    let path = Path::new(text);
    if text.is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(invalid(
            "metadata paths must be relative without parent components",
        ));
    }
    Ok(path)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(invalid)?
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)
        .map_err(invalid)?;
    if bytes.len() as u64 > MAX_METADATA {
        return Err(invalid("metadata or schemas exceed 4 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|error| invalid(format!("{}: {error}", path.display())))
}

fn validate_schemas(schemas: &BTreeMap<String, Json>) -> Result<(), Error> {
    if schemas.is_empty() {
        return Err(invalid("plugin must declare at least one device schema"));
    }
    if schemas
        .values()
        .any(|schema| schema.get("type").and_then(Json::as_str) != Some("object"))
    {
        return Err(invalid("device schemas must describe objects"));
    }
    let mut policy = Policy::default();
    set_schemas(schemas, &mut policy);
    crate::registry(&policy).map_err(|error| invalid(error.message))?;
    Ok(())
}

fn set_schemas(schemas: &BTreeMap<String, Json>, policy: &mut Policy) {
    policy.schemas = schemas
        .iter()
        .map(|(name, schema)| (name.clone(), schema.to_string()))
        .collect();
}

/// SHA-256 is an integrity association, not a signature or a trust decision.
pub fn artifact_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl ProviderMetadata {
    /// Validate metadata and its schemas before any WebAssembly is executed.
    pub fn load(path: impl AsRef<Path>) -> Result<(Self, BTreeMap<String, Json>), Error> {
        let path = path.as_ref();
        let metadata: Self = read_json(path)?;
        check_identity(metadata.format_version, &metadata.id, &metadata.interface)?;
        let schemas = read_json(
            &path
                .parent()
                .unwrap_or(Path::new("."))
                .join(relative_path(&metadata.schemas_file)?),
        )?;
        validate_schemas(&schemas)?;
        configuration_registry(&metadata.config_schema)?;
        Ok((metadata, schemas))
    }
}

impl InstalledMetadata {
    /// Create local metadata after native precompilation.
    pub fn new(
        provider: ProviderMetadata,
        schemas: BTreeMap<String, Json>,
        artifact: &str,
        bytes: &[u8],
    ) -> Result<Self, Error> {
        let metadata = Self {
            format_version: provider.format_version,
            id: provider.id,
            interface: provider.interface,
            artifact: artifact.to_string(),
            artifact_sha256: artifact_digest(bytes),
            schemas,
            config_schema: provider.config_schema,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    fn validate(&self) -> Result<(), Error> {
        check_identity(self.format_version, &self.id, &self.interface)?;
        if relative_path(&self.artifact)?.components().count() != 1 {
            return Err(invalid("installed artifact must be a sibling filename"));
        }
        if self.artifact_sha256.len() != 64
            || !self
                .artifact_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("invalid artifact-sha256"));
        }
        validate_schemas(&self.schemas)?;
        self.configuration_registry()?;
        Ok(())
    }

    /// Reads static metadata only; does not deserialize or execute the artifact.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let metadata: Self = read_json(path.as_ref())?;
        metadata.validate()?;
        Ok(metadata)
    }

    pub(crate) fn configuration_registry(&self) -> Result<netfyr_state::SchemaRegistry, Error> {
        configuration_registry(&self.config_schema)
    }

    pub(crate) fn config_schema(&self) -> &Json {
        &self.config_schema
    }

    /// Validate one instance's config without deserializing or executing native code.
    pub fn validate_config(&self, config: &Json) -> Result<(), Error> {
        validate_config(&self.configuration_registry()?, config)
    }

    /// Schema/artifact fields are private after validation. The caller must still
    /// trust the metadata's origin before deserializing the returned bytes.
    pub(crate) fn artifact(&self, path: &Path, policy: &mut Policy) -> Result<Vec<u8>, Error> {
        let bytes = std::fs::read(path.parent().unwrap_or(Path::new(".")).join(&self.artifact))
            .map_err(invalid)?;
        if artifact_digest(&bytes) != self.artifact_sha256 {
            return Err(invalid(
                "artifact SHA-256 does not match installed metadata",
            ));
        }
        set_schemas(&self.schemas, policy);
        Ok(bytes)
    }

    pub fn to_json(&self) -> Result<String, Error> {
        let json = serde_json::to_string_pretty(self).map_err(invalid)? + "\n";
        if json.len() as u64 > MAX_METADATA {
            return Err(invalid("installed metadata exceeds 4 MiB"));
        }
        Ok(json)
    }
}

/// The generated descriptor sits beside the artifact, e.g. ovs.cwasm.json.
pub fn installed_metadata_path(artifact: &Path) -> PathBuf {
    let mut path = artifact.as_os_str().to_os_string();
    path.push(".json");
    path.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> InstalledMetadata {
        serde_json::from_value(serde_json::json!({
            "format-version":1, "id":"example", "interface":INTERFACE,
            "artifact":"example.cwasm", "artifact-sha256":artifact_digest(b"native"),
            "config-schema":{"type":"object","properties":{"endpoint":{"type":"string"}}},
            "schemas":{"example_device":{"type":"object","properties":{"uuid":{"type":"string"}}}}
        }))
        .unwrap()
    }

    #[test]
    fn installed_metadata_roundtrips_and_supplies_schemas() {
        let original = metadata();
        let json = original.to_json().unwrap();
        let restored: InstalledMetadata = serde_json::from_str(&json).unwrap();
        restored.validate().unwrap();
        let mut policy = Policy::default();
        set_schemas(&restored.schemas, &mut policy);
        assert!(policy.schemas.contains_key("example_device"));
        assert!(policy.socket_paths.is_empty());
        assert_eq!(json, restored.to_json().unwrap());
    }

    #[test]
    fn plugin_configuration_is_validated_without_executing_a_guest() {
        let metadata = metadata();
        assert!(metadata.validate_config(&serde_json::json!({})).is_ok());
        assert!(
            metadata
                .validate_config(&serde_json::json!({"endpoint":"/private/socket"}))
                .is_ok()
        );
        for config in [
            serde_json::json!({"endpoint":12}),
            serde_json::json!({"typo":true}),
            serde_json::json!([]),
        ] {
            assert!(metadata.validate_config(&config).is_err());
        }
    }

    #[test]
    fn incompatible_or_unsafe_metadata_is_rejected() {
        for (key, value) in [
            ("format-version", serde_json::json!(2)),
            ("id", serde_json::json!("../invalid")),
            (
                "interface",
                serde_json::json!("netfyr:plugin/observer@9.0.0"),
            ),
            ("artifact", serde_json::json!("../outside.cwasm")),
            ("artifact", serde_json::json!("/absolute.cwasm")),
            ("artifact-sha256", serde_json::json!("bad")),
            ("schemas", serde_json::json!({})),
            (
                "schemas",
                serde_json::json!({"example_device":{"type":"string"}}),
            ),
            (
                "schemas",
                serde_json::json!({"example_device":{"type":"invalid"}}),
            ),
            ("schemas", serde_json::json!({"ipv4":{"type":"object"}})),
        ] {
            let mut json = serde_json::to_value(metadata()).unwrap();
            json[key] = value;
            let metadata: InstalledMetadata = serde_json::from_value(json).unwrap();
            assert!(metadata.validate().is_err(), "{key}");
        }
    }

    #[test]
    fn digest_is_standard_sha256() {
        assert_eq!(
            artifact_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
