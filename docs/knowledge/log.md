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
  * `idle_timeout` now defaults to 30 minutes (app: `idle_timeout_secs`,
    where 0 disables it), so idle sessions can't exhaust connection slots
  * `CachedLookup::invalidate` uses a reverse index
  * the example admin listener is on loopback
  * CI actions and Docker base images are pinned by SHA/digest, with
    Dependabot updates
* **Update**: After a ClientHello-parsing allocation review: extension
  types seen so far are stack-inline for up to 32 types (sized by count,
  not by the extensions block's byte length, since one large extension
  like `padding` would otherwise force a large allocation on sparse
  untrusted input — a correction after PR review flagged the first,
  byte-length-derived version), and record payload ranges are
  stack-inline for up to 4 records, both before spilling to a heap `Vec`.
  `scan_records` — rerun from byte 0 on every read-loop iteration for an
  incomplete hello — doesn't allocate at all in the common
  single/few-record, well-under-32-extensions case. See
  [ClientHello parsing](protocol/client-hello-parsing.md).
* **Update**: Proxy stage now uses `splice(2)` through an in-kernel pipe on
  Linux when the syscall probes as usable, so payload bytes never cross
  into a userspace buffer; falls back to
  `tokio::io::copy_bidirectional_with_sizes` elsewhere, if the probe fails
  (e.g. seccomp), or if `SNI_ROUTER_DISABLE_SPLICE` is set. Measured
  ~1.3–1.7× the userspace copy's loopback throughput
  (`examples/copy_benchmark.rs`, kept in-tree for re-measuring). After
  adversarial review: both directions' pipes are now built up front, before
  either socket is touched, so a setup failure (fd exhaustion) falls back
  instead of failing the connection; and client-read accounting moved to
  the read side (matching `MeteredStream`), since counting it only after
  the write to upstream succeeded under-counted bytes the client had
  already sent when upstream stalled or reset. A second review round found
  that admitting every connection to splice regardless of fd pressure could
  still exhaust descriptors before any one connection's own `prepare()`
  call failed (starving accept/connect instead). Fixed with a per-`Router`
  fd budget (a semaphore sized once from `RLIMIT_NOFILE` and
  `max_connections`): splice only runs while the budget has room. A third
  review round then found that budget was computed independently per
  `Router::serve` call from the *whole* process's `RLIMIT_NOFILE`, so two
  concurrent `Router`s (a supported embedding scenario) would each admit up
  to their own full share and together double-count the same fd limit.
  Fixed by making it one semaphore for the whole process
  (`splice::admission`, not keyed to any `Router`'s `max_connections`):
  splice may use at most half of `RLIMIT_NOFILE`, in units of 4 fds (one
  connection's two pipes), leaving the other half always available for
  sockets, listeners, and anything else in the process. See
  [connection lifecycle](delivery/connection-lifecycle.md).
