# AGENTS.md

High-level summary for `crates/sni_router/src/delivery`.

## Responsibility

All socket I/O: accepting, reading the ClientHello, connecting upstream, replaying, proxying, and shutdown.

- `server.rs` — `Router`, accept loops, connection slots, the shutdown sequence.
- `connection.rs` — one connection's pipeline and its single `ConnectionClosed` event / log line.
- `gate.rs` — `ProxyGate`, the mutex-guarded commit point between "handshaking" and "proxying".
- `hello_reader.rs` — bounded, deadline-free read loop (callers apply the deadline).
- `proxy.rs` — `copy_bidirectional` plus idle watchdog and force-cancel.
- `metered.rs` — client-side byte counters (from accept; reported at close by `connection.rs`'s `ConnectionRecord` drop guard) and the optional activity stamp.
- `resolver.rs` — backend DNS: single-flight lookups, TTL cache, stale-on-error, and a cap on concurrent blocking lookups.
- `mod.rs` — `RouterConfig`.

## Boundaries

- Every pre-proxy await must race the handshake-cancel token (use `cancellable`); only the proxy stage observes the force token. Don't add an await that bypasses both.
- Connection slots are acquired before `accept`; keep it that way so overload queues in the kernel.
- Decision logic belongs in `routing`/`protocol`; keep this layer about I/O and lifecycle.

## Testing Expectations

- Pure pieces (gate, reader) have unit tests here; lifecycle behavior is tested in `crates/sni_router/tests/proxy.rs` over real sockets, asserting outcomes.
- New stages or outcomes need a shutdown-in-that-stage test.
