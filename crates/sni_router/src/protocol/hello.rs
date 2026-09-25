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

//! ClientHello body and extension parsing (RFC 8446 §4.1.2, RFC 6066 §3).

use super::ParseError;
use super::reader::Reader;
use crate::lookup::{Hostname, NameError};

const EXTENSION_SERVER_NAME: u16 = 0;
const NAME_TYPE_HOST_NAME: u8 = 0;
const RANDOM_LEN: usize = 32;
const MAX_SESSION_ID_LEN: usize = 32;

/// Parses a ClientHello body (after the 4-byte handshake header) and returns
/// its SNI host_name, if any. Client random and session ID are skipped and
/// never surfaced, so they can't end up in logs.
pub(super) fn parse_client_hello_body(body: &[u8]) -> Result<Option<Hostname>, ParseError> {
    let mut reader = Reader::new(body);
    let version = reader
        .u16()
        .ok_or(ParseError::Malformed("truncated legacy_version"))?;
    if version >> 8 != 0x03 {
        return Err(ParseError::Malformed("unsupported legacy_version"));
    }
    reader
        .bytes(RANDOM_LEN)
        .ok_or(ParseError::Malformed("truncated random"))?;
    let session_id = reader
        .vec8()
        .ok_or(ParseError::Malformed("truncated legacy_session_id"))?;
    if session_id.len() > MAX_SESSION_ID_LEN {
        return Err(ParseError::Malformed(
            "legacy_session_id longer than 32 bytes",
        ));
    }
    let cipher_suites = reader
        .vec16()
        .ok_or(ParseError::Malformed("truncated cipher_suites"))?;
    if cipher_suites.is_empty() || cipher_suites.len() % 2 != 0 {
        return Err(ParseError::Malformed(
            "cipher_suites length is empty or odd",
        ));
    }
    let compression = reader.vec8().ok_or(ParseError::Malformed(
        "truncated legacy_compression_methods",
    ))?;
    if compression.is_empty() {
        return Err(ParseError::Malformed("empty legacy_compression_methods"));
    }
    if reader.is_empty() {
        // Pre-TLS-1.2 clients may omit extensions entirely.
        return Ok(None);
    }
    let extensions = reader
        .vec16()
        .ok_or(ParseError::Malformed("truncated extensions"))?;
    if !reader.is_empty() {
        return Err(ParseError::Malformed("trailing bytes after extensions"));
    }
    parse_extensions(extensions)
}

/// Extension types seen so far, for duplicate detection. Sized by *count*,
/// not by the extensions block's byte length: a single large extension
/// (e.g. `padding`, which real clients routinely send to dodge
/// TLS-intolerant middleboxes) can dominate the byte count while adding
/// only one type, so a byte-length-derived capacity would over-allocate.
/// Real ClientHellos (Chrome/Firefox, including GREASE) send well under
/// `INLINE_EXTENSIONS` distinct extensions, so this stays on the stack;
/// only a hello packed with more spills to the heap.
const INLINE_EXTENSIONS: usize = 32;

enum SeenTypes {
    Inline {
        types: [u16; INLINE_EXTENSIONS],
        len: usize,
    },
    Heap(Vec<u16>),
}

impl SeenTypes {
    fn new() -> Self {
        Self::Inline {
            types: [0; INLINE_EXTENSIONS],
            len: 0,
        }
    }

    fn push(&mut self, value: u16) {
        match self {
            Self::Inline { types, len } if *len < INLINE_EXTENSIONS => {
                types[*len] = value;
                *len += 1;
            }
            Self::Inline { types, len } => {
                let mut heap = Vec::with_capacity(*len + 1);
                heap.extend_from_slice(&types[..*len]);
                heap.push(value);
                *self = Self::Heap(heap);
            }
            Self::Heap(heap) => heap.push(value),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u16] {
        match self {
            Self::Inline { types, len } => &mut types[..*len],
            Self::Heap(heap) => heap.as_mut_slice(),
        }
    }
}

fn parse_extensions(extensions: &[u8]) -> Result<Option<Hostname>, ParseError> {
    let mut reader = Reader::new(extensions);
    let mut seen_types = SeenTypes::new();
    let mut sni = None;
    while !reader.is_empty() {
        let extension_type = reader
            .u16()
            .ok_or(ParseError::Malformed("truncated extension type"))?;
        let data = reader
            .vec16()
            .ok_or(ParseError::Malformed("truncated extension data"))?;
        seen_types.push(extension_type);
        if extension_type == EXTENSION_SERVER_NAME {
            sni = parse_server_name(data)?;
        }
    }
    // Sort-then-scan keeps duplicate detection O(n log n) even for a
    // hostile hello packed with thousands of empty extensions.
    let seen_types = seen_types.as_mut_slice();
    seen_types.sort_unstable();
    if seen_types.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ParseError::Malformed("duplicate extension type"));
    }
    Ok(sni)
}

fn parse_server_name(data: &[u8]) -> Result<Option<Hostname>, ParseError> {
    let mut outer = Reader::new(data);
    let list = outer
        .vec16()
        .ok_or(ParseError::Malformed("truncated server_name_list"))?;
    if !outer.is_empty() {
        return Err(ParseError::Malformed(
            "trailing bytes after server_name_list",
        ));
    }
    if list.is_empty() {
        return Err(ParseError::Malformed("empty server_name_list"));
    }
    let mut reader = Reader::new(list);
    let mut host_name = None;
    while !reader.is_empty() {
        let name_type = reader
            .u8()
            .ok_or(ParseError::Malformed("truncated server name type"))?;
        let name = reader
            .vec16()
            .ok_or(ParseError::Malformed("truncated server name"))?;
        if name.is_empty() {
            return Err(ParseError::Malformed("empty server name"));
        }
        if name_type != NAME_TYPE_HOST_NAME {
            continue;
        }
        if host_name.is_some() {
            return Err(ParseError::Malformed("multiple host_name entries"));
        }
        let name = std::str::from_utf8(name)
            .map_err(|_| ParseError::InvalidServerName(NameError::InvalidCharacter))?;
        host_name = Some(Hostname::parse(name).map_err(ParseError::InvalidServerName)?);
    }
    Ok(host_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HelloLimits, parse_client_hello};
    use crate::testing::ClientHelloBuilder;

    fn parse(builder: ClientHelloBuilder) -> Result<Option<String>, ParseError> {
        parse_client_hello(&builder.build(), &HelloLimits::default())
            .map(|info| info.sni().map(|h| h.as_str().to_owned()))
    }

    #[test]
    fn rejects_trailing_dot_and_invalid_names() {
        assert_eq!(
            parse(ClientHelloBuilder::new().sni("example.com.")),
            Err(ParseError::InvalidServerName(NameError::EmptyLabel))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().sni("10.1.2.3")),
            Err(ParseError::InvalidServerName(NameError::IpLiteral))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().raw_server_names(&[(0, b"\xff\xfe.com")])),
            Err(ParseError::InvalidServerName(NameError::InvalidCharacter))
        );
    }

    #[test]
    fn rejects_multiple_host_names() {
        assert_eq!(
            parse(ClientHelloBuilder::new().raw_server_names(&[(0, b"a.test"), (0, b"b.test")])),
            Err(ParseError::Malformed("multiple host_name entries"))
        );
    }

    #[test]
    fn skips_non_host_name_entries() {
        assert_eq!(
            parse(ClientHelloBuilder::new().raw_server_names(&[(7, b"ignored"), (0, b"a.test")])),
            Ok(Some("a.test".to_owned()))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().raw_server_names(&[(7, b"ignored")])),
            Ok(None)
        );
    }

    #[test]
    fn rejects_empty_server_name_list_and_empty_names() {
        assert_eq!(
            parse(ClientHelloBuilder::new().raw_server_names(&[])),
            Err(ParseError::Malformed("empty server_name_list"))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().raw_server_names(&[(0, b"")])),
            Err(ParseError::Malformed("empty server name"))
        );
    }

    #[test]
    fn rejects_duplicate_extensions() {
        assert_eq!(
            parse(
                ClientHelloBuilder::new()
                    .sni("a.test")
                    .extension(0x000a, &[0, 2, 0, 0x1d])
                    .extension(0x000a, &[0, 2, 0, 0x17])
            ),
            Err(ParseError::Malformed("duplicate extension type"))
        );
    }

    #[test]
    fn more_extensions_than_inline_capacity_still_parse() {
        let mut builder = ClientHelloBuilder::new().sni("many.test");
        for extension_type in 100..140u16 {
            builder = builder.extension(extension_type, &[]);
        }
        assert_eq!(parse(builder), Ok(Some("many.test".to_owned())));
    }

    #[test]
    fn duplicate_beyond_inline_capacity_is_rejected() {
        let mut builder = ClientHelloBuilder::new().sni("many.test");
        for extension_type in 100..140u16 {
            builder = builder.extension(extension_type, &[]);
        }
        // The duplicate of the first (inline-stored) extension type arrives
        // after the inline/heap spill, so detection must be correct across
        // the two storage modes.
        builder = builder.extension(100, &[]);
        assert_eq!(
            parse(builder),
            Err(ParseError::Malformed("duplicate extension type"))
        );
    }

    #[test]
    fn rejects_structural_body_errors() {
        assert_eq!(
            parse(ClientHelloBuilder::new().session_id(&[0; 33])),
            Err(ParseError::Malformed(
                "legacy_session_id longer than 32 bytes"
            ))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().cipher_suites(&[0x13])),
            Err(ParseError::Malformed(
                "cipher_suites length is empty or odd"
            ))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().compression_methods(&[])),
            Err(ParseError::Malformed("empty legacy_compression_methods"))
        );
        assert_eq!(
            parse(ClientHelloBuilder::new().legacy_version(0x0200)),
            Err(ParseError::Malformed("unsupported legacy_version"))
        );
    }

    #[test]
    fn rejects_trailing_bytes_after_extensions() {
        assert_eq!(
            parse(
                ClientHelloBuilder::new()
                    .sni("a.test")
                    .trailing_body_bytes(&[0])
            ),
            Err(ParseError::Malformed("trailing bytes after extensions"))
        );
    }

    #[test]
    fn hello_without_extensions_block_has_no_sni() {
        assert_eq!(parse(ClientHelloBuilder::new().omit_extensions()), Ok(None));
    }
}
