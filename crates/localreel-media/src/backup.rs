use std::collections::HashMap;
use std::path::Path;

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::account::{AccountService, Role, OWNER_ID};
use crate::service::MediaService;

pub use localreel_format::{
    artifact_dir_name, artifact_file_name, ArtifactKind, Backup, BackupState, BackupUser,
    FormatError, BACKUP_FORMAT_VERSION, MAX_BACKUP_SOURCES, MAX_BACKUP_STATES, MAX_BACKUP_USERS,
    MIN_BACKUP_FORMAT_VERSION,
};

/// Backup format version (shared with the format crate).
pub const BACKUP_VERSION: u32 = BACKUP_FORMAT_VERSION;
pub const MAX_BACKUP_BYTES: u64 = 32 * 1024 * 1024;
/// Uploaded restores use a separate, larger limit; the importer keeps 32 MiB.
pub const MAX_BACKUP_RESTORE_BYTES: u64 = 512 * 1024 * 1024;

/// Validate a backup with the shared format rules.
pub fn validate_backup(backup: &Backup) -> Result<()> {
    backup
        .validate()
        .map_err(|error| KernelError::InvalidInput(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupImportReport {
    pub dry_run: bool,
    pub sources: usize,
    pub sources_added: usize,
    pub users: usize,
    pub users_created: usize,
    pub states: usize,
    pub imported: usize,
    pub unchanged: usize,
    pub missing: usize,
    pub warnings: Vec<String>,
}

pub fn read_backup(path: &Path) -> Result<Backup> {
    read_backup_with_limit(path, MAX_BACKUP_BYTES)
}

pub fn read_backup_with_limit(path: &Path, max_bytes: u64) -> Result<Backup> {
    let file = std::fs::File::open(path).map_err(|error| {
        KernelError::InvalidInput(format!("cannot read backup {}: {error}", path.display()))
    })?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes {
        return Err(KernelError::InvalidInput(format!(
            "backup is too large: {} bytes (max {max_bytes})",
            metadata.len()
        )));
    }
    // Deserialize directly from the file: the raw JSON is never buffered as a
    // second full copy. The typed `Backup` (states) still lives in memory.
    let reader = std::io::BufReader::new(file);
    let backup: Backup = serde_json::from_reader(reader)
        .map_err(|error| KernelError::InvalidInput(format!("invalid backup JSON: {error}")))?;
    validate_backup(&backup)?;
    Ok(backup)
}

pub fn parse_backup(bytes: &[u8]) -> Result<Backup> {
    parse_backup_with_limit(bytes, MAX_BACKUP_BYTES)
}

pub fn parse_backup_with_limit(bytes: &[u8], max_bytes: u64) -> Result<Backup> {
    if bytes.len() as u64 > max_bytes {
        return Err(KernelError::InvalidInput(format!(
            "backup is too large: {} bytes (max {max_bytes})",
            bytes.len()
        )));
    }
    let backup: Backup = serde_json::from_slice(bytes)
        .map_err(|error| KernelError::InvalidInput(format!("invalid backup JSON: {error}")))?;
    validate_backup(&backup)?;
    Ok(backup)
}

/// Export the whole household: sources, accounts (without password hashes) and
/// every account's playback state.
pub fn export_backup(db: &Database) -> Result<Backup> {
    let service = MediaService::new(db.clone());
    let sources = service
        .list_sources()?
        .into_iter()
        .map(|source| source.uri)
        .collect::<Vec<_>>();

    let users = AccountService::new(db.clone())
        .list_users()?
        .into_iter()
        .map(|user| BackupUser {
            username: user.username,
            role: user.role.as_str().to_string(),
        })
        .collect::<Vec<_>>();

    let states = db.with_read(|connection| {
        let mut statement = connection.prepare(
            "SELECT m.uid, m.uri, u.username, s.liked, s.favorite, s.position, s.duration
             FROM media_state s
             JOIN media m ON m.id = s.media_id
             JOIN users u ON u.id = s.user_id
             ORDER BY m.id, u.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(BackupState {
                uid: Some(row.get(0)?),
                path: row.get(1)?,
                user: Some(row.get(2)?),
                liked: row.get::<_, i64>(3)? != 0,
                favorite: row.get::<_, i64>(4)? != 0,
                position: row.get(5)?,
                duration: row.get(6)?,
            })
        })?;
        let mut states = Vec::new();
        for row in rows {
            states.push(row?);
        }
        Ok(states)
    })?;

    let backup = Backup {
        version: BACKUP_VERSION,
        created_at: Some(localreel_kernel::time::now_secs()),
        sources,
        users,
        states,
    };
    validate_backup(&backup)?;
    Ok(backup)
}

pub fn import_backup(db: &Database, backup: &Backup, dry_run: bool) -> Result<BackupImportReport> {
    validate_backup(backup)?;
    let service = MediaService::new(db.clone());
    let accounts = AccountService::new(db.clone());
    let mut report = BackupImportReport {
        dry_run,
        sources: backup.sources.len(),
        sources_added: 0,
        users: backup.users.len(),
        users_created: 0,
        states: backup.states.len(),
        imported: 0,
        unchanged: 0,
        missing: 0,
        warnings: Vec::new(),
    };

    for source in &backup.sources {
        if dry_run {
            if let Err(error) = localreel_kernel::storage::MediaUri::parse(source) {
                report
                    .warnings
                    .push(format!("source '{source}' is invalid: {error}"));
            } else {
                report.sources_added += 1;
            }
            continue;
        }
        match service.add_source(source) {
            Ok(_) => report.sources_added += 1,
            Err(error) => report
                .warnings
                .push(format!("source '{source}' was not imported: {error}")),
        }
    }

    // Accounts first: every state names its owner. A restored account starts
    // without a password, so nobody can sign in until an administrator sets one.
    let mut user_ids: HashMap<String, i64> = accounts
        .list_users()?
        .into_iter()
        .map(|user| (user.username.to_ascii_lowercase(), user.id))
        .collect();

    for restored in &backup.users {
        let key = restored.username.to_ascii_lowercase();
        if user_ids.contains_key(&key) {
            continue;
        }
        if dry_run {
            report.users_created += 1;
            continue;
        }
        let role = if restored.role.eq_ignore_ascii_case("admin") {
            Role::Admin
        } else {
            Role::User
        };
        match accounts.create_user_without_password(&restored.username, role) {
            Ok(created) => {
                user_ids.insert(key, created.id);
                report.users_created += 1;
            }
            Err(error) => report.warnings.push(format!(
                "account '{}' was not created: {error}",
                restored.username
            )),
        }
    }

    for state in &backup.states {
        let target_id = find_target(db, state)?;
        let Some(media_id) = target_id else {
            report.missing += 1;
            continue;
        };
        let user_id = match state.user.as_deref() {
            Some(username) => match user_ids.get(&username.to_ascii_lowercase()) {
                Some(id) => *id,
                None => {
                    report.warnings.push(format!(
                        "state for '{username}' replayed into the owner account: that account is not in this backup"
                    ));
                    OWNER_ID
                }
            },
            // Version 2 backups had one implicit user.
            None => OWNER_ID,
        };
        let mut current = service.get_state(user_id, media_id)?;
        let before = current.clone();
        current.media_id = media_id;
        current.liked |= state.liked;
        current.favorite |= state.favorite;
        current.duration = if current.duration > 0.0 {
            current.duration
        } else {
            state.duration
        };
        let mut position = current.position.max(state.position);
        if current.duration > 0.0 {
            position = position.min((current.duration - 1.0).max(0.0));
        }
        current.position = position;
        let changed = before.liked != current.liked
            || before.favorite != current.favorite
            || (before.position - current.position).abs() > f64::EPSILON
            || (before.duration - current.duration).abs() > f64::EPSILON;
        if changed {
            report.imported += 1;
            if !dry_run {
                service.save_state(&current)?;
            }
        } else {
            report.unchanged += 1;
        }
    }

    Ok(report)
}

fn find_target(db: &Database, state: &BackupState) -> Result<Option<i64>> {
    db.with_read(|connection| {
        let by_uid: Option<i64> = match &state.uid {
            Some(uid) => connection
                .query_row("SELECT id FROM media WHERE uid = ?1", [uid], |row| {
                    row.get(0)
                })
                .optional()?,
            None => None,
        };
        if by_uid.is_some() {
            return Ok(by_uid);
        }
        Ok(connection
            .query_row(
                "SELECT id FROM media WHERE uri = ?1",
                [&state.path],
                |row| row.get(0),
            )
            .optional()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations as kernel_migrations;

    use crate::migrations;

    fn setup() -> (tempfile::TempDir, Database, MediaService) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("backup.db").to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        kernel_migrations::apply(&db, "media", migrations::list()).unwrap();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let service = MediaService::new(db.clone());
        (dir, db, service)
    }

    fn insert_media(db: &Database, source_id: i64, uid: &str, uri: &str) -> i64 {
        let now = localreel_kernel::time::now_secs();
        db.with_write(|connection| {
            connection.execute(
                "INSERT INTO media (uid, source_id, uri, canonical_path, filename,
                                        duration, file_size, modified_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?3, ?4, 100.0, 0, 0, ?5, ?5)",
                rusqlite::params![uid, source_id, uri, format!("{uid}.mp4"), now],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap()
    }

    #[test]
    fn reads_backup_from_file_and_enforces_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.json");
        let mut bytes = serde_json::to_vec(&Backup::new()).unwrap();
        bytes.extend(std::iter::repeat_n(b' ', 4096));
        std::fs::write(&path, &bytes).unwrap();

        let parsed = read_backup_with_limit(&path, 64 * 1024).unwrap();
        assert_eq!(parsed.version, BACKUP_VERSION);

        let error = read_backup_with_limit(&path, 10).unwrap_err();
        assert!(matches!(error, KernelError::InvalidInput(_)));
    }

    #[test]
    fn export_and_import_roundtrip() {
        let (dir, db, service) = setup();
        let source = service
            .add_source(dir.path().join("videos").to_str().unwrap())
            .unwrap();
        let media_id = insert_media(&db, source.id, "uid-1", "file:///videos/uid-1.mp4");
        let state = crate::model::MediaState {
            user_id: crate::account::OWNER_ID,
            media_id,
            liked: true,
            favorite: true,
            position: 42.0,
            duration: 100.0,
            updated_at: 0,
        };
        service.save_state(&state).unwrap();

        let backup = export_backup(&db).unwrap();
        assert_eq!(backup.version, BACKUP_VERSION);
        assert_eq!(backup.sources.len(), 1);
        assert_eq!(backup.states.len(), 1);

        let (dir2, db2, service2) = setup();
        let source2 = service2
            .add_source(dir2.path().join("videos").to_str().unwrap())
            .unwrap();
        insert_media(&db2, source2.id, "uid-1", "file:///videos/uid-1.mp4");
        let report = import_backup(&db2, &backup, false).unwrap();
        assert_eq!(report.imported, 1);
        let state = service2.get_state(crate::account::OWNER_ID, 1).unwrap();
        assert!(state.liked);
        assert!(state.favorite);
        assert_eq!(state.position, 42.0);
    }

    #[test]
    fn import_does_not_lower_existing_progress_and_reports_missing() {
        let (dir, db, service) = setup();
        let source = service
            .add_source(dir.path().join("videos").to_str().unwrap())
            .unwrap();
        let media_id = insert_media(&db, source.id, "uid-2", "file:///videos/uid-2.mp4");
        service
            .save_state(&crate::model::MediaState {
                user_id: crate::account::OWNER_ID,
                media_id,
                liked: false,
                favorite: false,
                position: 80.0,
                duration: 100.0,
                updated_at: 0,
            })
            .unwrap();

        let backup = Backup {
            version: BACKUP_VERSION,
            created_at: None,
            sources: Vec::new(),
            users: Vec::new(),
            states: vec![
                BackupState {
                    uid: Some("uid-2".into()),
                    user: None,
                    path: "file:///videos/uid-2.mp4".into(),
                    liked: true,
                    favorite: false,
                    position: 20.0,
                    duration: 100.0,
                },
                BackupState {
                    uid: Some("missing".into()),
                    user: None,
                    path: "file:///missing.mp4".into(),
                    liked: false,
                    favorite: false,
                    position: 1.0,
                    duration: 1.0,
                },
            ],
        };
        let report = import_backup(&db, &backup, false).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(report.missing, 1);
        let state = service
            .get_state(crate::account::OWNER_ID, media_id)
            .unwrap();
        assert!(state.liked);
        assert_eq!(state.position, 80.0);
    }

    #[test]
    fn household_backup_keeps_each_accounts_state() {
        let (dir, db, service) = setup();
        let accounts = crate::account::AccountService::new(db.clone());
        let guest = accounts
            .create_user("guest", "guest password", crate::account::Role::User)
            .unwrap();
        let source = service
            .add_source(dir.path().join("videos").to_str().unwrap())
            .unwrap();
        let media_id = insert_media(&db, source.id, "uid-m", "file:///videos/uid-m.mp4");
        for (user_id, position, favorite) in [
            (crate::account::OWNER_ID, 10.0, false),
            (guest.id, 50.0, true),
        ] {
            service
                .save_state(&crate::model::MediaState {
                    user_id,
                    media_id,
                    liked: false,
                    favorite,
                    position,
                    duration: 100.0,
                    updated_at: 0,
                })
                .unwrap();
        }

        let backup = export_backup(&db).unwrap();
        assert_eq!(backup.users.len(), 2, "owner + guest");
        assert_eq!(backup.states.len(), 2, "one state per account");
        assert!(backup.users.iter().all(|user| !user.username.is_empty()));
        assert_eq!(backup.version, BACKUP_VERSION);

        let (dir2, db2, service2) = setup();
        let source2 = service2
            .add_source(dir2.path().join("videos").to_str().unwrap())
            .unwrap();
        insert_media(&db2, source2.id, "uid-m", "file:///videos/uid-m.mp4");
        let report = import_backup(&db2, &backup, false).unwrap();
        assert_eq!(report.users_created, 1, "only the guest is new");
        assert_eq!(report.imported, 2);

        let restored = crate::account::AccountService::new(db2.clone())
            .list_users()
            .unwrap();
        let restored_guest = restored
            .iter()
            .find(|user| user.username == "guest")
            .expect("guest restored");
        // The restored account cannot sign in until an administrator sets a password.
        assert!(crate::account::AccountService::new(db2.clone())
            .authenticate("guest", "guest password")
            .unwrap()
            .is_none());
        let owner_state = service2.get_state(crate::account::OWNER_ID, 1).unwrap();
        let guest_state = service2.get_state(restored_guest.id, 1).unwrap();
        assert_eq!(owner_state.position, 10.0);
        assert!(!owner_state.favorite);
        assert_eq!(guest_state.position, 50.0);
        assert!(guest_state.favorite);
    }

    #[test]
    fn version_2_backups_replay_into_the_owner() {
        let (dir, db, service) = setup();
        let source = service
            .add_source(dir.path().join("videos").to_str().unwrap())
            .unwrap();
        insert_media(&db, source.id, "uid-v2", "file:///videos/uid-v2.mp4");
        let backup: Backup = serde_json::from_str(
            r#"{
                "version": 2,
                "sources": [],
                "states": [
                    {"uid": "uid-v2", "path": "file:///videos/uid-v2.mp4", "position": 30.0, "duration": 60.0}
                ]
            }"#,
        )
        .unwrap();
        let report = import_backup(&db, &backup, false).unwrap();
        assert_eq!(report.imported, 1);
        let state = service.get_state(crate::account::OWNER_ID, 1).unwrap();
        assert_eq!(state.position, 30.0);
    }

    #[test]
    fn rejects_unsupported_versions() {
        let mut backup = Backup::new();
        backup.version = 99;
        assert!(validate_backup(&backup).is_err());
    }
}
