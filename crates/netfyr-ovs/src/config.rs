use alloc::string::{String, ToString};

use crate::Failure;

/// Endpoint defaults and configuration decoding belong to the OVS provider,
/// not the generic plugin host. Configuration never grants socket access.
pub(crate) fn endpoint(config: &str) -> Result<String, Failure> {
    let config: serde_json::Value = serde_json::from_str(config)
        .map_err(|_| Failure::unsupported("invalid OVS configuration JSON"))?;
    let object = config
        .as_object()
        .ok_or_else(|| Failure::unsupported("OVS configuration must be an object"))?;
    match object.get("endpoint") {
        None => Ok("/run/openvswitch/db.sock".to_string()),
        Some(value) => value
            .as_str()
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .ok_or_else(|| Failure::unsupported("OVS endpoint must be a nonempty string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_owns_the_default_and_custom_endpoint() {
        assert_eq!(endpoint("{}").unwrap(), "/run/openvswitch/db.sock");
        assert_eq!(
            endpoint(r#"{"endpoint":"/private/db.sock"}"#).unwrap(),
            "/private/db.sock"
        );
    }

    #[test]
    fn invalid_configuration_does_not_fall_back_to_the_default() {
        for config in [
            "bad json",
            "[]",
            "null",
            r#"{"endpoint":null}"#,
            r#"{"endpoint":12}"#,
            r#"{"endpoint":""}"#,
        ] {
            assert!(endpoint(config).is_err(), "{config}");
        }
    }
}
