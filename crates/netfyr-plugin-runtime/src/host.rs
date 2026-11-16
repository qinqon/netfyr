use std::collections::BTreeSet;
use std::io;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use wasmtime::component::{Resource, ResourceTable};
use wasmtime::{StoreLimits, StoreLimitsBuilder};

use crate::bindings::netfyr::plugin::{host_log, host_unix_socket};
use crate::{LogRecord, Policy};

pub struct Connection(UnixStream);

pub(crate) struct Host {
    pub(crate) limits: StoreLimits,
    pub(crate) logs: Vec<LogRecord>,
    pub(crate) deadline: Instant,
    sockets: BTreeSet<String>,
    table: ResourceTable,
    io: tokio::runtime::Runtime,
    connections: usize,
    remaining_io: usize,
}

impl Host {
    pub(crate) fn new(policy: &Policy, deadline: Instant) -> io::Result<Self> {
        Ok(Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(64 * 1024 * 1024)
                .memories(4)
                .instances(16)
                .tables(8)
                .table_elements(16 * 1024)
                .trap_on_grow_failure(true)
                .build(),
            logs: Vec::new(),
            deadline,
            sockets: policy.socket_paths.iter().cloned().collect(),
            table: ResourceTable::new(),
            io: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            connections: 0,
            remaining_io: 8 * 1024 * 1024,
        })
    }

    fn charge(&mut self, bytes: usize) -> wasmtime::Result<()> {
        self.remaining_io = self
            .remaining_io
            .checked_sub(bytes)
            .ok_or_else(|| wasmtime::Error::msg("plugin exceeded its socket I/O budget"))?;
        Ok(())
    }

    fn check_deadline(&self) -> wasmtime::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(wasmtime::Error::msg("plugin query deadline exceeded"));
        }
        Ok(())
    }
}

impl host_log::Host for Host {
    fn log(&mut self, level: host_log::Level, message: String) -> wasmtime::Result<()> {
        self.check_deadline()?;
        if self.logs.len() >= 128 || message.len() > 4096 {
            return Err(wasmtime::Error::msg("plugin exceeded its logging budget"));
        }
        self.logs.push(LogRecord { level, message });
        Ok(())
    }
}

impl host_unix_socket::Host for Host {
    fn connect(
        &mut self,
        path: String,
    ) -> wasmtime::Result<Result<Resource<Connection>, host_unix_socket::ConnectError>> {
        use host_unix_socket::ConnectError;
        // Check the literal declared path before any filesystem or socket I/O.
        // The socket's directory is trusted host configuration; this is not a
        // symlink-safe filesystem sandbox against other local processes.
        if !self.sockets.contains(&path) {
            return Ok(Err(ConnectError::NotDeclared));
        }
        self.check_deadline()?;
        if self.connections >= 4 {
            return Err(wasmtime::Error::msg(
                "plugin exceeded its connection budget",
            ));
        }
        self.connections += 1;
        let result = self
            .io
            .block_on(async {
                tokio::time::timeout_at(self.deadline.into(), UnixStream::connect(&path)).await
            })
            .map_err(|_| wasmtime::Error::msg("plugin query deadline exceeded during connect"))?;
        Ok(match result {
            Ok(stream) => Ok(self.table.push(Connection(stream))?),
            Err(error) => Err(match error.kind() {
                io::ErrorKind::NotFound => ConnectError::NotFound,
                io::ErrorKind::PermissionDenied => ConnectError::PermissionDenied,
                io::ErrorKind::ConnectionRefused => ConnectError::Refused,
                _ => ConnectError::Other(error.to_string()),
            }),
        })
    }
}

impl host_unix_socket::HostConnection for Host {
    fn write(
        &mut self,
        connection: Resource<Connection>,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<(), String>> {
        self.check_deadline()?;
        self.charge(data.len())?;
        let stream = &mut self.table.get_mut(&connection)?.0;
        let result = self
            .io
            .block_on(async {
                tokio::time::timeout_at(self.deadline.into(), stream.write_all(&data)).await
            })
            .map_err(|_| wasmtime::Error::msg("plugin query deadline exceeded during write"))?;
        Ok(result.map_err(|error| error.to_string()))
    }

    fn read(
        &mut self,
        connection: Resource<Connection>,
        max: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, String>> {
        self.check_deadline()?;
        if max == 0 || max > 64 * 1024 {
            return Ok(Err(
                "read size must be between 1 and 65536 bytes".to_string()
            ));
        }
        let mut bytes = vec![0; max as usize];
        let stream = &mut self.table.get_mut(&connection)?.0;
        let result = self
            .io
            .block_on(async {
                tokio::time::timeout_at(self.deadline.into(), stream.read(&mut bytes)).await
            })
            .map_err(|_| wasmtime::Error::msg("plugin query deadline exceeded during read"))?;
        match result {
            Ok(count) => {
                self.charge(count)?;
                bytes.truncate(count);
                Ok(Ok(bytes))
            }
            Err(error) => Ok(Err(error.to_string())),
        }
    }

    fn drop(&mut self, connection: Resource<Connection>) -> wasmtime::Result<()> {
        self.table.delete(connection)?;
        Ok(())
    }
}
