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

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
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
    /// `max_connections` is shared across all listeners. A connection slot
    /// is taken *before* `accept`, so under load new connections queue in the
    /// kernel backlog instead of being accepted and starved.
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

        let mut accept_loops = JoinSet::new();
        for (listener, info) in listeners {
            tracing::info!(address = %info.address(), network = info.network(), "sni_router listening");
            accept_loops.spawn(accept_loop(
                listener,
                Arc::new(info),
                Arc::clone(&shared),
                Arc::clone(&slots),
                tracker.clone(),
                shutdown.clone(),
            ));
        }

        shutdown.cancelled().await;
        while let Some(result) = accept_loops.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "accept loop panicked");
            }
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
    listener: TcpListener,
    info: Arc<ListenerInfo>,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
) {
    loop {
        let slot = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            slot = Arc::clone(&slots).acquire_owned() => match slot {
                Ok(slot) => slot,
                Err(_closed) => return,
            },
        };
        let accepted = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, peer)) => {
                let shared = Arc::clone(&shared);
                let info = Arc::clone(&info);
                tracker.spawn(async move {
                    let _slot = slot;
                    handle_connection(shared, stream, peer, info).await;
                });
            }
            Err(error) => {
                tracing::warn!(%error, address = %info.address(), "accept failed");
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                }
            }
        }
    }
}
