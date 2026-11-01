use std::sync::Arc;

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
    let config = Config::load()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    Ok(())
}

fn doctor() -> anyhow::Result<()> {
    let config = Config::load()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;
    let media_ops = MediaOps::new(
        config.media.ffmpeg.clone(),
        config.media.ffprobe.clone(),
        config.media.probe_timeout,
        config.media.thumbnail_timeout,
        config.media.transcode_timeout,
    );
    println!("LocalReel doctor");
    println!("  host:      {}:{}", config.host, config.port);
    println!("  data dir:  {}", config.data_dir.display());
    println!("  database:  {} (ok)", config.db_path.display());
    println!("  ffmpeg:    {}", yes_no(media_ops.available()));
    println!("  ffmpeg:    {}", config.media.ffmpeg.display());
    println!("  ffprobe:   {}", config.media.ffprobe.display());
    println!("  workers:   {}", config.jobs.workers);
    Ok(())
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::load()?;
    config.ensure_dirs()?;
    let db = open_database(&config)?;
    run_migrations(&db)?;

    let events = EventBus::new(256);
    let jobs = JobQueue::new(db.clone(), events.clone());
    let media = MediaService::new(db.clone());
    let media_ops = MediaOps::new(
        config.media.ffmpeg.clone(),
        config.media.ffprobe.clone(),
        config.media.probe_timeout,
        config.media.thumbnail_timeout,
        config.media.transcode_timeout,
    );
    let ffmpeg_available = media_ops.available();
    if !ffmpeg_available {
        tracing::warn!(
            "ffmpeg/ffprobe not available; scanning metadata and transcoding are disabled"
        );
    }

    // Kernel workers are generic. The media crate registers handlers in Phase 1/2.
    let registry = Arc::new(JobRegistry::new());
    for index in 0..config.jobs.workers {
        JobWorker::new(
            jobs.clone(),
            registry.clone(),
            format!("worker-{index}"),
            config.jobs.lease,
            config.jobs.poll_interval,
        )
        .spawn();
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

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
