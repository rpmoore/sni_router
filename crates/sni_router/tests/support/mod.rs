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

#![allow(dead_code)] // each test binary uses a different subset

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sni_router::metrics::ConnectionOutcome;
use sni_router::testing::RecordingMetrics;
use sni_router::{ListenerInfo, RouteLookup, Router, RouterConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A router serving on loopback with a recording metrics sink.
pub struct Harness {
    pub addrs: Vec<SocketAddr>,
    pub shutdown: CancellationToken,
    pub task: JoinHandle<io::Result<()>>,
    pub metrics: Arc<RecordingMetrics>,
}

impl Harness {
    pub async fn start(lookup: Arc<dyn RouteLookup>, config: RouterConfig) -> Self {
        Self::start_with_listeners(lookup, config, 1).await
    }

    pub async fn start_with_listeners(
        lookup: Arc<dyn RouteLookup>,
        config: RouterConfig,
        count: usize,
    ) -> Self {
        let metrics = Arc::new(RecordingMetrics::new());
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for index in 0..count {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let info = ListenerInfo::from_listener(&listener)
                .unwrap()
                .with_name(format!("listener-{index}"));
            addrs.push(info.address());
            listeners.push((listener, info));
        }
        let router = Router::new(lookup, metrics.clone(), config);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { router.serve(listeners, shutdown).await }
        });
        Self {
            addrs,
            shutdown,
            task,
            metrics,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addrs[0]
    }

    pub async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr()).await.unwrap()
    }

    /// Waits until `count` connections have closed and returns their
    /// outcomes.
    pub async fn outcomes(&self, count: usize) -> Vec<ConnectionOutcome> {
        eventually(|| {
            let outcomes = self.metrics.outcomes();
            (outcomes.len() >= count).then_some(outcomes)
        })
        .await
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap().unwrap();
    }
}

/// Polls `check` every 10ms for up to 5s.
pub async fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How a test backend treats each connection.
#[derive(Clone, Copy, Debug)]
pub enum BackendMode {
    /// Echo every byte back as it arrives; close after the client closes.
    Echo,
    /// Read until EOF, then reply with `received:<n>` and close. Proves
    /// half-close reaches the backend and the reverse direction survives it.
    ReplyAfterEof,
    /// Never accept, with the accept backlog already full, so new TCP
    /// connects stall (the SYN is dropped) instead of completing.
    Stalled,
}

/// A loopback backend that records the bytes each connection sent.
pub struct Backend {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Backend {
    pub async fn start(mode: BackendMode) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        if let BackendMode::Stalled = mode {
            return Self::stalled(received).await;
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn({
            let received = received.clone();
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let index = {
                        let mut received = received.lock().unwrap();
                        received.push(Vec::new());
                        received.len() - 1
                    };
                    tokio::spawn(serve(stream, mode, received.clone(), index));
                }
            }
        });
        Self {
            addr,
            received,
            task,
        }
    }

    async fn stalled(received: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        // Fill the accept queue; once it's full the kernel drops new SYNs.
        let mut fillers = Vec::new();
        for _ in 0..8 {
            if let Ok(Ok(stream)) =
                tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(addr)).await
            {
                fillers.push(stream);
            }
        }
        let task = tokio::spawn(async move {
            let _hold = (listener, fillers);
            std::future::pending::<()>().await;
        });
        Self {
            addr,
            received,
            task,
        }
    }

    pub fn route(&self) -> String {
        self.addr.to_string()
    }

    pub fn connections(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    pub fn received(&self, index: usize) -> Vec<u8> {
        self.received.lock().unwrap()[index].clone()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    mode: BackendMode,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
    index: usize,
) {
    let mut buf = [0u8; 8192];
    loop {
        let read = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        received.lock().unwrap()[index].extend_from_slice(&buf[..read]);
        if let BackendMode::Echo = mode
            && stream.write_all(&buf[..read]).await.is_err()
        {
            return;
        }
    }
    if let BackendMode::ReplyAfterEof = mode {
        let count = received.lock().unwrap()[index].len();
        let _ = stream
            .write_all(format!("received:{count}").as_bytes())
            .await;
    }
    let _ = stream.shutdown().await;
}

/// Reads exactly `len` bytes.
pub async fn read_exact(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0; len];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .unwrap();
    buf
}

/// Reads until EOF (or a reset), returning what arrived.
pub async fn read_to_close(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("connection was not closed");
    buf
}

pub fn fast_config() -> RouterConfig {
    let mut config = RouterConfig::default();
    config.client_hello_timeout = Duration::from_secs(2);
    config.lookup_timeout = Duration::from_secs(1);
    config.upstream_timeout = Duration::from_secs(2);
    config.shutdown_grace = Duration::from_secs(2);
    config
}
