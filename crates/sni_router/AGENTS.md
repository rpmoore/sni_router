# AGENTS.md

High-level summary for `crates/sni_router` (the library).

## Responsibility

Routes TLS connections to backends by SNI without terminating TLS. Everything an embedder needs lives here: parsing, routing policy, timeouts, proxying, shutdown, and an optional lookup cache. The open-source app and the closed-source database-backed service both depend on it.

- `src/protocol` — pure ClientHello parsing and limits.
- `src/lookup` — the `RouteLookup` trait and domain types (the embedder contract).
- `src/routing` — candidate keys and precedence (exact over single-label wildcard); fail closed.
- `src/delivery` — `Router::serve`, the per-connection pipeline, the proxy gate, and shutdown.
- `src/cache.rs` — `CachedLookup` (TTL, single-flight, stale-if-error).
- `src/metrics.rs` — typed `MetricEvent`s and the `MetricsSink` trait.
- `src/testing.rs` — `test-util` feature helpers (ClientHello builder, scripted lookup, recording sink), public so embedders can test with them.

## Boundaries

- Dependencies stay minimal: `tokio`, `tokio-util`, `tracing`. No serde/TOML/HTTP/exporters; those belong in apps.
- Everything `pub` is a compatibility commitment to an external embedder. Prefer opaque structs with accessors and `#[non_exhaustive]` enums/configs; call out public API changes in PRs.
- Never log or return client random, session IDs, or raw ClientHello bytes.

## Testing Expectations

- Unit tests live beside the code; parser fixtures are in `src/protocol/testdata` (captured with `scripts/capture_client_hello.py`).
- `tests/proxy.rs` drives real loopback sockets for end-to-end behavior (routing, replay, half-close, limits, timeouts, backpressure, shutdown). Assert on `ConnectionOutcome`s via `RecordingMetrics`.
- Cache and routing time behavior use `#[tokio::test(start_paused = true)]`; socket tests use short real timeouts.
