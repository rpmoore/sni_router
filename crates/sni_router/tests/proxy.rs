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

//! End-to-end router behavior over real loopback sockets.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sni_router::metrics::ConnectionOutcome;
use sni_router::protocol::{HelloLimits, UNRECOGNIZED_NAME_ALERT};
use sni_router::testing::{
    ClientHelloBuilder, LookupBehavior, RecordedEvent, ScriptedLookup, route_table,
};
use support::{Backend, BackendMode, Harness, eventually, fast_config, read_exact, read_to_close};
use tokio::io::AsyncWriteExt;

fn lookup(routes: &[(&str, &str)]) -> Arc<ScriptedLookup> {
    Arc::new(ScriptedLookup::found(routes))
}

fn hello(sni: &str) -> Vec<u8> {
    ClientHelloBuilder::new().sni(sni).build()
}

#[tokio::test]
async fn routes_by_sni_and_replays_bytes_exactly() {
    let a = Backend::start(BackendMode::Echo).await;
    let b = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start(
        lookup(&[("a.test", &a.route()), ("b.test", &b.route())]),
        fast_config(),
    )
    .await;

    // The hello plus bytes sent in the same write (e.g. early data) must
    // reach the backend unchanged.
    let mut wire = hello("A.Test");
    wire.extend_from_slice(b"early-data");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    assert_eq!(read_exact(&mut client, wire.len()).await, wire);

    client.write_all(b"after").await.unwrap();
    assert_eq!(read_exact(&mut client, 5).await, b"after");
    drop(client);

    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::Proxied]);
    let mut expected = wire.clone();
    expected.extend_from_slice(b"after");
    assert_eq!(a.received(0), expected);
    assert_eq!(b.connections(), 0);
    harness.stop().await;
}

#[tokio::test]
async fn half_close_reaches_backend_and_reply_still_flows() {
    let backend = Backend::start(BackendMode::ReplyAfterEof).await;
    let harness = Harness::start(lookup(&[("half.test", &backend.route())]), fast_config()).await;

    let wire = hello("half.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    client.shutdown().await.unwrap();

    let reply = read_to_close(&mut client).await;
    assert_eq!(reply, format!("received:{}", wire.len()).as_bytes());
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::Proxied]);
    harness.stop().await;
}

#[tokio::test]
async fn fragmented_hello_is_routed_and_replayed_as_sent() {
    let backend = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start(lookup(&[("frag.test", &backend.route())]), fast_config()).await;

    let handshake = ClientHelloBuilder::new().sni("frag.test").handshake();
    let wire = ClientHelloBuilder::records_from_splits(&handshake, &[3, 40, 41]);
    let mut client = harness.connect().await;
    for piece in wire.chunks(7) {
        client.write_all(piece).await.unwrap();
        client.flush().await.unwrap();
    }
    assert_eq!(read_exact(&mut client, wire.len()).await, wire);
    drop(client);
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::Proxied]);
    harness.stop().await;
}

#[tokio::test]
async fn wildcard_routes_when_no_exact_match() {
    let exact = Backend::start(BackendMode::Echo).await;
    let wild = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start(
        lookup(&[
            ("api.example.com", &exact.route()),
            ("*.example.com", &wild.route()),
        ]),
        fast_config(),
    )
    .await;

    for sni in ["api.example.com", "web.example.com"] {
        let wire = hello(sni);
        let mut client = harness.connect().await;
        client.write_all(&wire).await.unwrap();
        read_exact(&mut client, wire.len()).await;
    }
    harness.outcomes(2).await;
    assert_eq!(exact.connections(), 1);
    assert_eq!(wild.connections(), 1);
    harness.stop().await;
}

#[tokio::test]
async fn unknown_sni_gets_unrecognized_name_alert() {
    let harness = Harness::start(lookup(&[]), fast_config()).await;
    let mut client = harness.connect().await;
    client.write_all(&hello("nope.test")).await.unwrap();
    assert_eq!(read_to_close(&mut client).await, UNRECOGNIZED_NAME_ALERT);
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::UnknownHost]);
    harness.stop().await;
}

#[tokio::test]
async fn missing_sni_gets_unrecognized_name_alert() {
    let harness = Harness::start(lookup(&[]), fast_config()).await;
    let mut client = harness.connect().await;
    client
        .write_all(&ClientHelloBuilder::new().build())
        .await
        .unwrap();
    assert_eq!(read_to_close(&mut client).await, UNRECOGNIZED_NAME_ALERT);
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::MissingSni]);
    harness.stop().await;
}

#[tokio::test]
async fn garbage_is_closed_without_a_reply() {
    let harness = Harness::start(lookup(&[]), fast_config()).await;
    let mut client = harness.connect().await;
    client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    assert!(read_to_close(&mut client).await.is_empty());
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::InvalidHello]);
    harness.stop().await;
}

#[tokio::test]
async fn hello_limits_are_enforced_end_to_end() {
    let backend = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start(lookup(&[("r.test", &backend.route())]), fast_config()).await;

    // 33 records: over the record limit.
    let handshake = ClientHelloBuilder::new().sni("r.test").handshake();
    let splits: Vec<usize> = (1..33).collect();
    let mut client = harness.connect().await;
    client
        .write_all(&ClientHelloBuilder::records_from_splits(
            &handshake, &splits,
        ))
        .await
        .unwrap();
    read_to_close(&mut client).await;

    // A declared handshake length over the limit, with no body sent.
    let declared = HelloLimits::DEFAULT_MAX_HANDSHAKE_BYTES + 1;
    let mut client = harness.connect().await;
    client
        .write_all(&[
            0x16,
            0x03,
            0x01,
            0x40,
            0x00,
            0x01,
            (declared >> 16) as u8,
            (declared >> 8) as u8,
            declared as u8,
        ])
        .await
        .unwrap();
    read_to_close(&mut client).await;

    assert_eq!(
        harness.outcomes(2).await,
        [
            ConnectionOutcome::InvalidHello,
            ConnectionOutcome::InvalidHello
        ]
    );
    assert_eq!(backend.connections(), 0);
    harness.stop().await;
}

#[tokio::test]
async fn post_quantum_sized_hello_routes_under_defaults() {
    let backend = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start(
        lookup(&[("firefox.localhost", &backend.route())]),
        support::fast_config(),
    )
    .await;
    let wire = include_bytes!("../src/protocol/testdata/firefox.bin");
    let mut client = harness.connect().await;
    client.write_all(wire).await.unwrap();
    assert_eq!(read_exact(&mut client, wire.len()).await, wire);
    drop(client);
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::Proxied]);
    harness.stop().await;
}

#[tokio::test]
async fn slow_hello_times_out() {
    let mut config = fast_config();
    config.client_hello_timeout = Duration::from_millis(200);
    let harness = Harness::start(lookup(&[]), config).await;
    let mut client = harness.connect().await;
    // A valid start, then silence (slowloris).
    client.write_all(&hello("slow.test")[..3]).await.unwrap();
    read_to_close(&mut client).await;
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::HelloTimeout]);
    harness.stop().await;
}

#[tokio::test]
async fn lookup_failure_and_timeout_fail_closed() {
    let mut config = fast_config();
    config.lookup_timeout = Duration::from_millis(200);
    let scripted = Arc::new(ScriptedLookup::failing("db down"));
    let harness = Harness::start(scripted.clone(), config).await;

    let mut client = harness.connect().await;
    client.write_all(&hello("a.test")).await.unwrap();
    assert!(read_to_close(&mut client).await.is_empty());
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::LookupFailed]);

    scripted.set_behavior(LookupBehavior::Hang);
    let mut client = harness.connect().await;
    client.write_all(&hello("a.test")).await.unwrap();
    assert!(read_to_close(&mut client).await.is_empty());
    assert_eq!(
        harness.outcomes(2).await[1],
        ConnectionOutcome::LookupTimeout
    );
    harness.stop().await;
}

#[tokio::test]
async fn refused_upstream_fails_the_connection() {
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let harness = Harness::start(
        lookup(&[("gone.test", &closed_port.to_string())]),
        fast_config(),
    )
    .await;
    let mut client = harness.connect().await;
    client.write_all(&hello("gone.test")).await.unwrap();
    assert!(read_to_close(&mut client).await.is_empty());
    assert_eq!(
        harness.outcomes(1).await,
        [ConnectionOutcome::UpstreamConnectFailed]
    );
    harness.stop().await;
}

#[tokio::test]
async fn stalled_upstream_hits_the_upstream_timeout_and_frees_its_slot() {
    // Connect and replay share one deadline; a backend whose accept queue is
    // full stalls the connect stage past it.
    let backend = Backend::start(BackendMode::Stalled).await;
    let mut config = fast_config();
    config.upstream_timeout = Duration::from_millis(300);
    config.max_connections = 1;
    let harness = Harness::start(lookup(&[("stall.test", &backend.route())]), config).await;

    let started = tokio::time::Instant::now();
    let mut client = harness.connect().await;
    client.write_all(&hello("stall.test")).await.unwrap();
    read_to_close(&mut client).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        harness.outcomes(1).await,
        [ConnectionOutcome::UpstreamTimeout]
    );

    // The slot was released: with max_connections = 1 a second connection
    // is still served.
    let mut client = harness.connect().await;
    client.write_all(b"x").await.unwrap();
    read_to_close(&mut client).await;
    assert_eq!(
        harness.outcomes(2).await[1],
        ConnectionOutcome::InvalidHello
    );
    harness.stop().await;
}

#[tokio::test]
async fn idle_proxied_connection_is_closed() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.idle_timeout = Some(Duration::from_millis(200));
    let harness = Harness::start(lookup(&[("idle.test", &backend.route())]), config).await;

    let wire = hello("idle.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_exact(&mut client, wire.len()).await;
    read_to_close(&mut client).await;
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::IdleTimeout]);
    harness.stop().await;
}

#[tokio::test]
async fn max_connections_queues_new_connections_until_a_slot_frees() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.max_connections = 1;
    let harness = Harness::start(lookup(&[("slot.test", &backend.route())]), config).await;

    let wire = hello("slot.test");
    let mut first = harness.connect().await;
    first.write_all(&wire).await.unwrap();
    read_exact(&mut first, wire.len()).await;

    // Connects (kernel backlog) but isn't served while `first` holds the slot.
    let mut second = harness.connect().await;
    second.write_all(&wire).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(backend.connections(), 1);

    drop(first);
    assert_eq!(read_exact(&mut second, wire.len()).await, wire);
    assert_eq!(backend.connections(), 2);
    drop(second);
    harness.outcomes(2).await;
    harness.stop().await;
}

#[tokio::test]
async fn connection_counts_client_bytes() {
    let backend = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start(lookup(&[("bytes.test", &backend.route())]), fast_config()).await;
    let wire = hello("bytes.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_exact(&mut client, wire.len()).await;
    client.write_all(b"12345").await.unwrap();
    read_exact(&mut client, 5).await;
    drop(client);
    harness.outcomes(1).await;
    let (read, written) = harness.metrics.client_bytes();
    assert_eq!(read, wire.len() as u64 + 5);
    assert_eq!(written, wire.len() as u64 + 5);
    harness.stop().await;
}

#[tokio::test]
async fn failed_handshakes_still_count_their_bytes() {
    let harness = Harness::start(lookup(&[]), fast_config()).await;

    // Garbage: every byte read is counted even though no hello completed.
    let garbage = b"GET / HTTP/1.1\r\n\r\n";
    let mut client = harness.connect().await;
    client.write_all(garbage).await.unwrap();
    read_to_close(&mut client).await;
    harness.outcomes(1).await;
    assert_eq!(harness.metrics.client_bytes(), (garbage.len() as u64, 0));

    // Unknown SNI: the hello is counted as read and the alert as written.
    let wire = hello("nope.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_to_close(&mut client).await;
    harness.outcomes(2).await;
    assert_eq!(
        harness.metrics.client_bytes(),
        (
            (garbage.len() + wire.len()) as u64,
            UNRECOGNIZED_NAME_ALERT.len() as u64
        )
    );
    harness.stop().await;
}

#[tokio::test]
async fn panicking_connection_task_still_closes_its_books() {
    let harness = Harness::start(
        Arc::new(ScriptedLookup::new(LookupBehavior::Panic)),
        fast_config(),
    )
    .await;
    let wire = hello("boom.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_to_close(&mut client).await;
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::Aborted]);
    assert_eq!(harness.metrics.client_bytes().0, wire.len() as u64);
    let opened = harness
        .metrics
        .events()
        .iter()
        .filter(|e| matches!(e, RecordedEvent::ConnectionOpened { .. }))
        .count();
    assert_eq!(opened, 1);
    // The router keeps serving after a connection task panics.
    let mut client = harness.connect().await;
    client.write_all(b"x").await.unwrap();
    read_to_close(&mut client).await;
    assert_eq!(
        harness.outcomes(2).await[1],
        ConnectionOutcome::InvalidHello
    );
    harness.stop().await;
}

#[tokio::test]
async fn events_identify_their_listener() {
    let backend = Backend::start(BackendMode::Echo).await;
    let harness = Harness::start_with_listeners(
        lookup(&[("multi.test", &backend.route())]),
        fast_config(),
        2,
    )
    .await;
    for addr in &harness.addrs {
        let wire = hello("multi.test");
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(&wire).await.unwrap();
        read_exact(&mut client, wire.len()).await;
    }
    harness.outcomes(2).await;
    let listeners: HashSet<_> = harness
        .metrics
        .events()
        .into_iter()
        .filter_map(|event| match event {
            RecordedEvent::ConnectionClosed { listener, .. } => Some(listener.address()),
            _ => None,
        })
        .collect();
    assert_eq!(listeners, harness.addrs.iter().copied().collect());
    harness.stop().await;
}

#[tokio::test]
async fn dns_named_backend_is_resolved_and_routed() {
    let backend = Backend::start(BackendMode::Echo).await;
    let route = format!("localhost:{}", backend.addr.port());
    let harness = Harness::start(lookup(&[("dns.test", &route)]), fast_config()).await;
    for _ in 0..3 {
        let wire = hello("dns.test");
        let mut client = harness.connect().await;
        client.write_all(&wire).await.unwrap();
        assert_eq!(read_exact(&mut client, wire.len()).await, wire);
    }
    harness.outcomes(3).await;
    assert_eq!(backend.connections(), 3);
    harness.stop().await;
}

#[tokio::test]
async fn huge_idle_timeout_does_not_break_proxying() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.idle_timeout = Some(Duration::MAX);
    let harness = Harness::start(lookup(&[("big.test", &backend.route())]), config).await;
    let wire = hello("big.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    assert_eq!(read_exact(&mut client, wire.len()).await, wire);
    client.write_all(b"more").await.unwrap();
    assert_eq!(read_exact(&mut client, 4).await, b"more");
    drop(client);
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::Proxied]);
    harness.stop().await;
}

#[tokio::test]
async fn small_copy_buffers_still_proxy_everything() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.copy_buffer_size = 1;
    let harness = Harness::start(lookup(&[("tiny.test", &backend.route())]), config).await;
    let wire = hello("tiny.test");
    let payload = vec![0xab; 100_000];
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_exact(&mut client, wire.len()).await;
    let (mut read_half, mut write_half) = client.into_split();
    let writer = {
        let payload = payload.clone();
        tokio::spawn(async move { write_half.write_all(&payload).await })
    };
    let mut echoed = vec![0; payload.len()];
    tokio::io::AsyncReadExt::read_exact(&mut read_half, &mut echoed)
        .await
        .unwrap();
    assert_eq!(echoed, payload);
    writer.await.unwrap().unwrap();
    harness.stop().await;
}

#[tokio::test]
async fn idle_listener_does_not_hold_the_only_slot() {
    // One slot, two listeners: the old per-listener accept loops let an idle
    // listener park holding the slot, so the other listener never accepted.
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.max_connections = 1;
    let harness =
        Harness::start_with_listeners(lookup(&[("two.test", &backend.route())]), config, 2).await;
    // Visit the listeners in an order that ends up asking a listener whose
    // old per-listener loop was not the one holding the slot.
    let order = [
        harness.addrs[1],
        harness.addrs[0],
        harness.addrs[1],
        harness.addrs[0],
    ];
    for addr in order {
        let wire = hello("two.test");
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(&wire).await.unwrap();
        assert_eq!(read_exact(&mut client, wire.len()).await, wire);
        drop(client);
    }
    harness.outcomes(4).await;
    harness.stop().await;
}

#[tokio::test]
async fn idle_timer_starts_when_proxying_begins() {
    // A lookup slower than the idle timeout must not make the freshly
    // proxied connection look idle.
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.idle_timeout = Some(Duration::from_millis(300));
    let scripted = Arc::new(ScriptedLookup::new(LookupBehavior::Delayed(
        Duration::from_millis(600),
        route_table(&[("slowlookup.test", &backend.route())]),
    )));
    let harness = Harness::start(scripted, config).await;
    let wire = hello("slowlookup.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    assert_eq!(read_exact(&mut client, wire.len()).await, wire);
    client.write_all(b"ping").await.unwrap();
    assert_eq!(read_exact(&mut client, 4).await, b"ping");
    // Then it does idle out after a full idle interval of silence.
    read_to_close(&mut client).await;
    assert_eq!(harness.outcomes(1).await, [ConnectionOutcome::IdleTimeout]);
    harness.stop().await;
}

// --- shutdown ---

#[tokio::test]
async fn shutdown_cancels_connection_waiting_for_hello() {
    let harness = Harness::start(lookup(&[]), fast_config()).await;
    let mut client = harness.connect().await;
    client.write_all(&hello("wait.test")[..3]).await.unwrap();
    eventually(|| {
        harness
            .metrics
            .events()
            .iter()
            .any(|e| matches!(e, RecordedEvent::ConnectionOpened { .. }))
            .then_some(())
    })
    .await;
    let metrics = harness.metrics.clone();
    let started = tokio::time::Instant::now();
    harness.stop().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "handshake stage should cancel at once"
    );
    assert_eq!(metrics.outcomes(), [ConnectionOutcome::ShutdownCancelled]);
    read_to_close(&mut client).await;
}

#[tokio::test]
async fn shutdown_cancels_connection_in_lookup() {
    let scripted = Arc::new(ScriptedLookup::hanging());
    let harness = Harness::start(scripted.clone(), fast_config()).await;
    let mut client = harness.connect().await;
    client.write_all(&hello("stuck.test")).await.unwrap();
    // Wait until the lookup has been called, so the connection is in it.
    eventually(|| (scripted.calls() > 0).then_some(())).await;
    let metrics = harness.metrics.clone();
    harness.stop().await;
    assert_eq!(metrics.outcomes(), [ConnectionOutcome::ShutdownCancelled]);
    read_to_close(&mut client).await;
}

#[tokio::test]
async fn shutdown_stops_waiting_for_a_slot() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.max_connections = 1;
    config.shutdown_grace = Duration::from_millis(200);
    let harness = Harness::start(lookup(&[("slot.test", &backend.route())]), config).await;
    let wire = hello("slot.test");
    let mut holder = harness.connect().await;
    holder.write_all(&wire).await.unwrap();
    read_exact(&mut holder, wire.len()).await;
    // The accept loop is now parked waiting for a slot.
    let started = tokio::time::Instant::now();
    harness.stop().await;
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn shutdown_lets_proxied_connections_drain_within_grace() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.shutdown_grace = Duration::from_secs(5);
    let harness = Harness::start(lookup(&[("drain.test", &backend.route())]), config).await;

    let wire = hello("drain.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_exact(&mut client, wire.len()).await;

    harness.shutdown.cancel();
    // New connections are refused once accept loops stop...
    tokio::time::sleep(Duration::from_millis(100)).await;
    // ...but the proxied connection keeps working.
    client.write_all(b"still-here").await.unwrap();
    assert_eq!(read_exact(&mut client, 10).await, b"still-here");
    drop(client);

    harness.task.await.unwrap().unwrap();
    assert_eq!(harness.metrics.outcomes(), [ConnectionOutcome::Proxied]);
}

#[tokio::test]
async fn shutdown_forces_proxied_connections_after_grace() {
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.shutdown_grace = Duration::from_millis(200);
    let harness = Harness::start(lookup(&[("force.test", &backend.route())]), config).await;

    let wire = hello("force.test");
    let mut client = harness.connect().await;
    client.write_all(&wire).await.unwrap();
    read_exact(&mut client, wire.len()).await;

    let metrics = harness.metrics.clone();
    harness.stop().await;
    assert_eq!(metrics.outcomes(), [ConnectionOutcome::ShutdownForced]);
    read_to_close(&mut client).await;
}

#[tokio::test]
async fn shutdown_racing_many_connections_leaves_none_stuck() {
    // Connections in every stage when shutdown fires: each must end as
    // cancelled, forced, or (if it raced ahead) proxied — and serve() must
    // return, proving no connection escaped both cancellation and draining.
    let backend = Backend::start(BackendMode::Echo).await;
    let mut config = fast_config();
    config.shutdown_grace = Duration::from_millis(300);
    let table = route_table(&[("race.test", &backend.route())]);
    let scripted = Arc::new(ScriptedLookup::new(LookupBehavior::Delayed(
        Duration::from_millis(5),
        table,
    )));
    let harness = Harness::start(scripted, config).await;
    let wire = hello("race.test");
    let mut clients = Vec::new();
    for _ in 0..40 {
        let mut client = harness.connect().await;
        client.write_all(&wire).await.unwrap();
        clients.push(client);
    }
    let metrics = harness.metrics.clone();
    tokio::time::timeout(Duration::from_secs(5), harness.stop())
        .await
        .expect("serve() must return after shutdown");
    for outcome in metrics.outcomes() {
        assert!(
            matches!(
                outcome,
                ConnectionOutcome::ShutdownCancelled
                    | ConnectionOutcome::ShutdownForced
                    | ConnectionOutcome::Proxied
            ),
            "unexpected outcome {outcome:?}"
        );
    }
}
