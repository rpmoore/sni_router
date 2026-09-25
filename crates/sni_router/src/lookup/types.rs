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

//! Domain types shared by the lookup contract: validated hostnames, route
//! keys (exact or wildcard), and backend addresses.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::num::NonZeroU16;
use std::str::FromStr;

const MAX_NAME_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;
const WILDCARD_PREFIX: &str = "*.";

/// Why a hostname, route key, or backend string was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NameError {
    Empty,
    TooLong,
    EmptyLabel,
    LabelTooLong,
    InvalidCharacter,
    IpLiteral,
    InvalidWildcard,
    MissingPort,
    InvalidPort,
    InvalidBackendHost,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            NameError::Empty => "name is empty",
            NameError::TooLong => "name is longer than 253 bytes",
            NameError::EmptyLabel => "name has an empty label (leading, trailing, or double dot)",
            NameError::LabelTooLong => "name has a label longer than 63 bytes",
            NameError::InvalidCharacter => "name contains a character outside [a-z0-9-_]",
            NameError::IpLiteral => "name is an IP address literal",
            NameError::InvalidWildcard => {
                "wildcard must be `*.` followed by at least two labels (e.g. `*.example.com`)"
            }
            NameError::MissingPort => "backend is missing a `:port` suffix",
            NameError::InvalidPort => "backend port must be an integer in 1..=65535",
            NameError::InvalidBackendHost => "backend host is not a valid DNS name or IP address",
        };
        f.write_str(message)
    }
}

impl std::error::Error for NameError {}

/// A validated, lowercase DNS hostname as it appears in SNI.
///
/// Every string that reaches a [`RouteLookup`](super::RouteLookup) is derived
/// from one of these, so lookups never see mixed case, trailing dots, or
/// non-ASCII input.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hostname(Box<str>);

impl Hostname {
    /// Validates and lowercases `name`.
    pub fn parse(name: &str) -> Result<Self, NameError> {
        let lowered = name.to_ascii_lowercase();
        validate_dns_name(&lowered)?;
        if lowered.parse::<IpAddr>().is_ok() {
            return Err(NameError::IpLiteral);
        }
        Ok(Self(lowered.into_boxed_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn parent(&self) -> Option<&str> {
        self.0.split_once('.').map(|(_, rest)| rest)
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A key a route is stored under: either an exact hostname
/// (`api.example.com`) or a single-label wildcard (`*.example.com`).
///
/// Lookups match keys literally; wildcard expansion and precedence are the
/// router's job (see [`crate::routing`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteKey {
    text: Box<str>,
    wildcard: bool,
}

impl RouteKey {
    /// Parses a stored route key, e.g. from a config file or database row.
    /// Wildcards must be exactly `*.` followed by at least two labels, so
    /// `*.com` and `a.*.example.com` are rejected.
    pub fn parse(key: &str) -> Result<Self, NameError> {
        let lowered = key.to_ascii_lowercase();
        if let Some(parent) = lowered.strip_prefix(WILDCARD_PREFIX) {
            let parent = Hostname::parse(parent).map_err(|error| match error {
                NameError::InvalidCharacter => NameError::InvalidWildcard,
                other => other,
            })?;
            if !parent.as_str().contains('.') {
                return Err(NameError::InvalidWildcard);
            }
            return Ok(Self {
                text: lowered.into_boxed_str(),
                wildcard: true,
            });
        }
        if lowered.contains('*') {
            return Err(NameError::InvalidWildcard);
        }
        Ok(Self::exact(&Hostname::parse(&lowered)?))
    }

    /// The key that matches exactly `host`.
    pub fn exact(host: &Hostname) -> Self {
        Self {
            text: host.0.clone(),
            wildcard: false,
        }
    }

    /// The wildcard key that covers `host`: `a.example.com` → `*.example.com`.
    /// `None` when the parent would have fewer than two labels (no `*.com`).
    pub fn wildcard_for(host: &Hostname) -> Option<Self> {
        let parent = host.parent()?;
        if !parent.contains('.') {
            return None;
        }
        let mut text = String::with_capacity(WILDCARD_PREFIX.len() + parent.len());
        text.push_str(WILDCARD_PREFIX);
        text.push_str(parent);
        Some(Self {
            text: text.into_boxed_str(),
            wildcard: true,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_wildcard(&self) -> bool {
        self.wildcard
    }
}

impl fmt::Display for RouteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Where a backend lives: a literal IP, or a DNS name resolved at connect
/// time (e.g. a Kubernetes service name).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackendHost {
    Ip(IpAddr),
    Dns(Box<str>),
}

/// A backend TCP endpoint a route forwards to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backend {
    host: BackendHost,
    port: NonZeroU16,
}

impl Backend {
    pub fn new(host: BackendHost, port: NonZeroU16) -> Self {
        Self { host, port }
    }

    pub fn host(&self) -> &BackendHost {
        &self.host
    }

    pub fn port(&self) -> NonZeroU16 {
        self.port
    }
}

impl FromStr for Backend {
    type Err = NameError;

    /// Parses `host:port`, `a.b.c.d:port`, or `[ipv6]:port`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (host, port) = split_host_port(value)?;
        let port = port
            .parse::<u16>()
            .ok()
            .and_then(NonZeroU16::new)
            .ok_or(NameError::InvalidPort)?;
        Ok(Self::new(parse_backend_host(host)?, port))
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            BackendHost::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            BackendHost::Ip(IpAddr::V4(ip)) => write!(f, "{ip}:{}", self.port),
            BackendHost::Dns(name) => write!(f, "{name}:{}", self.port),
        }
    }
}

fn split_host_port(value: &str) -> Result<(&str, &str), NameError> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(NameError::InvalidBackendHost)?;
        let port = after.strip_prefix(':').ok_or(NameError::MissingPort)?;
        host.parse::<Ipv6Addr>()
            .map_err(|_| NameError::InvalidBackendHost)?;
        return Ok((host, port));
    }
    let (host, port) = value.rsplit_once(':').ok_or(NameError::MissingPort)?;
    if host.contains(':') {
        // Bare IPv6 without brackets is ambiguous with the port separator.
        return Err(NameError::InvalidBackendHost);
    }
    Ok((host, port))
}

fn parse_backend_host(host: &str) -> Result<BackendHost, NameError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(BackendHost::Ip(ip));
    }
    let lowered = host.to_ascii_lowercase();
    validate_dns_name(&lowered).map_err(|_| NameError::InvalidBackendHost)?;
    Ok(BackendHost::Dns(lowered.into_boxed_str()))
}

/// Checks length, label structure, and the `[a-z0-9-_]` character set on an
/// already-lowercased name.
fn validate_dns_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err(NameError::EmptyLabel);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(NameError::LabelTooLong);
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(NameError::InvalidCharacter);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_lowercases_and_accepts_valid_names() {
        assert_eq!(
            Hostname::parse("Api.Example.COM").unwrap().as_str(),
            "api.example.com"
        );
        assert!(Hostname::parse("a_b-c.example").is_ok());
        assert!(Hostname::parse("localhost").is_ok());
    }

    #[test]
    fn hostname_rejects_malformed_names() {
        let cases = [
            ("", NameError::Empty),
            ("example.com.", NameError::EmptyLabel),
            (".example.com", NameError::EmptyLabel),
            ("a..b", NameError::EmptyLabel),
            ("*.example.com", NameError::InvalidCharacter),
            ("exa mple.com", NameError::InvalidCharacter),
            ("exämple.com", NameError::InvalidCharacter),
            ("10.0.0.1", NameError::IpLiteral),
        ];
        for (input, expected) in cases {
            assert_eq!(Hostname::parse(input), Err(expected), "input {input:?}");
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert_eq!(Hostname::parse(&long_label), Err(NameError::LabelTooLong));
        let long_name = ["a".repeat(63).as_str(); 5].join(".");
        assert_eq!(Hostname::parse(&long_name), Err(NameError::TooLong));
    }

    #[test]
    fn route_key_parses_exact_and_wildcard() {
        let exact = RouteKey::parse("API.example.com").unwrap();
        assert_eq!(exact.as_str(), "api.example.com");
        assert!(!exact.is_wildcard());

        let wildcard = RouteKey::parse("*.Apps.Example.com").unwrap();
        assert_eq!(wildcard.as_str(), "*.apps.example.com");
        assert!(wildcard.is_wildcard());
    }

    #[test]
    fn route_key_rejects_bad_wildcards() {
        for input in [
            "*.com",
            "*",
            "*.",
            "a.*.example.com",
            "*example.com",
            "**.a.b",
        ] {
            assert!(RouteKey::parse(input).is_err(), "input {input:?}");
        }
    }

    #[test]
    fn wildcard_for_strips_exactly_one_label() {
        let host = Hostname::parse("a.b.example.com").unwrap();
        assert_eq!(
            RouteKey::wildcard_for(&host).unwrap().as_str(),
            "*.b.example.com"
        );
        let two_labels = Hostname::parse("example.com").unwrap();
        assert_eq!(RouteKey::wildcard_for(&two_labels), None);
        let one_label = Hostname::parse("localhost").unwrap();
        assert_eq!(RouteKey::wildcard_for(&one_label), None);
    }

    #[test]
    fn wildcard_for_matches_parsed_wildcard_key() {
        let host = Hostname::parse("x.apps.example.com").unwrap();
        assert_eq!(
            RouteKey::wildcard_for(&host),
            Some(RouteKey::parse("*.apps.example.com").unwrap())
        );
    }

    #[test]
    fn backend_parses_dns_ipv4_and_ipv6() {
        let dns: Backend = "API-svc.default.svc:8443".parse().unwrap();
        assert_eq!(dns.host(), &BackendHost::Dns("api-svc.default.svc".into()));
        assert_eq!(dns.port().get(), 8443);
        assert_eq!(dns.to_string(), "api-svc.default.svc:8443");

        let v4: Backend = "10.0.0.5:443".parse().unwrap();
        assert_eq!(v4.host(), &BackendHost::Ip("10.0.0.5".parse().unwrap()));
        assert_eq!(v4.to_string(), "10.0.0.5:443");

        let v6: Backend = "[::1]:9000".parse().unwrap();
        assert_eq!(v6.host(), &BackendHost::Ip("::1".parse().unwrap()));
        assert_eq!(v6.to_string(), "[::1]:9000");
    }

    #[test]
    fn backend_rejects_malformed_values() {
        let cases = [
            ("svc", NameError::MissingPort),
            ("svc:0", NameError::InvalidPort),
            ("svc:65536", NameError::InvalidPort),
            ("svc:http", NameError::InvalidPort),
            ("::1:443", NameError::InvalidBackendHost),
            ("[::1]443", NameError::MissingPort),
            ("[nope]:443", NameError::InvalidBackendHost),
            ("bad host:443", NameError::InvalidBackendHost),
            (":443", NameError::InvalidBackendHost),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<Backend>(), Err(expected), "input {input:?}");
        }
    }
}
