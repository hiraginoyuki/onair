use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::{ConfigStore, HealthConfig, ResolvedBackend};
use crate::observe::BackendHealthStore;

const INACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct HealthProbeTask {
    task: JoinHandle<()>,
}

impl HealthProbeTask {
    pub(crate) fn start(config: ConfigStore, http: Client, health: BackendHealthStore) -> Self {
        let task = tokio::spawn(async move {
            loop {
                let snapshot = config.snapshot();
                let health_config = snapshot.health.clone();
                if health_config.active {
                    for backend in snapshot.backends.iter().cloned() {
                        probe_backend(&http, &health, &health_config, backend).await;
                    }
                }
                tokio::time::sleep(probe_sleep_interval(&health_config)).await;
            }
        });
        Self { task }
    }
}

impl Drop for HealthProbeTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn probe_backend(
    http: &Client,
    health: &BackendHealthStore,
    config: &HealthConfig,
    backend: ResolvedBackend,
) {
    let started = Instant::now();
    let url = health_url(&backend.base_url, &config.path);
    let mut request = http
        .get(url)
        .timeout(Duration::from_millis(config.timeout_ms));
    if let Some(api_key) = &backend.api_key {
        request = request.bearer_auth(api_key);
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            let status = response.status().as_u16();
            health.record_probe_success(&backend.id, started.elapsed(), status);
            debug!(backend = %backend.id, status, "backend health probe succeeded");
        }
        Ok(response) => {
            let status = response.status().as_u16();
            health.record_probe_failure(&backend.id, started.elapsed(), status, "probe_status");
            warn!(backend = %backend.id, status, "backend health probe returned non-success status");
        }
        Err(error) => {
            let error_kind = probe_error_kind(&error);
            let status = if error.is_timeout() { 504 } else { 502 };
            health.record_probe_failure(&backend.id, started.elapsed(), status, error_kind);
            warn!(backend = %backend.id, error_kind, "backend health probe failed");
        }
    }
}

fn health_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn probe_sleep_interval(config: &HealthConfig) -> Duration {
    if config.active {
        Duration::from_millis(config.interval_ms)
    } else {
        INACTIVE_POLL_INTERVAL
    }
}

fn probe_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_redirect() {
        "redirect"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_probe_url_joins_base_and_path() {
        assert_eq!(
            health_url("http://127.0.0.1:8000/", "/v1/models"),
            "http://127.0.0.1:8000/v1/models"
        );
        assert_eq!(
            health_url("http://127.0.0.1:8000", "v1/models"),
            "http://127.0.0.1:8000/v1/models"
        );
    }

    #[test]
    fn inactive_probe_loop_uses_short_poll_delay() {
        let config = HealthConfig {
            active: false,
            interval_ms: 60_000,
            timeout_ms: 2_000,
            path: "/v1/models".to_owned(),
        };
        assert_eq!(probe_sleep_interval(&config), INACTIVE_POLL_INTERVAL);
    }
}
