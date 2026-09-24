# AGENTS.md

Guidance for Codex and other coding agents working in this repository.

## Project Context

`sni_router` is a Rust TLS SNI router: it accepts TCP connections, peeks the TLS ClientHello to read the Server Name Indication, looks up a backend for that name, and proxies the raw (still-encrypted) stream to it. It uses Tokio for async networking. See `RUST.md` for Rust-specific standards (formatting, linting, testing, error handling, dependencies).

The project is **library-first**:

- The library crate owns SNI parsing, routing, and proxying, and exposes the route lookup as a trait so embedders can supply their own backend source.
- The open-source app binary in this repo wires the library to a TOML file of routes.
- A separate closed-source service will embed the same library with a database-backed lookup. Nothing in this repo may depend on, reference, or assume details of that service — the lookup trait is the only contract between them, so keep it small, async, and free of TOML/file-specific types.

Before working on a subsystem, check `docs/knowledge/` for its architecture and behavior. It's an [OKF](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)-conformant bundle: `docs/knowledge/index.md` is the root, each area has its own `index.md`, and each concept doc is grounded with `file:line` references to the code it describes. This documents *current, implemented* behavior — for point-in-time design history (why a decision was made, what alternatives were rejected), see `docs/plans/` instead.

## Automatic Triggers

These run without being asked, no explicit request needed:

- Plan drafted → adversarial-review the plan before presenting it (Change Workflow).
- Diff ready to commit → adversarial-review the diff before committing (Change Workflow).
- Nontrivial code change done → run `verify` skill before reporting done (Change Workflow).
- PR touches auth/untrusted-input/network code → run `/security-review` before opening PR (Change Workflow).
- PR feedback reviewed → mark resolved or state why not (Change Workflow).
- Code change complete → update `docs/knowledge/` for the affected code, creating a new concept doc if none exists yet (Knowledge Bundle).
- Code change complete → log entry to Obsidian daily note, if available (Change Logging).

## Caveman Tooling

- For read-only code location ("where is X defined", "map this directory"), delegate to `cavecrew-investigator` instead of exploring inline — output is context-compressed.
- For bounded 1–2 file mechanical edits (typo fixes, renames, format-preserving tweaks), delegate to `cavecrew-builder`. Do not route new features, new files, or cross-file refactors through it.
- For diff/PR review output, use `cavecrew-reviewer` or `/caveman-review` for compressed, severity-tagged one-line findings instead of prose review.
- For commit messages, use `/caveman-commit` to keep them terse and Conventional-Commits formatted.
- Keep cavecrew delegation scoped to narrow, mechanical work only — multi-file refactors, new features, and architecture decisions stay in the main agent.

## Rust Code Standards

See `RUST.md` for Rust-specific formatting, linting, testing, and code-standard rules (fmt/clippy/test gates, error handling, dependency policy, etc.). That file is the source of truth for anything toolchain- or language-level; this file stays scoped to process and workflow.

## Software Development Practices

- Write small, testable functions with one clear responsibility.
- The workspace has two crates: `crates/sni_router` (the library, no TOML/serde/HTTP dependencies) and `crates/sni_router_app` (the `sni_router` binary). Separate concerns by layer:
  - `crates/sni_router/src/protocol` owns TLS record / ClientHello parsing and SNI extraction. Pure, synchronous, no I/O.
  - `crates/sni_router/src/lookup` owns the `RouteLookup` trait and the domain types it uses (`Hostname`, `RouteKey`, `Backend`, `RouteCandidates`, `RouteHits`). Implementations (TOML-backed here, database-backed elsewhere) live behind it.
  - `crates/sni_router/src/routing` owns routing policy: candidate keys (exact, then single-label wildcard), precedence, and failing closed.
  - `crates/sni_router/src/delivery` owns listeners, accept, ClientHello reading/replay, upstream connect, proxying, and shutdown.
  - `crates/sni_router/src/cache.rs` is the optional `CachedLookup` decorator for slow/remote lookups; `metrics.rs` is the typed `MetricsSink` event API.
  - `crates/sni_router_app/src/config` owns the TOML schema, validation, and the reloadable `FileRouteLookup`; the rest of the app crate is admin HTTP, the OTel/Prometheus sink, SIGHUP reload, and wiring.
- Keep the binary (`main.rs`) thin: load config, build a lookup, hand both to the library. Anything an embedder would also need belongs in the library.
- Keep orchestration code thin. Move decision logic into helpers or domain services that can be unit tested directly.
- Preserve boundaries between pure logic, I/O, metrics, and logging/event emission.
- Prefer injected dependencies for lookups, clocks, sinks, and other side effects when testability matters. Tests should use in-memory lookup implementations, not files or databases.
- Add tests at the lowest useful level. ClientHello parsing edge cases belong near parser code; matching rules belong near routing code; socket behavior belongs near delivery code.
- Keep functions short enough to scan. If a function mixes validation, transformation, I/O, metrics, and error handling, split it.
- Name functions by behavior and outcome rather than implementation detail.
- Do not rewrite unrelated code for style alone.

## Change Workflow

- Inspect existing module patterns before editing.
- Keep edits scoped to the requested behavior.
- When adding behavior, add or update tests that would fail without the change.
- When fixing a bug, include a regression test whenever practical.
- Treat the lookup trait and other public library types as a public API: changes to them affect the closed-source embedder, so call them out explicitly in the change summary / PR description.
- Before committing substantive code or documentation changes, run `/codex:adversarial-review` on the intended diff to challenge the implementation approach, design choices, and assumptions. Address actionable feedback before committing, or document why feedback is not being acted on. If feedback fixes materially change the diff, run one follow-up adversarial review on the updated diff before committing.
- When working on a step in `docs/steps.md` that links to a GitHub issue, update that issue's status as work progresses. Leave a concise progress comment when starting or materially changing scope, and close the issue only after the step's acceptance criteria and verification are complete.
- When reviewing GitHub PR feedback, always mark the feedback as resolved when the feedback has been addressed, or state why the comment was not addressed.
- After drafting an implementation plan (Plan mode or `/deep-plan`) and before presenting it for approval, auto-run `/codex:adversarial-review` on the plan itself. Address feedback or state why not, same as diff review.
- After nontrivial code changes and before reporting the task done, auto-run the `verify` skill against the affected flow. Skip for test-only or doc-only diffs.
- Before reporting any Rust change done, satisfy the fmt/clippy/test gates in `RUST.md`.
- Before opening a PR that touches auth, parsing of untrusted input (ClientHello parsing is untrusted input), or network-facing code, auto-run `/security-review`.
- After changes, summarize what changed and which verification commands were run.

## Knowledge Bundle (`docs/knowledge/`)

After a nontrivial code change, before reporting the task done: check
whether any concept doc under `docs/knowledge/` describes the code just
touched, and bring it up to date in the same change.

- If a concept doc exists for that area and the change made it stale
  (behavior, invariant, file/line reference, or code snippet no longer
  matches), update it. Don't leave a doc describing pre-change behavior.
- If no concept doc exists yet for that section of code, add one,
  following the structure of existing docs under `docs/knowledge/`
  (frontmatter with `type`/`title`/`description`/`resource`/`tags`/`timestamp`,
  `file:line`-grounded claims, an index entry linking to it from that
  area's `index.md`). Use judgment on granularity — a small helper
  doesn't need its own concept doc; a subsystem with real invariants
  (lookup contract, ClientHello parsing limits, proxy lifecycle) does.
- Record bundle changes (new areas, new or substantially rewritten concept
  docs) as a dated entry in `docs/knowledge/log.md`.
- Skip this for test-only, doc-only, or purely mechanical changes
  (renames, formatting) that don't change documented behavior.
- This is separate from directory-local `AGENTS.md` summaries (see
  Directory Summary Instructions below): those are terse orientation
  notes; `docs/knowledge/` is the deeper, OKF-structured behavioral
  record. Updating one doesn't substitute for the other.

## Change Logging

After code changes are complete, write one summary of what changed and why to Obsidian's daily note, under a heading/section for `sni_router_change_log`, if an `mcp__obsidian__*` tool is available in the session. Daily notes for this project live under the vault directory `sni_router_change_log/` (e.g. `sni_router_change_log/2026-09-24.md`), not at the vault root — get the current daily note's path with `mcp__obsidian__periodic_note_get_path` (period `daily`) rather than guessing a root-level `YYYY-MM-DD.md` path. Use `mcp__obsidian__vault_patch` targeting the `sni_router_change_log` heading (or `vault_append` if no daily note structure exists yet). Do not log per-step; one entry per logical change is enough.

Keep entries compact: terse phrasing, no filler, drop restating obvious context (file paths/diffs are already in git). Entry must stay skimmable and cheap to reload into context later — aim for a few lines, not paragraphs.

If no `mcp__obsidian__*` tool is available, skip Obsidian logging and instead summarize what changed and why in the PR description or final response.

## Directory Summary Instructions

Do not use this root `AGENTS.md` to store a full summary of the current codebase. Instead, future agents should summarize code close to the directory being described.

- Before searching through a directory for context, inspect that directory's local `AGENTS.md` if one exists. Use it to understand the directory's responsibilities, boundaries, and testing expectations before reading or searching broader code.
- Do not update a directory-local `AGENTS.md` after every change. Add or update one only when a change makes the existing summary stale or when new context would materially help future agents understand that directory.
- Each directory-local summary must describe what that directory contains at a high level.
- Keep each directory summary under 200 lines.
- Summaries should explain responsibilities, important module boundaries, and testing expectations.
- Summaries should avoid listing every function or restating implementation details that are obvious from filenames.
- Update a directory summary when the directory's responsibilities change, new major modules are added, or old responsibilities move elsewhere.
