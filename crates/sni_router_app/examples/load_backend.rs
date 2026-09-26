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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
        let Ok((stream, _peer)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(serve(stream, Arc::clone(&reply)));
    }
}

async fn serve(mut stream: TcpStream, reply: Arc<Vec<u8>>) {
    let mut buf = [0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
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
