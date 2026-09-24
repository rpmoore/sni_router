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

//! Routing policy: which keys a hostname may match and which hit wins.

use std::fmt;
use std::time::Duration;

use crate::lookup::{
    Backend, Hostname, LookupError, RouteCandidates, RouteHits, RouteKey, RouteLookup,
};

/// The route chosen for a hostname.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteMatch {
    key: RouteKey,
    backend: Backend,
}

impl RouteMatch {
    pub fn key(&self) -> &RouteKey {
        &self.key
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }
}

/// Why no route was chosen. Every variant fails the connection closed.
#[derive(Debug)]
pub enum RouteError {
    NotFound,
    Lookup(LookupError),
    Timeout,
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::NotFound => f.write_str("no route for hostname"),
            RouteError::Lookup(error) => write!(f, "{error}"),
            RouteError::Timeout => f.write_str("route lookup timed out"),
        }
    }
}

impl std::error::Error for RouteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RouteError::Lookup(error) => Some(error),
            _ => None,
        }
    }
}

/// The keys `host` may match, in precedence order.
pub fn candidate_keys(host: &Hostname) -> RouteCandidates {
    RouteCandidates::for_hostname(host)
}

/// Picks the highest-precedence hit (exact over wildcard). Hits for keys that
/// weren't candidates are a lookup contract violation; they're ignored and
/// counted so the caller can report them.
pub fn select_route(candidates: &RouteCandidates, hits: &RouteHits) -> Selection {
    let ignored_hits = hits
        .iter()
        .filter(|(key, _)| !candidates.contains(key))
        .count();
    let matched = candidates.keys().find_map(|key| {
        hits.get(key).map(|backend| RouteMatch {
            key: key.clone(),
            backend: backend.clone(),
        })
    });
    Selection {
        matched,
        ignored_hits,
    }
}

/// The outcome of [`select_route`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub matched: Option<RouteMatch>,
    pub ignored_hits: usize,
}

/// Resolves `host` with one lookup call bounded by `timeout`. A lookup error
/// or timeout fails closed; there's no fallthrough to a partial answer.
pub async fn resolve_route(
    lookup: &dyn RouteLookup,
    host: &Hostname,
    timeout: Duration,
) -> Result<RouteMatch, RouteError> {
    let candidates = candidate_keys(host);
    let hits = match tokio::time::timeout(timeout, lookup.lookup(&candidates)).await {
        Ok(Ok(hits)) => hits,
        Ok(Err(error)) => return Err(RouteError::Lookup(error)),
        Err(_) => return Err(RouteError::Timeout),
    };
    let selection = select_route(&candidates, &hits);
    if selection.ignored_hits > 0 {
        tracing::warn!(
            hostname = %host,
            ignored_hits = selection.ignored_hits,
            "route lookup returned hits for keys that were not candidates; ignoring them"
        );
    }
    selection.matched.ok_or(RouteError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::InMemoryLookup;
    use crate::testing::ScriptedLookup;

    fn host(name: &str) -> Hostname {
        Hostname::parse(name).unwrap()
    }

    fn table(entries: &[(&str, &str)]) -> InMemoryLookup {
        entries
            .iter()
            .map(|(key, backend)| (RouteKey::parse(key).unwrap(), backend.parse().unwrap()))
            .collect()
    }

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn exact_match_beats_wildcard() {
        let lookup = table(&[("*.example.com", "wild:1"), ("api.example.com", "exact:1")]);
        let route = resolve_route(&lookup, &host("api.example.com"), TIMEOUT)
            .await
            .unwrap();
        assert_eq!(route.backend().to_string(), "exact:1");
        assert!(!route.key().is_wildcard());
    }

    #[tokio::test]
    async fn wildcard_matches_one_label_only() {
        let lookup = table(&[("*.example.com", "wild:1")]);
        assert!(
            resolve_route(&lookup, &host("a.example.com"), TIMEOUT)
                .await
                .is_ok()
        );
        assert!(matches!(
            resolve_route(&lookup, &host("example.com"), TIMEOUT).await,
            Err(RouteError::NotFound)
        ));
        assert!(matches!(
            resolve_route(&lookup, &host("a.b.example.com"), TIMEOUT).await,
            Err(RouteError::NotFound)
        ));
    }

    #[tokio::test]
    async fn uses_exactly_one_lookup_call() {
        let lookup = ScriptedLookup::found(&[("*.example.com", "wild:1")]);
        resolve_route(&lookup, &host("a.example.com"), TIMEOUT)
            .await
            .unwrap();
        assert_eq!(lookup.calls(), 1);
    }

    #[tokio::test]
    async fn lookup_error_fails_closed() {
        let lookup = ScriptedLookup::failing("db down");
        assert!(matches!(
            resolve_route(&lookup, &host("a.example.com"), TIMEOUT).await,
            Err(RouteError::Lookup(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_lookup_times_out() {
        let lookup = ScriptedLookup::hanging();
        assert!(matches!(
            resolve_route(&lookup, &host("a.example.com"), TIMEOUT).await,
            Err(RouteError::Timeout)
        ));
    }

    #[test]
    fn hits_for_non_candidate_keys_are_ignored() {
        let candidates = candidate_keys(&host("a.example.com"));
        let hits: RouteHits = [
            (
                RouteKey::parse("evil.test").unwrap(),
                "evil:1".parse().unwrap(),
            ),
            (
                RouteKey::parse("*.example.com").unwrap(),
                "wild:1".parse().unwrap(),
            ),
        ]
        .into_iter()
        .collect();
        let selection = select_route(&candidates, &hits);
        assert_eq!(selection.ignored_hits, 1);
        assert_eq!(selection.matched.unwrap().backend().to_string(), "wild:1");
    }

    #[test]
    fn only_non_candidate_hits_means_not_found() {
        let candidates = candidate_keys(&host("a.example.com"));
        let hits: RouteHits = [(
            RouteKey::parse("evil.test").unwrap(),
            "evil:1".parse().unwrap(),
        )]
        .into_iter()
        .collect();
        assert_eq!(select_route(&candidates, &hits).matched, None);
    }
}
