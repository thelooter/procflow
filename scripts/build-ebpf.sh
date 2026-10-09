#!/usr/bin/env bash
# Build the eBPF collector object (nightly + bpf-linker required):
#   rustup toolchain install nightly --component rust-src
#   cargo install bpf-linker
# Output: crates/procflow-ebpf/target/bpfel-unknown-none/release/procflow-ebpf
#
# PROCFLOW_NIGHTLY names the toolchain to use instead of `nightly`. CI sets
# it to a dated nightly so a new one cannot break an unrelated change.
set -euo pipefail
cd "$(dirname "$0")/../crates/procflow-ebpf"
exec cargo "+${PROCFLOW_NIGHTLY:-nightly}" build --release "$@"
