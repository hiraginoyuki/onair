pub mod auth;
pub mod compat;
pub mod config;
pub mod context_size_cache;
pub mod error;
pub mod ip_cidr;
pub mod openai;
pub mod routing_markers;
pub mod sanitize;

pub use context_size_cache::{ContextSizeCache, ContextSizeEntry};
pub use ip_cidr::IpCidr;
pub use routing_markers::{KNOWN_MARKERS, is_known_marker};
pub use sanitize::sanitize_for_storage;

/// Re-export of the `toml` crate's `Value` so downstream crates
/// can use the `extra_body` field type without depending on `toml`
/// directly. The `Value` enum is the canonical representation of a
/// parsed TOML value and is the type carried in `extra_body`.
pub use toml::value::Value as TomlValue;
