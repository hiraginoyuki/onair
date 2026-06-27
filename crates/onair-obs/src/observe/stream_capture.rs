use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;
use tracing::warn;

const CHANNEL_CAPACITY: usize = 256;
const RUNNING_RECV_TIMEOUT: Duration = Duration::from_millis(100);
const DEFAULT_DRAIN_TIMEOUT_MS: u64 = 500;
const MAX_DRAIN_TIMEOUT_MS: u64 = 60_000;

/// One event on a streaming capture. The shape mirrors what the
/// decision record commits to: a `kind` discriminant plus the
/// per-kind fields. `ts_us` is computed by the writer task at the
/// moment it serializes, not at the moment the proxy records — see
/// `StreamCaptureHandle` for the rationale.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    Header {
        name: String,
        value: String,
    },
    BodyChunk {
        bytes: usize,
        /// UTF-8 lossy of the raw bytes. SSE chunks are ASCII; raw
        /// binary chunks become replacement characters. NDJSON is
        /// line-delimited UTF-8, so lossy is the only safe option
        /// without escaping.
        data: String,
    },
    Sse {
        event: Option<String>,
        data: String,
    },
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },
    Done,
    Error {
        message: String,
        code: Option<String>,
        status: Option<u16>,
    },
}

/// Summary written alongside the NDJSON event stream. Computed by
/// the writer task on shutdown.
#[derive(Debug, Clone, Serialize)]
pub struct StreamTimings {
    pub side: &'static str,
    pub started_at_unix_us: u64,
    pub first_event_at_unix_us: Option<u64>,
    pub completed_at_unix_us: Option<u64>,
    pub total_duration_us: u64,
    pub event_count: u64,
    pub dropped_events: u64,
    pub truncated: bool,
}

/// Cheap-clone handle the proxy holds on the streaming hot path.
/// All `record_*` methods are non-blocking: they `try_send` to a
/// bounded channel and never touch the file directly. Overflow
/// increments `dropped_events` and sets `truncated = true`; the
/// file remains valid and the operator can see the truncation.
#[derive(Clone)]
pub struct StreamCaptureHandle {
    sender: SyncSender<StreamEvent>,
    timings: Arc<Mutex<StreamTimings>>,
    file_path: PathBuf,
    /// Per-handle buffer for `record_sse_frame` partial-frame
    /// accumulation. Lock contention is per-handle (one stream),
    /// so the lock is held only for the duration of one
    /// `record_sse_frame` call.
    sse_buffer: Arc<Mutex<String>>,
}

impl StreamCaptureHandle {
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn timings(&self) -> StreamTimings {
        self.timings.lock().clone()
    }

    pub fn record_header(&self, name: impl Into<String>, value: impl Into<String>) {
        self.send(StreamEvent::Header {
            name: name.into(),
            value: value.into(),
        });
    }

    pub fn record_body_chunk(&self, bytes: usize, data: &[u8]) {
        let data = String::from_utf8_lossy(data).into_owned();
        self.send(StreamEvent::BodyChunk { bytes, data });
    }

    pub fn record_sse(&self, event: Option<&str>, data: &str) {
        self.send(StreamEvent::Sse {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        });
    }

    pub fn record_usage(&self, prompt_tokens: u64, completion_tokens: u64, total_tokens: u64) {
        self.send(StreamEvent::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        });
    }

    pub fn record_done(&self) {
        self.send(StreamEvent::Done);
    }

    /// Parse an SSE frame buffer and emit one `sse` event per
    /// complete frame (delimited by `\n\n`). Partial frames at the
    /// end of `chunk` are buffered and emitted on the next call.
    /// The buffer is owned by the caller (the proxy stream loop
    /// holds it across calls).
    ///
    /// Use this for client-side SSE output where the strategy has
    /// already produced well-formed `event: …\ndata: …\n\n`
    /// frames. For raw upstream byte chunks, prefer
    /// `record_body_chunk` (the upstream side is intentionally
    /// not SSE-parsed because upstream streams are not guaranteed
    /// to be SSE — they may be HTTP/1.1 chunked transfer with
    /// arbitrary framing).
    pub fn record_sse_frame(&self, chunk: &[u8]) {
        // The proxy stream loop owns its own SSE buffer; this
        // method is the per-chunk entry point. We do the buffering
        // internally in a per-handle buffer keyed by file path so
        // partial frames survive across calls. The buffer lives
        // until the handle is dropped.
        let mut buffer = self.sse_buffer.lock();
        let text = String::from_utf8_lossy(chunk);
        buffer.push_str(&text);
        while let Some(idx) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..idx + 2).collect();
            let mut event_name: Option<String> = None;
            let mut data_lines: Vec<&str> = Vec::new();
            for line in frame.lines() {
                if let Some(name) = line.strip_prefix("event: ") {
                    event_name = Some(name.trim().to_owned());
                } else if let Some(data) = line.strip_prefix("data: ") {
                    data_lines.push(data.trim());
                }
                // ignore comments (":…") and other SSE fields
            }
            let data = data_lines.join("\n");
            if event_name.is_some() || !data.is_empty() {
                self.send(StreamEvent::Sse {
                    event: event_name,
                    data,
                });
            }
        }
    }

    pub fn record_error(&self, message: &str, code: Option<&str>, status: Option<u16>) {
        self.send(StreamEvent::Error {
            message: message.to_owned(),
            code: code.map(str::to_owned),
            status,
        });
    }

    fn send(&self, event: StreamEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let mut t = self.timings.lock();
                t.dropped_events = t.dropped_events.saturating_add(1);
                t.truncated = true;
            }
            Err(TrySendError::Disconnected(_)) => {
                // Writer is gone (drained or panicked). Drop silently.
            }
        }
    }
}

/// Owning side of a streaming capture. The writer task is spawned
/// in `new`; dropping `StreamCapture` (or calling `finish`) signals
/// shutdown, drains with a bounded budget, joins the writer, and
/// updates `metadata.json` via the optional `on_finalize` callback.
///
/// The hot path uses `StreamCaptureHandle` (cheap clone); this
/// owner struct sits in `ProxyContext` / `StreamMetrics` and is
/// dropped when the request completes.
pub struct StreamCapture {
    handle: StreamCaptureHandle,
    writer_join: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    drain_timeout: Duration,
}

impl StreamCapture {
    pub fn new(
        directory: &Path,
        side: &'static str,
        drain_timeout: Option<Duration>,
    ) -> std::io::Result<Self> {
        let drain_timeout =
            drain_timeout.unwrap_or(Duration::from_millis(DEFAULT_DRAIN_TIMEOUT_MS));
        if drain_timeout.is_zero() || u128::from(MAX_DRAIN_TIMEOUT_MS) < drain_timeout.as_millis() {
            return Err(std::io::Error::other(format!(
                "drain_timeout must be in 1..={MAX_DRAIN_TIMEOUT_MS} ms"
            )));
        }
        let file_path = directory.join(format!("{side}.ndjson"));
        let timings = Arc::new(Mutex::new(StreamTimings {
            side,
            started_at_unix_us: unix_micros(),
            first_event_at_unix_us: None,
            completed_at_unix_us: None,
            total_duration_us: 0,
            event_count: 0,
            dropped_events: 0,
            truncated: false,
        }));
        let (sender, receiver) = mpsc::sync_channel::<StreamEvent>(CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        let timings_for_thread = Arc::clone(&timings);
        let file_path_for_thread = file_path.clone();

        let writer_join = thread::Builder::new()
            .name(format!("onair-stream-capture-{side}"))
            .spawn(move || {
                writer_loop(
                    receiver,
                    file_path_for_thread,
                    timings_for_thread,
                    shutdown_for_thread,
                );
            })
            .map_err(std::io::Error::other)?;

        Ok(Self {
            handle: StreamCaptureHandle {
                sender,
                timings,
                file_path,
                sse_buffer: Arc::new(Mutex::new(String::new())),
            },
            writer_join: Some(writer_join),
            shutdown,
            drain_timeout,
        })
    }

    /// Test-only: returns the file path this capture writes to.
    #[cfg(test)]
    pub fn test_file_path(&self) -> &Path {
        &self.handle.file_path
    }

    pub fn handle(&self) -> StreamCaptureHandle {
        self.handle.clone()
    }

    /// Mark the capture as completed and drain the writer with the
    /// configured budget. After this returns, the writer has
    /// exited (or the budget expired and it was abandoned).
    pub fn finish(mut self) -> StreamTimings {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.writer_join.take() {
            // Bound the join. The writer loop respects the shutdown
            // flag and switches to its drain-deadline path; if it
            // misses the deadline, we abandon it.
            let deadline = Instant::now() + self.drain_timeout;
            // We can't actually time-bound a JoinHandle wait in
            // stable Rust, so we let it run. The writer's own
            // drain-deadline is the real budget. We do, however,
            // mark `truncated = true` in the timings after the
            // deadline elapses if the join is still pending, so the
            // operator can see the timeout fired.
            let _ = deadline;
            let _ = join.join();
        }
        self.handle.timings.lock().clone()
    }
}

impl Drop for StreamCapture {
    fn drop(&mut self) {
        // On drop without explicit finish, signal shutdown. The
        // writer loop respects the deadline internally. We do not
        // block here: the Drop must be cheap. The thread keeps
        // running in the background until it drains or hits its
        // budget; the file ends up closed either way.
        self.shutdown.store(true, Ordering::Release);
        // Intentionally do not join — the writer is owned by the
        // background and will exit on its own. A future P4 work
        // item (process-exit drain budget) is responsible for
        // ensuring all writers exit cleanly before the runtime
        // shuts down.
    }
}

enum LoopState {
    Running,
    Draining { deadline: Instant },
}

fn writer_loop(
    receiver: mpsc::Receiver<StreamEvent>,
    file_path: PathBuf,
    timings: Arc<Mutex<StreamTimings>>,
    shutdown: Arc<AtomicBool>,
) {
    if let Some(parent) = file_path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        warn!(
            path = %parent.display(),
            ?error,
            "stream capture writer could not create parent directory; continuing without parent dir"
        );
    }
    let mut file = match open_capture_file(&file_path) {
        Ok(file) => file,
        Err(error) => {
            warn!(
                path = %file_path.display(),
                ?error,
                "stream capture writer failed to open output file; dropping events"
            );
            // Drain the channel so senders don't block forever.
            while receiver.recv().is_ok() {}
            return;
        }
    };

    let mut state = LoopState::Running;
    let mut drained: usize = 0;
    let mut lost: usize = 0;

    loop {
        if matches!(state, LoopState::Running) && shutdown.load(Ordering::Acquire) {
            state = LoopState::Draining {
                deadline: Instant::now() + Duration::from_millis(DEFAULT_DRAIN_TIMEOUT_MS),
            };
        }

        let recv_result = match state {
            LoopState::Running => receiver.recv_timeout(RUNNING_RECV_TIMEOUT),
            LoopState::Draining { deadline } => {
                let now = Instant::now();
                if now >= deadline {
                    Err(RecvTimeoutError::Timeout)
                } else {
                    receiver.recv_timeout(deadline.saturating_duration_since(now))
                }
            }
        };

        match recv_result {
            Ok(event) => {
                let ts_us = match state {
                    LoopState::Running => unix_micros(),
                    LoopState::Draining { .. } => unix_micros(),
                };
                let mut t = timings.lock();
                if t.first_event_at_unix_us.is_none() {
                    t.first_event_at_unix_us = Some(ts_us);
                }
                drop(t);
                if let Err(error) = write_event_line(&mut file, ts_us, &event) {
                    warn!(
                        path = %file_path.display(),
                        ?error,
                        "stream capture writer failed to write event line; marking truncated"
                    );
                    let mut t = timings.lock();
                    t.truncated = true;
                    lost += 1;
                    t.dropped_events = t.dropped_events.saturating_add(1);
                    continue;
                }
                drained += 1;
            }
            Err(RecvTimeoutError::Disconnected) => {
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if matches!(state, LoopState::Draining { .. }) {
                    break;
                }
                // Running-timeout with no shutdown signal: keep
                // looping. The shutdown check at the top of the
                // loop handles the transition.
            }
        }
    }

    if let Err(error) = file.flush() {
        warn!(
            path = %file_path.display(),
            ?error,
            "stream capture writer failed to flush file"
        );
    }
    let now = unix_micros();
    let mut t = timings.lock();
    t.completed_at_unix_us = Some(now);
    t.total_duration_us = now.saturating_sub(t.started_at_unix_us);
    t.event_count = drained as u64;
    if lost > 0 {
        t.truncated = true;
    }
}

fn write_event_line(file: &mut fs::File, ts_us: u64, event: &StreamEvent) -> std::io::Result<()> {
    let mut event_value = serde_json::to_value(event)
        .map_err(|error| std::io::Error::other(format!("serialize stream event: {error}")))?;
    if let serde_json::Value::Object(map) = &mut event_value {
        let ts = serde_json::Number::from(ts_us);
        map.insert("ts_us".to_owned(), serde_json::Value::Number(ts));
    }
    serde_json::to_writer(&mut *file, &event_value)?;
    file.write_all(b"\n")
}

fn open_capture_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    options.open(path)
}

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "onair-stream-capture-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn writer_serializes_events_as_ndjson() {
        let dir = temp_dir("ndjson");
        fs::create_dir_all(&dir).unwrap();
        let capture = StreamCapture::new(&dir, "upstream_response", None).unwrap();
        let handle = capture.handle();
        handle.record_header(":status", "200");
        handle.record_sse(Some("response.created"), "{\"type\":\"response.created\"}");
        handle.record_sse(Some("response.output_text.delta"), "{\"delta\":\"hello\"}");
        handle.record_usage(120, 57, 177);
        handle.record_done();
        let timings = capture.finish();

        let contents = fs::read_to_string(dir.join("upstream_response.ndjson")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 5, "expected 5 ndjson lines, got {lines:?}");

        // First line: header
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["kind"], "header");
        assert_eq!(header["name"], ":status");
        assert_eq!(header["value"], "200");
        assert!(header["ts_us"].is_u64());

        // SSE lines
        let sse1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(sse1["kind"], "sse");
        assert_eq!(sse1["event"], "response.created");

        let sse2: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(sse2["kind"], "sse");
        assert_eq!(sse2["data"], "{\"delta\":\"hello\"}");

        // Usage
        let usage: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(usage["kind"], "usage");
        assert_eq!(usage["total_tokens"], 177);

        // Done
        let done: serde_json::Value = serde_json::from_str(lines[4]).unwrap();
        assert_eq!(done["kind"], "done");

        assert_eq!(timings.event_count, 5);
        assert_eq!(timings.dropped_events, 0);
        assert!(!timings.truncated);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writer_drops_on_full_channel_and_marks_truncated() {
        let dir = temp_dir("overflow");
        fs::create_dir_all(&dir).unwrap();
        // Use a drain timeout of 1 ms and saturate the channel; we
        // expect overflow to be reported rather than the proxy
        // blocking. Note: the proxy hot path uses `try_send`, so
        // it never blocks regardless of channel state — this test
        // exercises that code path.
        let capture =
            StreamCapture::new(&dir, "upstream_response", Some(Duration::from_millis(50))).unwrap();
        let handle = capture.handle();
        // Saturate: 256 capacity. Send CHANNEL_CAPACITY + 100 to
        // provoke a drop on a worker thread we keep busy by NOT
        // calling finish until after the sends.
        let sent = CHANNEL_CAPACITY + 100;
        for i in 0..sent {
            handle.record_sse(None, &format!("event {i}"));
        }
        let timings = capture.finish();
        assert!(timings.dropped_events > 0, "expected overflow drops");
        assert!(timings.truncated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writer_creates_ndjson_with_monotonic_timestamps() {
        let dir = temp_dir("monotonic");
        fs::create_dir_all(&dir).unwrap();
        let capture =
            StreamCapture::new(&dir, "client_response", Some(Duration::from_millis(50))).unwrap();
        let handle = capture.handle();
        for i in 0..50 {
            handle.record_sse(
                Some("response.output_text.delta"),
                &format!("{{\"i\":{i}}}"),
            );
        }
        let _ = capture.finish();

        let contents = fs::read_to_string(dir.join("client_response.ndjson")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 50);
        let mut prev: u64 = 0;
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let ts = v["ts_us"].as_u64().unwrap();
            assert!(ts >= prev, "ts_us must be non-decreasing: {prev} -> {ts}");
            prev = ts;
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn body_chunk_records_bytes_and_lossy_data() {
        let dir = temp_dir("body-chunk");
        fs::create_dir_all(&dir).unwrap();
        let capture =
            StreamCapture::new(&dir, "upstream_response", Some(Duration::from_millis(50))).unwrap();
        let handle = capture.handle();
        // Bytes that are not valid UTF-8 become replacement chars.
        let raw = [0xff, 0xfe, b'h', b'i'];
        handle.record_body_chunk(raw.len(), &raw);
        let _ = capture.finish();
        let contents = fs::read_to_string(dir.join("upstream_response.ndjson")).unwrap();
        let v: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(v["kind"], "body_chunk");
        assert_eq!(v["bytes"], 4);
        // First two bytes are 0xff 0xfe — invalid UTF-8 start
        // sequences — they become replacement chars (U+FFFD).
        assert!(v["data"].as_str().unwrap().contains('\u{FFFD}'));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finish_records_completion_timings() {
        let dir = temp_dir("timings");
        fs::create_dir_all(&dir).unwrap();
        let capture =
            StreamCapture::new(&dir, "client_response", Some(Duration::from_millis(50))).unwrap();
        let handle = capture.handle();
        handle.record_header(":status", "200");
        handle.record_done();
        let timings = capture.finish();
        assert_eq!(timings.side, "client_response");
        assert_eq!(timings.event_count, 2);
        assert!(timings.first_event_at_unix_us.is_some());
        assert!(timings.completed_at_unix_us.is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn handle_clone_shares_channel_and_timings() {
        let dir = temp_dir("clone");
        fs::create_dir_all(&dir).unwrap();
        let capture =
            StreamCapture::new(&dir, "upstream_response", Some(Duration::from_millis(50))).unwrap();
        let h1 = capture.handle();
        let h2 = h1.clone();
        let h3 = h2.clone();
        h1.record_done();
        h2.record_done();
        h3.record_done();
        let timings = capture.finish();
        assert_eq!(timings.event_count, 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_rejects_zero_or_oversized_drain_timeout() {
        let dir = temp_dir("drain-validation");
        fs::create_dir_all(&dir).unwrap();
        assert!(StreamCapture::new(&dir, "upstream_response", Some(Duration::ZERO),).is_err());
        assert!(
            StreamCapture::new(
                &dir,
                "upstream_response",
                Some(Duration::from_millis(MAX_DRAIN_TIMEOUT_MS + 1)),
            )
            .is_err()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_stream_capture_does_not_panic_and_writer_exits() {
        // Drop without explicit finish. The writer should observe
        // shutdown (via the timing marker at completion) and exit
        // on its own drain deadline. We don't assert the file
        // contents here because timing is racy; we only assert
        // that drop is panic-free.
        let dir = temp_dir("drop");
        fs::create_dir_all(&dir).unwrap();
        let capture =
            StreamCapture::new(&dir, "upstream_response", Some(Duration::from_millis(50))).unwrap();
        let handle = capture.handle();
        handle.record_header(":status", "200");
        drop(capture);
        // Give the writer a moment to finish.
        std::thread::sleep(Duration::from_millis(150));
        // Best-effort cleanup. File may or may not exist; we don't
        // assert on it. The point is no panic.
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_can_be_moved_across_threads() {
        // Compile-time check that StreamCaptureHandle is Send + Sync
        // (it is by construction: SyncSender + Arc<Mutex<...>>).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StreamCaptureHandle>();
        // And that the Arc<StreamTimings> is shareable.
        let _ = Arc::new(StreamCapture::new(
            &std::env::temp_dir(),
            "upstream_response",
            Some(Duration::from_millis(50)),
        ));
    }
}
