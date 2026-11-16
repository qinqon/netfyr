#!/bin/bash
# Build each PoC variant independently and measure one-shot process wall time.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
command -v cargo >/dev/null || { echo "FAIL: cargo not available" >&2; exit 1; }
command -v python3 >/dev/null || { echo "FAIL: python3 not available" >&2; exit 1; }
target_dir="${NETFYR_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')}"
output="$target_dir/plugin-measurements"
mkdir -p "$output"
artifacts="$(mktemp -d "$output/artifacts.XXXXXX")"
export CARGO_PROFILE_RELEASE_STRIP=symbols

# Separate Cargo invocations and target directories keep the compiler out of
# runtime-only builds. Count actual compiler-artifact package IDs, including
# build dependencies, even when Cargo reports cached/fresh artifacts.
build() {
    local variant="$1"
    shift
    cargo build --locked --release --target-dir "$output/$variant" --message-format=json "$@" \
        >"$artifacts/build-$variant.jsonl"
}
build baseline -p netfyr-backend --example query-kernel
build compile -p netfyr-plugin-runtime --example query-with-plugins
build native -p netfyr-plugin-runtime --example query-with-plugins --no-default-features
build pulley -p netfyr-plugin-runtime --example query-with-plugins --no-default-features --features pulley
build precompile -p netfyr-plugin-precompile --features pulley
cargo build --locked --release -p netfyr-ovs --target wasm32-wasip2 --target-dir "$target_dir/guest"
guest="$target_dir/guest/wasm32-wasip2/release/netfyr_ovs.wasm"
compiler="$output/precompile/release/netfyr-plugin-precompile"
"$compiler" "$guest" --output "$artifacts/ovs-native.cwasm"
"$compiler" "$guest" --pulley --output "$artifacts/ovs-pulley.cwasm"

cargo tree --locked -p netfyr-plugin-runtime --no-default-features -e features >"$artifacts/features-native.txt"
cargo tree --locked -p netfyr-plugin-runtime --no-default-features --features pulley -e features >"$artifacts/features-pulley.txt"

source "$repo_root/tests/integration/plugins/lib/fixture.sh"
trap ovs_fixture_cleanup EXIT
ovs_fixture_start
endpoint="$ovs_fixture_dir/sandbox/db.sock"
ovs_fixture_exec ovsdb-server --version >"$artifacts/ovs-version.txt"

"$output/compile/release/examples/query-with-plugins" --plugin "$guest" --ovsdb "$endpoint" --no-kernel >"$artifacts/compile.yaml"
"$output/native/release/examples/query-with-plugins" --trusted-precompiled "$artifacts/ovs-native.cwasm" --ovsdb "$endpoint" --no-kernel >"$artifacts/native.yaml"
"$output/pulley/release/examples/query-with-plugins" --trusted-precompiled "$artifacts/ovs-pulley.cwasm" --pulley --ovsdb "$endpoint" --no-kernel >"$artifacts/pulley.yaml"
cmp "$artifacts/compile.yaml" "$artifacts/native.yaml"
cmp "$artifacts/compile.yaml" "$artifacts/pulley.yaml"

python3 - "$output" "$artifacts" "$guest" "$endpoint" <<'PY'
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import time

output, artifacts, guest = map(pathlib.Path, sys.argv[1:4])
endpoint = sys.argv[4]
iterations = int(os.environ.get("NETFYR_MEASURE_ITERATIONS", "20"))
if iterations < 1:
    raise SystemExit("FAIL: NETFYR_MEASURE_ITERATIONS must be positive")

variants = [
    ("baseline", "query-kernel", []),
    ("compile", "query-with-plugins", ["--plugin", str(guest)]),
    ("native", "query-with-plugins", ["--trusted-precompiled", str(artifacts / "ovs-native.cwasm")]),
    ("pulley", "query-with-plugins", ["--trusted-precompiled", str(artifacts / "ovs-pulley.cwasm"), "--pulley"]),
]
results = []
for variant, name, args in variants:
    binary = output / variant / "release" / "examples" / name
    records = [json.loads(line) for line in (artifacts / f"build-{variant}.jsonl").read_text().splitlines()]
    packages = sorted({r["package_id"] for r in records if r.get("reason") == "compiler-artifact"})
    if variant in ("native", "pulley"):
        # cranelift-bitset is also an interpreter utility; its presence does
        # not mean a compiler was linked. Check the actual compiler packages.
        names = {p.rsplit("#", 1)[-1].split("@", 1)[0] for p in packages}
        compilers = {"cranelift-codegen", "wasmtime-internal-cranelift", "wasmtime-internal-winch", "winch-codegen"}
        assert names.isdisjoint(compilers), names & compilers
    command = [str(binary), *args]
    if variant != "baseline":
        command += ["--ovsdb", endpoint]
    elapsed = []
    for iteration in range(iterations + 3):
        start = time.perf_counter_ns()
        completed = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=30)
        if completed.returncode:
            raise SystemExit(completed.stderr.decode())
        duration = (time.perf_counter_ns() - start) / 1_000_000
        if iteration >= 3:
            elapsed.append(duration)
    results.append({
        "variant": variant,
        "binary_bytes": binary.stat().st_size,
        "packages": len(packages),
        "median_ms": statistics.median(elapsed),
        "min_ms": min(elapsed),
        "max_ms": max(elapsed),
        "samples_ms": elapsed,
    })

report = {
    "platform": platform.platform(),
    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "base_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "ovs": (artifacts / "ovs-version.txt").read_text().strip(),
    "guest_bytes": guest.stat().st_size,
    "native_artifact_bytes": (artifacts / "ovs-native.cwasm").stat().st_size,
    "pulley_artifact_bytes": (artifacts / "ovs-pulley.cwasm").stat().st_size,
    "warmups": 3,
    "iterations": iterations,
    "results": results,
}
report_path = artifacts / "measurements.json"
report_path.write_text(json.dumps(report, indent=2) + "\n")
print("\n| Variant | Stripped bytes | Packages built | Median query (ms) |")
print("| --- | ---: | ---: | ---: |")
for row in results:
    print(f'| {row["variant"]} | {row["binary_bytes"]:,} | {row["packages"]} | {row["median_ms"]:.2f} |')
print(f"\nRaw measurements and feature trees: {artifacts}")
PY
