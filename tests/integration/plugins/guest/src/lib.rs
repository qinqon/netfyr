#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[path = "../../../../../crates/netfyr-ovs/src/wasm.rs"]
mod wasm;

#[cfg(target_arch = "wasm32")]
mod component {
    extern crate alloc;
    use alloc::string::String;
    use alloc::vec;

    wit_bindgen::generate!({
        path: "../../../../crates/netfyr-plugin-runtime/wit",
        world: "observer",
    });
    use exports::netfyr::plugin::observation::*;

    struct Fixture;

    impl Guest for Fixture {
        fn query(config: String) -> Result<Snapshot, Failure> {
            match config.as_str() {
                "trap" => core::arch::wasm32::unreachable(),
                "loop" => loop {
                    core::hint::spin_loop();
                },
                "bad-schema" => Ok(Snapshot {
                    devices: vec![Device {
                        name: "br0".into(),
                        device_type: "ovs_bridge".into(),
                        fields_json:
                            r#"{"ovs_bridge":{"uuid":12,"datapath_type":"system","ports":[]}}"#
                                .into(),
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
