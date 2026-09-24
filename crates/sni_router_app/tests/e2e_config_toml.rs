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

//! The app end to end: TOML config in, routed TLS bytes and admin HTTP out.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use sni_router::testing::ClientHelloBuilder;
use sni_router_app::BoundApp;
use sni_router_app::config::AppConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Echoes each connection, prefixed with `tag` so the test can tell which
/// backend answered.
async fn tagged_echo(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let Ok(read) = stream.read(&mut buf).await else {
                    return;
                };
                let _ = stream.write_all(tag.as_bytes()).await;
                let _ = stream.write_all(&buf[..read]).await;
            });
        }
    });
    addr
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
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

async fn routed_tag(addr: SocketAddr, sni: &str) -> String {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&ClientHelloBuilder::new().sni(sni).build())
        .await
        .unwrap();
    let mut tag = [0u8; 3];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut tag))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(tag.to_vec()).unwrap()
}

#[tokio::test]
async fn toml_routes_traffic_and_serves_admin_endpoints() {
    let api = tagged_echo("API").await;
    let web = tagged_echo("WEB").await;
    let config = AppConfig::from_toml_str(&format!(
        r#"
        [server]
        listen = ["127.0.0.1:0"]
        shutdown_grace_secs = 1

        [admin]
        listen = "127.0.0.1:0"
        health_path = "/health"

        [[routes]]
        hostname = "api.example.com"
        backend = "{api}"

        [[routes]]
        hostname = "*.apps.example.com"
        backend = "{web}"
        "#
    ))
    .unwrap();

    let app = BoundApp::bind(config, PathBuf::from("unused.toml"))
        .await
        .unwrap();
    let data = app.listen_addrs()[0];
    let admin = app.admin_addr();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(app.run(shutdown.clone()));

    assert_eq!(routed_tag(data, "API.example.com").await, "API");
    assert_eq!(routed_tag(data, "shop.apps.example.com").await, "WEB");

    let health = http_get(admin, "/health").await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    // Golden metric names: these are the app's public contract with
    // dashboards and alerts. Byte and close metrics are recorded when a
    // connection closes, which happens asynchronously, so poll briefly.
    let names = [
        "snirouter_connections_opened_total{",
        "snirouter_connections_closed_total{",
        "snirouter_open_connections{",
        "snirouter_bytes_read_total{",
        "snirouter_bytes_written_total{",
        "snirouter_connection_duration_seconds_bucket{",
        "snirouter_route_lookup_duration_seconds_bucket{",
        "snirouter_upstream_connect_duration_seconds_bucket{",
    ];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let metrics = loop {
        let metrics = http_get(admin, "/metrics").await;
        if names.iter().all(|name| metrics.contains(name)) {
            break metrics;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "missing metric names in:\n{metrics}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(metrics.contains(&format!("address=\"{data}\"")));
    assert!(metrics.contains("network=\"tcp\""));
    assert!(metrics.contains("outcome=\"found\""));
    assert!(!metrics.contains("_total_total"));

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("app did not shut down")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn bind_conflict_fails_startup() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = AppConfig::from_toml_str(&format!(
        "[server]\nlisten = [\"{}\"]\n[admin]\nlisten = \"127.0.0.1:0\"\n",
        taken.local_addr().unwrap()
    ))
    .unwrap();
    let error = BoundApp::bind(config, PathBuf::from("unused.toml"))
        .await
        .err()
        .expect("bind should fail");
    assert!(error.to_string().contains("failed to bind"));
}
