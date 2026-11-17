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

# Build the install-time compiler and runtime-only host independently so Cargo
# cannot unify the compiler into the query executable.
cargo build --locked -p netfyr-plugin-precompile --target-dir "$target_dir"
cargo build --locked -p netfyr-plugin-runtime --no-default-features --example query-with-plugins --target-dir "$target_dir/plugin-native"
cargo build --locked --release --target wasm32-wasip2 -p netfyr-ovs -p netfyr-plugin-test-guest --target-dir "$target_dir/guest"
query="$target_dir/plugin-native/debug/examples/query-with-plugins"
compiler="$target_dir/debug/netfyr-plugin-precompile"
guest="$target_dir/guest/wasm32-wasip2/release/netfyr_ovs.wasm"
bad_guest="$target_dir/guest/wasm32-wasip2/release/netfyr_plugin_test_guest.wasm"

ovs_fixture_start
artifact="$ovs_fixture_dir/ovs.cwasm.json"
failure_artifact="$ovs_fixture_dir/failure.cwasm.json"
"$compiler" "$guest" --metadata "$repo_root/crates/netfyr-ovs/plugin.json" --output "${artifact%.json}"
"$compiler" "$bad_guest" --metadata "$repo_root/tests/integration/plugins/guest/plugin.json" --output "${failure_artifact%.json}"
# A different provider works without OVS configuration or socket grants.
"$query" --trusted-precompiled "$failure_artifact" --no-kernel >"$ovs_fixture_dir/example.yaml"
python3 - "$ovs_fixture_dir/example.yaml" <<'PY'
import pathlib, sys, yaml
text = pathlib.Path(sys.argv[1]).read_text()
assert '# source "example"' in text
row = yaml.safe_load(text)
assert row["type"] == "example_device"
assert row["name"] == "example0"
assert row["example_device"]["value"] == "0123"
PY
endpoint="$ovs_fixture_dir/sandbox/db.sock"
# OVS configuration is assembled by the test, not interpreted by the host.
config="$(python3 - "$endpoint" <<'PY'
import json, sys
print(json.dumps({"endpoint": sys.argv[1]}))
PY
)"
ovs_args=(--config-json "$config" --allow-socket "$endpoint")
ovs_fixture_exec ovsdb-server --version
ovs_fixture_exec ovsdb-client dump unix:/fixture/sandbox/db.sock Open_vSwitch >"$ovs_fixture_dir/before"
"$query" --trusted-precompiled "$artifact" "${ovs_args[@]}" >"$ovs_fixture_dir/query.yaml"
"$query" --trusted-precompiled "$artifact" "${ovs_args[@]}" --no-kernel >"$ovs_fixture_dir/again.yaml"
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

"$query" --trusted-precompiled "$artifact" "${ovs_args[@]}" --name br0 --no-kernel >"$ovs_fixture_dir/selected.yaml"
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

expect_failure 'runtime-only build needs' --plugin "$guest" --no-kernel
# Descriptor failures are rejected before deserializing/executing native code.
python3 - "$artifact" "$ovs_fixture_dir" <<'PY'
import json, pathlib, shutil, sys
original = json.loads(pathlib.Path(sys.argv[1]).read_text())
root = pathlib.Path(sys.argv[2])
for name, key, value in [
    ("version", "format-version", 99),
    ("interface", "interface", "netfyr:plugin/observer@9.0.0"),
    ("digest", "artifact-sha256", "0" * 64),
    ("path", "artifact", "../outside.cwasm"),
    ("missing", "artifact", "missing.cwasm"),
    ("schema", "schemas", {"bad":{"type":"invalid"}}),
]:
    metadata = dict(original)
    metadata[key] = value
    (root / f"invalid-{name}.json").write_text(json.dumps(metadata))
# A relocated installation needs only the local artifact and descriptor, not
# the provider's source tree or original schemas-file path.
relocated = root / "relocated"
relocated.mkdir()
shutil.copyfile(root / original["artifact"], relocated / original["artifact"])
shutil.copyfile(sys.argv[1], relocated / "plugin.json")
PY
for case in version interface digest path missing schema; do
    expect_failure 'InvalidMetadata' --trusted-precompiled "$ovs_fixture_dir/invalid-$case.json" --no-kernel
done
"$query" --trusted-precompiled "$ovs_fixture_dir/relocated/plugin.json" "${ovs_args[@]}" \
    --no-kernel >"$ovs_fixture_dir/relocated.yaml"
cmp "$ovs_fixture_dir/again.yaml" "$ovs_fixture_dir/relocated.yaml"

# The installation step validates provider metadata even with invalid Wasm input.
cat >"$ovs_fixture_dir/invalid-provider.json" <<'JSON'
{"format-version":99,"id":"invalid","interface":"netfyr:plugin/observer@0.1.0","schemas-file":"missing.json","config-schema":{"type":"object"}}
JSON
if "$compiler" /does/not/exist.wasm --metadata "$ovs_fixture_dir/invalid-provider.json" \
    --output "$ovs_fixture_dir/rejected.cwasm" 2>"$ovs_fixture_dir/install-error.log"; then
    echo "FAIL: precompiler accepted incompatible metadata" >&2
    exit 1
fi
grep -F 'unsupported metadata format-version' "$ovs_fixture_dir/install-error.log"
[ ! -e "$ovs_fixture_dir/rejected.cwasm" ] || { echo "FAIL: rejected package created an artifact" >&2; exit 1; }

# Neither configuration nor a grant for another path authorizes the endpoint.
expect_failure 'not declared' --trusted-precompiled "$artifact" --config-json "$config"
grep -F "$endpoint" "$ovs_fixture_dir/failure.log"
expect_failure 'not declared' --trusted-precompiled "$artifact" --config-json "$config" --allow-socket "$ovs_fixture_dir/another.sock"
# Every failure still presents the successful kernel observation.
grep -q 'name: lo' "$ovs_fixture_dir/failure.yaml"
expect_failure 'not found' --trusted-precompiled "$artifact" --config-json "{\"endpoint\":\"$ovs_fixture_dir/missing.sock\"}" --allow-socket "$ovs_fixture_dir/missing.sock"
grep -q 'name: lo' "$ovs_fixture_dir/failure.yaml"
python3 - "$ovs_fixture_dir/refused.sock" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.close()
PY
expect_failure 'refused' --trusted-precompiled "$artifact" --config-json "{\"endpoint\":\"$ovs_fixture_dir/refused.sock\"}" --allow-socket "$ovs_fixture_dir/refused.sock"
expect_failure 'Trap' --trusted-precompiled "$failure_artifact" --config-json '{"mode":"trap"}'
grep -q 'name: lo' "$ovs_fixture_dir/failure.yaml"
expect_failure 'InvalidOutput' --trusted-precompiled "$failure_artifact" --config-json '{"mode":"bad-schema"}'
expect_failure 'DeadlineExceeded' --trusted-precompiled "$failure_artifact" --config-json '{"mode":"loop"}' --timeout-ms 1

# Keep the component binary valid while changing its declared ABI major.
python3 - "$guest" "$ovs_fixture_dir/incompatible.wasm" <<'PY'
import pathlib, sys
component = pathlib.Path(sys.argv[1]).read_bytes()
assert b"netfyr:plugin/host-log@0.1.0" in component
pathlib.Path(sys.argv[2]).write_bytes(component.replace(b"@0.1.0", b"@9.0.0"))
PY
"$compiler" "$ovs_fixture_dir/incompatible.wasm" --metadata "$repo_root/crates/netfyr-ovs/plugin.json" --output "$ovs_fixture_dir/incompatible.cwasm"
expect_failure 'host-log@9.0.0' --trusted-precompiled "$ovs_fixture_dir/incompatible.cwasm.json"

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
expect_failure 'DeadlineExceeded' --trusted-precompiled "$artifact" --config-json "{\"endpoint\":\"$ovs_fixture_dir/stalled.sock\"}" --allow-socket "$ovs_fixture_dir/stalled.sock" --timeout-ms 100

# A fresh one-shot query still succeeds after the failure cases.
"$query" --trusted-precompiled "$artifact" "${ovs_args[@]}" --no-kernel >"$ovs_fixture_dir/final.yaml"
cmp "$ovs_fixture_dir/again.yaml" "$ovs_fixture_dir/final.yaml"

# Same-provider instances isolate grants; different providers isolate settings.
mkdir "$ovs_fixture_dir/plugins" "$ovs_fixture_dir/plugin.d"
cp "${artifact%.json}" "$ovs_fixture_dir/plugins/ovs.cwasm"
cp "$artifact" "$ovs_fixture_dir/plugins/ovs.cwasm.json"
"$compiler" "$bad_guest" --metadata "$repo_root/tests/integration/plugins/guest/plugin.json" --output "$ovs_fixture_dir/plugins/example.cwasm"
python3 - "$ovs_fixture_dir/plugin.d" "$endpoint" <<'PY'
import json, pathlib, sys
directory = pathlib.Path(sys.argv[1])
endpoint = sys.argv[2]
instances = {
    "a-denied": {"plugin":"ovs", "config":{"endpoint":endpoint}},
    "b-allowed": {"plugin":"ovs", "config":{"endpoint":endpoint}, "grants":{"unix-sockets":[endpoint]}},
    "c-other": {"plugin":"example", "config":{"endpoint":"other-provider-value"}},
    "z-denied": {"plugin":"ovs", "config":{"endpoint":endpoint}},
}
for name, data in instances.items():
    (directory / f"{name}.yaml").write_text(json.dumps(data))
PY
if "$query" --plugin-config-dir "$ovs_fixture_dir/plugin.d" --plugin-dir "$ovs_fixture_dir/plugins" --no-kernel \
    >"$ovs_fixture_dir/instances.yaml" 2>"$ovs_fixture_dir/instances.log"; then
    echo "FAIL: denied instance was not reported as a failure" >&2
    exit 1
fi
grep -F 'a-denied' "$ovs_fixture_dir/instances.log"
grep -F 'z-denied' "$ovs_fixture_dir/instances.log"
grep -F 'not declared' "$ovs_fixture_dir/instances.log"
python3 - "$ovs_fixture_dir/instances.yaml" <<'PY'
import pathlib, sys, yaml
text = pathlib.Path(sys.argv[1]).read_text()
assert 'b-allowed (ovs)' in text
assert 'c-other (example)' in text
ovs, other = list(yaml.safe_load_all(text))
assert len(ovs) == 11
assert other["example_device"]["value"] == "other-provider-value"
PY
echo "OVS component discovery, read-only transaction, capability denial, failures and precompilation passed"
