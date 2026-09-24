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

//! SIGHUP route reload: re-read the config file, validate all of it, and
//! swap in the new `[[routes]]` only if everything is valid.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{AdminConfig, AppConfig, ConfigError, FileRouteLookup, ServerConfig};

/// The settings the process started with. Only routes reload; differences
/// here are reported and ignored.
#[derive(Clone, Debug)]
pub struct StartupSettings {
    pub server: ServerConfig,
    pub admin: AdminConfig,
}

/// What a successful reload did.
#[derive(Debug, PartialEq, Eq)]
pub struct ReloadOutcome {
    pub routes: usize,
    /// `[server]` or `[admin]` changed on disk; those need a restart.
    pub ignored_restart_only_changes: bool,
}

/// Loads `path` and, if the whole file is valid, replaces the routes in
/// `lookup`. On any error the current routes stay in place.
pub fn reload_from_path(
    path: &Path,
    startup: &StartupSettings,
    lookup: &FileRouteLookup,
) -> Result<ReloadOutcome, ConfigError> {
    let config = AppConfig::load(path)?;
    let ignored_restart_only_changes =
        config.server != startup.server || config.admin != startup.admin;
    let routes = config.routes.len();
    lookup.replace(config.routes);
    Ok(ReloadOutcome {
        routes,
        ignored_restart_only_changes,
    })
}

/// Reloads routes from `path` on every SIGHUP until `shutdown` fires.
#[cfg(unix)]
pub fn spawn_sighup_reload(
    path: PathBuf,
    startup: StartupSettings,
    lookup: Arc<FileRouteLookup>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(hangup) => hangup,
            Err(error) => {
                tracing::error!(%error, "failed to install SIGHUP handler; route reload disabled");
                return;
            }
        };
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                received = hangup.recv() => if received.is_none() { return },
            }
            let (path, startup, lookup) = (path.clone(), startup.clone(), Arc::clone(&lookup));
            let reloaded = tokio::task::spawn_blocking(move || {
                let result = reload_from_path(&path, &startup, &lookup);
                (path, result)
            })
            .await;
            match reloaded {
                Ok((path, Ok(outcome))) => {
                    if outcome.ignored_restart_only_changes {
                        tracing::warn!(
                            path = %path.display(),
                            "[server] or [admin] changed; those settings need a restart and were not applied"
                        );
                    }
                    tracing::info!(path = %path.display(), routes = outcome.routes, "reloaded routes");
                }
                Ok((path, Err(error))) => tracing::error!(
                    path = %path.display(),
                    %error,
                    "route reload failed; keeping current routes"
                ),
                Err(error) => {
                    tracing::error!(%error, "route reload task failed; keeping current routes")
                }
            }
        }
    })
}

#[cfg(not(unix))]
pub fn spawn_sighup_reload(
    _path: PathBuf,
    _startup: StartupSettings,
    _lookup: Arc<FileRouteLookup>,
    _shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use sni_router::{Hostname, RouteCandidates, RouteLookup};

    struct TempConfig(PathBuf);

    impl TempConfig {
        fn new(name: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sni_router_reload_{}_{name}.toml",
                std::process::id()
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }

        fn write(&self, contents: &str) {
            std::fs::write(&self.0, contents).unwrap();
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    const V1: &str = "[[routes]]\nhostname = \"a.test\"\nbackend = \"old:1\"\n";
    const V2: &str = "[[routes]]\nhostname = \"a.test\"\nbackend = \"new:1\"\n[[routes]]\nhostname = \"b.test\"\nbackend = \"b:1\"\n";

    fn setup(file: &TempConfig) -> (StartupSettings, FileRouteLookup) {
        let config = AppConfig::load(&file.0).unwrap();
        let startup = StartupSettings {
            server: config.server,
            admin: config.admin,
        };
        (startup, FileRouteLookup::new(config.routes))
    }

    async fn backend_for(lookup: &FileRouteLookup, name: &str) -> Option<String> {
        let candidates = RouteCandidates::for_hostname(&Hostname::parse(name).unwrap());
        let hits = lookup.lookup(&candidates).await.unwrap();
        hits.iter().next().map(|(_, backend)| backend.to_string())
    }

    #[tokio::test]
    async fn valid_file_swaps_routes() {
        let file = TempConfig::new("valid", V1);
        let (startup, lookup) = setup(&file);
        file.write(V2);
        let outcome = reload_from_path(&file.0, &startup, &lookup).unwrap();
        assert_eq!(
            outcome,
            ReloadOutcome {
                routes: 2,
                ignored_restart_only_changes: false
            }
        );
        assert_eq!(
            backend_for(&lookup, "a.test").await.as_deref(),
            Some("new:1")
        );
        assert_eq!(backend_for(&lookup, "b.test").await.as_deref(), Some("b:1"));
    }

    #[tokio::test]
    async fn invalid_file_keeps_current_routes() {
        let file = TempConfig::new("invalid", V1);
        let (startup, lookup) = setup(&file);
        file.write("[[routes]]\nhostname = \"a.test\"\nbackend = \"no-port\"\n");
        assert!(reload_from_path(&file.0, &startup, &lookup).is_err());
        file.write("not toml [");
        assert!(reload_from_path(&file.0, &startup, &lookup).is_err());
        assert_eq!(
            backend_for(&lookup, "a.test").await.as_deref(),
            Some("old:1")
        );
    }

    #[tokio::test]
    async fn missing_file_keeps_current_routes() {
        let file = TempConfig::new("missing", V1);
        let (startup, lookup) = setup(&file);
        std::fs::remove_file(&file.0).unwrap();
        assert!(reload_from_path(&file.0, &startup, &lookup).is_err());
        assert_eq!(
            backend_for(&lookup, "a.test").await.as_deref(),
            Some("old:1")
        );
    }

    #[tokio::test]
    async fn server_changes_are_reported_not_applied() {
        let file = TempConfig::new("server", V1);
        let (startup, lookup) = setup(&file);
        file.write(&format!("[server]\nmax_connections = 5\n{V2}"));
        let outcome = reload_from_path(&file.0, &startup, &lookup).unwrap();
        assert!(outcome.ignored_restart_only_changes);
        assert_eq!(
            backend_for(&lookup, "a.test").await.as_deref(),
            Some("new:1")
        );
    }
}
