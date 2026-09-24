---
name: verify
description: Build, launch, and drive sni_router end-to-end to verify a change at its TLS/SNI surface (routed handshakes via openssl/curl, malformed-ClientHello probes, unknown-SNI handling, SIGHUP reload, SIGTERM drain).
---

# Verifying sni_router changes end-to-end

Build: `cargo build` (debug binary at `target/debug/sni_router`).

## Launch

The app reads `SNI_ROUTER_CONFIG` (default `./sni_router.toml`); there are
no built-in routes, so a missing file fails startup. Use an absolute path.
Work in the scratchpad directory.

Throwaway TLS backends with distinct certs make the routed destination
visible in the handshake:

```sh
for n in a b; do
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=$n.test \
    -keyout $n.key -out $n.crt 2>/dev/null
done
(openssl s_server -accept 127.0.0.1:19101 -cert a.crt -key a.key -www >a.log 2>&1 &)
(openssl s_server -accept 127.0.0.1:19102 -cert b.crt -key b.key -www >b.log 2>&1 &)
```

```toml
[server]
listen = ["127.0.0.1:18443"]
client_hello_timeout_ms = 1000
shutdown_grace_secs = 3
[admin]
listen = "127.0.0.1:18081"
[[routes]]
hostname = "a.test"
backend = "127.0.0.1:19101"
[[routes]]
hostname = "*.wild.test"
backend = "127.0.0.1:19102"
```

Run in the background, capturing the JSON log:
`(SNI_ROUTER_CONFIG=$PWD/config.toml target/debug/sni_router > router.log 2>&1 &)`.
Startup logs `loaded config`, `sni_router starting` (with `routes`), and one
`sni_router listening` per listener.

## Drive

- Routing: `openssl s_client -connect 127.0.0.1:18443 -servername a.test </dev/null 2>/dev/null | grep ^subject`
  → `CN = a.test`; `-servername X.Wild.Test` → `CN = b.test` (case-insensitive wildcard).
- Full request: `curl -sk --resolve a.test:18443:127.0.0.1 https://a.test:18443/`.
- Unknown SNI and `-noservername`: openssl reports `tlsv1 unrecognized name ... alert number 112`.
- Health/metrics: `curl -s -w '%{http_code}' http://127.0.0.1:18081/health`, `curl -s http://127.0.0.1:18081/metrics | grep snirouter_`.
- Reload: edit a route, `kill -HUP <pid>` → log `reloaded routes`; new handshakes use the new backend. Append junk to the file and HUP again → `route reload failed; keeping current routes`, routing unchanged.
- Drain: hold a TLS connection open (python `ssl.wrap_socket`), `kill -TERM <pid>` → `/health` returns `503 draining`, the held connection keeps working, log shows `draining proxied connections` with `proxying: 1`, then `shutdown complete`.

## Probes that worked

- Garbage (`printf 'GET / HTTP/1.1\r\n\r\n' | nc -q2 127.0.0.1 18443`): closed with 0 bytes, outcome `invalid_hello`.
- Slowloris (python: send `b'\x16\x03'`, then `recv`): closed after `client_hello_timeout_ms`.
- With `RUST_LOG=debug`, every connection logs one `connection closed` line with `outcome`, `sni`, `backend`, byte counts (it's `debug`, not `info`, by design).

## Gotchas

- **Don't use `pkill -f`/`pgrep -f` with a pattern that also appears in your
  own command line** (e.g. `pkill -f "openssl s_server"` inside a Bash call
  whose text contains that string); it kills the tool's own shell (exit 144).
  Use `pgrep -x openssl | xargs -r kill` and `pgrep -f "target/debug/sni_router$"`.
- Stale instances on the same ports answer instead of your build — check
  `pgrep -f "target/debug/sni_router$"` first.
- `openssl s_server -www` exits on some errors; check the backends are
  alive before blaming the router.
