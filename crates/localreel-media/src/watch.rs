//! Filesystem watching for local sources.
//!
//! A media server that only notices new files when someone presses "scan" is
//! not a media server. Every enabled local source gets one recursive OS watcher;
//! the events are debounced and turned into the same deduplicated `scan_source`
//! job the admin UI enqueues, so dropping a whole season into a folder collapses
//! into a single incremental scan.
//!
//! Watching is best effort by design. A source that cannot be watched — a plugin
//! URI, an SMB/NFS mount the platform refuses to subscribe to, a kernel without
//! inotify slots left — is skipped and reported through the admin health
//! endpoint. It never fails the request that touched the source and never stops
//! the server.

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::Mutex;
use std::time::Duration;

use localreel_kernel::error::Result;
use localreel_kernel::jobs::JobQueue;
use localreel_kernel::storage::MediaUri;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::Serialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::service::MediaService;

/// How long a burst of filesystem events folds into one scan.
///
/// Copying a folder into the library produces hundreds of events. Two seconds is
/// long enough for the copy to settle — a scan started mid-copy would record
/// half-written files — and short enough that a single dropped file is picked up
/// while the user is still looking at the library.
pub const DEBOUNCE: Duration = Duration::from_secs(2);

/// What the admin health endpoint reports about watching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatcherStatus {
    /// Whether watching is configured at all (`[scan] watch` / `LOCALREEL_WATCH`).
    pub enabled: bool,
    /// Sources with a live OS subscription.
    pub active: usize,
    /// Sources that are not watched, as `kind:uri` (with a reason when the
    /// subscription itself failed).
    pub skipped: Vec<String>,
}

/// Owns one OS watcher and one debounce task per enabled local source.
pub struct WatcherManager {
    media: MediaService,
    jobs: JobQueue,
    enabled: bool,
    debounce: Duration,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    active: BTreeMap<i64, ActiveWatcher>,
    skipped: Vec<String>,
}

struct ActiveWatcher {
    /// The source URI the subscription was built for; a remap restarts it.
    uri: String,
    /// Dropping this stops the OS subscription.
    _watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

impl Drop for ActiveWatcher {
    fn drop(&mut self) {
        // Dropping a tokio `JoinHandle` detaches the task instead of stopping
        // it, so the debounce loop has to be cancelled explicitly.
        self.task.abort();
    }
}

impl WatcherManager {
    pub fn new(media: MediaService, jobs: JobQueue, enabled: bool) -> Self {
        Self::with_debounce(media, jobs, enabled, DEBOUNCE)
    }

    pub fn with_debounce(
        media: MediaService,
        jobs: JobQueue,
        enabled: bool,
        debounce: Duration,
    ) -> Self {
        Self {
            media,
            jobs,
            enabled,
            debounce,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn status(&self) -> WatcherStatus {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        WatcherStatus {
            enabled: self.enabled,
            active: inner.active.len(),
            skipped: inner.skipped.clone(),
        }
    }

    /// Reconcile the running subscriptions with the current source list.
    ///
    /// Called at startup and after every source mutation (add, enable, disable,
    /// archive, remap, delete). Sources that are unchanged keep their watcher;
    /// removed, disabled or remapped ones are stopped.
    pub fn refresh(&self) -> Result<()> {
        let sources = self.media.list_sources_with_archived(false)?;
        let mut desired: BTreeMap<i64, (String, std::path::PathBuf)> = BTreeMap::new();
        let mut skipped: Vec<String> = Vec::new();

        if self.enabled {
            for source in sources {
                if !source.enabled {
                    continue;
                }
                if source.kind != "local" {
                    skipped.push(format!("{}:{}", source.kind, source.uri));
                    continue;
                }
                match MediaUri::parse(&source.uri) {
                    Ok(MediaUri::Local(path)) => {
                        desired.insert(source.id, (source.uri.clone(), path));
                    }
                    _ => skipped.push(format!("{}:{}", source.kind, source.uri)),
                }
            }
        } else {
            skipped.push("disabled".to_string());
        }

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let stale: Vec<i64> = inner
            .active
            .iter()
            .filter(|(id, watcher)| match desired.get(id) {
                Some((uri, _)) => uri != &watcher.uri,
                None => true,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            inner.active.remove(&id);
        }

        for (id, (uri, path)) in desired {
            if inner.active.contains_key(&id) {
                continue;
            }
            match self.subscribe(id, &path, &uri) {
                Ok(watcher) => {
                    inner.active.insert(id, watcher);
                }
                Err(reason) => {
                    warn!(
                        source_id = id,
                        path = %path.display(),
                        "source cannot be watched: {reason}"
                    );
                    skipped.push(format!("local:{uri} ({reason})"));
                }
            }
        }

        inner.skipped = skipped;
        Ok(())
    }

    fn subscribe(
        &self,
        source_id: i64,
        root: &Path,
        uri: &str,
    ) -> std::result::Result<ActiveWatcher, String> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "watching needs a running async runtime".to_string())?;

        let (sender, receiver) = mpsc::unbounded_channel::<()>();
        let sender: UnboundedSender<()> = sender;
        let mut watcher = notify::recommended_watcher(
            move |event: std::result::Result<notify::Event, notify::Error>| {
                let Ok(event) = event else {
                    return;
                };
                if !matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    return;
                }
                if !event.paths.iter().any(|path| is_relevant_path(path)) {
                    return;
                }
                // The receiver is gone once the source is no longer watched.
                let _ = sender.send(());
            },
        )
        .map_err(|error| format!("cannot create a filesystem watcher: {error}"))?;

        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|error| format!("the platform refused to watch it: {error}"))?;

        let task = handle.spawn(debounce_loop(
            receiver,
            self.jobs.clone(),
            source_id,
            self.debounce,
        ));
        Ok(ActiveWatcher {
            uri: uri.to_string(),
            _watcher: watcher,
            task,
        })
    }
}

/// Wait for a change, then wait until the burst settles, then enqueue one scan.
async fn debounce_loop(
    mut events: UnboundedReceiver<()>,
    jobs: JobQueue,
    source_id: i64,
    debounce: Duration,
) {
    while events.recv().await.is_some() {
        let settle = tokio::time::Instant::now() + debounce;
        loop {
            tokio::select! {
                next = events.recv() => {
                    if next.is_none() {
                        return;
                    }
                }
                _ = tokio::time::sleep_until(settle) => break,
            }
        }
        enqueue_scan(&jobs, source_id);
    }
}

/// The same job, payload and dedupe key the admin "scan" endpoint uses, so a
/// manual scan and a filesystem change cannot produce two competing scans.
fn enqueue_scan(jobs: &JobQueue, source_id: i64) {
    let payload = serde_json::json!({ "source_id": source_id }).to_string();
    match jobs.enqueue(
        "scan_source",
        &payload,
        Some(&format!("scan:{source_id}")),
        1,
    ) {
        Ok(job_id) => info!(
            source_id,
            job_id = %job_id,
            "a filesystem change triggered a rescan"
        ),
        Err(error) => warn!(
            source_id,
            "failed to enqueue a scan after a filesystem change: {error}"
        ),
    }
}

/// Paths that never signal a library change.
///
/// Thumbnails, editors' lock files and our own cache writes would otherwise wake
/// the scanner for nothing; a scan is cheap but not free on a NAS.
///
/// Only the file name is judged for a leading dot or tilde: a library is allowed
/// to live under a dot-named directory (and the test fixtures do), while
/// `.hidden.mp4` and `~$lock.docx` are noise wherever they are.
fn is_relevant_path(path: &Path) -> bool {
    const IGNORED_DIRECTORIES: &[&str] = &[
        "@eaDir",
        "#recycle",
        "$RECYCLE.BIN",
        ".stfolder",
        ".stversions",
        "node_modules",
    ];
    for component in path.components() {
        if let Component::Normal(name) = component {
            let text = name.to_string_lossy();
            if IGNORED_DIRECTORIES
                .iter()
                .any(|ignored| text.eq_ignore_ascii_case(ignored))
            {
                return false;
            }
        }
    }

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() || name.starts_with('.') || name.starts_with('~') {
        return false;
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => !matches!(
            extension.to_ascii_lowercase().as_str(),
            "tmp"
                | "temp"
                | "part"
                | "partial"
                | "crdownload"
                | "swp"
                | "swx"
                | "db-wal"
                | "db-shm"
                | "db-journal"
        ),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use localreel_kernel::db::Database;
    use localreel_kernel::events::EventBus;
    use localreel_kernel::migrations;

    use crate::model::Source;

    fn path(value: &str) -> PathBuf {
        PathBuf::from(value)
    }

    fn database(dir: &Path) -> Database {
        let db = Database::open(dir.join("watch.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        migrations::apply(&db, "media", crate::migrations::list()).unwrap();
        db
    }

    fn fixture(dir: &Path, library: &Path) -> (MediaService, JobQueue, Source) {
        let db = database(dir);
        let media = MediaService::new(db.clone());
        let source = media.add_source(library.to_str().unwrap()).unwrap();
        let jobs = JobQueue::new(db, EventBus::new(16));
        (media, jobs, source)
    }

    #[test]
    fn temporary_and_hidden_files_are_not_relevant() {
        assert!(!is_relevant_path(&path("C:/lib/.hidden.mp4")));
        assert!(!is_relevant_path(&path("C:/lib/poster.jpg.tmp")));
        assert!(!is_relevant_path(&path("C:/lib/~$draft.docx")));
        assert!(!is_relevant_path(&path("C:/lib/@eaDir/thumb.jpg")));
        assert!(!is_relevant_path(&path("C:/lib/movie.mp4.crdownload")));
        assert!(is_relevant_path(&path("C:/lib/movie.mp4")));
        assert!(is_relevant_path(&path("C:/lib/Season 1")));
    }

    #[test]
    fn a_disabled_manager_watches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let (media, jobs, _source) = fixture(dir.path(), library.path());
        let manager = WatcherManager::new(media, jobs, false);

        // No runtime here on purpose: a disabled manager must not even try.
        manager.refresh().unwrap();
        assert_eq!(
            manager.status(),
            WatcherStatus {
                enabled: false,
                active: 0,
                skipped: vec!["disabled".to_string()],
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_burst_of_changes_enqueues_exactly_one_scan() {
        let dir = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let (media, jobs, source) = fixture(dir.path(), library.path());
        let manager =
            WatcherManager::with_debounce(media, jobs.clone(), true, Duration::from_millis(300));
        manager.refresh().unwrap();
        assert_eq!(
            manager.status().active,
            1,
            "the local source must be watched"
        );

        for index in 0..5 {
            std::fs::write(library.path().join(format!("episode-{index}.mp4")), b"x").unwrap();
        }

        // The debounce folds the burst into a single job; a scan started mid-copy
        // would otherwise record half-written files.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while jobs.list_recent(10).unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;

        let scans: Vec<_> = jobs
            .list_recent(20)
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == "scan_source")
            .collect();
        assert_eq!(scans.len(), 1, "the burst must collapse into one scan job");
        assert_eq!(
            scans[0].dedupe_key.as_deref(),
            Some(format!("scan:{}", source.id).as_str())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_source_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let (media, jobs, source) = fixture(dir.path(), library.path());
        media.set_source_enabled(source.id, false).unwrap();
        let manager = WatcherManager::new(media, jobs, true);
        manager.refresh().unwrap();
        assert_eq!(manager.status().active, 0);
        assert!(manager.status().skipped.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remapped_source_restarts_its_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let (media, jobs, source) = fixture(dir.path(), first.path());
        let manager = WatcherManager::new(media.clone(), jobs, true);
        manager.refresh().unwrap();
        assert_eq!(manager.status().active, 1);

        media
            .remap_source(source.id, second.path().to_str().unwrap(), false)
            .unwrap();
        manager.refresh().unwrap();
        assert_eq!(
            manager.status().active,
            1,
            "the watcher must follow the new root"
        );
    }
}
