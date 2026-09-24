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

//! One connection's pipeline:
//! ClientHello → route → upstream connect + replay → proxy gate → proxy.
//!
//! Every pre-proxy stage races the gate's handshake-cancel token, so shutdown
//! interrupts it immediately. Only the proxy stage observes the force token.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::gate::ProxyGate;
use super::hello_reader::{HelloReadError, read_client_hello};
use super::metered::{Activity, ByteCounters, MeteredStream};
use super::proxy::{ProxyEnd, proxy};
use super::resolver::Resolver;
use super::{ALERT_WRITE_TIMEOUT, RouterConfig};
use crate::lookup::{Backend, BackendHost, Hostname, RouteLookup};
use crate::metrics::{ConnectionOutcome, ListenerInfo, LookupOutcome, MetricEvent, MetricsSink};
use crate::protocol::UNRECOGNIZED_NAME_ALERT;
use crate::routing::{RouteError, resolve_route};

/// State shared by every connection a router handles.
pub(super) struct Shared {
    pub(super) lookup: Arc<dyn RouteLookup>,
    pub(super) metrics: Arc<dyn MetricsSink>,
    pub(super) config: RouterConfig,
    pub(super) gate: ProxyGate,
    pub(super) force_cancel: CancellationToken,
    pub(super) resolver: Arc<Resolver>,
}

/// Smallest copy buffer used, whatever the config says.
const MIN_COPY_BUFFER: usize = 512;

/// Records a connection's close — byte totals, exactly one
/// `ConnectionClosed`, and one log line — when dropped. Dropping (rather
/// than a call after `run` returns) means a connection task that panics or
/// is aborted still closes its books: its bytes are reported and the
/// open-connections gauge is decremented, with outcome `Aborted`.
struct ConnectionRecord {
    shared: Arc<Shared>,
    listener: Arc<ListenerInfo>,
    peer: SocketAddr,
    start: Instant,
    bytes: Arc<ByteCounters>,
    outcome: Option<ConnectionOutcome>,
    sni: Option<Hostname>,
    backend: Option<Backend>,
}

impl Drop for ConnectionRecord {
    fn drop(&mut self) {
        let outcome = self.outcome.unwrap_or(ConnectionOutcome::Aborted);
        let lifetime = self.start.elapsed();
        let (read, written) = self.bytes.totals();
        let listener = &*self.listener;
        let metrics = &self.shared.metrics;
        if read > 0 {
            metrics.record(MetricEvent::BytesRead {
                listener,
                bytes: read,
            });
        }
        if written > 0 {
            metrics.record(MetricEvent::BytesWritten {
                listener,
                bytes: written,
            });
        }
        metrics.record(MetricEvent::ConnectionClosed {
            listener,
            outcome,
            lifetime,
        });
        // Debug, not info: at thousands of connections per second a line per
        // connection costs real CPU and log volume. Failures that need
        // operator attention log their own warnings.
        tracing::debug!(
            peer = %self.peer,
            listener = %listener.address(),
            sni = self.sni.as_ref().map(Hostname::as_str),
            backend = self.backend.as_ref().map(tracing::field::display),
            outcome = outcome.as_str(),
            bytes_from_client = read,
            bytes_to_client = written,
            lifetime_ms = u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX),
            "connection closed"
        );
    }
}

/// Runs one connection to completion. The client stream is metered from
/// accept onward, so bytes from failed handshakes and partial alert writes
/// are counted too; totals are reported once, at close.
pub(super) async fn handle_connection(
    shared: Arc<Shared>,
    client: TcpStream,
    peer: SocketAddr,
    listener: Arc<ListenerInfo>,
) {
    let start = Instant::now();
    shared.metrics.record(MetricEvent::ConnectionOpened {
        listener: &listener,
    });
    let bytes = Arc::new(ByteCounters::default());
    let mut record = ConnectionRecord {
        shared: Arc::clone(&shared),
        listener: Arc::clone(&listener),
        peer,
        start,
        bytes: Arc::clone(&bytes),
        outcome: None,
        sni: None,
        backend: None,
    };
    let _ = client.set_nodelay(true);
    let activity = shared.config.idle_timeout.map(|_| Activity::new(start));
    let mut client = MeteredStream::new(client, bytes, activity.clone());
    let outcome = run(&shared, &mut client, activity.as_ref(), &mut record).await;
    record.outcome = Some(outcome);
}

async fn run(
    shared: &Shared,
    client: &mut MeteredStream<TcpStream>,
    activity: Option<&Activity>,
    log: &mut ConnectionRecord,
) -> ConnectionOutcome {
    let config = &shared.config;
    let listener = &*log.listener;
    let cancel = shared.gate.handshakes_cancelled();

    // ClientHello, under one overall deadline.
    let read = cancellable(
        cancel,
        tokio::time::timeout(
            config.client_hello_timeout,
            read_client_hello(client, &config.hello_limits),
        ),
    )
    .await;
    let (buffered, hello) = match read {
        None => return ConnectionOutcome::ShutdownCancelled,
        Some(Err(_elapsed)) => return ConnectionOutcome::HelloTimeout,
        Some(Ok(Err(HelloReadError::Parse(error)))) => {
            tracing::debug!(%error, "rejecting invalid ClientHello");
            return ConnectionOutcome::InvalidHello;
        }
        Some(Ok(Err(HelloReadError::Closed))) => return ConnectionOutcome::ClientClosed,
        Some(Ok(Err(HelloReadError::Io(error)))) => {
            tracing::debug!(%error, "client read failed before ClientHello completed");
            return ConnectionOutcome::ClientClosed;
        }
        Some(Ok(Ok(read))) => read,
    };

    let Some(host) = hello.sni().cloned() else {
        reject_unrecognized_name(shared, client).await;
        return ConnectionOutcome::MissingSni;
    };
    log.sni = Some(host.clone());

    // Route lookup.
    let lookup_start = Instant::now();
    let Some(resolved) = cancellable(
        cancel,
        resolve_route(&*shared.lookup, &host, config.lookup_timeout),
    )
    .await
    else {
        return ConnectionOutcome::ShutdownCancelled;
    };
    shared.metrics.record(MetricEvent::RouteLookup {
        listener,
        outcome: lookup_outcome(&resolved),
        elapsed: lookup_start.elapsed(),
    });
    let route = match resolved {
        Ok(route) => route,
        Err(RouteError::NotFound) => {
            reject_unrecognized_name(shared, client).await;
            return ConnectionOutcome::UnknownHost;
        }
        Err(RouteError::Lookup(error)) => {
            tracing::warn!(hostname = %host, %error, "route lookup failed");
            return ConnectionOutcome::LookupFailed;
        }
        Err(RouteError::Timeout) => {
            tracing::warn!(hostname = %host, "route lookup timed out");
            return ConnectionOutcome::LookupTimeout;
        }
    };
    log.backend = Some(route.backend().clone());

    // Upstream connect + replay, under one deadline.
    let upstream_start = Instant::now();
    let Some(connected) = cancellable(
        cancel,
        tokio::time::timeout(
            config.upstream_timeout,
            connect_and_replay(&shared.resolver, route.backend(), &buffered),
        ),
    )
    .await
    else {
        return ConnectionOutcome::ShutdownCancelled;
    };
    shared.metrics.record(MetricEvent::UpstreamConnect {
        listener,
        ok: matches!(connected, Ok(Ok(_))),
        elapsed: upstream_start.elapsed(),
    });
    let upstream = match connected {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(error)) => {
            tracing::warn!(backend = %route.backend(), %error, "upstream connect or replay failed");
            return ConnectionOutcome::UpstreamConnectFailed;
        }
        Err(_elapsed) => {
            tracing::warn!(backend = %route.backend(), "upstream connect or replay timed out");
            return ConnectionOutcome::UpstreamTimeout;
        }
    };
    drop(buffered);

    // Commit to proxying, or lose the race with shutdown.
    let Some(_pass) = shared.gate.enter() else {
        return ConnectionOutcome::ShutdownCancelled;
    };

    let mut upstream = upstream;
    // The idle clock measures the proxy stage only: restart it now, so time
    // spent on the lookup and upstream connect doesn't count as idle.
    if let Some(activity) = activity {
        activity.touch();
    }
    let end = proxy(
        client,
        &mut upstream,
        activity,
        config.idle_timeout,
        config.copy_buffer_size.max(MIN_COPY_BUFFER),
        &shared.force_cancel,
    )
    .await;
    match end {
        ProxyEnd::Finished => ConnectionOutcome::Proxied,
        ProxyEnd::Error(error) => {
            tracing::debug!(%error, "proxy ended with an I/O error");
            ConnectionOutcome::ProxyError
        }
        ProxyEnd::Idle => ConnectionOutcome::IdleTimeout,
        ProxyEnd::Forced => ConnectionOutcome::ShutdownForced,
    }
}

/// Runs `future` unless `cancel` fires first (checked first, so an
/// already-cancelled token always wins).
async fn cancellable<F: Future>(cancel: &CancellationToken, future: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        output = future => Some(output),
    }
}

fn lookup_outcome<T>(resolved: &Result<T, RouteError>) -> LookupOutcome {
    match resolved {
        Ok(_) => LookupOutcome::Found,
        Err(RouteError::NotFound) => LookupOutcome::NotFound,
        Err(RouteError::Lookup(_)) => LookupOutcome::Error,
        Err(RouteError::Timeout) => LookupOutcome::Timeout,
    }
}

async fn connect_and_replay(
    resolver: &Arc<Resolver>,
    backend: &Backend,
    buffered: &[u8],
) -> io::Result<TcpStream> {
    let port = backend.port().get();
    let mut stream = match backend.host() {
        BackendHost::Ip(ip) => TcpStream::connect(SocketAddr::new(*ip, port)).await?,
        BackendHost::Dns(name) => connect_any(&resolver.resolve(name, port).await?).await?,
    };
    stream.set_nodelay(true)?;
    stream.write_all(buffered).await?;
    Ok(stream)
}

/// Tries each address in order (the resolver rotates the order per call so
/// load spreads across them) and returns the first connection that succeeds.
async fn connect_any(addrs: &[SocketAddr]) -> io::Result<TcpStream> {
    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no addresses to connect to")))
}

/// Best-effort TLS `unrecognized_name` alert, then close. Bounded and
/// cancellable so a client that won't read can't hold the connection.
async fn reject_unrecognized_name(shared: &Shared, client: &mut MeteredStream<TcpStream>) {
    let send = async {
        client.write_all(&UNRECOGNIZED_NAME_ALERT).await?;
        client.shutdown().await
    };
    // Best effort; whatever was written is counted by the metered stream.
    let _ = cancellable(
        shared.gate.handshakes_cancelled(),
        tokio::time::timeout(ALERT_WRITE_TIMEOUT, send),
    )
    .await;
}
