---
type: Interface
title: RouteLookup Contract
description: >
          The extension point between the router and a route store: inputs,
          outputs, error semantics, and why wildcard expansion and precedence
          stay in the library.
resource: crates/sni_router/src/lookup/mod.rs
tags: [lookup, api, extensibility, embedding]
timestamp: 2026-09-24T00:00:00Z
---

`RouteLookup` (`crates/sni_router/src/lookup/mod.rs:52`) is the only
contract between the router and wherever routes live. The TOML app's
`FileRouteLookup` and a database-backed lookup in another service both
implement it.

```rust
pub trait RouteLookup: Send + Sync {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a>;
}
```

`LookupFuture` is a boxed `Send` future, so the trait works as
`Arc<dyn RouteLookup>` without the `async-trait` crate. There is also a
blanket impl for `Arc<T>` (`crates/sni_router/src/lookup/mod.rs:56`).

# Input: candidate keys

The router passes a `RouteCandidates`
(`crates/sni_router/src/lookup/mod.rs:65`): the exact key for the SNI
hostname and, when the hostname has at least three labels, its wildcard
key (`crates/sni_router/src/lookup/mod.rs:71`). For
`web.apps.example.com` that's `web.apps.example.com` and
`*.apps.example.com`. Every key is derived from a validated, lowercase
`Hostname` (`crates/sni_router/src/lookup/types.rs:75`), so lookups never
see mixed case, trailing dots, IP literals, or non-ASCII.

# Output: every literal hit

Return a `RouteHits` (`crates/sni_router/src/lookup/mod.rs:98`) holding
every stored route whose key is *literally* one of the candidates, in any
order. Wildcard routes are stored as literal `*.example.com` keys. A
database needs one indexed query:

```sql
SELECT dns_name, backend FROM routes WHERE dns_name IN ($1, $2)
```

Rules:

- **No wildcard expansion and no precedence in the implementation.** The
  router picks the winner (exact over wildcard) in `select_route`
  (`crates/sni_router/src/routing/mod.rs:76`). One call and one snapshot
  answer both keys, so a route change can't produce a mix of two states.
- **"No route" is an empty `RouteHits`, not an error.**
- **`Err(LookupError)` means a transient failure only** (store unreachable,
  query timeout). The router fails the connection closed; it never falls
  back to a partial answer. `LookupError`
  (`crates/sni_router/src/lookup/mod.rs:138`) wraps an `Arc<dyn Error>`,
  so one failure can be handed to every waiter on a shared load.
- **Be cancel-safe.** The router bounds each call with `lookup_timeout`
  and drops the future when it expires.
- Hits for keys that weren't candidates are ignored and logged as a
  contract violation (`crates/sni_router/src/routing/mod.rs:114`).

# Domain types

- `RouteKey::parse` (`crates/sni_router/src/lookup/types.rs:114`) is how
  stored keys (config entries, DB rows) become keys. A wildcard must be
  `*.` followed by at least two labels, so `*.com` and `a.*.b.c` are
  rejected.
- `Backend` parses `host:port`, `a.b.c.d:port`, or `[v6]:port`
  (`crates/sni_router/src/lookup/types.rs:200`). DNS hosts are resolved
  at connect time. Port 0 and bare IPv6 are rejected
  (`crates/sni_router/src/lookup/types.rs:225`).
- `InMemoryLookup` is a ready-made fixed table (used by the app and by
  tests).

# Stability

Every type here is part of the public API a closed-source embedder
compiles against. `RouteCandidates` and `RouteHits` are opaque structs
with accessors, so fields can be added without breaking callers.
