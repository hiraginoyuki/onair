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
pub use sanitize::{DISPLAY_SEGMENT_MAX_CHARS, STORAGE_SEGMENT_MAX_CHARS, sanitize_for_storage};

/// Re-export of the `toml` crate's `Value` and `Map` types so
/// downstream crates can use the `extra_body` field type without
/// depending on `toml` directly. The `Value` enum is the canonical
/// representation of a parsed TOML value; `Map` is the
/// `BTreeMap`-backed table type that backs `Value::Table`.
pub use toml::value::{Table as TomlTable, Value as TomlValue};
