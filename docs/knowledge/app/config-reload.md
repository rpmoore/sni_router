---
type: Behavior
title: Reload, Shutdown, and Admin Endpoints
description: >
          SIGHUP swaps routes atomically if the whole file validates;
          SIGTERM/SIGINT drain via the library's shutdown sequence while
          /health reports 503; the admin listener serves /metrics and health.
resource: crates/sni_router_app/src/reload.rs
tags: [app, reload, sighup, shutdown, health]
timestamp: 2026-09-24T00:00:00Z
---

# SIGHUP route reload

`spawn_sighup_reload` (`crates/sni_router_app/src/reload.rs:62`) re-reads
the config file on every SIGHUP, off the async workers, in
`spawn_blocking` (`crates/sni_router_app/src/reload.rs:84`), and calls
`reload_from_path` (`crates/sni_router_app/src/reload.rs:44`):

- **The whole file must validate.** One bad route, a TOML syntax error, or
  a missing file keeps the current routes and logs an error.
- **Only `[[routes]]` is applied.** If `[server]` or `[admin]` changed,
  the app logs a warning that those need a restart
  (`crates/sni_router_app/src/reload.rs:50`).
- **The swap is atomic.** `FileRouteLookup`
  (`crates/sni_router_app/src/config/routes.rs:63`) holds
  `RwLock<Arc<InMemoryLookup>>`. A lookup clones the `Arc` and answers from
  that snapshot (`crates/sni_router_app/src/config/routes.rs:82`), and
  `replace` swaps the pointer
  (`crates/sni_router_app/src/config/routes.rs:75`). A connection that is
  already proxying holds its resolved backend, so a reload never affects
  it.

# SIGTERM / SIGINT

`main` cancels the shutdown token on SIGTERM or SIGINT. `BoundApp::run`
(`crates/sni_router_app/src/lib.rs:114`) then:

1. switches `/health` to 503 `draining` right away
   (`crates/sni_router_app/src/lib.rs:133`), so load balancers stop sending
   new connections
2. runs the library's
   [shutdown sequence](../delivery/shutdown-and-limits.md) through
   `Router::serve` (`crates/sni_router_app/src/lib.rs:137`)
3. stops the admin server only after the router has drained
   (`crates/sni_router_app/src/lib.rs:139`), so probes see the drain
   rather than a refused port

# Admin endpoints

`AdminServer` (`crates/sni_router_app/src/admin.rs`) is a small hyper
HTTP/1 server. `AdminLimits`
(`crates/sni_router_app/src/admin.rs:45`) sets these bounds:

- at most 256 connections
- one request per connection, with no keep-alive
- request headers must arrive within 2s
- each connection is cut off after 5s

Idle or trickling clients therefore release their slots quickly and can't
starve health probes; a test checks this. The port is unauthenticated, so
these limits raise the cost of starving it without making it immune. Keep
`[admin]` on loopback or a private interface (for Kubernetes, the pod IP).

Routes (`crates/sni_router_app/src/admin.rs:198`):

- `GET /metrics`: Prometheus text format
- `GET <health_path>`: `200 ok` normally, `503 draining` once shutdown
  starts
- anything else: 404, or 405 for non-GET

# Startup order

`BoundApp::bind` (`crates/sni_router_app/src/lib.rs:50`) binds every data
listener and the admin listener before serving. A port conflict fails
startup with the address in the error, before anything reports healthy.
