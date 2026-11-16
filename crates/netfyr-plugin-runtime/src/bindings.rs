wasmtime::component::bindgen!({
    path: "wit",
    world: "observer",
    imports: { default: trappable },
    with: {
        "netfyr:plugin/host-unix-socket.connection": super::host::Connection,
    },
});
