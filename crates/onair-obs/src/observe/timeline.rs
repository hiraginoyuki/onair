use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RequestTimeline {
    started: Instant,
    started_unix_ms: u128,
    proxy_entry_us: u64,
    auth_done_us: Option<u64>,
    request_inspected_us: Option<u64>,
    route_selected_us: Option<u64>,
    request_rewritten_us: Option<u64>,
    debug_capture_done_us: Option<u64>,
    backend_forward_start_us: Option<u64>,
    backend_headers_received_us: Option<u64>,
    backend_body_first_chunk_us: Option<u64>,
    backend_body_complete_us: Option<u64>,
    response_rewritten_us: Option<u64>,
    client_response_ready_us: Option<u64>,
    stream_complete_us: Option<u64>,
}

impl RequestTimeline {
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            started_unix_ms: unix_millis(),
            proxy_entry_us: 0,
            auth_done_us: None,
            request_inspected_us: None,
            route_selected_us: None,
            request_rewritten_us: None,
            debug_capture_done_us: None,
            backend_forward_start_us: None,
            backend_headers_received_us: None,
            backend_body_first_chunk_us: None,
            backend_body_complete_us: None,
            response_rewritten_us: None,
            client_response_ready_us: None,
            stream_complete_us: None,
        }
    }

    pub fn mark(&mut self, event: TimelineEvent) -> u64 {
        let elapsed_us = duration_micros(self.started.elapsed());
        match event {
            TimelineEvent::AuthDone => self.auth_done_us = Some(elapsed_us),
            TimelineEvent::RequestInspected => self.request_inspected_us = Some(elapsed_us),
            TimelineEvent::RouteSelected => self.route_selected_us = Some(elapsed_us),
            TimelineEvent::RequestRewritten => self.request_rewritten_us = Some(elapsed_us),
            TimelineEvent::DebugCaptureDone => self.debug_capture_done_us = Some(elapsed_us),
            TimelineEvent::BackendForwardStart => self.backend_forward_start_us = Some(elapsed_us),
            TimelineEvent::BackendHeadersReceived => {
                self.backend_headers_received_us = Some(elapsed_us)
            }
            TimelineEvent::BackendBodyFirstChunk => {
                if self.backend_body_first_chunk_us.is_none() {
                    self.backend_body_first_chunk_us = Some(elapsed_us);
                }
            }
            TimelineEvent::BackendBodyComplete => self.backend_body_complete_us = Some(elapsed_us),
            TimelineEvent::ResponseRewritten => self.response_rewritten_us = Some(elapsed_us),
            TimelineEvent::ClientResponseReady => self.client_response_ready_us = Some(elapsed_us),
            TimelineEvent::StreamComplete => self.stream_complete_us = Some(elapsed_us),
        }
        elapsed_us
    }

    pub fn snapshot(&self) -> TimelineSnapshot {
        TimelineSnapshot {
            started_unix_ms: saturating_u64(self.started_unix_ms),
            total_us: duration_micros(self.started.elapsed()),
            proxy_entry_us: self.proxy_entry_us,
            auth_done_us: self.auth_done_us,
            request_inspected_us: self.request_inspected_us,
            route_selected_us: self.route_selected_us,
            request_rewritten_us: self.request_rewritten_us,
            debug_capture_done_us: self.debug_capture_done_us,
            backend_forward_start_us: self.backend_forward_start_us,
            backend_headers_received_us: self.backend_headers_received_us,
            backend_body_first_chunk_us: self.backend_body_first_chunk_us,
            backend_body_complete_us: self.backend_body_complete_us,
            response_rewritten_us: self.response_rewritten_us,
            client_response_ready_us: self.client_response_ready_us,
            stream_complete_us: self.stream_complete_us,
        }
    }

    pub fn elapsed_us(&self) -> u64 {
        duration_micros(self.started.elapsed())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TimelineEvent {
    AuthDone,
    RequestInspected,
    RouteSelected,
    RequestRewritten,
    DebugCaptureDone,
    BackendForwardStart,
    BackendHeadersReceived,
    BackendBodyFirstChunk,
    BackendBodyComplete,
    ResponseRewritten,
    ClientResponseReady,
    StreamComplete,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TimelineSnapshot {
    pub started_unix_ms: u64,
    pub total_us: u64,
    pub proxy_entry_us: u64,
    pub auth_done_us: Option<u64>,
    pub request_inspected_us: Option<u64>,
    pub route_selected_us: Option<u64>,
    pub request_rewritten_us: Option<u64>,
    pub debug_capture_done_us: Option<u64>,
    pub backend_forward_start_us: Option<u64>,
    pub backend_headers_received_us: Option<u64>,
    pub backend_body_first_chunk_us: Option<u64>,
    pub backend_body_complete_us: Option<u64>,
    pub response_rewritten_us: Option<u64>,
    pub client_response_ready_us: Option<u64>,
    pub stream_complete_us: Option<u64>,
}

fn duration_micros(duration: Duration) -> u64 {
    saturating_u64(duration.as_micros())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn saturating_u64(value: u128) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_records_named_milestones() {
        let mut timeline = RequestTimeline::start();
        timeline.mark(TimelineEvent::AuthDone);
        timeline.mark(TimelineEvent::BackendForwardStart);

        let snapshot = timeline.snapshot();
        assert_eq!(snapshot.proxy_entry_us, 0);
        assert!(snapshot.auth_done_us.is_some());
        assert!(snapshot.backend_forward_start_us.is_some());
        assert!(snapshot.backend_headers_received_us.is_none());
    }

    #[test]
    fn first_body_chunk_keeps_first_mark() {
        let mut timeline = RequestTimeline::start();
        let first = timeline.mark(TimelineEvent::BackendBodyFirstChunk);
        let second = timeline.mark(TimelineEvent::BackendBodyFirstChunk);

        assert_eq!(timeline.snapshot().backend_body_first_chunk_us, Some(first));
        assert!(second >= first);
    }
}
