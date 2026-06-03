#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSizeEntry {
    pub value: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    pub last_failure_unix_ms: Option<u64>,
    pub last_error_kind: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ContextSizeCache {
    inner: Arc<Mutex<BTreeMap<String, ContextSizeEntry>>>,
}

impl ContextSizeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup(&self, public_model: &str) -> Option<u64> {
        let map = self.inner.lock().expect("context size cache lock poisoned");
        map.get(public_model).and_then(|entry| entry.value)
    }

    pub fn entry(&self, public_model: &str) -> Option<ContextSizeEntry> {
        let map = self.inner.lock().expect("context size cache lock poisoned");
        map.get(public_model).cloned()
    }

    pub fn set(&self, public_model: &str, value: Option<u64>, error_kind: Option<&str>) {
        if let (None, None) = (value, error_kind) {
            return;
        }
        let now_unix_ms = unix_millis();
        let mut map = self.inner.lock().expect("context size cache lock poisoned");
        let entry = map.entry(public_model.to_owned()).or_default();
        entry.value = value;
        match (value, error_kind) {
            (Some(_), None) => {
                entry.last_success_unix_ms = Some(now_unix_ms);
                entry.last_error_kind = None;
            }
            (None, Some(kind)) => {
                entry.last_failure_unix_ms = Some(now_unix_ms);
                entry.last_error_kind = Some(kind.to_owned());
            }
            _ => {}
        }
    }

    pub fn prune(&self, active: &std::collections::BTreeSet<String>) {
        let mut map = self.inner.lock().expect("context size cache lock poisoned");
        map.retain(|public_model, _| active.contains(public_model));
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_none_for_missing_key() {
        let cache = ContextSizeCache::new();
        assert_eq!(cache.lookup("missing"), None);
        assert_eq!(cache.entry("missing"), None);
    }

    #[test]
    fn set_updates_value_and_stamps_success() {
        let cache = ContextSizeCache::new();
        cache.set("public", Some(131072), None);
        assert_eq!(cache.lookup("public"), Some(131072));
        let entry = cache.entry("public").unwrap();
        assert_eq!(entry.value, Some(131072));
        assert!(entry.last_success_unix_ms.is_some());
        assert!(entry.last_failure_unix_ms.is_none());
        assert!(entry.last_error_kind.is_none());
    }

    #[test]
    fn set_with_error_stamps_failure_only() {
        let cache = ContextSizeCache::new();
        cache.set("public", None, Some("timeout"));
        assert_eq!(cache.lookup("public"), None);
        let entry = cache.entry("public").unwrap();
        assert_eq!(entry.value, None);
        assert!(entry.last_success_unix_ms.is_none());
        assert!(entry.last_failure_unix_ms.is_some());
        assert_eq!(entry.last_error_kind.as_deref(), Some("timeout"));
    }

    #[test]
    fn set_with_neither_value_nor_error_is_a_noop() {
        let cache = ContextSizeCache::new();
        cache.set("public", None, None);
        assert!(cache.entry("public").is_none());
    }

    #[test]
    fn prune_removes_inactive_keys_only() {
        let cache = ContextSizeCache::new();
        cache.set("a", Some(1), None);
        cache.set("b", Some(2), None);
        cache.set("c", Some(3), None);
        let active = std::collections::BTreeSet::from(["a".to_owned(), "c".to_owned()]);
        cache.prune(&active);
        assert_eq!(cache.lookup("a"), Some(1));
        assert_eq!(cache.lookup("b"), None);
        assert_eq!(cache.lookup("c"), Some(3));
    }
}
