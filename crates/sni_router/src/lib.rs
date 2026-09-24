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

//! Route TLS connections to backends by SNI without terminating TLS.
//!
//! The router peeks the ClientHello, resolves its SNI through a pluggable
//! [`RouteLookup`], replays the buffered bytes to the chosen backend, and
//! splices the two TCP streams. Embedders supply the lookup (a file, a
//! database, a service) and a [`MetricsSink`]; the router owns parsing,
//! matching policy, timeouts, and shutdown.

pub mod cache;
pub mod delivery;
pub mod lookup;
pub mod metrics;
pub mod protocol;
pub mod routing;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

pub use cache::{CacheConfig, CachedLookup};
pub use delivery::{Router, RouterConfig};
pub use lookup::{
    Backend, BackendHost, Hostname, InMemoryLookup, LookupError, LookupFuture, NameError,
    RouteCandidates, RouteHits, RouteKey, RouteLookup,
};
pub use metrics::{ListenerInfo, MetricEvent, MetricsSink, NoopMetrics};
