# AGENTS.md

High-level summary for `crates/sni_router_app` (the `sni_router` binary).

## Responsibility

Wires the library to a TOML route file for the open-source deployment.

- `src/config/` — TOML schema, defaults, validation (`AppConfig`), and the reloadable `FileRouteLookup`.
- `src/reload.rs` — SIGHUP: validate the whole file, then swap routes; `[server]`/`[admin]` changes are reported, not applied.
- `src/admin.rs` — HTTP `/metrics` and the health path (503 while draining).
- `src/otel_metrics.rs` — `MetricsSink` → OpenTelemetry → Prometheus, with the app's metric names.
- `src/lib.rs` — `BoundApp` (bind everything, then run); `src/main.rs` stays thin (env, logging, signals).

## Boundaries

- Anything an embedder would also need belongs in the library, not here.
- Metric names are a public contract with dashboards; the e2e test pins them.

## Testing Expectations

- Config validation and reload have unit tests; `tests/e2e_config_toml.rs` drives the app end to end (routing, health, golden metric names).
- `examples/load_backend.rs` and `examples/load_test.rs` back the `just loadtest*` recipes (repo-root `Justfile`) — manual load testing of the real compiled binary, not part of `cargo test`.
