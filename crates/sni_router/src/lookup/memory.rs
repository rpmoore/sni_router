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

use std::collections::HashMap;

use super::{Backend, LookupFuture, RouteCandidates, RouteHits, RouteKey, RouteLookup};

/// A fixed route table held in memory. Used by the TOML app (one table per
/// config generation) and by tests.
#[derive(Clone, Debug, Default)]
pub struct InMemoryLookup {
    routes: HashMap<RouteKey, Backend>,
}

impl InMemoryLookup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the route for `key`, returning the previous backend.
    pub fn insert(&mut self, key: RouteKey, backend: Backend) -> Option<Backend> {
        self.routes.insert(key, backend)
    }

    pub fn get(&self, key: &RouteKey) -> Option<&Backend> {
        self.routes.get(key)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Synchronous form of [`RouteLookup::lookup`].
    pub fn hits(&self, candidates: &RouteCandidates) -> RouteHits {
        candidates
            .keys()
            .filter_map(|key| Some((key.clone(), self.routes.get(key)?.clone())))
            .collect()
    }
}

impl FromIterator<(RouteKey, Backend)> for InMemoryLookup {
    fn from_iter<I: IntoIterator<Item = (RouteKey, Backend)>>(iter: I) -> Self {
        Self {
            routes: iter.into_iter().collect(),
        }
    }
}

impl RouteLookup for InMemoryLookup {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a> {
        let hits = self.hits(candidates);
        Box::pin(async move { Ok(hits) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::Hostname;

    fn table() -> InMemoryLookup {
        [
            ("api.example.com", "api:443"),
            ("*.example.com", "wild:443"),
            ("other.test", "other:443"),
        ]
        .into_iter()
        .map(|(key, backend)| (RouteKey::parse(key).unwrap(), backend.parse().unwrap()))
        .collect()
    }

    #[tokio::test]
    async fn returns_hits_for_every_matching_candidate() {
        let candidates =
            RouteCandidates::for_hostname(&Hostname::parse("api.example.com").unwrap());
        let hits = table().lookup(&candidates).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits.get(candidates.exact()).unwrap().to_string(), "api:443");
    }

    #[tokio::test]
    async fn returns_empty_hits_when_nothing_matches() {
        let candidates = RouteCandidates::for_hostname(&Hostname::parse("nope.test").unwrap());
        assert!(table().lookup(&candidates).await.unwrap().is_empty());
    }
}
