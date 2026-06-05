use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

    pub fn prune(&self, active: &BTreeSet<String>) {
        let mut map = self.inner.lock().expect("context size cache lock poisoned");
        map.retain(|public_model, _| active.contains(public_model));
    }
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
