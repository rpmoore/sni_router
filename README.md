# sni_router

Routes TLS connections to backends by SNI **without terminating TLS**. It
reads the ClientHello, finds the backend for its server name, replays the
buffered bytes to that backend, and splices the two TCP streams.

This workspace has two crates:

- **[`sni_router`](crates/sni_router)**: the library. It parses the
  ClientHello, applies the routing policy, and handles timeouts, proxying and
  graceful shutdown. Route storage is pluggable through the `RouteLookup`
  trait, so the same library can sit behind a TOML file, a database, or any
  other store.
- **[`sni_router_app`](crates/sni_router_app)**: the `sni_router` binary. It
  reads routes from a TOML file, reloads them on SIGHUP, and serves
  Prometheus metrics and a health check.

## Run

```bash
cargo build --release
SNI_ROUTER_CONFIG=examples/sni_router.toml target/release/sni_router
```

`examples/sni_router.toml` documents every setting. A minimal config:

```toml
[server]
listen = ["0.0.0.0:8080"]

[[routes]]
hostname = "api.example.com"
backend = "api-svc.default.svc.cluster.local:8443"

[[routes]]
hostname = "*.apps.example.com"   # exactly one label: web.apps.example.com
backend = "10.0.0.5:443"
```

- **Matching:** matching is case-insensitive. An exact hostname beats a
  wildcard. A connection with no match or no SNI gets a TLS
  `unrecognized_name` alert.
- **Reload:** `kill -HUP` reloads the routes. The whole file must be valid,
  otherwise the current routes stay.
- **Shutdown:** `SIGTERM` makes `/health` return 503, cancels connections
  that are still in the TLS handshake, and lets proxied connections drain for
  `shutdown_grace_secs` before closing them.
- **Idle connections:** proxied connections with no traffic are closed
  after 30 minutes by default (`idle_timeout_secs`; `0` disables).
- **Admin:** the admin listener (default `127.0.0.1:8081`) serves
  `GET /metrics` and `GET /health`.

Docker:

```bash
docker build -t sni_router .
docker run -v $PWD/examples/sni_router.toml:/etc/sni_router/sni_router.toml -p 8080:8080 sni_router
```

## Embed

```rust
let router = sni_router::Router::new(Arc::new(my_lookup), Arc::new(my_metrics), RouterConfig::default());
let info = ListenerInfo::from_listener(&listener)?;
router.serve(vec![(listener, info)], shutdown).await?;
```

For how to implement the lookup trait, add a cache with `CachedLookup`, and
a deployment checklist, see
[docs/knowledge/embedding.md](docs/knowledge/embedding.md).

## Documentation

- [docs/knowledge/](docs/knowledge/index.md): how the code works today (an
  OKF bundle).
- [AGENTS.md](AGENTS.md) and [RUST.md](RUST.md): contributor and agent
  workflow.

## License

Apache-2.0
