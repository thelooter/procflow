#!/usr/bin/env bash
# Build the eBPF collector object (nightly + bpf-linker required):
#   rustup toolchain install nightly --component rust-src
#   cargo install bpf-linker
# Output: crates/procflow-ebpf/target/bpfel-unknown-none/release/procflow-ebpf
set -euo pipefail
cd "$(dirname "$0")/../crates/procflow-ebpf"
exec cargo +nightly build --release "$@"
