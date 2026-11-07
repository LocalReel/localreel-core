//! Transcode budget, playback sessions and the idle reaper.
//!
//! A NAS has one CPU and one memory budget; letting every viewer start their own
//! ffmpeg is how a media server becomes the reason the household network is
//! slow. [`TranscodeGate`] is the single budget:
//!
//! - the kernel worker asks it for a slot before running a `transcode` or
//!   `hls_package` job, so a saturated budget leaves those jobs `queued` while
//!   scans and probes keep flowing;
//! - a slot is released automatically when the handler returns, including when
//!   it panics or is cancelled;
//! - playback sessions remember when a media item was last touched, and the
//!   reaper cancels work nobody is watching any more.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use localreel_kernel::jobs::{Admission, JobCapacity, JobQueue};
use serde::Serialize;
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// Job kinds that draw from the budget. Everything else runs unmetered.
pub const LIMITED_KINDS: &[&str] = &["transcode", "hls_package"];

/// Below this much free memory a new encode is refused outright.
const MEMORY_FLOOR_BYTES: u64 = 200 * 1024 * 1024;
/// …and never accept less than this share of total memory.
const MEMORY_FLOOR_SHARE: f64 = 0.05;

/// What the admin health endpoint reports about the budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranscodeStatus {
    /// Jobs currently holding a slot.
    pub running: usize,
    /// Jobs waiting for one (persisted queue rows of a limited kind).
    pub queued: usize,
    /// Configured maximum.
    pub limit: usize,
    /// Live playback sessions.
    pub sessions: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Sessions dropped for being idle.
    pub sessions: usize,
    /// Jobs cancelled because of them.
    pub canceled: usize,
}

struct Session {
    kind: String,
    job_id: Option<String>,
    last_accessed: Instant,
}

/// The single transcode budget of this process.
pub struct TranscodeGate {
    limit: usize,
    permits: Arc<Semaphore>,
    idle: Duration,
    sessions: Mutex<HashMap<i64, Session>>,
}

impl TranscodeGate {
    pub fn new(limit: usize, idle: Duration) -> Self {
        let limit = limit.max(1);
        Self {
            limit,
            permits: Arc::new(Semaphore::new(limit)),
            idle,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Whether `kind` draws from the budget.
    pub fn is_limited(kind: &str) -> bool {
        LIMITED_KINDS.contains(&kind)
    }

    pub fn running(&self) -> usize {
        self.limit - self.permits.available_permits()
    }

    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Compose the health payload. `queued` comes from the job table, which only
    /// the caller can query.
    pub fn status(&self, queued: usize) -> TranscodeStatus {
        TranscodeStatus {
            running: self.running(),
            queued,
            limit: self.limit,
            sessions: self.session_count(),
        }
    }

    /// Record that `media_id` was played. Creates the session on first sight.
    pub fn touch(&self, media_id: i64) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .entry(media_id)
            .and_modify(|session| session.last_accessed = Instant::now())
            .or_insert_with(|| Session {
                kind: "playback".to_string(),
                job_id: None,
                last_accessed: Instant::now(),
            });
    }

    /// Remember the job a media item is waiting on, so the reaper can cancel it.
    pub fn attach_job(&self, media_id: i64, kind: &str, job_id: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let session = sessions.entry(media_id).or_insert_with(|| Session {
            kind: kind.to_string(),
            job_id: None,
            last_accessed: Instant::now(),
        });
        session.kind = kind.to_string();
        session.job_id = Some(job_id.to_string());
        session.last_accessed = Instant::now();
    }

    /// Forget a session, for example when playback was closed cleanly.
    pub fn close(&self, media_id: i64) {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&media_id);
    }

    /// Drop every session idle for longer than the timeout, cancelling the job it
    /// was waiting on. Artifacts of a cancelled job are removed by the job
    /// itself; finished cache files belong to `cache_gc`.
    pub fn reap_idle(&self, jobs: &JobQueue) -> ReapReport {
        let expired: Vec<i64> = {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            sessions
                .iter()
                .filter(|(_, session)| now.duration_since(session.last_accessed) >= self.idle)
                .map(|(media_id, _)| *media_id)
                .collect()
        };

        let mut report = ReapReport::default();
        for media_id in expired {
            let session = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&media_id);
            let Some(session) = session else {
                continue;
            };
            report.sessions += 1;
            let Some(job_id) = session.job_id else {
                continue;
            };
            match jobs.cancel(&job_id) {
                Ok(true) => {
                    report.canceled += 1;
                    info!(
                        media_id,
                        kind = %session.kind,
                        job_id = %job_id,
                        "cancelled a transcode nobody is watching"
                    );
                }
                Ok(false) => {}
                Err(error) => warn!(
                    media_id,
                    job_id = %job_id,
                    "failed to cancel an idle transcode: {error}"
                ),
            }
        }
        report
    }
}

impl JobCapacity for TranscodeGate {
    fn admit(&self, kind: &str) -> Option<Admission> {
        if !Self::is_limited(kind) {
            return Some(Admission::unlimited());
        }
        match self.permits.clone().try_acquire_owned() {
            Ok(permit) => Some(Admission::new(move || drop(permit))),
            Err(_) => None,
        }
    }

    fn saturated(&self) -> Vec<String> {
        if self.permits.available_permits() == 0 {
            LIMITED_KINDS.iter().map(|kind| kind.to_string()).collect()
        } else {
            Vec::new()
        }
    }
}

/// Whether the machine is too short on memory to start another encode.
///
/// Only Linux exposes a cheap "available" figure (`/proc/meminfo`); elsewhere
/// this is a no-op rather than a guess.
pub fn memory_pressure() -> bool {
    pressure_from(read_available_memory())
}

fn pressure_from(memory: Option<(u64, u64)>) -> bool {
    let Some((available, total)) = memory else {
        return false;
    };
    let floor = MEMORY_FLOOR_BYTES.max((total as f64 * MEMORY_FLOOR_SHARE) as u64);
    available < floor
}

#[cfg(target_os = "linux")]
fn read_available_memory() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let key = fields.next()?;
        let value = fields.next()?.parse::<u64>().ok()? * 1024;
        match key {
            "MemTotal:" => total = Some(value),
            "MemAvailable:" => available = Some(value),
            _ => {}
        }
    }
    Some((available?, total?))
}

#[cfg(not(target_os = "linux"))]
fn read_available_memory() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_admits_up_to_its_limit() {
        let gate = TranscodeGate::new(2, Duration::from_secs(60));
        assert_eq!(gate.running(), 0);
        let first = gate.admit("transcode").expect("first slot");
        assert_eq!(gate.running(), 1);
        let second = gate.admit("hls_package").expect("second slot");
        assert_eq!(gate.running(), 2);
        assert!(gate.admit("transcode").is_none(), "the third must wait");
        assert_eq!(gate.saturated(), LIMITED_KINDS);

        // Dropping a slot is what frees it, so a panicking handler cannot leak.
        drop(first);
        assert_eq!(gate.running(), 1);
        assert!(gate.admit("transcode").is_some());
        drop(second);
    }

    #[test]
    fn unrelated_kinds_are_not_metered() {
        let gate = TranscodeGate::new(1, Duration::from_secs(60));
        let _held = gate.admit("transcode").unwrap();
        assert!(!gate.saturated().is_empty());
        // A scan or probe never asks the budget for permission.
        let admission = gate.admit("scan_source").expect("unmetered");
        drop(admission);
        assert_eq!(gate.running(), 1, "only the transcode holds a slot");
    }

    #[test]
    fn sessions_are_replaced_and_refreshed() {
        let gate = TranscodeGate::new(2, Duration::from_secs(60));
        gate.touch(7);
        assert_eq!(gate.session_count(), 1);
        gate.touch(7);
        assert_eq!(gate.session_count(), 1, "a second play reuses the session");
        gate.attach_job(7, "transcode", "job-1");
        assert_eq!(gate.session_count(), 1);
        gate.close(7);
        assert_eq!(gate.session_count(), 0);
    }

    #[test]
    fn the_health_snapshot_reports_the_budget() {
        let gate = TranscodeGate::new(2, Duration::from_secs(60));
        let _held = gate.admit("transcode").unwrap();
        gate.touch(1);
        let status = gate.status(3);
        assert_eq!(status.running, 1);
        assert_eq!(status.queued, 3);
        assert_eq!(status.limit, 2);
        assert_eq!(status.sessions, 1);
    }

    #[test]
    fn memory_pressure_uses_both_a_floor_and_a_share() {
        // Below the absolute floor: refuse.
        assert!(pressure_from(Some((
            150 * 1024 * 1024,
            64 * 1024 * 1024 * 1024
        ))));
        // Plenty of memory: allow.
        assert!(!pressure_from(Some((
            8 * 1024 * 1024 * 1024,
            16 * 1024 * 1024 * 1024
        ))));
        // Small machine, 4 % free: the share floor applies.
        assert!(pressure_from(Some((
            400 * 1024 * 1024,
            10 * 1024 * 1024 * 1024
        ))));
        // Unknown platform: never block.
        assert!(!pressure_from(None));
    }

    #[test]
    fn idle_sessions_are_reaped_and_their_job_cancelled() {
        use localreel_kernel::db::Database;
        use localreel_kernel::events::EventBus;
        use localreel_kernel::migrations;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("gate.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        let jobs = JobQueue::new(db, EventBus::new(16));

        let gate = TranscodeGate::new(1, Duration::from_millis(50));
        let job_id = jobs.enqueue("transcode", "{}", None, 3).unwrap();
        gate.touch(42);
        gate.attach_job(42, "transcode", &job_id);

        std::thread::sleep(Duration::from_millis(80));
        let report = gate.reap_idle(&jobs);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.canceled, 1);
        assert_eq!(gate.session_count(), 0);
        assert_eq!(
            jobs.get(&job_id).unwrap().unwrap().status,
            localreel_kernel::jobs::status::CANCELED
        );

        // Nothing left to reap.
        assert_eq!(gate.reap_idle(&jobs), ReapReport::default());
    }

    #[test]
    fn an_active_session_is_not_reaped() {
        use localreel_kernel::db::Database;
        use localreel_kernel::events::EventBus;
        use localreel_kernel::migrations;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("gate.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        let jobs = JobQueue::new(db, EventBus::new(16));

        let gate = TranscodeGate::new(1, Duration::from_secs(60));
        gate.touch(9);
        assert_eq!(gate.reap_idle(&jobs), ReapReport::default());
        assert_eq!(gate.session_count(), 1);
    }
}
