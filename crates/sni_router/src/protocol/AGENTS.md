# AGENTS.md

High-level summary for `crates/sni_router/src/protocol`.

## Responsibility

Parses untrusted bytes from the start of a TLS connection to find the SNI. Pure and synchronous; no I/O, no allocation beyond reassembling a fragmented hello.

- `record.rs` — record-layer walk, handshake-header checks, reassembly, `parse_client_hello`.
- `hello.rs` — ClientHello body and extensions, `server_name` extraction.
- `reader.rs` — bounds-checked cursor; every read returns `None` instead of panicking.
- `alert.rs` — the `unrecognized_name` alert bytes.
- `mod.rs` — `HelloLimits`, `ClientHelloInfo`, `ParseError`.

## Boundaries

- Treat all input as hostile: no indexing without a prior length check, no `unsafe`, reject as early as the bytes allow.
- Keep incremental re-parsing O(records): header scans must not copy payloads (the reader re-parses after every read).
- Don't expose client random/session ID; return only the SNI and `consumed`.
- Behavior is documented in `docs/knowledge/protocol/client-hello-parsing.md`; keep it in sync.

## Testing Expectations

- Every parser change needs tests near the code, including the prefix / split / corruption sweeps in `record.rs` and `fixtures.rs`.
- Real-client fixtures must keep parsing under `HelloLimits::default()`.
- Run `/security-review` on changes here.
