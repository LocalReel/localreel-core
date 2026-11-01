use std::path::Path;
use std::sync::atomic::AtomicBool;

use localreel_kernel::db::Database;
use localreel_kernel::error::Result;
use localreel_kernel::media_ops::MediaOps;
use localreel_kernel::storage::MediaUri;
use serde::{Deserialize, Serialize};

use crate::backup::{import_backup, Backup};
use crate::scan::discover_source;
use crate::service::MediaService;

/// One-command restore: import sources, scan/probe/thumbnail, then replay
/// states. `dry_run` reports the backup contents without writing anything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreReport {
    pub dry_run: bool,
    pub sources: usize,
    pub sources_added: usize,
    pub states: usize,
    pub media_discovered: usize,
    pub probed: usize,
    pub thumbnails: usize,
    pub states_imported: usize,
    pub missing: usize,
    pub warnings: Vec<String>,
}

pub fn restore_backup(
    db: &Database,
    backup: &Backup,
    media_ops: &MediaOps,
    thumbnail_dir: &Path,
    dry_run: bool,
) -> Result<RestoreReport> {
    backup.validate()?;
    let mut report = RestoreReport {
        dry_run,
        sources: backup.sources.len(),
        states: backup.states.len(),
        ..RestoreReport::default()
    };

    let first = import_backup(db, backup, dry_run)?;
    report.sources_added = first.sources_added;
    report.warnings.extend(first.warnings);
    if dry_run {
        report.missing = first.missing;
        return Ok(report);
    }

    let service = MediaService::new(db.clone());
    for uri in &backup.sources {
        let Some(source) = service.list_sources()?.into_iter().find(|s| &s.uri == uri) else {
            report
                .warnings
                .push(format!("source '{uri}' was not found after import"));
            continue;
        };
        if !matches!(MediaUri::parse(&source.uri), Ok(MediaUri::Local(_))) {
            report.warnings.push(format!(
                "source '{}' is not local; skipped scan",
                source.uri
            ));
            continue;
        }

        let discovery = match discover_source(db, &source) {
            Ok(discovery) => discovery,
            Err(error) => {
                report
                    .warnings
                    .push(format!("scan '{}' failed: {error}", source.uri));
                continue;
            }
        };
        report.media_discovered += discovery.discovered;

        let candidate_ids = match service.probe_candidate_ids(source.id) {
            Ok(ids) => ids,
            Err(error) => {
                report
                    .warnings
                    .push(format!("probe candidates '{}' failed: {error}", source.uri));
                continue;
            }
        };
        for media_id in candidate_ids {
            let Some(media) = service.get_media(media_id)? else {
                continue;
            };
            let Some(path) = MediaUri::parse(&media.uri)?
                .local_path()
                .map(Path::to_path_buf)
            else {
                continue;
            };
            if !path.is_file() {
                report
                    .warnings
                    .push(format!("media {} is missing: {}", media.id, path.display()));
                continue;
            }

            if media.duration.is_none() || media.video_codec.is_none() {
                match media_ops.probe(&path, &AtomicBool::new(false)) {
                    Ok(probe) => {
                        if let Err(error) = service.update_probe(media.id, &probe) {
                            report
                                .warnings
                                .push(format!("media {} probe update failed: {error}", media.id));
                        } else {
                            report.probed += 1;
                        }
                    }
                    Err(error) => report
                        .warnings
                        .push(format!("media {} probe failed: {error}", media.id)),
                }
            }

            if media.thumbnail_path.is_none() {
                let filename = format!("thumb_{}.jpg", media.uid);
                let output = thumbnail_dir.join(&filename);
                let seek = media
                    .duration
                    .map(|duration| (duration * 0.1).clamp(1.0, 10.0))
                    .unwrap_or(3.0);
                match media_ops.thumbnail(&path, &output, seek, 480, &AtomicBool::new(false)) {
                    Ok(()) => {
                        let size = std::fs::metadata(&output)
                            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
                            .unwrap_or(0);
                        if let Err(error) = service.set_thumbnail(media.id, &filename) {
                            report.warnings.push(format!(
                                "media {} thumbnail path update failed: {error}",
                                media.id
                            ));
                        } else if let Err(error) = service.record_variant(
                            media.id,
                            "thumbnail",
                            "default",
                            &filename,
                            size,
                        ) {
                            report.warnings.push(format!(
                                "media {} thumbnail record failed: {error}",
                                media.id
                            ));
                        } else {
                            report.thumbnails += 1;
                        }
                    }
                    Err(error) => report
                        .warnings
                        .push(format!("media {} thumbnail failed: {error}", media.id)),
                }
            }
        }
    }

    let second = import_backup(db, backup, false)?;
    report.states_imported = second.imported;
    report.missing = second.missing;
    report.warnings.extend(second.warnings);
    Ok(report)
}
