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

//! Test helpers, available with the `test-util` feature: a ClientHello
//! builder, a scriptable [`RouteLookup`], and a recording [`MetricsSink`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::lookup::{
    InMemoryLookup, LookupError, LookupFuture, RouteCandidates, RouteKey, RouteLookup,
};
use crate::metrics::{
    CacheEvent, ConnectionOutcome, ListenerInfo, LookupOutcome, MetricEvent, MetricsSink,
};
use crate::protocol::{HANDSHAKE_HEADER_LEN, MAX_RECORD_PAYLOAD};

const EXTENSION_PADDING: u16 = 0x0015;

/// Builds syntactically valid (or deliberately broken) ClientHellos.
#[derive(Clone, Debug)]
pub struct ClientHelloBuilder {
    legacy_version: u16,
    session_id: Vec<u8>,
    cipher_suites: Vec<u8>,
    compression_methods: Vec<u8>,
    server_names: Option<Vec<(u8, Vec<u8>)>>,
    extensions: Vec<(u16, Vec<u8>)>,
    padded_body_len: Option<usize>,
    omit_extensions: bool,
    trailing_body_bytes: Vec<u8>,
}

impl Default for ClientHelloBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientHelloBuilder {
    pub fn new() -> Self {
        Self {
            legacy_version: 0x0303,
            session_id: vec![0x5a; 32],
            cipher_suites: vec![0x13, 0x01, 0x13, 0x02],
            compression_methods: vec![0],
            server_names: None,
            extensions: Vec::new(),
            padded_body_len: None,
            omit_extensions: false,
            trailing_body_bytes: Vec::new(),
        }
    }

    /// Adds a `server_name` extension with one host_name.
    pub fn sni(self, name: &str) -> Self {
        self.raw_server_names(&[(0, name.as_bytes())])
    }

    /// Adds a `server_name` extension with arbitrary (type, name) entries.
    pub fn raw_server_names(mut self, names: &[(u8, &[u8])]) -> Self {
        self.server_names = Some(
            names
                .iter()
                .map(|(kind, name)| (*kind, name.to_vec()))
                .collect(),
        );
        self
    }

    pub fn extension(mut self, extension_type: u16, data: &[u8]) -> Self {
        self.extensions.push((extension_type, data.to_vec()));
        self
    }

    pub fn legacy_version(mut self, version: u16) -> Self {
        self.legacy_version = version;
        self
    }

    pub fn session_id(mut self, session_id: &[u8]) -> Self {
        self.session_id = session_id.to_vec();
        self
    }

    pub fn cipher_suites(mut self, suites: &[u8]) -> Self {
        self.cipher_suites = suites.to_vec();
        self
    }

    pub fn compression_methods(mut self, methods: &[u8]) -> Self {
        self.compression_methods = methods.to_vec();
        self
    }

    /// Appends a padding extension so the ClientHello body is exactly
    /// `body_len` bytes.
    pub fn padding_to(mut self, body_len: usize) -> Self {
        self.padded_body_len = Some(body_len);
        self
    }

    pub fn omit_extensions(mut self) -> Self {
        self.omit_extensions = true;
        self
    }

    pub fn trailing_body_bytes(mut self, bytes: &[u8]) -> Self {
        self.trailing_body_bytes = bytes.to_vec();
        self
    }

    /// The ClientHello body (no handshake header).
    pub fn body(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&self.legacy_version.to_be_bytes());
        body.extend_from_slice(&[0x42; 32]);
        push_vec8(&mut body, &self.session_id);
        push_vec16(&mut body, &self.cipher_suites);
        push_vec8(&mut body, &self.compression_methods);
        if !self.omit_extensions {
            let mut extensions = self.extension_bytes();
            if let Some(target) = self.padded_body_len {
                let fixed = body.len() + 2 + extensions.len() + 4 + self.trailing_body_bytes.len();
                let pad = target
                    .checked_sub(fixed)
                    .expect("padding target smaller than the unpadded hello");
                extensions.extend_from_slice(&EXTENSION_PADDING.to_be_bytes());
                push_vec16(&mut extensions, &vec![0; pad]);
            }
            push_vec16(&mut body, &extensions);
        }
        body.extend_from_slice(&self.trailing_body_bytes);
        body
    }

    /// The handshake message: type, 24-bit length, body.
    pub fn handshake(&self) -> Vec<u8> {
        let body = self.body();
        let len = body.len();
        let mut handshake = Vec::with_capacity(HANDSHAKE_HEADER_LEN + len);
        handshake.extend_from_slice(&[0x01, (len >> 16) as u8, (len >> 8) as u8, len as u8]);
        handshake.extend_from_slice(&body);
        handshake
    }

    /// The handshake in maximum-size TLS records, as a real client sends it.
    pub fn build(&self) -> Vec<u8> {
        Self::records_of(&self.handshake(), MAX_RECORD_PAYLOAD)
    }

    /// Wraps `handshake` in handshake records of at most `chunk` bytes each.
    pub fn records_of(handshake: &[u8], chunk: usize) -> Vec<u8> {
        let mut wire = Vec::new();
        for fragment in handshake.chunks(chunk) {
            push_record(&mut wire, fragment);
        }
        wire
    }

    /// Wraps `handshake` in handshake records split at the given offsets.
    pub fn records_from_splits(handshake: &[u8], splits: &[usize]) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut start = 0;
        for &split in splits.iter().chain(std::iter::once(&handshake.len())) {
            push_record(&mut wire, &handshake[start..split]);
            start = split;
        }
        wire
    }

    fn extension_bytes(&self) -> Vec<u8> {
        let mut extensions = Vec::new();
        if let Some(names) = &self.server_names {
            let mut list = Vec::new();
            for (kind, name) in names {
                list.push(*kind);
                push_vec16(&mut list, name);
            }
            let mut data = Vec::new();
            push_vec16(&mut data, &list);
            extensions.extend_from_slice(&0u16.to_be_bytes());
            push_vec16(&mut extensions, &data);
        }
        for (extension_type, data) in &self.extensions {
            extensions.extend_from_slice(&extension_type.to_be_bytes());
            push_vec16(&mut extensions, data);
        }
        extensions
    }
}

fn push_record(wire: &mut Vec<u8>, fragment: &[u8]) {
    wire.extend_from_slice(&[0x16, 0x03, 0x01]);
    push_vec16(wire, fragment);
}

fn push_vec8(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(u8::try_from(bytes.len()).expect("vec8 too long"));
    out.extend_from_slice(bytes);
}

fn push_vec16(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(
        &u16::try_from(bytes.len())
            .expect("vec16 too long")
            .to_be_bytes(),
    );
    out.extend_from_slice(bytes);
}

/// What a [`ScriptedLookup`] does on each call.
#[derive(Clone, Debug)]
pub enum LookupBehavior {
    /// Answer from a route table.
    Routes(InMemoryLookup),
    /// Answer from a route table after a delay.
    Delayed(Duration, InMemoryLookup),
    /// Fail with this message.
    Fail(String),
    /// Never complete.
    Hang,
    /// Panic.
    Panic,
}

/// A [`RouteLookup`] whose behavior tests control and whose calls are
/// counted.
#[derive(Debug)]
pub struct ScriptedLookup {
    behavior: Mutex<LookupBehavior>,
    calls: AtomicUsize,
}

impl ScriptedLookup {
    pub fn new(behavior: LookupBehavior) -> Self {
        Self {
            behavior: Mutex::new(behavior),
            calls: AtomicUsize::new(0),
        }
    }

    /// Answers from `(route key, backend)` pairs.
    pub fn found(routes: &[(&str, &str)]) -> Self {
        Self::new(LookupBehavior::Routes(route_table(routes)))
    }

    pub fn failing(message: &str) -> Self {
        Self::new(LookupBehavior::Fail(message.to_owned()))
    }

    pub fn hanging() -> Self {
        Self::new(LookupBehavior::Hang)
    }

    pub fn set_behavior(&self, behavior: LookupBehavior) {
        *self.behavior.lock().unwrap_or_else(|e| e.into_inner()) = behavior;
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl RouteLookup for ScriptedLookup {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self
            .behavior
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Box::pin(async move {
            match behavior {
                LookupBehavior::Routes(table) => Ok(table.hits(candidates)),
                LookupBehavior::Delayed(delay, table) => {
                    tokio::time::sleep(delay).await;
                    Ok(table.hits(candidates))
                }
                LookupBehavior::Fail(message) => Err(LookupError::new(message)),
                LookupBehavior::Hang => std::future::pending().await,
                LookupBehavior::Panic => panic!("scripted lookup panic"),
            }
        })
    }
}

/// Builds an [`InMemoryLookup`] from `(route key, backend)` string pairs.
/// Panics on invalid input; for tests only.
pub fn route_table(routes: &[(&str, &str)]) -> InMemoryLookup {
    routes
        .iter()
        .map(|(key, backend)| {
            (
                RouteKey::parse(key).expect("valid route key"),
                backend.parse().expect("valid backend"),
            )
        })
        .collect()
}

/// An owned copy of a [`MetricEvent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedEvent {
    ConnectionOpened {
        listener: ListenerInfo,
    },
    ConnectionClosed {
        listener: ListenerInfo,
        outcome: ConnectionOutcome,
    },
    BytesRead {
        listener: ListenerInfo,
        bytes: u64,
    },
    BytesWritten {
        listener: ListenerInfo,
        bytes: u64,
    },
    RouteLookup {
        listener: ListenerInfo,
        outcome: LookupOutcome,
    },
    UpstreamConnect {
        listener: ListenerInfo,
        ok: bool,
    },
    Cache(CacheEvent),
}

/// A [`MetricsSink`] that keeps every event for assertions.
#[derive(Debug, Default)]
pub struct RecordingMetrics {
    events: Mutex<Vec<RecordedEvent>>,
}

impl RecordingMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<RecordedEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Outcomes of closed connections, in close order.
    pub fn outcomes(&self) -> Vec<ConnectionOutcome> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                RecordedEvent::ConnectionClosed { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect()
    }

    pub fn cache_events(&self) -> Vec<CacheEvent> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                RecordedEvent::Cache(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    pub fn count_cache(&self, wanted: CacheEvent) -> usize {
        self.cache_events()
            .into_iter()
            .filter(|e| *e == wanted)
            .count()
    }

    /// Total bytes read from and written to clients.
    pub fn client_bytes(&self) -> (u64, u64) {
        self.events()
            .into_iter()
            .fold((0, 0), |(read, written), event| match event {
                RecordedEvent::BytesRead { bytes, .. } => (read + bytes, written),
                RecordedEvent::BytesWritten { bytes, .. } => (read, written + bytes),
                _ => (read, written),
            })
    }
}

impl MetricsSink for RecordingMetrics {
    fn record(&self, event: MetricEvent<'_>) {
        let recorded = match event {
            MetricEvent::ConnectionOpened { listener, .. } => RecordedEvent::ConnectionOpened {
                listener: listener.clone(),
            },
            MetricEvent::ConnectionClosed {
                listener, outcome, ..
            } => RecordedEvent::ConnectionClosed {
                listener: listener.clone(),
                outcome,
            },
            MetricEvent::BytesRead {
                listener, bytes, ..
            } => RecordedEvent::BytesRead {
                listener: listener.clone(),
                bytes,
            },
            MetricEvent::BytesWritten {
                listener, bytes, ..
            } => RecordedEvent::BytesWritten {
                listener: listener.clone(),
                bytes,
            },
            MetricEvent::RouteLookup {
                listener, outcome, ..
            } => RecordedEvent::RouteLookup {
                listener: listener.clone(),
                outcome,
            },
            MetricEvent::UpstreamConnect { listener, ok, .. } => RecordedEvent::UpstreamConnect {
                listener: listener.clone(),
                ok,
            },
            MetricEvent::Cache(event) => RecordedEvent::Cache(event),
        };
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(recorded);
    }
}
