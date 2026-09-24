---
type: Configuration
title: TOML Config
description: >
          The sni_router app's config file: where it's read from, every key
          and default, and the validation that runs before anything binds.
resource: crates/sni_router_app/src/config/mod.rs
tags: [app, config, toml, validation]
timestamp: 2026-09-24T00:00:00Z
---

The path comes from `SNI_ROUTER_CONFIG`
(`crates/sni_router_app/src/config/mod.rs:34`). If it's unset or blank,
the app uses `sni_router.toml` in the working directory
(`crates/sni_router_app/src/config/mod.rs:78`). A missing or invalid file
fails startup; there are no built-in routes. The annotated
reference is `examples/sni_router.toml`.

# `[server]` (restart to change)

Defaults at `crates/sni_router_app/src/config/mod.rs:134`:

| Key | Default | Maps to |
|---|---|---|
| `listen` | `["0.0.0.0:8080"]` | listeners |
| `max_connections` | 10000 | `RouterConfig::max_connections` |
| `client_hello_timeout_ms` | 5000 | `client_hello_timeout` |
| `lookup_timeout_ms` | 2000 | `lookup_timeout` |
| `upstream_timeout_ms` | 5000 | `upstream_timeout` (DNS + connect + replay) |
| `idle_timeout_secs` | 1800 (30 min; 0 disables) | `idle_timeout` |
| `shutdown_grace_secs` | 30 | `shutdown_grace` |
| `max_client_hello_bytes` | 32768 | `HelloLimits::max_handshake_bytes` |
| `max_client_hello_records` | 32 | `HelloLimits::max_records` |
| `dns_cache_ttl_secs` | 30 | `dns_cache_ttl` (0 = resolve per connection) |
| `max_concurrent_dns_lookups` | 64 | `max_concurrent_dns_lookups` |
| `copy_buffer_bytes` | 32768 | `copy_buffer_size` |

# `[admin]` (restart to change)

`listen` defaults to `127.0.0.1:8081` and `health_path` to `/health`
(`crates/sni_router_app/src/config/mod.rs:250`). The admin server always
serves `/metrics` as well.

The admin port is unauthenticated. The default (loopback) and the example
config keep it private. For Kubernetes probes, which connect to the pod IP,
bind `0.0.0.0:8081` and restrict the port with a NetworkPolicy.

# `[[routes]]` (reloadable)

Each entry has a `hostname` (exact or `*.` wildcard) and a `backend`
(`host:port`, `ip:port`, or `[v6]:port`).

# Validation

`RawServer::validate` (`crates/sni_router_app/src/config/mod.rs:155`),
`RawAdmin::validate` (`crates/sni_router_app/src/config/mod.rs:260`), and
`build_route_table` (`crates/sni_router_app/src/config/routes.rs:30`)
reject:

- unknown keys anywhere (`deny_unknown_fields`)
- an empty or duplicate `listen` list
- any value outside its range. Every duration and size has an upper
  bound, so a typo can't overflow time arithmetic (an idle timeout near
  `u64::MAX` seconds used to panic each proxied connection):

  | Key | Range |
  |---|---|
  | `max_connections` | 1..=1,000,000 |
  | hello, lookup, and upstream timeouts | 1..=3,600,000 ms |
  | `idle_timeout_secs` | 0..=30 days |
  | `shutdown_grace_secs` | 0..=3600 |
  | `max_client_hello_bytes` | 1024..=65536 |
  | `max_client_hello_records` | 1..=256 |
  | `dns_cache_ttl_secs` | 0..=86400 |
  | `max_concurrent_dns_lookups` | 1..=256, well under tokio's 512 blocking threads |
  | `copy_buffer_bytes` | 1024..=1 MiB |
- a `health_path` that doesn't start with `/`, contains whitespace, or is
  `/metrics`
- invalid hostnames, including `*.com`, trailing dots, and IP literals;
  invalid backends (missing port, port 0, bare IPv6); duplicate
  hostnames after lowercasing
  (`crates/sni_router_app/src/config/routes.rs:50`)

Errors name the field or `routes[i]` entry at fault. An empty route list
is allowed: every connection gets `unrecognized_name`, and startup logs a
warning.
