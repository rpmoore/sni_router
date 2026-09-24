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

use std::io;
use std::process::ExitCode;

use sni_router_app::BoundApp;
use sni_router_app::config::{AppConfig, config_path_from_env};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON logs on stdout; level from `RUST_LOG` (default `info`).
fn init_logging() {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(io::stdout)
        .init();
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("version") {
        println!("sni_router {VERSION}");
        return ExitCode::SUCCESS;
    }
    init_logging();

    let path = config_path_from_env();
    let config = match AppConfig::load(&path) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "invalid config");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(path = %path.display(), version = VERSION, "loaded config");

    let app = match BoundApp::bind(config, path).await {
        Ok(app) => app,
        Err(error) => {
            tracing::error!(%error, "startup failed");
            return ExitCode::FAILURE;
        }
    };

    let shutdown = CancellationToken::new();
    tokio::spawn(cancel_on_signal(shutdown.clone()));
    match app.run(shutdown).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "sni_router exited with an error");
            ExitCode::FAILURE
        }
    }
}

/// Cancels `shutdown` on SIGINT or SIGTERM.
async fn cancel_on_signal(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(terminate) => terminate,
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
                shutdown.cancel();
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received; shutting down"),
            _ = terminate.recv() => tracing::info!("SIGTERM received; shutting down"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    shutdown.cancel();
}
