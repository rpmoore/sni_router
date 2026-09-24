---
type: Behavior
title: Hostname Matching
description: >
          Case-insensitive exact matching, then a single-label wildcard;
          no default backend; lookup errors and timeouts fail closed.
resource: crates/sni_router/src/routing/mod.rs
tags: [routing, wildcard, policy]
timestamp: 2026-09-24T00:00:00Z
---

# Normalization

The SNI host_name is parsed into a `Hostname`
(`crates/sni_router/src/lookup/types.rs:75`). It is lowercased and
checked:

- at most 253 bytes
- labels of 1–63 bytes
- only `[a-z0-9-_]`
- not an IP literal
- no empty labels, so a trailing dot is rejected

An invalid name fails ClientHello parsing (`InvalidServerName`). The
router never tries to route it.

# Candidates and precedence

`candidate_keys` (`crates/sni_router/src/routing/mod.rs:69`) produces, in
order:

1. the exact key (`api.example.com`)
2. the wildcard key made by replacing the first label with `*`
   (`*.example.com`), only when at least two labels remain
   (`crates/sni_router/src/lookup/types.rs:145`)

So `*.example.com` matches `a.example.com`. It does **not** match
`example.com` or `a.b.example.com`: a wildcard covers exactly one label.

`resolve_route` (`crates/sni_router/src/routing/mod.rs:102`) makes **one**
lookup call with both candidates, bounded by `lookup_timeout`. Then
`select_route` (`crates/sni_router/src/routing/mod.rs:76`) picks the
first candidate, in order, that has a hit. Exact beats wildcard even when
the store returns both.

# Failure behavior

There is no default backend. Every failure closes the connection:

| Result | Connection outcome | Client sees |
|---|---|---|
| No hit | `UnknownHost` | TLS `unrecognized_name` alert |
| No SNI in the hello | `MissingSni` | TLS `unrecognized_name` alert |
| Lookup `Err` | `LookupFailed` | close |
| Lookup exceeds `lookup_timeout` | `LookupTimeout` | close |

An error or timeout never falls through to a partial answer. Routing a
host to its wildcard backend because its exact row was momentarily
unreachable would misroute it.
