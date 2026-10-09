#!/usr/bin/env bash
# Check the built eBPF object: every hook the daemon attaches is in it
# (ADR-0006), it carries its maps and BTF, and its license section is the
# one ADR-0012 requires for the gpl_only kernel helpers.
#
# usage: check-ebpf.sh [OBJECT]
set -euo pipefail

object=${1:-"$(dirname "$0")/../crates/procflow-ebpf/target/bpfel-unknown-none/release/procflow-ebpf"}
sections=$(readelf --section-headers --wide "$object")

for section in \
    fentry/tcp_cleanup_rbuf \
    fexit/tcp_sendmsg fexit/udp_sendmsg fexit/udpv6_sendmsg \
    fexit/udp_recvmsg fexit/udpv6_recvmsg \
    maps .BTF; do
    if ! grep --quiet --fixed-strings "] $section " <<<"$sections"; then
        echo "check-ebpf: $object has no section '$section'" >&2
        exit 1
    fi
done

license=$(readelf --string-dump=license "$object" | sed -n 's/^ *\[ *0\] *//p')
if [ "$license" != "Dual MIT/GPL" ]; then
    echo "check-ebpf: license section is '$license', expected 'Dual MIT/GPL' (ADR-0012)" >&2
    exit 1
fi

echo "check-ebpf: ok, 6 programs, maps, BTF, license '$license'"
