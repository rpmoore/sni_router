# Bundle Update Log

## 2026-09-24
* **Initialization**: Created the empty bundle root.
* **Creation**: Documented the initial implementation — [overview](sni-router-overview.md),
  [embedding](embedding.md), [lookup contract](lookup/lookup-contract.md),
  [cached lookup](lookup/cached-lookup.md), [hostname matching](routing/hostname-matching.md),
  [ClientHello parsing](protocol/client-hello-parsing.md),
  [connection lifecycle](delivery/connection-lifecycle.md),
  [shutdown and limits](delivery/shutdown-and-limits.md), [metrics](observability/metrics.md),
  [TOML config](app/toml-config.md), and [reload/shutdown/admin](app/config-reload.md).
* **Update**: After adversarial review — `CachedLookup` gained `load_timeout`, `max_inflight_loads`
  (`CacheEvent::Shed`), and per-key invalidation via in-flight slot ownership (replacing a global
  epoch); admin HTTP gained header-read/connection limits
  ([cached lookup](lookup/cached-lookup.md), [reload/shutdown/admin](app/config-reload.md)).
* **Update**: After security and performance review:
  * a DNS resolver cache with a cap on concurrent lookups
  * byte metrics reported once at close
  * activity tracking only when `idle_timeout` is set
  * a configurable copy buffer size
  * per-connection close logs moved to `debug`
  * single-flight DNS lookups
  * client bytes metered from accept, with a drop guard that records the
    close (including an `Aborted` outcome for panicked tasks)
  * exact growth for the ClientHello buffer
  * overflow-safe time arithmetic, and upper bounds on every config value

  See [connection lifecycle](delivery/connection-lifecycle.md) and
  [TOML config](app/toml-config.md).
* **Update**: After the second review round (measured performance review,
  Codex security review):
  * the default copy buffer is now 32 KiB, the measured throughput knee
  * `idle_timeout` now defaults to 30 minutes (configurable, 0 disables),
    so idle sessions can't exhaust connection slots
  * `CachedLookup::invalidate` uses a reverse index
  * the example admin listener is on loopback
  * CI actions and Docker base images are pinned by SHA/digest, with
    Dependabot updates
