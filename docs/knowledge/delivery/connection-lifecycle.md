---
type: Behavior
title: Connection Lifecycle
description: >
          The per-connection pipeline (ClientHello, route, upstream connect
          and replay, proxy gate, proxy), each stage's deadline, and the
          outcome recorded when it ends.
resource: crates/sni_router/src/delivery/connection.rs
tags: [delivery, proxy, timeouts, lifecycle]
timestamp: 2026-09-24T00:00:00Z
---

`handle_connection` (`crates/sni_router/src/delivery/connection.rs:115`)
runs one connection. It records `ConnectionOpened` and runs the pipeline
in `run` (`crates/sni_router/src/delivery/connection.rs:143`). A
`ConnectionRecord` drop guard then records the close:

- the byte totals
- exactly one `ConnectionClosed { outcome, lifetime }`
- one `debug` log line with peer, listener, SNI, backend, outcome, byte
  counts, and lifetime

Because the close is recorded on drop, a connection task that panics or
is aborted still reports its bytes and decrements the open-connections
gauge, with outcome `Aborted`.

# Stages and deadlines

Defaults come from `RouterConfig`
(`crates/sni_router/src/delivery/mod.rs:73`).

1. **ClientHello**, `client_hello_timeout` (5s), one deadline for the
   whole hello, not per read
   (`crates/sni_router/src/delivery/connection.rs:158`). The reader
   (`crates/sni_router/src/delivery/hello_reader.rs:36`) keeps every byte
   it reads, capped at `HelloLimits::max_wire_bytes()`.
2. **Route lookup**, `lookup_timeout` (2s)
   (`crates/sni_router/src/delivery/connection.rs:187`). See
   [hostname-matching](../routing/hostname-matching.md).
3. **Upstream**, `upstream_timeout` (5s), one deadline covering DNS
   resolution, TCP connect, and replaying the buffered bytes
   (`crates/sni_router/src/delivery/connection.rs:296`). A backend that
   accepts but never reads can't hold the connection past this deadline.
   DNS backends go through the router's resolver
   (`crates/sni_router/src/delivery/resolver.rs`):
   - **Cache:** resolved addresses are reused for `dns_cache_ttl`
     (default 30s). Failures are cached for 1s, and if re-resolving fails,
     the last good answer is used for up to 60s more.
   - **Concurrency cap:** at most `max_concurrent_dns_lookups` (default 64)
     system lookups run at once. `getaddrinfo` blocks and can't be
     cancelled, so each lookup holds its slot until the call returns, even
     after the connection that asked for it has given up. A DNS slowdown
     therefore can't fill tokio's blocking pool.
   - **Single flight:** lookups are single-flight per `(name, port)`.
     When an entry expires, concurrent connections share one lookup, which
     runs on its own task and is cached even if every caller has given up.
     So there's no burst of lookups at expiry, and no race between answers.
   - **Address order:** rotates per connection, and the router tries each
     address in turn.
4. **Proxy gate**, the commit point for shutdown
   (`crates/sni_router/src/delivery/connection.rs:247`). See
   [shutdown-and-limits](shutdown-and-limits.md).
5. **Proxy**, bounded only by `idle_timeout`: 30 minutes by default,
   configurable, and `None` disables it
   (`crates/sni_router/src/delivery/proxy.rs:24`). The default exists
   because, without it, idle TLS sessions to any routed host could hold
   every connection slot forever. It doesn't stop a client that keeps
   trickling bytes; per-client limits belong in front of the router.
   The copy propagates half-close: when one side finishes sending, the
   other side's write half is shut down while the reverse direction keeps
   flowing. `copy` (`crates/sni_router/src/delivery/proxy.rs:69`) picks the
   mechanism: on Linux, `splice(2)` through an in-kernel pipe
   (`crates/sni_router/src/delivery/splice.rs:292`), when
   `splice::available()` (`crates/sni_router/src/delivery/splice.rs:40`)
   finds the syscall usable in this process *and* `splice::admission()`
   (`crates/sni_router/src/delivery/splice.rs:144`) has fd budget left.
   `admission()` is one semaphore for the whole process, shared by every
   `Router::serve` call — sized once from `RLIMIT_NOFILE` alone, not from
   any one `Router`'s `max_connections`, since an embedder may run several
   `Router`s at once and a per-`Router` budget would double-count the same
   process-wide fd limit across them. Each splice connection holds 4 extra
   fds (two pipes), so at most half of `RLIMIT_NOFILE` is ever committed to
   splice pipes (in units of 4); the other half is always available for
   sockets, listeners, and anything else in the process. Both directions'
   pipes are then built by `splice::prepare` before either socket is
   touched (`crates/sni_router/src/delivery/splice.rs:230`), so a setup
   failure (fd budget reserved but the kernel still refuses, e.g. a global
   limit) is always safe to answer by falling back — no byte has been
   consumed yet. Once running, payload bytes never cross into userspace at
   all. Every other OS, Linux when splice probes as unavailable, the fd
   budget is exhausted, or setup fails, falls back to
   `tokio::io::copy_bidirectional_with_sizes`, which copies through two
   buffers of `copy_buffer_size` (default 32 KiB) held for the life of the
   connection. The `SNI_ROUTER_DISABLE_SPLICE` env var forces the fallback,
   e.g. to work around a kernel bug or A/B the two paths. Because the
   splice path bypasses `MeteredStream`, it reports bytes to the same
   counters directly (`crates/sni_router/src/delivery/metered.rs:122`) at
   the same boundary `MeteredStream` uses — a client read counts as soon as
   it's off the client socket, a client write as soon as it's landed there
   — so byte accounting matches the portable copy even when a direction
   fails partway. Measured on loopback (`cargo run --release --example
   copy_benchmark --features test-util`), splice moved a 512 MiB payload at
   roughly 1.3–1.7× the userspace copy's throughput.
   The idle watchdog (`crates/sni_router/src/delivery/proxy.rs:99`) closes the
   connection after `idle_timeout` with no bytes in *either* direction.
   The idle clock restarts when proxying begins, so time spent on the
   lookup and the upstream connect never counts as idle.
   The activity stamp (`crates/sni_router/src/delivery/metered.rs:41`)
   exists only when an idle timeout is set. With it disabled, the
   per-chunk path does no clock reads or atomics. An idle timeout too large to
   represent never fires instead of overflowing.

Every await in stages 1–3 and the alert write races the shutdown token
through `cancellable`
(`crates/sni_router/src/delivery/connection.rs:279`). The alert write is
capped at 1s (`crates/sni_router/src/delivery/connection.rs:327`).

# Outcomes

`ConnectionOutcome` (`crates/sni_router/src/metrics.rs:71`):

| Outcome | When |
|---|---|
| `Proxied` | Both directions finished |
| `ProxyError` | I/O error (e.g. reset) while proxying |
| `ClientClosed` | Client closed or errored before a full hello |
| `HelloTimeout` | Hello not complete within `client_hello_timeout` |
| `InvalidHello` | Not TLS, malformed, or over the limits; closed without a reply |
| `MissingSni` / `UnknownHost` | TLS `unrecognized_name` alert, then close |
| `LookupFailed` / `LookupTimeout` | Close |
| `UpstreamConnectFailed` | Refused, or the connect/replay failed |
| `UpstreamTimeout` | Connect and replay exceeded `upstream_timeout` |
| `IdleTimeout` | No bytes either way for `idle_timeout` |
| `ShutdownCancelled` / `ShutdownForced` | See [shutdown-and-limits](shutdown-and-limits.md) |
| `Aborted` | The connection task panicked or was aborted; recorded by the drop guard |

# Byte accounting and logging

`BytesRead` and `BytesWritten` count every client-side byte: the
ClientHello, bytes from handshakes that fail, the alert (including partial
writes), and proxied traffic. The client stream is wrapped in
`MeteredStream` (`crates/sni_router/src/delivery/metered.rs:100`) from
accept onward. Wrapping only the client side is enough, because every
proxied byte is either read from or written to the client.

The counters are uncontended relaxed atomics, one add per chunk, shared
with the close record. The totals are reported **once, when the
connection closes**, so a long-lived connection contributes its bytes only
at close. That trade avoids a metrics call on every copied chunk.

The per-connection "connection closed" line is logged at `debug`. At
thousands of connections per second an `info` line each is real CPU and
log volume. Lookup and upstream failures still log their own warnings.

# Memory sizing

- **Handshake phase:** the ClientHello buffer grows exactly, never past
  `HelloLimits::max_wire_bytes()` (about 33 KiB by default), and is freed
  before proxying. Worst case is about `max_connections ×
  max_client_hello_bytes`.
- **Proxy phase:** each connection allocates `2 × copy_buffer_size` of
  copy buffer (64 KiB by default). Only pages that have actually carried
  data are resident, so the cost depends on activity.
  - **Idle connections (measured, release binary, 10k connections):**
    about 19 KiB of RSS each with 8 KiB buffers and about 21 KiB with
    32 KiB buffers.
  - **Busy connections:** they approach the full 64 KiB, plus kernel
    socket buffers.

  The 32 KiB default is the throughput knee measured on loopback for one
  connection: about 0.8 GiB/s at 8 KiB, 1.5 at 16 KiB, and 2.4–2.6 from
  32 KiB up. Direct, without the router, was 4.9 GiB/s.
