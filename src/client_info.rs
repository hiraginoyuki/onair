use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use axum::http::HeaderMap;
use axum::http::header::{FORWARDED, HeaderName, USER_AGENT};
use serde::Deserialize;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");
const UNKNOWN: &str = "unknown";
const NONE: &str = "none";
const MAX_HEADER_LOG_CHARS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    peer_addr: String,
    effective_client_addr: String,
    trusted_proxy_addr: Option<String>,
    forwarded_for: Option<String>,
    user_agent: Option<String>,
}

impl ClientInfo {
    pub fn from_headers(
        headers: &HeaderMap,
        peer_addr: Option<SocketAddr>,
        trusted_proxy_cidrs: &[IpCidr],
    ) -> Self {
        let peer_addr_string = peer_addr
            .map(|address| address.to_string())
            .unwrap_or_else(|| UNKNOWN.to_owned());
        let trusted_proxy = peer_addr
            .map(|address| {
                trusted_proxy_cidrs
                    .iter()
                    .any(|cidr| cidr.contains(address.ip()))
            })
            .unwrap_or(false);
        let forwarded_for = trusted_proxy.then(|| forwarded_client(headers)).flatten();
        let effective_client_addr = forwarded_for
            .clone()
            .unwrap_or_else(|| peer_addr_string.clone());
        let trusted_proxy_addr = forwarded_for.as_ref().map(|_| peer_addr_string.clone());

        Self {
            peer_addr: peer_addr_string,
            effective_client_addr,
            trusted_proxy_addr,
            forwarded_for,
            user_agent: sanitized_header(headers, &USER_AGENT),
        }
    }

    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }

    pub fn effective_client_addr(&self) -> &str {
        &self.effective_client_addr
    }

    pub fn trusted_proxy_addr(&self) -> &str {
        self.trusted_proxy_addr.as_deref().unwrap_or(NONE)
    }

    pub fn forwarded_for(&self) -> &str {
        self.forwarded_for.as_deref().unwrap_or(NONE)
    }

    pub fn user_agent(&self) -> &str {
        self.user_agent.as_deref().unwrap_or(NONE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpCidr {
    network: IpAddr,
    prefix: u8,
}

impl IpCidr {
    pub fn contains(self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) if self.prefix <= 32 => {
                let mask = ipv4_mask(self.prefix);
                u32::from(network) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(address)) if self.prefix <= 128 => {
                let mask = ipv6_mask(self.prefix);
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

impl FromStr for IpCidr {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err("CIDR must not be empty".to_owned());
        }

        let (network, prefix) = if let Some((address, prefix)) = value.split_once('/') {
            let network = address
                .parse::<IpAddr>()
                .map_err(|error| format!("invalid CIDR address: {error}"))?;
            let prefix = prefix
                .parse::<u8>()
                .map_err(|error| format!("invalid CIDR prefix: {error}"))?;
            (network, prefix)
        } else {
            let network = value
                .parse::<IpAddr>()
                .map_err(|error| format!("invalid CIDR address: {error}"))?;
            let prefix = match network {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            (network, prefix)
        };

        let max_prefix = match network {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > max_prefix {
            return Err(format!(
                "CIDR prefix {prefix} exceeds maximum {max_prefix} for {network}"
            ));
        }

        Ok(Self { network, prefix })
    }
}

impl fmt::Display for IpCidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.network, self.prefix)
    }
}

impl<'de> Deserialize<'de> for IpCidr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

fn ipv4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn ipv6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

fn forwarded_client(headers: &HeaderMap) -> Option<String> {
    headers
        .get(FORWARDED)
        .and_then(|value| value.to_str().ok())
        .and_then(forwarded_header_client)
        .or_else(|| sanitized_header(headers, &X_FORWARDED_FOR).and_then(first_forwarded_for))
        .or_else(|| sanitized_header(headers, &X_REAL_IP))
}

fn forwarded_header_client(value: &str) -> Option<String> {
    value.split(',').find_map(|entry| {
        entry.split(';').find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            key.trim()
                .eq_ignore_ascii_case("for")
                .then(|| clean_forwarded_value(value))
                .flatten()
        })
    })
}

fn first_forwarded_for(value: String) -> Option<String> {
    value
        .split(',')
        .map(str::trim)
        .find(|part| !part.is_empty())
        .and_then(sanitized_value)
}

fn sanitized_header(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(sanitized_value)
}

fn clean_forwarded_value(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"').trim();
    let value = value
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(address, _)| address))
        .unwrap_or(value);
    if value.eq_ignore_ascii_case("unknown") || value.starts_with('_') {
        return None;
    }
    sanitized_value(value)
}

fn sanitized_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                '_'
            } else {
                character
            }
        })
        .take(MAX_HEADER_LOG_CHARS)
        .collect::<String>();
    (!sanitized.is_empty()).then_some(sanitized)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn cidr_matches_ipv4_and_ipv6_ranges() {
        let local_v4 = "127.0.0.0/8".parse::<IpCidr>().unwrap();
        assert!(local_v4.contains("127.0.0.1".parse().unwrap()));
        assert!(!local_v4.contains("10.0.0.1".parse().unwrap()));

        let local_v6 = "::1/128".parse::<IpCidr>().unwrap();
        assert!(local_v6.contains("::1".parse().unwrap()));
        assert!(!local_v6.contains("::2".parse().unwrap()));
    }

    #[test]
    fn trusted_proxy_headers_override_effective_client() {
        let mut headers = HeaderMap::new();
        headers.insert(
            X_FORWARDED_FOR,
            HeaderValue::from_static("203.0.113.10, 10.0.0.2"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("friend-client/1.0"));
        let peer = "127.0.0.1:55432".parse().unwrap();
        let info =
            ClientInfo::from_headers(&headers, Some(peer), &["127.0.0.1/32".parse().unwrap()]);

        assert_eq!(info.peer_addr(), "127.0.0.1:55432");
        assert_eq!(info.effective_client_addr(), "203.0.113.10");
        assert_eq!(info.trusted_proxy_addr(), "127.0.0.1:55432");
        assert_eq!(info.user_agent(), "friend-client/1.0");
    }

    #[test]
    fn untrusted_forwarded_headers_are_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_FOR, HeaderValue::from_static("203.0.113.10"));
        let peer = "198.51.100.20:55432".parse().unwrap();
        let info =
            ClientInfo::from_headers(&headers, Some(peer), &["127.0.0.1/32".parse().unwrap()]);

        assert_eq!(info.effective_client_addr(), "198.51.100.20:55432");
        assert_eq!(info.trusted_proxy_addr(), "none");
    }

    #[test]
    fn forwarded_header_takes_precedence_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            FORWARDED,
            HeaderValue::from_static("for=\"[2001:db8::1]:1234\";proto=https"),
        );
        headers.insert(X_FORWARDED_FOR, HeaderValue::from_static("203.0.113.10"));
        let peer = "127.0.0.1:55432".parse().unwrap();
        let info =
            ClientInfo::from_headers(&headers, Some(peer), &["127.0.0.1/32".parse().unwrap()]);

        assert_eq!(info.effective_client_addr(), "2001:db8::1");
    }
}
