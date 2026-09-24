---
type: Guide
title: Embedding the Library
description: >
          How another application (e.g. a service backed by a database)
          embeds sni_router, and a checklist for deploying it.
resource: crates/sni_router/src/lib.rs
tags: [embedding, api, deployment]
timestamp: 2026-09-24T00:00:00Z
---

# Minimal embedding

```rust
use std::sync::Arc;
use sni_router::{CacheConfig, CachedLookup, ListenerInfo, Router, RouterConfig};
use tokio_util::sync::CancellationToken;

let lookup = CachedLookup::new(MyDbLookup::new(pool), CacheConfig::default(), metrics.clone());
let router = Router::new(Arc::new(lookup), metrics, RouterConfig::default());

let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
let info = ListenerInfo::from_listener(&listener)?;
let shutdown = CancellationToken::new();
router.serve(vec![(listener, info)], shutdown.clone()).await?;
```

You supply:

1. **A `RouteLookup`.** See [lookup-contract](lookup/lookup-contract.md).
   For a database, answer both candidate keys with one query
   (`WHERE dns_name IN ($1, $2)`), store wildcard routes as literal
   `*.example.com` rows, return an empty `RouteHits` for "no route", and
   return `Err` only for transient failures.
2. **A `MetricsSink`.** See [metrics](observability/metrics.md). Match
   `MetricEvent` with a `_ =>` arm.
3. **Optionally a `CachedLookup`.** See
   [cached-lookup](lookup/cached-lookup.md). If your store has a change
   feed, call `invalidate` from it; otherwise staleness is bounded by
   `ttl` and `negative_ttl`.
4. **Your own admin, health, and signal handling.** The app's `admin.rs`
   and `main.rs` are a reference.

Test your lookup with the `test-util` feature: `ClientHelloBuilder`,
`RecordingMetrics`, and `ScriptedLookup`.

# Deployment checklist

For any deployment, whether of the app or your own embedding:

- [ ] **Ports:** the data-plane listener(s) for TLS passthrough (the app
  defaults to 8080), and the admin listener for health and metrics.
- [ ] **Health:** point liveness and readiness probes at the health path.
  It returns 503 once shutdown starts, so load balancers drain first. Set
  the shutdown grace period (`shutdown_grace_secs` in the app,
  `RouterConfig::shutdown_grace` when embedding) to fit your longest
  acceptable drain.
- [ ] **Admin exposure:** the admin port is unauthenticated. Keep it on
  loopback or a private interface; for Kubernetes probes, restrict it with a
  NetworkPolicy.
- [ ] **Route data:** every route needs an explicit backend `host:port`.
  Wildcards are stored as literal `*.example.com` keys.
- [ ] **Limits:** size the connection limit, copy buffer, and idle timeout
  for your traffic, using the memory sizing in
  [connection-lifecycle](delivery/connection-lifecycle.md). In the app's
  TOML these are `[server]` `max_connections`, `copy_buffer_bytes`, and
  `idle_timeout_secs`. When embedding, they are the `RouterConfig` fields
  `max_connections`, `copy_buffer_size`, and `idle_timeout`. See
  [TOML config](app/toml-config.md) for the full mapping. Put per-client
  connection limits in front of the router if clients are untrusted.
- [ ] **Metrics:** map `MetricEvent`s to your own names, or reuse the
  app's (see [metrics](observability/metrics.md)).
