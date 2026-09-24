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
listener. A single accept loop takes a slot **before** accepting
(`crates/sni_router/src/delivery/server.rs:150`). It then accepts from
whichever listener is ready first, starting from a rotating index so a busy
listener can't starve the others (`accept_any`,
`crates/sni_router/src/delivery/server.rs:186`).

- **Backpressure:** when all slots are in use, new connections wait in the
  kernel's accept backlog instead of being accepted and starved.
- **Idle listeners:** at most one slot is ever held waiting for a
  connection, however many listeners there are. An idle listener can't
  hold a slot that a connection on another listener needs, even with
  `max_connections` smaller than the number of listeners.
- **Release:** the slot is released when the connection task ends,
  including on a timeout or a panic.
- **Accept errors:** a failed `accept` (e.g. EMFILE) returns its slot and
  then backs off 50 ms, so the loop doesn't spin
  (`crates/sni_router/src/delivery/server.rs:35`).

# Shutdown sequence

`Router::serve` (`crates/sni_router/src/delivery/server.rs:76`) runs
until the caller cancels its `shutdown` token, then uses three signals,
each with one job:

1. **Stop accepting.** The caller's token ends the accept loop, including
   one parked waiting for a slot.
2. **Cancel handshakes.** `ProxyGate::close`
   (`crates/sni_router/src/delivery/gate.rs:69`, called at
   `crates/sni_router/src/delivery/server.rs:115`) fires the
   handshake-cancel token. Every connection still reading its hello,
   looking up its route, connecting, or replaying ends at once with
   `ShutdownCancelled`.
3. **Drain.** Proxying connections keep running for `shutdown_grace`
   (default 30s) (`crates/sni_router/src/delivery/server.rs:122`).
4. **Force.** Whatever is left is cancelled through the force token, which
   only the proxy stage observes. Those connections end with
   `ShutdownForced` (`crates/sni_router/src/delivery/server.rs:130`).
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
