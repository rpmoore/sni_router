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

//! Backend DNS resolution with a TTL cache and a cap on concurrent lookups.
//!
//! System resolution (`getaddrinfo`) is blocking and can't be cancelled, so
//! it runs on tokio's blocking pool and keeps running after the connection
//! that asked for it gives up. Without a cap, a burst of connections during
//! a DNS slowdown could fill the blocking pool (512 threads by default) and
//! stall everything else that uses it. Each lookup therefore holds a permit
//! for as long as its blocking call runs — not just while a caller waits —
//! and results are cached so steady traffic doesn't resolve per connection.
//!
//! Lookups are single-flight per `(name, port)`: when an entry expires,
//! concurrent callers share one lookup (run on its own task, independent of
//! any caller) instead of each starting one, so there's no burst at expiry
//! and no race between an older and a newer answer overwriting each other.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, watch};

/// How long a failed resolution is remembered before retrying.
const NEGATIVE_TTL: Duration = Duration::from_secs(1);
/// How long past its TTL a previously good answer may be used when
/// re-resolving fails.
const STALE_IF_ERROR: Duration = Duration::from_secs(60);
/// Prune expired entries once the cache holds this many names. Names come
/// from the route table, not from clients, so this is a backstop.
const PRUNE_THRESHOLD: usize = 4096;

type LookupFn = dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync;
type Key = (Box<str>, u16);
/// A lookup's result as shared with every waiter (`io::Error` isn't `Clone`).
type Outcome = Result<Arc<[SocketAddr]>, (io::ErrorKind, Arc<str>)>;

pub(super) struct Resolver {
    state: Mutex<State>,
    slots: Arc<Semaphore>,
    ttl: Duration,
    lookup: Arc<LookupFn>,
    rotation: AtomicUsize,
}

#[derive(Default)]
struct State {
    cache: HashMap<Key, Entry>,
    inflight: HashMap<Key, watch::Receiver<Option<Outcome>>>,
}

struct Entry {
    /// The last good answer, if any.
    addrs: Option<Arc<[SocketAddr]>>,
    /// Until when this entry is used without re-resolving.
    fresh_until: Instant,
    /// Until when `addrs` may be used if re-resolving fails.
    stale_until: Instant,
}

impl Resolver {
    pub(super) fn new(ttl: Duration, max_concurrent: usize) -> Self {
        Self::with_lookup(ttl, max_concurrent, Arc::new(system_lookup))
    }

    pub(super) fn with_lookup(ttl: Duration, max_concurrent: usize, lookup: Arc<LookupFn>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
            ttl,
            lookup,
            rotation: AtomicUsize::new(0),
        }
    }

    /// Resolves `name:port`, from cache when fresh. The returned addresses
    /// are rotated per call so connections spread across all of them.
    pub(super) async fn resolve(
        self: &Arc<Self>,
        name: &str,
        port: u16,
    ) -> io::Result<Vec<SocketAddr>> {
        let mut outcome = {
            let mut state = self.lock();
            let key: Key = (name.into(), port);
            if let Some(cached) = fresh(&state.cache, &key) {
                return into_io(cached).map(|addrs| self.rotated(&addrs));
            }
            match state.inflight.get(&key) {
                Some(outcome) => outcome.clone(),
                None => {
                    let (sender, outcome) = watch::channel(None);
                    state.inflight.insert(key.clone(), outcome.clone());
                    self.spawn_lookup(key, sender);
                    outcome
                }
            }
        };
        let resolved = match outcome.wait_for(Option::is_some).await {
            Ok(value) => value.clone().unwrap_or_else(|| Err(closed())),
            Err(_sender_dropped) => Err(closed()),
        };
        into_io(resolved).map(|addrs| self.rotated(&addrs))
    }

    /// Runs one lookup on its own task, so it completes (and is cached) even
    /// if every caller waiting on it gives up.
    fn spawn_lookup(self: &Arc<Self>, key: Key, sender: watch::Sender<Option<Outcome>>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = match Arc::clone(&this.slots).acquire_owned().await {
                Ok(permit) => {
                    let lookup = Arc::clone(&this.lookup);
                    let (name, port) = (key.0.clone(), key.1);
                    tokio::task::spawn_blocking(move || {
                        // Held until the blocking call returns.
                        let _permit = permit;
                        lookup(&name, port)
                    })
                    .await
                    .unwrap_or_else(|error| {
                        Err(io::Error::other(format!("resolver task failed: {error}")))
                    })
                }
                Err(_closed) => Err(io::Error::other("resolver closed")),
            };
            let outcome = {
                let mut state = this.lock();
                state.inflight.remove(&key);
                this.store(&mut state.cache, key, &result)
            };
            let _ = sender.send(Some(outcome));
        });
    }

    /// Records a lookup result and returns what the caller should use:
    /// the new answer, a stale answer if the lookup failed within the stale
    /// window, or the error.
    fn store(
        &self,
        cache: &mut HashMap<Key, Entry>,
        key: Key,
        result: &io::Result<Vec<SocketAddr>>,
    ) -> Outcome {
        let now = Instant::now();
        let (name, port) = key;
        if cache.len() >= PRUNE_THRESHOLD {
            cache.retain(|_, entry| now < entry.stale_until.max(entry.fresh_until));
        }
        match result {
            Ok(addrs) if !addrs.is_empty() => {
                let addrs: Arc<[SocketAddr]> = Arc::from(addrs.as_slice());
                let fresh_until = later(now, self.ttl);
                cache.insert(
                    (name, port),
                    Entry {
                        addrs: Some(Arc::clone(&addrs)),
                        fresh_until,
                        stale_until: later(fresh_until, STALE_IF_ERROR),
                    },
                );
                Ok(addrs)
            }
            failed => {
                let error: (io::ErrorKind, Arc<str>) = match failed {
                    Err(error) => (error.kind(), format!("resolving {name}: {error}").into()),
                    Ok(_) => (
                        io::ErrorKind::NotFound,
                        format!("{name} resolved to no addresses").into(),
                    ),
                };
                let entry = cache.entry((name, port)).or_insert(Entry {
                    addrs: None,
                    fresh_until: now,
                    stale_until: now,
                });
                entry.fresh_until = later(now, NEGATIVE_TTL);
                match &entry.addrs {
                    Some(addrs) if now < entry.stale_until => Ok(Arc::clone(addrs)),
                    _ => {
                        entry.addrs = None;
                        Err(error)
                    }
                }
            }
        }
    }

    fn rotated(&self, addrs: &[SocketAddr]) -> Vec<SocketAddr> {
        let start = self.rotation.fetch_add(1, Ordering::Relaxed) % addrs.len().max(1);
        addrs[start..]
            .iter()
            .chain(&addrs[..start])
            .copied()
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The cached outcome for `key` if it's still fresh.
fn fresh(cache: &HashMap<Key, Entry>, key: &Key) -> Option<Outcome> {
    let entry = cache.get(key)?;
    (Instant::now() < entry.fresh_until).then(|| {
        entry.addrs.clone().ok_or_else(|| {
            (
                io::ErrorKind::NotFound,
                format!("recent resolution of {} failed", key.0).into(),
            )
        })
    })
}

fn into_io(outcome: Outcome) -> io::Result<Arc<[SocketAddr]>> {
    outcome.map_err(|(kind, message)| io::Error::new(kind, message.to_string()))
}

fn closed() -> (io::ErrorKind, Arc<str>) {
    (
        io::ErrorKind::Other,
        "resolver lookup ended without a result".into(),
    )
}

fn system_lookup(name: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok((name, port).to_socket_addrs()?.collect())
}

/// `instant + duration`, saturating instead of panicking on overflow.
fn later(instant: Instant, duration: Duration) -> Instant {
    instant
        .checked_add(duration)
        .unwrap_or_else(|| instant + Duration::from_secs(100 * 365 * 24 * 3600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], 443))
    }

    fn counting(
        answer: impl Fn(usize) -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static,
    ) -> (Arc<LookupFn>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let lookup: Arc<LookupFn> = Arc::new(move |_name: &str, _port: u16| {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            answer(call)
        });
        (lookup, calls)
    }

    #[tokio::test]
    async fn fresh_answers_are_served_from_cache() {
        let (lookup, calls) = counting(|_| Ok(vec![addr(1)]));
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 4, lookup));
        for _ in 0..5 {
            assert_eq!(resolver.resolve("svc", 443).await.unwrap(), [addr(1)]);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn expired_answers_are_re_resolved() {
        let (lookup, calls) = counting(|call| Ok(vec![addr(call as u8 + 1)]));
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_millis(20), 4, lookup));
        assert_eq!(resolver.resolve("svc", 443).await.unwrap(), [addr(1)]);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(resolver.resolve("svc", 443).await.unwrap(), [addr(2)]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failures_are_briefly_cached() {
        let (lookup, calls) =
            counting(|_| Err(io::Error::new(io::ErrorKind::NotFound, "nxdomain")));
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 4, lookup));
        assert!(resolver.resolve("gone", 443).await.is_err());
        assert!(resolver.resolve("gone", 443).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_answer_is_used_when_re_resolution_fails() {
        let (lookup, _) = counting(|call| {
            if call == 0 {
                Ok(vec![addr(1)])
            } else {
                Err(io::Error::other("resolver down"))
            }
        });
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_millis(20), 4, lookup));
        resolver.resolve("svc", 443).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(resolver.resolve("svc", 443).await.unwrap(), [addr(1)]);
    }

    #[tokio::test]
    async fn empty_answer_is_an_error() {
        let (lookup, _) = counting(|_| Ok(Vec::new()));
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 4, lookup));
        assert!(resolver.resolve("svc", 443).await.is_err());
    }

    #[tokio::test]
    async fn addresses_rotate_across_calls() {
        let (lookup, _) = counting(|_| Ok(vec![addr(1), addr(2), addr(3)]));
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 4, lookup));
        let mut firsts = Vec::new();
        for _ in 0..3 {
            let addrs = resolver.resolve("svc", 443).await.unwrap();
            assert_eq!(addrs.len(), 3);
            firsts.push(addrs[0]);
        }
        firsts.sort();
        assert_eq!(firsts, [addr(1), addr(2), addr(3)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_one_lookup() {
        let (lookup, calls) = counting(|_| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(vec![addr(1)])
        });
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 16, lookup));
        let callers: Vec<_> = (0..50)
            .map(|_| {
                let resolver = Arc::clone(&resolver);
                tokio::spawn(async move { resolver.resolve("svc", 443).await })
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.await.unwrap().unwrap(), [addr(1)]);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lookup_completes_and_caches_after_callers_give_up() {
        let (lookup, calls) = counting(|_| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(vec![addr(1)])
        });
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 4, lookup));
        let _ = tokio::time::timeout(Duration::from_millis(5), resolver.resolve("svc", 443)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(resolver.resolve("svc", 443).await.unwrap(), [addr(1)]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn panicking_lookup_reports_an_error_and_recovers() {
        let (lookup, _) = counting(|call| {
            if call == 0 {
                panic!("resolver bug");
            }
            Ok(vec![addr(1)])
        });
        let resolver = Arc::new(Resolver::with_lookup(Duration::ZERO, 4, lookup));
        assert!(resolver.resolve("svc", 443).await.is_err());
        tokio::time::sleep(Duration::from_millis(1100)).await; // past the negative TTL
        assert_eq!(resolver.resolve("svc", 443).await.unwrap(), [addr(1)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_lookups_are_capped_even_after_callers_give_up() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let lookup: Arc<LookupFn> = {
            let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
            Arc::new(move |_name: &str, _port: u16| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
                running.fetch_sub(1, Ordering::SeqCst);
                Err(io::Error::other("slow resolver"))
            })
        };
        let resolver = Arc::new(Resolver::with_lookup(Duration::from_secs(30), 2, lookup));
        // Each caller gives up long before its lookup finishes; distinct
        // names defeat the cache.
        let callers: Vec<_> = (0..10)
            .map(|i| {
                let resolver = Arc::clone(&resolver);
                tokio::spawn(async move {
                    let name = format!("svc{i}");
                    let _ = tokio::time::timeout(
                        Duration::from_millis(5),
                        resolver.resolve(&name, 443),
                    )
                    .await;
                })
            })
            .collect();
        for caller in callers {
            caller.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak {} > cap",
            peak.load(Ordering::SeqCst)
        );
    }
}
