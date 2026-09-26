// Copyright 2026 Ryan Moore
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Compares the router's two proxy-copy paths on this machine: `splice(2)`
//! through an in-kernel pipe (Linux, the default) against the portable
//! `tokio::io::copy_bidirectional` fallback every other OS uses. Run with:
//!
//! ```text
//! cargo run --release --example copy_benchmark --features test-util
//! ```
//!
//! A real client pushes a large payload through a real `Router` (loopback
//! TCP, no mocks) to a sink backend, and the elapsed time to drain it all
//! and get one byte back is used to compute throughput. On Linux this runs
//! both paths, forcing the fallback with the same `SNI_ROUTER_DISABLE_SPLICE`
//! escape hatch operators can use; elsewhere only the fallback exists.
//!
//! Not a criterion benchmark: one large transfer's throughput is a stable,
//! easy-to-read number without needing statistical sampling, and this
//! avoids a heavyweight dev-dependency for a single comparison.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sni_router::testing::{ClientHelloBuilder, RecordingMetrics, ScriptedLookup};
use sni_router::{ListenerInfo, Router, RouterConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const PAYLOAD_BYTES: usize = 512 * 1024 * 1024;
const WRITE_CHUNK: usize = 256 * 1024;

#[tokio::main]
async fn main() {
    println!(
        "payload: {} MiB per run, default copy_buffer_size\n",
        PAYLOAD_BYTES / (1024 * 1024)
    );
    run_case("default (splice on Linux, else the portable copy)").await;

    #[cfg(target_os = "linux")]
    {
        // SAFETY: no other task has started yet, so nothing else can be
        // racing this read/write of the process environment.
        unsafe { std::env::set_var("SNI_ROUTER_DISABLE_SPLICE", "1") };
        run_case("portable copy (splice disabled)").await;
        unsafe { std::env::remove_var("SNI_ROUTER_DISABLE_SPLICE") };
    }
}

async fn run_case(label: &str) {
    let elapsed = time_one_transfer().await;
    let gib = PAYLOAD_BYTES as f64 / (1024.0 * 1024.0 * 1024.0);
    println!(
        "{label}: {:.2} GiB/s ({:.3}s)",
        gib / elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );
}

/// Starts a router with one route to a sink backend, pushes `PAYLOAD_BYTES`
/// through it from a real client, and returns the wall-clock time for the
/// backend to drain it all and the client to see the backend's one-byte
/// reply — covering both proxy directions.
async fn time_one_transfer() -> Duration {
    let backend_addr = start_sink_backend().await;
    let lookup = Arc::new(ScriptedLookup::found(&[(
        "bench.test",
        &backend_addr.to_string(),
    )]));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let router_addr = listener.local_addr().unwrap();
    let info = ListenerInfo::from_listener(&listener).unwrap();
    let router = Router::new(
        lookup,
        Arc::new(RecordingMetrics::new()),
        RouterConfig::default(),
    );
    let shutdown = CancellationToken::new();
    let serve = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { router.serve(vec![(listener, info)], shutdown).await })
    };

    let mut client = TcpStream::connect(router_addr).await.unwrap();
    client
        .write_all(&ClientHelloBuilder::new().sni("bench.test").build())
        .await
        .unwrap();

    let start = Instant::now();
    let payload = vec![0xabu8; WRITE_CHUNK];
    let mut sent = 0usize;
    while sent < PAYLOAD_BYTES {
        client.write_all(&payload).await.unwrap();
        sent += payload.len();
    }
    client.shutdown().await.unwrap();
    let mut ack = [0u8; 1];
    client.read_exact(&mut ack).await.unwrap();
    let elapsed = start.elapsed();

    drop(client);
    shutdown.cancel();
    let _ = serve.await;
    elapsed
}

/// Drains every byte sent to it, then writes back one byte once the sender
/// closes its write half — the round-trip signal `time_one_transfer` waits
/// on before stopping the clock.
async fn start_sink_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; WRITE_CHUNK];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = stream.write_all(&[1]).await;
    });
    addr
}
