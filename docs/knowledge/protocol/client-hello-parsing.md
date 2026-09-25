---
type: Behavior
title: ClientHello Parsing
description: >
          How the router extracts the SNI from untrusted TLS bytes: record
          walking and reassembly, the three independent size limits, the
          O(records) incremental check, and what is rejected.
resource: crates/sni_router/src/protocol/record.rs
tags: [protocol, tls, parsing, security, untrusted-input]
timestamp: 2026-09-24T00:00:00Z
---

`parse_client_hello(buf, &HelloLimits)`
(`crates/sni_router/src/protocol/record.rs:34`) is pure and synchronous.
`buf` is everything read from the client so far. It returns the SNI (if
any) and `consumed`, the end of the TLS record that completed the hello,
or a `ParseError`. `Incomplete` means "read more".

# Records and reassembly

A ClientHello can span several TLS records.
`scan_records` (`crates/sni_router/src/protocol/record.rs:122`) walks the
record headers:

- Each record must be a handshake record (`0x16`) with legacy major
  version 3 and a payload of 1..=16384 bytes
  (`crates/sni_router/src/protocol/record.rs:163`).
- Garbage is rejected on the **first byte**, before five have arrived
  (`crates/sni_router/src/protocol/record.rs:174`). A non-handshake first
  byte is `NotHandshake`. A bad record later is `InvalidRecord`.
- The 4-byte handshake header can itself be split across records.
  `HandshakeHeader::observe` (`crates/sni_router/src/protocol/record.rs:203`)
  checks the message type (must be ClientHello) as soon as its byte is
  visible.

`scan_records` touches only headers, so re-checking a growing buffer
after every read costs O(records), not O(bytes). A client dripping one
byte per TCP segment can't make the read loop quadratic. Payload ranges
are tracked in `Payloads` (`crates/sni_router/src/protocol/record.rs:72`),
which holds up to 4 ranges inline on the stack — covering every
legitimately-fragmented hello — before spilling to a heap `Vec` for
anything more fragmented than that. Payloads are copied into one message
only once the hello is complete, and only when it was fragmented. The
single-record case parses in place.

# Limits

`HelloLimits` (`crates/sni_router/src/protocol/mod.rs:45`) has three
independent bounds:

1. **Per-record payload:** 16384 bytes, fixed by TLS.
2. **Declared handshake length:** at most `max_handshake_bytes` (default
   32 KiB, `crates/sni_router/src/protocol/mod.rs:53`). It is rejected as
   `TooLarge` as soon as the 4 header bytes are visible, before any of
   the body is buffered.
3. **Record count:** at most `max_records` (default 32,
   `crates/sni_router/src/protocol/record.rs:135`). This stops a flood of
   1-byte records from turning header overhead into unbounded work.

These combine into `max_wire_bytes()`
(`crates/sni_router/src/protocol/mod.rs:66`), the hard cap on buffered
bytes that the reader enforces
(`crates/sni_router/src/delivery/hello_reader.rs:54`). The buffer grows
with `reserve_exact`. Plain `reserve` doubles capacity, which would let a
client pin about twice the limit per connection.

The default is sized from captured real hellos. The largest mainstream
shape today is a browser with an X25519MLKEM768 post-quantum key share,
about 1.9 KB. The Firefox fixture must parse under the defaults
(`crates/sni_router/src/protocol/fixtures.rs`).

# Body and extensions

`parse_client_hello_body` (`crates/sni_router/src/protocol/hello.rs:29`)
uses a bounds-checked `Reader`: no indexing and no `unsafe`. It walks:

1. `legacy_version`, which must have major version 3
2. `random`, which is skipped
3. `session_id`, at most 32 bytes
4. `cipher_suites`, non-empty and of even length
5. `compression_methods`, non-empty
6. an optional extensions block

Trailing bytes are rejected. The client random and session ID are never
returned, so they can't reach logs.

Extensions (`crates/sni_router/src/protocol/hello.rs:125`):

- Duplicate extension types are rejected. Detection is sort-then-scan,
  O(n log n) even for a hello packed with thousands of empty extensions
  (`crates/sni_router/src/protocol/hello.rs:144`). Seen types are tracked
  in `SeenTypes` (`crates/sni_router/src/protocol/hello.rs:85`), which
  holds up to 32 types inline on the stack before spilling to a heap
  `Vec`. Sized by extension *count*, not by the extensions block's byte
  length: a single large extension (e.g. `padding`) can dominate the byte
  count while adding only one type, so a byte-length-derived capacity
  would over-allocate on sparse untrusted input.
- `server_name` (`crates/sni_router/src/protocol/hello.rs:151`): the list
  must be non-empty with no empty names. More than one `host_name` is an
  error (`crates/sni_router/src/protocol/hello.rs:180`). Non-`host_name`
  entries are skipped. A list without a `host_name` means no SNI.

**ECH:** with Encrypted Client Hello, the router routes on the outer
(public) SNI, because the inner hello is encrypted.

# Replay

The parser only locates the SNI. The router replays **every byte it
read** to the backend, including anything after `consumed` (more
handshake data, early data), so the backend sees exactly what the client
sent.

# Tests

- Captured real hellos from openssl, curl, and Firefox with a PQ key share
  (`crates/sni_router/src/protocol/testdata/`), recaptured with
  `scripts/capture_client_hello.py`.
- Deterministic sweeps over the fixtures and built hellos:
  - every prefix returns `Incomplete`
  - every two-record split parses to the same SNI
  - single-byte corruption at every offset never panics
