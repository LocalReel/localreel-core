use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing::info;

use localreel_api::AppState;
use localreel_kernel::config::Config;
use localreel_kernel::db::Database;
use localreel_kernel::events::{Event, EventBus};
use localreel_kernel::jobs::{JobQueue, JobRegistry, JobWorker};
use localreel_kernel::media_ops::MediaOps;
use localreel_kernel::migrations;
use localreel_media::MediaService;

#[derive(Debug, Parser)]
#[command(name = "localreel", version, about = "Local-first media server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP server (default).
    Serve,
    /// Apply database migrations and exit.
    Migrate,
    /// Print configuration and environment checks.
    Doctor,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Migrate => migrate(),
        Command::Doctor => doctor(),
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
    println!(
        "  config:    {}",
        source
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "defaults + environment".to_string())
    );
    println!("  host:      {}:{}", config.host, config.port);
    println!("  data dir:  {}", config.data_dir.display());
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

async fn serve() -> anyhow::Result<()> {
    let (config, source) = Config::load_with_source()?;
    if let Some(path) = &source {
        info!("loaded configuration from {}", path.display());
    }
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;

    let events = EventBus::new(256);
    let jobs = JobQueue::new(db.clone(), events.clone());
    let media = MediaService::new(db.clone());
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
    }

    // Kernel workers are generic; the media crate registers the domain handlers.
    let mut registry = JobRegistry::new();
    localreel_media::MediaJobs::new(
        db.clone(),
        media_ops.clone(),
        config.media.thumbnail_dir.clone(),
        config.media.transcode_dir.clone(),
    )
    .register(&mut registry);
    let registry = Arc::new(registry);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for index in 0..config.jobs.workers {
        workers.push(
            JobWorker::new(
                jobs.clone(),
                registry.clone(),
                format!("worker-{index}"),
                config.jobs.lease,
                config.jobs.poll_interval,
            )
            .spawn_with_shutdown(shutdown.clone()),
        );
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

    let listen_host = config.host.clone();
    let listen_port = config.port;
    let state = AppState::new(config, db, events.clone(), jobs, media, ffmpeg_available);
    let app = localreel_api::router(state);

    let listener = tokio::net::TcpListener::bind((listen_host.as_str(), listen_port)).await?;
    info!("LocalReel listening on http://{}", listener.local_addr()?);
    events.publish(Event::ServerStarted {
        version: localreel_kernel::VERSION.to_string(),
    });

    let shutdown_signal = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_signal.store(true, Ordering::SeqCst);
        })
        .await?;

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
