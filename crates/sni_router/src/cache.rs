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

//! A caching [`RouteLookup`] decorator for slow or remote route stores.
//!
//! # Consistency
//!
//! A changed or removed route becomes visible within `ttl`; a newly added
//! route within `negative_ttl` (misses are cached too). The cache has no
//! change feed of its own; embedders that have one call
//! [`CachedLookup::invalidate`] or [`CachedLookup::clear`].
//!
//! # Load behavior
//!
//! Concurrent misses for the same hostname share one load (single-flight),
//! so a hot entry expiring sends one query to the store, not one per
//! connection. Each load runs on its own task: a caller that times out or is
//! cancelled never cancels the load, and a load that panics or is dropped
//! wakes its waiters with an error and clears its slot so the next lookup
//! starts fresh.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::lookup::{LookupError, LookupFuture, RouteCandidates, RouteHits, RouteKey, RouteLookup};
use crate::metrics::{CacheEvent, MetricEvent, MetricsSink};

/// Cache tuning. Construct with [`CacheConfig::default`] and override fields.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CacheConfig {
    /// How long a found route is served from cache.
    pub ttl: Duration,
    /// How long "no route" is served from cache.
    pub negative_ttl: Duration,
    /// Each positive entry's TTL is scaled by a random factor in
    /// `1 ± jitter` so entries loaded together don't expire together.
    pub jitter: f64,
    /// Most entries held. When full, expired entries are purged first, then
    /// the oldest entry is evicted.
    pub max_entries: usize,
    /// When set, an expired entry may still be served for this long if
    /// reloading it fails.
    pub stale_if_error: Option<Duration>,
    /// Upper bound on one load from the inner lookup. Loads outlive the
    /// callers that started them, so without this a store that never
    /// answers would pin a task and an in-flight slot per hostname forever.
    pub load_timeout: Duration,
    /// Most loads running at once across all keys. Loads outlive the
    /// callers that started them (up to `load_timeout`), so a stream of
    /// unique hostnames could otherwise run arrival-rate × `load_timeout`
    /// queries against the store. When saturated, a miss serves its stale
    /// entry if it has one and otherwise fails closed.
    pub max_inflight_loads: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30),
            negative_ttl: Duration::from_secs(5),
            jitter: 0.1,
            max_entries: 100_000,
            stale_if_error: None,
            load_timeout: Duration::from_secs(10),
            max_inflight_loads: 1024,
        }
    }
}

type LoadResult = Result<RouteHits, LookupError>;

/// Wraps a [`RouteLookup`] with a TTL cache and single-flight loads.
pub struct CachedLookup<L> {
    inner: Arc<L>,
    state: Arc<Mutex<CacheState>>,
    config: Arc<CacheConfig>,
    metrics: Arc<dyn MetricsSink>,
}

struct CacheState {
    entries: HashMap<RouteCandidates, Entry>,
    /// Every cached entry, indexed by each key in its candidate set, so
    /// `invalidate` touches only the entries a key could have answered
    /// instead of scanning the whole cache under the lock.
    index: HashMap<RouteKey, HashSet<RouteCandidates>>,
    /// Insertion order for eviction. May hold superseded records; an entry is
    /// live only if its id matches.
    order: VecDeque<(RouteCandidates, u64)>,
    inflight: HashMap<RouteCandidates, Inflight>,
    next_id: u64,
    /// Loads whose task hasn't finished, including ones whose in-flight
    /// slot was dropped by invalidation.
    running_loads: usize,
    last_purge: Option<Instant>,
    jitter_seed: RandomState,
}

struct Entry {
    hits: RouteHits,
    expires: Instant,
    stale_until: Instant,
    id: u64,
}

struct Inflight {
    result: watch::Receiver<Option<LoadResult>>,
    id: u64,
}

impl<L: RouteLookup + 'static> CachedLookup<L> {
    pub fn new(inner: L, config: CacheConfig, metrics: Arc<dyn MetricsSink>) -> Self {
        Self {
            inner: Arc::new(inner),
            state: Arc::new(Mutex::new(CacheState {
                entries: HashMap::new(),
                index: HashMap::new(),
                order: VecDeque::new(),
                inflight: HashMap::new(),
                next_id: 0,
                running_loads: 0,
                last_purge: None,
                jitter_seed: RandomState::new(),
            })),
            config: Arc::new(config),
            metrics,
        }
    }

    pub fn inner(&self) -> &L {
        &self.inner
    }

    /// Drops every cached entry that `key` could have answered, e.g. after a
    /// change feed reports that `key`'s row changed. Loads already in flight
    /// still answer the callers already waiting on them, but aren't cached
    /// and aren't joined by later lookups.
    pub fn invalidate(&self, key: &RouteKey) {
        let mut state = lock(&self.state);
        for candidates in state.index.remove(key).unwrap_or_default() {
            state.remove_entry(&candidates);
        }
        // Dropping the in-flight slot makes lookups that start from now on
        // begin a fresh load, and stops the old load from caching its result
        // (it only caches while it still owns its slot). In-flight loads are
        // bounded by `max_inflight_loads`, so this scan is too.
        state
            .inflight
            .retain(|candidates, _| !candidates.contains(key));
    }

    /// Drops every cached entry.
    pub fn clear(&self) {
        let mut state = lock(&self.state);
        state.entries.clear();
        state.index.clear();
        state.order.clear();
        state.inflight.clear();
    }

    /// Number of cached entries, including expired ones not yet purged.
    pub fn len(&self) -> usize {
        lock(&self.state).entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    async fn cached_lookup(&self, candidates: &RouteCandidates) -> LoadResult {
        let now = Instant::now();
        let (mut result, stale, event) = {
            let mut state = lock(&self.state);
            let mut stale = None;
            if let Some(entry) = state.entries.get(candidates) {
                if now < entry.expires {
                    let event = if entry.hits.is_empty() {
                        CacheEvent::NegativeHit
                    } else {
                        CacheEvent::Hit
                    };
                    let hits = entry.hits.clone();
                    drop(state);
                    self.record(event);
                    return Ok(hits);
                }
                if now < entry.stale_until {
                    stale = Some(entry.hits.clone());
                }
            }
            match state.inflight.get(candidates) {
                Some(inflight) => (inflight.result.clone(), stale, CacheEvent::Coalesced),
                None if state.running_loads >= self.config.max_inflight_loads => {
                    drop(state);
                    return self.shed(stale);
                }
                None => (
                    self.start_load(&mut state, candidates.clone()),
                    stale,
                    CacheEvent::Miss,
                ),
            }
        };
        self.record(event);

        let loaded = match result.wait_for(Option::is_some).await {
            Ok(value) => value.clone().unwrap_or_else(|| Err(load_aborted())),
            Err(_sender_dropped) => Err(load_aborted()),
        };
        match (loaded, stale) {
            (Ok(hits), _) => Ok(hits),
            (Err(_), Some(stale)) => {
                self.record(CacheEvent::StaleServed);
                Ok(stale)
            }
            (Err(error), None) => Err(error),
        }
    }

    /// Too many loads are running: serve stale if possible, else fail closed
    /// without starting another load.
    fn shed(&self, stale: Option<RouteHits>) -> LoadResult {
        self.record(CacheEvent::Shed);
        match stale {
            Some(stale) => {
                self.record(CacheEvent::StaleServed);
                Ok(stale)
            }
            None => Err(LookupError::new("too many route loads in flight")),
        }
    }

    /// Registers an in-flight load and spawns it. Called with the lock held.
    fn start_load(
        &self,
        state: &mut CacheState,
        candidates: RouteCandidates,
    ) -> watch::Receiver<Option<LoadResult>> {
        let (sender, receiver) = watch::channel(None);
        let id = state.take_id();
        state.running_loads += 1;
        state.inflight.insert(
            candidates.clone(),
            Inflight {
                result: receiver.clone(),
                id,
            },
        );
        let guard = LoadGuard {
            state: Arc::clone(&self.state),
            config: Arc::clone(&self.config),
            metrics: Arc::clone(&self.metrics),
            candidates: candidates.clone(),
            id,
            sender: Some(sender),
        };
        let inner = Arc::clone(&self.inner);
        let load_timeout = self.config.load_timeout;
        tokio::spawn(async move {
            let result = tokio::time::timeout(load_timeout, inner.lookup(&candidates))
                .await
                .unwrap_or_else(|_| Err(LookupError::new("route load timed out")));
            guard.complete(result);
        });
        receiver
    }

    fn record(&self, event: CacheEvent) {
        self.metrics.record(MetricEvent::Cache(event));
    }
}

impl<L: RouteLookup + 'static> RouteLookup for CachedLookup<L> {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a> {
        Box::pin(self.cached_lookup(candidates))
    }
}

impl CacheState {
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn insert(&mut self, candidates: RouteCandidates, hits: RouteHits, config: &CacheConfig) {
        if config.max_entries == 0 {
            return;
        }
        let now = Instant::now();
        let id = self.take_id();
        let ttl = if hits.is_empty() {
            config.negative_ttl
        } else {
            self.jittered(config.ttl, config.jitter, id)
        };
        let expires = later(now, ttl);
        let stale_until = later(expires, config.stale_if_error.unwrap_or_default());
        if !self.entries.contains_key(&candidates) {
            self.make_room(now, config);
            for key in candidates.keys() {
                self.index
                    .entry(key.clone())
                    .or_default()
                    .insert(candidates.clone());
            }
        }
        self.entries.insert(
            candidates.clone(),
            Entry {
                hits,
                expires,
                stale_until,
                id,
            },
        );
        self.order.push_back((candidates, id));
        if self.order.len() > config.max_entries.saturating_mul(2) {
            let entries = &self.entries;
            self.order
                .retain(|(key, id)| entries.get(key).is_some_and(|entry| entry.id == *id));
        }
    }

    /// Frees a slot: purge dead entries (at most once per `negative_ttl`,
    /// since it's a full scan), then evict oldest-first.
    fn make_room(&mut self, now: Instant, config: &CacheConfig) {
        if self.entries.len() < config.max_entries {
            return;
        }
        let purge_due = self
            .last_purge
            .is_none_or(|last| now.duration_since(last) >= config.negative_ttl);
        if purge_due {
            let dead: Vec<RouteCandidates> = self
                .entries
                .iter()
                .filter(|(_, entry)| now >= entry.stale_until)
                .map(|(candidates, _)| candidates.clone())
                .collect();
            for candidates in &dead {
                self.remove_entry(candidates);
            }
            self.last_purge = Some(now);
        }
        while self.entries.len() >= config.max_entries {
            let Some((key, id)) = self.order.pop_front() else {
                break;
            };
            if self.entries.get(&key).is_some_and(|entry| entry.id == id) {
                self.remove_entry(&key);
            }
        }
    }

    /// Removes an entry and its index records.
    fn remove_entry(&mut self, candidates: &RouteCandidates) {
        if self.entries.remove(candidates).is_none() {
            return;
        }
        for key in candidates.keys() {
            if let Some(indexed) = self.index.get_mut(key) {
                indexed.remove(candidates);
                if indexed.is_empty() {
                    self.index.remove(key);
                }
            }
        }
    }

    fn jittered(&self, ttl: Duration, jitter: f64, id: u64) -> Duration {
        let jitter = jitter.clamp(0.0, 1.0);
        // Uniform in [0, 1) from the top 53 bits of a keyed hash.
        let unit = (self.jitter_seed.hash_one(id) >> 11) as f64 / (1u64 << 53) as f64;
        // `mul_f64` panics on overflow or NaN; fall back to the plain TTL.
        Duration::try_from_secs_f64(ttl.as_secs_f64() * (1.0 + jitter * (2.0 * unit - 1.0)))
            .unwrap_or(ttl)
    }
}

/// Owns an in-flight load's completion. Dropping it without completing —
/// the load task panicked or was dropped — clears the slot and wakes
/// waiters with an error, so no key is ever left stuck "in flight".
struct LoadGuard {
    state: Arc<Mutex<CacheState>>,
    config: Arc<CacheConfig>,
    metrics: Arc<dyn MetricsSink>,
    candidates: RouteCandidates,
    id: u64,
    sender: Option<watch::Sender<Option<LoadResult>>>,
}

impl LoadGuard {
    fn complete(mut self, result: LoadResult) {
        self.finish(result);
    }

    fn finish(&mut self, result: LoadResult) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        {
            let mut state = lock(&self.state);
            state.running_loads -= 1;
            // Still owning the slot means nothing invalidated this key while
            // the load ran, so its result is safe to cache.
            let owns_slot = state
                .inflight
                .get(&self.candidates)
                .is_some_and(|inflight| inflight.id == self.id);
            if owns_slot {
                state.inflight.remove(&self.candidates);
            }
            match &result {
                Ok(hits) if owns_slot => {
                    state.insert(self.candidates.clone(), hits.clone(), &self.config);
                }
                Ok(_) => {}
                Err(_) => {
                    // Drop the entry once it's past its stale window so a
                    // failing store doesn't pin a dead entry.
                    let now = Instant::now();
                    if state
                        .entries
                        .get(&self.candidates)
                        .is_some_and(|entry| now >= entry.stale_until)
                    {
                        state.remove_entry(&self.candidates);
                    }
                }
            }
        }
        if result.is_err() {
            self.metrics
                .record(MetricEvent::Cache(CacheEvent::LoadError));
        }
        // No receivers left is fine: the result is cached for the next caller.
        let _ = sender.send(Some(result));
    }
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.finish(Err(load_aborted()));
    }
}

/// `instant + duration`, saturating instead of panicking when an embedder
/// configures a TTL too large to represent.
fn later(instant: Instant, duration: Duration) -> Instant {
    instant
        .checked_add(duration)
        .unwrap_or_else(|| instant + Duration::from_secs(100 * 365 * 24 * 3600))
}

fn load_aborted() -> LookupError {
    LookupError::new("route load aborted before completing")
}

fn lock(state: &Mutex<CacheState>) -> MutexGuard<'_, CacheState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::Hostname;
    use crate::testing::{LookupBehavior, RecordingMetrics, ScriptedLookup, route_table};

    const ROUTES: &[(&str, &str)] = &[("a.test", "a:1"), ("b.test", "b:1"), ("c.test", "c:1")];

    fn candidates(name: &str) -> RouteCandidates {
        RouteCandidates::for_hostname(&Hostname::parse(name).unwrap())
    }

    fn cache(
        behavior: LookupBehavior,
        config: CacheConfig,
    ) -> (CachedLookup<ScriptedLookup>, Arc<RecordingMetrics>) {
        let metrics = Arc::new(RecordingMetrics::new());
        let cache = CachedLookup::new(ScriptedLookup::new(behavior), config, metrics.clone());
        (cache, metrics)
    }

    fn found() -> LookupBehavior {
        LookupBehavior::Routes(route_table(ROUTES))
    }

    fn no_jitter() -> CacheConfig {
        CacheConfig {
            jitter: 0.0,
            ..CacheConfig::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn second_lookup_is_served_from_cache() {
        let (cache, metrics) = cache(found(), no_jitter());
        let key = candidates("a.test");
        assert_eq!(cache.lookup(&key).await.unwrap().len(), 1);
        assert_eq!(cache.lookup(&key).await.unwrap().len(), 1);
        assert_eq!(cache.inner().calls(), 1);
        assert_eq!(metrics.cache_events(), [CacheEvent::Miss, CacheEvent::Hit]);
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_ttl() {
        let (cache, _) = cache(found(), no_jitter());
        let key = candidates("a.test");
        cache.lookup(&key).await.unwrap();
        tokio::time::advance(Duration::from_secs(29)).await;
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn jitter_keeps_ttl_within_bounds() {
        let (cache, _) = cache(found(), CacheConfig::default());
        let key = candidates("a.test");
        cache.lookup(&key).await.unwrap();
        tokio::time::advance(Duration::from_millis(26_900)).await;
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 1, "expired before ttl * 0.9");
        tokio::time::advance(Duration::from_millis(6_200)).await;
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 2, "still cached after ttl * 1.1");
    }

    #[tokio::test(start_paused = true)]
    async fn misses_are_cached_for_negative_ttl() {
        let (cache, metrics) = cache(found(), no_jitter());
        let key = candidates("missing.test");
        assert!(cache.lookup(&key).await.unwrap().is_empty());
        assert!(cache.lookup(&key).await.unwrap().is_empty());
        assert_eq!(cache.inner().calls(), 1);
        assert_eq!(metrics.count_cache(CacheEvent::NegativeHit), 1);
        tokio::time::advance(Duration::from_secs(6)).await;
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn errors_are_not_cached() {
        let (cache, metrics) = cache(LookupBehavior::Fail("down".into()), no_jitter());
        let key = candidates("a.test");
        assert!(cache.lookup(&key).await.is_err());
        assert!(cache.lookup(&key).await.is_err());
        assert_eq!(cache.inner().calls(), 2);
        assert_eq!(metrics.count_cache(CacheEvent::LoadError), 2);
        assert!(cache.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_misses_share_one_load() {
        let (cache, metrics) = cache(
            LookupBehavior::Delayed(Duration::from_millis(100), route_table(ROUTES)),
            no_jitter(),
        );
        let cache = Arc::new(cache);
        let key = candidates("a.test");
        let waiters: Vec<_> = (0..100)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let key = key.clone();
                tokio::spawn(async move { cache.lookup(&key).await.map(|hits| hits.len()) })
            })
            .collect();
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap().unwrap(), 1);
        }
        assert_eq!(cache.inner().calls(), 1);
        assert_eq!(metrics.count_cache(CacheEvent::Miss), 1);
        assert_eq!(metrics.count_cache(CacheEvent::Coalesced), 99);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_first_caller_does_not_cancel_the_load() {
        let (cache, _) = cache(
            LookupBehavior::Delayed(Duration::from_millis(100), route_table(ROUTES)),
            no_jitter(),
        );
        let key = candidates("a.test");
        // The caller that started the load gives up early.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), cache.lookup(&key))
                .await
                .is_err()
        );
        // A later caller joins the same load instead of starting another.
        assert_eq!(cache.lookup(&key).await.unwrap().len(), 1);
        assert_eq!(cache.inner().calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn load_completes_and_caches_with_no_waiters_left() {
        let (cache, metrics) = cache(
            LookupBehavior::Delayed(Duration::from_millis(100), route_table(ROUTES)),
            no_jitter(),
        );
        let key = candidates("a.test");
        let _ = tokio::time::timeout(Duration::from_millis(10), cache.lookup(&key)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 1);
        assert_eq!(metrics.cache_events().last(), Some(&CacheEvent::Hit));
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_load_wakes_waiters_and_next_lookup_starts_fresh() {
        let (cache, _) = cache(LookupBehavior::Panic, no_jitter());
        let key = candidates("a.test");
        assert!(cache.lookup(&key).await.is_err());
        cache.inner().set_behavior(found());
        assert_eq!(cache.lookup(&key).await.unwrap().len(), 1);
        assert_eq!(cache.inner().calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_load_is_abandoned_after_load_timeout() {
        let config = CacheConfig {
            load_timeout: Duration::from_secs(1),
            ..no_jitter()
        };
        let (cache, metrics) = cache(LookupBehavior::Hang, config);
        let key = candidates("a.test");
        // The caller gives up (as the router's lookup timeout would)...
        let _ = tokio::time::timeout(Duration::from_millis(100), cache.lookup(&key)).await;
        // ...and the load itself is bounded, freeing its in-flight slot.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(metrics.count_cache(CacheEvent::LoadError), 1);
        cache.inner().set_behavior(found());
        assert_eq!(cache.lookup(&key).await.unwrap().len(), 1);
        assert_eq!(cache.inner().calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn lookup_after_invalidate_does_not_join_older_load() {
        let (cache, _) = cache(
            LookupBehavior::Delayed(
                Duration::from_millis(100),
                route_table(&[("a.test", "old:1")]),
            ),
            no_jitter(),
        );
        let cache = Arc::new(cache);
        let key = candidates("a.test");
        let before = tokio::spawn({
            let cache = Arc::clone(&cache);
            let key = key.clone();
            async move { cache.lookup(&key).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.inner().set_behavior(LookupBehavior::Delayed(
            Duration::from_millis(100),
            route_table(&[("a.test", "new:1")]),
        ));
        cache.invalidate(&RouteKey::parse("a.test").unwrap());
        // Started after invalidation while the old load is still running.
        let after = cache.lookup(&key).await.unwrap();
        assert_eq!(after.iter().next().unwrap().1.to_string(), "new:1");
        let old = before.await.unwrap().unwrap();
        assert_eq!(old.iter().next().unwrap().1.to_string(), "old:1");
        assert_eq!(cache.inner().calls(), 2);
        // The old load finishing must not overwrite the newer entry.
        let cached = cache.lookup(&key).await.unwrap();
        assert_eq!(cached.iter().next().unwrap().1.to_string(), "new:1");
    }

    #[tokio::test(start_paused = true)]
    async fn invalidating_one_key_still_caches_unrelated_loads() {
        let (cache, _) = cache(
            LookupBehavior::Delayed(Duration::from_millis(100), route_table(ROUTES)),
            no_jitter(),
        );
        let cache = Arc::new(cache);
        let b = candidates("b.test");
        let loading = tokio::spawn({
            let cache = Arc::clone(&cache);
            let b = b.clone();
            async move { cache.lookup(&b).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.invalidate(&RouteKey::parse("a.test").unwrap());
        loading.await.unwrap().unwrap();
        cache.lookup(&b).await.unwrap();
        assert_eq!(
            cache.inner().calls(),
            1,
            "b.test result should have been cached"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_loads_fail_closed_without_starting_more() {
        let config = CacheConfig {
            max_inflight_loads: 2,
            load_timeout: Duration::from_secs(5),
            ..no_jitter()
        };
        let (cache, metrics) = cache(LookupBehavior::Hang, config);
        for name in ["a.test", "b.test", "c.test", "d.test"] {
            let _ =
                tokio::time::timeout(Duration::from_millis(10), cache.lookup(&candidates(name)))
                    .await;
        }
        assert_eq!(
            cache.inner().calls(),
            2,
            "only max_inflight_loads loads may start"
        );
        assert_eq!(metrics.count_cache(CacheEvent::Shed), 2);
        // Once the hung loads time out, capacity returns.
        tokio::time::sleep(Duration::from_secs(6)).await;
        cache.inner().set_behavior(found());
        assert_eq!(cache.lookup(&candidates("c.test")).await.unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_loads_serve_stale_when_allowed() {
        let config = CacheConfig {
            max_inflight_loads: 1,
            stale_if_error: Some(Duration::from_secs(60)),
            ..no_jitter()
        };
        let (cache, metrics) = cache(found(), config);
        cache.lookup(&candidates("a.test")).await.unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        cache.inner().set_behavior(LookupBehavior::Hang);
        let _ = tokio::time::timeout(
            Duration::from_millis(10),
            cache.lookup(&candidates("b.test")),
        )
        .await;
        // a.test is expired and the one load slot is taken: serve stale.
        assert_eq!(cache.lookup(&candidates("a.test")).await.unwrap().len(), 1);
        assert_eq!(metrics.count_cache(CacheEvent::StaleServed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn huge_ttls_do_not_panic() {
        let config = CacheConfig {
            ttl: Duration::MAX,
            negative_ttl: Duration::MAX,
            stale_if_error: Some(Duration::MAX),
            jitter: 0.1,
            ..no_jitter()
        };
        let (cache, _) = cache(found(), config);
        cache.lookup(&candidates("a.test")).await.unwrap();
        cache.lookup(&candidates("missing.test")).await.unwrap();
        cache.lookup(&candidates("a.test")).await.unwrap();
        assert_eq!(cache.inner().calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_entry_is_served_when_reload_fails_within_window() {
        let config = CacheConfig {
            stale_if_error: Some(Duration::from_secs(10)),
            ..no_jitter()
        };
        let (cache, metrics) = cache(found(), config);
        let key = candidates("a.test");
        cache.lookup(&key).await.unwrap();
        cache
            .inner()
            .set_behavior(LookupBehavior::Fail("down".into()));

        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(cache.lookup(&key).await.unwrap().len(), 1);
        assert_eq!(metrics.count_cache(CacheEvent::StaleServed), 1);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(cache.lookup(&key).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_is_not_served_by_default() {
        let (cache, _) = cache(found(), no_jitter());
        let key = candidates("a.test");
        cache.lookup(&key).await.unwrap();
        cache
            .inner()
            .set_behavior(LookupBehavior::Fail("down".into()));
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(cache.lookup(&key).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn full_cache_evicts_oldest_first() {
        let config = CacheConfig {
            max_entries: 2,
            ..no_jitter()
        };
        let (cache, _) = cache(found(), config);
        for name in ["a.test", "b.test", "c.test"] {
            cache.lookup(&candidates(name)).await.unwrap();
        }
        assert_eq!(cache.len(), 2);
        cache.lookup(&candidates("c.test")).await.unwrap();
        cache.lookup(&candidates("b.test")).await.unwrap();
        assert_eq!(cache.inner().calls(), 3);
        cache.lookup(&candidates("a.test")).await.unwrap();
        assert_eq!(cache.inner().calls(), 4, "oldest entry was evicted");
    }

    #[tokio::test(start_paused = true)]
    async fn full_cache_purges_expired_before_evicting_live() {
        let config = CacheConfig {
            max_entries: 2,
            ..no_jitter()
        };
        let (cache, _) = cache(found(), config);
        cache.lookup(&candidates("missing.test")).await.unwrap(); // negative, 5s
        cache.lookup(&candidates("a.test")).await.unwrap(); // positive, 30s
        tokio::time::advance(Duration::from_secs(6)).await;
        cache.lookup(&candidates("b.test")).await.unwrap();
        // a.test survived; the expired negative entry was purged instead.
        cache.lookup(&candidates("a.test")).await.unwrap();
        assert_eq!(cache.inner().calls(), 3);
    }

    /// Index records exist for exactly the cached entries' keys.
    fn assert_index_consistent<L: RouteLookup + 'static>(cache: &CachedLookup<L>) {
        let state = lock(&cache.state);
        let mut expected: HashMap<RouteKey, HashSet<RouteCandidates>> = HashMap::new();
        for candidates in state.entries.keys() {
            for key in candidates.keys() {
                expected
                    .entry(key.clone())
                    .or_default()
                    .insert(candidates.clone());
            }
        }
        assert_eq!(state.index, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn index_tracks_inserts_evictions_purges_and_invalidations() {
        let config = CacheConfig {
            max_entries: 3,
            ..no_jitter()
        };
        let routes = route_table(&[("a.test", "a:1"), ("*.w.test", "w:1"), ("b.test", "b:1")]);
        let (cache, _) = cache(LookupBehavior::Routes(routes), config);
        for name in ["a.test", "x.w.test", "y.w.test", "missing.test", "b.test"] {
            cache.lookup(&candidates(name)).await.unwrap();
            assert_index_consistent(&cache);
        }
        tokio::time::advance(Duration::from_secs(6)).await; // negative entry expires
        cache.lookup(&candidates("z.w.test")).await.unwrap();
        assert_index_consistent(&cache);
        cache.invalidate(&RouteKey::parse("*.w.test").unwrap());
        assert_index_consistent(&cache);
        assert!(
            lock(&cache.state)
                .entries
                .keys()
                .all(|c| c.wildcard().map(RouteKey::as_str) != Some("*.w.test"))
        );
        cache.clear();
        assert_index_consistent(&cache);
    }

    #[tokio::test(start_paused = true)]
    async fn invalidate_drops_entries_for_a_key() {
        let (cache, _) = cache(found(), no_jitter());
        cache.lookup(&candidates("x.a.test")).await.unwrap();
        cache.lookup(&candidates("b.test")).await.unwrap();
        cache.invalidate(&RouteKey::parse("*.a.test").unwrap());
        cache.lookup(&candidates("x.a.test")).await.unwrap();
        cache.lookup(&candidates("b.test")).await.unwrap();
        assert_eq!(cache.inner().calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn load_started_before_invalidation_is_not_cached() {
        let (cache, _) = cache(
            LookupBehavior::Delayed(Duration::from_millis(100), route_table(ROUTES)),
            no_jitter(),
        );
        let cache = Arc::new(cache);
        let key = candidates("a.test");
        let first = tokio::spawn({
            let cache = Arc::clone(&cache);
            let key = key.clone();
            async move { cache.lookup(&key).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.clear();
        assert!(first.await.unwrap().is_ok());
        cache.lookup(&key).await.unwrap();
        assert_eq!(cache.inner().calls(), 2);
    }
}
