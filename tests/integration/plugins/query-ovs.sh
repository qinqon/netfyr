#!/bin/bash
# tags: ovs plugins wasm
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$repo_root"
target_dir="${NETFYR_TARGET_DIR:-$repo_root/target}"
command -v cargo >/dev/null || { echo "FAIL: cargo not available" >&2; exit 1; }
command -v python3 >/dev/null || { echo "FAIL: python3 not available" >&2; exit 1; }
python3 -c 'import yaml' || { echo "FAIL: Python PyYAML not available" >&2; exit 1; }

source "$repo_root/tests/integration/plugins/lib/fixture.sh"
listener=""
cleanup() {
    if [ -n "$listener" ]; then
        kill "$listener" 2>/dev/null || true
        wait "$listener" 2>/dev/null || true
    fi
    ovs_fixture_cleanup
}
trap cleanup EXIT

cargo build --locked -p netfyr-plugin-runtime --example query-with-plugins --target-dir "$target_dir"
cargo build --locked -p netfyr-plugin-precompile --target-dir "$target_dir"
cargo build --locked --release --target wasm32-wasip2 -p netfyr-ovs -p netfyr-plugin-test-guest --target-dir "$target_dir/guest"
query="$target_dir/debug/examples/query-with-plugins"
compiler="$target_dir/debug/netfyr-plugin-precompile"
guest="$target_dir/guest/wasm32-wasip2/release/netfyr_ovs.wasm"
bad_guest="$target_dir/guest/wasm32-wasip2/release/netfyr_plugin_test_guest.wasm"

ovs_fixture_start
endpoint="$ovs_fixture_dir/sandbox/db.sock"
ovs_fixture_exec ovsdb-server --version
ovs_fixture_exec ovsdb-client dump unix:/fixture/sandbox/db.sock Open_vSwitch >"$ovs_fixture_dir/before"
"$query" --plugin "$guest" --ovsdb "$endpoint" >"$ovs_fixture_dir/query.yaml"
"$query" --plugin "$guest" --ovsdb "$endpoint" --no-kernel >"$ovs_fixture_dir/again.yaml"
ovs_fixture_exec ovsdb-client dump unix:/fixture/sandbox/db.sock Open_vSwitch >"$ovs_fixture_dir/after"
cmp "$ovs_fixture_dir/before" "$ovs_fixture_dir/after"
for table in Bridge Port Interface; do
    ovs_fixture_exec ovs-vsctl --db=unix:/fixture/sandbox/db.sock --format=json --columns=_uuid,name list "$table" \
        >"$ovs_fixture_dir/$table.json"
done

python3 - "$ovs_fixture_dir" <<'PY'
import json
import pathlib
import sys
import yaml

root = pathlib.Path(sys.argv[1])
kernel, rows = list(yaml.safe_load_all((root / "query.yaml").read_text()))
kernel = kernel if isinstance(kernel, list) else [kernel]
assert any(row["name"] == "lo" for row in kernel), kernel
assert rows == yaml.safe_load((root / "again.yaml").read_text())
devices = {(row["type"], row["name"]): row for row in rows}
assert len(devices) == 11, devices
for table, kind in [("Bridge", "ovs_bridge"), ("Port", "ovs_port"), ("Interface", "ovs_interface")]:
    observed = json.loads((root / f"{table}.json").read_text())
    expected = {(kind, row[1]): row[0][1] for row in observed["data"]}
    actual = {key: row[kind]["uuid"] for key, row in devices.items() if key[0] == kind}
    assert actual == expected, (actual, expected)
assert devices["ovs_bridge", "br0"]["ovs_bridge"]["ports"] == ["bond0", "br0", "eth1", "gre0"]
assert devices["ovs_port", "bond0"]["ovs_port"]["interfaces"] == ["eth2", "eth3"]
assert devices["ovs_interface", "eth2"]["ovs_interface"]["port"] == "bond0"
assert devices["ovs_interface", "eth2"]["ovs_interface"]["bridge"] == "br0"
assert devices["ovs_interface", "eth1"]["ovs_interface"]["type"] == "system"
assert devices["ovs_interface", "br0"]["ovs_interface"]["type"] == "internal"
assert devices["ovs_interface", "gre0"]["ovs_interface"]["type"] == "gre"
PY

"$compiler" "$guest" --output "$ovs_fixture_dir/ovs.cwasm"
"$query" --trusted-precompiled "$ovs_fixture_dir/ovs.cwasm" --ovsdb "$endpoint" --no-kernel >"$ovs_fixture_dir/precompiled.yaml"
cmp "$ovs_fixture_dir/again.yaml" "$ovs_fixture_dir/precompiled.yaml"
"$query" --plugin "$guest" --ovsdb "$endpoint" --name br0 --no-kernel >"$ovs_fixture_dir/selected.yaml"
python3 - "$ovs_fixture_dir/selected.yaml" <<'PY'
import sys, yaml
rows = yaml.safe_load(open(sys.argv[1]))
assert [row["type"] for row in rows] == ["ovs_bridge", "ovs_port", "ovs_interface"]
PY

expect_failure() {
    local expected="$1"
    shift
    if "$query" "$@" >"$ovs_fixture_dir/failure.yaml" 2>"$ovs_fixture_dir/failure.log"; then
        echo "FAIL: query unexpectedly succeeded ($expected)" >&2
        return 1
    fi
    grep -F "$expected" "$ovs_fixture_dir/failure.log" || { cat "$ovs_fixture_dir/failure.log" >&2; return 1; }
}

expect_failure 'not declared' --plugin "$guest" --ovsdb "$endpoint" --allow-socket "$ovs_fixture_dir/another.sock"
# Every failure still presents the successful kernel observation.
grep -q 'name: lo' "$ovs_fixture_dir/failure.yaml"
expect_failure 'not found' --plugin "$guest" --ovsdb "$ovs_fixture_dir/missing.sock"
grep -q 'name: lo' "$ovs_fixture_dir/failure.yaml"
python3 - "$ovs_fixture_dir/refused.sock" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.close()
PY
expect_failure 'refused' --plugin "$guest" --ovsdb "$ovs_fixture_dir/refused.sock"
expect_failure 'Trap' --plugin "$bad_guest" --config-json trap
grep -q 'name: lo' "$ovs_fixture_dir/failure.yaml"
expect_failure 'InvalidOutput' --plugin "$bad_guest" --config-json bad-schema
expect_failure 'DeadlineExceeded' --plugin "$bad_guest" --config-json loop --timeout-ms 1

# Keep the component binary valid while changing its declared ABI major.
python3 - "$guest" "$ovs_fixture_dir/incompatible.wasm" <<'PY'
import pathlib, sys
component = pathlib.Path(sys.argv[1]).read_bytes()
assert b"netfyr:plugin/host-log@0.1.0" in component
pathlib.Path(sys.argv[2]).write_bytes(component.replace(b"@0.1.0", b"@9.0.0"))
PY
expect_failure 'host-log@9.0.0' --plugin "$ovs_fixture_dir/incompatible.wasm" --ovsdb "$endpoint"

python3 - "$ovs_fixture_dir/stalled.sock" <<'PY' &
import pathlib, select, socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.listen()
pathlib.Path(sys.argv[1] + ".ready").touch()
c, _ = s.accept()
select.select([], [], [], 30)
PY
listener=$!
for ((attempt = 0; attempt < 100; attempt++)); do
    [ ! -f "$ovs_fixture_dir/stalled.sock.ready" ] || break
    sleep 0.01
done
[ -f "$ovs_fixture_dir/stalled.sock.ready" ] || { echo "FAIL: stalled listener did not start" >&2; exit 1; }
expect_failure 'DeadlineExceeded' --trusted-precompiled "$ovs_fixture_dir/ovs.cwasm" --ovsdb "$ovs_fixture_dir/stalled.sock" --timeout-ms 100

# A fresh one-shot query still succeeds after the failure cases.
"$query" --trusted-precompiled "$ovs_fixture_dir/ovs.cwasm" --ovsdb "$endpoint" --no-kernel >"$ovs_fixture_dir/final.yaml"
cmp "$ovs_fixture_dir/again.yaml" "$ovs_fixture_dir/final.yaml"
echo "OVS component discovery, read-only transaction, capability denial, failures and precompilation passed"
