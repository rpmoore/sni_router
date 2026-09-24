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

//! TLS ClientHello parsing: pure, synchronous, no I/O.
//!
//! The router only needs the SNI, so it parses just enough of the TLS record
//! layer and ClientHello to find the `server_name` extension. Everything read
//! off the wire is untrusted; every length is bounds-checked and every limit
//! is enforced before buffering more.

mod alert;
#[cfg(test)]
mod fixtures;
mod hello;
mod reader;
mod record;

use std::fmt;

pub use alert::UNRECOGNIZED_NAME_ALERT;
pub use record::parse_client_hello;

use crate::lookup::{Hostname, NameError};

/// TLS caps a record's plaintext fragment at 2^14 bytes (RFC 8446 §5.1).
pub const MAX_RECORD_PAYLOAD: usize = 16_384;
/// Size of a TLS record header: type (1), version (2), length (2).
pub const RECORD_HEADER_LEN: usize = 5;
/// Size of a handshake message header: type (1), length (3).
pub const HANDSHAKE_HEADER_LEN: usize = 4;

/// Bounds on how much ClientHello the router will buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloLimits {
    /// Largest declared ClientHello handshake length accepted.
    pub max_handshake_bytes: usize,
    /// Most TLS records the ClientHello may be fragmented across.
    pub max_records: usize,
}

impl HelloLimits {
    pub const DEFAULT_MAX_HANDSHAKE_BYTES: usize = 32 * 1024;
    pub const DEFAULT_MAX_RECORDS: usize = 32;

    pub fn new(max_handshake_bytes: usize, max_records: usize) -> Self {
        Self {
            max_handshake_bytes,
            max_records,
        }
    }

    /// Most raw wire bytes a complete ClientHello can occupy under these
    /// limits: the handshake header and body plus one record header per
    /// allowed record.
    pub fn max_wire_bytes(&self) -> usize {
        HANDSHAKE_HEADER_LEN + self.max_handshake_bytes + self.max_records * RECORD_HEADER_LEN
    }
}

impl Default for HelloLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_HANDSHAKE_BYTES, Self::DEFAULT_MAX_RECORDS)
    }
}

/// What the router learned from a complete ClientHello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientHelloInfo {
    sni: Option<Hostname>,
    consumed: usize,
}

impl ClientHelloInfo {
    /// The `host_name` from the `server_name` extension, if the client sent
    /// one. With ECH this is the outer (public) name.
    pub fn sni(&self) -> Option<&Hostname> {
        self.sni.as_ref()
    }

    /// Wire bytes up to the end of the TLS record that completed the
    /// ClientHello. Bytes after this (more handshake data, early data) belong
    /// to the connection and must be replayed too.
    pub fn consumed(&self) -> usize {
        self.consumed
    }
}

/// Why a buffer isn't (yet) a usable ClientHello.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Not enough bytes yet; read more and try again.
    Incomplete,
    /// The first byte isn't a TLS handshake record.
    NotHandshake,
    /// A record header is malformed, or a non-handshake record arrived
    /// before the ClientHello completed.
    InvalidRecord,
    /// The first handshake message isn't a ClientHello.
    NotClientHello,
    /// The ClientHello exceeds [`HelloLimits`].
    TooLarge,
    /// The ClientHello body is structurally invalid.
    Malformed(&'static str),
    /// The SNI host_name isn't a valid hostname.
    InvalidServerName(NameError),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Incomplete => f.write_str("incomplete ClientHello"),
            ParseError::NotHandshake => f.write_str("not a TLS handshake record"),
            ParseError::InvalidRecord => f.write_str("invalid TLS record"),
            ParseError::NotClientHello => f.write_str("first handshake message is not ClientHello"),
            ParseError::TooLarge => f.write_str("ClientHello exceeds configured limits"),
            ParseError::Malformed(what) => write!(f, "malformed ClientHello: {what}"),
            ParseError::InvalidServerName(error) => write!(f, "invalid SNI host_name: {error}"),
        }
    }
}

impl std::error::Error for ParseError {}
