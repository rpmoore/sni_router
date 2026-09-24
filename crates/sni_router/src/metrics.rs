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

//! Metrics as typed events. The library never names a Prometheus series;
//! embedders implement [`MetricsSink`] and map events onto whatever exporter
//! and naming scheme they use.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

/// Identifies the listener a connection arrived on, so per-listener series
/// (e.g. `{network, address}` labels) stay distinguishable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerInfo {
    network: &'static str,
    address: SocketAddr,
    name: Option<Arc<str>>,
}

impl ListenerInfo {
    pub fn tcp(address: SocketAddr) -> Self {
        Self {
            network: "tcp",
            address,
            name: None,
        }
    }

    /// Describes a bound listener by its local address.
    pub fn from_listener(listener: &TcpListener) -> io::Result<Self> {
        Ok(Self::tcp(listener.local_addr()?))
    }

    /// Attaches an operator-chosen name (e.g. `"public"`).
    pub fn with_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn network(&self) -> &'static str {
        self.network
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// How a connection ended. Every accepted connection records exactly one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConnectionOutcome {
    /// Proxied until one side closed.
    Proxied,
    /// Proxying ended on an I/O error (e.g. a reset) from either side.
    ProxyError,
    /// The client closed or errored before sending a full ClientHello.
    ClientClosed,
    /// The ClientHello didn't arrive within the deadline.
    HelloTimeout,
    /// The bytes weren't a valid ClientHello, or exceeded the limits.
    InvalidHello,
    /// The ClientHello carried no SNI.
    MissingSni,
    /// No route matched the SNI.
    UnknownHost,
    /// The route lookup failed.
    LookupFailed,
    /// The route lookup didn't finish within the deadline.
    LookupTimeout,
    /// The backend refused or failed the connection or the replay.
    UpstreamConnectFailed,
    /// Connecting to and replaying to the backend exceeded the deadline.
    UpstreamTimeout,
    /// Neither side sent data for the idle timeout.
    IdleTimeout,
    /// Shutdown started before the connection reached the proxy stage.
    ShutdownCancelled,
    /// The shutdown grace period ended while still proxying.
    ShutdownForced,
    /// The connection's task ended without finishing (it panicked or was
    /// aborted, e.g. by runtime shutdown).
    Aborted,
}

impl ConnectionOutcome {
    /// A stable snake_case label value.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionOutcome::Proxied => "proxied",
            ConnectionOutcome::ProxyError => "proxy_error",
            ConnectionOutcome::ClientClosed => "client_closed",
            ConnectionOutcome::HelloTimeout => "hello_timeout",
            ConnectionOutcome::InvalidHello => "invalid_hello",
            ConnectionOutcome::MissingSni => "missing_sni",
            ConnectionOutcome::UnknownHost => "unknown_host",
            ConnectionOutcome::LookupFailed => "lookup_failed",
            ConnectionOutcome::LookupTimeout => "lookup_timeout",
            ConnectionOutcome::UpstreamConnectFailed => "upstream_connect_failed",
            ConnectionOutcome::UpstreamTimeout => "upstream_timeout",
            ConnectionOutcome::IdleTimeout => "idle_timeout",
            ConnectionOutcome::ShutdownCancelled => "shutdown_cancelled",
            ConnectionOutcome::ShutdownForced => "shutdown_forced",
            ConnectionOutcome::Aborted => "aborted",
        }
    }
}

/// How a route lookup ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LookupOutcome {
    Found,
    NotFound,
    Error,
    Timeout,
}

impl LookupOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            LookupOutcome::Found => "found",
            LookupOutcome::NotFound => "not_found",
            LookupOutcome::Error => "error",
            LookupOutcome::Timeout => "timeout",
        }
    }
}

/// Events from [`CachedLookup`](crate::cache::CachedLookup).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CacheEvent {
    /// Served a fresh cached route.
    Hit,
    /// Served a fresh cached "no route".
    NegativeHit,
    /// Started a load from the inner lookup.
    Miss,
    /// Waited on a load another caller had already started.
    Coalesced,
    /// Served an expired entry because the reload failed (stale-if-error).
    StaleServed,
    /// The inner lookup failed or the load was aborted.
    LoadError,
    /// A miss couldn't start a load because `max_inflight_loads` were
    /// already running.
    Shed,
}

impl CacheEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheEvent::Hit => "hit",
            CacheEvent::NegativeHit => "negative_hit",
            CacheEvent::Miss => "miss",
            CacheEvent::Coalesced => "coalesced",
            CacheEvent::StaleServed => "stale_served",
            CacheEvent::LoadError => "load_error",
            CacheEvent::Shed => "shed",
        }
    }
}

/// One observable thing the router did. New variants and fields may be
/// added in minor releases; match with a `_ =>` arm and `..` patterns.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum MetricEvent<'a> {
    #[non_exhaustive]
    ConnectionOpened {
        listener: &'a ListenerInfo,
    },
    #[non_exhaustive]
    ConnectionClosed {
        listener: &'a ListenerInfo,
        outcome: ConnectionOutcome,
        lifetime: Duration,
    },
    /// Bytes read from the client over the connection's life. Reported once,
    /// when the connection closes, so long-lived connections show up late.
    #[non_exhaustive]
    BytesRead {
        listener: &'a ListenerInfo,
        bytes: u64,
    },
    /// Bytes written to the client over the connection's life. Reported once,
    /// when the connection closes.
    #[non_exhaustive]
    BytesWritten {
        listener: &'a ListenerInfo,
        bytes: u64,
    },
    #[non_exhaustive]
    RouteLookup {
        listener: &'a ListenerInfo,
        outcome: LookupOutcome,
        elapsed: Duration,
    },
    /// Connecting to the backend and replaying the ClientHello.
    #[non_exhaustive]
    UpstreamConnect {
        listener: &'a ListenerInfo,
        ok: bool,
        elapsed: Duration,
    },
    Cache(CacheEvent),
}

/// Receives [`MetricEvent`]s. Called inline on the connection's task, so
/// implementations should be cheap and must not block.
pub trait MetricsSink: Send + Sync {
    fn record(&self, event: MetricEvent<'_>);
}

impl<T: MetricsSink + ?Sized> MetricsSink for Arc<T> {
    fn record(&self, event: MetricEvent<'_>) {
        (**self).record(event)
    }
}

/// Discards every event.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopMetrics;

impl MetricsSink for NoopMetrics {
    fn record(&self, _event: MetricEvent<'_>) {}
}
