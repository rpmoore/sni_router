---
type: System
title: sni_router Overview
description: >
          What sni_router is, how the library and the app divide the work,
          and how one connection flows from accept to a proxied backend.
resource: crates/sni_router/src/lib.rs
tags: [architecture, overview, tls, sni, routing, proxy]
timestamp: 2026-09-24T00:00:00Z
---

sni_router routes TLS connections to backends by the Server Name
Indication in the ClientHello, **without terminating TLS**. It reads just
enough of the first flight to learn the SNI, picks a backend, replays the
buffered bytes to it, and splices the two TCP streams.

# Library first

The workspace has two crates:

- **`crates/sni_router`** — the library. Parsing, routing policy,
  timeouts, proxying, and shutdown. It depends only on `tokio`,
  `tokio-util`, and `tracing` (`crates/sni_router/Cargo.toml`); no serde,
  TOML, HTTP, or metrics exporter.
- **`crates/sni_router_app`** — the open-source `sni_router` binary. It
  wires the library to a TOML route file, an admin HTTP listener, an
  OpenTelemetry/Prometheus metrics sink, and SIGHUP reload
  (`crates/sni_router_app/src/lib.rs:50`).

Embedders supply two things: a [`RouteLookup`](lookup/lookup-contract.md)
(where routes live) and a [`MetricsSink`](observability/metrics.md)
(where metrics go). A closed-source service embeds the same library with a
database-backed lookup; see [embedding](embedding.md).

# Module layout

`crates/sni_router/src/lib.rs` exposes one module per layer:

- **`protocol`** — pure ClientHello parsing (`parse_client_hello`,
  `crates/sni_router/src/protocol/record.rs:34`). See
  [client-hello-parsing](protocol/client-hello-parsing.md).
- **`lookup`** — the `RouteLookup` trait and domain types
  (`crates/sni_router/src/lookup/mod.rs:52`).
- **`routing`** — candidate keys and precedence (`resolve_route`,
  `crates/sni_router/src/routing/mod.rs:102`).
- **`delivery`** — `Router::serve`, the per-connection pipeline, and
  shutdown (`crates/sni_router/src/delivery/server.rs:76`).
- **`cache`** — the optional `CachedLookup` decorator
  (`crates/sni_router/src/cache.rs:90`).
- **`metrics`** — typed `MetricEvent`s and the `MetricsSink` trait
  (`crates/sni_router/src/metrics.rs:188`).
- **`testing`** (feature `test-util`) — ClientHello builder, scripted
  lookups, recording metrics sink, for embedders' tests too.

# A connection, end to end

1. The accept loop takes a connection slot, then accepts
   (`crates/sni_router/src/delivery/server.rs:138`).
2. The ClientHello is read under one overall deadline and parsed
   (`crates/sni_router/src/delivery/connection.rs:158`).
3. The SNI is resolved with one lookup call: exact key, then wildcard
   (`crates/sni_router/src/delivery/connection.rs:187`). A miss or missing
   SNI gets a TLS `unrecognized_name` alert.
4. The router connects to the backend and replays every buffered byte,
   under one upstream deadline
   (`crates/sni_router/src/delivery/connection.rs:221`).
5. The connection commits to the proxy stage through the shutdown gate
   (`crates/sni_router/src/delivery/connection.rs:247`), then bytes are
   spliced both ways with half-close propagation until done
   (`crates/sni_router/src/delivery/connection.rs:257`).
6. Exactly one `ConnectionClosed` event and one log line are emitted
   (`crates/sni_router/src/delivery/connection.rs:115`).

Details: [connection-lifecycle](delivery/connection-lifecycle.md),
[shutdown-and-limits](delivery/shutdown-and-limits.md).

# Robustness guarantees

| Guarantee | Covered by |
|---|---|
| A ClientHello fragmented across TLS records (up to 32) is reassembled | `protocol::record::tests::every_two_record_split_parses_the_same`, `tests/proxy.rs::fragmented_hello_is_routed_and_replayed_as_sent` |
| Record framing and handshake framing are checked separately | `protocol::record::tests::every_prefix_is_incomplete`, `protocol::fixtures::*` |
| A slow-drip ClientHello is cut off by one overall deadline | `tests/proxy.rs::slow_hello_times_out` |
| Routes can change at runtime (SIGHUP; `CachedLookup` TTLs and `invalidate`) | `reload::tests::*`, `cache::tests::*` |
| Every route carries an explicit backend `host:port` | `lookup::types::tests::backend_parses_dns_ipv4_and_ipv6` |
| Half-close propagates in both directions | `tests/proxy.rs::half_close_reaches_backend_and_reply_still_flows` |
| The client random and session ID never leave the parser | `protocol/hello.rs` returns only the SNI |
| The handshake buffer is capped at `HelloLimits::max_wire_bytes()` | `delivery::hello_reader::tests::buffer_never_exceeds_wire_limit`, `buffer_capacity_stays_within_the_wire_limit` |
| An unknown or missing SNI gets a TLS `unrecognized_name` alert | `tests/proxy.rs::unknown_sni_gets_unrecognized_name_alert` |
