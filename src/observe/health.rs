use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UNHEALTHY_FAILURE_THRESHOLD: u64 = 3;

#[derive(Debug, Clone)]
pub(crate) struct BackendHealthStore {
    inner: Arc<Mutex<BTreeMap<String, BackendHealthRecord>>>,
}

impl BackendHealthStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(crate) fn record_success(&self, backend: &str, latency: Duration, status: u16) {
        let observed_at_unix_ms = unix_millis();
        let mut records = self.inner.lock().expect("backend health lock poisoned");
        let record = records.entry(backend.to_owned()).or_default();
        record.successes += 1;
        record.consecutive_failures = 0;
        record.last_success_unix_ms = Some(observed_at_unix_ms);
        record.last_observed_unix_ms = Some(observed_at_unix_ms);
        record.last_status = Some(status);
        record.last_error_kind = None;
        record.last_latency_ms = Some(duration_millis(latency));
    }

    pub(crate) fn record_failure(
        &self,
        backend: &str,
        latency: Duration,
        status: u16,
        error_kind: &'static str,
    ) {
        let observed_at_unix_ms = unix_millis();
        let mut records = self.inner.lock().expect("backend health lock poisoned");
        let record = records.entry(backend.to_owned()).or_default();
        record.failures += 1;
        record.consecutive_failures += 1;
        record.last_failure_unix_ms = Some(observed_at_unix_ms);
        record.last_observed_unix_ms = Some(observed_at_unix_ms);
        record.last_status = Some(status);
        record.last_error_kind = Some(error_kind.to_owned());
        record.last_latency_ms = Some(duration_millis(latency));
    }

    pub(crate) fn snapshot(&self, configured_backends: &[String]) -> Vec<BackendHealthSnapshot> {
        let records = self.inner.lock().expect("backend health lock poisoned");
        configured_backends
            .iter()
            .map(|backend| {
                let record = records.get(backend).cloned().unwrap_or_default();
                BackendHealthSnapshot {
                    backend: backend.clone(),
                    status: record.status(),
                    successes: record.successes,
                    failures: record.failures,
                    consecutive_failures: record.consecutive_failures,
                    last_success_unix_ms: record.last_success_unix_ms,
                    last_failure_unix_ms: record.last_failure_unix_ms,
                    last_observed_unix_ms: record.last_observed_unix_ms,
                    last_status: record.last_status,
                    last_error_kind: record.last_error_kind,
                    last_latency_ms: record.last_latency_ms,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
struct BackendHealthRecord {
    successes: u64,
    failures: u64,
    consecutive_failures: u64,
    last_success_unix_ms: Option<u64>,
    last_failure_unix_ms: Option<u64>,
    last_observed_unix_ms: Option<u64>,
    last_status: Option<u16>,
    last_error_kind: Option<String>,
    last_latency_ms: Option<u64>,
}

impl BackendHealthRecord {
    fn status(&self) -> &'static str {
        if self.last_observed_unix_ms.is_none() {
            "unknown"
        } else if self.consecutive_failures == 0 {
            "healthy"
        } else if self.consecutive_failures >= UNHEALTHY_FAILURE_THRESHOLD {
            "unhealthy"
        } else {
            "degraded"
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BackendHealthSnapshot {
    pub(crate) backend: String,
    pub(crate) status: &'static str,
    pub(crate) successes: u64,
    pub(crate) failures: u64,
    pub(crate) consecutive_failures: u64,
    pub(crate) last_success_unix_ms: Option<u64>,
    pub(crate) last_failure_unix_ms: Option<u64>,
    pub(crate) last_observed_unix_ms: Option<u64>,
    pub(crate) last_status: Option<u16>,
    pub(crate) last_error_kind: Option<String>,
    pub(crate) last_latency_ms: Option<u64>,
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_tracks_success_and_failure_state() {
        let store = BackendHealthStore::new();
        let backends = vec!["backend-a".to_owned(), "backend-b".to_owned()];

        let initial = store.snapshot(&backends);
        assert_eq!(initial[0].status, "unknown");
        assert_eq!(initial[1].status, "unknown");

        store.record_failure("backend-a", Duration::from_millis(12), 502, "connect");
        let failed = store.snapshot(&backends);
        assert_eq!(failed[0].status, "degraded");
        assert_eq!(failed[0].consecutive_failures, 1);
        assert_eq!(failed[0].last_error_kind.as_deref(), Some("connect"));

        store.record_success("backend-a", Duration::from_millis(8), 200);
        let recovered = store.snapshot(&backends);
        assert_eq!(recovered[0].status, "healthy");
        assert_eq!(recovered[0].consecutive_failures, 0);
        assert_eq!(recovered[0].successes, 1);
        assert_eq!(recovered[0].failures, 1);
    }
}
