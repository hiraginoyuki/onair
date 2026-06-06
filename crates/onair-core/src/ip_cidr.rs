use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::Deserialize;

const V4_MAPPED_PREFIX: u128 = 0xFFFF_u128 << 32;

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
            (IpAddr::V6(network), IpAddr::V4(address)) if self.prefix <= 128 => {
                let mapped = u128::from(u32::from(address)) | V4_MAPPED_PREFIX;
                let mask = ipv6_mask(self.prefix);
                u128::from(network) & mask == mapped & mask
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
        if prefix == 0 {
            return Err(
                "CIDR prefix 0 matches every address and is not allowed for trusted_proxy_cidrs"
                    .to_owned(),
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v4_cidr() {
        let cidr: IpCidr = "10.0.0.0/8".parse().unwrap();
        assert!(cidr.contains("10.1.2.3".parse().unwrap()));
        assert!(!cidr.contains("11.1.2.3".parse().unwrap()));
    }

    #[test]
    fn parses_v6_cidr() {
        let cidr: IpCidr = "2001:db8::/32".parse().unwrap();
        assert!(cidr.contains("2001:db8::1".parse().unwrap()));
        assert!(!cidr.contains("2001:db9::1".parse().unwrap()));
    }

    #[test]
    fn rejects_empty() {
        assert!("".parse::<IpCidr>().is_err());
    }

    #[test]
    fn rejects_oversized_prefix() {
        assert!("10.0.0.0/33".parse::<IpCidr>().is_err());
    }

    #[test]
    fn rejects_zero_prefix_v4() {
        let error = "0.0.0.0/0".parse::<IpCidr>().unwrap_err();
        assert!(
            error.contains("prefix 0"),
            "unexpected zero-prefix error: {error}"
        );
    }

    #[test]
    fn rejects_zero_prefix_v6() {
        let error = "::/0".parse::<IpCidr>().unwrap_err();
        assert!(
            error.contains("prefix 0"),
            "unexpected zero-prefix error: {error}"
        );
    }

    #[test]
    fn v6_cidr_matches_v4_mapped_address() {
        let cidr: IpCidr = "::ffff:127.0.0.0/104".parse().unwrap();
        assert!(cidr.contains("127.0.0.1".parse().unwrap()));
        assert!(cidr.contains("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!cidr.contains("128.0.0.1".parse().unwrap()));
    }

    #[test]
    fn display_round_trip() {
        let cidr: IpCidr = "192.168.1.0/24".parse().unwrap();
        assert_eq!(cidr.to_string(), "192.168.1.0/24");
    }
}
