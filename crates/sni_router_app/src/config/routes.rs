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

use std::sync::{Arc, RwLock};

use serde::Deserialize;
use sni_router::{Backend, InMemoryLookup, LookupFuture, RouteCandidates, RouteKey, RouteLookup};

use super::ConfigError;

/// One `[[routes]]` entry as written in the file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRoute {
    hostname: String,
    backend: String,
}

pub(super) fn build_route_table(routes: &[RawRoute]) -> Result<InMemoryLookup, ConfigError> {
    let mut table = InMemoryLookup::new();
    for (index, route) in routes.iter().enumerate() {
        let key = RouteKey::parse(&route.hostname).map_err(|error| {
            ConfigError::InvalidRouteHostname {
                index,
                hostname: route.hostname.clone(),
                error,
            }
        })?;
        let backend: Backend =
            route
                .backend
                .parse()
                .map_err(|error| ConfigError::InvalidRouteBackend {
                    index,
                    backend: route.backend.clone(),
                    error,
                })?;
        if table.insert(key.clone(), backend).is_some() {
            return Err(ConfigError::DuplicateRoute(key.to_string()));
        }
    }
    Ok(table)
}

/// The route table loaded from the config file, swappable at runtime.
///
/// Lookups clone the current table's `Arc` under a brief read lock and
/// answer from that snapshot, so a reload never blocks or tears an
/// in-progress lookup. Connections already proxying hold their resolved
/// backend and are unaffected by a swap.
#[derive(Debug)]
pub struct FileRouteLookup {
    table: RwLock<Arc<InMemoryLookup>>,
}

impl FileRouteLookup {
    pub fn new(table: InMemoryLookup) -> Self {
        Self {
            table: RwLock::new(Arc::new(table)),
        }
    }

    /// Atomically replaces the route table.
    pub fn replace(&self, table: InMemoryLookup) {
        *self
            .table
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(table);
    }

    pub fn snapshot(&self) -> Arc<InMemoryLookup> {
        Arc::clone(
            &self
                .table
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl RouteLookup for FileRouteLookup {
    fn lookup<'a>(&'a self, candidates: &'a RouteCandidates) -> LookupFuture<'a> {
        let hits = self.snapshot().hits(candidates);
        Box::pin(std::future::ready(Ok(hits)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sni_router::Hostname;
    use sni_router::testing::route_table;

    #[tokio::test]
    async fn replace_swaps_the_table_for_new_lookups() {
        let lookup = FileRouteLookup::new(route_table(&[("a.test", "old:1")]));
        let candidates = RouteCandidates::for_hostname(&Hostname::parse("a.test").unwrap());
        let before = lookup.lookup(&candidates).await.unwrap();
        lookup.replace(route_table(&[("a.test", "new:1")]));
        let after = lookup.lookup(&candidates).await.unwrap();
        assert_eq!(before.iter().next().unwrap().1.to_string(), "old:1");
        assert_eq!(after.iter().next().unwrap().1.to_string(), "new:1");
    }
}
