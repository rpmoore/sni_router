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

//! The route-lookup contract between the router and whatever stores routes.
//!
//! This is the extension point for embedders: implement [`RouteLookup`]
//! against any backing store (a TOML file, a database, a service) and hand it
//! to [`Router`](crate::delivery::Router).

mod memory;
mod types;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use memory::InMemoryLookup;
pub use types::{Backend, BackendHost, Hostname, NameError, RouteKey};

/// The future a [`RouteLookup`] returns. Boxed so the trait stays usable as
/// `Arc<dyn RouteLookup>`.
pub type LookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RouteHits, LookupError>> + Send + 'a>>;

/// Resolves route keys to backends.
///
/// # Contract
///
/// - Return every stored route whose key is *literally* one of `candidates`
///   — e.g. `SELECT dns_name, backend FROM routes WHERE dns_name IN ($1, $2)`.
///   Wildcard rows are stored as literal `*.example.com` keys.
/// - Do no wildcard expansion and no precedence: the router generates the
///   candidates and picks the winner (exact beats wildcard).
/// - "No route" is an empty [`RouteHits`], not an error. Reserve `Err` for
///   transient failures (store unreachable, query timeout); the router fails
///   the connection closed on `Err` rather than guessing.
/// - The router bounds each call with its lookup timeout and drops the future
///   on expiry, so implementations must be cancel-safe.
pub trait RouteLookup: Send + Sync {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a>;
}

impl<T: RouteLookup + ?Sized> RouteLookup for Arc<T> {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a> {
        (**self).lookup(candidates)
    }
}

/// The ordered keys the router will accept for one SNI hostname: the exact
/// key, then (when the name has at least three labels) its wildcard key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteCandidates {
    exact: RouteKey,
    wildcard: Option<RouteKey>,
}

impl RouteCandidates {
    pub fn for_hostname(host: &Hostname) -> Self {
        Self {
            exact: RouteKey::exact(host),
            wildcard: RouteKey::wildcard_for(host),
        }
    }

    pub fn exact(&self) -> &RouteKey {
        &self.exact
    }

    pub fn wildcard(&self) -> Option<&RouteKey> {
        self.wildcard.as_ref()
    }

    /// Keys in precedence order (exact first).
    pub fn keys(&self) -> impl Iterator<Item = &RouteKey> {
        std::iter::once(&self.exact).chain(self.wildcard.as_ref())
    }

    pub fn contains(&self, key: &RouteKey) -> bool {
        self.keys().any(|candidate| candidate == key)
    }
}

/// Routes a [`RouteLookup`] found for a set of candidates, in any order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteHits(Vec<(RouteKey, Backend)>);

impl RouteHits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, key: RouteKey, backend: Backend) {
        self.0.push((key, backend));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RouteKey, &Backend)> {
        self.0.iter().map(|(key, backend)| (key, backend))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The backend stored under exactly `key`, if present.
    pub fn get(&self, key: &RouteKey) -> Option<&Backend> {
        self.iter()
            .find(|(hit_key, _)| *hit_key == key)
            .map(|(_, backend)| backend)
    }
}

impl FromIterator<(RouteKey, Backend)> for RouteHits {
    fn from_iter<I: IntoIterator<Item = (RouteKey, Backend)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A transient lookup failure. Cheap to clone so one failure can be handed
/// to every caller waiting on the same load.
#[derive(Clone)]
pub struct LookupError(Arc<dyn Error + Send + Sync>);

impl LookupError {
    pub fn new(error: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self(Arc::from(error.into()))
    }
}

impl fmt::Debug for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LookupError").field(&self.0).finish()
    }
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "route lookup failed: {}", self.0)
    }
}

impl Error for LookupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&*self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> Hostname {
        Hostname::parse(name).unwrap()
    }

    #[test]
    fn candidates_list_exact_then_wildcard() {
        let candidates = RouteCandidates::for_hostname(&host("a.example.com"));
        let keys: Vec<_> = candidates.keys().map(RouteKey::as_str).collect();
        assert_eq!(keys, ["a.example.com", "*.example.com"]);
    }

    #[test]
    fn candidates_for_two_label_name_have_no_wildcard() {
        let candidates = RouteCandidates::for_hostname(&host("example.com"));
        assert_eq!(candidates.keys().count(), 1);
        assert!(candidates.wildcard().is_none());
    }

    #[test]
    fn lookup_error_exposes_source() {
        let error = LookupError::new("db down");
        assert_eq!(error.to_string(), "route lookup failed: db down");
        assert!(error.source().is_some());
    }
}
