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

//! Maps router [`MetricEvent`]s onto Prometheus series via OpenTelemetry.
//!
//! Per-listener series carry `{network, address}` labels. Embedders with
//! different naming needs map the same events in their own sink.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider, UpDownCounter};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::Registry;
use sni_router::metrics::{ListenerInfo, MetricEvent, MetricsSink};

/// Connection lifetime buckets, in seconds: short handshakes through
/// multi-minute sessions.
const CONNECTION_DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];
/// Lookup and upstream-connect latency buckets, in seconds.
const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

/// A [`MetricsSink`] exporting to a Prometheus [`Registry`].
pub struct OtelMetrics {
    _provider: SdkMeterProvider,
    registry: Registry,
    listener_labels: RwLock<HashMap<SocketAddr, Arc<[KeyValue]>>>,
    connections_opened: Counter<u64>,
    connections_closed: Counter<u64>,
    open_connections: UpDownCounter<i64>,
    bytes_read: Counter<u64>,
    bytes_written: Counter<u64>,
    connection_duration: Histogram<f64>,
    route_lookup_duration: Histogram<f64>,
    upstream_connect_duration: Histogram<f64>,
    cache_events: Counter<u64>,
}

impl OtelMetrics {
    pub fn new() -> Result<Self, String> {
        let registry = Registry::new();
        // Instrument names below are final Prometheus names (with `_total`
        // and unit suffixes spelled out), so disable the exporter's suffixing.
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .without_counter_suffixes()
            .without_units()
            .build()
            .map_err(|error| format!("failed to build Prometheus exporter: {error}"))?;
        let provider = SdkMeterProvider::builder().with_reader(exporter).build();
        let meter = provider.meter("sni_router");
        Ok(Self {
            registry,
            listener_labels: RwLock::new(HashMap::new()),
            connections_opened: meter
                .u64_counter("snirouter_connections_opened_total")
                .with_description("Connections accepted")
                .build(),
            connections_closed: meter
                .u64_counter("snirouter_connections_closed_total")
                .with_description("Connections closed, by outcome")
                .build(),
            open_connections: meter
                .i64_up_down_counter("snirouter_open_connections")
                .with_description("Connections currently open")
                .build(),
            bytes_read: meter
                .u64_counter("snirouter_bytes_read_total")
                .with_description("Bytes read from clients")
                .build(),
            bytes_written: meter
                .u64_counter("snirouter_bytes_written_total")
                .with_description("Bytes written to clients")
                .build(),
            connection_duration: meter
                .f64_histogram("snirouter_connection_duration_seconds")
                .with_description("Connection lifetime, recorded at close")
                .with_boundaries(CONNECTION_DURATION_BUCKETS.to_vec())
                .build(),
            route_lookup_duration: meter
                .f64_histogram("snirouter_route_lookup_duration_seconds")
                .with_description("Route lookup latency, by outcome")
                .with_boundaries(LATENCY_BUCKETS.to_vec())
                .build(),
            upstream_connect_duration: meter
                .f64_histogram("snirouter_upstream_connect_duration_seconds")
                .with_description("Backend connect plus ClientHello replay latency, by result")
                .with_boundaries(LATENCY_BUCKETS.to_vec())
                .build(),
            cache_events: meter
                .u64_counter("snirouter_cache_events_total")
                .with_description("Route cache events")
                .build(),
            _provider: provider,
        })
    }

    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    /// `{network, address}` for a listener, built once per listener so
    /// per-read byte events don't allocate label strings.
    fn labels(&self, listener: &ListenerInfo) -> Arc<[KeyValue]> {
        let address = listener.address();
        if let Some(labels) = self
            .listener_labels
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address)
        {
            return Arc::clone(labels);
        }
        let labels: Arc<[KeyValue]> = Arc::from(vec![
            KeyValue::new("network", listener.network()),
            KeyValue::new("address", address.to_string()),
        ]);
        self.listener_labels
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(address)
            .or_insert(labels)
            .clone()
    }

    fn labels_with(
        &self,
        listener: &ListenerInfo,
        key: &'static str,
        value: &'static str,
    ) -> Vec<KeyValue> {
        let mut labels = self.labels(listener).to_vec();
        labels.push(KeyValue::new(key, value));
        labels
    }
}

impl MetricsSink for OtelMetrics {
    fn record(&self, event: MetricEvent<'_>) {
        match event {
            MetricEvent::ConnectionOpened { listener, .. } => {
                let labels = self.labels(listener);
                self.connections_opened.add(1, &labels);
                self.open_connections.add(1, &labels);
            }
            MetricEvent::ConnectionClosed {
                listener,
                outcome,
                lifetime,
                ..
            } => {
                let labels = self.labels(listener);
                self.open_connections.add(-1, &labels);
                self.connection_duration
                    .record(lifetime.as_secs_f64(), &labels);
                self.connections_closed
                    .add(1, &self.labels_with(listener, "outcome", outcome.as_str()));
            }
            MetricEvent::BytesRead {
                listener, bytes, ..
            } => self.bytes_read.add(bytes, &self.labels(listener)),
            MetricEvent::BytesWritten {
                listener, bytes, ..
            } => self.bytes_written.add(bytes, &self.labels(listener)),
            MetricEvent::RouteLookup {
                listener,
                outcome,
                elapsed,
                ..
            } => self.route_lookup_duration.record(
                elapsed.as_secs_f64(),
                &self.labels_with(listener, "outcome", outcome.as_str()),
            ),
            MetricEvent::UpstreamConnect {
                listener,
                ok,
                elapsed,
                ..
            } => self.upstream_connect_duration.record(
                elapsed.as_secs_f64(),
                &self.labels_with(listener, "result", if ok { "ok" } else { "error" }),
            ),
            MetricEvent::Cache(event) => self
                .cache_events
                .add(1, &[KeyValue::new("event", event.as_str())]),
            _ => {}
        }
    }
}
