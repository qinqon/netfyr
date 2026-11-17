//! Read-only OVSDB discovery for the WebAssembly plugin PoC.
//!
//! The protocol and topology decoder also build natively for pure unit tests.
//! The component uses only the two host interfaces in `plugin-query.wit`.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

#[cfg(any(test, target_arch = "wasm32"))]
mod config;
mod protocol;
mod topology;
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use protocol::{Failure, FailureKind, Transport, discover};
pub use topology::{Device, Diagnostic, Snapshot};

#[cfg(target_arch = "wasm32")]
mod component {
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use super::{Failure, FailureKind, Transport, discover};

    wit_bindgen::generate!({
        path: "../netfyr-plugin-runtime/wit",
        world: "observer",
    });

    use exports::netfyr::plugin::observation as wire;
    use netfyr::plugin::{host_log, host_unix_socket};

    struct Socket(host_unix_socket::Connection);

    impl Transport for Socket {
        fn write(&mut self, bytes: &[u8]) -> Result<(), Failure> {
            self.0.write(bytes).map_err(|message| Failure {
                kind: FailureKind::Unreachable,
                message,
            })
        }

        fn read(&mut self) -> Result<Vec<u8>, Failure> {
            self.0.read(16 * 1024).map_err(|message| Failure {
                kind: FailureKind::Unreachable,
                message,
            })
        }
    }

    struct Ovs;

    impl wire::Guest for Ovs {
        fn query(config_json: String) -> Result<wire::Snapshot, wire::Failure> {
            query(&config_json).map_err(|failure| wire::Failure {
                kind: match failure.kind {
                    FailureKind::PermissionDenied => wire::FailureKind::PermissionDenied,
                    FailureKind::Unreachable => wire::FailureKind::Unreachable,
                    FailureKind::Unsupported => wire::FailureKind::Unsupported,
                    FailureKind::Undecodable => wire::FailureKind::Undecodable,
                },
                message: failure.message,
            })
        }
    }

    fn query(config: &str) -> Result<wire::Snapshot, Failure> {
        let endpoint = crate::config::endpoint(config)?;
        query_endpoint(&endpoint).map_err(|failure| Failure {
            kind: failure.kind,
            message: format!("OVSDB endpoint {endpoint:?}: {}", failure.message),
        })
    }

    fn query_endpoint(endpoint: &str) -> Result<wire::Snapshot, Failure> {
        host_log::log(host_log::Level::Debug, "reading an OVSDB snapshot");
        let connection = host_unix_socket::connect(endpoint).map_err(|error| {
            use host_unix_socket::ConnectError;
            let (kind, message) = match error {
                ConnectError::NotDeclared => (
                    FailureKind::PermissionDenied,
                    "socket path is not declared".to_string(),
                ),
                ConnectError::PermissionDenied => (
                    FailureKind::PermissionDenied,
                    "permission denied opening OVSDB socket".to_string(),
                ),
                ConnectError::NotFound => (
                    FailureKind::Unreachable,
                    "OVSDB socket not found".to_string(),
                ),
                ConnectError::Refused => (
                    FailureKind::Unreachable,
                    "OVSDB connection refused".to_string(),
                ),
                ConnectError::Other(message) => (FailureKind::Unreachable, message),
            };
            Failure { kind, message }
        })?;
        let snapshot = discover(&mut Socket(connection))?;
        Ok(wire::Snapshot {
            devices: snapshot
                .devices
                .into_iter()
                .map(|device| wire::Device {
                    name: device.name,
                    device_type: device.device_type,
                    fields_json: device.fields_json,
                })
                .collect(),
            diagnostics: snapshot
                .diagnostics
                .into_iter()
                .map(|diagnostic| wire::Diagnostic {
                    device_name: diagnostic.device_name,
                    field: diagnostic.field,
                    message: diagnostic.message,
                })
                .collect(),
        })
    }

    export!(Ovs);
}
