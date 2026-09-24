---
type: Component
title: CachedLookup
description: >
          A caching RouteLookup decorator for slow or remote route stores:
          TTLs with jitter, negative caching, single-flight loads that
          survive caller cancellation and panics, stale-if-error, and
          invalidation.
resource: crates/sni_router/src/cache.rs
tags: [lookup, cache, concurrency, embedding]
timestamp: 2026-09-24T00:00:00Z
---

`CachedLookup<L>` (`crates/sni_router/src/cache.rs:90`) wraps any
`RouteLookup`. It exists for the database-backed embedder. The TOML app
doesn't use it because its table is already in memory.

# Configuration

`CacheConfig` (`crates/sni_router/src/cache.rs:47`, defaults at
`crates/sni_router/src/cache.rs:73`):

| Field | Default | Meaning |
|---|---|---|
| `ttl` | 30s | How long a found route is served |
| `jitter` | 0.1 | Positive TTLs are scaled by `1 ± jitter` so entries loaded together don't expire together (`crates/sni_router/src/cache.rs:387`) |
| `negative_ttl` | 5s | How long "no route" is served |
| `max_entries` | 100,000 | When full: purge dead entries (at most once per `negative_ttl`), then evict oldest-first (`crates/sni_router/src/cache.rs:343`) |
| `stale_if_error` | off | If set, an expired entry may be served this long when its reload fails |
| `max_inflight_loads` | 1024 | Most loads running at once across all keys. When saturated, a miss serves its stale entry if allowed, otherwise fails closed without starting a load (`CacheEvent::Shed`) |
| `load_timeout` | 10s | Upper bound on one inner lookup. Loads outlive their callers, so without it a store that never answers would pin a task and an in-flight slot per hostname forever |

The cache key is the whole `RouteCandidates`, and the value is the whole
`RouteHits`. An empty value is the negative entry.

# Consistency contract

A changed or removed route becomes visible within `ttl`. A newly added
route becomes visible within `negative_ttl`. The cache has no change feed.
Embedders that have one call `invalidate(&RouteKey)`
(`crates/sni_router/src/cache.rs:154`), which drops every entry that key
could have answered. It works through a reverse index from each route key
to the entries whose candidate set contains it, so its cost is
proportional to the entries it drops, not the size of the cache. A full
scan took about 0.5 ms under the lock at 100k entries. The other option is
`clear()`
(`crates/sni_router/src/cache.rs:169`). Both also drop the matching
in-flight slots:

- A lookup that starts after the invalidation begins a fresh load instead
  of joining the old one.
- A load caches its result only if it still owns its slot
  (`crates/sni_router/src/cache.rs:431`). A load that was running when its
  key was invalidated still answers the callers already waiting on it, but
  it can't overwrite newer data.
- Invalidating one key doesn't stop loads for other keys from being
  cached.

# Single-flight loads

Concurrent misses for the same candidates share one load: the first
caller registers an in-flight slot and later callers subscribe to it
(`crates/sni_router/src/cache.rs:186`). A hot entry expiring sends one
query to the store, not one per connection.

The load runs on its **own spawned task**
(`crates/sni_router/src/cache.rs:250`, spawn at
`crates/sni_router/src/cache.rs:275`), not on any caller's task:

- A caller that times out or is cancelled never cancels the load. The
  result is cached even if nobody is still waiting.
- Each load is bounded by `load_timeout`. A hung store therefore can't
  accumulate stuck loads, even when a client sends a stream of distinct
  hostnames.
- The task owns a `LoadGuard` (`crates/sni_router/src/cache.rs:400`).
  If the load panics or its task is dropped, the guard's `Drop`
  (`crates/sni_router/src/cache.rs:458`) clears the in-flight slot and
  wakes waiters with an error, so a key is never left stuck "in flight".
  The next lookup starts a fresh load.
- Errors are never cached.

# Stale-if-error

With `stale_if_error` set, a caller that finds an expired entry still
inside its stale window keeps a copy. If the reload fails, it serves that
copy and emits `CacheEvent::StaleServed`
(`crates/sni_router/src/cache.rs:229`). Past the window, the error goes
back to the router, which fails the connection closed.

# Metrics

Each outcome emits a `MetricEvent::Cache(CacheEvent)`: `Hit`,
`NegativeHit`, `Miss`, `Coalesced`, `StaleServed`, `LoadError`, `Shed`
(`crates/sni_router/src/metrics.rs:152`).
