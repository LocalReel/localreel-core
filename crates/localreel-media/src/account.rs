//! Accounts, password hashing and login sessions.
//!
//! LocalReel is a household product: one library, one owner account that can
//! manage everything, and additional accounts with their own watch progress.
//!
//! Passwords are stored as Argon2id hashes. Sessions are opaque random tokens
//! kept in the database (so a restart does not log everybody out), delivered as
//! an `HttpOnly` cookie, and paired with a separate CSRF token the client must
//! echo on state-changing requests.
//!
//! The owner account (id [`OWNER_ID`]) is created by the accounts migration with
//! an empty password: that means "not configured yet", and the UI asks for a
//! password before anything else. An empty hash never authenticates.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The owner account created by the accounts migration.
pub const OWNER_ID: i64 = 1;

pub const MIN_USERNAME_LEN: usize = 3;
pub const MAX_USERNAME_LEN: usize = 32;
pub const MIN_PASSWORD_LEN: usize = 8;
pub const MAX_PASSWORD_LEN: usize = 256;

/// How long a login stays valid without activity.
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;
/// Do not write `last_seen_at` more often than this.
pub const SESSION_TOUCH_SECS: i64 = 300;

/// What an account may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "admin" => Ok(Role::Admin),
            "user" => Ok(Role::User),
            other => Err(KernelError::InvalidInput(format!(
                "unknown role '{other}': expected 'admin' or 'user'"
            ))),
        }
    }

    pub const fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// A user as the API exposes it. Never carries the password hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_login_at: Option<i64>,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role.is_admin()
    }

    pub fn is_owner(&self) -> bool {
        self.id == OWNER_ID
    }
}

/// Who a request is authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    pub user: User,
    pub csrf: String,
}

/// A freshly created session. The token is only ever shown once, to the client
/// that just logged in.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub csrf: String,
    pub expires_at: i64,
}

/// Changes an administrator may apply to an account.
#[derive(Debug, Clone, Default)]
pub struct UserPatch {
    pub role: Option<Role>,
    pub enabled: Option<bool>,
    pub password: Option<String>,
}

/// Account storage and authentication.
#[derive(Clone)]
pub struct AccountService {
    db: Database,
}

impl AccountService {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// True until the owner account has a password.
    pub fn setup_required(&self) -> Result<bool> {
        Ok(self.owner_password_hash()?.is_empty())
    }

    /// Finish the very first run: give the owner account a name and password.
    pub fn setup_owner(&self, username: &str, password: &str) -> Result<User> {
        let username = validate_username(username)?;
        validate_password(password)?;
        let hash = hash_password(password)?;
        let now = now_secs();
        self.db.with_write(|connection| {
            let current: String = connection.query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                [OWNER_ID],
                |row| row.get(0),
            )?;
            if !current.is_empty() {
                return Err(KernelError::Conflict(
                    "the owner account is already configured".into(),
                ));
            }
            connection.execute(
                "UPDATE users SET username = ?1, password_hash = ?2, updated_at = ?3 WHERE id = ?4",
                params![username, hash, now, OWNER_ID],
            )?;
            Ok(())
        })?;
        self.get_user(OWNER_ID)?
            .ok_or_else(|| KernelError::NotFound("owner account".into()))
    }

    /// Check a username/password pair. Returns `None` for every failure so the
    /// API cannot be used to enumerate accounts.
    pub fn authenticate(&self, username: &str, password: &str) -> Result<Option<User>> {
        let record = self.find_credentials(username)?;
        let Some((user, hash)) = record else {
            // Spend comparable time so a missing account is not obviously faster.
            let _ = hash_password(password);
            return Ok(None);
        };
        if hash.is_empty() || !user.enabled {
            return Ok(None);
        }
        if !verify_password(password, &hash) {
            return Ok(None);
        }
        let now = now_secs();
        self.db.with_write(|connection| {
            connection.execute(
                "UPDATE users SET last_login_at = ?1 WHERE id = ?2",
                params![now, user.id],
            )?;
            Ok(())
        })?;
        Ok(Some(user))
    }

    /// Start a session for a user.
    pub fn create_session(&self, user_id: i64) -> Result<Session> {
        let now = now_secs();
        let session = Session {
            id: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            csrf: Uuid::new_v4().simple().to_string(),
            expires_at: now + SESSION_TTL_SECS,
        };
        let id = session.id.clone();
        let csrf = session.csrf.clone();
        let expires_at = session.expires_at;
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO sessions (id, csrf, user_id, created_at, expires_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?4)",
                params![id, csrf, user_id, now, expires_at],
            )?;
            Ok(())
        })?;
        Ok(session)
    }

    /// Resolve a session token to its user, refreshing the activity timestamp.
    pub fn session(&self, token: &str) -> Result<Option<CurrentUser>> {
        if token.is_empty() {
            return Ok(None);
        }
        let now = now_secs();
        let found = self.db.with_read(|connection| {
            let row = connection
                .query_row(
                    "SELECT s.csrf, s.expires_at, s.last_seen_at,
                            u.id, u.username, u.role, u.enabled,
                            u.created_at, u.updated_at, u.last_login_at
                     FROM sessions s JOIN users u ON u.id = s.user_id
                     WHERE s.id = ?1",
                    [token],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            user_from_row(row, 3)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })?;

        let Some((csrf, expires_at, last_seen_at, user)) = found else {
            return Ok(None);
        };
        if expires_at <= now || !user.enabled {
            self.end_session(token)?;
            return Ok(None);
        }
        if now - last_seen_at >= SESSION_TOUCH_SECS {
            let token = token.to_string();
            self.db.with_write(|connection| {
                connection.execute(
                    "UPDATE sessions SET last_seen_at = ?1, expires_at = ?2 WHERE id = ?3",
                    params![now, now + SESSION_TTL_SECS, token],
                )?;
                Ok(())
            })?;
        }
        Ok(Some(CurrentUser { user, csrf }))
    }

    pub fn end_session(&self, token: &str) -> Result<()> {
        self.db.with_write(|connection| {
            connection.execute("DELETE FROM sessions WHERE id = ?1", [token])?;
            Ok(())
        })
    }

    /// Drop expired sessions; called on startup and periodically.
    pub fn purge_expired_sessions(&self) -> Result<usize> {
        let now = now_secs();
        self.db.with_write(|connection| {
            let removed =
                connection.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
            Ok(removed)
        })
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, username, role, enabled, created_at, updated_at, last_login_at
                 FROM users ORDER BY id",
            )?;
            let users = statement
                .query_map([], |row| user_from_row(row, 0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(users)
        })
    }

    pub fn user_count(&self) -> Result<i64> {
        self.db.with_read(|connection| {
            Ok(connection.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?)
        })
    }

    pub fn get_user(&self, id: i64) -> Result<Option<User>> {
        self.db.with_read(|connection| {
            let user = connection
                .query_row(
                    "SELECT id, username, role, enabled, created_at, updated_at, last_login_at
                     FROM users WHERE id = ?1",
                    [id],
                    |row| user_from_row(row, 0),
                )
                .optional()?;
            Ok(user)
        })
    }

    /// Create an account. The caller decides the role (only admins reach here).
    pub fn create_user(&self, username: &str, password: &str, role: Role) -> Result<User> {
        let username = validate_username(username)?;
        validate_password(password)?;
        let hash = hash_password(password)?;
        let now = now_secs();
        let id = self.db.with_write(|connection| {
            if username_taken(connection, &username, None)? {
                return Err(KernelError::Conflict(format!(
                    "user '{username}' already exists"
                )));
            }
            connection.execute(
                "INSERT INTO users (username, password_hash, role, enabled, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4)",
                params![username, hash, role.as_str(), now],
            )?;
            Ok(connection.last_insert_rowid())
        })?;
        self.get_user(id)?
            .ok_or_else(|| KernelError::NotFound(format!("user {id}")))
    }

    /// Apply administrator changes to an account.
    ///
    /// The owner account cannot be demoted or disabled: someone must always be
    /// able to manage the server.
    pub fn update_user(&self, id: i64, patch: UserPatch) -> Result<User> {
        let existing = self
            .get_user(id)?
            .ok_or_else(|| KernelError::NotFound(format!("user {id}")))?;

        if let Some(role) = patch.role {
            if existing.is_owner() && !role.is_admin() {
                return Err(KernelError::PermissionDenied(
                    "the owner account must stay an administrator".into(),
                ));
            }
        }
        if let Some(false) = patch.enabled {
            if existing.is_owner() {
                return Err(KernelError::PermissionDenied(
                    "the owner account cannot be disabled".into(),
                ));
            }
        }

        let hash = match patch.password.as_deref() {
            Some(password) => {
                validate_password(password)?;
                Some(hash_password(password)?)
            }
            None => None,
        };

        let now = now_secs();
        let role = patch.role.map(Role::as_str).map(str::to_string);
        self.db.with_write(|connection| {
            if let Some(role) = &role {
                connection.execute(
                    "UPDATE users SET role = ?1, updated_at = ?2 WHERE id = ?3",
                    params![role, now, id],
                )?;
            }
            if let Some(enabled) = patch.enabled {
                connection.execute(
                    "UPDATE users SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
                    params![i64::from(enabled), now, id],
                )?;
            }
            if let Some(hash) = &hash {
                connection.execute(
                    "UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3",
                    params![hash, now, id],
                )?;
                // A password change ends every other session of that account.
                connection.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
            }
            Ok(())
        })?;
        self.get_user(id)?
            .ok_or_else(|| KernelError::NotFound(format!("user {id}")))
    }

    /// Delete an account and everything that belongs to it.
    pub fn delete_user(&self, id: i64) -> Result<()> {
        if id == OWNER_ID {
            return Err(KernelError::PermissionDenied(
                "the owner account cannot be deleted".into(),
            ));
        }
        let removed = self.db.with_write(|connection| {
            Ok(connection.execute("DELETE FROM users WHERE id = ?1", [id])?)
        })?;
        if removed == 0 {
            return Err(KernelError::NotFound(format!("user {id}")));
        }
        Ok(())
    }

    /// The owner account's password hash (empty when not configured yet).
    fn owner_password_hash(&self) -> Result<String> {
        self.db.with_read(|connection| {
            Ok(connection.query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                [OWNER_ID],
                |row| row.get(0),
            )?)
        })
    }

    fn find_credentials(&self, username: &str) -> Result<Option<(User, String)>> {
        let username = username.trim().to_string();
        self.db.with_read(|connection| {
            let record = connection
                .query_row(
                    "SELECT id, username, role, enabled, created_at, updated_at, last_login_at,
                            password_hash
                     FROM users WHERE username = ?1 COLLATE NOCASE",
                    [username],
                    |row| Ok((user_from_row(row, 0)?, row.get::<_, String>(7)?)),
                )
                .optional()?;
            Ok(record)
        })
    }
}

fn user_from_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<User> {
    let role: String = row.get(offset + 2)?;
    Ok(User {
        id: row.get(offset)?,
        username: row.get(offset + 1)?,
        role: Role::parse(&role).unwrap_or(Role::User),
        enabled: row.get::<_, i64>(offset + 3)? != 0,
        created_at: row.get(offset + 4)?,
        updated_at: row.get(offset + 5)?,
        last_login_at: row.get(offset + 6)?,
    })
}

fn username_taken(
    connection: &rusqlite::Connection,
    username: &str,
    except: Option<i64>,
) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM users WHERE username = ?1 COLLATE NOCASE AND id != ?2",
        params![username, except.unwrap_or(-1)],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Usernames are shown and typed; keep them short, lower-case friendly and free
/// of characters that would need escaping in HTML or a cookie.
pub fn validate_username(raw: &str) -> Result<String> {
    let username = raw.trim().to_string();
    let length = username.chars().count();
    if !(MIN_USERNAME_LEN..=MAX_USERNAME_LEN).contains(&length) {
        return Err(KernelError::InvalidInput(format!(
            "username must be {MIN_USERNAME_LEN}-{MAX_USERNAME_LEN} characters"
        )));
    }
    let mut characters = username.chars();
    let first_ok = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric());
    if !first_ok {
        return Err(KernelError::InvalidInput(
            "username must start with a letter or digit".into(),
        ));
    }
    if !username
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        return Err(KernelError::InvalidInput(
            "username may contain letters, digits, '.', '_' and '-'".into(),
        ));
    }
    Ok(username)
}

pub fn validate_password(password: &str) -> Result<()> {
    let length = password.chars().count();
    if length < MIN_PASSWORD_LEN {
        return Err(KernelError::InvalidInput(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if length > MAX_PASSWORD_LEN {
        return Err(KernelError::InvalidInput(format!(
            "password must be at most {MAX_PASSWORD_LEN} characters"
        )));
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())
        .map_err(|error| KernelError::Config(format!("cannot create a password salt: {error}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| KernelError::Config(format!("cannot hash the password: {error}")))
}

fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations::{apply, Migration};

    const TEST_MIGRATIONS: &[Migration] = &[Migration {
        version: 1,
        name: "accounts",
        sql: include_str!("../migrations/0005_accounts.sql"),
    }];

    fn service() -> (AccountService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::open(&path.to_string_lossy(), 1).unwrap();
        // The accounts migration references media_state; create the minimum
        // shape it needs instead of running the whole media schema.
        db.with_write(|connection| {
            connection.execute_batch(
                "CREATE TABLE media (id INTEGER PRIMARY KEY);
                 CREATE TABLE media_state (
                     media_id INTEGER PRIMARY KEY,
                     liked INTEGER NOT NULL DEFAULT 0,
                     favorite INTEGER NOT NULL DEFAULT 0,
                     position REAL NOT NULL DEFAULT 0,
                     duration REAL NOT NULL DEFAULT 0,
                     updated_at INTEGER NOT NULL
                 );",
            )?;
            Ok(())
        })
        .unwrap();
        apply(&db, "accounts-test", TEST_MIGRATIONS).unwrap();
        (AccountService::new(db), dir)
    }

    #[test]
    fn setup_is_required_until_the_owner_has_a_password() {
        let (accounts, _dir) = service();
        assert!(accounts.setup_required().unwrap());
        let owner = accounts.setup_owner("owner", "correct horse").unwrap();
        assert_eq!(owner.username, "owner");
        assert!(owner.is_admin());
        assert!(!accounts.setup_required().unwrap());
        assert!(accounts.setup_owner("again", "another pass").is_err());
    }

    #[test]
    fn authentication_rejects_wrong_passwords_and_disabled_accounts() {
        let (accounts, _dir) = service();
        accounts.setup_owner("owner", "correct horse").unwrap();
        assert!(accounts
            .authenticate("owner", "correct horse")
            .unwrap()
            .is_some());
        assert!(accounts
            .authenticate("owner", "wrong horse")
            .unwrap()
            .is_none());
        assert!(accounts
            .authenticate("nobody", "correct horse")
            .unwrap()
            .is_none());

        let guest = accounts
            .create_user("guest", "guest password", Role::User)
            .unwrap();
        assert!(accounts
            .authenticate("guest", "guest password")
            .unwrap()
            .is_some());
        accounts
            .update_user(
                guest.id,
                UserPatch {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(accounts
            .authenticate("guest", "guest password")
            .unwrap()
            .is_none());
    }

    #[test]
    fn sessions_resolve_to_their_user_and_can_be_ended() {
        let (accounts, _dir) = service();
        let owner = accounts.setup_owner("owner", "correct horse").unwrap();
        let session = accounts.create_session(owner.id).unwrap();
        let current = accounts.session(&session.id).unwrap().unwrap();
        assert_eq!(current.user.id, owner.id);
        assert_eq!(current.csrf, session.csrf);

        accounts.end_session(&session.id).unwrap();
        assert!(accounts.session(&session.id).unwrap().is_none());
        assert!(accounts.session("not-a-session").unwrap().is_none());
    }

    #[test]
    fn owner_cannot_be_disabled_demoted_or_deleted() {
        let (accounts, _dir) = service();
        accounts.setup_owner("owner", "correct horse").unwrap();
        assert!(accounts
            .update_user(
                OWNER_ID,
                UserPatch {
                    enabled: Some(false),
                    ..Default::default()
                }
            )
            .is_err());
        assert!(accounts
            .update_user(
                OWNER_ID,
                UserPatch {
                    role: Some(Role::User),
                    ..Default::default()
                }
            )
            .is_err());
        assert!(accounts.delete_user(OWNER_ID).is_err());
    }

    #[test]
    fn deleting_a_user_removes_their_state_and_sessions() {
        let (accounts, _dir) = service();
        accounts.setup_owner("owner", "correct horse").unwrap();
        let guest = accounts
            .create_user("guest", "guest password", Role::User)
            .unwrap();
        let session = accounts.create_session(guest.id).unwrap();
        accounts.delete_user(guest.id).unwrap();
        assert!(accounts.session(&session.id).unwrap().is_none());
        assert!(accounts.get_user(guest.id).unwrap().is_none());
    }

    #[test]
    fn usernames_are_validated() {
        assert!(validate_username("ab").is_err());
        assert!(validate_username("_bad").is_err());
        assert!(validate_username("bad name").is_err());
        assert_eq!(validate_username("  Good.Name-1 ").unwrap(), "Good.Name-1");
    }

    #[test]
    fn passwords_are_validated() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("long enough").is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LEN + 1)).is_err());
    }

    #[test]
    fn duplicate_usernames_are_rejected_case_insensitively() {
        let (accounts, _dir) = service();
        accounts.setup_owner("owner", "correct horse").unwrap();
        accounts
            .create_user("guest", "guest password", Role::User)
            .unwrap();
        assert!(accounts
            .create_user("GUEST", "other password", Role::User)
            .is_err());
    }
}
