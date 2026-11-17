# netfyr

Declarative network configuration for Linux, applied over netlink.

You describe the network state you want in YAML. netfyr reads the current state from
the kernel, works out the difference, and applies it over netlink. The query and apply
paths need no daemon.

YAML values are decoded using the embedded schema at each property path. For
example, an address-looking interface name remains a string, while an
`ipv4.addresses[].ip` value uses the declared CIDR format.

Work here is driven spec by spec from
[netfyr/specs](https://github.com/netfyr/specs).

## Build and test

```
cargo build                    # compile the workspace
cargo test                     # unit tests
make test                      # shell integration tests
make test TAGS=ipv4,routing    # integration tests tagged ipv4 OR routing
make test TAGS=ipv4+routing    # integration tests tagged ipv4 AND routing
```

The experimental [OVS WebAssembly plugin PoC](docs/plugin-poc.md) adds a read-only
query example and a separate install-time native precompiler. Providers ship a
portable `.wasm`; the runtime-only host loads its locally precompiled `.cwasm`.
Static plugin metadata supplies identity and schemas automatically; installation
emits a descriptor associating them with the native artifact's digest.
The example discovers instance settings from `plugin.d` tiers under `/usr/lib`,
`/etc` and `/run/netfyr`, with isolated configuration and grants for each instance.
The Wasm-specific crates require Rust 1.96 or newer.
The Wasmtime-independent `netfyr-plugin-api` defines the common observer
contract and validation. Wasm is the only production loader; a future trusted
native implementation can fit behind the same API.
Run `make test-plugins` for the component integration tests against a private
`ovs-sandbox` fixture (requires `wasm32-wasip2`, Podman or Docker, and Python PyYAML).

## API documentation

Run `make docs` to generate the library API reference. Open
`target/doc/netfyr_state/index.html`; use the Crates sidebar to switch libraries.
User guides and configuration reference are at [docs.netfyr.io](https://docs.netfyr.io/).

## License

MIT. See [LICENSE](LICENSE).
