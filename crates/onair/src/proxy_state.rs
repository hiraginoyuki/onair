use std::sync::Arc;

use reqwest::Client;
use tokio::sync::watch;

use onair_obs::metrics::Metrics;
use onair_obs::observe::{BackendHealthStore, InspectorStore};

use onair_core::config::ConfigStore;

use crate::routing::RoundRobinCounters;

pub struct ProxyState {
    pub config: Arc<ConfigStore>,
    pub http: Arc<Client>,
    pub inspector: Arc<InspectorStore>,
    pub metrics: Arc<Metrics>,
    pub health: Arc<BackendHealthStore>,
    pub round_robin: Arc<RoundRobinCounters>,
    pub shutdown: watch::Receiver<bool>,
}

impl ProxyState {
    pub fn from_app_state(
        config: Arc<ConfigStore>,
        http: Arc<Client>,
        inspector: Arc<InspectorStore>,
        metrics: Arc<Metrics>,
        health: Arc<BackendHealthStore>,
        round_robin: Arc<RoundRobinCounters>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            config,
            http,
            inspector,
            metrics,
            health,
            round_robin,
            shutdown,
        }
    }
}
