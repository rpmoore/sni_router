# Load-testing recipes for sni_router. Drives the real, compiled `sni_router`
# binary (not the library in-process) through a dedicated `loadtest/router.toml`,
# with two toy backends standing in for "small" and "large" upstreams, so this
# exercises the actual deployed proxy stage (splice fast path, fd admission
# control, accept loop) end to end. See loadtest/router.toml and
# crates/sni_router_app/examples/{load_backend,load_test}.rs.
#
# Usage: `just loadtest`, `just loadtest-heavy`, `just loadtest-large-only`,
# `just loadtest-smoke`, or with explicit (positional) params, in order:
# `just loadtest 200 30 4096 1048576 0.05`
# (concurrency, duration_secs, small_bytes, large_bytes, large_ratio).

set shell := ["bash", "-uc"]

router_addr := "127.0.0.1:18080"
admin_addr := "127.0.0.1:18081"
small_backend_addr := "127.0.0.1:19101"
large_backend_addr := "127.0.0.1:19102"

build:
    cargo build --release -p sni_router_app --bins --examples

# Starts both toy backends and the real router, waits for /health, drives
# load_test with the given parameters, then tears everything down.
loadtest concurrency="50" duration="20" small_bytes="4096" large_bytes="1048576" large_ratio="0.05": build
    #!/usr/bin/env bash
    set -euo pipefail
    trap 'kill $(jobs -p) 2>/dev/null || true; wait 2>/dev/null || true' EXIT

    ./target/release/examples/load_backend --listen {{small_backend_addr}} --reply-bytes {{small_bytes}} &
    ./target/release/examples/load_backend --listen {{large_backend_addr}} --reply-bytes {{large_bytes}} &
    SNI_ROUTER_CONFIG=loadtest/router.toml ./target/release/sni_router &

    for _ in $(seq 1 50); do
        curl -sf "http://{{admin_addr}}/health" >/dev/null 2>&1 && break
        sleep 0.1
    done

    ./target/release/examples/load_test \
        --router {{router_addr}} \
        --concurrency {{concurrency}} \
        --duration {{duration}} \
        --small-bytes {{small_bytes}} \
        --large-bytes {{large_bytes}} \
        --large-ratio {{large_ratio}}

# High concurrency, default payload sizes.
loadtest-heavy: (loadtest "500" "30" "4096" "1048576" "0.05")

# Every request is a large payload.
loadtest-large-only: (loadtest "50" "20" "1048576" "8388608" "1.0")

# Fast (~5s) sanity check that the whole pipeline still wires together.
loadtest-smoke: (loadtest "10" "5" "1024" "65536" "0.1")
