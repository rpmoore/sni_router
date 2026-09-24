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

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::protocol::{ClientHelloInfo, HelloLimits, ParseError, parse_client_hello};

const READ_CHUNK: usize = 4096;

#[derive(Debug)]
pub(super) enum HelloReadError {
    /// The client closed before a complete ClientHello arrived.
    Closed,
    Io(io::Error),
    Parse(ParseError),
}

/// Reads until a complete ClientHello is buffered. Returns every byte read —
/// possibly more than the ClientHello — because all of it must be replayed
/// to the backend. The buffer never exceeds `limits.max_wire_bytes()`.
///
/// Has no deadline of its own; the caller bounds the whole call.
pub(super) async fn read_client_hello<S>(
    stream: &mut S,
    limits: &HelloLimits,
) -> Result<(Vec<u8>, ClientHelloInfo), HelloReadError>
where
    S: AsyncRead + Unpin,
{
    let max = limits.max_wire_bytes();
    let mut buf = Vec::with_capacity(READ_CHUNK.min(max));
    loop {
        let remaining = max - buf.len();
        if remaining == 0 {
            return Err(HelloReadError::Parse(ParseError::TooLarge));
        }
        // Exact growth: plain `reserve` doubles capacity, which would let a
        // hostile client pin ~2x the wire limit per connection.
        buf.reserve_exact(READ_CHUNK.min(remaining));
        let read = (&mut *stream)
            .take(remaining as u64)
            .read_buf(&mut buf)
            .await
            .map_err(HelloReadError::Io)?;
        if read == 0 {
            return Err(HelloReadError::Closed);
        }
        match parse_client_hello(&buf, limits) {
            Ok(info) => return Ok((buf, info)),
            Err(ParseError::Incomplete) => continue,
            Err(error) => return Err(HelloReadError::Parse(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ClientHelloBuilder;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn reads_a_hello_delivered_one_byte_at_a_time() {
        let wire = ClientHelloBuilder::new().sni("drip.test").build();
        let (mut client, mut server) = tokio::io::duplex(1);
        let writer = tokio::spawn({
            let wire = wire.clone();
            async move { client.write_all(&wire).await }
        });
        let (buf, info) = read_client_hello(&mut server, &HelloLimits::default())
            .await
            .unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(buf, wire);
        assert_eq!(info.sni().unwrap().as_str(), "drip.test");
    }

    #[tokio::test]
    async fn keeps_bytes_read_past_the_hello_for_replay() {
        let mut wire = ClientHelloBuilder::new().sni("early.test").build();
        let hello_len = wire.len();
        wire.extend_from_slice(b"early-data");
        let mut reader = &wire[..];
        let (buf, info) = read_client_hello(&mut reader, &HelloLimits::default())
            .await
            .unwrap();
        assert_eq!(info.consumed(), hello_len);
        assert_eq!(buf, wire);
    }

    #[tokio::test]
    async fn eof_before_complete_hello_is_closed() {
        let wire = ClientHelloBuilder::new().sni("short.test").build();
        let mut reader = &wire[..wire.len() - 1];
        assert!(matches!(
            read_client_hello(&mut reader, &HelloLimits::default()).await,
            Err(HelloReadError::Closed)
        ));
    }

    #[tokio::test]
    async fn garbage_fails_on_first_read() {
        let mut reader = &b"GET / HTTP/1.1\r\n\r\n"[..];
        assert!(matches!(
            read_client_hello(&mut reader, &HelloLimits::default()).await,
            Err(HelloReadError::Parse(ParseError::NotHandshake))
        ));
    }

    #[tokio::test]
    async fn buffer_never_exceeds_wire_limit() {
        // A valid-looking record stream that never completes the hello: the
        // declared handshake length is at the limit but records keep coming
        // with max-size padding beyond the record-count allowance.
        let limits = HelloLimits::new(1024, 2);
        let handshake = ClientHelloBuilder::new()
            .sni("x.test")
            .padding_to(1024)
            .handshake();
        let wire = ClientHelloBuilder::records_of(&handshake, 100);
        let mut reader = &wire[..];
        assert!(matches!(
            read_client_hello(&mut reader, &limits).await,
            Err(HelloReadError::Parse(ParseError::TooLarge))
        ));
    }

    #[tokio::test]
    async fn buffer_capacity_stays_within_the_wire_limit() {
        let limits = HelloLimits::default();
        let wire = ClientHelloBuilder::new()
            .sni("big.test")
            .padding_to(HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES)
            .build();
        let (mut client, mut server) = tokio::io::duplex(1500);
        let writer = tokio::spawn(async move { client.write_all(&wire).await });
        let (buf, _) = read_client_hello(&mut server, &limits).await.unwrap();
        writer.await.unwrap().unwrap();
        assert!(
            buf.capacity() <= limits.max_wire_bytes(),
            "capacity {} exceeds wire limit {}",
            buf.capacity(),
            limits.max_wire_bytes()
        );
    }
}
