# Initial design: sni_router

Point-in-time design record: why the router is built the way it is, and
which alternatives were rejected. For current, implemented behavior see
`docs/knowledge/`.

## Goals

- Route TLS connections to backends by SNI **without terminating TLS**:
  read the ClientHello, pick a backend, replay the buffered bytes, and splice
  the streams.
- **Library first.** Parsing, routing policy, timeouts, proxying, and
  shutdown live in `crates/sni_router`. Where routes are stored is pluggable
  through one trait, so the same library can be embedded behind a file, a
  database, or a service.
- Ship a small open-source app (`crates/sni_router_app`) that reads routes
  from a TOML file and reloads them on SIGHUP.

## Key decisions

- **Read-only lookup.** The library only resolves hostnames to backends.
  Managing routes (writes, admin APIs) belongs to whatever owns the store.
- **One `host:port` backend per route.** DNS names are resolved at connect
  time, through a TTL cache that makes lookups single-flight and caps how
  many run at once.
- **Matching.** Hostnames are case-insensitive. An exact match wins, then a
  single-label wildcard (`*.example.com`). There is no default backend. An
  unknown or missing SNI gets a TLS `unrecognized_name` alert, and lookup
  errors or timeouts fail closed.
- **Batched lookup contract.** `RouteLookup::lookup` receives the ordered
  candidate keys (exact, then wildcard) and returns every literal hit. The
  library owns wildcard expansion and precedence, so a database answers with
  one indexed `IN` query from one snapshot.
- **ClientHello limits.** Three independent bounds, each tested on its own:
  - the per-record payload (fixed by TLS)
  - the declared handshake length (default 32 KiB)
  - the record count (default 32)

  Re-checking a growing buffer costs O(records), not O(bytes). Real
  captured hellos, including a post-quantum Firefox hello, must parse under
  the defaults.
- **Deadlines.** One each for:
  - the whole ClientHello
  - the lookup
  - DNS, connect, and replay combined

  Plus an idle timeout (30 minutes by default) once proxying.
- **Shutdown.** Three signals with one job each:
  1. stop accepting
  2. cancel handshakes
  3. force-close after the grace period

  A mutex-guarded gate decides whether each connection counts as
  handshaking or proxying, so shutdown can't race a connection that is just
  finishing its handshake.
- **Metrics as typed events.** The library emits `MetricEvent`s to a
  `MetricsSink`, and embedders choose the exporter and names. Client bytes
  are counted from accept and reported once at close, by a drop guard.
- **Optional `CachedLookup`.** For slow or remote stores. It has TTL with
  jitter and negative caching. Loads are single-flight, run on their own
  task, and are bounded in time and count. Stale-if-error is opt-in, and
  invalidation is indexed.

## Rejected alternatives

- **rustls in dev-dependencies (real TLS handshakes in tests):** its crypto
  backends pull in licenses outside `deny.toml`'s allowlist, and the router
  never terminates TLS. Byte-exact echo backends plus captured real
  ClientHellos (`scripts/capture_client_hello.py`) test more precisely.
- **`moka` for `CachedLookup`:** the needed behavior is small enough to
  hand-roll and test deterministically with paused time: single-flight that
  survives cancellation and panics, invalidation by slot ownership, and
  stale-if-error.
- **`arc-swap` for route reload:** `RwLock<Arc<InMemoryLookup>>` holds the
  lock only for an `Arc` clone and needs no new dependency.
- **The `metrics` facade crate in the library:** a global recorder is
  awkward for an embedder with its own exporter.
- **`async-trait` for `RouteLookup`:** a hand-written boxed future keeps the
  trait dyn-compatible without a dependency.
- **A single-key lookup (`lookup(&RouteKey)`):** a wildcard hit would take
  two store round trips, and exact and wildcard wouldn't share a snapshot.

## Review record

Codex reviewed the design and each change set adversarially:
- the plan, in two rounds
- the implementation, in two rounds
- security and performance, in two rounds with the reviewers' roles swapped
- each set of fixes

Every finding was either fixed with a regression test or documented.
Performance decisions were measured on loopback, for example the 32 KiB
copy-buffer default, which is the throughput knee. See
`docs/knowledge/delivery/connection-lifecycle.md`.

## Deferred

- PROXY protocol v2 to backends, to preserve the client IP.
- TCP keepalive, and `SO_REUSEPORT`/backlog options on listeners.
- Sharding `CachedLookup` state.
- Coverage-guided fuzzing in CI.
