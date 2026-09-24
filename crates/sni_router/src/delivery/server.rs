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

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::RouterConfig;
use super::connection::{Shared, handle_connection};
use super::gate::ProxyGate;
use super::resolver::Resolver;
use crate::lookup::RouteLookup;
use crate::metrics::{ListenerInfo, MetricsSink};

/// Pause after a failed `accept` (e.g. EMFILE) so the loop doesn't spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Accepts TLS connections and routes each one by SNI.
pub struct Router {
    lookup: Arc<dyn RouteLookup>,
    metrics: Arc<dyn MetricsSink>,
    config: RouterConfig,
}

impl Router {
    pub fn new(
        lookup: Arc<dyn RouteLookup>,
        metrics: Arc<dyn MetricsSink>,
        config: RouterConfig,
    ) -> Self {
        Self {
            lookup,
            metrics,
            config,
        }
    }

    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// Accepts on every listener until `shutdown` is cancelled, then shuts
    /// down in order:
    ///
    /// 1. stop accepting (this also abandons any wait for a connection slot);
    /// 2. cancel every connection still handshaking (reading the ClientHello,
    ///    looking up the route, connecting, or replaying);
    /// 3. let proxied connections drain for `shutdown_grace`;
    /// 4. force-close whatever is left, then return once every connection
    ///    task has finished.
    ///
    /// `max_connections` is shared across all listeners. One accept loop
    /// takes a connection slot *before* accepting, then accepts from
    /// whichever listener is ready first, so under load new connections
    /// queue in the kernel backlog instead of being accepted and starved, and
    /// an idle listener never holds a slot another listener could use.
    pub async fn serve(
        &self,
        listeners: Vec<(TcpListener, ListenerInfo)>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let shared = Arc::new(Shared {
            lookup: Arc::clone(&self.lookup),
            metrics: Arc::clone(&self.metrics),
            config: self.config.clone(),
            gate: ProxyGate::new(),
            force_cancel: CancellationToken::new(),
            resolver: Arc::new(Resolver::new(
                self.config.dns_cache_ttl,
                self.config.max_concurrent_dns_lookups,
            )),
        });
        let slots = Arc::new(Semaphore::new(self.config.max_connections));
        let tracker = TaskTracker::new();

        let listeners: Vec<_> = listeners
            .into_iter()
            .map(|(listener, info)| {
                tracing::info!(address = %info.address(), network = info.network(), "sni_router listening");
                (listener, Arc::new(info))
            })
            .collect();
        let accept_task = tokio::spawn(accept_loop(
            listeners,
            Arc::clone(&shared),
            slots,
            tracker.clone(),
            shutdown.clone(),
        ));

        shutdown.cancelled().await;
        if let Err(error) = accept_task.await {
            tracing::error!(%error, "accept loop panicked");
        }

        let proxying = shared.gate.close();
        tracker.close();
        tracing::info!(
            proxying,
            grace_secs = self.config.shutdown_grace.as_secs(),
            "shutdown: stopped accepting and cancelled handshakes; draining proxied connections"
        );
        if tokio::time::timeout(self.config.shutdown_grace, tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                remaining = tracker.len(),
                "shutdown grace period elapsed; force-closing proxied connections"
            );
            shared.force_cancel.cancel();
            tracker.wait().await;
        }
        tracing::info!("shutdown complete");
        Ok(())
    }
}

async fn accept_loop(
    listeners: Vec<(TcpListener, Arc<ListenerInfo>)>,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
) {
    let mut next = 0;
    loop {
        let slot = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            slot = Arc::clone(&slots).acquire_owned() => match slot {
                Ok(slot) => slot,
                Err(_closed) => return,
            },
        };
        let (index, accepted) = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            accepted = accept_any(&listeners, &mut next) => accepted,
        };
        let info = &listeners[index].1;
        match accepted {
            Ok((stream, peer)) => {
                let shared = Arc::clone(&shared);
                let info = Arc::clone(info);
                tracker.spawn(async move {
                    let _slot = slot;
                    handle_connection(shared, stream, peer, info).await;
                });
            }
            Err(error) => {
                // Return the slot before backing off so repeated accept
                // failures (e.g. EMFILE) don't also shrink capacity.
                drop(slot);
                tracing::warn!(%error, address = %info.address(), "accept failed");
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                }
            }
        }
    }
}

/// Accepts from whichever listener is ready first. Polling starts at a
/// rotating index so a busy listener can't starve the others.
fn accept_any<'a>(
    listeners: &'a [(TcpListener, Arc<ListenerInfo>)],
    next: &'a mut usize,
) -> impl Future<Output = (usize, io::Result<(TcpStream, SocketAddr)>)> + 'a {
    std::future::poll_fn(move |cx| {
        let count = listeners.len();
        for offset in 0..count {
            let index = (*next + offset) % count;
            if let Poll::Ready(accepted) = listeners[index].0.poll_accept(cx) {
                *next = (index + 1) % count;
                return Poll::Ready((index, accepted));
            }
        }
        Poll::Pending
    })
}
