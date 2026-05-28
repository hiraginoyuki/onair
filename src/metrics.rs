use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use crate::config::{TelemetryConfig, TelemetryExporter};
use crate::error::{Error, Result};
use crate::openai::UsageTotals;

#[derive(Debug, Clone)]
pub struct Metrics {
    requests_total: Counter<u64>,
    backend_requests_total: Counter<u64>,
    tokens_total: Counter<u64>,
    request_duration: Histogram<f64>,
    stream_duration: Histogram<f64>,
}

impl Metrics {
    pub fn new() -> Self {
        let meter = global::meter("onair");
        Self {
            requests_total: meter
                .u64_counter("onair.requests")
                .with_description("Total routed client requests")
                .with_unit("{request}")
                .build(),
            backend_requests_total: meter
                .u64_counter("onair.backend.requests")
                .with_description("Total upstream backend requests")
                .with_unit("{request}")
                .build(),
            tokens_total: meter
                .u64_counter("onair.tokens")
                .with_description("Tokens reported by OpenAI-compatible usage objects")
                .with_unit("{token}")
                .build(),
            request_duration: meter
                .f64_histogram("onair.request.duration")
                .with_description("Client request duration")
                .with_unit("s")
                .build(),
            stream_duration: meter
                .f64_histogram("onair.stream.duration")
                .with_description("Streaming response duration")
                .with_unit("s")
                .build(),
        }
    }

    pub fn record_backend_attempt(&self, labels: &MetricLabels) {
        self.backend_requests_total
            .add(1, &labels.backend_attributes());
    }

    pub fn record_request(&self, labels: &MetricLabels, status_code: u16, duration: Duration) {
        let attributes = labels.with_status(status_code);
        self.requests_total.add(1, &attributes);
        self.request_duration
            .record(duration.as_secs_f64(), &attributes);
    }

    pub fn record_stream(&self, labels: &MetricLabels, status_code: u16, duration: Duration) {
        self.stream_duration
            .record(duration.as_secs_f64(), &labels.with_status(status_code));
    }

    pub fn record_usage(&self, labels: &MetricLabels, usage: UsageTotals) {
        if usage.input > 0 {
            let mut attributes = labels.base_attributes();
            attributes.push(KeyValue::new("direction", "input"));
            self.tokens_total.add(usage.input, &attributes);
        }
        if usage.cached_input > 0 {
            let mut attributes = labels.base_attributes();
            attributes.push(KeyValue::new("direction", "cached_input"));
            self.tokens_total.add(usage.cached_input, &attributes);
        }
        if usage.output > 0 {
            let mut attributes = labels.base_attributes();
            attributes.push(KeyValue::new("direction", "output"));
            self.tokens_total.add(usage.output, &attributes);
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricLabels {
    pub route: String,
    pub identity: String,
    pub public_model: String,
    pub backend: String,
    pub stream: bool,
}

impl MetricLabels {
    fn base_attributes(&self) -> Vec<KeyValue> {
        vec![
            KeyValue::new("route", self.route.clone()),
            KeyValue::new("identity", self.identity.clone()),
            KeyValue::new("model", self.public_model.clone()),
            KeyValue::new("backend", self.backend.clone()),
            KeyValue::new("stream", self.stream),
        ]
    }

    fn backend_attributes(&self) -> Vec<KeyValue> {
        self.base_attributes()
    }

    fn with_status(&self, status_code: u16) -> Vec<KeyValue> {
        let mut attributes = self.base_attributes();
        attributes.push(KeyValue::new("status_code", i64::from(status_code)));
        attributes
    }
}

pub struct RequestTimer {
    start: Instant,
}

impl RequestTimer {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

#[derive(Debug)]
pub struct TelemetryGuard {
    provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    pub fn install(config: &TelemetryConfig) -> Result<Self> {
        match config.exporter {
            TelemetryExporter::None => Ok(Self { provider: None }),
            TelemetryExporter::Otlp => install_otlp(config),
        }
    }

    pub fn shutdown(self) {
        if let Some(provider) = self.provider {
            if let Err(error) = provider.shutdown() {
                tracing::warn!(?error, "failed to shutdown OpenTelemetry meter provider");
            }
        }
    }
}

fn install_otlp(config: &TelemetryConfig) -> Result<TelemetryGuard> {
    let mut exporter_builder = opentelemetry_otlp::MetricExporter::builder().with_tonic();
    if let Some(endpoint) = &config.otlp_endpoint {
        exporter_builder = exporter_builder.with_endpoint(endpoint);
    }
    let exporter = exporter_builder
        .build()
        .map_err(|error| Error::Telemetry(error.to_string()))?;
    let reader = PeriodicReader::builder(exporter)
        .with_interval(Duration::from_millis(config.export_interval_ms))
        .build();
    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .build();
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();
    global::set_meter_provider(provider.clone());
    Ok(TelemetryGuard {
        provider: Some(provider),
    })
}
