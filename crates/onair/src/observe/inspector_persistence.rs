use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, params};
use tracing::{info, warn};

use crate::error::{Error, Result};

use super::inspector::{InspectorOutcome, InspectorRequestRecord};

const WRITER_QUEUE_CAPACITY: usize = 1024;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const RUNNING_RECV_TIMEOUT: Duration = Duration::from_millis(100);
const CURRENT_SCHEMA_VERSION: i64 = 1;

pub(super) struct InspectorPersistenceWriter {
    sender: SyncSender<PersistenceMessage>,
    shutdown: Arc<AtomicBool>,
}

#[allow(clippy::large_enum_variant)]
enum PersistenceMessage {
    Record {
        record: InspectorRequestRecord,
        retention_requests: usize,
    },
    Shutdown,
}

enum LoopState {
    Running,
    Draining { deadline: Instant },
}

pub(super) fn restore_records(
    path: &Path,
    retention_requests: usize,
) -> Result<(
    Vec<InspectorRequestRecord>,
    InspectorPersistenceWriter,
    thread::JoinHandle<()>,
)> {
    prepare_parent_directory(path)?;
    let connection = open_connection(path)?;
    initialize_schema(&connection)?;
    prune_records(&connection, retention_requests)?;
    let records = load_latest_records(&connection, retention_requests)?;
    drop(connection);

    let (sender, receiver) = mpsc::sync_channel::<PersistenceMessage>(WRITER_QUEUE_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let path_for_thread = path.to_owned();
    let writer_shutdown = Arc::clone(&shutdown);
    let handle = thread::Builder::new()
        .name("onair-inspector-sqlite".to_owned())
        .spawn(move || {
            let connection = match open_connection(&path_for_thread).and_then(|connection| {
                initialize_schema(&connection)?;
                Ok(connection)
            }) {
                Ok(connection) => connection,
                Err(error) => {
                    warn!(
                        ?error,
                        path = %path_for_thread.display(),
                        "inspector persistence writer failed to start"
                    );
                    return;
                }
            };
            info!(
                path = %path_for_thread.display(),
                "inspector persistence writer started"
            );

            let mut state = LoopState::Running;
            let mut drained: usize = 0;
            let mut lost: usize = 0;

            loop {
                if matches!(state, LoopState::Running) && shutdown.load(Ordering::Acquire) {
                    state = LoopState::Draining {
                        deadline: Instant::now() + SHUTDOWN_DRAIN_TIMEOUT,
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
                    Ok(PersistenceMessage::Record {
                        record,
                        retention_requests,
                    }) => {
                        if let Err(error) = persist_record(&connection, &record, retention_requests)
                        {
                            warn!(?error, "failed to persist inspector record");
                        }
                        drained += 1;
                    }
                    Ok(PersistenceMessage::Shutdown) => {
                        if matches!(state, LoopState::Running) {
                            state = LoopState::Draining {
                                deadline: Instant::now() + SHUTDOWN_DRAIN_TIMEOUT,
                            };
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if matches!(state, LoopState::Draining { .. }) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            while let Ok(PersistenceMessage::Record { .. }) = receiver.try_recv() {
                lost += 1;
            }

            if let Err(error) = connection.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
                warn!(
                    ?error,
                    "failed to checkpoint inspector persistence on shutdown"
                );
            }

            if lost > 0 {
                warn!(
                    drained,
                    lost, "inspector persistence shutdown timed out; dropped records"
                );
            }
        })
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;

    Ok((
        records,
        InspectorPersistenceWriter {
            sender,
            shutdown: writer_shutdown,
        },
        handle,
    ))
}

impl InspectorPersistenceWriter {
    pub(super) fn record(&self, record: InspectorRequestRecord, retention_requests: usize) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        match self.sender.try_send(PersistenceMessage::Record {
            record,
            retention_requests,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                warn!("inspector persistence writer queue is full; dropping record");
            }
            Err(TrySendError::Disconnected(_)) => {
                warn!("inspector persistence writer is closed; dropping record");
            }
        }
    }

    pub(super) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sender.try_send(PersistenceMessage::Shutdown);
    }
}

fn prepare_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection> {
    let existed = path.exists();
    let connection =
        Connection::open(path).map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    if !existed {
        set_owner_only_permissions(path)?;
    }
    Ok(connection)
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS inspector_records (
                record_id TEXT PRIMARY KEY NOT NULL,
                started_at_unix_ms INTEGER NOT NULL,
                completed_at_unix_ms INTEGER NOT NULL,
                status INTEGER NOT NULL,
                outcome_kind TEXT NOT NULL,
                route TEXT NOT NULL,
                identity TEXT NOT NULL,
                public_model TEXT NOT NULL,
                backend TEXT NOT NULL,
                error_kind TEXT,
                schema_version INTEGER NOT NULL DEFAULT 1,
                record_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS inspector_records_started_idx
                ON inspector_records (started_at_unix_ms DESC, record_id DESC);
            CREATE INDEX IF NOT EXISTS inspector_records_status_idx
                ON inspector_records (status);
            CREATE INDEX IF NOT EXISTS inspector_records_route_idx
                ON inspector_records (route);
            "#,
        )
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;

    let has_schema_version: bool = connection
        .prepare("PRAGMA table_info(inspector_records)")
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?
        .filter_map(|result| result.ok())
        .any(|name| name == "schema_version");

    if !has_schema_version {
        connection
            .execute(
                "ALTER TABLE inspector_records ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 1",
                [],
            )
            .map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    }

    Ok(())
}

fn persist_record(
    connection: &Connection,
    record: &InspectorRequestRecord,
    retention_requests: usize,
) -> Result<()> {
    let record_json = serde_json::to_string(record)
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    connection
        .execute(
            r#"
            INSERT OR REPLACE INTO inspector_records (
                record_id,
                started_at_unix_ms,
                completed_at_unix_ms,
                status,
                outcome_kind,
                route,
                identity,
                public_model,
                backend,
                error_kind,
                schema_version,
                record_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                record.base.record_id,
                sqlite_u64(record.base.started_at_unix_ms),
                sqlite_u64(record.completed_at_unix_ms),
                i64::from(record.status),
                outcome_kind(&record.outcome),
                record.base.route,
                record.base.identity,
                record.base.public_model,
                record.base.backend,
                record.error_kind,
                CURRENT_SCHEMA_VERSION,
                record_json,
            ],
        )
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    prune_records(connection, retention_requests)
}

fn load_latest_records(
    connection: &Connection,
    retention_requests: usize,
) -> Result<Vec<InspectorRequestRecord>> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT record_json
            FROM (
                SELECT record_json, started_at_unix_ms, record_id
                FROM inspector_records
                WHERE schema_version <= ?2
                ORDER BY started_at_unix_ms DESC, record_id DESC
                LIMIT ?1
            )
            ORDER BY started_at_unix_ms ASC, record_id ASC
            "#,
        )
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;
    let rows = statement
        .query_map(
            params![sqlite_usize(retention_requests), CURRENT_SCHEMA_VERSION],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| Error::InspectorPersistence(error.to_string()))?;

    let mut records = Vec::new();
    for row in rows {
        let record_json = row.map_err(|error| Error::InspectorPersistence(error.to_string()))?;
        match serde_json::from_str::<InspectorRequestRecord>(&record_json) {
            Ok(record) => records.push(record),
            Err(error) => warn!(?error, "skipping malformed persisted inspector record"),
        }
    }
    Ok(records)
}

fn prune_records(connection: &Connection, retention_requests: usize) -> Result<()> {
    connection
        .execute(
            r#"
            DELETE FROM inspector_records
            WHERE record_id NOT IN (
                SELECT record_id
                FROM inspector_records
                ORDER BY started_at_unix_ms DESC, record_id DESC
                LIMIT ?1
            )
            "#,
            [sqlite_usize(retention_requests)],
        )
        .map(|_| ())
        .map_err(|error| Error::InspectorPersistence(error.to_string()))
}

fn outcome_kind(outcome: &InspectorOutcome) -> &'static str {
    match outcome {
        InspectorOutcome::InFlight => "in_flight",
        InspectorOutcome::Completed => "completed",
        InspectorOutcome::Preflight { .. } => "preflight",
        InspectorOutcome::UpstreamTimeout => "upstream_timeout",
        InspectorOutcome::UpstreamRequestFailed => "upstream_request_failed",
        InspectorOutcome::UpstreamNonSuccess => "upstream_non_success",
        InspectorOutcome::UpstreamBodyReadFailed => "upstream_body_read_failed",
        InspectorOutcome::UpstreamStreamFailed => "upstream_stream_failed",
        InspectorOutcome::StreamIncomplete => "stream_incomplete",
        InspectorOutcome::Interrupted => "interrupted",
    }
}

fn sqlite_u64(value: u64) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn sqlite_usize(value: usize) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

#[cfg(test)]
pub(crate) fn stored_count(path: &Path) -> Result<usize> {
    let connection = open_connection(path)?;
    initialize_schema(&connection)?;
    connection
        .query_row("SELECT COUNT(*) FROM inspector_records", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map(|count| count.unwrap_or_default().try_into().unwrap_or(usize::MAX))
        .map_err(|error| Error::InspectorPersistence(error.to_string()))
}
