# AGENTS.md

High-level summary for `crates/sni_router/src/lookup`.

## Responsibility

The embedder contract: `RouteLookup` and the types that cross it.

- `mod.rs` — `RouteLookup`, `LookupFuture`, `RouteCandidates`, `RouteHits`, `LookupError`.
- `types.rs` — `Hostname`, `RouteKey`, `Backend`, `BackendHost`, `NameError` (validation and normalization).
- `memory.rs` — `InMemoryLookup`, a fixed table.

## Boundaries

- This is public API consumed by a closed-source service. Changes are breaking unless additive; keep structs opaque and call out any change in the PR.
- Implementations match keys literally. Wildcard expansion and precedence stay in `routing`; don't move them into lookups.
- `Err` is transient failure only; "no route" is an empty `RouteHits`.

## Testing Expectations

- Parsing/normalization tables live in `types.rs`; keep adding rejected-input cases when validation changes.
