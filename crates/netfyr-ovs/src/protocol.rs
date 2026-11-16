use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{Value as Json, json};

use crate::topology::{Snapshot, decode};

const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// Failure categories independent of the host's Rust types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    PermissionDenied,
    Unreachable,
    Unsupported,
    Undecodable,
}

/// An observation fails as a whole; no partial topology is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}

impl Failure {
    pub(crate) fn unsupported(message: &str) -> Self {
        Self {
            kind: FailureKind::Unsupported,
            message: message.to_string(),
        }
    }

    pub(crate) fn undecodable(message: &str) -> Self {
        Self {
            kind: FailureKind::Undecodable,
            message: message.to_string(),
        }
    }
}

/// The guest's only I/O dependency; the host owns sockets and deadlines.
pub trait Transport {
    /// Write every byte or return an error.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Failure>;
    /// Return the next chunk; an empty chunk is EOF.
    fn read(&mut self) -> Result<Vec<u8>, Failure>;
}

fn request() -> Json {
    json!({
        "method": "transact",
        "params": [
            "Open_vSwitch",
            {"op": "select", "table": "Open_vSwitch", "where": [], "columns": ["_uuid", "bridges"]},
            {"op": "select", "table": "Bridge", "where": [], "columns": ["_uuid", "name", "ports", "datapath_type"]},
            {"op": "select", "table": "Port", "where": [], "columns": ["_uuid", "name", "interfaces"]},
            {"op": "select", "table": "Interface", "where": [], "columns": ["_uuid", "name", "type"]}
        ],
        "id": 1
    })
}

/// Read all four tables in one RFC 7047 select-only transaction.
pub fn discover(transport: &mut impl Transport) -> Result<Snapshot, Failure> {
    transport.write(request().to_string().as_bytes())?;
    let mut buffer = Vec::new();
    let mut received = 0;
    // Echo requests can arrive between any two messages. Bound the total work
    // even if a server continuously sends well-formed echo requests.
    for _ in 0..32 {
        let message = read_message(transport, &mut buffer, &mut received)?;
        if message.get("method").and_then(Json::as_str) == Some("echo") {
            if !message.get("params").is_some_and(Json::is_array)
                || message.get("id").is_none_or(Json::is_null)
            {
                return Err(Failure::undecodable("invalid OVSDB echo request"));
            }
            let reply = json!({"result": message["params"], "error": null, "id": message["id"]});
            transport.write(reply.to_string().as_bytes())?;
            continue;
        }
        if message.get("id") != Some(&json!(1)) || message.get("method").is_some() {
            return Err(Failure::undecodable(
                "unexpected OVSDB response ID or method",
            ));
        }
        let error = message
            .get("error")
            .ok_or_else(|| Failure::undecodable("OVSDB response has no error member"))?;
        if !error.is_null() {
            return Err(Failure::unsupported(
                "OVSDB rejected the Open_vSwitch transaction",
            ));
        }
        return decode(&message["result"]);
    }
    Err(Failure::undecodable(
        "too many OVSDB messages before the transaction reply",
    ))
}

fn read_message(
    transport: &mut impl Transport,
    buffer: &mut Vec<u8>,
    received: &mut usize,
) -> Result<Json, Failure> {
    loop {
        let mut stream = serde_json::Deserializer::from_slice(buffer).into_iter::<Json>();
        match stream.next() {
            Some(Ok(message)) => {
                let consumed = stream.byte_offset();
                buffer.drain(..consumed);
                return Ok(message);
            }
            Some(Err(error)) if !error.is_eof() => {
                return Err(Failure::undecodable("malformed OVSDB JSON reply"));
            }
            _ => {}
        }
        let chunk = transport.read()?;
        if chunk.is_empty() {
            return Err(Failure::undecodable(
                "OVSDB closed the stream before a complete reply",
            ));
        }
        *received = received.saturating_add(chunk.len());
        if *received > MAX_REPLY_BYTES {
            return Err(Failure::undecodable(&format!(
                "OVSDB reply exceeds {MAX_REPLY_BYTES} bytes"
            )));
        }
        buffer.extend_from_slice(&chunk);
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::vec;

    use super::*;

    struct MemoryTransport {
        reads: VecDeque<Vec<u8>>,
        writes: Vec<Json>,
    }

    impl Transport for MemoryTransport {
        fn write(&mut self, bytes: &[u8]) -> Result<(), Failure> {
            self.writes.push(serde_json::from_slice(bytes).unwrap());
            Ok(())
        }
        fn read(&mut self) -> Result<Vec<u8>, Failure> {
            Ok(self.reads.pop_front().unwrap_or_default())
        }
    }

    #[test]
    fn fragmented_stream_with_echo_uses_one_select_only_transaction() {
        let messages = concat!(
            r#"{"method":"echo","params":["alive"],"id":"probe"}"#,
            r#"{"id":1,"error":null,"result":[{"rows":[]},{"rows":[]},{"rows":[]},{"rows":[]}]}"#,
        );
        let mut transport = MemoryTransport {
            reads: messages.as_bytes().chunks(7).map(<[u8]>::to_vec).collect(),
            writes: vec![],
        };
        assert!(discover(&mut transport).unwrap().devices.is_empty());
        assert_eq!(transport.writes.len(), 2);
        assert_eq!(transport.writes[0]["method"], "transact");
        let params = transport.writes[0]["params"].as_array().unwrap();
        assert_eq!(params.len(), 5);
        for operation in &params[1..] {
            assert_eq!(operation["op"], "select");
            assert_eq!(operation["where"], json!([]));
            assert!(operation["columns"].is_array());
        }
        assert_eq!(
            transport.writes[1],
            json!({"result":["alive"],"error":null,"id":"probe"})
        );
    }

    #[test]
    fn invalid_or_truncated_replies_are_failures() {
        for reply in [
            r#"{"id":2,"error":null,"result":[]}"#,
            r#"{"id":1,"error":null,"result":["#,
            r#"{"id":1,"result":[]}"#,
            r#"not json"#,
            r#"{"id":1,"error":null,"result":[]}"#,
        ] {
            let mut transport = MemoryTransport {
                reads: vec![reply.as_bytes().to_vec()].into(),
                writes: vec![],
            };
            assert_eq!(
                discover(&mut transport).unwrap_err().kind,
                FailureKind::Undecodable,
                "{reply}"
            );
        }
    }
}
