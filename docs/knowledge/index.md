---
okf_version: "0.1"
---

# sni_router Knowledge Bundle

* [sni-router-overview](sni-router-overview.md) - What sni_router is, the library/app split, and
  how a connection flows from accept to proxied backend. Start here.
* [embedding](embedding.md) - How to embed the library with your own route store (e.g. a
  database), and a deployment checklist.
* [lookup](lookup/) - The `RouteLookup` extension point and the `CachedLookup` decorator.
* [routing](routing/) - Hostname matching policy: exact, then single-label wildcard; fail closed.
* [protocol](protocol/) - ClientHello parsing and the limits that bound it.
* [delivery](delivery/) - Connection lifecycle, timeouts, backpressure, and shutdown.
* [observability](observability/) - Metric events and the app's Prometheus names.
* [app](app/) - The TOML-driven `sni_router` binary: config, reload, admin endpoints.
