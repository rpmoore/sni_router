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

//! Network I/O: accepting connections, reading the ClientHello, connecting
//! upstream, proxying, and shutdown.

mod connection;
mod gate;
mod hello_reader;
mod metered;
mod proxy;
mod resolver;
mod server;
#[cfg(target_os = "linux")]
mod splice;

use std::time::Duration;

pub use server::Router;

use crate::protocol::HelloLimits;

/// Router timeouts and limits. Construct with [`RouterConfig::default`] and
/// override fields; new fields may be added in minor releases.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RouterConfig {
    /// One overall deadline for the whole ClientHello to arrive
    /// (not per read), so a slow-drip client can't hold a slot.
    pub client_hello_timeout: Duration,
    /// Deadline for the route lookup.
    pub lookup_timeout: Duration,
    /// One deadline covering DNS resolution, TCP connect, and replaying the
    /// buffered ClientHello to the backend.
    pub upstream_timeout: Duration,
    /// Close a proxied connection after this long with no bytes in either
    /// direction. Defaults to 30 minutes so idle sessions can't hold every
    /// connection slot forever; `None` disables it (only do that behind
    /// per-client connection limits).
    pub idle_timeout: Option<Duration>,
    /// Bounds on ClientHello size and fragmentation.
    pub hello_limits: HelloLimits,
    /// Most connections handled at once across all listeners. Further
    /// connections wait in the kernel accept backlog.
    pub max_connections: usize,
    /// How long proxied connections may keep draining after shutdown starts
    /// before they're closed.
    pub shutdown_grace: Duration,
    /// How long a DNS backend's resolved addresses are reused before
    /// resolving again. `Duration::ZERO` resolves on every connection.
    pub dns_cache_ttl: Duration,
    /// Most backend DNS resolutions running at once. Each holds a blocking
    /// thread until it returns, even after its connection gives up.
    pub max_concurrent_dns_lookups: usize,
    /// Size of each of the two copy buffers a proxied connection holds for
    /// its lifetime (or, on Linux with `splice(2)` available, the size each
    /// direction's in-kernel pipe is grown to). Larger buffers mean fewer
    /// syscalls per byte but more memory per busy connection (2 × this;
    /// untouched pages of an idle connection's buffers aren't resident).
    /// Measured on loopback, one connection moved ~0.8 GiB/s at 8 KiB,
    /// ~1.5 at 16 KiB, and levelled off at ~2.4–2.6 from 32 KiB up with the
    /// userspace copy; `splice` moved a large payload at roughly 1.3–1.5×
    /// that (`cargo run --release --example copy_benchmark --features
    /// test-util`).
    pub copy_buffer_size: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            client_hello_timeout: Duration::from_secs(5),
            lookup_timeout: Duration::from_secs(2),
            upstream_timeout: Duration::from_secs(5),
            idle_timeout: Some(Duration::from_secs(30 * 60)),
            hello_limits: HelloLimits::default(),
            max_connections: 10_000,
            shutdown_grace: Duration::from_secs(30),
            dns_cache_ttl: Duration::from_secs(30),
            max_concurrent_dns_lookups: 64,
            copy_buffer_size: 32 * 1024,
        }
    }
}

/// How long the `unrecognized_name` alert write may take before the
/// connection is just closed.
const ALERT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
