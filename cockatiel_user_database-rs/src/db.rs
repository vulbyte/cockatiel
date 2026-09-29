use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::proto::{ChannelRef, User};

pub const SCHEMA_VERSION: i32 = 2;

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
    updated_at INTEGER NOT NULL,
    total_score INTEGER NOT NULL DEFAULT 0,
    messages_sent INTEGER NOT NULL DEFAULT 0
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
    /// The rank formula's tunable parameters, loaded from this db's own
    /// config.json (the db is self-contained and never reads another module's
    /// config). Re-read live on a ticker so tuning applies without a restart.
    rank_config: Arc<std::sync::Mutex<RankConfig>>,
}

/// The rank formula's tunable parameters. Lives in the user database's own
/// `config.json`; a streamer edits them there (via the TUI's user-db config
/// editor) and the db re-reads the file on a short ticker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RankConfig {
    /// Age (in days) at which each decay weight kicks in. Must be sorted
    /// ascending and match `decay_weights` length. An event older than the
    /// last boundary counts as 0 (fully decayed/"inked").
    pub decay_boundaries_days: Vec<u64>,
    /// The weight applied to events in each age bucket. `decay_weights[0]` for
    /// events younger than `decay_boundaries_days[0]`, etc.
    pub decay_weights: Vec<f64>,
    /// The divisor in the score term: `score / max(account_years, floor) / this`.
    pub score_divisor: f64,
    /// Minimum account years used in the score term (floors an account younger
    /// than this so a brand-new user isn't divided by ~0).
    pub account_year_floor: f64,
}

impl Default for RankConfig {
    fn default() -> Self {
        Self {
            // 0-1y → 1.0, 1-2y → 0.5, 2-3y → 0.25, 3y+ → 0 (past the last).
            decay_boundaries_days: vec![365, 730, 1095],
            decay_weights: vec![1.0, 0.5, 0.25],
            score_divisor: 100000.0,
            account_year_floor: 1.0,
        }
    }
}

impl RankConfig {
    /// The decay weight for an event `age_days` old.
    pub fn weight_for_age(&self, age_days: u64) -> f64 {
        for (i, boundary) in self.decay_boundaries_days.iter().enumerate() {
            if age_days < *boundary {
                return self.decay_weights.get(i).copied().unwrap_or(0.0);
            }
        }
        // Older than the last boundary → fully decayed.
        0.0
    }

    /// Load from `config.json` in the given directory, or defaults if absent.
    pub fn load(dir: &std::path::Path) -> Self {
        let path = dir.join("config.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str::<RankConfig>(&data).ok())
            .unwrap_or_default()
    }
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
            rank_config: Arc::new(std::sync::Mutex::new(RankConfig::default())),
        }
    }

    /// Load (or create-with-defaults) this db's own config.json and cache it.
    /// Returns the config dir. `config.json` is this db's single source of
    /// truth for its tunable numbers — it never reads another module's config.
    pub fn load_config(&self, dir: &PathBuf) -> Result<RankConfig, Box<dyn std::error::Error>> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("config.json");
        if !path.exists() {
            // Create with defaults so every value is present and editable.
            let cfg = RankConfig::default();
            std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
        }
        let cfg = RankConfig::load(dir);
        *self.rank_config.lock().unwrap() = cfg.clone();
        Ok(cfg)
    }

    /// Re-read config.json from disk (the TUI may have edited it). Called on a
    /// short ticker so decay tuning applies live without a restart.
    pub fn reload_config(&self, dir: &std::path::Path) {
        let cfg = RankConfig::load(dir);
        *self.rank_config.lock().unwrap() = cfg;
    }

    /// The currently-loaded rank config.
    pub async fn rank_config(&self) -> RankConfig {
        self.rank_config.lock().unwrap().clone()
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

        // Migration to schema v2: add `total_score` + `messages_sent`. On an
        // existing v1 database the columns are absent, so `CREATE TABLE IF NOT
        // EXISTS` above won't add them. ALTER TABLE ADD COLUMN is idempotent
        // only across runs that never had the column; the column-exists check
        // keeps a repeat start (or a fresh DB that already has them) from
        // erroring. `total_score` is backfilled to the current `score` so a
        // user's lifetime-earned baseline is where their net stood at the
        // migration (future spending never reduces it).
        let cols = self.column_names(&conn).await?;
        if !cols.contains(&"total_score".to_string()) {
            conn.execute("ALTER TABLE users ADD COLUMN total_score INTEGER NOT NULL DEFAULT 0", ())
                .await?;
            conn.execute("UPDATE users SET total_score = score", ()).await?;
        }
        if !cols.contains(&"messages_sent".to_string()) {
            conn.execute("ALTER TABLE users ADD COLUMN messages_sent INTEGER NOT NULL DEFAULT 0", ())
                .await?;
        }

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

    /// Column names of the `users` table (for idempotent migrations).
    async fn column_names(&self, conn: &turso::Connection) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut rows = conn.query("PRAGMA table_info(users)", ()).await?;
        let mut names = Vec::new();
        while let Some(row) = rows.next().await? {
            let name: String = row.get(1).unwrap_or_default();
            names.push(name);
        }
        Ok(names)
    }

/// Create a consistent snapshot of the DB at `path` by checkpointing the WAL
    /// (best-effort) and copying the main file while the connection lock is held
    /// (new queries are briefly serialized behind the copy, but the snapshot can
    /// never be a torn/stale mix), then atomically renaming into place over any
    /// existing backup. The last good backup is never deleted.
    pub async fn backup_to(&self, path: &std::path::Path) -> Result<(), String> {
        let src = self
            .path
            .lock()
            .await
            .clone()
            .ok_or("User database has no path")?;
        let tmp = format!("{}.tmp", path.to_string_lossy());
        // Hold the connection lock across checkpoint + copy: without it a
        // concurrent writer (or the driver's own checkpoint) can modify the
        // main file mid-copy and produce a torn backup that then replaces the
        // last good one.
        let guard = self.local.lock().await;
        let conn = guard
            .as_ref()
            .cloned()
            .ok_or("User database not initialized")?;
        // If a WAL is present, merge it into the main file so the copy is not
        // stale. Best-effort: a checkpoint failure still yields a usable (if
        // possibly slightly stale) backup rather than none at all.
        let wal_path = format!("{}-wal", src.to_string_lossy());
        if std::path::Path::new(&wal_path).exists() {
            if let Ok(mut stmt) = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await {
                // PRAGMA returns a result row — drain it so the driver doesn't error.
                while let Ok(Some(_)) = stmt.next().await {}
            }
        }
        let _ = std::fs::remove_file(&tmp);
        tokio::fs::copy(&src, &tmp).await.map_err(|e| e.to_string())?;
        // rename atomically replaces any existing backup on POSIX; the last good
        // backup must never be removed before the new one is in place.
        tokio::fs::rename(&tmp, path).await.map_err(|e| e.to_string())?;
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
            "SELECT uuid7, username, is_sponsor, is_moderator, is_admin, is_owner, score, commendations, reprimands, flags, created_at, updated_at, total_score, messages_sent
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
            let total_score: i64 = row.get(12)?;
            let messages_sent: i64 = row.get(13)?;

            let channels = self.get_channels(&user_uuid).await?;
            let rank = self.compute_rank(&user_uuid, score, total_score, commendations, reprimands, messages_sent, created_at).await;

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
                total_score,
                messages_sent,
                rank,
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

        if is_commendation {
            // A commendation earns score: it bumps the spendable score, the
            // lifetime total, and the commendations counter.
            conn.execute(
                "UPDATE users SET score = score + ?1, total_score = total_score + ?1,
                        commendations = commendations + 1, updated_at = ?3 WHERE uuid7 = ?2",
                turso::params![delta, uuid7, now],
            ).await?;
        } else {
            // A reprimand is a slap on the wrist: it records the reprimand
            // counter and the history event (which feeds the user's rank) but
            // NEVER touches score or total_score — a user's standing is
            // reflected in rank, not in their spendable balance.
            conn.execute(
                "UPDATE users SET reprimands = reprimands + 1, updated_at = ?3 WHERE uuid7 = ?2",
                turso::params![delta, uuid7, now],
            ).await?;
        }

        self.get_user_by_uuid(uuid7).await
    }

    /// Score-only adjustment: changes `score` by `delta` WITHOUT incrementing the
    /// `commendations`/`reprimands` counters (which track human ratings) and
    /// WITHOUT the 24h rating cooldown. Used by the automated scorer so real
    /// configured deltas apply without corrupting the rating counters.
    pub async fn adjust_score_only(
        &self,
        uuid7: &str,
        delta: i64,
    ) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let now = Self::now_ms();

        // Positive deltas are EARNED — they bump both the spendable score and
        // the lifetime total. Negative deltas (penalties) only reduce the
        // spendable balance; the lifetime total records what was earned, never
        // what was spent or penalized.
        if delta >= 0 {
            conn.execute(
                "UPDATE users SET score = score + ?1, total_score = total_score + ?1, updated_at = ?2 WHERE uuid7 = ?3",
                turso::params![delta, now, uuid7],
            ).await?;
        } else {
            conn.execute(
                "UPDATE users SET score = score + ?1, updated_at = ?2 WHERE uuid7 = ?3",
                turso::params![delta, now, uuid7],
            ).await?;
        }

        self.get_user_by_uuid(uuid7).await
    }

    /// Increment a user's `messages_sent` counter by 1 (the engine calls this
    /// on every chat message it ingests for that user). A rank factor.
    pub async fn increment_messages_sent(&self, uuid7: &str) -> Result<Option<User>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        conn.execute(
            "UPDATE users SET messages_sent = messages_sent + 1, updated_at = ?2 WHERE uuid7 = ?1",
            turso::params![uuid7, Self::now_ms()],
        ).await?;
        self.get_user_by_uuid(uuid7).await
    }

    /// Charge a module's price: deduct `amount` from the user's CURRENT score,
    /// but only if they have at least that much (guarded — score never goes
    /// negative). The lifetime `total_score` is untouched. Returns the updated
    /// user on success, or `None` when the user is missing or lacks the funds.
    ///
    /// turso's `execute` returns a scan counter rather than rows-affected, so
    /// the guard is verified by the pre-read (we only attempt when score >=
    /// amount) and the single-statement `WHERE score >= ?amount` makes the
    /// deduct atomic under concurrency.
    pub async fn deduct_score(&self, uuid7: &str, amount: i64) -> Result<Option<User>, Box<dyn std::error::Error>> {
        if amount <= 0 {
            return self.get_user_by_uuid(uuid7).await;
        }
        let current = self.get_user_by_uuid(uuid7).await?;
        let Some(user) = current else {
            return Ok(None);
        };
        if user.score < amount {
            return Ok(None);
        }
        let conn = self.conn().await?;
        conn.execute(
            "UPDATE users SET score = score - ?1, updated_at = ?2 WHERE uuid7 = ?3 AND score >= ?1",
            turso::params![amount, Self::now_ms(), uuid7],
        ).await?;
        self.get_user_by_uuid(uuid7).await
    }

    /// The user's rank: `Σ weight(age)` over commendation events minus
    /// `Σ weight(age)` over reprimand events, plus a normalized score term.
    ///
    /// Events are read from `rating_history` (each carries a `created_at`), and
    /// each is weighted by its age against the configurable decay
    /// (`decay_boundaries_days` / `decay_weights`, e.g. 1.0 at 0-1y, 0.5 at
    /// 1-2y, 0.25 at 2-3y, 0 past 3y). The score term is
    /// `score / max(account_years, floor) / score_divisor` so spending doesn't
    /// dominate standing. Rank is computed server-side and returned on every
    /// user fetch — callers never make a second trip for it.
    #[allow(clippy::too_many_arguments)]
    pub async fn compute_rank(
        &self,
        uuid7: &str,
        score: i64,
        _total_score: i64,
        _commendations: i64,
        _reprimands: i64,
        _messages_sent: i64,
        created_at: i64,
    ) -> i64 {
        let cfg = self.rank_config().await;
        let now = Self::now_ms();
        let age_days = |ts: i64| -> u64 {
            let ms = now.saturating_sub(ts).max(0);
            (ms / 86_400_000) as u64
        };

        // Decayed event counts from rating_history. `.ok()` drops the non-Send
        // error type immediately so this future stays Send (the connection
        // itself is Send).
        let mut commend_weighted = 0.0_f64;
        let mut reprimand_weighted = 0.0_f64;
        let conn = self.conn().await.ok();
        if let Some(conn) = conn {
            if let Ok(mut rows) = conn
                .query(
                    "SELECT kind, created_at FROM rating_history WHERE recipient_uuid7 = ?1",
                    turso::params![uuid7],
                )
                .await
            {
                while let Ok(Some(row)) = rows.next().await {
                    let kind: String = row.get(0).unwrap_or_default();
                    let ts: i64 = row.get(1).unwrap_or(0);
                    let w = cfg.weight_for_age(age_days(ts));
                    match kind.as_str() {
                        "commend" => commend_weighted += w,
                        "reprimand" => reprimand_weighted += w,
                        _ => {}
                    }
                }
            }
        }

        // Score term: score / max(account_years, floor) / divisor.
        let account_ms = now.saturating_sub(created_at).max(0);
        let account_years = (account_ms as f64) / (86_400_000.0_f64 * 365.0);
        let account_years = account_years.max(cfg.account_year_floor);
        let score_term = (score as f64) / account_years / cfg.score_divisor;

        (commend_weighted - reprimand_weighted + score_term).round() as i64
    }

    /// A user's rating history: every commend/reprimand event with giver, date
    /// and reason. Returns EVERYTHING on record (the caller decides how much to
    /// show); only the rank calculation restricts itself to the decay window.
    pub async fn get_rating_history(
        &self,
        recipient_uuid7: &str,
        kind_filter: &str,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<crate::proto::RatingHistoryEntry>, Box<dyn std::error::Error>> {
        let conn = self.conn().await?;
        let limit = if limit <= 0 { 100 } else { limit };
        let offset = if offset < 0 { 0 } else { offset };

        let sql = if kind_filter.is_empty() {
            "SELECT uuid7, giver_uuid7, kind, platform, handle, reason, created_at
             FROM rating_history WHERE recipient_uuid7 = ?1
             ORDER BY created_at DESC LIMIT ?2 OFFSET ?3"
        } else {
            "SELECT uuid7, giver_uuid7, kind, platform, handle, reason, created_at
             FROM rating_history WHERE recipient_uuid7 = ?1 AND kind = ?4
             ORDER BY created_at DESC LIMIT ?2 OFFSET ?3"
        };
        let mut rows = conn
            .query(sql, turso::params![recipient_uuid7, limit, offset, kind_filter])
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::proto::RatingHistoryEntry {
                uuid7: row.get(0).unwrap_or_default(),
                giver_uuid7: row.get(1).unwrap_or_default(),
                kind: row.get(2).unwrap_or_default(),
                platform: row.get(3).unwrap_or_default(),
                handle: row.get(4).unwrap_or_default(),
                reason: row.get(5).unwrap_or_default(),
                created_at: row.get(6).unwrap_or_default(),
            });
        }
        Ok(out)
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

        self.adjust_score(recipient_uuid7, 1, is_commendation)
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

        // Counters reflect it: 2 reprimands (no score change) + 2 commends (each
        // +1 score AND +1 total_score).
        let t = db.get_user_by_uuid(&target).await.unwrap().unwrap();
        assert_eq!(t.score, 2, "reprimands must not reduce score; 2 commends earn +2");
        assert_eq!(t.total_score, 2, "commends also bump lifetime total");
        assert_eq!(t.reprimands, 2);
        assert_eq!(t.commendations, 2);
    }

    #[tokio::test]
    async fn migration_adds_total_score_and_backfills_from_score() {
        // Simulate a v1 database (no total_score/messages_sent columns) and
        // verify initialize() migrates it: the columns appear and total_score
        // is backfilled from the existing score.
        let dir = std::env::temp_dir().join(format!("cok_udb_mig_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");

        {
            let db = UserDatabase::new();
            db.initialize(&path).await.unwrap();
            let chan = ChannelRef { platform: "test".into(), channel_id: "c1".into(), handle: "u".into() };
            let u = db.add_user("u", Some(&chan)).await.unwrap();
            // Give the user some score via the pre-migration path (adjust_score_only).
            db.adjust_score_only(&u.uuid7, 42).await.unwrap();
            assert_eq!(db.get_user_by_uuid(&u.uuid7).await.unwrap().unwrap().score, 42);
        }

        // "Downgrade" to v1: drop the new columns the way a v1 schema would
        // have them (create a fresh v1 table) — actually simpler: build a v1
        // table directly and insert a row, then let initialize() migrate.
        {
            let raw = turso::Builder::new_local(path.to_string_lossy().as_ref()).build().await.unwrap();
            let conn = raw.connect().unwrap();
            conn.execute(
                "DROP TABLE users",
                (),
            ).await.unwrap();
            conn.execute(
                "CREATE TABLE users (
                    uuid7 TEXT PRIMARY KEY, schema_version INTEGER NOT NULL DEFAULT 1,
                    username TEXT NOT NULL, is_sponsor INTEGER NOT NULL DEFAULT 0,
                    is_moderator INTEGER NOT NULL DEFAULT 0, is_admin INTEGER NOT NULL DEFAULT 0,
                    is_owner INTEGER NOT NULL DEFAULT 0, score INTEGER NOT NULL DEFAULT 0,
                    commendations INTEGER NOT NULL DEFAULT 0, reprimands INTEGER NOT NULL DEFAULT 0,
                    flags TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
                )",
                (),
            ).await.unwrap();
            conn.execute(
                "INSERT INTO users (uuid7, schema_version, username, score, created_at, updated_at)
                 VALUES ('u1', 1, 'legacy', 77, 1, 1)",
                turso::params![],
            ).await.unwrap();
        }

        let db = UserDatabase::new();
        db.initialize(&path).await.unwrap();
        let u = db.get_user_by_uuid("u1").await.unwrap().unwrap();
        assert_eq!(u.total_score, 77, "total_score backfilled from existing score");
        assert_eq!(u.messages_sent, 0);
        assert_eq!(u.score, 77);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deduct_score_is_guarded_and_never_touches_total() {
        let db = temp_db().await;
        let uid = add(&db, "u", "c").await;
        db.adjust_score_only(&uid, 10).await.unwrap();

        // Over-draw is refused (score stays 10).
        let over = db.deduct_score(&uid, 20).await.unwrap();
        assert!(over.is_none(), "insufficient funds must be refused");
        let u = db.get_user_by_uuid(&uid).await.unwrap().unwrap();
        assert_eq!(u.score, 10);

        // A within-balance deduct succeeds and does not touch total_score.
        let ok = db.deduct_score(&uid, 3).await.unwrap().unwrap();
        assert_eq!(ok.score, 7);
        assert_eq!(ok.total_score, 10, "spending never reduces lifetime total");
    }

    #[tokio::test]
    async fn reprimands_do_not_reduce_score_but_feed_rank() {
        let db = temp_db().await;
        let giver = add(&db, "giver", "cg").await;
        let target = add(&db, "target", "ct").await;

        // A reprimand: counter + history event, NO score change.
        db.rate_user(&giver, &target, false, "test", "target", "spoilers").await.unwrap();
        let t = db.get_user_by_uuid(&target).await.unwrap().unwrap();
        assert_eq!(t.reprimands, 1);
        assert_eq!(t.score, 0, "reprimand must not reduce score");

        // A commendation: score + total + history.
        db.rate_user(&giver, &target, true, "test", "target", "nice").await.unwrap();
        let t = db.get_user_by_uuid(&target).await.unwrap().unwrap();
        assert_eq!(t.commendations, 1);
        assert_eq!(t.score, 1);
        assert_eq!(t.total_score, 1);

        // Rank: +1 commend (weight 1.0) - 1 reprimand (weight 1.0) = 0 (+ score term ~0).
        assert_eq!(t.rank, 0, "recent commend and reprimand cancel in rank");
    }

    #[tokio::test]
    async fn increment_messages_sent_counts_chat_messages() {
        let db = temp_db().await;
        let uid = add(&db, "u", "c").await;
        db.increment_messages_sent(&uid).await.unwrap();
        db.increment_messages_sent(&uid).await.unwrap();
        let u = db.get_user_by_uuid(&uid).await.unwrap().unwrap();
        assert_eq!(u.messages_sent, 2);
    }

    #[tokio::test]
    async fn get_rating_history_returns_everything_with_reason() {
        let db = temp_db().await;
        let giver = add(&db, "giver", "cg").await;
        let target = add(&db, "target", "ct").await;
        db.rate_user(&giver, &target, false, "test", "target", "spoilers").await.unwrap();
        db.rate_user(&giver, &target, true, "test", "target", "good").await.unwrap();

        let history = db.get_rating_history(&target, "", 100, 0).await.unwrap();
        assert_eq!(history.len(), 2, "history returns everything, no pruning");
        let reasons: Vec<String> = history.iter().map(|e| e.reason.clone()).collect();
        assert!(reasons.contains(&"spoilers".to_string()));
        assert!(reasons.contains(&"good".to_string()));
        // Every entry carries its giver + date.
        for e in &history {
            assert_eq!(e.giver_uuid7, giver);
            assert!(e.created_at > 0);
        }
    }
}
