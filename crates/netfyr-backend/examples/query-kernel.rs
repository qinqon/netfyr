//! Baseline for comparing one-shot queries with and without a plugin runtime.

use netfyr_backend::{NetlinkBackend, NetworkBackend};
use netfyr_state::SchemaRegistry;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = NetlinkBackend::open("/proc/self/ns/net")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let observation = runtime.block_on(backend.query_all())?;
    let states = observation
        .devices
        .into_iter()
        .map(|device| device.state)
        .collect::<Vec<_>>();
    print!("{}", SchemaRegistry::new().to_yaml_query(&states)?);
    for diagnostic in observation.diagnostics {
        eprintln!("kernel: {}", diagnostic.message);
    }
    Ok(())
}
