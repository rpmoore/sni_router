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

//! Tests against ClientHellos captured from real clients with
//! `scripts/capture_client_hello.py`.

use super::{HelloLimits, ParseError, RECORD_HEADER_LEN, parse_client_hello};
use crate::testing::ClientHelloBuilder;

struct Fixture {
    name: &'static str,
    wire: &'static [u8],
    sni: &'static str,
}

/// OpenSSL 3.0 `s_client`, curl 8.5 (OpenSSL 3.0), and Firefox with an
/// X25519MLKEM768 post-quantum key share (~1.9 KB, the largest mainstream
/// ClientHello shape today).
const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "openssl_s_client",
        wire: include_bytes!("testdata/openssl_s_client.bin"),
        sni: "openssl.example.com",
    },
    Fixture {
        name: "curl",
        wire: include_bytes!("testdata/curl.bin"),
        sni: "curl.example.com",
    },
    Fixture {
        name: "firefox_pq",
        wire: include_bytes!("testdata/firefox.bin"),
        sni: "firefox.localhost",
    },
];

fn handshake_of(wire: &[u8]) -> &[u8] {
    &wire[RECORD_HEADER_LEN..]
}

#[test]
fn real_clients_parse_under_default_limits() {
    for fixture in FIXTURES {
        let info = parse_client_hello(fixture.wire, &HelloLimits::default())
            .unwrap_or_else(|e| panic!("{}: {e}", fixture.name));
        assert_eq!(
            info.sni().unwrap().as_str(),
            fixture.sni,
            "{}",
            fixture.name
        );
        assert_eq!(info.consumed(), fixture.wire.len(), "{}", fixture.name);
    }
}

#[test]
fn post_quantum_hello_is_the_large_case() {
    let firefox = FIXTURES.iter().find(|f| f.name == "firefox_pq").unwrap();
    assert!(firefox.wire.len() > 1_500, "fixture lost its PQ key share");
}

#[test]
fn every_prefix_of_real_hellos_is_incomplete() {
    for fixture in FIXTURES {
        for end in 0..fixture.wire.len() {
            assert_eq!(
                parse_client_hello(&fixture.wire[..end], &HelloLimits::default()),
                Err(ParseError::Incomplete),
                "{} prefix {end}",
                fixture.name
            );
        }
    }
}

#[test]
fn every_two_record_split_of_real_hellos_parses() {
    for fixture in FIXTURES {
        let handshake = handshake_of(fixture.wire);
        for split in 1..handshake.len() {
            let wire = ClientHelloBuilder::records_from_splits(handshake, &[split]);
            let info = parse_client_hello(&wire, &HelloLimits::default())
                .unwrap_or_else(|e| panic!("{} split {split}: {e}", fixture.name));
            assert_eq!(info.sni().unwrap().as_str(), fixture.sni);
        }
    }
}

#[test]
fn real_hellos_fragmented_to_the_record_limit_parse() {
    for fixture in FIXTURES {
        let handshake = handshake_of(fixture.wire);
        let chunk = handshake.len().div_ceil(HelloLimits::DEFAULT_MAX_RECORDS);
        let wire = ClientHelloBuilder::records_of(handshake, chunk);
        assert!(
            parse_client_hello(&wire, &HelloLimits::default()).is_ok(),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn single_byte_corruption_of_real_hellos_never_panics() {
    for fixture in FIXTURES {
        for index in 0..fixture.wire.len() {
            for value in [0x00, 0x01, 0x80, 0xff] {
                let mut mutated = fixture.wire.to_vec();
                mutated[index] = value;
                let _ = parse_client_hello(&mutated, &HelloLimits::default());
            }
        }
    }
}
