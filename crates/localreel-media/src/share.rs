//! Share links: one unguessable token that grants read-only access to one item.
//!
//! A share link is the only thing in the product an unauthenticated request can
//! name, so the rules are deliberately narrow: one token, one media item, an
//! optional expiry, a revocation flag and a download switch. Nothing about the
//! rest of the library is reachable through it, and the token is random rather
//! than derived from the media id.

use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::service::MediaService;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub token: String,
    pub media_id: i64,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
    pub allow_download: bool,
}

impl Share {
    /// Whether this link may be used right now.
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|at| at > now)
    }
}

/// A fresh token: 32 hex characters from a v4 UUID, so 122 random bits.
///
/// The media id never appears in the token, and guessing one is not something
/// rate limiting has to compensate for.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn row_to_share(row: &Row<'_>) -> rusqlite::Result<Share> {
    Ok(Share {
        token: row.get(0)?,
        media_id: row.get(1)?,
        created_at: row.get(2)?,
        expires_at: row.get(3)?,
        revoked_at: row.get(4)?,
        allow_download: row.get::<_, i64>(5)? != 0,
    })
}

const COLUMNS: &str = "token, media_id, created_at, expires_at, revoked_at, allow_download";

impl MediaService {
    /// Create a link for one item. `ttl_secs` of `None` means it does not expire
    /// until it is revoked.
    pub fn create_share(
        &self,
        media_id: i64,
        created_by: Option<i64>,
        ttl_secs: Option<i64>,
        allow_download: bool,
    ) -> Result<Share> {
        let now = now_secs();
        let expires_at = match ttl_secs {
            Some(seconds) if seconds > 0 => Some(now + seconds),
            Some(_) => {
                return Err(KernelError::InvalidInput(
                    "the expiry must be positive".into(),
                ))
            }
            None => None,
        };
        let exists = self.database().with_read(|connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM media WHERE id = ?1",
                [media_id],
                |row| row.get::<_, i64>(0),
            )? > 0)
        })?;
        if !exists {
            return Err(KernelError::InvalidInput(format!(
                "media {media_id} does not exist"
            )));
        }

        let token = new_token();
        self.database().with_write(|connection| {
            connection.execute(
                "INSERT INTO media_share (media_id, token, created_by, created_at, expires_at, allow_download)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![media_id, token, created_by, now, expires_at, allow_download as i64],
            )?;
            Ok(())
        })?;

        Ok(Share {
            token,
            media_id,
            created_at: now,
            expires_at,
            revoked_at: None,
            allow_download,
        })
    }

    /// Every link ever created for an item, newest first, so the owner can see
    /// what exists and revoke the ones they forgot about.
    pub fn list_shares(&self, media_id: i64) -> Result<Vec<Share>> {
        self.database().with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM media_share WHERE media_id = ?1 ORDER BY id DESC"
            ))?;
            let shares = statement
                .query_map([media_id], row_to_share)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(shares)
        })
    }

    /// The raw row, active or not; the caller decides whether that matters.
    pub fn share_by_token(&self, token: &str) -> Result<Option<Share>> {
        self.database().with_read(|connection| {
            Ok(connection
                .query_row(
                    &format!("SELECT {COLUMNS} FROM media_share WHERE token = ?1"),
                    [token],
                    row_to_share,
                )
                .optional()?)
        })
    }

    /// Revoke a link. Returns `false` when there was nothing to revoke, so the
    /// API can answer 404 instead of pretending it worked.
    pub fn revoke_share(&self, token: &str) -> Result<bool> {
        let now = now_secs();
        self.database().with_write(|connection| {
            let changed = connection.execute(
                "UPDATE media_share SET revoked_at = ?2 WHERE token = ?1 AND revoked_at IS NULL",
                params![token, now],
            )?;
            Ok(changed > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::db::Database;
    use localreel_kernel::migrations;

    fn setup() -> (tempfile::TempDir, MediaService, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("share.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        migrations::apply(&db, "media", crate::migrations::list()).unwrap();
        let media_id = db
            .with_write(|connection| {
                connection.execute(
                    "INSERT INTO sources (uri, created_at, updated_at) VALUES ('file:///tmp', 0, 0)",
                    [],
                )?;
                let source_id = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO media (uid, source_id, uri, canonical_path, filename, created_at, updated_at)
                     VALUES ('test-uid', ?1, 'file:///tmp/a.mp4', '/tmp/a.mp4', 'a.mp4', 0, 0)",
                    [source_id],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        (dir, MediaService::new(db), media_id)
    }

    #[test]
    fn creates_and_resolves_a_token() {
        let (_dir, service, media_id) = setup();
        let share = service
            .create_share(media_id, Some(1), None, false)
            .unwrap();
        assert_eq!(share.token.len(), 32);
        assert!(share.is_active(now_secs()));
        let found = service.share_by_token(&share.token).unwrap().unwrap();
        assert_eq!(found, share);
        assert_eq!(service.list_shares(media_id).unwrap().len(), 1);
    }

    #[test]
    fn tokens_do_not_repeat() {
        let (_dir, service, media_id) = setup();
        let first = service.create_share(media_id, None, None, false).unwrap();
        let second = service.create_share(media_id, None, None, false).unwrap();
        assert_ne!(first.token, second.token);
        assert_eq!(service.list_shares(media_id).unwrap().len(), 2);
    }

    #[test]
    fn revoked_and_expired_links_are_inactive() {
        let (_dir, service, media_id) = setup();
        let share = service
            .create_share(media_id, None, Some(1), false)
            .unwrap();
        assert!(share.is_active(now_secs()));
        assert!(!share.is_active(now_secs() + 5));

        assert!(service.revoke_share(&share.token).unwrap());
        let revoked = service.share_by_token(&share.token).unwrap().unwrap();
        assert!(!revoked.is_active(now_secs()));
        // Revoking twice reports that nothing changed.
        assert!(!service.revoke_share(&share.token).unwrap());
    }

    #[test]
    fn refuses_a_media_item_that_does_not_exist() {
        let (_dir, service, _media_id) = setup();
        let error = service.create_share(9999, None, None, false).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn refuses_a_non_positive_expiry() {
        let (_dir, service, media_id) = setup();
        assert!(service
            .create_share(media_id, None, Some(0), false)
            .is_err());
        assert!(service
            .create_share(media_id, None, Some(-5), false)
            .is_err());
    }
}
