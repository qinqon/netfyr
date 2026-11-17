#!/bin/bash
# Compare the same OVS discovery code as a native reference and precompiled Wasm.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
command -v cargo >/dev/null || { echo "FAIL: cargo not available" >&2; exit 1; }
command -v python3 >/dev/null || { echo "FAIL: python3 not available" >&2; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "FAIL: resource measurements require Linux" >&2; exit 1; }
target_dir="${NETFYR_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')}"
output="$target_dir/wasm-overhead"
mkdir -p "$output"
artifacts="$(mktemp -d "$output/artifacts.XXXXXX")"
export CARGO_PROFILE_RELEASE_STRIP=symbols
cargo build --locked --release -p netfyr-plugin-overhead-probe --bins --target-dir "$output/native"
cargo build --locked --release -p netfyr-plugin-overhead-probe --bin netfyr-plugin-overhead-probe --features wasm --target-dir "$output/wasm"
cargo build --locked --release -p netfyr-plugin-precompile --target-dir "$target_dir/plugin-measurements/precompile"
cargo build --locked --release -p netfyr-ovs --target wasm32-wasip2 --target-dir "$target_dir/guest"
cargo tree --locked -p netfyr-plugin-overhead-probe --no-default-features --prefix none >"$artifacts/native-dependencies.txt"
if grep -Eq '^(wasmtime|wit-bindgen)' "$artifacts/native-dependencies.txt"; then
    echo "FAIL: native reference depends on the Wasm runtime/tooling" >&2
    exit 1
fi
"$target_dir/plugin-measurements/precompile/release/netfyr-plugin-precompile" \
    "$target_dir/guest/wasm32-wasip2/release/netfyr_ovs.wasm" \
    --metadata crates/netfyr-ovs/plugin.json --output "$artifacts/ovs.cwasm"

source "$repo_root/tests/integration/plugins/lib/fixture.sh"
trap ovs_fixture_cleanup EXIT
ovs_fixture_start
endpoint="$ovs_fixture_dir/sandbox/db.sock"
ovs_fixture_exec ovsdb-server --version >"$artifacts/ovs-version.txt"
descriptor="$artifacts/ovs.cwasm.json"
native="$output/native/release/netfyr-plugin-overhead-probe"
wasm="$output/wasm/release/netfyr-plugin-overhead-probe"
"$native" --descriptor "$descriptor" --endpoint "$endpoint" --dump >"$artifacts/native.yaml"
"$wasm" --wasm --descriptor "$descriptor" --endpoint "$endpoint" --dump >"$artifacts/wasm.yaml"
cmp "$artifacts/native.yaml" "$artifacts/wasm.yaml"

python3 - "$output" "$artifacts" "$descriptor" "$endpoint" <<'PY'
import json, os, pathlib, platform, statistics, subprocess, sys

output, artifacts, descriptor = map(pathlib.Path, sys.argv[1:4])
endpoint = sys.argv[4]
samples = int(os.environ.get("NETFYR_OVERHEAD_SAMPLES", "30"))
queries = int(os.environ.get("NETFYR_OVERHEAD_WARM_QUERIES", "100"))
if not 1 <= samples <= 1000 or not 1 <= queries <= 10000:
    raise SystemExit("FAIL: sample count or warm-query count outside supported bounds")
launcher = output / "native/release/resource-probe"
commands = {
    "native": [str(output / "native/release/netfyr-plugin-overhead-probe")],
    "wasm": [str(output / "wasm/release/netfyr-plugin-overhead-probe"), "--wasm"],
}
records = {mode: {variant: [] for variant in commands} for mode in ("one-shot", "warm")}
for mode in records:
    for sample in range(samples + 3):
        # Alternate pair order to reduce systematic drift/thermal bias.
        variants = list(commands) if sample % 2 == 0 else list(reversed(commands))
        for variant in variants:
            child_output = artifacts / "child.json"
            command = commands[variant] + ["--descriptor", str(descriptor), "--endpoint", endpoint]
            if mode == "warm":
                command += ["--iterations", str(queries), "--warmups", "5"]
            result = subprocess.run([str(launcher), str(child_output), *command], capture_output=True, text=True, timeout=120)
            if result.returncode:
                raise SystemExit(result.stderr or result.stdout)
            resources = json.loads(result.stdout)
            assert resources["exit_code"] == 0 and resources["wall_ms"] > 0
            assert resources["peak_rss_kib"] > 0 and resources["user_ms"] >= 0 and resources["system_ms"] >= 0
            measured = json.loads(child_output.read_text())
            assert measured["devices"] == 11, measured
            resources["cpu_ms"] = resources["user_ms"] + resources["system_ms"]
            resources["query_ms"] = statistics.median(measured["query_samples_ms"])
            resources["setup_ms"] = measured["setup_ms"]
            resources["query_samples_ms"] = measured["query_samples_ms"]
            if sample >= 3:
                records[mode][variant].append(resources)

summary = {}
for mode, variants in records.items():
    summary[mode] = {}
    for variant, rows in variants.items():
        summary[mode][variant] = {field: statistics.median(row[field] for row in rows) for field in
            ("wall_ms", "user_ms", "system_ms", "cpu_ms", "peak_rss_kib", "query_ms", "setup_ms")}

report = {
    "platform": platform.platform(),
    "cpu_model": next((line.split(":", 1)[1].strip() for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown"),
    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "working_tree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], text=True).strip()),
    "ovs": (artifacts / "ovs-version.txt").read_text().strip(),
    "samples": samples, "warm_queries": queries, "process_warmups":3,"in_process_warmups":5,
    "binary_bytes": {variant:pathlib.Path(command[0]).stat().st_size for variant,command in commands.items()},
    "summary":summary,"records":records,
}
(artifacts / "overhead.json").write_text(json.dumps(report, indent=2) + "\n")
for mode, variants in summary.items():
    print(f"\n{mode} (medians, {samples} processes per variant)")
    print("| Variant | Process wall ms | CPU ms | Peak RSS MiB | Setup ms | Query ms |")
    print("| --- | ---: | ---: | ---: | ---: | ---: |")
    for variant,row in variants.items():
        print(f'| {variant} | {row["wall_ms"]:.3f} | {row["cpu_ms"]:.3f} | {row["peak_rss_kib"] / 1024:.2f} | {row["setup_ms"]:.3f} | {row["query_ms"]:.3f} |')
print(f"\nRaw per-process resources and query samples: {artifacts / 'overhead.json'}")
PY
