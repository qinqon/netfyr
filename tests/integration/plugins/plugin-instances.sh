#!/bin/bash
# tags: plugins wasm config
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$repo_root"
target_dir="${NETFYR_TARGET_DIR:-$repo_root/target}"
command -v cargo >/dev/null || { echo "FAIL: cargo not available" >&2; exit 1; }
command -v python3 >/dev/null || { echo "FAIL: python3 not available" >&2; exit 1; }
python3 -c 'import yaml' || { echo "FAIL: Python PyYAML not available" >&2; exit 1; }
cargo build --locked -p netfyr-plugin-precompile --target-dir "$target_dir"
cargo build --locked -p netfyr-plugin-runtime --no-default-features --example query-with-plugins --target-dir "$target_dir/plugin-native"
cargo build --locked --release --target wasm32-wasip2 -p netfyr-plugin-test-guest --target-dir "$target_dir/guest"
directory="$(mktemp -d "${TMPDIR:-/tmp}/netfyr-instances.XXXXXX")"
trap 'rm -rf -- "$directory"' EXIT
mkdir "$directory/plugins"
"$target_dir/debug/netfyr-plugin-precompile" \
    "$target_dir/guest/wasm32-wasip2/release/netfyr_plugin_test_guest.wasm" \
    --metadata "$repo_root/tests/integration/plugins/guest/plugin.json" --output "$directory/plugins/example.cwasm"
python3 - "$target_dir/plugin-native/debug/examples/query-with-plugins" "$directory" <<'PY'
import json, pathlib, subprocess, sys, yaml
binary, root = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])

def put(tier, name, data):
    directory = root / tier
    directory.mkdir(exist_ok=True)
    path = directory / name
    path.write_text(data if isinstance(data, str) else json.dumps(data))
    return directory

def run(*tiers, success=True):
    command = [str(binary), "--no-kernel", "--plugin-dir", str(root / "plugins")]
    for tier in tiers:
        command += ["--plugin-config-dir", str(root / tier)]
    result = subprocess.run(command, capture_output=True, text=True, timeout=10)
    assert (result.returncode == 0) == success, (command, result.stdout, result.stderr)
    return result

def value(result):
    return yaml.safe_load(result.stdout)["example_device"]["value"]

assert yaml.safe_load(run("missing").stdout) == []
put("disabled", "off.yaml", {"enabled":False})
assert yaml.safe_load(run("disabled").stdout) == []

put("vendor", "host.yaml", {"plugin":"example", "config":{"endpoint":"vendor"}, "grants":{"unix-sockets":["/vendor.sock"]}})
put("admin", "host.yaml", {"plugin":"example", "config":{"endpoint":"admin"}})
put("runtime", "host.yaml", {"plugin":"example", "config":{"endpoint":"runtime"}})
assert value(run("vendor")) == "vendor"
assert value(run("vendor", "admin")) == "admin"
assert value(run("vendor", "admin", "runtime")) == "runtime"
put("replace", "host.yaml", {"plugin":"example"})
assert value(run("vendor", "replace")) == "0123"

put("broken", "host.yaml", "not: [valid YAML")
put("mask", "host.yaml", {"enabled":False})
assert yaml.safe_load(run("broken", "mask").stdout) == []
run("broken", success=False)

put("ordered", "z.yaml", {"plugin":"example", "config":{"endpoint":"last"}})
put("ordered", "a.yaml", {"plugin":"example", "config":{"endpoint":"first"}})
put("ordered", ".hidden.yaml", "invalid")
put("ordered", "ignored.txt", "invalid")
rows = list(yaml.safe_load_all(run("ordered").stdout))
assert [row["example_device"]["value"] for row in rows] == ["first", "last"]

for name, data in [
    ("unknown", {"plugin":"example", "typo":True}),
    ("schema-unknown", {"plugin":"example", "config":{"unknown":"secret-marker"}}),
    ("schema-type", {"plugin":"example", "config":{"endpoint":12}}),
    ("relative-grant", {"plugin":"example", "grants":{"unix-sockets":["relative.sock"]}}),
    ("duplicate-grant", {"plugin":"example", "grants":{"unix-sockets":["/same", "/same"]}}),
    ("zero-timeout", {"plugin":"example", "timeout-ms":0}),
    ("bad-timeout", {"plugin":"example", "timeout-ms":60001}),
    ("bad-id", {"plugin":"../example"}),
    ("missing-package", {"plugin":"absent"}),
    ("not-object", {"plugin":"example", "config":[]}),
]:
    put(name, "bad.yaml", data)
    result = run(name, success=False)
    assert 'bad.yaml' in result.stderr
    assert 'secret-marker' not in result.stderr

put("duplicate", "same.yaml", {"plugin":"example"})
put("duplicate", "same.yml", {"plugin":"example"})
assert 'duplicate instance name' in run("duplicate", success=False).stderr
put("oversized", "bad.yaml", "#" + "x" * (64 * 1024))
run("oversized", success=False)
symlinks = root / "symlinks"
symlinks.mkdir()
(symlinks / "bad.yaml").symlink_to(root / "admin/host.yaml")
run("symlinks", success=False)

# Preflight all configs before even an earlier, valid trapping guest executes.
put("preflight", "a-trap.yaml", {"plugin":"example", "config":{"mode":"trap"}})
put("preflight", "z-invalid.yaml", {"plugin":"example", "config":{"unknown":True}})
result = run("preflight", success=False)
assert not result.stdout
assert 'z-invalid.yaml' in result.stderr
assert 'Trap' not in result.stderr

put("partial", "a-trap.yaml", {"plugin":"example", "config":{"mode":"trap"}})
put("partial", "b-ok.yaml", {"plugin":"example"})
result = run("partial", success=False)
assert 'a-trap' in result.stderr and 'Trap' in result.stderr
assert value(result) == "0123"

put("all-failed", "trap.yaml", {"plugin":"example", "config":{"mode":"trap"}})
assert not run("all-failed", success=False).stdout

command = [str(binary), "--plugin-config-dir", str(root / "admin"), "--config-json", "{}", "--no-kernel"]
assert subprocess.run(command, capture_output=True, timeout=10).returncode != 0
print("Instance tiering, validation, disabled overrides, isolation and partial failures passed")
PY
