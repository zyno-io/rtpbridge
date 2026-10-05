#!/usr/bin/env bash
set -euo pipefail

bench_base_sha="${1:?Usage: compare-benchmarks.sh <baseline-commit> [criterion-arguments...]}"
shift
bench_base_dir="$(mktemp -d)"
trap 'rm -rf -- "$bench_base_dir"' EXIT

bench_target_dir="${CARGO_TARGET_DIR:-target}"
case "$bench_target_dir" in
    /*) ;;
    *) bench_target_dir="$(pwd)/$bench_target_dir" ;;
esac
export CARGO_TARGET_DIR="$bench_target_dir"

git archive "$bench_base_sha" | tar -x -C "$bench_base_dir"
echo "Benchmark baseline: $bench_base_sha"
cargo clean --manifest-path "$bench_base_dir/Cargo.toml" -p rtpbridge --release
cargo bench --manifest-path "$bench_base_dir/Cargo.toml" --bench '*' -- "$@" --noise-threshold 0.05 --save-baseline comparison-base

# The package name/version can match across snapshots. Force a candidate rebuild
# while retaining dependency artifacts and the freshly measured Criterion data.
cargo clean -p rtpbridge --release
cargo bench --bench '*' -- "$@" --noise-threshold 0.05 --baseline comparison-base 2>&1 | tee bench-output.txt

if grep -q 'Performance has regressed' bench-output.txt; then
    echo 'Benchmark regressions detected'
    grep -B 4 'Performance has regressed' bench-output.txt
    exit 1
fi
