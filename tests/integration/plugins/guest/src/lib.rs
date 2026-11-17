#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[path = "../../../../../crates/netfyr-ovs/src/wasm.rs"]
mod wasm;

#[cfg(target_arch = "wasm32")]
mod component {
    extern crate alloc;
    use alloc::string::{String, ToString};
    use alloc::vec;

    wit_bindgen::generate!({
        path: "../../../../crates/netfyr-plugin-runtime/wit",
        world: "observer",
    });
    use exports::netfyr::plugin::observation::*;

    struct Fixture;

    impl Guest for Fixture {
        fn query(config: String) -> Result<Snapshot, Failure> {
            let config: serde_json::Value = serde_json::from_str(&config).map_err(|_| Failure {
                kind: FailureKind::Undecodable,
                message: "invalid configuration".into(),
            })?;
            match config
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("ok")
            {
                "ok" => Ok(Snapshot {
                    devices: vec![Device {
                        name: "example0".into(),
                        device_type: "example_device".into(),
                        fields_json: serde_json::json!({"example_device":{"value":config.get("endpoint").and_then(serde_json::Value::as_str).unwrap_or("0123")}}).to_string(),
                    }],
                    diagnostics: vec![],
                }),
                "trap" => core::arch::wasm32::unreachable(),
                "loop" => loop {
                    core::hint::spin_loop();
                },
                "bad-schema" => Ok(Snapshot {
                    devices: vec![Device {
                        name: "example0".into(),
                        device_type: "example_device".into(),
                        fields_json: r#"{"example_device":{"value":12}}"#.into(),
                    }],
                    diagnostics: vec![],
                }),
                _ => Err(Failure {
                    kind: FailureKind::Unsupported,
                    message: "unknown fixture mode".into(),
                }),
            }
        }
    }

    export!(Fixture);
}
