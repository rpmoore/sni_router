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

//! TLS record-layer walking and handshake reassembly.

use super::hello::parse_client_hello_body;
use super::{
    ClientHelloInfo, HANDSHAKE_HEADER_LEN, HelloLimits, MAX_RECORD_PAYLOAD, ParseError,
    RECORD_HEADER_LEN,
};

const CONTENT_TYPE_HANDSHAKE: u8 = 0x16;
const HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 0x01;
const RECORD_VERSION_MAJOR: u8 = 0x03;

/// Parses a ClientHello from the start of a connection's byte stream.
///
/// `buf` is everything read from the client so far. Returns
/// [`ParseError::Incomplete`] until the whole ClientHello has arrived, which
/// may span several TLS records. Limits are enforced as early as the bytes
/// allow: an oversized declared handshake length is rejected as soon as the
/// four-byte handshake header is visible, before the body is buffered.
pub fn parse_client_hello(buf: &[u8], limits: &HelloLimits) -> Result<ClientHelloInfo, ParseError> {
    let layout = scan_records(buf, limits)?;
    let message_len = HANDSHAKE_HEADER_LEN + layout.handshake_len;
    let sni = match layout.payloads.as_slice() {
        // Common case: the whole ClientHello is in one record; no copy.
        [only] => parse_client_hello_body(
            &buf[only.start + HANDSHAKE_HEADER_LEN..only.start + message_len],
        )?,
        fragments => {
            let mut message = Vec::with_capacity(message_len);
            for fragment in fragments {
                let take = (message_len - message.len()).min(fragment.len());
                message.extend_from_slice(&buf[fragment.start..fragment.start + take]);
            }
            parse_client_hello_body(&message[HANDSHAKE_HEADER_LEN..])?
        }
    };
    Ok(ClientHelloInfo {
        sni,
        consumed: layout.consumed,
    })
}

/// Where a complete ClientHello's handshake bytes sit in the wire buffer.
struct RecordLayout {
    payloads: Payloads,
    handshake_len: usize,
    consumed: usize,
}

/// A well-formed ClientHello almost always arrives in 1-2 TLS records; only
/// a hostile sender fragments it further (bounded by `HelloLimits::max_records`,
/// default 32). Payload ranges live inline on the stack until more than
/// `INLINE_RECORDS` records are seen, so `scan_records` — rerun from byte 0
/// on every read-loop iteration while a hello is still incomplete — doesn't
/// pay a heap allocation per call in the common case.
const INLINE_RECORDS: usize = 4;

enum Payloads {
    Inline {
        ranges: [std::ops::Range<usize>; INLINE_RECORDS],
        len: usize,
    },
    Heap(Vec<std::ops::Range<usize>>),
}

impl Payloads {
    fn new() -> Self {
        Self::Inline {
            ranges: std::array::from_fn(|_| 0..0),
            len: 0,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => *len,
            Self::Heap(heap) => heap.len(),
        }
    }

    fn push(&mut self, range: std::ops::Range<usize>) {
        match self {
            Self::Inline { ranges, len } if *len < INLINE_RECORDS => {
                ranges[*len] = range;
                *len += 1;
            }
            Self::Inline { ranges, len } => {
                let mut heap = Vec::with_capacity(*len + 1);
                heap.extend(ranges[..*len].iter().cloned());
                heap.push(range);
                *self = Self::Heap(heap);
            }
            Self::Heap(heap) => heap.push(range),
        }
    }

    fn as_slice(&self) -> &[std::ops::Range<usize>] {
        match self {
            Self::Inline { ranges, len } => &ranges[..*len],
            Self::Heap(heap) => heap.as_slice(),
        }
    }
}

/// Walks record headers without copying payloads, so re-checking a growing
/// buffer after every read costs O(records), not O(bytes) — a client
/// trickling one byte per segment can't turn the read loop quadratic.
fn scan_records(buf: &[u8], limits: &HelloLimits) -> Result<RecordLayout, ParseError> {
    let mut offset = 0;
    let mut payloads = Payloads::new();
    let mut header = HandshakeHeader::default();
    let mut payload_total = 0;

    loop {
        let rest = &buf[offset..];
        let Some(record_header) = rest.get(..RECORD_HEADER_LEN) else {
            check_partial_record_header(rest, offset == 0)?;
            return Err(ParseError::Incomplete);
        };
        let payload_len = check_record_header(record_header, offset == 0)?;
        if payloads.len() == limits.max_records {
            return Err(ParseError::TooLarge);
        }

        let payload_start = offset + RECORD_HEADER_LEN;
        let payload_end = payload_start + payload_len;
        let available = &buf[payload_start..payload_end.min(buf.len())];
        let handshake_len = header.observe(available, limits)?;
        if available.len() < payload_len {
            return Err(ParseError::Incomplete);
        }
        payloads.push(payload_start..payload_end);
        payload_total += payload_len;
        offset = payload_end;

        if let Some(handshake_len) = handshake_len
            && payload_total >= HANDSHAKE_HEADER_LEN + handshake_len
        {
            return Ok(RecordLayout {
                payloads,
                handshake_len,
                consumed: offset,
            });
        }
    }
}

/// Validates a full record header and returns its payload length.
fn check_record_header(header: &[u8], first_record: bool) -> Result<usize, ParseError> {
    check_partial_record_header(header, first_record)?;
    let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
    if len == 0 || len > MAX_RECORD_PAYLOAD {
        return Err(ParseError::InvalidRecord);
    }
    Ok(len)
}

/// Validates whatever prefix of a record header has arrived, so garbage is
/// rejected on the first byte instead of after waiting for five.
fn check_partial_record_header(header: &[u8], first_record: bool) -> Result<(), ParseError> {
    if let Some(&content_type) = header.first()
        && content_type != CONTENT_TYPE_HANDSHAKE
    {
        return Err(if first_record {
            ParseError::NotHandshake
        } else {
            ParseError::InvalidRecord
        });
    }
    if let Some(&major) = header.get(1)
        && major != RECORD_VERSION_MAJOR
    {
        return Err(ParseError::InvalidRecord);
    }
    Ok(())
}

/// Accumulates the 4-byte handshake message header, which may itself be
/// split across records, and checks it as soon as each byte is visible.
#[derive(Default)]
struct HandshakeHeader {
    bytes: [u8; HANDSHAKE_HEADER_LEN],
    known: usize,
}

impl HandshakeHeader {
    /// Feeds the next record's (possibly partial) payload. Returns the
    /// declared body length once all four header bytes are known.
    fn observe(
        &mut self,
        payload: &[u8],
        limits: &HelloLimits,
    ) -> Result<Option<usize>, ParseError> {
        for &byte in payload.iter().take(HANDSHAKE_HEADER_LEN - self.known) {
            self.bytes[self.known] = byte;
            self.known += 1;
        }
        if self.known >= 1 && self.bytes[0] != HANDSHAKE_TYPE_CLIENT_HELLO {
            return Err(ParseError::NotClientHello);
        }
        if self.known < HANDSHAKE_HEADER_LEN {
            return Ok(None);
        }
        let [_, high, mid, low] = self.bytes;
        let len = usize::from(high) << 16 | usize::from(mid) << 8 | usize::from(low);
        if len > limits.max_handshake_bytes {
            return Err(ParseError::TooLarge);
        }
        Ok(Some(len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ClientHelloBuilder;

    fn limits() -> HelloLimits {
        HelloLimits::default()
    }

    fn sni(buf: &[u8]) -> Result<Option<String>, ParseError> {
        parse_client_hello(buf, &limits()).map(|info| info.sni().map(|h| h.as_str().to_owned()))
    }

    #[test]
    fn parses_single_record_hello() {
        let wire = ClientHelloBuilder::new().sni("Example.COM").build();
        let info = parse_client_hello(&wire, &limits()).unwrap();
        assert_eq!(info.sni().unwrap().as_str(), "example.com");
        assert_eq!(info.consumed(), wire.len());
    }

    #[test]
    fn hello_without_sni_parses_with_none() {
        let wire = ClientHelloBuilder::new().build();
        assert_eq!(sni(&wire), Ok(None));
    }

    #[test]
    fn consumed_excludes_trailing_bytes() {
        let mut wire = ClientHelloBuilder::new().sni("a.test").build();
        let hello_len = wire.len();
        wire.extend_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x01, 0xff]);
        let info = parse_client_hello(&wire, &limits()).unwrap();
        assert_eq!(info.consumed(), hello_len);
    }

    #[test]
    fn every_prefix_is_incomplete() {
        let wire = ClientHelloBuilder::new().sni("prefix.example.com").build();
        for end in 0..wire.len() {
            assert_eq!(
                parse_client_hello(&wire[..end], &limits()),
                Err(ParseError::Incomplete),
                "prefix of length {end}"
            );
        }
    }

    #[test]
    fn every_two_record_split_parses_the_same() {
        let builder = ClientHelloBuilder::new().sni("split.example.com");
        let handshake = builder.handshake();
        for split in 1..handshake.len() {
            let wire = ClientHelloBuilder::records_from_splits(&handshake, &[split]);
            assert_eq!(
                sni(&wire),
                Ok(Some("split.example.com".to_owned())),
                "split at {split}"
            );
        }
    }

    #[test]
    fn one_byte_records_parse_up_to_the_record_limit() {
        let builder = ClientHelloBuilder::new().sni("a.b");
        let handshake = builder.handshake();
        let wire = ClientHelloBuilder::records_of(&handshake, 1);
        let generous = HelloLimits::new(HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES, handshake.len());
        let info = parse_client_hello(&wire, &generous).unwrap();
        assert_eq!(info.sni().unwrap().as_str(), "a.b");

        let tight = HelloLimits::new(
            HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES,
            handshake.len() - 1,
        );
        assert_eq!(parse_client_hello(&wire, &tight), Err(ParseError::TooLarge));
    }

    #[test]
    fn record_limit_is_inclusive() {
        let handshake = ClientHelloBuilder::new().sni("r.test").handshake();
        // N-1 one-byte records followed by the remainder: exactly N records.
        let splits = |records: usize| (1..records).collect::<Vec<_>>();
        let wire = ClientHelloBuilder::records_from_splits(&handshake, &splits(32));
        assert!(parse_client_hello(&wire, &limits()).is_ok());

        let wire = ClientHelloBuilder::records_from_splits(&handshake, &splits(33));
        assert_eq!(
            parse_client_hello(&wire, &limits()),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn oversized_declared_length_is_rejected_from_the_header_alone() {
        // Record header + handshake header declaring a body over the limit,
        // with none of the body present.
        let declared = HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES + 1;
        let wire = [
            0x16,
            0x03,
            0x01,
            0x40,
            0x00,
            0x01,
            (declared >> 16) as u8,
            (declared >> 8) as u8,
            declared as u8,
        ];
        assert_eq!(
            parse_client_hello(&wire, &limits()),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn large_hello_at_the_limit_parses() {
        let builder = ClientHelloBuilder::new()
            .sni("big.example.com")
            .padding_to(HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES);
        let handshake = builder.handshake();
        assert_eq!(
            handshake.len(),
            HANDSHAKE_HEADER_LEN + HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES
        );
        let wire = builder.build();
        assert_eq!(sni(&wire), Ok(Some("big.example.com".to_owned())));
    }

    #[test]
    fn rejects_non_handshake_first_byte_immediately() {
        assert_eq!(sni(b"G"), Err(ParseError::NotHandshake));
        assert_eq!(sni(b"GET / HTTP/1.1\r\n"), Err(ParseError::NotHandshake));
    }

    #[test]
    fn rejects_bad_record_headers() {
        assert_eq!(sni(&[0x16, 0x02]), Err(ParseError::InvalidRecord));
        assert_eq!(
            sni(&[0x16, 0x03, 0x01, 0x00, 0x00]),
            Err(ParseError::InvalidRecord)
        );
        assert_eq!(
            sni(&[0x16, 0x03, 0x01, 0x40, 0x01]),
            Err(ParseError::InvalidRecord)
        );
    }

    #[test]
    fn rejects_non_client_hello_on_first_byte() {
        assert_eq!(
            sni(&[0x16, 0x03, 0x01, 0x00, 0x10, 0x02]),
            Err(ParseError::NotClientHello)
        );
    }

    #[test]
    fn rejects_non_handshake_record_mid_hello() {
        let handshake = ClientHelloBuilder::new().sni("a.test").handshake();
        let mut wire = ClientHelloBuilder::records_from_splits(&handshake, &[10]);
        // Turn the second record into application data.
        let second = RECORD_HEADER_LEN + 10;
        wire[second] = 0x17;
        assert_eq!(sni(&wire), Err(ParseError::InvalidRecord));
    }

    #[test]
    fn corrupting_length_bytes_never_panics() {
        let wire = ClientHelloBuilder::new().sni("fuzz.example.com").build();
        for index in 0..wire.len() {
            for value in [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff] {
                let mut mutated = wire.clone();
                mutated[index] = value;
                let _ = parse_client_hello(&mutated, &limits());
            }
        }
    }
}
