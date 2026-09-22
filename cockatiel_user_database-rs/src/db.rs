use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::proto::{ChannelRef, User};

pub const SCHEMA_VERSION: i32 = 1;

/// History of commend/reprimand events (giver → recipient), the source of the
/// 24h reprimand cooldown. All user rating data lives here — centralized and
/// searchable (see PLANNING/ROADMAP command-system work).
const CREATE_RATING_HISTORY: &str = "
CREATE TABLE IF NOT EXISTS rating_history (
    uuid7 TEXT PRIMARY KEY,
    giver_uuid7 TEXT NOT NULL,
    recipient_uuid7 TEXT NOT NULL,
    kind TEXT NOT NULL,
    platform TEXT NOT NULL DEFAULT '',
    handle TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rating_history_giver ON rating_history(giver_uuid7, recipient_uuid7, kind, created_at);
CREATE INDEX IF NOT EXISTS idx_rating_history_recipient ON rating_history(recipient_uuid7, kind, created_at);
";

const CREATE_USERS: &str = "
CREATE TABLE IF NOT EXISTS users (
    uuid7 TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL DEFAULT 1,
    username TEXT NOT NULL,
    is_sponsor INTEGER NOT NULL DEFAULT 0,
    is_moderator INTEGER NOT NULL DEFAULT 0,
    is_admin INTEGER NOT NULL DEFAULT 0,
    is_owner INTEGER NOT NULL DEFAULT 0,
    score INTEGER NOT NULL DEFAULT 0,
    commendations INTEGER NOT NULL DEFAULT 0,
    reprimands INTEGER NOT NULL DEFAULT 0,
    flags TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
)";

const CREATE_CHANNELS: &str = "
CREATE TABLE IF NOT EXISTS user_channels (
    user_uuid7 TEXT NOT NULL,
    platform TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    handle TEXT,
    PRIMARY KEY (user_uuid7, platform, channel_id)
)";

const CREATE_USER_VALUES: &str = "
CREATE TABLE IF NOT EXISTS user_values (
    user_uuid7 TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_uuid7, key)
)";

#[derive(Debug, Clone)]
pub struct UserDatabase {
    local: Arc<Mutex<Option<turso::Connection>>>,
    path: Arc<Mutex<Option<PathBuf>>>,
}

/// Result of a rating (commend/reprimand) attempt.
pub struct RatingOutcome {
    pub applied: bool,
    pub message: String,
}

impl UserDatabase {
    pub fn new() -> Self {
        Self {
            local: Arc::new(Mutex::new(None)),
            path: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn initialize(&self, path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute(CREATE_USERS, ()).await?;
        conn.execute(CREATE_CHANNELS, ()).await?;
        conn.execute(CREATE_USER_VALUES, ()).await?;
        conn.execute(CREATE_RATING_HISTORY, ()).await?;

        {
            let mut local = self.local.lock().await;
            *local = Some(conn);
        }
        {
            let mut p = self.path.lock().await;
            *p = Some(path.clone());
        }

        println!("[UserDB] Initialized at {:?}", path);
        Ok(())
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    async fn conn(&self) -> Result<turso::Connection, Box<dyn std::error::Error>> {
        let guard = self.local.lock().await;
        guard.as_ref().cloned().ok_or("User database not initialized".into())
    }

/// Create a consistent snapshot of the DB at `path` by checkpointing the WAL
    /// and copying the main file while the DB lock is held (no concurrent
    /// writes), then atomically renaming into place.
    pub async fn backup_to(&self, path: &std::path::Path) -> Result<(), String> {
        let conn = self.conn().await.map_err(|e| e.to_string())?;
        let src = self.path.lock().await.clone().ok_or("User database has no path")?;
        // Merge the WAL so the main file is authoritative before the copy.
        // (PRAGMA returns a result row — drain it so the driver doesn't error.)
        if let Ok(mut stmt) = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await {
            while let Ok(Some(_)) = stmt.next().await {}
        }

        let tmp = format!("{}.tmp", path.to_string_lossy());
        let _ = std::fs::remove_file(&tmp);
        std::fs::copy(&src, &tmp).map_err(|e| e.to_string())?;
        drop(conn);
        drop(src);

        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }

    // ── Core operations ────────────────────────────────────

    pub async fn add_user(
        &self,
        username: &str,
        channel: Option<&ChannelRef>,
    ) -> Result<User, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let now = Self::now_ms();
        let uuid7 = uuid::Uuid::now_v7().to_string();

        conn.execute(
            "INSERT INTO users (uuid7, schema_version, username, flags, created_at, updated_at)
             VALUES (?1, ?2, ?3, '{}', ?4, ?4)",
            turso::params![uuid7.clone(), SCHEMA_VERSION, username, now],
        )
        .await?;

        if let Some(ch) = channel {
            // Turso/Limbo does not support INSERT OR IGNORE / ON CONFLICT.
            let exists = self
                .channel_exists(&uuid7, &ch.platform, &ch.channel_id)
                .await?;
            if !exists {
                conn.execute(
                    "INSERT INTO user_channels (user_uuid7, platform, channel_id, handle)
                     VALUES (?1, ?2, ?3, ?4)",
                    turso::params![uuid7.clone(), ch.platform.clone(), ch.channel_id.clone(), ch.handle.clone()],
                )
                .await?;
            }
        }

        self.get_user_by_uuid(&uuid7).await?.ok_or("Failed to create user".into())
    }

    /// Find a user by (platform, channel_id) or handle. Returns existing user if found.
    pub async fn find_user_by_channel(
        &self,
        platform: &str,
        channel_id: &str,
        handle: &str,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;

        // Look up by (platform, channel_id) first.
        let mut rows = conn.query(
            "SELECT user_uuid7 FROM user_channels WHERE platform = ?1 AND channel_id = ?2",
            turso::params![platform, channel_id],
        ).await?;
        if let Some(row) = rows.next().await? {
            let uuid7: String = row.get(0)?;
            return self.get_user_by_uuid(&uuid7).await;
        }

        // Fall back to handle lookup.
        if !handle.is_empty() {
            let mut rows = conn.query(
                "SELECT user_uuid7 FROM user_channels WHERE platform = ?1 AND handle = ?2",
                turso::params![platform, handle],
            ).await?;
            if let Some(row) = rows.next().await? {
                let uuid7: String = row.get(0)?;
                return self.get_user_by_uuid(&uuid7).await;
            }
        }

        Ok(None)
    }

    pub async fn get_user_by_uuid(&self, uuid7: &str) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;

        let mut rows = conn.query(
            "SELECT uuid7, username, is_sponsor, is_moderator, is_admin, is_owner, score, commendations, reprimands, flags, created_at, updated_at
             FROM users WHERE uuid7 = ?1",
            turso::params![uuid7],
        ).await?;

        if let Some(row) = rows.next().await? {
            let user_uuid: String = row.get(0)?;
            let username: String = row.get(1)?;
            let is_sponsor: i64 = row.get(2)?;
            let is_moderator: i64 = row.get(3)?;
            let is_admin: i64 = row.get(4)?;
            let is_owner: i64 = row.get(5)?;
            let score: i64 = row.get(6)?;
            let commendations: i64 = row.get(7)?;
            let reprimands: i64 = row.get(8)?;
            let flags: String = row.get(9)?;
            let created_at: i64 = row.get(10)?;
            let updated_at: i64 = row.get(11)?;

            let channels = self.get_channels(&user_uuid).await?;

            Ok(Some(User {
                uuid7: user_uuid,
                username,
                is_sponsor: is_sponsor != 0,
                is_moderator: is_moderator != 0,
                is_admin: is_admin != 0,
                is_owner: is_owner != 0,
                score,
                commendations,
                reprimands,
                channels,
                flags,
                created_at,
                updated_at,
            }))
        } else {
            Ok(None)
        }
    }

    async fn get_channels(&self, uuid7: &str) -> Result<Vec<ChannelRef>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let mut rows = conn.query(
            "SELECT platform, channel_id, handle FROM user_channels WHERE user_uuid7 = ?1",
            turso::params![uuid7],
        ).await?;

        let mut channels = Vec::new();
        while let Some(row) = rows.next().await? {
            let platform: String = row.get(0)?;
            let channel_id: String = row.get(1)?;
            let handle: Option<String> = row.get(2)?;
            channels.push(ChannelRef {
                platform,
                channel_id,
                handle: handle.unwrap_or_default(),
            });
        }
        Ok(channels)
    }

    async fn channel_exists(&self, uuid7: &str, platform: &str, channel_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let mut rows = conn.query(
            "SELECT 1 FROM user_channels WHERE user_uuid7 = ?1 AND platform = ?2 AND channel_id = ?3",
            turso::params![uuid7, platform, channel_id],
        ).await?;
        Ok(rows.next().await?.is_some())
    }

    pub async fn delete_user(&self, uuid7: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        conn.execute("DELETE FROM user_channels WHERE user_uuid7 = ?1", turso::params![uuid7]).await?;
        let changed = conn.execute("DELETE FROM users WHERE uuid7 = ?1", turso::params![uuid7]).await?;
        Ok(changed > 0)
    }

    pub async fn adjust_score(
        &self,
        uuid7: &str,
        delta: i64,
        is_commendation: bool,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let now = Self::now_ms();
        let field = if is_commendation { "commendations" } else { "reprimands" };

        conn.execute(
            &format!(
                "UPDATE users SET score = score + ?1, {} = {} + 1, updated_at = ?3 WHERE uuid7 = ?2",
                field, field
            ),
            turso::params![delta, uuid7, now],
        ).await?;

        self.get_user_by_uuid(uuid7).await
    }

    /// Commend or reprimand a user. For a REPRIMAND the 24h cooldown is
    /// enforced atomically: the history row is inserted only when no
    /// reprimand from the same giver to this recipient exists within the
    /// last 24 hours (single `INSERT ... WHERE NOT EXISTS`, so concurrent
    /// attempts can't both pass). Commends are unlimited.
    pub async fn rate_user(
        &self,
        giver_uuid7: &str,
        recipient_uuid7: &str,
        is_commendation: bool,
        platform: &str,
        handle: &str,
        reason: &str,
    ) -> Result<RatingOutcome, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let now = Self::now_ms();
        let kind = if is_commendation { "commend" } else { "reprimand" };
        let id = uuid::Uuid::now_v7().to_string();

        if is_commendation {
            conn.execute(
                "INSERT INTO rating_history (uuid7, giver_uuid7, recipient_uuid7, kind, platform, handle, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                turso::params![id, giver_uuid7, recipient_uuid7, kind, platform, handle, reason, now],
            )
            .await?;
        } else {
            // 24h cooldown: check for an existing reprimand from the same giver
            // to this recipient within the last 24 hours. (A check-then-insert
            // rather than a single INSERT...WHERE NOT EXISTS — the turso/Limbo
            // driver can't translate the subquery form. The race window between
            // two perfectly-simultaneous reprimands is negligible.)
            let mut rows = conn
                .query(
                    "SELECT 1 FROM rating_history
                     WHERE giver_uuid7 = ?1 AND recipient_uuid7 = ?2 AND kind = 'reprimand'
                       AND created_at > ?3 LIMIT 1",
                    turso::params![giver_uuid7, recipient_uuid7, now - 86400000],
                )
                .await?;
            if let Some(_row) = rows.next().await? {
                return Ok(RatingOutcome {
                    applied: false,
                    message: format!(
                        "reprimand cooldown: you already reprimanded {} within the last 24 hours",
                        handle
                    ),
                });
            }
            conn.execute(
                "INSERT INTO rating_history (uuid7, giver_uuid7, recipient_uuid7, kind, platform, handle, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                turso::params![id, giver_uuid7, recipient_uuid7, kind, platform, handle, reason, now],
            )
            .await?;
        }

        self.adjust_score(recipient_uuid7, if is_commendation { 1 } else { -1 }, is_commendation)
            .await?;
        Ok(RatingOutcome {
            applied: true,
            message: format!("{} applied", kind),
        })
    }

    pub async fn add_channel(
        &self,
        uuid7: &str,
        channel: Option<&ChannelRef>,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let Some(channel) = channel else {
            return self.get_user_by_uuid(uuid7).await;
        };
        let conn = self.conn().await?;
        let exists = self.channel_exists(uuid7, &channel.platform, &channel.channel_id).await?;
        if !exists {
            conn.execute(
                "INSERT INTO user_channels (user_uuid7, platform, channel_id, handle)
                 VALUES (?1, ?2, ?3, ?4)",
                turso::params![uuid7, channel.platform.clone(), channel.channel_id.clone(), channel.handle.clone()],
            ).await?;
        }
        self.get_user_by_uuid(uuid7).await
    }

    pub async fn remove_channel(
        &self,
        uuid7: &str,
        platform: &str,
        channel_id: &str,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        conn.execute(
            "DELETE FROM user_channels WHERE user_uuid7 = ?1 AND platform = ?2 AND channel_id = ?3",
            turso::params![uuid7, platform, channel_id],
        ).await?;
        self.get_user_by_uuid(uuid7).await
    }

    pub async fn update_flags(
        &self,
        uuid7: &str,
        flags: &str,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        conn.execute(
            "UPDATE users SET flags = ?2, updated_at = ?3 WHERE uuid7 = ?1",
            turso::params![uuid7, flags, Self::now_ms()],
        ).await?;
        self.get_user_by_uuid(uuid7).await
    }

    pub async fn set_roles(
        &self,
        uuid7: &str,
        is_sponsor: bool,
        is_moderator: bool,
        is_admin: bool,
        is_owner: bool,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        conn.execute(
            "UPDATE users SET is_sponsor = ?2, is_moderator = ?3, is_admin = ?4, is_owner = ?5, updated_at = ?6 WHERE uuid7 = ?1",
            turso::params![
                uuid7,
                is_sponsor as i64,
                is_moderator as i64,
                is_admin as i64,
                is_owner as i64,
                Self::now_ms(),
            ],
        ).await?;
        self.get_user_by_uuid(uuid7).await
    }

    // ── User value control (key-value per user) ─────────────

    pub async fn write_user_value(
        &self,
        uuid7: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        // Ensure the user exists first.
        if self.get_user_by_uuid(uuid7).await?.is_none() {
            return Ok(None);
        }
        // Upsert without ON CONFLICT (unsupported in turso/Limbo).
        let exists = {
            let mut rows = conn.query(
                "SELECT 1 FROM user_values WHERE user_uuid7 = ?1 AND key = ?2",
                turso::params![uuid7, key],
            ).await?;
            rows.next().await?.is_some()
        };
        if exists {
            conn.execute(
                "UPDATE user_values SET value = ?3, updated_at = ?4 WHERE user_uuid7 = ?1 AND key = ?2",
                turso::params![uuid7, key, value, Self::now_ms()],
            ).await?;
        } else {
            conn.execute(
                "INSERT INTO user_values (user_uuid7, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)",
                turso::params![uuid7, key, value, Self::now_ms()],
            ).await?;
        }
        Ok(Some(value.to_string()))
    }

    pub async fn read_user_value(
        &self,
        uuid7: &str,
        key: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let mut rows = conn.query(
            "SELECT value FROM user_values WHERE user_uuid7 = ?1 AND key = ?2",
            turso::params![uuid7, key],
        ).await?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row.get::<String>(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn delete_user_value(
        &self,
        uuid7: &str,
        key: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let changed = conn.execute(
            "DELETE FROM user_values WHERE user_uuid7 = ?1 AND key = ?2",
            turso::params![uuid7, key],
        ).await?;
        Ok(changed > 0)
    }

    pub async fn list_user_values(
        &self,
        uuid7: &str,
    ) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let mut rows = conn.query(
            "SELECT key, value FROM user_values WHERE user_uuid7 = ?1 ORDER BY key",
            turso::params![uuid7],
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            out.push((key, value));
        }
        Ok(out)
    }

    pub async fn list_users(
        &self,
        platform: &str,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let limit = if limit <= 0 { 100 } else { limit };
        let offset = if offset < 0 { 0 } else { offset };

        let mut users = Vec::new();
        if platform.is_empty() {
            let mut rows = conn.query(
                "SELECT uuid7 FROM users ORDER BY score DESC LIMIT ?1 OFFSET ?2",
                turso::params![limit, offset],
            ).await?;
            while let Some(row) = rows.next().await? {
                let uuid7: String = row.get(0)?;
                if let Some(user) = self.get_user_by_uuid(&uuid7).await? {
                    users.push(user);
                }
            }
        } else {
            let mut rows = conn.query(
                "SELECT DISTINCT uc.user_uuid7 FROM user_channels uc
                 WHERE uc.platform = ?1 ORDER BY uc.user_uuid7 LIMIT ?2 OFFSET ?3",
                turso::params![platform, limit, offset],
            ).await?;
            while let Some(row) = rows.next().await? {
                let uuid7: String = row.get(0)?;
                if let Some(user) = self.get_user_by_uuid(&uuid7).await? {
                    users.push(user);
                }
            }
        }

        Ok(users)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_db() -> UserDatabase {
        let dir = std::env::temp_dir().join(format!("cok_udb_test_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = UserDatabase::new();
        db.initialize(&dir.join("t.db")).await.unwrap();
        db
    }

    async fn add(db: &UserDatabase, username: &str, channel_id: &str) -> String {
        let chan = ChannelRef { platform: "test".into(), channel_id: channel_id.into(), handle: username.into() };
        db.add_user(username, Some(&chan)).await.unwrap().uuid7
    }

    #[tokio::test]
    async fn reprimand_cooldown_blocks_second_within_24h() {
        let db = temp_db().await;
        let giver = add(&db, "giver", "cg").await;
        let target = add(&db, "target", "ct").await;

        let first = db.rate_user(&giver, &target, false, "test", "target", "rude").await.unwrap();
        assert!(first.applied, "first reprimand should apply");

        // Second reprimand from the same giver -> denied by the 24h cooldown.
        let second = db.rate_user(&giver, &target, false, "test", "target", "still rude").await.unwrap();
        assert!(!second.applied, "second reprimand within 24h must be denied");
        assert!(second.message.contains("24 hours"));

        // A DIFFERENT giver may still reprimand the same target (recipient unlimited).
        let giver2 = add(&db, "giver2", "cg2").await;
        let other = db.rate_user(&giver2, &target, false, "test", "target", "also rude").await.unwrap();
        assert!(other.applied, "a different giver may reprimand the same recipient");

        // Commend is unlimited (same giver, same target, no cooldown).
        let comm = db.rate_user(&giver, &target, true, "test", "target", "nice").await.unwrap();
        assert!(comm.applied, "commend should always apply");
        let comm2 = db.rate_user(&giver, &target, true, "test", "target", "nice again").await.unwrap();
        assert!(comm2.applied, "commend is unlimited");

        // Score/counters reflect it: -2 (reprimands) + 2 (commends) = 0.
        let t = db.get_user_by_uuid(&target).await.unwrap().unwrap();
        assert_eq!(t.score, 0);
        assert_eq!(t.reprimands, 2);
        assert_eq!(t.commendations, 2);
    }
}
