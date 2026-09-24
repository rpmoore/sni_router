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

//! The admin HTTP listener: `GET /metrics` and `GET <health_path>`.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use prometheus::{Encoder, Registry, TextEncoder};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(1);

/// Bounds that keep slow or idle clients from starving health probes and
/// scrapes. Connections serve one request (no keep-alive), must send their
/// request headers within `header_timeout`, and are cut off after
/// `connection_timeout` regardless. These raise the cost of starving the
/// listener but can't make an unauthenticated port immune: keep `[admin]`
/// on loopback or a private interface.
#[derive(Clone, Copy, Debug)]
struct AdminLimits {
    max_connections: usize,
    header_timeout: Duration,
    connection_timeout: Duration,
}

impl Default for AdminLimits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            header_timeout: Duration::from_secs(2),
            connection_timeout: Duration::from_secs(5),
        }
    }
}

/// Whether the health endpoint reports ready. Flipped to draining when
/// shutdown starts so load balancers stop sending new connections first.
#[derive(Clone, Debug, Default)]
pub struct Health {
    draining: Arc<AtomicBool>,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_draining(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }
}

pub struct AdminServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    routes: Arc<AdminRoutes>,
    limits: AdminLimits,
}

struct AdminRoutes {
    registry: Registry,
    health_path: String,
    health: Health,
}

impl AdminServer {
    pub async fn bind(
        address: SocketAddr,
        registry: Registry,
        health_path: String,
        health: Health,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
            routes: Arc::new(AdminRoutes {
                registry,
                health_path,
                health,
            }),
            limits: AdminLimits::default(),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves until `shutdown` is cancelled, then gives open connections a
    /// short grace period before aborting them.
    pub async fn serve(self, shutdown: CancellationToken) -> io::Result<()> {
        let slots = Arc::new(Semaphore::new(self.limits.max_connections));
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = accept_with_slot(&self.listener, Arc::clone(&slots)) => {
                    match accepted {
                        Ok(Some((stream, slot))) => spawn_connection(&mut tasks, stream, slot, Arc::clone(&self.routes), self.limits),
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(%error, "admin accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "admin connection task panicked");
                    }
                }
            }
        }
        if tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        Ok(())
    }
}

async fn accept_with_slot(
    listener: &TcpListener,
    slots: Arc<Semaphore>,
) -> io::Result<Option<(TcpStream, OwnedSemaphorePermit)>> {
    let Ok(slot) = slots.acquire_owned().await else {
        return Ok(None);
    };
    let (stream, _peer) = listener.accept().await?;
    Ok(Some((stream, slot)))
}

fn spawn_connection(
    tasks: &mut JoinSet<()>,
    stream: TcpStream,
    slot: OwnedSemaphorePermit,
    routes: Arc<AdminRoutes>,
    limits: AdminLimits,
) {
    tasks.spawn(async move {
        let _slot = slot;
        let serve = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_timeout)
            .keep_alive(false)
            .serve_connection(
                TokioIo::new(stream),
                service_fn(move |request| {
                    let routes = Arc::clone(&routes);
                    async move { Ok::<_, Infallible>(routes.handle(&request)) }
                }),
            );
        match tokio::time::timeout(limits.connection_timeout, serve).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::debug!(%error, "admin connection error"),
            Err(_) => tracing::debug!("admin connection timed out"),
        }
    });
}

impl AdminRoutes {
    fn handle(&self, request: &Request<Incoming>) -> Response<Full<Bytes>> {
        if request.method() != Method::GET {
            return text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
        }
        let path = request.uri().path();
        if path == "/metrics" {
            return self.metrics();
        }
        if path == self.health_path {
            return if self.health.is_draining() {
                text(StatusCode::SERVICE_UNAVAILABLE, "draining\n")
            } else {
                text(StatusCode::OK, "ok\n")
            };
        }
        text(StatusCode::NOT_FOUND, "not found\n")
    }

    fn metrics(&self) -> Response<Full<Bytes>> {
        let mut body = Vec::new();
        if let Err(error) = TextEncoder::new().encode(&self.registry.gather(), &mut body) {
            tracing::warn!(%error, "failed to encode Prometheus metrics");
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "metrics encoding failed\n",
            );
        }
        let mut response = Response::new(Full::new(Bytes::from(body)));
        response.headers_mut().insert(
            hyper::header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        );
        response
    }
}

fn text(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn idle_connections_cannot_starve_health_checks() {
        let mut server = AdminServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            Registry::new(),
            "/health".into(),
            Health::new(),
        )
        .await
        .unwrap();
        server.limits = AdminLimits {
            max_connections: 4,
            header_timeout: Duration::from_millis(200),
            connection_timeout: Duration::from_secs(5),
        };
        let addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        // Take every slot with clients that never send a request.
        let mut idle = Vec::new();
        for _ in 0..4 {
            idle.push(TcpStream::connect(addr).await.unwrap());
        }
        let started = tokio::time::Instant::now();
        let health = tokio::time::timeout(Duration::from_secs(2), get(addr, "/health"))
            .await
            .expect("health check starved by idle connections");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        assert!(started.elapsed() < Duration::from_secs(1));

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn serves_health_metrics_and_404() {
        let registry = Registry::new();
        let counter = prometheus::IntCounter::new("test_total", "test").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();
        let health = Health::new();
        let server = AdminServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            registry,
            "/health".into(),
            health.clone(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        assert!(get(addr, "/health").await.starts_with("HTTP/1.1 200"));
        let metrics = get(addr, "/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200"));
        assert!(metrics.contains("test_total 1"));
        assert!(get(addr, "/nope").await.starts_with("HTTP/1.1 404"));

        health.set_draining();
        let draining = get(addr, "/health").await;
        assert!(draining.starts_with("HTTP/1.1 503"));
        assert!(draining.ends_with("draining\n"));

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }
}
