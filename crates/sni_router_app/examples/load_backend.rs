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

//! Toy backend for `just loadtest*` (see `../../../Justfile`). Drains
//! whatever a connection sends (the router's replayed ClientHello plus a
//! request payload), and only once the client half-closes does it write
//! back `--reply-bytes` of data and half-close itself — so a load
//! generator reading to EOF measures a real request-in/reply-out round
//! trip through the router in both directions, the same shape as
//! `BackendMode::ReplyAfterEof` in `crates/sni_router/tests/support`.
//!
//! Usage: `load_backend --listen 127.0.0.1:19101 --reply-bytes 4096`

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// Bounds the drain phase, so a client that connects but never sends and
/// half-closes (a bug elsewhere, not real traffic) can't pin this task and
/// its fd open forever.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let reply = Arc::new(vec![0xab_u8; args.reply_bytes]);
    let listener = TcpListener::bind(args.listen)
        .await
        .unwrap_or_else(|error| panic!("bind {}: {error}", args.listen));
    println!(
        "load_backend listening on {} (reply-bytes={})",
        args.listen, args.reply_bytes
    );
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(serve(stream, Arc::clone(&reply)));
            }
            Err(error) => {
                eprintln!("accept error: {error}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve(mut stream: TcpStream, reply: Arc<Vec<u8>>) {
    let mut buf = [0u8; 8 * 1024];
    loop {
        match timeout(READ_TIMEOUT, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                eprintln!("connection read error: {error}, dropping it without a reply");
                return;
            }
            Err(_) => {
                eprintln!("connection read timed out after {READ_TIMEOUT:?}, dropping it");
                return;
            }
        }
    }
    let _ = stream.write_all(&reply).await;
    let _ = stream.shutdown().await;
}

struct Args {
    listen: SocketAddr,
    reply_bytes: usize,
}

impl Args {
    fn parse() -> Self {
        let mut listen = None;
        let mut reply_bytes = None;
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .unwrap_or_else(|| panic!("{flag} needs a value"));
            match flag.as_str() {
                "--listen" => listen = Some(value.parse().expect("--listen: invalid address")),
                "--reply-bytes" => {
                    reply_bytes = Some(value.parse().expect("--reply-bytes: invalid number"))
                }
                other => panic!("unknown flag {other}"),
            }
        }
        Self {
            listen: listen.expect("--listen is required"),
            reply_bytes: reply_bytes.expect("--reply-bytes is required"),
        }
    }
}
