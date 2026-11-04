use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Error recorded when a job is resolved because its worker disappeared.
const INTERRUPTED: &str = "interrupted: the server stopped while this job was running";

use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::db::Database;
use crate::error::{KernelError, Result};
use crate::events::{Event, EventBus};
use crate::migrations::Migration;
use crate::time::now_secs;

/// Job lifecycle values persisted in `sys_jobs.status`.
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
    pub const CANCELED: &str = "canceled";
}

/// A persisted job. `payload` is opaque JSON interpreted by the handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub payload_version: i64,
    pub dedupe_key: Option<String>,
    pub status: String,
    pub priority: i64,
    pub progress: f64,
    pub message: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub available_at: i64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub cancel_requested: bool,
}

/// Persistent queue. Cloning is cheap; all clones share the same DB/event bus
/// and cancellation flags.
#[derive(Clone)]
pub struct JobQueue {
    db: Database,
    events: EventBus,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl JobQueue {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db,
            events,
            cancel_flags: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// Enqueue a job. When `dedupe_key` matches a queued/running job of the same
    /// kind, the existing job id is returned instead of creating a duplicate.
    pub fn enqueue(
        &self,
        kind: &str,
        payload: &str,
        dedupe_key: Option<&str>,
        max_attempts: i64,
    ) -> Result<String> {
        self.enqueue_with_priority(kind, payload, dedupe_key, max_attempts, 0)
    }

    /// Enqueue with an explicit priority. `claim` orders by
    /// `priority DESC, created_at ASC`, so short follow-up work (thumbnails) can
    /// overtake a long backlog (bulk probes) instead of starving behind it.
    pub fn enqueue_with_priority(
        &self,
        kind: &str,
        payload: &str,
        dedupe_key: Option<&str>,
        max_attempts: i64,
        priority: i64,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let max_attempts = max_attempts.max(1);

        let (job_id, created) = self.db.with_write(|connection| {
            let transaction = connection.transaction()?;

            if let Some(key) = dedupe_key {
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM sys_jobs
                         WHERE kind = ?1 AND dedupe_key = ?2
                           AND status IN ('queued', 'running')
                         LIMIT 1",
                        params![kind, key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing) = existing {
                    transaction.commit()?;
                    return Ok((existing, false));
                }
            }

            transaction.execute(
                "INSERT INTO sys_jobs (
                     id, kind, payload, payload_version, dedupe_key, status,
                     priority, progress, attempts, max_attempts, available_at,
                     created_at, cancel_requested
                 ) VALUES (?1, ?2, ?3, 1, ?4, 'queued', ?5, 0, 0, ?6, ?7, ?8, 0)",
                params![
                    id,
                    kind,
                    payload,
                    dedupe_key,
                    priority,
                    max_attempts,
                    now,
                    now
                ],
            )?;
            transaction.commit()?;
            Ok((id.clone(), true))
        })?;

        if created {
            self.events.publish(Event::JobQueued {
                job_id: job_id.clone(),
                kind: kind.to_string(),
            });
        }
        Ok(job_id)
    }

    /// Atomically claim one queued job and mark it running.
    ///
    /// Uses `BEGIN IMMEDIATE` so two workers cannot both select the same row
    /// and then lose the upgrade to a write lock (the vtx prototype's bug).
    pub fn claim(&self, worker_id: &str, lease_secs: i64) -> Result<Option<JobRecord>> {
        let job = self.db.with_write(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now = now_secs();

            let job = transaction
                .query_row(
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM sys_jobs
                         WHERE status = 'queued' AND available_at <= ?1
                         ORDER BY priority DESC, created_at ASC
                         LIMIT 1"
                    ),
                    [now],
                    row_to_job,
                )
                .optional()?;

            let Some(mut job) = job else {
                transaction.commit()?;
                return Ok(None);
            };

            let updated = transaction.execute(
                "UPDATE sys_jobs
                 SET status = 'running',
                     lease_owner = ?1,
                     lease_expires_at = ?2,
                     started_at = COALESCE(started_at, ?3),
                     attempts = attempts + 1
                 WHERE id = ?4 AND status = 'queued'",
                params![worker_id, now + lease_secs, now, job.id],
            )?;
            if updated == 0 {
                transaction.commit()?;
                return Ok(None);
            }
            transaction.commit()?;

            job.status = status::RUNNING.to_string();
            job.lease_owner = Some(worker_id.to_string());
            job.lease_expires_at = Some(now + lease_secs);
            job.started_at = Some(job.started_at.unwrap_or(now));
            job.attempts += 1;
            Ok(Some(job))
        })?;

        if let Some(job) = &job {
            self.cancel_flag(&job.id);
        }
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Result<Option<JobRecord>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    &format!("SELECT {JOB_COLUMNS} FROM sys_jobs WHERE id = ?1"),
                    [id],
                    row_to_job,
                )
                .optional()?)
        })
    }

    pub fn list_recent(&self, limit: i64) -> Result<Vec<JobRecord>> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM sys_jobs ORDER BY created_at DESC LIMIT ?1"
            ))?;
            let rows = statement.query_map([limit.max(1)], row_to_job)?;
            let mut jobs = Vec::new();
            for row in rows {
                jobs.push(row?);
            }
            Ok(jobs)
        })
    }

    /// Update progress (0..1) for a running job and publish an event.
    pub fn update_progress(&self, id: &str, progress: f64, message: Option<&str>) -> Result<()> {
        let progress = progress.clamp(0.0, 1.0);
        let updated = self.db.with_write(|connection| {
            Ok(connection.execute(
                "UPDATE sys_jobs SET progress = ?1, message = ?2
                 WHERE id = ?3 AND status = 'running'",
                params![progress, message, id],
            )?)
        })?;
        if updated > 0 {
            self.events.publish(Event::JobProgress {
                job_id: id.to_string(),
                progress,
                message: message.map(str::to_string),
            });
        }
        Ok(())
    }

    /// Mark a running job succeeded. No-op when the job is no longer running.
    pub fn complete(&self, id: &str, result: &str) -> Result<()> {
        let kind = self.kind_of(id)?;
        let now = now_secs();
        let updated = self.db.with_write(|connection| {
            Ok(connection.execute(
                "UPDATE sys_jobs
                 SET status = 'succeeded', progress = 1.0, result = ?1,
                     finished_at = ?2, lease_owner = NULL, lease_expires_at = NULL
                 WHERE id = ?3 AND status = 'running'",
                params![result, now, id],
            )?)
        })?;
        if updated > 0 {
            self.events.publish(Event::JobFinished {
                job_id: id.to_string(),
                kind,
            });
            self.forget_cancel_flag(id);
        }
        Ok(())
    }

    /// Mark a running job failed, retrying with backoff while attempts remain.
    pub fn fail(&self, id: &str, error: &str) -> Result<()> {
        let now = now_secs();
        let (kind, retry) = self.db.with_write(|connection| {
            let transaction = connection.transaction()?;
            let record: Option<(String, i64, i64)> = transaction
                .query_row(
                    "SELECT kind, attempts, max_attempts FROM sys_jobs
                     WHERE id = ?1 AND status = 'running'",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((kind, attempts, max_attempts)) = record else {
                transaction.commit()?;
                return Err(KernelError::Job(format!("job '{id}' is not running")));
            };

            let retry = attempts < max_attempts;
            if retry {
                let backoff = backoff_secs(attempts);
                transaction.execute(
                    "UPDATE sys_jobs
                     SET status = 'queued', error = ?1, available_at = ?2,
                         lease_owner = NULL, lease_expires_at = NULL
                     WHERE id = ?3 AND status = 'running'",
                    params![error, now + backoff, id],
                )?;
            } else {
                transaction.execute(
                    "UPDATE sys_jobs
                     SET status = 'failed', error = ?1, finished_at = ?2,
                         lease_owner = NULL, lease_expires_at = NULL
                     WHERE id = ?3 AND status = 'running'",
                    params![error, now, id],
                )?;
            }
            transaction.commit()?;
            Ok((kind, retry))
        })?;

        self.forget_cancel_flag(id);
        if retry {
            self.events.publish(Event::JobQueued {
                job_id: id.to_string(),
                kind,
            });
        } else {
            self.events.publish(Event::JobFailed {
                job_id: id.to_string(),
                kind,
                error: error.to_string(),
            });
        }
        Ok(())
    }

    /// Mark a running job canceled (used by the worker after a handler returns).
    pub fn mark_canceled(&self, id: &str) -> Result<()> {
        let kind = self.kind_of(id)?;
        let now = now_secs();
        let updated = self.db.with_write(|connection| {
            Ok(connection.execute(
                "UPDATE sys_jobs
                 SET status = 'canceled', finished_at = ?1,
                     lease_owner = NULL, lease_expires_at = NULL
                 WHERE id = ?2 AND status = 'running'",
                params![now, id],
            )?)
        })?;
        if updated > 0 {
            self.events.publish(Event::JobCanceled {
                job_id: id.to_string(),
                kind,
            });
        }
        self.forget_cancel_flag(id);
        Ok(())
    }

    /// Request cancellation. Queued jobs become canceled immediately; running
    /// jobs get a flag that the handler observes and the worker finalizes.
    pub fn cancel(&self, id: &str) -> Result<bool> {
        let now = now_secs();
        let (changed, was_queued, kind) = self.db.with_write(|connection| {
            let transaction = connection.transaction()?;
            let record: Option<(String, String)> = transaction
                .query_row(
                    "SELECT kind, status FROM sys_jobs WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((kind, current_status)) = record else {
                transaction.commit()?;
                return Ok((false, false, String::new()));
            };
            match current_status.as_str() {
                status::QUEUED => {
                    transaction.execute(
                        "UPDATE sys_jobs
                         SET status = 'canceled', cancel_requested = 1, finished_at = ?1
                         WHERE id = ?2 AND status = 'queued'",
                        params![now, id],
                    )?;
                    transaction.commit()?;
                    Ok((true, true, kind))
                }
                status::RUNNING => {
                    transaction.execute(
                        "UPDATE sys_jobs SET cancel_requested = 1
                         WHERE id = ?1 AND status = 'running'",
                        [id],
                    )?;
                    transaction.commit()?;
                    Ok((true, false, kind))
                }
                _ => {
                    transaction.commit()?;
                    Ok((false, false, kind))
                }
            }
        })?;

        if changed {
            // Insert the flag when it is missing: a cancel can land before the
            // worker created its flag, and the flag must carry the canceled
            // state to the handler either way.
            self.cancel_flag(id).store(true, Ordering::SeqCst);
            if was_queued {
                self.events.publish(Event::JobCanceled {
                    job_id: id.to_string(),
                    kind,
                });
            }
        }
        Ok(changed)
    }

    pub fn is_cancel_requested(&self, id: &str) -> Result<bool> {
        self.db.with_read(|connection| {
            let requested: Option<i64> = connection
                .query_row(
                    "SELECT cancel_requested FROM sys_jobs WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(requested.unwrap_or(0) != 0)
        })
    }

    /// Release a running job back to the queue (graceful shutdown).
    ///
    /// The attempt increment from `claim` is undone, so shutting down does not
    /// consume the job's retry budget. Only the worker that owns the lease may
    /// release it.
    pub fn release(&self, id: &str, worker_id: &str) -> Result<bool> {
        let now = now_secs();
        let (released, kind) = self.db.with_write(|connection| {
            let transaction = connection.transaction()?;
            let record: Option<(String, i64)> = transaction
                .query_row(
                    "SELECT kind, attempts FROM sys_jobs
                     WHERE id = ?1 AND status = 'running' AND lease_owner = ?2",
                    params![id, worker_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((kind, attempts)) = record else {
                transaction.commit()?;
                return Ok((false, String::new()));
            };
            transaction.execute(
                "UPDATE sys_jobs
                 SET status = 'queued', lease_owner = NULL, lease_expires_at = NULL,
                     available_at = ?1, attempts = ?2, started_at = NULL
                 WHERE id = ?3 AND status = 'running'",
                params![now, (attempts - 1).max(0), id],
            )?;
            transaction.commit()?;
            Ok((true, kind))
        })?;

        if released {
            self.forget_cancel_flag(id);
            self.events.publish(Event::JobQueued {
                job_id: id.to_string(),
                kind,
            });
        }
        Ok(released)
    }

    /// Renew a lease. Returns false when the worker no longer owns the job.
    pub fn renew_lease(&self, id: &str, worker_id: &str, lease_secs: i64) -> Result<bool> {
        let updated = self.db.with_write(|connection| {
            Ok(connection.execute(
                "UPDATE sys_jobs SET lease_expires_at = ?1
                 WHERE id = ?2 AND status = 'running' AND lease_owner = ?3",
                params![now_secs() + lease_secs, id, worker_id],
            )?)
        })?;
        Ok(updated > 0)
    }

    /// Requeue (or fail) jobs whose lease expired, incrementing attempts so a
    /// poison job cannot be requeued forever.
    pub fn reclaim_expired(&self) -> Result<usize> {
        let now = now_secs();
        let (reclaimed, events) = self.db.with_write(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let rows: Vec<(String, String, i64, i64)> = {
                let mut statement = transaction.prepare(
                    "SELECT id, kind, attempts, max_attempts FROM sys_jobs
                     WHERE status = 'running'
                       AND lease_expires_at IS NOT NULL
                       AND lease_expires_at < ?1",
                )?;
                let rows = statement.query_map([now], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };

            let mut events = Vec::new();
            for (id, kind, attempts, max_attempts) in &rows {
                let next_attempt = attempts + 1;
                if next_attempt >= *max_attempts {
                    transaction.execute(
                        "UPDATE sys_jobs
                         SET status = 'failed', attempts = ?1,
                             error = 'interrupted: the server stopped while this job was running',
                             finished_at = ?2,
                             lease_owner = NULL, lease_expires_at = NULL
                         WHERE id = ?3 AND status = 'running'",
                        params![next_attempt, now, id],
                    )?;
                    events.push(Event::JobFailed {
                        job_id: id.clone(),
                        kind: kind.clone(),
                        error: INTERRUPTED.to_string(),
                    });
                } else {
                    transaction.execute(
                        "UPDATE sys_jobs
                         SET status = 'queued', attempts = ?1, available_at = ?2,
                             error = NULL, lease_owner = NULL,
                             lease_expires_at = NULL
                         WHERE id = ?3 AND status = 'running'",
                        params![next_attempt, now, id],
                    )?;
                    events.push(Event::JobQueued {
                        job_id: id.clone(),
                        kind: kind.clone(),
                    });
                }
            }
            transaction.commit()?;
            Ok((rows.len(), events))
        })?;

        for event in events {
            self.events.publish(event);
        }
        Ok(reclaimed)
    }

    /// Cancellation flag for a job. A claimed job gets a fresh flag; the
    /// handler observes it through [`JobContext::is_cancelled`].
    pub fn cancel_flag(&self, id: &str) -> Arc<AtomicBool> {
        let mut flags = self
            .cancel_flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        flags
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    fn forget_cancel_flag(&self, id: &str) {
        if let Ok(mut flags) = self.cancel_flags.lock() {
            flags.remove(id);
        }
    }

    fn kind_of(&self, id: &str) -> Result<String> {
        self.db.with_read(|connection| {
            connection
                .query_row("SELECT kind FROM sys_jobs WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?
                .ok_or_else(|| KernelError::NotFound(format!("job '{id}'")))
        })
    }
}

/// Kernel-owned migrations (the job table).
pub fn migrations() -> &'static [Migration] {
    &[Migration {
        version: 1,
        name: "jobs",
        sql: include_str!("../migrations/0001_jobs.sql"),
    }]
}

/// Result returned by a handler. Serialized into `sys_jobs.result`.
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub result: serde_json::Value,
}

impl JobOutcome {
    pub fn empty() -> Self {
        Self {
            result: serde_json::Value::Null,
        }
    }

    pub fn json<T: Serialize>(value: T) -> Result<Self> {
        Ok(Self {
            result: serde_json::to_value(value).map_err(|error| {
                KernelError::Job(format!("failed to encode job result: {error}"))
            })?,
        })
    }
}

/// A unit of work registered by a module (the media crate registers scan,
/// probe, thumbnail and transcode handlers).
pub trait JobHandler: Send + Sync {
    fn kind(&self) -> &'static str;
    fn run(&self, context: &JobContext) -> Result<JobOutcome>;
}

#[derive(Clone)]
pub struct JobContext {
    pub job: JobRecord,
    queue: JobQueue,
    cancel: Arc<AtomicBool>,
}

impl JobContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn report_progress(&self, progress: f64, message: Option<&str>) {
        if let Err(error) = self.queue.update_progress(&self.job.id, progress, message) {
            warn!(job_id = %self.job.id, "failed to update job progress: {error}");
        }
    }

    pub fn payload<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.job.payload).map_err(|error| {
            KernelError::Job(format!(
                "invalid payload for job '{}' ({}): {error}",
                self.job.id, self.job.kind
            ))
        })
    }

    pub fn queue(&self) -> &JobQueue {
        &self.queue
    }

    /// Shared cancellation flag, for host operations such as ffprobe/ffmpeg
    /// that poll a flag rather than an async token.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
}

#[derive(Default)]
pub struct JobRegistry {
    handlers: HashMap<String, Arc<dyn JobHandler>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<H: JobHandler + 'static>(&mut self, handler: H) -> &mut Self {
        self.handlers
            .insert(handler.kind().to_string(), Arc::new(handler));
        self
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn JobHandler>> {
        self.handlers.get(kind).cloned()
    }

    pub fn kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self.handlers.keys().cloned().collect();
        kinds.sort();
        kinds
    }
}

/// Worker loop. One task per configured worker; handlers run on blocking
/// threads so scans and ffmpeg never occupy an async executor thread.
pub struct JobWorker {
    queue: JobQueue,
    registry: Arc<JobRegistry>,
    worker_id: String,
    lease: Duration,
    poll: Duration,
}

impl JobWorker {
    pub fn new(
        queue: JobQueue,
        registry: Arc<JobRegistry>,
        worker_id: impl Into<String>,
        lease: Duration,
        poll: Duration,
    ) -> Self {
        Self {
            queue,
            registry,
            worker_id: worker_id.into(),
            lease,
            poll,
        }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        self.spawn_with_shutdown(Arc::new(AtomicBool::new(false)))
    }

    /// Spawn a worker that stops claiming once `shutdown` becomes true.
    ///
    /// A job claimed but not yet started is released back to the queue; a job
    /// already executing is allowed to finish so its result is not lost. On
    /// process exit the lease expiry path is the backstop.
    pub fn spawn_with_shutdown(self, shutdown: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run(shutdown).await })
    }

    async fn run(self, shutdown: Arc<AtomicBool>) {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            match self
                .queue
                .claim(&self.worker_id, self.lease.as_secs() as i64)
            {
                Ok(Some(job)) => {
                    if shutdown.load(Ordering::Relaxed) {
                        let _ = self.queue.release(&job.id, &self.worker_id);
                        break;
                    }
                    self.execute(job).await;
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Ok(None) => {
                    tokio::time::sleep(self.poll).await;
                }
                Err(error) => {
                    error!(worker = %self.worker_id, "job claim failed: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn execute(&self, job: JobRecord) {
        let context = JobContext {
            job: job.clone(),
            queue: self.queue.clone(),
            cancel: self.queue.cancel_flag(&job.id),
        };

        // A cancel that arrived between claim and execution must not be lost.
        if context.is_cancelled() {
            if let Err(error) = self.queue.mark_canceled(&job.id) {
                error!(job_id = %job.id, "failed to mark canceled job: {error}");
            }
            return;
        }

        self.queue.events().publish(Event::JobStarted {
            job_id: job.id.clone(),
            kind: job.kind.clone(),
        });

        let handler = self.registry.get(&job.kind);

        let heartbeat = {
            let queue = self.queue.clone();
            let job_id = job.id.clone();
            let worker_id = self.worker_id.clone();
            let lease = self.lease;
            tokio::spawn(async move {
                let interval = Duration::from_secs((lease.as_secs() / 2).max(1));
                loop {
                    tokio::time::sleep(interval).await;
                    match queue.renew_lease(&job_id, &worker_id, lease.as_secs() as i64) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(error) => {
                            warn!(job_id = %job_id, "lease renewal failed: {error}");
                            break;
                        }
                    }
                }
            })
        };

        let result = match handler {
            Some(handler) => {
                let context = context.clone();
                match tokio::task::spawn_blocking(move || handler.run(&context)).await {
                    Ok(result) => result,
                    Err(error) => Err(KernelError::Job(format!(
                        "job handler panicked or was aborted: {error}"
                    ))),
                }
            }
            None => Err(KernelError::Job(format!(
                "no handler registered for job kind '{}'",
                job.kind
            ))),
        };

        heartbeat.abort();

        if context.is_cancelled()
            || matches!(result, Err(KernelError::Media(ref message)) if message == "transcode canceled")
        {
            let _ = self.queue.mark_canceled(&job.id);
            return;
        }

        match result {
            Ok(outcome) => {
                let result_json =
                    serde_json::to_string(&outcome.result).unwrap_or_else(|_| "null".into());
                if let Err(error) = self.queue.complete(&job.id, &result_json) {
                    error!(job_id = %job.id, "failed to complete job: {error}");
                }
            }
            Err(error) => {
                if let Err(update_error) = self.queue.fail(&job.id, &error.to_string()) {
                    error!(job_id = %job.id, "failed to record job failure: {update_error}");
                }
            }
        }
    }
}

fn backoff_secs(attempts: i64) -> i64 {
    let exponent = attempts.clamp(1, 6) as u32;
    (1i64 << exponent).min(60)
}

const JOB_COLUMNS: &str = "id, kind, payload, payload_version, dedupe_key, status, \
    priority, progress, message, result, error, attempts, max_attempts, available_at, \
    lease_owner, lease_expires_at, created_at, started_at, finished_at, cancel_requested";

fn row_to_job(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
    Ok(JobRecord {
        id: row.get(0)?,
        kind: row.get(1)?,
        payload: row.get(2)?,
        payload_version: row.get(3)?,
        dedupe_key: row.get(4)?,
        status: row.get(5)?,
        priority: row.get(6)?,
        progress: row.get(7)?,
        message: row.get(8)?,
        result: row.get(9)?,
        error: row.get(10)?,
        attempts: row.get(11)?,
        max_attempts: row.get(12)?,
        available_at: row.get(13)?,
        lease_owner: row.get(14)?,
        lease_expires_at: row.get(15)?,
        created_at: row.get(16)?,
        started_at: row.get(17)?,
        finished_at: row.get(18)?,
        cancel_requested: row.get::<_, i64>(19)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations;

    fn setup() -> (tempfile::TempDir, JobQueue) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.db");
        let db = Database::open(path.to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", migrations()).unwrap();
        let queue = JobQueue::new(db, EventBus::new(64));
        (dir, queue)
    }

    #[test]
    fn enqueue_claim_complete() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();
        let job = queue.claim("worker-1", 60).unwrap().unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.status, status::RUNNING);
        assert_eq!(job.attempts, 1);
        queue.complete(&id, r#"{"ok":true}"#).unwrap();
        let job = queue.get(&id).unwrap().unwrap();
        assert_eq!(job.status, status::SUCCEEDED);
        assert_eq!(job.result.as_deref(), Some(r#"{"ok":true}"#));
        assert_eq!(job.progress, 1.0);
    }

    #[test]
    fn dedupe_returns_existing_job() {
        let (_dir, queue) = setup();
        let first = queue.enqueue("probe", "{}", Some("media-1"), 3).unwrap();
        let second = queue.enqueue("probe", "{}", Some("media-1"), 3).unwrap();
        assert_eq!(first, second);
        assert_eq!(queue.list_recent(10).unwrap().len(), 1);
    }

    #[test]
    fn release_requeues_without_consuming_an_attempt() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();
        queue.claim("worker-1", 60).unwrap().unwrap();
        assert!(queue.release(&id, "worker-1").unwrap());
        let job = queue.get(&id).unwrap().unwrap();
        assert_eq!(job.status, status::QUEUED);
        assert_eq!(job.attempts, 0);
        assert!(job.lease_owner.is_none());

        let claimed = queue.claim("worker-2", 60).unwrap().unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.attempts, 1);
    }

    #[test]
    fn cancel_queued_job() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();
        assert!(queue.cancel(&id).unwrap());
        assert!(queue.claim("worker-1", 60).unwrap().is_none());
        assert_eq!(queue.get(&id).unwrap().unwrap().status, status::CANCELED);
    }

    #[test]
    fn cancel_running_job_flips_the_handler_flag() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();
        let claimed = queue.claim("worker-1", 60).unwrap().unwrap();
        // The worker creates this clone when it starts the handler.
        let flag = queue.cancel_flag(&claimed.id);
        assert!(!flag.load(Ordering::SeqCst));

        assert!(queue.cancel(&id).unwrap());
        assert!(
            flag.load(Ordering::SeqCst),
            "cancelling a running job must reach the handler flag"
        );
        // The row stays running until the handler returns and the worker
        // finalises it as canceled.
        assert_eq!(queue.get(&id).unwrap().unwrap().status, status::RUNNING);
        queue.mark_canceled(&id).unwrap();
        assert_eq!(queue.get(&id).unwrap().unwrap().status, status::CANCELED);
    }

    #[test]
    fn cancel_before_the_worker_claims_it_keeps_the_flag() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();
        assert!(queue.cancel(&id).unwrap());
        assert!(queue.claim("worker-1", 60).unwrap().is_none());
        // A flag created afterwards must already say "canceled", so a worker
        // that starts late does not re-run the handler.
        assert!(queue.cancel_flag(&id).load(Ordering::SeqCst));
        assert_eq!(queue.get(&id).unwrap().unwrap().status, status::CANCELED);
    }

    #[test]
    fn higher_priority_jobs_are_claimed_first() {
        let (_dir, queue) = setup();
        let low = queue.enqueue("test", "{}", None, 3).unwrap();
        let high = queue
            .enqueue_with_priority("test", "{}", None, 3, 10)
            .unwrap();
        assert_eq!(queue.claim("worker-1", 60).unwrap().unwrap().id, high);
        assert_eq!(queue.claim("worker-2", 60).unwrap().unwrap().id, low);
    }

    #[test]
    fn failure_retries_with_backoff_then_fails() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 2).unwrap();
        queue.claim("worker-1", 60).unwrap().unwrap();
        queue.fail(&id, "boom").unwrap();
        let job = queue.get(&id).unwrap().unwrap();
        assert_eq!(job.status, status::QUEUED);
        assert_eq!(job.attempts, 1);
        assert!(job.available_at > now_secs());

        // Make the retry immediately available for the test, then exhaust it.
        queue
            .db
            .with_write(|connection| {
                connection.execute("UPDATE sys_jobs SET available_at = 0 WHERE id = ?1", [&id])?;
                Ok(())
            })
            .unwrap();
        queue.claim("worker-1", 60).unwrap().unwrap();
        queue.fail(&id, "boom again").unwrap();
        assert_eq!(queue.get(&id).unwrap().unwrap().status, status::FAILED);
    }

    #[test]
    fn reclaim_expired_lease_requeues_or_fails() {
        let (_dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 1).unwrap();
        queue.claim("worker-1", -1).unwrap().unwrap();
        let reclaimed = queue.reclaim_expired().unwrap();
        assert_eq!(reclaimed, 1);
        assert_eq!(queue.get(&id).unwrap().unwrap().status, status::FAILED);
    }

    #[test]
    fn restart_recovers_a_crashed_job_after_its_lease_expires() {
        let (dir, queue) = setup();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();
        queue.claim("worker-1", 60).unwrap().unwrap();

        // The process "dies" here: a fresh queue on the same database must not
        // pick up the running row, and must leave it alone while the lease is
        // still valid.
        let restarted = JobQueue::new(queue.db.clone(), queue.events().clone());
        assert!(restarted.claim("worker-2", 60).unwrap().is_none());
        assert_eq!(restarted.reclaim_expired().unwrap(), 0);
        assert_eq!(restarted.get(&id).unwrap().unwrap().status, status::RUNNING);

        // Once the old lease expires the job is reclaimed (requeued, attempts
        // incremented) and can be claimed again.
        restarted
            .db
            .with_write(|connection| {
                connection.execute(
                    "UPDATE sys_jobs SET lease_expires_at = 0 WHERE id = ?1",
                    [&id],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(restarted.reclaim_expired().unwrap(), 1);
        let recovered = restarted.get(&id).unwrap().unwrap();
        assert_eq!(recovered.status, status::QUEUED);
        // claim already counted the first attempt, reclaim counts the retry.
        assert_eq!(recovered.attempts, 2);
        let claimed = restarted.claim("worker-2", 60).unwrap().unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.attempts, 3);
        drop(dir);
    }

    struct TestHandler;

    impl JobHandler for TestHandler {
        fn kind(&self) -> &'static str {
            "test"
        }

        fn run(&self, context: &JobContext) -> Result<JobOutcome> {
            context.report_progress(0.5, Some("halfway"));
            JobOutcome::json(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn worker_runs_handler() {
        let (_dir, queue) = setup();
        let mut registry = JobRegistry::new();
        registry.register(TestHandler);
        let worker = JobWorker::new(
            queue.clone(),
            Arc::new(registry),
            "worker-1",
            Duration::from_secs(2),
            Duration::from_millis(20),
        );
        let handle = worker.spawn();
        let id = queue.enqueue("test", "{}", None, 3).unwrap();

        for _ in 0..100 {
            if let Some(job) = queue.get(&id).unwrap() {
                if job.status == status::SUCCEEDED || job.status == status::FAILED {
                    assert_eq!(job.status, status::SUCCEEDED);
                    assert_eq!(job.result.as_deref(), Some(r#"{"ok":true}"#));
                    handle.abort();
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.abort();
        panic!("job did not finish in time");
    }
}
