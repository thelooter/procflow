# eBPF collector: socket-layer fentry/fexit hooks; kernel counts per-PID, userspace resolves Identity

## Context

The collector must attribute **payload bytes** (ADR-0002), split by **Direction**
and **Scope**, to an **Identity** (ADR-0004). But an Identity's natural key
includes `project_root` and `normalized_cmdline` — derived by walking `/proc` and
the filesystem for a marker. eBPF runs in-kernel: it can cheaply see the current
task's `tgid`/`uid`/`cgroup`/`comm` and the `struct sock`, but it **cannot** do
the filesystem marker-walk that defines a `project_root`. So the kernel cannot
compute an Identity key; something in userspace must.

## Decision

**The kernel counts bytes per `tgid`; the userspace daemon resolves `tgid` →
Identity** (ADR-0007). The collector is BTF/CO-RE, using **fentry/fexit** hooks
(kernel ≥5.8 with `CONFIG_DEBUG_INFO_BTF=y`), via [`aya`](https://aya-rs.dev/):

| Path    | Hook                                    | Bytes source            |
|---------|-----------------------------------------|-------------------------|
| egress  | fexit `tcp_sendmsg`                     | return value, when `>0` |
| egress  | fexit `udp_sendmsg`, `udpv6_sendmsg`    | return value, when `>0` |
| ingress | fentry `tcp_cleanup_rbuf`               | `copied` arg, when `>0` |
| ingress | fexit `udp_recvmsg`, `udpv6_recvmsg`    | return value, when `>0` |

- All these hooks fire **in process context**, so `bpf_get_current_pid_tgid()`
  is the *owning* process. This is exactly why ingress hooks `tcp_cleanup_rbuf`
  (called when the app drains its receive buffer) rather than
  `tcp_rcv_established` / the NET_RX softirq path, where the running task is
  arbitrary.
- **fexit** (not kretprobe) so a single program sees both the call args *and*
  the return value — needed to count actual bytes sent, not requested.
- Counters live in a `traffic` map: `BPF_MAP_TYPE_PERCPU_HASH`, key
  `{ u32 tgid, u8 dir, u8 scope }`, value `u64 bytes`. Per-CPU makes increments
  contention-free; userspace sums across CPUs on read.
- **Direction** comes from the hook (send=egress, recv=ingress). **Scope** is
  read from the `sock`: `loopback` if v4 `daddr ∈ 127.0.0.0/8` (or
  `saddr == daddr`), or v6 `daddr == ::1`, or the socket is bound to the
  loopback ifindex; otherwise `external`. Unconnected UDP has no `daddr` on the
  sock — the destination is read best-effort, defaulting to `external`.
- The daemon polls `traffic` on an interval **much finer than the minute grain**
  (e.g. 5s), reads-and-clears each entry, resolves `tgid` → Identity, folds bytes
  into the in-memory current-minute accumulator, and flushes closed minutes to
  the minute tier (ADR-0005).

## Considered options

- **Aggregate by Identity in-kernel.** Impossible: `project_root` needs a
  userspace filesystem walk the verifier/helpers can't do. This constraint is
  *why* the kernel keys by `tgid` and userspace resolves.
- **kprobe/kretprobe instead of fentry/fexit.** Runs on pre-BTF kernels, but
  higher overhead, a kretprobe can't see the entry args (forcing a paired-probe
  scratch map), and raw struct access is brittle without CO-RE. Kept as a
  *possible* fallback for old kernels; not built for v1.
- **Lower hooks (cgroup/skb, tc, XDP) for wire-accurate bytes.** Rejected by
  ADR-0002: they collapse all terminal-launched dev processes into one
  shell-session cgroup, destroying per-project attribution.
- **Stream every send/recv over a ring buffer, count in userspace.** High event
  rate, per-event syscall/copy overhead. In-kernel per-CPU aggregation is the
  entire point of the per-Identity-counter model (ADR-0002). (A ring buffer *is*
  used, but only for low-rate first-seen notifications — ADR-0007.)

## Consequences

- **Hard requirement:** a modern BTF-enabled kernel (≥5.8). The daemon needs
  `CAP_BPF` + `CAP_PERFMON` (or root).
- Undercounts vs the wire (expected, ADR-0002) and *additionally* misses
  zero-copy `sendfile`/`splice` egress (`do_tcp_sendpages`) — a documented minor
  gap, rare for the dev-tooling workloads procflow targets. `AF_UNIX` sockets
  never hit these hooks (not network traffic — correct to exclude).
- The `traffic` map has a `max_entries` cap sized for concurrent `tgid`s; on
  overflow an increment fails → bounded undercount, logged.
- Attribution is per-`tgid` (process), not per-thread — intentional; Identity is
  a process/project-level concept.
