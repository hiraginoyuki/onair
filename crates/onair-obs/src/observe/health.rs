use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

const UNHEALTHY_FAILURE_THRESHOLD: u64 = 3;

#[derive(Debug, Clone)]
pub struct BackendHealthStore {
    inner: Arc<Mutex<BTreeMap<String, BackendHealthRecord>>>,
}

impl Default for BackendHealthStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendHealthStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn record_success(&self, backend: &str, latency: Duration, status: u16) {
        self.record_success_with_source(backend, latency, status, ObservationSource::Traffic);
    }

    pub fn record_failure(
        &self,
        backend: &str,
        latency: Duration,
        status: u16,
        error_kind: &'static str,
    ) {
        self.record_failure_with_source(
            backend,
            latency,
            status,
            error_kind,
            ObservationSource::Traffic,
        );
    }

    pub fn record_probe_success(&self, backend: &str, latency: Duration, status: u16) {
        self.record_success_with_source(backend, latency, status, ObservationSource::Probe);
    }

    pub fn record_probe_failure(
        &self,
        backend: &str,
        latency: Duration,
        status: u16,
        error_kind: &'static str,
    ) {
        self.record_failure_with_source(
            backend,
            latency,
            status,
            error_kind,
            ObservationSource::Probe,
        );
    }

    fn record_success_with_source(
        &self,
        backend: &str,
        latency: Duration,
        status: u16,
        source: ObservationSource,
    ) {
        let observed_at_unix_ms = unix_millis();
        let mut records = self.inner.lock();
        let record = records.entry(backend.to_owned()).or_default();
        match source {
            ObservationSource::Traffic => {
                record.traffic_successes = record.traffic_successes.saturating_add(1)
            }
            ObservationSource::Probe => {
                record.probe_successes = record.probe_successes.saturating_add(1)
            }
        }
        record.consecutive_failures = 0;
        record.last_success_unix_ms = Some(observed_at_unix_ms);
        record.last_observed_unix_ms = Some(observed_at_unix_ms);
        record.last_status = Some(status);
        record.last_error_kind = None;
        record.last_latency_ms = Some(duration_millis(latency));
        record.last_source = Some(source);
    }

    fn record_failure_with_source(
        &self,
        backend: &str,
        latency: Duration,
        status: u16,
        error_kind: &'static str,
        source: ObservationSource,
    ) {
        let observed_at_unix_ms = unix_millis();
        let mut records = self.inner.lock();
        let record = records.entry(backend.to_owned()).or_default();
        match source {
            ObservationSource::Traffic => {
                record.traffic_failures = record.traffic_failures.saturating_add(1)
            }
            ObservationSource::Probe => {
                record.probe_failures = record.probe_failures.saturating_add(1)
            }
        }
        record.consecutive_failures = record.consecutive_failures.saturating_add(1);
        record.last_failure_unix_ms = Some(observed_at_unix_ms);
        record.last_observed_unix_ms = Some(observed_at_unix_ms);
        record.last_status = Some(status);
        record.last_error_kind = Some(error_kind.to_owned());
        record.last_latency_ms = Some(duration_millis(latency));
        record.last_source = Some(source);
    }

    pub fn snapshot(&self, configured_backends: &[String]) -> Vec<BackendHealthSnapshot> {
        let records = self.inner.lock();
        configured_backends
            .iter()
            .map(|backend| {
                let record = records.get(backend).cloned().unwrap_or_default();
                BackendHealthSnapshot {
                    backend: backend.clone(),
                    status: record.status(),
                    successes: record.successes(),
                    failures: record.failures(),
                    traffic_successes: record.traffic_successes,
                    traffic_failures: record.traffic_failures,
                    probe_successes: record.probe_successes,
                    probe_failures: record.probe_failures,
                    consecutive_failures: record.consecutive_failures,
                    last_success_unix_ms: record.last_success_unix_ms,
                    last_failure_unix_ms: record.last_failure_unix_ms,
                    last_observed_unix_ms: record.last_observed_unix_ms,
                    last_status: record.last_status,
                    last_error_kind: record.last_error_kind,
                    last_latency_ms: record.last_latency_ms,
                    last_source: record.last_source.map(ObservationSource::as_str),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
struct BackendHealthRecord {
    traffic_successes: u64,
    traffic_failures: u64,
    probe_successes: u64,
    probe_failures: u64,
    consecutive_failures: u64,
    last_success_unix_ms: Option<u64>,
    last_failure_unix_ms: Option<u64>,
    last_observed_unix_ms: Option<u64>,
    last_status: Option<u16>,
    last_error_kind: Option<String>,
    last_latency_ms: Option<u64>,
    last_source: Option<ObservationSource>,
}

impl BackendHealthRecord {
    fn successes(&self) -> u64 {
        self.traffic_successes + self.probe_successes
    }

    fn failures(&self) -> u64 {
        self.traffic_failures + self.probe_failures
    }

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

#[derive(Debug, Clone, Copy)]
enum ObservationSource {
    Traffic,
    Probe,
}

impl ObservationSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Traffic => "traffic",
            Self::Probe => "probe",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendHealthSnapshot {
    pub backend: String,
    pub status: &'static str,
    pub successes: u64,
    pub failures: u64,
    pub traffic_successes: u64,
    pub traffic_failures: u64,
    pub probe_successes: u64,
    pub probe_failures: u64,
    pub consecutive_failures: u64,
    pub last_success_unix_ms: Option<u64>,
    pub last_failure_unix_ms: Option<u64>,
    pub last_observed_unix_ms: Option<u64>,
    pub last_status: Option<u16>,
    pub last_error_kind: Option<String>,
    pub last_latency_ms: Option<u64>,
    pub last_source: Option<&'static str>,
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
