---
type: Interface
title: Metrics Events and Prometheus Names
description: >
          The library emits typed MetricEvents to a MetricsSink supplied by
          the embedder; the app maps them to Prometheus series with
          network/address labels.
resource: crates/sni_router/src/metrics.rs
tags: [observability, metrics, prometheus, api]
timestamp: 2026-09-24T00:00:00Z
---

# Library: typed events

The library never names a metric series. It calls
`MetricsSink::record(MetricEvent)`
(`crates/sni_router/src/metrics.rs:231`) inline on the connection task,
so sinks must be cheap and non-blocking. Embedders map events onto
whatever exporter and naming scheme they use; `NoopMetrics` discards them.

Every per-connection event carries `listener: &ListenerInfo`
(`crates/sni_router/src/metrics.rs:29`): the network, the bound address,
and an optional name. Series stay distinct per listener.

`MetricEvent` (`crates/sni_router/src/metrics.rs:188`):

| Event | Fields |
|---|---|
| `ConnectionOpened` | listener |
| `ConnectionClosed` | listener, outcome, lifetime |
| `BytesRead` / `BytesWritten` | listener, bytes (client side, from accept, reported once at close, including for aborted tasks) |
| `RouteLookup` | listener, outcome (`found`/`not_found`/`error`/`timeout`), elapsed |
| `UpstreamConnect` | listener, ok, elapsed (connect + replay) |
| `Cache(CacheEvent)` | from `CachedLookup` |

The enum, its variants, and its outcome enums are `#[non_exhaustive]`, so
match them with `..` and a `_ =>` arm. New events and fields can be
added in minor releases.

# App: Prometheus names

`OtelMetrics` (`crates/sni_router_app/src/otel_metrics.rs`) spells out
the final names and disables the exporter's suffixing
(`crates/sni_router_app/src/otel_metrics.rs:63`):

| Series | Labels |
|---|---|
| `snirouter_connections_opened_total` | network, address |
| `snirouter_connections_closed_total` | network, address, outcome |
| `snirouter_open_connections` | network, address |
| `snirouter_bytes_read_total`, `snirouter_bytes_written_total` | network, address |
| `snirouter_connection_duration_seconds` (5 ms to 300 s buckets) | network, address |
| `snirouter_route_lookup_duration_seconds` | network, address, outcome |
| `snirouter_upstream_connect_duration_seconds` | network, address, result |
| `snirouter_cache_events_total` | event (`hit`, `negative_hit`, `miss`, `coalesced`, `stale_served`, `load_error`, `shed`) |

Listener labels are built once per listener and reused
(`crates/sni_router_app/src/otel_metrics.rs:121`), so per-read byte
events don't allocate. The app e2e test pins these names
(`crates/sni_router_app/tests/e2e_config_toml.rs`).

Embedders that need different names can map the same events to their own
names in their own sink.
