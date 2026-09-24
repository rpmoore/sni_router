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

//! The TOML config file: parsing, defaults, and validation. Nothing here
//! opens sockets; a validated [`AppConfig`] is handed to the app wiring.

mod routes;

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sni_router::protocol::HelloLimits;
use sni_router::{InMemoryLookup, NameError, RouterConfig};

pub use routes::FileRouteLookup;

/// Env var naming the config file.
pub const CONFIG_PATH_ENV_VAR: &str = "SNI_ROUTER_CONFIG";
/// Config file used when the env var is unset or blank.
pub const DEFAULT_CONFIG_PATH: &str = "sni_router.toml";

// Upper bounds keep every duration and size representable and sane; an
// absurd value is almost certainly a typo, and some (an idle timeout near
// u64::MAX seconds) would otherwise overflow time arithmetic.
const MAX_CONNECTIONS: u64 = 1_000_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000; // 1 hour
const MAX_IDLE_TIMEOUT_SECS: u64 = 30 * 24 * 3600; // 30 days
const MAX_SHUTDOWN_GRACE_SECS: u64 = 3600;
const MIN_CLIENT_HELLO_BYTES: u64 = 1024;
const MAX_CLIENT_HELLO_BYTES: u64 = 65_536;
const MAX_HELLO_RECORDS: u64 = 256;
const MAX_DNS_CACHE_TTL_SECS: u64 = 24 * 3600;
// Tokio's blocking pool defaults to 512 threads; keep DNS well under it.
const MAX_CONCURRENT_DNS_LOOKUPS: u64 = 256;
const MIN_COPY_BUFFER_BYTES: u64 = 1024;
const MAX_COPY_BUFFER_BYTES: u64 = 1024 * 1024;

/// A validated config file.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub admin: AdminConfig,
    pub routes: InMemoryLookup,
}

/// `[server]`: data-plane listeners and router limits. Fixed at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub listen: Vec<SocketAddr>,
    pub router: RouterConfig,
}

/// `[admin]`: the HTTP listener for metrics and health. Fixed at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminConfig {
    pub listen: SocketAddr,
    pub health_path: String,
}

/// Resolves the config path from [`CONFIG_PATH_ENV_VAR`], falling back to
/// [`DEFAULT_CONFIG_PATH`] when it's unset or blank.
pub fn config_path_from_env() -> PathBuf {
    std::env::var_os(CONFIG_PATH_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(|error| ConfigError::Read {
            path: path.to_path_buf(),
            error,
        })?;
        Self::from_toml_str(&source)
    }

    pub fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(source).map_err(ConfigError::Parse)?;
        Ok(Self {
            server: raw.server.validate()?,
            admin: raw.admin.validate()?,
            routes: routes::build_route_table(&raw.routes)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    admin: RawAdmin,
    #[serde(default)]
    routes: Vec<routes::RawRoute>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawServer {
    listen: Vec<SocketAddr>,
    max_connections: usize,
    client_hello_timeout_ms: u64,
    lookup_timeout_ms: u64,
    upstream_timeout_ms: u64,
    /// Default 1800 (30 minutes); 0 disables the idle timeout.
    idle_timeout_secs: u64,
    shutdown_grace_secs: u64,
    max_client_hello_bytes: usize,
    max_client_hello_records: usize,
    /// 0 resolves DNS backends on every connection.
    dns_cache_ttl_secs: u64,
    max_concurrent_dns_lookups: usize,
    copy_buffer_bytes: usize,
}

impl Default for RawServer {
    fn default() -> Self {
        let router = RouterConfig::default();
        Self {
            listen: vec![SocketAddr::from(([0, 0, 0, 0], 8080))],
            max_connections: router.max_connections,
            client_hello_timeout_ms: millis(router.client_hello_timeout),
            lookup_timeout_ms: millis(router.lookup_timeout),
            upstream_timeout_ms: millis(router.upstream_timeout),
            idle_timeout_secs: router.idle_timeout.map_or(0, |idle| idle.as_secs()),
            shutdown_grace_secs: router.shutdown_grace.as_secs(),
            max_client_hello_bytes: router.hello_limits.max_handshake_bytes,
            max_client_hello_records: router.hello_limits.max_records,
            dns_cache_ttl_secs: router.dns_cache_ttl.as_secs(),
            max_concurrent_dns_lookups: router.max_concurrent_dns_lookups,
            copy_buffer_bytes: router.copy_buffer_size,
        }
    }
}

impl RawServer {
    fn validate(self) -> Result<ServerConfig, ConfigError> {
        if self.listen.is_empty() {
            return Err(ConfigError::NoListenAddresses);
        }
        let mut seen = HashSet::new();
        if let Some(duplicate) = self.listen.iter().find(|addr| !seen.insert(**addr)) {
            return Err(ConfigError::DuplicateListenAddress(*duplicate));
        }
        let max_connections = in_range(
            self.max_connections as u64,
            1,
            MAX_CONNECTIONS,
            "server.max_connections",
        )?;
        let timeout_ms =
            |value, field| in_range(value, 1, MAX_TIMEOUT_MS, field).map(Duration::from_millis);
        let client_hello_timeout = timeout_ms(
            self.client_hello_timeout_ms,
            "server.client_hello_timeout_ms",
        )?;
        let lookup_timeout = timeout_ms(self.lookup_timeout_ms, "server.lookup_timeout_ms")?;
        let upstream_timeout = timeout_ms(self.upstream_timeout_ms, "server.upstream_timeout_ms")?;
        let idle_timeout_secs = in_range(
            self.idle_timeout_secs,
            0,
            MAX_IDLE_TIMEOUT_SECS,
            "server.idle_timeout_secs",
        )?;
        let shutdown_grace_secs = in_range(
            self.shutdown_grace_secs,
            0,
            MAX_SHUTDOWN_GRACE_SECS,
            "server.shutdown_grace_secs",
        )?;
        let max_client_hello_bytes = in_range(
            self.max_client_hello_bytes as u64,
            MIN_CLIENT_HELLO_BYTES,
            MAX_CLIENT_HELLO_BYTES,
            "server.max_client_hello_bytes",
        )?;
        let max_client_hello_records = in_range(
            self.max_client_hello_records as u64,
            1,
            MAX_HELLO_RECORDS,
            "server.max_client_hello_records",
        )?;
        let dns_cache_ttl_secs = in_range(
            self.dns_cache_ttl_secs,
            0,
            MAX_DNS_CACHE_TTL_SECS,
            "server.dns_cache_ttl_secs",
        )?;
        let max_concurrent_dns_lookups = in_range(
            self.max_concurrent_dns_lookups as u64,
            1,
            MAX_CONCURRENT_DNS_LOOKUPS,
            "server.max_concurrent_dns_lookups",
        )?;
        let copy_buffer_bytes = in_range(
            self.copy_buffer_bytes as u64,
            MIN_COPY_BUFFER_BYTES,
            MAX_COPY_BUFFER_BYTES,
            "server.copy_buffer_bytes",
        )?;

        // Everything below was range-checked above, so the casts are exact.
        let mut router = RouterConfig::default();
        router.client_hello_timeout = client_hello_timeout;
        router.lookup_timeout = lookup_timeout;
        router.upstream_timeout = upstream_timeout;
        router.idle_timeout =
            (idle_timeout_secs > 0).then(|| Duration::from_secs(idle_timeout_secs));
        router.shutdown_grace = Duration::from_secs(shutdown_grace_secs);
        router.max_connections = max_connections as usize;
        router.hello_limits = HelloLimits::new(
            max_client_hello_bytes as usize,
            max_client_hello_records as usize,
        );
        router.dns_cache_ttl = Duration::from_secs(dns_cache_ttl_secs);
        router.max_concurrent_dns_lookups = max_concurrent_dns_lookups as usize;
        router.copy_buffer_size = copy_buffer_bytes as usize;
        Ok(ServerConfig {
            listen: self.listen,
            router,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawAdmin {
    listen: SocketAddr,
    health_path: String,
}

impl Default for RawAdmin {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8081)),
            health_path: "/health".to_owned(),
        }
    }
}

impl RawAdmin {
    fn validate(self) -> Result<AdminConfig, ConfigError> {
        let path = &self.health_path;
        if !path.starts_with('/') || path == "/metrics" || path.contains(char::is_whitespace) {
            return Err(ConfigError::InvalidHealthPath(self.health_path));
        }
        Ok(AdminConfig {
            listen: self.listen,
            health_path: self.health_path,
        })
    }
}

fn in_range(value: u64, min: u64, max: u64, field: &'static str) -> Result<u64, ConfigError> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(ConfigError::OutOfRange { field, min, max })
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Why a config file was rejected.
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        error: io::Error,
    },
    Parse(toml::de::Error),
    NoListenAddresses,
    DuplicateListenAddress(SocketAddr),
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
    },
    InvalidHealthPath(String),
    InvalidRouteHostname {
        index: usize,
        hostname: String,
        error: NameError,
    },
    InvalidRouteBackend {
        index: usize,
        backend: String,
        error: NameError,
    },
    DuplicateRoute(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, error } => {
                write!(f, "failed to read {}: {error}", path.display())
            }
            ConfigError::Parse(error) => write!(f, "invalid TOML: {error}"),
            ConfigError::NoListenAddresses => f.write_str("server.listen must not be empty"),
            ConfigError::DuplicateListenAddress(addr) => {
                write!(f, "server.listen contains {addr} more than once")
            }
            ConfigError::OutOfRange { field, min, max } => {
                write!(f, "{field} must be between {min} and {max}")
            }
            ConfigError::InvalidHealthPath(path) => write!(
                f,
                "admin.health_path {path:?} must start with `/`, contain no whitespace, and not be /metrics"
            ),
            ConfigError::InvalidRouteHostname {
                index,
                hostname,
                error,
            } => write!(f, "routes[{index}].hostname {hostname:?}: {error}"),
            ConfigError::InvalidRouteBackend {
                index,
                backend,
                error,
            } => write!(f, "routes[{index}].backend {backend:?}: {error}"),
            ConfigError::DuplicateRoute(hostname) => {
                write!(f, "route hostname {hostname:?} appears more than once")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { error, .. } => Some(error),
            ConfigError::Parse(error) => Some(error),
            ConfigError::InvalidRouteHostname { error, .. }
            | ConfigError::InvalidRouteBackend { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sni_router::{Hostname, RouteCandidates};

    const EXAMPLE: &str = include_str!("../../../../examples/sni_router.toml");

    fn parse(source: &str) -> Result<AppConfig, ConfigError> {
        AppConfig::from_toml_str(source)
    }

    #[test]
    fn example_config_is_valid() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(config.server.listen, ["0.0.0.0:8080".parse().unwrap()]);
        assert_eq!(config.admin.health_path, "/health");
        assert!(!config.routes.is_empty());
    }

    #[test]
    fn empty_file_uses_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.server.listen, ["0.0.0.0:8080".parse().unwrap()]);
        assert_eq!(config.server.router, RouterConfig::default());
        assert_eq!(config.admin.listen, "127.0.0.1:8081".parse().unwrap());
        assert!(config.routes.is_empty());
    }

    #[test]
    fn idle_timeout_defaults_to_30_minutes_and_zero_disables_it() {
        let config = parse("").unwrap();
        assert_eq!(
            config.server.router.idle_timeout,
            Some(Duration::from_secs(1800))
        );
        let config = parse("[server]\nidle_timeout_secs = 0").unwrap();
        assert_eq!(config.server.router.idle_timeout, None);
        let config = parse("[server]\nidle_timeout_secs = 45").unwrap();
        assert_eq!(
            config.server.router.idle_timeout,
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn copy_buffer_defaults_to_32_kib() {
        assert_eq!(parse("").unwrap().server.router.copy_buffer_size, 32 * 1024);
    }

    #[test]
    fn server_settings_map_onto_router_config() {
        let config = parse(
            r#"
            [server]
            listen = ["127.0.0.1:9443", "[::1]:9443"]
            max_connections = 7
            client_hello_timeout_ms = 1500
            lookup_timeout_ms = 250
            upstream_timeout_ms = 3000
            idle_timeout_secs = 600
            shutdown_grace_secs = 0
            max_client_hello_bytes = 4096
            max_client_hello_records = 4
            dns_cache_ttl_secs = 0
            max_concurrent_dns_lookups = 8
            copy_buffer_bytes = 65536
            "#,
        )
        .unwrap();
        let router = &config.server.router;
        assert_eq!(config.server.listen.len(), 2);
        assert_eq!(router.max_connections, 7);
        assert_eq!(router.client_hello_timeout, Duration::from_millis(1500));
        assert_eq!(router.lookup_timeout, Duration::from_millis(250));
        assert_eq!(router.upstream_timeout, Duration::from_secs(3));
        assert_eq!(router.idle_timeout, Some(Duration::from_secs(600)));
        assert_eq!(router.shutdown_grace, Duration::ZERO);
        assert_eq!(router.hello_limits, HelloLimits::new(4096, 4));
        assert_eq!(router.dns_cache_ttl, Duration::ZERO);
        assert_eq!(router.max_concurrent_dns_lookups, 8);
        assert_eq!(router.copy_buffer_size, 65536);
    }

    #[test]
    fn routes_are_normalized_and_matchable() {
        let config = parse(
            r#"
            [[routes]]
            hostname = "API.Example.com"
            backend = "API-svc:8443"
            [[routes]]
            hostname = "*.apps.example.com"
            backend = "10.0.0.5:443"
            "#,
        )
        .unwrap();
        let hits = config.routes.hits(&RouteCandidates::for_hostname(
            &Hostname::parse("x.apps.example.com").unwrap(),
        ));
        assert_eq!(hits.len(), 1);
        let hits = config.routes.hits(&RouteCandidates::for_hostname(
            &Hostname::parse("api.example.com").unwrap(),
        ));
        assert_eq!(hits.iter().next().unwrap().1.to_string(), "api-svc:8443");
    }

    #[test]
    fn rejects_invalid_values() {
        let cases: &[(&str, &str)] = &[
            ("[server]\nlisten = []", "server.listen must not be empty"),
            (
                "[server]\nlisten = [\"127.0.0.1:1\", \"127.0.0.1:1\"]",
                "more than once",
            ),
            ("[server]\nmax_connections = 0", "server.max_connections"),
            (
                "[server]\nlookup_timeout_ms = 0",
                "server.lookup_timeout_ms",
            ),
            (
                "[server]\nclient_hello_timeout_ms = 0",
                "server.client_hello_timeout_ms",
            ),
            (
                "[server]\nupstream_timeout_ms = 0",
                "server.upstream_timeout_ms",
            ),
            (
                "[server]\nmax_client_hello_bytes = 512",
                "between 1024 and 65536",
            ),
            (
                "[server]\nmax_client_hello_bytes = 70000",
                "between 1024 and 65536",
            ),
            (
                "[server]\nmax_client_hello_records = 0",
                "max_client_hello_records",
            ),
            (
                "[server]\nidle_timeout_secs = 18446744073709551615",
                "server.idle_timeout_secs",
            ),
            (
                "[server]\nlookup_timeout_ms = 3600001",
                "server.lookup_timeout_ms",
            ),
            (
                "[server]\nshutdown_grace_secs = 99999",
                "server.shutdown_grace_secs",
            ),
            (
                "[server]\nmax_connections = 2000000",
                "server.max_connections",
            ),
            (
                "[server]\ndns_cache_ttl_secs = 100000",
                "server.dns_cache_ttl_secs",
            ),
            (
                "[server]\nmax_concurrent_dns_lookups = 0",
                "server.max_concurrent_dns_lookups",
            ),
            (
                "[server]\nmax_concurrent_dns_lookups = 1000",
                "server.max_concurrent_dns_lookups",
            ),
            (
                "[server]\ncopy_buffer_bytes = 10",
                "server.copy_buffer_bytes",
            ),
            ("[admin]\nhealth_path = \"health\"", "admin.health_path"),
            ("[admin]\nhealth_path = \"/metrics\"", "admin.health_path"),
            ("[server]\nbogus = 1", "unknown field"),
            ("bogus = 1", "unknown field"),
            (
                "[[routes]]\nhostname = \"*.com\"\nbackend = \"a:1\"",
                "routes[0].hostname",
            ),
            (
                "[[routes]]\nhostname = \"a.test.\"\nbackend = \"a:1\"",
                "routes[0].hostname",
            ),
            (
                "[[routes]]\nhostname = \"a.test\"\nbackend = \"a\"",
                "routes[0].backend",
            ),
            (
                "[[routes]]\nhostname = \"a.test\"\nbackend = \"a:0\"",
                "routes[0].backend",
            ),
            (
                "[[routes]]\nhostname = \"a.test\"\nbackend = \"a:1\"\n[[routes]]\nhostname = \"A.TEST\"\nbackend = \"b:1\"",
                "appears more than once",
            ),
            (
                "[[routes]]\nhostname = \"a.test\"\nbackend = \"a:1\"\nextra = true",
                "unknown field",
            ),
        ];
        for (source, expected) in cases {
            let error = parse(source).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{source:?}: expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn missing_file_reports_its_path() {
        let error = AppConfig::load(Path::new("/nonexistent/sni_router.toml")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/sni_router.toml"));
    }
}
