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

//! Load generator for `just loadtest*` (see `../../../Justfile`). Drives
//! concurrent connections through a real `sni_router` process, mixing
//! small and large payloads, and prints throughput/latency/error stats.
//!
//! The router only ever parses ClientHello *bytes* to read the SNI — it
//! never does a real TLS handshake — so, like the whole existing test
//! suite (`sni_router::testing::ClientHelloBuilder`), this sends a
//! synthetic-but-wire-valid ClientHello rather than a real TLS client.
//!
//! Usage:
//!
//!     load_test --router 127.0.0.1:18080 --concurrency 50 --duration 20 \
//!         --small-bytes 4096 --large-bytes 1048576 --large-ratio 0.05

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sni_router::testing::ClientHelloBuilder;
use tokio::io::{AsyncWriteExt, copy, sink};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Bounds one request's whole lifetime (connect + write + read-to-EOF), so
/// one stuck connection can't hold up the run past `--duration`.
const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Small,
    Large,
}

impl Class {
    fn sni(self) -> &'static str {
        match self {
            Class::Small => "small.test",
            Class::Large => "large.test",
        }
    }
}

impl Class {
    fn expected_bytes(self, args: &Args) -> u64 {
        match self {
            Class::Small => args.small_bytes as u64,
            Class::Large => args.large_bytes as u64,
        }
    }
}

/// Why a request didn't count as a clean success, so the report can tell
/// "the run's own --duration cut this off" apart from "the router or
/// backend actually misbehaved" without a debug-log dig.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Ok,
    /// Hit its per-request deadline (routinely the run's own tail: the
    /// deadline shrinks each request's own timeout as --duration runs out).
    Timeout,
    /// connect/write/read failed outright (reset, broken pipe, refused).
    ConnectionError,
    /// Closed cleanly, but the reply was the wrong size.
    SizeMismatch,
}

struct Outcome {
    class: Class,
    elapsed: Duration,
    bytes_received: u64,
    status: Status,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let hello_small = Arc::new(ClientHelloBuilder::new().sni(Class::Small.sni()).build());
    let hello_large = Arc::new(ClientHelloBuilder::new().sni(Class::Large.sni()).build());
    let payload_small = Arc::new(vec![0xcd_u8; args.small_bytes]);
    let payload_large = Arc::new(vec![0xcd_u8; args.large_bytes]);

    // A shared counter, marked "large" at positions spread evenly by
    // `large_ratio` (a standard error-diffusion distribution: request `i`
    // is large iff floor(i * ratio) > floor((i - 1) * ratio)) — deterministic
    // and reproducible across runs, and correct in aggregate regardless of
    // which worker happens to grab which sequence number.
    let sequence = Arc::new(AtomicU64::new(0));
    let large_ratio = args.large_ratio;
    let small_bytes = Class::Small.expected_bytes(&args);
    let large_bytes = Class::Large.expected_bytes(&args);
    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let run_start = Instant::now();

    let mut workers = Vec::with_capacity(args.concurrency);
    for _ in 0..args.concurrency {
        let sequence = Arc::clone(&sequence);
        let hello_small = Arc::clone(&hello_small);
        let hello_large = Arc::clone(&hello_large);
        let payload_small = Arc::clone(&payload_small);
        let payload_large = Arc::clone(&payload_large);
        let router = args.router;
        workers.push(tokio::spawn(async move {
            let mut outcomes = Vec::new();
            while Instant::now() < deadline {
                let i = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                let is_large = is_marked(i, large_ratio);
                let (class, hello, payload, expected_bytes) = if is_large {
                    (Class::Large, &hello_large, &payload_large, large_bytes)
                } else {
                    (Class::Small, &hello_small, &payload_small, small_bytes)
                };
                outcomes
                    .push(run_one(router, class, hello, payload, expected_bytes, deadline).await);
            }
            outcomes
        }));
    }

    let mut outcomes = Vec::new();
    for worker in workers {
        outcomes.extend(worker.await.expect("worker task panicked"));
    }

    report(&outcomes, run_start.elapsed());
}

/// Whether request number `i` (1-based) falls on a mark spread evenly
/// across the sequence at `ratio` per request.
fn is_marked(i: u64, ratio: f64) -> bool {
    (i as f64 * ratio).floor() > ((i - 1) as f64 * ratio).floor()
}

async fn run_one(
    router: SocketAddr,
    class: Class,
    hello: &[u8],
    payload: &[u8],
    expected_bytes: u64,
    deadline: Instant,
) -> Outcome {
    let start = Instant::now();
    // Cap this request's own timeout to whatever's left before `deadline`,
    // so a request that starts near the end of the run can't itself run
    // for up to `PER_REQUEST_TIMEOUT` past it.
    let remaining = deadline.saturating_duration_since(start);
    let per_request_timeout = PER_REQUEST_TIMEOUT
        .min(remaining)
        .max(Duration::from_millis(1));
    let result = timeout(per_request_timeout, async {
        let mut stream = TcpStream::connect(router).await?;
        stream.write_all(hello).await?;
        stream.write_all(payload).await?;
        stream.shutdown().await?;
        copy(&mut stream, &mut sink()).await
    })
    .await;
    let elapsed = start.elapsed();
    match result {
        Ok(Ok(bytes_received)) if bytes_received == expected_bytes => Outcome {
            class,
            elapsed,
            bytes_received,
            status: Status::Ok,
        },
        Ok(Ok(bytes_received)) => Outcome {
            class,
            elapsed,
            bytes_received,
            status: Status::SizeMismatch,
        },
        Ok(Err(_)) => Outcome {
            class,
            elapsed,
            bytes_received: 0,
            status: Status::ConnectionError,
        },
        Err(_) => Outcome {
            class,
            elapsed,
            bytes_received: 0,
            status: Status::Timeout,
        },
    }
}

fn report(outcomes: &[Outcome], elapsed: Duration) {
    println!("--- overall ---");
    report_class(outcomes, None, elapsed);
    for class in [Class::Small, Class::Large] {
        println!("--- {} ---", class.sni());
        report_class(outcomes, Some(class), elapsed);
    }
}

fn report_class(outcomes: &[Outcome], class: Option<Class>, elapsed: Duration) {
    let filtered: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| class.is_none_or(|c| o.class == c))
        .collect();
    let total = filtered.len();
    let count = |status: Status| filtered.iter().filter(|o| o.status == status).count();
    let timeouts = count(Status::Timeout);
    let connection_errors = count(Status::ConnectionError);
    let size_mismatches = count(Status::SizeMismatch);
    let errors = timeouts + connection_errors + size_mismatches;
    let bytes: u64 = filtered.iter().map(|o| o.bytes_received).sum();
    let mut latencies: Vec<Duration> = filtered
        .iter()
        .filter(|o| o.status == Status::Ok)
        .map(|o| o.elapsed)
        .collect();
    latencies.sort_unstable();

    let mib_s = bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    println!(
        "requests={total} errors={errors} (timeout={timeouts} connection_error={connection_errors} \
         size_mismatch={size_mismatches}) bytes={bytes} req_s={:.1} throughput={mib_s:.1}MiB/s",
        total as f64 / elapsed.as_secs_f64()
    );
    if latencies.is_empty() {
        println!("latency: n/a (no successful requests)");
        return;
    }
    let percentile = |p: f64| latencies[((latencies.len() - 1) as f64 * p) as usize];
    let mean: Duration = latencies.iter().sum::<Duration>() / latencies.len() as u32;
    println!(
        "latency: mean={:?} p50={:?} p95={:?} p99={:?} max={:?}",
        mean,
        percentile(0.50),
        percentile(0.95),
        percentile(0.99),
        latencies[latencies.len() - 1],
    );
}

struct Args {
    router: SocketAddr,
    concurrency: usize,
    duration_secs: u64,
    small_bytes: usize,
    large_bytes: usize,
    large_ratio: f64,
}

impl Args {
    fn parse() -> Self {
        let mut router = "127.0.0.1:18080".parse().unwrap();
        let mut concurrency = 50;
        let mut duration_secs = 20;
        let mut small_bytes = 4096;
        let mut large_bytes = 1024 * 1024;
        let mut large_ratio = 0.05;

        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .unwrap_or_else(|| panic!("{flag} needs a value"));
            match flag.as_str() {
                "--router" => router = value.parse().expect("--router: invalid address"),
                "--concurrency" => {
                    concurrency = value.parse().expect("--concurrency: invalid number")
                }
                "--duration" => {
                    duration_secs = value
                        .parse()
                        .expect("--duration: invalid number of seconds")
                }
                "--small-bytes" => {
                    small_bytes = value.parse().expect("--small-bytes: invalid number")
                }
                "--large-bytes" => {
                    large_bytes = value.parse().expect("--large-bytes: invalid number")
                }
                "--large-ratio" => {
                    large_ratio = value.parse().expect("--large-ratio: invalid fraction")
                }
                other => panic!("unknown flag {other}"),
            }
        }
        assert!(
            (0.0..=1.0).contains(&large_ratio),
            "--large-ratio must be between 0 and 1"
        );
        assert!(duration_secs > 0, "--duration must be greater than 0");
        Self {
            router,
            concurrency,
            duration_secs,
            small_bytes,
            large_bytes,
            large_ratio,
        }
    }
}
