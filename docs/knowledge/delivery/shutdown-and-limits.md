---
type: Behavior
title: Shutdown and Connection Limits
description: >
          How max_connections applies backpressure, and how shutdown
          cancels handshakes immediately, drains proxied connections for a
          grace period, then forces the rest, without races at the
          handshake-to-proxy boundary.
resource: crates/sni_router/src/delivery/server.rs
tags: [delivery, shutdown, concurrency, backpressure]
timestamp: 2026-09-24T00:00:00Z
---

# Connection slots

`max_connections` (default 10,000) is one semaphore shared by every
listener. The accept loop takes a slot **before** calling `accept`
(`crates/sni_router/src/delivery/server.rs:146`). When all slots are in
use, new connections wait in the kernel's accept backlog instead of being
accepted and starved. The slot is released when the connection task ends,
including on a timeout or a panic. A failed `accept` (e.g. EMFILE) backs
off 50ms so the loop doesn't spin
(`crates/sni_router/src/delivery/server.rs:33`).

# Shutdown sequence

`Router::serve` (`crates/sni_router/src/delivery/server.rs:72`) runs
until the caller cancels its `shutdown` token, then uses three signals,
each with one job:

1. **Stop accepting.** The caller's token ends every accept loop, including
   one parked waiting for a slot.
2. **Cancel handshakes.** `ProxyGate::close`
   (`crates/sni_router/src/delivery/gate.rs:69`, called at
   `crates/sni_router/src/delivery/server.rs:111`) fires the
   handshake-cancel token. Every connection still reading its hello,
   looking up its route, connecting, or replaying ends at once with
   `ShutdownCancelled`.
3. **Drain.** Proxying connections keep running for `shutdown_grace`
   (default 30s) (`crates/sni_router/src/delivery/server.rs:118`).
4. **Force.** Whatever is left is cancelled through the force token, which
   only the proxy stage observes. Those connections end with
   `ShutdownForced` (`crates/sni_router/src/delivery/server.rs:126`).
   `serve` then waits for every task and returns.

# The proxy gate

Shutdown has to classify every connection as either "still handshaking"
(cancel now) or "proxying" (let it drain). A connection that finishes its
replay at the same moment shutdown starts must land on exactly one side.

`ProxyGate::enter` (`crates/sni_router/src/delivery/gate.rs:58`) and
`ProxyGate::close` take the same mutex:

- If `enter` runs first, the connection is counted as proxying and gets
  the grace period.
- If `close` runs first, `enter` returns `None` and the connection ends
  `ShutdownCancelled`.

There's no window where a connection is neither cancelled nor drained.
`gate::tests::racing_enter_and_close_always_land_on_one_side` checks this
under a barrier, and
`tests/proxy.rs::shutdown_racing_many_connections_leaves_none_stuck`
checks it end to end.
