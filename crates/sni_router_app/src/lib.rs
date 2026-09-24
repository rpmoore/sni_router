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

//! The `sni_router` server: the [`sni_router`] library wired to a TOML route
//! file, a Prometheus/health admin listener, and SIGHUP reload.

pub mod admin;
pub mod config;
pub mod otel_metrics;
pub mod reload;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use sni_router::{ListenerInfo, Router};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::admin::{AdminServer, Health};
use crate::config::{AppConfig, FileRouteLookup};
use crate::otel_metrics::OtelMetrics;
use crate::reload::{StartupSettings, spawn_sighup_reload};

/// The app with every socket bound, ready to serve. Binding first means a
/// port conflict fails startup before anything reports healthy.
pub struct BoundApp {
    config_path: PathBuf,
    startup: StartupSettings,
    router: Router,
    lookup: Arc<FileRouteLookup>,
    listeners: Vec<(TcpListener, ListenerInfo)>,
    admin: AdminServer,
    health: Health,
}

impl BoundApp {
    pub async fn bind(config: AppConfig, config_path: PathBuf) -> io::Result<Self> {
        let mut listeners = Vec::with_capacity(config.server.listen.len());
        for address in &config.server.listen {
            let listener = TcpListener::bind(address).await.map_err(|error| {
                io::Error::new(error.kind(), format!("failed to bind {address}: {error}"))
            })?;
            let info = ListenerInfo::from_listener(&listener)?;
            listeners.push((listener, info));
        }

        let metrics = OtelMetrics::new().map_err(io::Error::other)?;
        let health = Health::new();
        let admin = AdminServer::bind(
            config.admin.listen,
            metrics.registry(),
            config.admin.health_path.clone(),
            health.clone(),
        )
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to bind admin {}: {error}", config.admin.listen),
            )
        })?;

        if config.routes.is_empty() {
            tracing::warn!("no [[routes]] configured; every connection will be rejected");
        }
        let lookup = Arc::new(FileRouteLookup::new(config.routes));
        let router = Router::new(
            lookup.clone(),
            Arc::new(metrics),
            config.server.router.clone(),
        );
        Ok(Self {
            config_path,
            startup: StartupSettings {
                server: config.server,
                admin: config.admin,
            },
            router,
            lookup,
            listeners,
            admin,
            health,
        })
    }

    /// Bound data-plane addresses (resolves `:0` ports).
    pub fn listen_addrs(&self) -> Vec<SocketAddr> {
        self.listeners
            .iter()
            .map(|(_, info)| info.address())
            .collect()
    }

    pub fn admin_addr(&self) -> SocketAddr {
        self.admin.local_addr()
    }

    /// Serves until `shutdown` is cancelled. The health endpoint turns 503
    /// as soon as shutdown starts and stays up until proxied connections
    /// have drained, so probes see the drain rather than a refused port.
    pub async fn run(self, shutdown: CancellationToken) -> io::Result<()> {
        tracing::info!(
            routes = self.lookup.len(),
            admin = %self.admin.local_addr(),
            "sni_router starting"
        );
        let admin_shutdown = CancellationToken::new();
        let admin_task = tokio::spawn(self.admin.serve(admin_shutdown.clone()));
        let reload_task = spawn_sighup_reload(
            self.config_path,
            self.startup,
            Arc::clone(&self.lookup),
            shutdown.clone(),
        );
        let drain_watch = tokio::spawn({
            let shutdown = shutdown.clone();
            let health = self.health.clone();
            async move {
                shutdown.cancelled().await;
                health.set_draining();
            }
        });

        let served = self.router.serve(self.listeners, shutdown).await;

        admin_shutdown.cancel();
        let _ = drain_watch.await;
        let _ = reload_task.await;
        match admin_task.await {
            Ok(result) => result?,
            Err(error) => tracing::error!(%error, "admin server task failed"),
        }
        served
    }
}
