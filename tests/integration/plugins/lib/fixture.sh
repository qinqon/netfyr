# shellcheck shell=bash
# Source this helper and install ovs_fixture_cleanup as an EXIT trap first.
: "${repo_root:?set repo_root before sourcing the fixture helper}"
ovs_fixture_start() {
    ovs_engine="${CONTAINER_ENGINE:-podman}"
    command -v "$ovs_engine" >/dev/null || { echo "FAIL: $ovs_engine not available" >&2; return 1; }
    ovs_fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/netfyr-ovs.XXXXXX")"
    ovs_image="${NETFYR_OVS_IMAGE:-localhost/netfyr-ovs-poc:debian12}"
    if [ -z "${NETFYR_OVS_IMAGE:-}" ]; then
        "$ovs_engine" build -t "$ovs_image" -f "$repo_root/fixtures/ovsdb/Containerfile" \
            "$repo_root/fixtures/ovsdb"
    fi
    ovs_container="$("$ovs_engine" run -d --network=none --cap-drop=ALL \
        --security-opt=no-new-privileges -v "$ovs_fixture_dir:/fixture:Z" "$ovs_image")"
    for ((attempt = 0; attempt < 200; attempt++)); do
        [ ! -f "$ovs_fixture_dir/ready" ] || return 0
        sleep 0.05
    done
    "$ovs_engine" logs "$ovs_container" >&2 || true
    echo "FAIL: OVSDB fixture did not become ready" >&2
    return 1
}

ovs_fixture_cleanup() {
    if [ -n "${ovs_container:-}" ]; then
        "$ovs_engine" rm -f "$ovs_container" >/dev/null 2>&1 || true
    fi
    if [ -n "${ovs_fixture_dir:-}" ]; then
        rm -rf -- "$ovs_fixture_dir"
    fi
}

ovs_fixture_exec() {
    "$ovs_engine" exec "$ovs_container" "$@"
}
