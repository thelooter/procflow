#!/usr/bin/env bash
# End-to-end smoke test against the running kernel. Starts procflowd with the
# real eBPF collector, moves a known number of bytes over loopback, and
# expects them attributed to the processes that moved them: first in the live
# stream, then in the store once the minute has closed.
#
# Needs a workspace build, the eBPF object (scripts/build-ebpf.sh), python3
# and curl. The daemon needs CAP_BPF and CAP_PERFMON, so it runs under sudo.
# Takes up to two minutes, most of it waiting for a minute to close.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin=${PROCFLOW_BIN_DIR:-$root/target/debug}
object=${PROCFLOW_BPF_OBJECT:-$root/crates/procflow-ebpf/target/bpfel-unknown-none/release/procflow-ebpf}
payload_bytes=$((4 * 1024 * 1024))

work=$(mktemp -d)
mkdir "$work/www"
export PROCFLOW_SOCKET=$work/procflow.sock
daemon='' server='' watch=''

cleanup() {
    status=$?
    set +e
    [ -n "$watch" ] && kill "$watch" 2>/dev/null
    [ -n "$server" ] && kill "$server" 2>/dev/null
    # sudo passes the signal on to the daemon.
    [ -n "$daemon" ] && sudo kill "$daemon" 2>/dev/null
    if [ "$status" -ne 0 ]; then
        echo "--- daemon log" >&2
        cat "$work/daemon.log" >&2
    fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT

fail() {
    echo "smoke-test: $*" >&2
    exit 1
}

# Ask for the password up front; a backgrounded sudo cannot prompt.
sudo --validate
# The log is meant to be written as the calling user, not as root.
# shellcheck disable=SC2024
sudo env PROCFLOW_SOCKET="$PROCFLOW_SOCKET" PROCFLOW_DB=:memory: PROCFLOW_BPF_OBJECT="$object" \
    "$bin/procflowd" >"$work/daemon.log" 2>&1 &
daemon=$!

for _ in $(seq 1 100); do
    "$bin/procflow" status >"$work/status" 2>/dev/null && break
    sleep 0.2
done
grep --quiet '^collector: running' "$work/status" 2>/dev/null ||
    fail "the daemon did not come up with its collector running"

# A local HTTP server, so the test needs no network and knows the byte count.
head --bytes "$payload_bytes" /dev/zero >"$work/www/payload"
python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$work/www" >"$work/http.log" 2>&1 &
server=$!
port=''
for _ in $(seq 1 50); do
    port=$(sed -n 's/.* port \([0-9]*\).*/\1/p' "$work/http.log" | head -n 1)
    [ -n "$port" ] && break
    sleep 0.1
done
[ -n "$port" ] || fail "the HTTP server did not start"

"$bin/procflow" watch --json --scope all >"$work/chunks.jsonl" &
watch=$!
sleep 2 # past one poll, so the stream is subscribed before the traffic
curl --fail --silent --show-error --max-time 30 --output /dev/null "http://127.0.0.1:$port/payload"
sleep 3 # past two more polls, so the traffic's interval has been published
kill "$watch"
wait "$watch" 2>/dev/null || true
watch=''

# curl must have received the payload and the server sent it. curl is gone
# before the daemon can read /proc, so it also covers the Unresolved path;
# the server is still running and must be resolved.
python3 - "$work/chunks.jsonl" "$payload_bytes" <<'EOF' || fail "the live stream did not attribute the transfer"
import json
import sys

path, want = sys.argv[1], int(sys.argv[2])
moved = {}
for line in open(path):
    for row in json.loads(line)["rows"]:
        identity = row["identity"]
        entry = moved.setdefault(identity["name"], {"in": 0, "out": 0, "exe": identity["exe"]})
        entry["in"] += row["ingress_bytes"]
        entry["out"] += row["egress_bytes"]
print("live:", json.dumps(moved))
server = next((v for name, v in moved.items() if name.startswith("python")), None)
ok = moved.get("curl", {"in": 0})["in"] >= want
ok = ok and server is not None and server["out"] >= want and server["exe"] != "<unresolved>"
sys.exit(0 if ok else 1)
EOF

# The collector stores a minute on the first poll after it has closed.
sleep $((62 - $(date +%s) % 60))
"$bin/procflow" top --since 10m --scope all --limit 0 --json >"$work/top.json"
python3 - "$work/top.json" "$payload_bytes" <<'EOF' || fail "the store did not hold the transfer"
import json
import sys

path, want = sys.argv[1], int(sys.argv[2])
rows = json.load(open(path))["rows"]
received = sum(row["ingress_bytes"] for row in rows if row["identity"]["name"] == "curl")
print(f"stored: curl received {received} bytes")
sys.exit(0 if received >= want else 1)
EOF

echo "smoke-test: ok"
