mod autostart;
mod launch;
mod logging;
mod tray;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing::{info, warn};

use anyhow::Context;
use localreel_api::AppState;
use localreel_kernel::config::{Config, RunMode};
use localreel_kernel::db::Database;
use localreel_kernel::events::{Event, EventBus};
use localreel_kernel::jobs::{JobQueue, JobRegistry, JobWorker};
use localreel_kernel::media_ops::MediaOps;
use localreel_kernel::migrations;
use localreel_media::{
    read_backup_with_limit, restore_backup, MediaService, MAX_BACKUP_RESTORE_BYTES,
};

#[derive(Debug, Parser)]
#[command(name = "localreel", version, about = "Local-first media server")]
struct Cli {
    /// Force a persona instead of detecting it: `desktop` opens the UI and
    /// heals a busy port, `server` stays headless on a fixed port.
    #[arg(long, global = true, value_name = "MODE")]
    mode: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP server (default).
    Serve {
        /// Do not open the web UI in a browser.
        #[arg(long)]
        no_open: bool,
        /// Show a notification-area icon and hide this console (desktop only).
        #[arg(long)]
        tray: bool,
        /// Bind this port instead of the configured one.
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
        /// Bind this host instead of the configured one.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
    },
    /// Apply database migrations and exit.
    Migrate,
    /// Print configuration and environment checks.
    Doctor,
    /// Start LocalReel with the user session (enable/disable/status).
    Autostart {
        #[arg(value_enum, default_value_t = AutostartAction::Status)]
        action: AutostartAction,
    },
    /// Discover media under one configured source (walk/fingerprint/upsert).
    Scan { source_id: i64 },
    /// Rewrite a local source root, preserving media ids/state.
    RemapSource {
        source_id: i64,
        /// New local directory (or file:// URI).
        uri: String,
        /// Validate and report without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Import a copy of a LocalReel v1 database.
    Import {
        /// Path to the legacy `localreel.db` (a copy, never the live file).
        from: PathBuf,
        /// Validate and report without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy legacy video_states onto current media, matched by episode number.
    StateImport {
        /// Path to the legacy `localreel.db` (a copy, never the live file).
        from: PathBuf,
        /// Restrict targets to one current source.
        #[arg(long)]
        source_id: Option<i64>,
        /// Validate and report without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Export a versioned backup of sources and per-media state.
    BackupExport {
        /// Output JSON file.
        out: PathBuf,
    },
    /// Import a backup, merging flags and never lowering progress.
    BackupImport {
        /// Backup JSON file.
        from: PathBuf,
        /// Validate and report without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// One-command restore: sources, scan/probe/thumbnail, then state replay.
    Restore {
        /// Backup JSON file.
        from: PathBuf,
        /// Validate and report without writing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let explicit = match cli.mode.as_deref() {
        Some(raw) => Some(raw.parse::<RunMode>()?),
        None => None,
    };
    let mode = launch::decide(explicit, &launch::Signals::detect());
    logging::init(&localreel_kernel::config::default_log_dir());

    let result = dispatch(cli, mode).await;
    if let Err(error) = &result {
        // A double-clicked window must not vanish before it can be read.
        eprintln!();
        eprintln!("LocalReel could not start:");
        eprintln!("  {error:#}");
        if launch::is_interactive() {
            launch::pause_before_exit();
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum AutostartAction {
    Status,
    Enable,
    Disable,
}
async fn dispatch(cli: Cli, mode: RunMode) -> anyhow::Result<()> {
    match cli.command.unwrap_or(Command::Serve {
        no_open: false,
        tray: false,
        port: None,
        host: None,
    }) {
        Command::Serve {
            no_open,
            tray,
            port,
            host,
        } => serve(mode, no_open, tray, port, host).await,
        Command::Migrate => migrate(),
        Command::Doctor => doctor(),
        Command::Autostart { action } => autostart(action),
        Command::Scan { source_id } => scan(source_id),
        Command::RemapSource {
            source_id,
            uri,
            dry_run,
        } => remap_source(source_id, uri, dry_run),
        Command::Import { from, dry_run } => import(from, dry_run),
        Command::StateImport {
            from,
            source_id,
            dry_run,
        } => state_import(from, source_id, dry_run),
        Command::BackupExport { out } => backup_export(out),
        Command::BackupImport { from, dry_run } => backup_import(from, dry_run),
        Command::Restore { from, dry_run } => restore(from, dry_run),
    }
}

fn open_database(config: &Config) -> anyhow::Result<Database> {
    Database::open(&config.db_path_string(), 2).map_err(Into::into)
}

fn run_migrations(db: &Database) -> anyhow::Result<()> {
    migrations::apply(db, "kernel", localreel_kernel::jobs::migrations())?;
    migrations::apply(db, "media", localreel_media::migrations::list())?;
    info!("database migrations are up to date ({})", db.path());
    Ok(())
}

fn migrate() -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    Ok(())
}

fn autostart(action: AutostartAction) -> anyhow::Result<()> {
    match action {
        AutostartAction::Status => println!("autostart: {}", autostart::status()),
        AutostartAction::Enable => {
            autostart::enable()?;
            println!("autostart: {}", autostart::status());
        }
        AutostartAction::Disable => {
            autostart::disable()?;
            println!("autostart: {}", autostart::status());
        }
    }
    Ok(())
}
fn doctor() -> anyhow::Result<()> {
    let (config, source) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let media_ops = MediaOps::discover(
        config.media.ffmpeg.clone(),
        config.media.ffprobe.clone(),
        config.media.probe_timeout,
        config.media.thumbnail_timeout,
        config.media.transcode_timeout,
    );
    println!("LocalReel doctor");
    println!("  version:   {}", localreel_kernel::VERSION);
    println!("  mode:      {} ({})", config.mode, config.mode_source);
    println!("  autostart: {}", autostart::status());
    println!(
        "  config:    {}",
        source
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "defaults + environment".to_string())
    );
    println!("  host:      {}:{}", config.host, config.port);
    println!("  data dir:  {}", config.data_dir.display());
    println!("  log dir:   {}", config.log_dir.display());
    println!("  database:  {} (ok)", config.db_path.display());
    println!(
        "  ffmpeg:    {} ({})",
        media_ops.ffmpeg_path().display(),
        yes_no(media_ops.available())
    );
    println!("  ffprobe:   {}", media_ops.ffprobe_path().display());
    if !media_ops.available() {
        println!(
            "  hint:      put ffmpeg/ffprobe on PATH, next to localreel.exe, or set LOCALREEL_FFMPEG_DIR"
        );
    }
    let web = if localreel_api::web::is_embedded() {
        "embedded".to_string()
    } else if let Some(dir) = config.resolved_web_dir() {
        format!("disk ({})", dir.display())
    } else {
        "missing (API-only placeholder)".to_string()
    };
    println!("  web:       {web}");
    println!("  workers:   {}", config.jobs.workers);
    Ok(())
}

fn state_import(from: PathBuf, source_id: Option<i64>, dry_run: bool) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let report = localreel_media::migrate_legacy_state(&db, &from, source_id, dry_run)?;
    println!(
        "legacy state import ({}): legacy={} matched={} updated={} unchanged={} ambiguous={} unmatched={} conflicts={}",
        if dry_run { "dry-run" } else { "write" },
        report.legacy_states,
        report.matched,
        report.updated,
        report.unchanged,
        report.ambiguous,
        report.unmatched,
        report.conflicts,
    );
    for detail in report.details.iter().take(20) {
        println!(
            "  {} -> {} [{}] episode={:?} position={:.1}->{:.1}",
            detail.legacy_filename,
            detail.target_filename.as_deref().unwrap_or("<none>"),
            detail.status,
            detail.episode,
            detail.before_position,
            detail.after_position,
        );
    }
    Ok(())
}

fn backup_export(out: PathBuf) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let backup = localreel_media::export_backup(&db)?;
    let bytes = serde_json::to_vec_pretty(&backup)?;
    if bytes.len() as u64 > localreel_media::MAX_BACKUP_BYTES {
        anyhow::bail!(
            "backup is too large: {} bytes (max {})",
            bytes.len(),
            localreel_media::MAX_BACKUP_BYTES
        );
    }
    std::fs::write(&out, &bytes)?;
    println!(
        "backup exported: {} sources, {} states, {} bytes -> {}",
        backup.sources.len(),
        backup.states.len(),
        bytes.len(),
        out.display()
    );
    Ok(())
}

fn backup_import(from: PathBuf, dry_run: bool) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let backup = localreel_media::read_backup(&from)?;
    let report = localreel_media::import_backup(&db, &backup, dry_run)?;
    println!(
        "backup import ({}): sources={} added={} states={} imported={} unchanged={} missing={} warnings={}",
        if dry_run { "dry-run" } else { "write" },
        report.sources,
        report.sources_added,
        report.states,
        report.imported,
        report.unchanged,
        report.missing,
        report.warnings.len(),
    );
    for warning in report.warnings.iter().take(20) {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn restore(from: PathBuf, dry_run: bool) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let backup = read_backup_with_limit(&from, MAX_BACKUP_RESTORE_BYTES)?;
    let media_ops = MediaOps::discover(
        config.media.ffmpeg.clone(),
        config.media.ffprobe.clone(),
        config.media.probe_timeout,
        config.media.thumbnail_timeout,
        config.media.transcode_timeout,
    );
    let cancel = AtomicBool::new(false);
    let mut progress = |_: f32, _: &str| {};
    let report = restore_backup(
        &db,
        localreel_media::restore::RestoreInput {
            backup: &backup,
            media_ops: &media_ops,
            thumbnail_dir: &config.media.thumbnail_dir,
            dry_run,
            cancel: &cancel,
        },
        &mut progress,
    )?;
    println!(
        "restore ({}): sources={} added={} states={} discovered={} probed={} thumbnails={} imported={} missing={}",
        if dry_run { "dry-run" } else { "write" },
        report.sources,
        report.sources_added,
        report.states,
        report.media_discovered,
        report.probed,
        report.thumbnails,
        report.states_imported,
        report.missing,
    );
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn remap_source(source_id: i64, uri: String, dry_run: bool) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let report = MediaService::new(db).remap_source(source_id, &uri, dry_run)?;
    println!(
        "remap source {} ({}): total={} remapped={} missing={} skipped={}",
        report.source_id,
        if dry_run { "dry-run" } else { "write" },
        report.media_total,
        report.remapped,
        report.missing,
        report.skipped,
    );
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn scan(source_id: i64) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let service = MediaService::new(db.clone());
    let source = service
        .get_source(source_id)?
        .ok_or_else(|| anyhow::anyhow!("source {source_id} was not found"))?;
    let result = localreel_media::discover_source(&db, &source)?;
    println!(
        "scan source {} ({}) status={} discovered={} added={} updated={} unchanged={} missing={} failed={}",
        source.id,
        source.uri,
        result.status,
        result.discovered,
        result.added,
        result.updated,
        result.unchanged,
        result.missing,
        result.failed
    );
    Ok(())
}

fn import(from: PathBuf, dry_run: bool) -> anyhow::Result<()> {
    let (config, _) = Config::load_with_source()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let report = localreel_media::import_legacy(&db, &from, dry_run)?;
    println!(
        "legacy import ({}): sources={} media={} states={} missing_files={}",
        if dry_run { "dry-run" } else { "applied" },
        report.sources,
        report.media,
        report.states,
        report.missing_files
    );
    println!(
        "  legacy scan data: runs={} issues={} jobs={} pending_states={}",
        report.scan_runs, report.scan_issues, report.scan_jobs, report.pending_states
    );
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

/// Remove crash-stale streaming backup uploads (jobs normally delete their own).
fn prune_backup_uploads(data_dir: &std::path::Path) {
    let upload_dir = data_dir.join("backup-uploads");
    let Ok(entries) = std::fs::read_dir(&upload_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 24 * 60 * 60);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Ctrl+C is the terminal way out. A service manager and the GUI launcher give
/// the process no console, so the signal can never arrive (and registering the
/// handler reports an error): wait forever instead of shutting down at once.
async fn wait_for_ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
async fn serve(
    mode: RunMode,
    no_open: bool,
    tray_icon: bool,
    port: Option<u16>,
    host: Option<String>,
) -> anyhow::Result<()> {
    let (mut config, source) = Config::load_with_mode(Some(mode))?;
    if let Some(port) = port {
        config.port = port;
    }
    if let Some(host) = host {
        config.host = host;
    }
    config.validate()?;
    config.ensure_dirs()?;

    if let Some(path) = &source {
        info!("loaded configuration from {}", path.display());
    }

    // Desktop: a second double click means "show me the app", not "fail".
    if config.mode.is_desktop() {
        if let Some(running) = launch::probe(&config.host, config.port) {
            let url = launch::web_url(&config.host, config.port);
            println!();
            println!("LocalReel {} is already running.", running.version);
            println!("  Open: {url}");
            if launch::should_open_ui(&config, no_open) {
                if let Err(error) = launch::open_browser(&url) {
                    eprintln!("  Open the address above (no browser could be started: {error}).");
                }
            }
            return Ok(());
        }
    }

    // Bind before the expensive setup so a busy port fails fast and visibly.
    let requested_port = config.port;
    let (listener, bound_port) =
        launch::bind_listener(&config.host, requested_port, config.mode).await?;
    config.port = bound_port;
    if bound_port != requested_port {
        println!("Port {requested_port} is busy; using {bound_port} instead.");
    }
    let url = launch::web_url(&config.host, bound_port);
    launch::print_banner(&config, &url, localreel_kernel::VERSION);

    prune_backup_uploads(&config.data_dir);
    let db = open_database(&config)?;
    run_migrations(&db)?;

    let events = EventBus::new(256);
    let jobs = JobQueue::new(db.clone(), events.clone());

    // Start configured sidecar plugins before anything resolves a source.
    let storage = Arc::new(localreel_media::StorageRegistry::new());
    let mut plugin_runtimes = Vec::new();
    for plugin in config.plugins.iter().filter(|plugin| plugin.enabled) {
        let manifest = localreel_plugin::inspect(&plugin.package)
            .with_context(|| format!("plugin package {}", plugin.package.display()))?;
        let install = config
            .data_dir
            .join("plugins")
            .join(&manifest.id)
            .join(&manifest.version);
        let manifest = localreel_plugin::extract(&plugin.package, &install)?;
        let program = install.join(&manifest.entry.program);
        let runtime = Arc::new(localreel_plugin::PluginRuntime::start(
            &program,
            &manifest,
            &plugin.config,
        )?);
        storage.register(
            manifest.id.clone(),
            Arc::new(localreel_plugin::PluginStorage::new(runtime.clone())),
        );
        info!(id = %manifest.id, version = %manifest.version, "plugin started");
        plugin_runtimes.push(runtime);
    }
    let media = MediaService::with_storage(db.clone(), storage.clone());
    let accounts = localreel_media::AccountService::new(db.clone());
    if let Ok(removed) = accounts.purge_expired_sessions() {
        if removed > 0 {
            info!(removed, "removed expired sessions");
        }
    }
    if accounts.setup_required().unwrap_or(false) {
        println!("  First run: open the UI and set the owner password.");
        println!();
    }
    let media_ops = Arc::new(MediaOps::discover(
        config.media.ffmpeg.clone(),
        config.media.ffprobe.clone(),
        config.media.probe_timeout,
        config.media.thumbnail_timeout,
        config.media.transcode_timeout,
    ));
    let ffmpeg_available = media_ops.available();
    if !ffmpeg_available {
        tracing::warn!(
            "ffmpeg/ffprobe not available; scanning metadata and transcoding are disabled"
        );
        println!("  ffmpeg not found: metadata, thumbnails and transcoding are disabled.");
        println!(
            "  Install ffmpeg on PATH, place it in ffmpeg/bin next to the program, or set LOCALREEL_FFMPEG."
        );
        println!();
    }

    // Kernel workers are generic; the media crate registers the domain handlers.
    let mut registry = JobRegistry::new();
    // Remote media is read through the server's own range-capable stream
    // endpoint; bind address `0.0.0.0` is reached via loopback.
    let stream_host = match config.host.as_str() {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        host => host.to_string(),
    };
    localreel_media::MediaJobs::with_storage(
        db.clone(),
        media_ops.clone(),
        config.media.thumbnail_dir.clone(),
        config.media.transcode_dir.clone(),
        storage.clone(),
    )
    .with_stream_base(format!("http://{stream_host}:{}", config.port))
    .with_hls_dir(config.media.hls_dir.clone())
    .register(&mut registry);
    let registry = Arc::new(registry);
    let shutdown = Arc::new(AtomicBool::new(false));

    // One ffmpeg budget for the whole process. The workers draw from it, the API
    // reports it and the reaper cancels work nobody is watching.
    let transcodes = Arc::new(localreel_media::TranscodeGate::new(
        config.media.max_concurrent_transcodes,
        Duration::from_secs(config.media.idle_session_secs),
    ));
    info!(
        limit = transcodes.limit(),
        idle_session_secs = config.media.idle_session_secs,
        "transcode budget configured"
    );

    // Recover jobs left `running` by a previous process (crash, kill, reboot).
    // Workers only claim `queued` rows, so without a reclaimer such a row would
    // stay stuck until the lease expires and then forever.
    match jobs.reclaim_expired() {
        Ok(0) => {}
        Ok(count) => info!(count, "reclaimed jobs with expired leases on startup"),
        Err(error) => tracing::warn!("failed to reclaim jobs on startup: {error}"),
    }

    let mut workers = Vec::new();
    let transcode_capacity: Arc<dyn localreel_kernel::jobs::JobCapacity> = transcodes.clone();
    for index in 0..config.jobs.workers {
        workers.push(
            JobWorker::new(
                jobs.clone(),
                registry.clone(),
                format!("worker-{index}"),
                config.jobs.lease,
                config.jobs.poll_interval,
            )
            .with_capacity(transcode_capacity.clone())
            .spawn_with_shutdown(shutdown.clone()),
        );
    }

    // Reclaim playback sessions that stopped being used. The interval is short
    // so a cancelled encode frees the budget quickly, but every pass is a cheap
    // in-memory scan when nothing is idle.
    {
        let reaper_gate = transcodes.clone();
        let reaper_jobs = jobs.clone();
        let reaper_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                if reaper_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let report = reaper_gate.reap_idle(&reaper_jobs);
                if report.sessions > 0 {
                    info!(
                        sessions = report.sessions,
                        canceled = report.canceled,
                        "reclaimed idle playback sessions"
                    );
                }
            }
        });
    }

    // Keep reclaiming: a job whose worker died while its lease was still valid
    // is only reclaimable once the lease has expired.
    {
        let reclaim_jobs = jobs.clone();
        let reclaim_shutdown = shutdown.clone();
        let reclaim_interval = Duration::from_secs((config.jobs.lease.as_secs() / 2).max(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(reclaim_interval).await;
                if reclaim_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match reclaim_jobs.reclaim_expired() {
                    Ok(0) => {}
                    Ok(count) => tracing::warn!(count, "reclaimed jobs with expired leases"),
                    Err(error) => tracing::warn!("failed to reclaim expired jobs: {error}"),
                }
            }
        });
    }

    let cache_gc_interval = config.jobs.cache_gc_interval;
    if !cache_gc_interval.is_zero() {
        let payload = serde_json::json!({
            "max_bytes": config.media.cache_max_bytes,
            "max_age_secs": config.media.cache_max_age.as_secs(),
            "protect_recent_secs": 3600,
        })
        .to_string();
        if let Err(error) = jobs.enqueue("cache_gc", &payload, Some("cache_gc:periodic"), 1) {
            tracing::warn!("failed to enqueue initial cache_gc: {error}");
        }
        let gc_jobs = jobs.clone();
        let gc_payload = payload;
        let gc_shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(cache_gc_interval).await;
                if gc_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(error) =
                    gc_jobs.enqueue("cache_gc", &gc_payload, Some("cache_gc:periodic"), 1)
                {
                    tracing::warn!("failed to enqueue periodic cache_gc: {error}");
                }
            }
        });
        info!(
            interval_secs = cache_gc_interval.as_secs(),
            "periodic cache GC enabled"
        );
    }

    let open_ui = launch::should_open_ui(&config, no_open);

    // The tray is the desktop persona's way to stop the server without the
    // console. It is only installed when asked for, and the console is only
    // hidden once the icon really exists.
    let (quit_tx, mut quit_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let tray_handle: Option<tray::Tray> = if tray_icon && config.mode.is_desktop() {
        let tray_url = url.clone();
        tray::spawn(tray_url, move || {
            let _ = quit_tx.send(());
        })
    } else {
        None
    };
    let tray_active = tray_handle.is_some();
    if tray_active {
        let console = tray::hide_console();
        info!(
            "tray icon installed; console {}",
            match console {
                tray::ConsoleState::Absent => "absent",
                tray::ConsoleState::Hidden => "hidden",
                tray::ConsoleState::Kept => "kept",
            }
        );
        println!("  Tray:    right-click the notification-area icon to open or quit");
        if console == tray::ConsoleState::Kept {
            println!("  Stop:    close this window, or press Ctrl+C");
        }
    } else if tray_icon {
        println!("  Tray:    not available on this platform or in server mode");
    }

    let state = AppState::new(localreel_api::state::AppStateParts {
        config,
        db,
        events: events.clone(),
        jobs,
        media,
        accounts,
        transcodes,
        ffmpeg_available,
    });
    let app = localreel_api::router(state.clone());

    // Subscribe to every enabled local source before the first request can
    // change one. Unwatchable sources are logged and reported, never fatal.
    state.refresh_watchers();
    let watcher_status = state.watchers.status();
    if watcher_status.enabled {
        info!(
            active = watcher_status.active,
            skipped = watcher_status.skipped.len(),
            "watching local sources for changes"
        );
        if !watcher_status.skipped.is_empty() {
            warn!(
                skipped = ?watcher_status.skipped,
                "some sources cannot be watched and will only rescan on demand"
            );
        }
    } else {
        info!("filesystem watching is disabled (LOCALREEL_WATCH=0)");
    }

    info!("LocalReel listening on {url}");
    events.publish(Event::ServerStarted {
        version: localreel_kernel::VERSION.to_string(),
    });

    if open_ui {
        let url = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Err(error) = launch::open_browser(&url) {
                warn!("could not open a browser: {error}");
            }
        });
    }

    let shutdown_signal = shutdown.clone();
    let shutdown_notify = state.shutdown.clone();
    let tray_shutdown = async move {
        if tray_active {
            let _ = quit_rx.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    // Connect info carries the peer address to the login rate limiter.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = wait_for_ctrl_c() => {}
            _ = tray_shutdown => {}
            _ = shutdown_notify.notified() => {
                info!("shutdown requested from the admin page");
            }
        }
        shutdown_signal.store(true, Ordering::SeqCst);
    })
    .await?;

    if let Some(mut tray) = tray_handle {
        tray.close();
    }

    // Give workers a short window to finish/release the current job.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    for worker in workers {
        let _ = tokio::time::timeout_at(deadline, worker).await;
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
