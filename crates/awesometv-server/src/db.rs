use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

const WEEK_MS: i64 = 7 * 86_400_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub id: String,
    pub path: String,
    pub title: String,
    pub kind: String,
    pub duration_ms: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<i64>,
    #[serde(skip)]
    pub chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chapter {
    pub start_ms: i64,
    pub end_ms: i64,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub number: String,
    pub name: String,
    pub start_ms: i64,
    pub schedule_mode: String,
    pub graphics: GraphicsSettings,
    pub stream: StreamSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphicsSettings {
    #[serde(default)]
    pub text: String,
    #[serde(default = "default_graphics_position")]
    pub position: String,
    #[serde(default = "default_graphics_opacity")]
    pub opacity: i64,
    #[serde(default = "default_graphics_size")]
    pub size: i64,
    #[serde(default)]
    pub watermark_media_id: Option<String>,
    #[serde(default = "default_watermark_position")]
    pub watermark_position: String,
    #[serde(default = "default_watermark_opacity")]
    pub watermark_opacity: i64,
    #[serde(default = "default_watermark_scale")]
    pub watermark_scale: i64,
    #[serde(default)]
    pub lower_third_text: String,
    #[serde(default = "default_lower_third_size")]
    pub lower_third_size: i64,
    #[serde(default = "default_lower_third_opacity")]
    pub lower_third_opacity: i64,
}

impl Default for GraphicsSettings {
    fn default() -> Self {
        Self {
            text: String::new(),
            position: default_graphics_position(),
            opacity: default_graphics_opacity(),
            size: default_graphics_size(),
            watermark_media_id: None,
            watermark_position: default_watermark_position(),
            watermark_opacity: default_watermark_opacity(),
            watermark_scale: default_watermark_scale(),
            lower_third_text: String::new(),
            lower_third_size: default_lower_third_size(),
            lower_third_opacity: default_lower_third_opacity(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSettings {
    #[serde(default = "default_stream_width")]
    pub width: i64,
    #[serde(default = "default_stream_height")]
    pub height: i64,
    #[serde(default = "default_stream_preset")]
    pub preset: String,
    #[serde(default = "default_stream_crf")]
    pub crf: i64,
    /// Video bitrate ceiling in kbps. 0 uses the global default.
    #[serde(default)]
    pub max_kbps: i64,
    #[serde(default = "default_stream_audio_kbps")]
    pub audio_kbps: i64,
    #[serde(default)]
    pub audio_language: Option<String>,
    #[serde(default = "default_subtitle_mode")]
    pub subtitle_mode: String,
    #[serde(default)]
    pub subtitle_language: Option<String>,
    #[serde(default = "default_hdr_mode")]
    pub hdr_mode: String,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default = "default_segment_seconds")]
    pub segment_seconds: i64,
    #[serde(default = "default_prebuffer_seconds")]
    pub prebuffer_seconds: i64,
    #[serde(default = "default_cache_segments")]
    pub cache_segments: i64,
    #[serde(default)]
    pub prewarm: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            width: default_stream_width(),
            height: default_stream_height(),
            preset: default_stream_preset(),
            crf: default_stream_crf(),
            max_kbps: 0,
            audio_kbps: default_stream_audio_kbps(),
            audio_language: None,
            subtitle_mode: default_subtitle_mode(),
            subtitle_language: None,
            hdr_mode: default_hdr_mode(),
            encoder: default_encoder(),
            segment_seconds: default_segment_seconds(),
            prebuffer_seconds: default_prebuffer_seconds(),
            cache_segments: default_cache_segments(),
            prewarm: false,
        }
    }
}

fn default_graphics_position() -> String {
    "bottom-right".into()
}
fn default_graphics_opacity() -> i64 {
    60
}
fn default_graphics_size() -> i64 {
    28
}
fn default_watermark_position() -> String {
    "top-right".into()
}
fn default_watermark_opacity() -> i64 {
    70
}
fn default_watermark_scale() -> i64 {
    12
}
fn default_lower_third_size() -> i64 {
    22
}
fn default_lower_third_opacity() -> i64 {
    65
}
fn default_stream_width() -> i64 {
    1280
}
fn default_stream_height() -> i64 {
    720
}
fn default_stream_preset() -> String {
    "veryfast".into()
}
fn default_stream_crf() -> i64 {
    22
}
fn default_stream_audio_kbps() -> i64 {
    160
}
fn default_subtitle_mode() -> String {
    "off".into()
}
fn default_hdr_mode() -> String {
    "passthrough".into()
}
fn default_encoder() -> String {
    "software".into()
}
fn default_segment_seconds() -> i64 {
    4
}
fn default_prebuffer_seconds() -> i64 {
    8
}
fn default_cache_segments() -> i64 {
    12
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelItem {
    pub media_id: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub in_ms: i64,
    pub out_ms: Option<i64>,
    pub kind: String,
}

pub async fn init(data_dir: &Path) -> Result<SqlitePool> {
    tokio::fs::create_dir_all(data_dir).await?;
    let db_path = data_dir.join("awesometv.db");
    let pending = data_dir.join("restore.pending");
    if pending.is_file() {
        crate::backup::validate(&pending)
            .await
            .context("validate pending restore")?;
        // Fold any committed WAL into the old database before preserving it.
        if db_path.is_file() {
            let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
            let old = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await?;
            let checkpoint: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_one(&old)
                .await?;
            anyhow::ensure!(
                checkpoint.0 == 0,
                "cannot checkpoint existing database before restore"
            );
            old.close().await;
            // Remove checkpointed sidecars before replacing the database.
            for suffix in ["awesometv.db-wal", "awesometv.db-shm"] {
                match std::fs::remove_file(data_dir.join(suffix)) {
                    Ok(()) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error).context("remove old SQLite sidecar"),
                }
            }
            let previous =
                data_dir.join(format!("awesometv.pre-restore.{}.db", uuid::Uuid::new_v4()));
            std::fs::rename(&db_path, &previous).context("preserve database before restore")?;
            if let Err(error) = std::fs::rename(&pending, &db_path) {
                std::fs::rename(&previous, &db_path).context("roll back failed restore")?;
                return Err(error).context("apply restore");
            }
        } else {
            anyhow::ensure!(
                !data_dir.join("awesometv.db-wal").exists()
                    && !data_dir.join("awesometv.db-shm").exists(),
                "orphaned SQLite sidecars require operator inspection before restore"
            );
            std::fs::rename(&pending, &db_path).context("apply restore")?;
        }
    }
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .with_context(|| format!("open sqlite database {url}"))?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS media_items (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            kind TEXT NOT NULL DEFAULT 'movie',
            duration_ms INTEGER NOT NULL,
            width INTEGER,
            height INTEGER,
            video_codec TEXT,
            audio_codec TEXT,
            size_bytes INTEGER,
            chapters_probed INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS media_chapters (
            media_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            start_ms INTEGER NOT NULL,
            end_ms INTEGER NOT NULL,
            title TEXT,
            PRIMARY KEY (media_id, position)
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS audit_events (
            id TEXT PRIMARY KEY,
            at_ms INTEGER NOT NULL,
            actor TEXT NOT NULL,
            kind TEXT NOT NULL,
            action TEXT NOT NULL,
            target TEXT,
            detail TEXT,
            client_ip TEXT
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_audit_events_at ON audit_events(at_ms DESC);")
        .execute(&pool)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS api_tokens (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            scopes TEXT NOT NULL DEFAULT 'admin',
            expires_at INTEGER,
            created_at INTEGER NOT NULL,
            last_used_at INTEGER,
            last_used_ip TEXT
        );
        "#,
    )
    .execute(&pool)
    .await?;
    ensure_column(
        &pool,
        "api_tokens",
        "scopes",
        "TEXT NOT NULL DEFAULT 'admin'",
    )
    .await?;
    ensure_column(&pool, "api_tokens", "expires_at", "INTEGER").await?;
    ensure_column(&pool, "api_tokens", "last_used_ip", "TEXT").await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cloud_sources (
            id TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            name TEXT NOT NULL,
            auth_type TEXT NOT NULL,
            policy TEXT NOT NULL DEFAULT 'auto',
            enabled INTEGER NOT NULL DEFAULT 1,
            secret_blob TEXT,
            config_json TEXT NOT NULL DEFAULT '{}',
            cursor TEXT,
            last_sync_at INTEGER,
            last_error TEXT,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cloud_roots (
            id TEXT PRIMARY KEY,
            source_id TEXT NOT NULL,
            remote_id TEXT NOT NULL,
            remote_path TEXT NOT NULL,
            display_path TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cloud_media (
            media_id TEXT PRIMARY KEY,
            source_id TEXT NOT NULL,
            remote_id TEXT NOT NULL,
            remote_path TEXT NOT NULL,
            size_bytes INTEGER,
            mime_type TEXT,
            etag TEXT,
            remote_modified_at INTEGER,
            cache_state TEXT NOT NULL DEFAULT 'none',
            cache_path TEXT,
            cached_bytes INTEGER,
            last_seen_at INTEGER,
            last_probe_at INTEGER,
            last_access_at INTEGER
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cloud_provider_config (
            provider TEXT PRIMARY KEY,
            client_id TEXT,
            secret_blob TEXT
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS precache_entries (
            media_id TEXT PRIMARY KEY,
            source_path TEXT NOT NULL,
            source_size INTEGER NOT NULL,
            source_mtime INTEGER NOT NULL,
            cache_path TEXT,
            mode TEXT NOT NULL,
            status TEXT NOT NULL,
            bytes INTEGER NOT NULL DEFAULT 0,
            error TEXT,
            start_ms INTEGER NOT NULL DEFAULT 0,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL,
            last_used_ms INTEGER NOT NULL DEFAULT 0,
            deadline_ms INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channels (
            id TEXT PRIMARY KEY,
            number TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            start_ms INTEGER NOT NULL,
            schedule_mode TEXT NOT NULL DEFAULT 'blocks',
            graphics_text TEXT NOT NULL DEFAULT '',
            graphics_position TEXT NOT NULL DEFAULT 'bottom-right',
            graphics_opacity INTEGER NOT NULL DEFAULT 60,
            graphics_size INTEGER NOT NULL DEFAULT 28,
            watermark_media_id TEXT,
            watermark_position TEXT NOT NULL DEFAULT 'top-right',
            watermark_opacity INTEGER NOT NULL DEFAULT 70,
            watermark_scale INTEGER NOT NULL DEFAULT 12,
            lower_third_text TEXT NOT NULL DEFAULT '',
            lower_third_size INTEGER NOT NULL DEFAULT 22,
            lower_third_opacity INTEGER NOT NULL DEFAULT 65,
            stream_width INTEGER NOT NULL DEFAULT 1280,
            stream_height INTEGER NOT NULL DEFAULT 720,
            stream_preset TEXT NOT NULL DEFAULT 'veryfast',
            stream_crf INTEGER NOT NULL DEFAULT 22,
            stream_max_kbps INTEGER NOT NULL DEFAULT 0,
            stream_audio_kbps INTEGER NOT NULL DEFAULT 160,
            audio_language TEXT,
            subtitle_mode TEXT NOT NULL DEFAULT 'inherit',
            subtitle_language TEXT,
            hdr_mode TEXT NOT NULL DEFAULT 'passthrough',
            encoder TEXT NOT NULL DEFAULT 'software',
            segment_seconds INTEGER NOT NULL DEFAULT 4,
            prebuffer_seconds INTEGER NOT NULL DEFAULT 8,
            cache_segments INTEGER NOT NULL DEFAULT 12,
            prewarm INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channel_items (
            channel_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            media_id TEXT NOT NULL,
            title TEXT NOT NULL,
            start_ms INTEGER NOT NULL,
            end_ms INTEGER NOT NULL,
            in_ms INTEGER NOT NULL DEFAULT 0,
            out_ms INTEGER,
            kind TEXT NOT NULL DEFAULT 'program',
            PRIMARY KEY (channel_id, position)
        );
        "#,
    )
    .execute(&pool)
    .await?;

    // Absence means unowned, including all legacy and manually created timelines.
    sqlx::query("CREATE TABLE IF NOT EXISTS generated_timeline_state (channel_id TEXT PRIMARY KEY, definition_version TEXT NOT NULL, boundary_ms INTEGER NOT NULL, continuation_json TEXT NOT NULL)")
        .execute(&pool).await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_channel_items_time ON channel_items(channel_id, start_ms, end_ms);",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS collections (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS collection_items (
            collection_id TEXT NOT NULL,
            media_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            PRIMARY KEY (collection_id, position)
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channel_blocks (
            id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            days_mask INTEGER NOT NULL,
            start_minute INTEGER NOT NULL,
            end_minute INTEGER NOT NULL,
            collection_id TEXT NOT NULL,
            filler_collection_id TEXT,
            play_order TEXT NOT NULL,
            position INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS schedule_templates (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            graphics_text TEXT NOT NULL DEFAULT '',
            graphics_position TEXT NOT NULL DEFAULT 'bottom-right',
            graphics_opacity INTEGER NOT NULL DEFAULT 60,
            graphics_size INTEGER NOT NULL DEFAULT 28,
            watermark_media_id TEXT,
            watermark_position TEXT NOT NULL DEFAULT 'top-right',
            watermark_opacity INTEGER NOT NULL DEFAULT 70,
            watermark_scale INTEGER NOT NULL DEFAULT 12,
            lower_third_text TEXT NOT NULL DEFAULT '',
            lower_third_size INTEGER NOT NULL DEFAULT 22,
            lower_third_opacity INTEGER NOT NULL DEFAULT 65,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS schedule_template_blocks (
            id TEXT PRIMARY KEY,
            template_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            days_mask INTEGER NOT NULL,
            start_minute INTEGER NOT NULL,
            end_minute INTEGER NOT NULL,
            collection_id TEXT NOT NULL,
            filler_collection_id TEXT,
            play_order TEXT NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS schedule_overrides (
            id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            name TEXT NOT NULL,
            start_date TEXT NOT NULL,
            end_date TEXT NOT NULL,
            priority INTEGER NOT NULL DEFAULT 0,
            schedule_yaml TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channel_classic_items (
            id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            collection_id TEXT NOT NULL,
            mode TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 1,
            duration_minutes INTEGER NOT NULL DEFAULT 60,
            break_markers TEXT NOT NULL DEFAULT '[]',
            play_order TEXT NOT NULL DEFAULT 'sequential'
        );
        "#,
    )
    .execute(&pool)
    .await?;

    // Add the kind column to databases created before Phase 2.
    let columns = sqlx::query("PRAGMA table_info(media_items)")
        .fetch_all(&pool)
        .await?;
    let has_kind = columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "kind");
    if !has_kind {
        sqlx::query("ALTER TABLE media_items ADD COLUMN kind TEXT NOT NULL DEFAULT 'movie'")
            .execute(&pool)
            .await?;
    }
    let channel_columns = sqlx::query("PRAGMA table_info(channels)")
        .fetch_all(&pool)
        .await?;
    let has_schedule_mode = channel_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "schedule_mode");
    if !has_schedule_mode {
        sqlx::query("ALTER TABLE channels ADD COLUMN schedule_mode TEXT NOT NULL DEFAULT 'blocks'")
            .execute(&pool)
            .await?;
    }
    for (column, statement) in [
        (
            "graphics_text",
            "ALTER TABLE channels ADD COLUMN graphics_text TEXT NOT NULL DEFAULT ''",
        ),
        (
            "graphics_position",
            "ALTER TABLE channels ADD COLUMN graphics_position TEXT NOT NULL DEFAULT 'bottom-right'",
        ),
        (
            "graphics_opacity",
            "ALTER TABLE channels ADD COLUMN graphics_opacity INTEGER NOT NULL DEFAULT 60",
        ),
        (
            "graphics_size",
            "ALTER TABLE channels ADD COLUMN graphics_size INTEGER NOT NULL DEFAULT 28",
        ),
        (
            "watermark_media_id",
            "ALTER TABLE channels ADD COLUMN watermark_media_id TEXT",
        ),
        (
            "watermark_position",
            "ALTER TABLE channels ADD COLUMN watermark_position TEXT NOT NULL DEFAULT 'top-right'",
        ),
        (
            "watermark_opacity",
            "ALTER TABLE channels ADD COLUMN watermark_opacity INTEGER NOT NULL DEFAULT 70",
        ),
        (
            "watermark_scale",
            "ALTER TABLE channels ADD COLUMN watermark_scale INTEGER NOT NULL DEFAULT 12",
        ),
        (
            "lower_third_text",
            "ALTER TABLE channels ADD COLUMN lower_third_text TEXT NOT NULL DEFAULT ''",
        ),
        (
            "lower_third_size",
            "ALTER TABLE channels ADD COLUMN lower_third_size INTEGER NOT NULL DEFAULT 22",
        ),
        (
            "lower_third_opacity",
            "ALTER TABLE channels ADD COLUMN lower_third_opacity INTEGER NOT NULL DEFAULT 65",
        ),
        (
            "stream_width",
            "ALTER TABLE channels ADD COLUMN stream_width INTEGER NOT NULL DEFAULT 1280",
        ),
        (
            "stream_height",
            "ALTER TABLE channels ADD COLUMN stream_height INTEGER NOT NULL DEFAULT 720",
        ),
        (
            "stream_preset",
            "ALTER TABLE channels ADD COLUMN stream_preset TEXT NOT NULL DEFAULT 'veryfast'",
        ),
        (
            "stream_crf",
            "ALTER TABLE channels ADD COLUMN stream_crf INTEGER NOT NULL DEFAULT 22",
        ),
        (
            "stream_max_kbps",
            "ALTER TABLE channels ADD COLUMN stream_max_kbps INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "stream_audio_kbps",
            "ALTER TABLE channels ADD COLUMN stream_audio_kbps INTEGER NOT NULL DEFAULT 160",
        ),
        (
            "audio_language",
            "ALTER TABLE channels ADD COLUMN audio_language TEXT",
        ),
        (
            "subtitle_mode",
            "ALTER TABLE channels ADD COLUMN subtitle_mode TEXT NOT NULL DEFAULT 'inherit'",
        ),
        (
            "subtitle_language",
            "ALTER TABLE channels ADD COLUMN subtitle_language TEXT",
        ),
        (
            "hdr_mode",
            "ALTER TABLE channels ADD COLUMN hdr_mode TEXT NOT NULL DEFAULT 'passthrough'",
        ),
        (
            "encoder",
            "ALTER TABLE channels ADD COLUMN encoder TEXT NOT NULL DEFAULT 'software'",
        ),
        (
            "segment_seconds",
            "ALTER TABLE channels ADD COLUMN segment_seconds INTEGER NOT NULL DEFAULT 4",
        ),
        (
            "prebuffer_seconds",
            "ALTER TABLE channels ADD COLUMN prebuffer_seconds INTEGER NOT NULL DEFAULT 8",
        ),
        (
            "cache_segments",
            "ALTER TABLE channels ADD COLUMN cache_segments INTEGER NOT NULL DEFAULT 12",
        ),
        (
            "prewarm",
            "ALTER TABLE channels ADD COLUMN prewarm INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        let exists = channel_columns
            .iter()
            .any(|row| row.get::<String, _>("name") == column);
        if !exists {
            sqlx::query(statement).execute(&pool).await?;
        }
    }
    let template_columns = sqlx::query("PRAGMA table_info(schedule_templates)")
        .fetch_all(&pool)
        .await?;
    for (column, statement) in [
        (
            "graphics_text",
            "ALTER TABLE schedule_templates ADD COLUMN graphics_text TEXT NOT NULL DEFAULT ''",
        ),
        (
            "graphics_position",
            "ALTER TABLE schedule_templates ADD COLUMN graphics_position TEXT NOT NULL DEFAULT 'bottom-right'",
        ),
        (
            "graphics_opacity",
            "ALTER TABLE schedule_templates ADD COLUMN graphics_opacity INTEGER NOT NULL DEFAULT 60",
        ),
        (
            "graphics_size",
            "ALTER TABLE schedule_templates ADD COLUMN graphics_size INTEGER NOT NULL DEFAULT 28",
        ),
        (
            "watermark_media_id",
            "ALTER TABLE schedule_templates ADD COLUMN watermark_media_id TEXT",
        ),
        (
            "watermark_position",
            "ALTER TABLE schedule_templates ADD COLUMN watermark_position TEXT NOT NULL DEFAULT 'top-right'",
        ),
        (
            "watermark_opacity",
            "ALTER TABLE schedule_templates ADD COLUMN watermark_opacity INTEGER NOT NULL DEFAULT 70",
        ),
        (
            "watermark_scale",
            "ALTER TABLE schedule_templates ADD COLUMN watermark_scale INTEGER NOT NULL DEFAULT 12",
        ),
        (
            "lower_third_text",
            "ALTER TABLE schedule_templates ADD COLUMN lower_third_text TEXT NOT NULL DEFAULT ''",
        ),
        (
            "lower_third_size",
            "ALTER TABLE schedule_templates ADD COLUMN lower_third_size INTEGER NOT NULL DEFAULT 22",
        ),
        (
            "lower_third_opacity",
            "ALTER TABLE schedule_templates ADD COLUMN lower_third_opacity INTEGER NOT NULL DEFAULT 65",
        ),
    ] {
        let exists = template_columns
            .iter()
            .any(|row| row.get::<String, _>("name") == column);
        if !exists {
            sqlx::query(statement).execute(&pool).await?;
        }
    }
    let precache_columns = sqlx::query("PRAGMA table_info(precache_entries)")
        .fetch_all(&pool)
        .await?;
    let has_deadline = precache_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "deadline_ms");
    if !has_deadline {
        sqlx::query(
            "ALTER TABLE precache_entries ADD COLUMN deadline_ms INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await?;
    }
    if !precache_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "identity_known")
    {
        sqlx::query(
            "ALTER TABLE precache_entries ADD COLUMN identity_known INTEGER NOT NULL DEFAULT 1",
        )
        .execute(&pool)
        .await?;
        // Rows produced by the previous planner carry these explicit diagnostics.
        sqlx::query("UPDATE precache_entries SET identity_known = 0 WHERE status = 'queued' AND error IN ('source mtime unavailable', 'source size and mtime unavailable')").execute(&pool).await?;
    }
    if !precache_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "retry_count")
    {
        sqlx::query(
            "ALTER TABLE precache_entries ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await?;
    }
    if !precache_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "generation")
    {
        sqlx::query("ALTER TABLE precache_entries ADD COLUMN generation TEXT NOT NULL DEFAULT ''")
            .execute(&pool)
            .await?;
    }
    if !precache_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "last_attempt_order")
    {
        sqlx::query(
            "ALTER TABLE precache_entries ADD COLUMN last_attempt_order INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await?;
    }
    sqlx::query(
        "UPDATE precache_entries SET generation = lower(hex(randomblob(16))) WHERE generation = ''",
    )
    .execute(&pool)
    .await?;
    // Existing databases retain their original column default; repair any legacy writer atomically.
    sqlx::query("CREATE TRIGGER IF NOT EXISTS precache_generation_insert AFTER INSERT ON precache_entries WHEN NEW.generation = '' BEGIN UPDATE precache_entries SET generation=lower(hex(randomblob(16))) WHERE media_id=NEW.media_id AND generation=''; END").execute(&pool).await?;
    sqlx::query("CREATE TRIGGER IF NOT EXISTS precache_generation_update AFTER UPDATE OF generation ON precache_entries WHEN NEW.generation = '' BEGIN UPDATE precache_entries SET generation=lower(hex(randomblob(16))) WHERE media_id=NEW.media_id AND generation=''; END").execute(&pool).await?;
    let media_columns = sqlx::query("PRAGMA table_info(media_items)")
        .fetch_all(&pool)
        .await?;
    let has_chapters_probed = media_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "chapters_probed");
    if !has_chapters_probed {
        sqlx::query(
            "ALTER TABLE media_items ADD COLUMN chapters_probed INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await?;
    }
    let has_size_bytes = media_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "size_bytes");
    if !has_size_bytes {
        sqlx::query("ALTER TABLE media_items ADD COLUMN size_bytes INTEGER")
            .execute(&pool)
            .await?;
    }
    let block_columns = sqlx::query("PRAGMA table_info(channel_blocks)")
        .fetch_all(&pool)
        .await?;
    let has_block_filler = block_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "filler_collection_id");
    if !has_block_filler {
        sqlx::query("ALTER TABLE channel_blocks ADD COLUMN filler_collection_id TEXT")
            .execute(&pool)
            .await?;
    }
    let item_columns = sqlx::query("PRAGMA table_info(channel_items)")
        .fetch_all(&pool)
        .await?;
    let has_in_point = item_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "in_ms");
    if !has_in_point {
        sqlx::query("ALTER TABLE channel_items ADD COLUMN in_ms INTEGER NOT NULL DEFAULT 0")
            .execute(&pool)
            .await?;
        sqlx::query("ALTER TABLE channel_items ADD COLUMN out_ms INTEGER")
            .execute(&pool)
            .await?;
        sqlx::query("ALTER TABLE channel_items ADD COLUMN kind TEXT NOT NULL DEFAULT 'program'")
            .execute(&pool)
            .await?;
    }
    let classic_columns = sqlx::query("PRAGMA table_info(channel_classic_items)")
        .fetch_all(&pool)
        .await?;
    let has_classic_breaks = classic_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "filler_collection_id");
    if !has_classic_breaks {
        sqlx::query("ALTER TABLE channel_classic_items ADD COLUMN filler_collection_id TEXT")
            .execute(&pool)
            .await?;
        sqlx::query(
            "ALTER TABLE channel_classic_items ADD COLUMN mid_roll_interval_minutes INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "ALTER TABLE channel_classic_items ADD COLUMN mid_roll_duration_seconds INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "ALTER TABLE channel_classic_items ADD COLUMN chapter_breaks INTEGER NOT NULL DEFAULT 1",
        )
        .execute(&pool)
        .await?;
    }
    let classic_columns = sqlx::query("PRAGMA table_info(channel_classic_items)")
        .fetch_all(&pool)
        .await?;
    let has_chapter_breaks = classic_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "chapter_breaks");
    if !has_chapter_breaks {
        sqlx::query(
            "ALTER TABLE channel_classic_items ADD COLUMN chapter_breaks INTEGER NOT NULL DEFAULT 1",
        )
        .execute(&pool)
        .await?;
    }
    let classic_columns = sqlx::query("PRAGMA table_info(channel_classic_items)")
        .fetch_all(&pool)
        .await?;
    let has_break_markers = classic_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "break_markers");
    if !has_break_markers {
        sqlx::query(
            "ALTER TABLE channel_classic_items ADD COLUMN break_markers TEXT NOT NULL DEFAULT '[]'",
        )
        .execute(&pool)
        .await?;
    }

    // A monotonic revision catches edits (including change-and-revert) while a
    // renewal reads its inputs across multiple connections. SQLite triggers
    // participate in the writer's transaction and also cover direct SQL edits.
    sqlx::query("CREATE TABLE IF NOT EXISTS renewal_input_revision (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL)").execute(&pool).await?;
    sqlx::query("INSERT OR IGNORE INTO renewal_input_revision VALUES (1, 0)")
        .execute(&pool)
        .await?;
    for table in [
        "channels",
        "channel_classic_items",
        "channel_blocks",
        "schedule_overrides",
        "collections",
        "collection_items",
        "media_items",
        "media_chapters",
    ] {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let trigger = format!(
                "CREATE TRIGGER IF NOT EXISTS renewal_revision_{table}_{operation} AFTER {operation} ON {table} BEGIN UPDATE renewal_input_revision SET revision = revision + 1 WHERE id = 1; END"
            );
            sqlx::query(&trigger).execute(&pool).await?;
        }
    }

    sqlx::query("CREATE TABLE IF NOT EXISTS rebuild_timeline_revision (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL)").execute(&pool).await?;
    sqlx::query("INSERT OR IGNORE INTO rebuild_timeline_revision VALUES (1, 0)")
        .execute(&pool)
        .await?;
    for table in ["channel_items", "generated_timeline_state"] {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let trigger = format!(
                "CREATE TRIGGER IF NOT EXISTS rebuild_revision_{table}_{operation} AFTER {operation} ON {table} BEGIN UPDATE rebuild_timeline_revision SET revision = revision + 1 WHERE id = 1; END"
            );
            sqlx::query(&trigger).execute(&pool).await?;
        }
    }

    Ok(pool)
}

pub async fn upsert_media(pool: &SqlitePool, media: &Media) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO media_items (id, path, title, kind, duration_ms, width, height, video_codec, audio_codec, size_bytes, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            kind = excluded.kind,
            duration_ms = excluded.duration_ms,
            width = excluded.width,
            height = excluded.height,
            video_codec = excluded.video_codec,
            audio_codec = excluded.audio_codec,
            size_bytes = excluded.size_bytes
        "#,
    )
    .bind(&media.id)
    .bind(&media.path)
    .bind(&media.title)
    .bind(&media.kind)
    .bind(media.duration_ms)
    .bind(media.width)
    .bind(media.height)
    .bind(&media.video_codec)
    .bind(&media.audio_codec)
    .bind(media.size_bytes)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

// Scan ownership is scoped by canonical root and kind; media rows are never deleted.
pub async fn reconcile_local_scan(
    pool: &SqlitePool,
    root: &str,
    kind: &str,
    media: &[Media],
    complete: bool,
) -> Result<Vec<String>> {
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS local_scan_inventory (root TEXT NOT NULL, kind TEXT NOT NULL, path TEXT NOT NULL, missing INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(root, kind, path))").execute(&mut *tx).await?;
    if complete {
        sqlx::query("UPDATE local_scan_inventory SET missing = 1 WHERE root = ? AND kind = ?")
            .bind(root)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
    }
    for item in media {
        sqlx::query("INSERT INTO local_scan_inventory(root, kind, path, missing) VALUES (?, ?, ?, 0) ON CONFLICT(root, kind, path) DO UPDATE SET missing = 0")
            .bind(root).bind(kind).bind(&item.path).execute(&mut *tx).await?;
    }
    let missing: Vec<String> = sqlx::query_scalar("SELECT path FROM local_scan_inventory WHERE root = ? AND kind = ? AND missing = 1 ORDER BY path")
        .bind(root).bind(kind).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(missing)
}

pub async fn list_media(pool: &SqlitePool) -> Result<Vec<Media>> {
    let rows = sqlx::query(
        "SELECT id, path, title, kind, duration_ms, width, height, video_codec, audio_codec, size_bytes FROM media_items ORDER BY kind, title COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Media {
            id: row.get("id"),
            path: row.get("path"),
            title: row.get("title"),
            kind: row.get("kind"),
            duration_ms: row.get("duration_ms"),
            width: row.get("width"),
            height: row.get("height"),
            video_codec: row.get("video_codec"),
            audio_codec: row.get("audio_codec"),
            size_bytes: row.get("size_bytes"),
            chapters: Vec::new(),
        })
        .collect())
}

pub(crate) async fn publish_auto_channels(
    pool: &SqlitePool,
    entries: &[(
        String,
        String,
        ClassicItemInput,
        crate::programming::StagedClassic,
    )],
    revision: (i64, i64),
) -> Result<Vec<Channel>> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE renewal_input_revision SET revision = revision WHERE id = 1")
        .execute(&mut *tx)
        .await?;
    let actual: (i64, i64) = sqlx::query_as("SELECT (SELECT revision FROM renewal_input_revision WHERE id = 1), revision FROM rebuild_timeline_revision WHERE id = 1").fetch_one(&mut *tx).await?;
    if actual != revision {
        bail!("auto-build inputs changed; retry the request");
    }
    let existing = sqlx::query("SELECT number, name FROM channels")
        .fetch_all(&mut *tx)
        .await?;
    for (number, name, _, _) in entries {
        if existing
            .iter()
            .any(|r| r.get::<String, _>("number").parse::<i64>().ok() == number.parse::<i64>().ok())
        {
            bail!("channel number '{number}' is already in use; retry the request");
        }
        if existing
            .iter()
            .any(|r| r.get::<String, _>("name").eq_ignore_ascii_case(name))
        {
            bail!("channel name '{name}' is already in use; retry the request");
        }
    }
    let mut ids = Vec::new();
    for (number, name, input, staged) in entries {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO channels (id, number, name, start_ms, created_at, schedule_mode, subtitle_mode) VALUES (?, ?, ?, ?, ?, 'classic', 'inherit')")
            .bind(&id).bind(number).bind(name).bind(staged.start_ms).bind(now_ms()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO channel_classic_items (id, channel_id, position, collection_id, mode, count, duration_minutes, filler_collection_id, mid_roll_interval_minutes, mid_roll_duration_seconds, chapter_breaks, break_markers, play_order) VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(Uuid::new_v4().to_string()).bind(&id).bind(&input.collection_id).bind(&input.mode).bind(input.count).bind(input.duration_minutes).bind(&input.filler_collection_id).bind(input.mid_roll_interval_minutes).bind(input.mid_roll_duration_seconds).bind(input.chapter_breaks).bind(serde_json::to_string(&input.break_markers)?).bind(&input.order).execute(&mut *tx).await?;
        for (position, item) in staged.items.iter().enumerate() {
            sqlx::query("INSERT INTO channel_items (channel_id, position, media_id, title, start_ms, end_ms, in_ms, out_ms, kind) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(&id).bind(position as i64).bind(&item.media_id).bind(&item.title).bind(item.start_ms).bind(item.end_ms).bind(item.in_ms).bind(item.out_ms).bind(&item.kind).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO generated_timeline_state (channel_id, definition_version, boundary_ms, continuation_json) VALUES (?, ?, ?, ?)")
            .bind(&id).bind(&staged.state.definition_version).bind(staged.state.boundary_ms).bind(&staged.state.continuation_json).execute(&mut *tx).await?;
        ids.push(id);
    }
    let mut channels = Vec::new();
    for id in ids {
        channels.push(
            channel_by_id_on(&mut tx, &id)
                .await?
                .context("channel disappeared before creation")?,
        );
    }
    tx.commit().await?;
    Ok(channels)
}

#[cfg(test)]
pub async fn create_channel_shell(pool: &SqlitePool, number: &str, name: &str) -> Result<Channel> {
    let id = Uuid::new_v4().to_string();
    let start_ms = now_ms();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO channels (id, number, name, start_ms, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(number)
    .bind(name)
    .bind(start_ms)
    .bind(now_ms())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE channels SET subtitle_mode = 'inherit' WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    channel_by_id(pool, &id)
        .await?
        .context("channel disappeared after creation")
}

pub async fn create_channel(
    pool: &SqlitePool,
    number: &str,
    name: &str,
    media_ids: &[String],
) -> Result<Channel> {
    if media_ids.is_empty() {
        bail!("select at least one media item");
    }
    let all = list_media(pool).await?;
    let selected: Vec<Media> = media_ids
        .iter()
        .filter_map(|id| all.iter().find(|media| &media.id == id).cloned())
        .collect();
    if selected.is_empty() {
        bail!("none of the selected media ids exist");
    }
    let cycle_ms: i64 = selected.iter().map(|m| m.duration_ms).sum();
    if cycle_ms <= 0 {
        bail!("selected media has no duration");
    }

    let id = Uuid::new_v4().to_string();
    let start_ms = now_ms();
    let target_ms = start_ms + WEEK_MS;
    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO channels (id, number, name, start_ms, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(number)
    .bind(name)
    .bind(start_ms)
    .bind(now_ms())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE channels SET subtitle_mode = 'inherit' WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;

    let mut position: i64 = 0;
    let mut cursor = start_ms;
    while cursor < target_ms {
        for media in &selected {
            if cursor >= target_ms {
                break;
            }
            let end = cursor + media.duration_ms;
            sqlx::query(
                "INSERT INTO channel_items (channel_id, position, media_id, title, start_ms, end_ms) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(position)
            .bind(&media.id)
            .bind(&media.title)
            .bind(cursor)
            .bind(end)
            .execute(&mut *tx)
            .await?;
            position += 1;
            cursor = end;
        }
    }

    tx.commit().await?;
    Ok(Channel {
        id,
        number: number.to_string(),
        name: name.to_string(),
        start_ms,
        schedule_mode: "blocks".into(),
        graphics: GraphicsSettings::default(),
        stream: StreamSettings::default(),
    })
}

pub async fn list_channels(pool: &SqlitePool) -> Result<Vec<Channel>> {
    let mut conn = pool.acquire().await?;
    list_channels_on(&mut conn).await
}

async fn list_channels_on(conn: &mut sqlx::SqliteConnection) -> Result<Vec<Channel>> {
    let rows = sqlx::query(
        "SELECT id, number, name, start_ms, schedule_mode, graphics_text, graphics_position, graphics_opacity, graphics_size, watermark_media_id, watermark_position, watermark_opacity, watermark_scale, lower_third_text, lower_third_size, lower_third_opacity, stream_width, stream_height, stream_preset, stream_crf, stream_max_kbps, stream_audio_kbps, audio_language, subtitle_mode, subtitle_language, hdr_mode, encoder, segment_seconds, prebuffer_seconds, cache_segments, prewarm FROM channels ORDER BY CAST(number AS INTEGER), number",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Channel {
            id: row.get("id"),
            number: row.get("number"),
            name: row.get("name"),
            start_ms: row.get("start_ms"),
            schedule_mode: row.get("schedule_mode"),
            graphics: graphics_from_row(&row),
            stream: stream_from_row(&row),
        })
        .collect())
}

pub async fn channel_by_number(pool: &SqlitePool, number: &str) -> Result<Option<Channel>> {
    let row = sqlx::query(
        "SELECT id, number, name, start_ms, schedule_mode, graphics_text, graphics_position, graphics_opacity, graphics_size, watermark_media_id, watermark_position, watermark_opacity, watermark_scale, lower_third_text, lower_third_size, lower_third_opacity, stream_width, stream_height, stream_preset, stream_crf, stream_max_kbps, stream_audio_kbps, audio_language, subtitle_mode, subtitle_language, hdr_mode, encoder, segment_seconds, prebuffer_seconds, cache_segments, prewarm FROM channels WHERE number = ? LIMIT 1",
    )
    .bind(number)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| Channel {
        id: row.get("id"),
        number: row.get("number"),
        name: row.get("name"),
        start_ms: row.get("start_ms"),
        schedule_mode: row.get("schedule_mode"),
        graphics: graphics_from_row(&row),
        stream: stream_from_row(&row),
    }))
}

pub async fn current_item(
    pool: &SqlitePool,
    channel_id: &str,
    at_ms: i64,
) -> Result<Option<ChannelItem>> {
    let row = sqlx::query(
        "SELECT ci.media_id, COALESCE(m.title, ci.title) AS title, ci.start_ms, ci.end_ms, ci.in_ms, ci.out_ms, ci.kind FROM channel_items ci LEFT JOIN media_items m ON m.id = ci.media_id WHERE ci.channel_id = ? AND ci.start_ms <= ? AND ci.end_ms > ? ORDER BY ci.start_ms LIMIT 1",
    )
    .bind(channel_id)
    .bind(at_ms)
    .bind(at_ms)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| ChannelItem {
        media_id: row.get("media_id"),
        title: row.get("title"),
        start_ms: row.get("start_ms"),
        end_ms: row.get("end_ms"),
        in_ms: row.get("in_ms"),
        out_ms: row.get("out_ms"),
        kind: row.get("kind"),
    }))
}

pub async fn next_item_start(
    pool: &SqlitePool,
    channel_id: &str,
    after_ms: i64,
) -> Result<Option<i64>> {
    let row = sqlx::query(
        "SELECT MIN(start_ms) AS next_start FROM channel_items WHERE channel_id = ? AND start_ms > ?",
    )
    .bind(channel_id)
    .bind(after_ms)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<Option<i64>, _>("next_start"))
}

pub async fn guide_items(
    pool: &SqlitePool,
    channel_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<ChannelItem>> {
    let rows = sqlx::query(
        "SELECT ci.media_id, COALESCE(m.title, ci.title) AS title, ci.start_ms, ci.end_ms, ci.in_ms, ci.out_ms, ci.kind FROM channel_items ci LEFT JOIN media_items m ON m.id = ci.media_id WHERE ci.channel_id = ? AND ci.end_ms > ? AND ci.start_ms < ? ORDER BY ci.start_ms",
    )
    .bind(channel_id)
    .bind(from_ms)
    .bind(to_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ChannelItem {
            media_id: row.get("media_id"),
            title: row.get("title"),
            start_ms: row.get("start_ms"),
            end_ms: row.get("end_ms"),
            in_ms: row.get("in_ms"),
            out_ms: row.get("out_ms"),
            kind: row.get("kind"),
        })
        .collect())
}

/// Guide/EPG view: merge program segments that are split only by short
/// mid-roll breaks, so the displayed runtime matches the media runtime while
/// the playout timeline still contains the breaks.
pub async fn guide_blocks(
    pool: &SqlitePool,
    channel_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<ChannelItem>> {
    let items = guide_items(pool, channel_id, from_ms, to_ms).await?;
    Ok(merge_guide_blocks(items))
}

fn merge_guide_blocks(items: Vec<ChannelItem>) -> Vec<ChannelItem> {
    const MAX_BREAK_GAP_MS: i64 = 5 * 60_000;
    let mut merged: Vec<ChannelItem> = Vec::new();
    for item in items {
        if item.kind == "program"
            && let Some(position) = merged.iter().rposition(|entry| entry.kind == "program")
        {
            let can_merge = {
                let previous = &merged[position];
                previous.media_id == item.media_id
                    && previous.out_ms == Some(item.in_ms)
                    && item.in_ms > previous.in_ms
                    && (0..=MAX_BREAK_GAP_MS)
                        .contains(&item.start_ms.saturating_sub(previous.end_ms))
            };
            if can_merge {
                // Drop the short break items between the two program segments.
                merged.truncate(position + 1);
                let previous = &mut merged[position];
                previous.end_ms = item.end_ms;
                previous.out_ms = item.out_ms;
                continue;
            }
        }
        merged.push(item);
    }
    merged
}

pub async fn replace_chapters(
    pool: &SqlitePool,
    media_id: &str,
    chapters: &[Chapter],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM media_chapters WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    for (position, chapter) in chapters.iter().enumerate() {
        sqlx::query(
            "INSERT INTO media_chapters (media_id, position, start_ms, end_ms, title) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(media_id)
        .bind(position as i64)
        .bind(chapter.start_ms)
        .bind(chapter.end_ms)
        .bind(&chapter.title)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE media_items SET chapters_probed = 1 WHERE id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn chapter_starts(pool: &SqlitePool) -> Result<HashMap<String, Vec<i64>>> {
    let rows = sqlx::query(
        "SELECT media_id, start_ms FROM media_chapters WHERE start_ms > 0 ORDER BY media_id, position",
    )
    .fetch_all(pool)
    .await?;
    let mut chapters: HashMap<String, Vec<i64>> = HashMap::new();
    for row in rows {
        chapters
            .entry(row.get("media_id"))
            .or_default()
            .push(row.get("start_ms"));
    }
    Ok(chapters)
}

pub async fn media_missing_chapters(
    pool: &SqlitePool,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT id, path FROM media_items WHERE kind IN ('episode', 'movie') AND chapters_probed = 0 ORDER BY title COLLATE NOCASE LIMIT ?",
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("id"), row.get("path")))
        .collect())
}

pub async fn count_missing_chapters(pool: &SqlitePool) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS count FROM media_items WHERE kind IN ('episode', 'movie') AND chapters_probed = 0",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.get("count"))
}

pub async fn collection_media_paths(
    pool: &SqlitePool,
    collection_id: &str,
) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT m.id, m.path FROM collection_items ci JOIN media_items m ON m.id = ci.media_id WHERE ci.collection_id = ? AND m.path NOT LIKE '%.iso' ORDER BY ci.position",
    )
    .bind(collection_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("id"), row.get("path")))
        .collect())
}

fn media_from_row(row: sqlx::sqlite::SqliteRow) -> Media {
    Media {
        id: row.get("id"),
        path: row.get("path"),
        title: row.get("title"),
        kind: row.get("kind"),
        duration_ms: row.get("duration_ms"),
        width: row.get("width"),
        height: row.get("height"),
        video_codec: row.get("video_codec"),
        audio_codec: row.get("audio_codec"),
        size_bytes: row.get("size_bytes"),
        chapters: Vec::new(),
    }
}

pub async fn media_by_reference(pool: &SqlitePool, reference: &str) -> Result<Option<Media>> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Ok(None);
    }
    let columns =
        "id, path, title, kind, duration_ms, width, height, video_codec, audio_codec, size_bytes";
    let by_id = sqlx::query(&format!(
        "SELECT {columns} FROM media_items WHERE id = ? LIMIT 1"
    ))
    .bind(reference)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = by_id {
        return Ok(Some(media_from_row(row)));
    }
    let rows = sqlx::query(&format!(
        "SELECT {columns} FROM media_items WHERE title = ? COLLATE NOCASE LIMIT 2"
    ))
    .bind(reference)
    .fetch_all(pool)
    .await?;
    if rows.len() > 1 {
        bail!("media title is ambiguous: {reference}");
    }
    Ok(rows.into_iter().next().map(media_from_row))
}

pub async fn media_kind(pool: &SqlitePool, media_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT kind FROM media_items WHERE id = ? LIMIT 1")
        .bind(media_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| row.get("kind")))
}

#[derive(Debug, Clone)]
pub struct TimelineWrite {
    pub media_id: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub in_ms: i64,
    pub out_ms: i64,
    pub kind: String,
}

/// An opaque definition/epoch version and serialized generator state. A null cursor
/// records that rebuild does not yet supply resumable mode-specific cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedTimelineState {
    pub definition_version: String,
    pub boundary_ms: i64,
    pub continuation_json: String,
}

pub(crate) async fn generated_timeline_state(
    pool: &SqlitePool,
    channel_id: &str,
) -> Result<Option<GeneratedTimelineState>> {
    let row = sqlx::query("SELECT definition_version, boundary_ms, continuation_json FROM generated_timeline_state WHERE channel_id = ?")
        .bind(channel_id).fetch_optional(pool).await?;
    Ok(row.map(|r| GeneratedTimelineState {
        definition_version: r.get(0),
        boundary_ms: r.get(1),
        continuation_json: r.get(2),
    }))
}

pub(crate) async fn renewal_input_revision(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT revision FROM renewal_input_revision WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

pub(crate) async fn rebuild_revision(pool: &SqlitePool) -> Result<(i64, i64)> {
    let row = sqlx::query_as("SELECT (SELECT revision FROM renewal_input_revision WHERE id = 1), revision FROM rebuild_timeline_revision WHERE id = 1").fetch_one(pool).await?;
    Ok(row)
}

/// Compare under the SQLite write lock. Stale requests (including retries) fail
/// without writing; the caller can reload state to distinguish a prior success.
#[cfg(test)]
pub(crate) async fn compare_append_generated_timeline(
    pool: &SqlitePool,
    channel_id: &str,
    expected: &GeneratedTimelineState,
    items: &[TimelineWrite],
    next: &GeneratedTimelineState,
) -> Result<()> {
    compare_append_generated_timeline_guarded(pool, channel_id, expected, items, next, None, None)
        .await
}

#[cfg(test)]
pub(crate) async fn compare_append_generated_timeline_with_overrides(
    pool: &SqlitePool,
    channel_id: &str,
    expected: &GeneratedTimelineState,
    items: &[TimelineWrite],
    next: &GeneratedTimelineState,
    overrides: Option<&[ScheduleOverride]>,
) -> Result<()> {
    compare_append_generated_timeline_guarded(
        pool, channel_id, expected, items, next, overrides, None,
    )
    .await
}

pub(crate) async fn compare_append_generated_timeline_guarded(
    pool: &SqlitePool,
    channel_id: &str,
    expected: &GeneratedTimelineState,
    items: &[TimelineWrite],
    next: &GeneratedTimelineState,
    overrides: Option<&[ScheduleOverride]>,
    revision: Option<i64>,
) -> Result<()> {
    serde_json::from_str::<serde_json::Value>(&next.continuation_json)?;
    if items.is_empty()
        || next.definition_version.is_empty()
        || next.boundary_ms <= expected.boundary_ms
    {
        bail!("invalid generated append");
    }
    let mut cursor = expected.boundary_ms;
    for item in items {
        if item.start_ms != cursor || item.end_ms <= item.start_ms {
            bail!("generated append is not contiguous");
        }
        cursor = item.end_ms;
    }
    if cursor != next.boundary_ms {
        bail!("generated append boundary mismatch");
    }
    let mut tx = pool.begin().await?;
    // First statement acquires a write lock before any read, avoiding a deferred
    // transaction's read-to-write upgrade race between competing renewals.
    sqlx::query(
        "UPDATE generated_timeline_state SET boundary_ms = boundary_ms WHERE channel_id = ?",
    )
    .bind(channel_id)
    .execute(&mut *tx)
    .await?;
    if let Some(expected_revision) = revision {
        let actual: i64 =
            sqlx::query_scalar("SELECT revision FROM renewal_input_revision WHERE id = 1")
                .fetch_one(&mut *tx)
                .await?;
        if actual != expected_revision {
            bail!("renewal inputs changed during generation");
        }
    }
    let row = sqlx::query("SELECT definition_version, boundary_ms, continuation_json FROM generated_timeline_state WHERE channel_id = ?")
        .bind(channel_id).fetch_optional(&mut *tx).await?.context("timeline is not generated-owned")?;
    if row.get::<String, _>(0) != expected.definition_version
        || row.get::<i64, _>(1) != expected.boundary_ms
        || row.get::<String, _>(2) != expected.continuation_json
    {
        bail!("generated timeline state changed");
    }
    if let Some(overrides) = overrides {
        let rows = sqlx::query("SELECT id, name, start_date, end_date, priority, schedule_yaml FROM schedule_overrides WHERE channel_id = ? ORDER BY id")
            .bind(channel_id).fetch_all(&mut *tx).await?;
        let actual: Vec<(String, String, String, String, i64, String)> = rows
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
            .collect();
        let mut expected: Vec<_> = overrides
            .iter()
            .map(|r| {
                (
                    r.id.clone(),
                    r.name.clone(),
                    r.start_date.clone(),
                    r.end_date.clone(),
                    r.priority,
                    r.schedule_yaml.clone(),
                )
            })
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        if actual != expected {
            bail!("override definitions changed during renewal");
        }
    }
    let rows = sqlx::query("SELECT position, start_ms, end_ms FROM channel_items WHERE channel_id = ? ORDER BY start_ms, position")
        .bind(channel_id).fetch_all(&mut *tx).await?;
    if rows.is_empty() {
        bail!("generated timeline is empty");
    }
    let mut last = None;
    let mut max_position = -1i64;
    for row in rows {
        let (position, start, end): (i64, i64, i64) = (row.get(0), row.get(1), row.get(2));
        if end <= start || last.is_some_and(|v| v != start) {
            bail!("published timeline has gaps or overlaps");
        }
        last = Some(end);
        max_position = max_position.max(position);
    }
    if last != Some(expected.boundary_ms) {
        bail!("published boundary changed");
    }
    for (index, item) in items.iter().enumerate() {
        let position = max_position
            .checked_add(index as i64 + 1)
            .context("timeline position overflow")?;
        sqlx::query("INSERT INTO channel_items (channel_id, position, media_id, title, start_ms, end_ms, in_ms, out_ms, kind) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(channel_id).bind(position).bind(&item.media_id).bind(&item.title).bind(item.start_ms).bind(item.end_ms).bind(item.in_ms).bind(item.out_ms).bind(&item.kind).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE generated_timeline_state SET definition_version = ?, boundary_ms = ?, continuation_json = ? WHERE channel_id = ?")
        .bind(&next.definition_version).bind(next.boundary_ms).bind(&next.continuation_json).bind(channel_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn replace_channel_timeline(
    pool: &SqlitePool,
    channel_id: &str,
    items: &[TimelineWrite],
    start_ms: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM generated_timeline_state WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM channel_items WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    for (position, item) in items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO channel_items (channel_id, position, media_id, title, start_ms, end_ms, in_ms, out_ms, kind) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(channel_id)
        .bind(position as i64)
        .bind(&item.media_id)
        .bind(&item.title)
        .bind(item.start_ms)
        .bind(item.end_ms)
        .bind(item.in_ms)
        .bind(item.out_ms)
        .bind(&item.kind)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE channels SET start_ms = ? WHERE id = ?")
        .bind(start_ms)
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn channel_items_from(
    pool: &SqlitePool,
    channel_id: &str,
    from_ms: i64,
) -> Result<Vec<ChannelItem>> {
    let rows = sqlx::query(
        "SELECT ci.media_id, COALESCE(m.title, ci.title) AS title, ci.start_ms, ci.end_ms, ci.in_ms, ci.out_ms, ci.kind FROM channel_items ci LEFT JOIN media_items m ON m.id = ci.media_id WHERE ci.channel_id = ? AND ci.start_ms >= ? ORDER BY ci.start_ms",
    )
    .bind(channel_id)
    .bind(from_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ChannelItem {
            media_id: row.get("media_id"),
            title: row.get("title"),
            start_ms: row.get("start_ms"),
            end_ms: row.get("end_ms"),
            in_ms: row.get("in_ms"),
            out_ms: row.get("out_ms"),
            kind: row.get("kind"),
        })
        .collect())
}

pub async fn all_channel_items(pool: &SqlitePool, channel_id: &str) -> Result<Vec<ChannelItem>> {
    let rows = sqlx::query(
        "SELECT ci.media_id, COALESCE(m.title, ci.title) AS title, ci.start_ms, ci.end_ms, ci.in_ms, ci.out_ms, ci.kind FROM channel_items ci LEFT JOIN media_items m ON m.id = ci.media_id WHERE ci.channel_id = ? ORDER BY ci.start_ms",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ChannelItem {
            media_id: row.get("media_id"),
            title: row.get("title"),
            start_ms: row.get("start_ms"),
            end_ms: row.get("end_ms"),
            in_ms: row.get("in_ms"),
            out_ms: row.get("out_ms"),
            kind: row.get("kind"),
        })
        .collect())
}

pub async fn count_channel_items(pool: &SqlitePool, channel_id: &str) -> Result<usize> {
    let row = sqlx::query("SELECT COUNT(*) AS count FROM channel_items WHERE channel_id = ?")
        .bind(channel_id)
        .fetch_one(pool)
        .await?;
    Ok(row.get::<i64, _>("count") as usize)
}

#[derive(Debug, Clone, Serialize)]
pub struct PrecacheEntry {
    pub media_id: String,
    pub title: String,
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime: i64,
    pub identity_known: bool,
    pub generation: String,
    pub cache_path: Option<String>,
    pub mode: String,
    pub status: String,
    pub bytes: i64,
    pub error: Option<String>,
    pub start_ms: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub last_used_ms: i64,
    pub deadline_ms: i64,
}

#[derive(Debug, Clone)]
pub struct UpcomingMedia {
    pub channel_id: String,
    pub end_ms: i64,
    pub media_id: String,
    pub path: String,
    pub duration_ms: i64,
    pub size_bytes: i64,
    pub start_ms: i64,
}

fn precache_row(row: &sqlx::sqlite::SqliteRow) -> PrecacheEntry {
    use sqlx::Row;
    PrecacheEntry {
        media_id: row.get("media_id"),
        title: row.try_get("title").unwrap_or_default(),
        source_path: row.get("source_path"),
        source_size: row.get("source_size"),
        source_mtime: row.get("source_mtime"),
        identity_known: row.get::<i64, _>("identity_known") != 0,
        generation: row.get("generation"),
        cache_path: row.get("cache_path"),
        mode: row.get("mode"),
        status: row.get("status"),
        bytes: row.get("bytes"),
        error: row.get("error"),
        start_ms: row.get("start_ms"),
        created_ms: row.get("created_ms"),
        updated_ms: row.get("updated_ms"),
        last_used_ms: row.get("last_used_ms"),
        deadline_ms: row.get("deadline_ms"),
    }
}

/// Distinct media that will air in the window, with its earliest start time.
pub async fn upcoming_media(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<UpcomingMedia>> {
    let rows = sqlx::query(
        "SELECT ci.channel_id, ci.end_ms, ci.media_id, m.path, m.duration_ms, COALESCE(m.size_bytes, 0) AS size_bytes, ci.start_ms \
         FROM channel_items ci JOIN media_items m ON m.id = ci.media_id \
         WHERE ci.end_ms > ? AND ci.start_ms <= ? AND m.kind != 'image' \
         ORDER BY ci.start_ms",
    )
    .bind(from_ms)
    .bind(to_ms)
    .fetch_all(pool)
    .await?;
    use sqlx::Row;
    Ok(rows
        .into_iter()
        .map(|row| UpcomingMedia {
            channel_id: row.get("channel_id"),
            end_ms: row.get("end_ms"),
            media_id: row.get("media_id"),
            path: row.get("path"),
            duration_ms: row.get("duration_ms"),
            size_bytes: row.get("size_bytes"),
            start_ms: row.get("start_ms"),
        })
        .collect())
}

/// One SQLite read transaction supplies revisions, channels, schedule, and cache entries.
#[derive(Debug)]
pub(crate) struct PrecacheSnapshot {
    #[allow(dead_code)]
    pub now_ms: i64,
    #[allow(dead_code)]
    pub revisions: (i64, i64),
    pub channels: Vec<Channel>,
    pub schedule: Vec<PrecacheScheduleRow>,
    pub entries: Vec<PrecacheEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct PrecacheScheduleRow {
    pub in_ms: i64,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub media_kind: Option<String>,
    pub channel_id: String,
    pub media_id: String,
    pub start_ms: i64,
    pub end_ms: i64,
    #[allow(dead_code)]
    pub kind: String,
    pub title: String,
    pub path: Option<String>,
    pub duration_ms: Option<i64>,
    pub size_bytes: Option<i64>,
}

pub(crate) async fn precache_snapshot(
    pool: &SqlitePool,
    now: i64,
    horizon: i64,
) -> Result<PrecacheSnapshot> {
    let mut tx = pool.begin().await?;
    let snapshot = precache_snapshot_on(&mut tx, now, horizon).await?;
    tx.commit().await?;
    Ok(snapshot)
}

async fn precache_snapshot_on(
    conn: &mut sqlx::SqliteConnection,
    now: i64,
    horizon: i64,
) -> Result<PrecacheSnapshot> {
    let revisions = sqlx::query_as("SELECT (SELECT revision FROM renewal_input_revision WHERE id = 1), revision FROM rebuild_timeline_revision WHERE id = 1")
        .fetch_one(&mut *conn).await?;
    let channels = list_channels_on(&mut *conn).await?;
    // Keep the current row and its next two rows even outside the optional horizon.
    let rows = sqlx::query("SELECT ci.in_ms, m.video_codec, m.audio_codec, m.kind AS media_kind, ci.channel_id, ci.end_ms, ci.media_id, ci.kind, COALESCE(m.title, ci.title, ci.media_id) AS title, m.path, m.duration_ms, m.size_bytes, ci.start_ms FROM channel_items ci LEFT JOIN media_items m ON m.id = ci.media_id WHERE ci.end_ms > ? AND (m.kind IS NULL OR m.kind != 'image') ORDER BY ci.channel_id, ci.start_ms, ci.position")
        .bind(now).fetch_all(&mut *conn).await?;
    let mut schedule = Vec::new();
    let mut channel = String::new();
    let mut after_current = None::<usize>;
    for row in rows {
        let item = PrecacheScheduleRow {
            in_ms: row.get("in_ms"),
            video_codec: row.get("video_codec"),
            audio_codec: row.get("audio_codec"),
            media_kind: row.get("media_kind"),
            channel_id: row.get("channel_id"),
            end_ms: row.get("end_ms"),
            media_id: row.get("media_id"),
            kind: row.get("kind"),
            title: row.get("title"),
            path: row.get("path"),
            duration_ms: row.get("duration_ms"),
            size_bytes: row.get("size_bytes"),
            start_ms: row.get("start_ms"),
        };
        if item.channel_id != channel {
            channel = item.channel_id.clone();
            after_current = None;
        }
        if after_current.is_none() && item.start_ms <= now && item.end_ms > now {
            after_current = Some(0);
        }
        let required = after_current.is_some_and(|n| n < 3);
        if item.start_ms <= horizon || required {
            schedule.push(item);
        }
        if let Some(n) = after_current.as_mut() {
            *n += 1;
        }
    }
    let rows = sqlx::query("SELECT p.*, COALESCE(m.title, p.media_id) AS title FROM precache_entries p LEFT JOIN media_items m ON m.id = p.media_id ORDER BY p.start_ms").fetch_all(&mut *conn).await?;
    Ok(PrecacheSnapshot {
        now_ms: now,
        revisions,
        channels,
        schedule,
        entries: rows.iter().map(precache_row).collect(),
    })
}

pub async fn precache_entry(pool: &SqlitePool, media_id: &str) -> Result<Option<PrecacheEntry>> {
    let row = sqlx::query("SELECT * FROM precache_entries WHERE media_id = ? LIMIT 1")
        .bind(media_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(precache_row))
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_precache(
    pool: &SqlitePool,
    media_id: &str,
    source_path: &str,
    source_size: i64,
    source_mtime: i64,
    cache_path: &str,
    mode: &str,
    start_ms: i64,
    deadline_ms: i64,
) -> Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO precache_entries \
         (media_id, source_path, source_size, source_mtime, cache_path, mode, status, bytes, error, start_ms, created_ms, updated_ms, last_used_ms, deadline_ms, generation) \
         VALUES (?, ?, ?, ?, ?, ?, 'queued', 0, NULL, ?, ?, ?, 0, ?, ?) \
         ON CONFLICT(media_id) DO UPDATE SET \
           source_path = excluded.source_path, source_size = excluded.source_size, \
           source_mtime = excluded.source_mtime, cache_path = excluded.cache_path, \
           mode = excluded.mode, status = 'queued', bytes = CASE WHEN precache_entries.source_path = excluded.source_path AND precache_entries.source_size = excluded.source_size AND precache_entries.source_mtime = excluded.source_mtime AND precache_entries.cache_path IS excluded.cache_path AND precache_entries.mode = excluded.mode THEN precache_entries.bytes ELSE 0 END, error = NULL, identity_known = 1, \
           start_ms = excluded.start_ms, deadline_ms = excluded.deadline_ms, updated_ms = excluded.updated_ms, generation = CASE WHEN precache_entries.source_path = excluded.source_path AND precache_entries.source_size = excluded.source_size AND precache_entries.source_mtime = excluded.source_mtime AND precache_entries.cache_path IS excluded.cache_path AND precache_entries.mode = excluded.mode AND precache_entries.generation != '' THEN precache_entries.generation ELSE excluded.generation END \
         WHERE precache_entries.status != 'running' AND (precache_entries.status NOT IN ('queued', 'ready') OR precache_entries.source_path != excluded.source_path OR precache_entries.source_size != excluded.source_size OR precache_entries.source_mtime != excluded.source_mtime OR precache_entries.cache_path IS NOT excluded.cache_path OR precache_entries.mode != excluded.mode)",
    )
    .bind(media_id)
    .bind(source_path)
    .bind(source_size)
    .bind(source_mtime)
    .bind(cache_path)
    .bind(mode)
    .bind(start_ms)
    .bind(now)
    .bind(now)
    .bind(deadline_ms)
    .bind(uuid::Uuid::new_v4().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_precache_status(
    pool: &SqlitePool,
    media_id: &str,
    status: &str,
    bytes: i64,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE precache_entries SET status = ?, bytes = ?, error = ?, updated_ms = ?, retry_count = CASE WHEN ? = 'failed' THEN MIN(retry_count + 1, 5) WHEN ? = 'ready' THEN 0 ELSE retry_count END WHERE media_id = ?",
    )
    .bind(status)
    .bind(bytes)
    .bind(error)
    .bind(now_ms())
    .bind(status)
    .bind(status)
    .bind(media_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn invalidate_ready_precache(pool: &SqlitePool, media_id: &str) -> Result<bool> {
    let result = sqlx::query("UPDATE precache_entries SET status = 'failed', bytes = 0, error = 'evicted', updated_ms = ? WHERE media_id = ? AND status = 'ready'")
        .bind(now_ms()).bind(media_id).execute(pool).await?;
    Ok(result.rows_affected() == 1)
}

pub async fn set_precache_progress(
    pool: &SqlitePool,
    media_id: &str,
    generation: &str,
    bytes: i64,
) -> Result<()> {
    let result = sqlx::query("UPDATE precache_entries SET bytes = ?, updated_ms = ? WHERE media_id = ? AND generation = ? AND status = 'running'")
        .bind(bytes)
        .bind(now_ms())
        .bind(media_id)
        .bind(generation)
        .execute(pool)
        .await?;
    anyhow::ensure!(result.rows_affected() == 1, "precache generation lost");
    Ok(())
}

pub async fn list_precache(pool: &SqlitePool) -> Result<Vec<PrecacheEntry>> {
    let rows = sqlx::query(
        "SELECT p.*, COALESCE(m.title, p.media_id) AS title FROM precache_entries p \
         LEFT JOIN media_items m ON m.id = p.media_id ORDER BY p.start_ms",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(precache_row).collect())
}

pub async fn precache_statuses(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, (String, i64)>> {
    let rows = sqlx::query("SELECT media_id, status, updated_ms FROM precache_entries")
        .fetch_all(pool)
        .await?;
    use sqlx::Row;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("media_id"),
                (
                    row.get::<String, _>("status"),
                    row.get::<i64, _>("updated_ms"),
                ),
            )
        })
        .collect())
}

pub async fn prune_precache_queue(
    pool: &SqlitePool,
    cutoff_ms: i64,
    protected: &std::collections::HashSet<String>,
) -> Result<u64> {
    let rows = sqlx::query("SELECT media_id, generation, updated_ms FROM precache_entries WHERE status = 'queued' AND NOT EXISTS (SELECT 1 FROM channel_items ci WHERE ci.media_id = precache_entries.media_id AND ci.end_ms > ?)")
        .bind(cutoff_ms).fetch_all(pool).await?;
    let mut removed = 0;
    for row in rows {
        let id: String = row.get("media_id");
        if protected.contains(&id) {
            continue;
        }
        removed += sqlx::query("DELETE FROM precache_entries WHERE media_id = ? AND status = 'queued' AND generation = ? AND updated_ms = ? AND NOT EXISTS (SELECT 1 FROM channel_items ci WHERE ci.media_id = precache_entries.media_id AND ci.end_ms > ?)")
            .bind(id).bind(row.get::<String, _>("generation")).bind(row.get::<i64, _>("updated_ms")).bind(cutoff_ms).execute(pool).await?.rows_affected();
    }
    Ok(removed)
}

pub async fn trim_precache_queue(
    pool: &SqlitePool,
    keep: i64,
    protected: &std::collections::HashSet<String>,
) -> Result<u64> {
    let rows = sqlx::query("SELECT media_id, generation, updated_ms FROM precache_entries WHERE status = 'queued' ORDER BY deadline_ms, start_ms")
        .fetch_all(pool).await?;
    let optional_slots = (keep
        - rows
            .iter()
            .filter(|row| protected.contains(&row.get::<String, _>("media_id")))
            .count() as i64)
        .max(0) as usize;
    let mut removed = 0;
    for row in rows
        .iter()
        .filter(|row| !protected.contains(&row.get::<String, _>("media_id")))
        .skip(optional_slots)
    {
        removed += sqlx::query("DELETE FROM precache_entries WHERE media_id = ? AND status = 'queued' AND generation = ? AND updated_ms = ?")
            .bind(row.get::<String, _>("media_id")).bind(row.get::<String, _>("generation")).bind(row.get::<i64, _>("updated_ms")).execute(pool).await?.rows_affected();
    }
    Ok(removed)
}

/// Hydrate only the exact queued generation observed by the worker.
pub async fn hydrate_precache_identity(
    pool: &SqlitePool,
    entry: &PrecacheEntry,
    size: i64,
    mtime: i64,
) -> Result<bool> {
    let result = sqlx::query("UPDATE precache_entries SET generation = CASE WHEN source_mtime != 0 AND (source_size != ? OR source_mtime != ?) THEN lower(hex(randomblob(16))) ELSE generation END, source_size = ?, source_mtime = ?, identity_known = 1, error = NULL, updated_ms = ? WHERE media_id = ? AND status = 'queued' AND identity_known = 0 AND source_path = ? AND source_size = ? AND source_mtime = ? AND updated_ms = ? AND cache_path IS ? AND generation = ?")
        .bind(size).bind(mtime).bind(size).bind(mtime).bind(now_ms()).bind(&entry.media_id).bind(&entry.source_path)
        .bind(entry.source_size).bind(entry.source_mtime).bind(entry.updated_ms).bind(&entry.cache_path).bind(&entry.generation).execute(pool).await?;
    Ok(result.rows_affected() == 1)
}

pub async fn fail_precache_identity(
    pool: &SqlitePool,
    entry: &PrecacheEntry,
    error: &str,
) -> Result<()> {
    sqlx::query("UPDATE precache_entries SET status = 'failed', error = ?, updated_ms = ?, retry_count = MIN(retry_count + 1, 5) WHERE media_id = ? AND status = 'queued' AND identity_known = 0 AND source_path = ? AND source_size = ? AND source_mtime = ? AND updated_ms = ? AND cache_path IS ?")
        .bind(error).bind(now_ms()).bind(&entry.media_id).bind(&entry.source_path).bind(entry.source_size).bind(entry.source_mtime).bind(entry.updated_ms).bind(&entry.cache_path).execute(pool).await?;
    Ok(())
}

pub(crate) async fn claim_queued_precache(
    pool: &SqlitePool,
    entry: &PrecacheEntry,
) -> Result<bool> {
    let result = sqlx::query("UPDATE precache_entries SET status = 'running', error = NULL, updated_ms = ?, last_attempt_order = (SELECT COALESCE(MAX(last_attempt_order), 0) + 1 FROM precache_entries) WHERE media_id = ? AND status = 'queued' AND identity_known = 1 AND source_path = ? AND source_size = ? AND source_mtime = ? AND cache_path IS ? AND mode = ? AND updated_ms = ? AND generation = ?")
        .bind(now_ms()).bind(&entry.media_id).bind(&entry.source_path).bind(entry.source_size)
        .bind(entry.source_mtime).bind(&entry.cache_path).bind(&entry.mode).bind(entry.updated_ms).bind(&entry.generation)
        .execute(pool).await?;
    Ok(result.rows_affected() == 1)
}

pub(crate) async fn defer_queued_precache(
    pool: &SqlitePool,
    entry: &PrecacheEntry,
    reason: &str,
) -> Result<()> {
    sqlx::query("UPDATE precache_entries SET error = ? WHERE media_id = ? AND status = 'queued' AND identity_known = 1 AND source_path = ? AND source_size = ? AND source_mtime = ? AND cache_path IS ? AND mode = ? AND updated_ms = ?")
        .bind(reason).bind(&entry.media_id).bind(&entry.source_path).bind(entry.source_size)
        .bind(entry.source_mtime).bind(&entry.cache_path).bind(&entry.mode).bind(entry.updated_ms)
        .execute(pool).await?;
    Ok(())
}

#[allow(dead_code)]
pub async fn next_queued_precache(
    pool: &SqlitePool,
    not_before_ms: i64,
    allow_proxy: bool,
) -> Result<Option<PrecacheEntry>> {
    let sql = if allow_proxy {
        "SELECT * FROM precache_entries WHERE status = 'queued' AND ? >= 0 \
         ORDER BY start_ms, deadline_ms LIMIT 1"
    } else {
        "SELECT * FROM precache_entries WHERE status = 'queued' AND mode != 'proxy' AND ? >= 0 \
         ORDER BY start_ms, deadline_ms LIMIT 1"
    };
    let row = sqlx::query(sql)
        .bind(not_before_ms)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(precache_row))
}

/// Select a queued entry in a mandatory tier, preserving queue order.
pub async fn next_queued_precache_in(
    pool: &SqlitePool,
    ids: &[String],
    allow_proxy: bool,
) -> Result<Option<PrecacheEntry>> {
    let rows = sqlx::query("SELECT * FROM precache_entries WHERE status = 'queued' ORDER BY last_attempt_order, start_ms, deadline_ms, media_id")
        .fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(precache_row)
        .find(|entry| ids.contains(&entry.media_id) && (allow_proxy || entry.mode != "proxy")))
}

pub async fn reset_running_precache(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE precache_entries SET status = 'queued', updated_ms = ? WHERE status = 'running'",
    )
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_precache(pool: &SqlitePool, media_id: &str) -> Result<Option<String>> {
    let entry = precache_entry(pool, media_id).await?;
    sqlx::query("DELETE FROM precache_entries WHERE media_id = ?")
        .bind(media_id)
        .execute(pool)
        .await?;
    Ok(entry.and_then(|entry| entry.cache_path))
}

pub async fn set_media_size(pool: &SqlitePool, media_id: &str, size_bytes: i64) -> Result<()> {
    sqlx::query("UPDATE media_items SET size_bytes = ? WHERE id = ?")
        .bind(size_bytes)
        .bind(media_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn media_path(pool: &SqlitePool, media_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT path FROM media_items WHERE id = ? LIMIT 1")
        .bind(media_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| row.get::<String, _>("path")))
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelView {
    pub id: String,
    pub number: String,
    pub name: String,
    pub start_ms: i64,
    pub schedule_mode: String,
    pub now: Option<ChannelItem>,
    pub next: Option<ChannelItem>,
}

pub async fn list_channel_views(pool: &SqlitePool) -> Result<Vec<ChannelView>> {
    let channels = list_channels(pool).await?;
    let now = now_ms();
    let mut views = Vec::new();
    for channel in channels {
        let now_item = current_item(pool, &channel.id, now).await?;
        let next_item = sqlx::query(
            "SELECT ci.media_id, COALESCE(m.title, ci.title) AS title, ci.start_ms, ci.end_ms, ci.in_ms, ci.out_ms, ci.kind FROM channel_items ci LEFT JOIN media_items m ON m.id = ci.media_id WHERE ci.channel_id = ? AND ci.start_ms > ? ORDER BY ci.start_ms LIMIT 1",
        )
        .bind(&channel.id)
        .bind(now)
        .fetch_optional(pool)
        .await?
        .map(|row| ChannelItem {
            media_id: row.get("media_id"),
            title: row.get("title"),
            start_ms: row.get("start_ms"),
            end_ms: row.get("end_ms"),
            in_ms: row.get("in_ms"),
            out_ms: row.get("out_ms"),
            kind: row.get("kind"),
        });
        views.push(ChannelView {
            id: channel.id,
            number: channel.number,
            name: channel.name,
            start_ms: channel.start_ms,
            schedule_mode: channel.schedule_mode,
            now: now_item,
            next: next_item,
        });
    }
    Ok(views)
}

pub async fn counts_by_kind(pool: &SqlitePool) -> Result<Vec<(String, i64)>> {
    let rows =
        sqlx::query("SELECT kind, COUNT(*) AS count FROM media_items GROUP BY kind ORDER BY kind")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("kind"), row.get("count")))
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionSummary {
    pub id: String,
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionDetail {
    pub id: String,
    pub name: String,
    pub items: Vec<Media>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockInput {
    pub days_mask: u32,
    pub start_minute: u16,
    pub end_minute: u16,
    pub collection_id: String,
    #[serde(default)]
    pub filler_collection_id: Option<String>,
    #[serde(default = "default_order")]
    pub order: String,
}

fn default_order() -> String {
    "sequential".into()
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelBlock {
    pub id: String,
    pub channel_id: String,
    pub days_mask: u32,
    pub start_minute: u16,
    pub end_minute: u16,
    pub collection_id: String,
    pub filler_collection_id: Option<String>,
    pub order: String,
    pub position: i64,
}

pub async fn create_collection(
    pool: &SqlitePool,
    name: &str,
    media_ids: &[String],
) -> Result<CollectionSummary> {
    if name.trim().is_empty() {
        bail!("collection name is required");
    }
    if media_ids.is_empty() {
        bail!("select at least one media item");
    }
    let id = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO collections (id, name, created_at) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(name.trim())
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    for (position, media_id) in media_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO collection_items (collection_id, media_id, position) VALUES (?, ?, ?)",
        )
        .bind(&id)
        .bind(media_id)
        .bind(position as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(CollectionSummary {
        id,
        name: name.trim().into(),
        count: media_ids.len() as i64,
    })
}

pub async fn list_collections(pool: &SqlitePool) -> Result<Vec<CollectionSummary>> {
    let rows = sqlx::query(
        "SELECT c.id, c.name, COUNT(ci.media_id) AS count FROM collections c LEFT JOIN collection_items ci ON ci.collection_id = c.id GROUP BY c.id, c.name ORDER BY c.name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CollectionSummary {
            id: row.get("id"),
            name: row.get("name"),
            count: row.get("count"),
        })
        .collect())
}

pub async fn collection_detail(pool: &SqlitePool, id: &str) -> Result<Option<CollectionDetail>> {
    let row = sqlx::query("SELECT id, name FROM collections WHERE id = ? LIMIT 1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let items = sqlx::query(
        "SELECT m.id, m.path, m.title, m.kind, m.duration_ms, m.width, m.height, m.video_codec, m.audio_codec, m.size_bytes FROM collection_items ci JOIN media_items m ON m.id = ci.media_id WHERE ci.collection_id = ? AND m.path NOT LIKE '%.iso' ORDER BY ci.position",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| Media {
        id: row.get("id"),
        path: row.get("path"),
        title: row.get("title"),
        kind: row.get("kind"),
        duration_ms: row.get("duration_ms"),
        width: row.get("width"),
        height: row.get("height"),
        video_codec: row.get("video_codec"),
        audio_codec: row.get("audio_codec"),
        size_bytes: row.get("size_bytes"),
        chapters: Vec::new(),
    })
    .collect();
    Ok(Some(CollectionDetail {
        id: row.get("id"),
        name: row.get("name"),
        items,
    }))
}

pub async fn collection_id_by_name(pool: &SqlitePool, name: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM collections WHERE name = ? COLLATE NOCASE LIMIT 1")
        .bind(name.trim())
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| row.get("id")))
}

pub async fn delete_collection(pool: &SqlitePool, id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM collection_items WHERE collection_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM collections WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM channel_blocks WHERE collection_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM schedule_template_blocks WHERE collection_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_channel_blocks(
    pool: &SqlitePool,
    channel_id: &str,
    blocks: &[BlockInput],
) -> Result<Vec<ChannelBlock>> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM channel_blocks WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    for (position, block) in blocks.iter().enumerate() {
        if block.end_minute > 1440 || block.start_minute >= block.end_minute {
            bail!("invalid block time range");
        }
        let exists = sqlx::query("SELECT 1 FROM collections WHERE id = ? LIMIT 1")
            .bind(&block.collection_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            bail!("collection not found: {}", block.collection_id);
        }
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO channel_blocks (id, channel_id, days_mask, start_minute, end_minute, collection_id, filler_collection_id, play_order, position) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(channel_id)
        .bind(block.days_mask as i64)
        .bind(block.start_minute as i64)
        .bind(block.end_minute as i64)
        .bind(&block.collection_id)
        .bind(&block.filler_collection_id)
        .bind(&block.order)
        .bind(position as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    list_channel_blocks(pool, channel_id).await
}

pub async fn list_channel_blocks(pool: &SqlitePool, channel_id: &str) -> Result<Vec<ChannelBlock>> {
    let rows = sqlx::query(
        "SELECT id, channel_id, days_mask, start_minute, end_minute, collection_id, filler_collection_id, play_order, position FROM channel_blocks WHERE channel_id = ? ORDER BY position",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ChannelBlock {
            id: row.get("id"),
            channel_id: row.get("channel_id"),
            days_mask: row.get::<i64, _>("days_mask") as u32,
            start_minute: row.get::<i64, _>("start_minute") as u16,
            end_minute: row.get::<i64, _>("end_minute") as u16,
            collection_id: row.get("collection_id"),
            filler_collection_id: row.get("filler_collection_id"),
            order: row.get("play_order"),
            position: row.get("position"),
        })
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudSource {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub auth_type: String,
    pub policy: String,
    pub enabled: bool,
    pub config: serde_json::Value,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudSourceInput {
    pub provider: String,
    pub name: String,
    pub auth_type: String,
    #[serde(default = "default_cloud_policy")]
    pub policy: String,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub roots: Vec<CloudRootInput>,
}

fn default_cloud_policy() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CloudSourceUpdate {
    pub name: Option<String>,
    pub policy: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudRootInput {
    pub remote_id: String,
    pub remote_path: String,
    pub display_path: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CloudRootUpdate {
    pub display_path: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudRoot {
    pub id: String,
    pub source_id: String,
    pub remote_id: String,
    pub remote_path: String,
    pub display_path: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct NewCloudMedia {
    pub media_id: String,
    pub source_id: String,
    pub remote_id: String,
    pub remote_path: String,
    pub size_bytes: Option<i64>,
    pub mime_type: Option<String>,
    pub etag: Option<String>,
    pub remote_modified_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CloudMediaRow {
    pub media_id: String,
    pub source_id: String,
    pub remote_id: String,
    pub remote_path: String,
    pub size_bytes: Option<i64>,
    pub cache_path: Option<String>,
}

pub async fn create_cloud_source(
    pool: &SqlitePool,
    input: &CloudSourceInput,
    secret_blob: &str,
) -> Result<CloudSource> {
    let name = input.name.trim();
    if name.is_empty() {
        bail!("cloud source name is required");
    }
    if !matches!(input.policy.as_str(), "auto" | "cache_all" | "stream_all") {
        bail!("cloud policy must be auto, cache_all, or stream_all");
    }
    let id = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO cloud_sources (id, provider, name, auth_type, policy, enabled, secret_blob, config_json, created_at) VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.provider)
    .bind(name)
    .bind(&input.auth_type)
    .bind(&input.policy)
    .bind(secret_blob)
    .bind(serde_json::to_string(&input.config)?)
    .bind(now_ms())
    .execute(&mut *tx)
    .await?;
    for root in &input.roots {
        sqlx::query(
            "INSERT INTO cloud_roots (id, source_id, remote_id, remote_path, display_path, enabled) VALUES (?, ?, ?, ?, ?, 1)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&id)
        .bind(&root.remote_id)
        .bind(&root.remote_path)
        .bind(&root.display_path)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    cloud_source(pool, &id)
        .await?
        .context("cloud source disappeared after creation")
}

pub async fn list_cloud_sources(pool: &SqlitePool) -> Result<Vec<CloudSource>> {
    let rows = sqlx::query(
        "SELECT id, provider, name, auth_type, policy, enabled, config_json, last_sync_at, last_error, created_at FROM cloud_sources ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(cloud_source_row).collect())
}

pub async fn update_cloud_source(
    pool: &SqlitePool,
    id: &str,
    update: &CloudSourceUpdate,
) -> Result<CloudSource> {
    let name = match update.name.as_deref() {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                bail!("cloud source name cannot be empty");
            }
            Some(value)
        }
        None => None,
    };
    if let Some(policy) = update.policy.as_deref()
        && !matches!(policy, "auto" | "cache_all" | "stream_all")
    {
        bail!("cloud policy must be auto, cache_all, or stream_all");
    }
    let result = sqlx::query(
        "UPDATE cloud_sources SET name = COALESCE(?, name), policy = COALESCE(?, policy), enabled = COALESCE(?, enabled) WHERE id = ?",
    )
    .bind(name)
    .bind(update.policy.as_deref())
    .bind(update.enabled.map(|value| if value { 1 } else { 0 }))
    .bind(id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        bail!("cloud source not found");
    }
    cloud_source(pool, id)
        .await?
        .context("cloud source disappeared after update")
}

pub async fn cloud_source(pool: &SqlitePool, id: &str) -> Result<Option<CloudSource>> {
    let row = sqlx::query(
        "SELECT id, provider, name, auth_type, policy, enabled, config_json, last_sync_at, last_error, created_at FROM cloud_sources WHERE id = ? LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(cloud_source_row))
}

fn cloud_source_row(row: sqlx::sqlite::SqliteRow) -> CloudSource {
    CloudSource {
        id: row.get("id"),
        provider: row.get("provider"),
        name: row.get("name"),
        auth_type: row.get("auth_type"),
        policy: row.get("policy"),
        enabled: row.get::<i64, _>("enabled") != 0,
        config: row
            .get::<String, _>("config_json")
            .parse()
            .unwrap_or_else(|_| serde_json::json!({})),
        last_sync_at: row.get("last_sync_at"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
    }
}

pub async fn cloud_source_secret(pool: &SqlitePool, id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT secret_blob FROM cloud_sources WHERE id = ? LIMIT 1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.get::<Option<String>, _>("secret_blob")))
}

pub async fn delete_cloud_source(pool: &SqlitePool, id: &str) -> Result<()> {
    let media_ids: Vec<String> =
        sqlx::query("SELECT media_id FROM cloud_media WHERE source_id = ?")
            .bind(id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| row.get("media_id"))
            .collect();
    let mut tx = pool.begin().await?;
    for media_id in &media_ids {
        sqlx::query("DELETE FROM collection_items WHERE media_id = ?")
            .bind(media_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM channel_items WHERE media_id = ?")
            .bind(media_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM media_items WHERE id = ?")
            .bind(media_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM cloud_media WHERE source_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM cloud_roots WHERE source_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM cloud_sources WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
pub async fn stale_cloud_media(
    pool: &SqlitePool,
    source_id: &str,
    cutoff_ms: i64,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT media_id FROM cloud_media WHERE source_id = ? AND (last_seen_at IS NULL OR last_seen_at < ?)",
    )
    .bind(source_id)
    .bind(cutoff_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| row.get("media_id")).collect())
}

#[cfg(test)]
pub async fn delete_cloud_media(pool: &SqlitePool, media_id: &str) -> Result<Option<String>> {
    let cache_path: Option<String> =
        sqlx::query("SELECT cache_path FROM cloud_media WHERE media_id = ? LIMIT 1")
            .bind(media_id)
            .fetch_optional(pool)
            .await?
            .and_then(|row| row.get("cache_path"));
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM collection_items WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM channel_items WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM media_chapters WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM precache_entries WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM cloud_media WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM media_items WHERE id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(cache_path)
}

pub async fn list_cloud_roots(pool: &SqlitePool, source_id: &str) -> Result<Vec<CloudRoot>> {
    let rows = sqlx::query(
        "SELECT id, source_id, remote_id, remote_path, display_path, enabled FROM cloud_roots WHERE source_id = ? ORDER BY display_path",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CloudRoot {
            id: row.get("id"),
            source_id: row.get("source_id"),
            remote_id: row.get("remote_id"),
            remote_path: row.get("remote_path"),
            display_path: row.get("display_path"),
            enabled: row.get::<i64, _>("enabled") != 0,
        })
        .collect())
}

pub async fn cloud_root(pool: &SqlitePool, id: &str) -> Result<Option<CloudRoot>> {
    let row = sqlx::query(
        "SELECT id, source_id, remote_id, remote_path, display_path, enabled FROM cloud_roots WHERE id = ? LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| CloudRoot {
        id: row.get("id"),
        source_id: row.get("source_id"),
        remote_id: row.get("remote_id"),
        remote_path: row.get("remote_path"),
        display_path: row.get("display_path"),
        enabled: row.get::<i64, _>("enabled") != 0,
    }))
}

pub async fn update_cloud_root(
    pool: &SqlitePool,
    id: &str,
    update: &CloudRootUpdate,
) -> Result<CloudRoot> {
    let display_path = match update.display_path.as_deref() {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                bail!("root display path cannot be empty");
            }
            Some(value)
        }
        None => None,
    };
    let result = sqlx::query(
        "UPDATE cloud_roots SET display_path = COALESCE(?, display_path), enabled = COALESCE(?, enabled) WHERE id = ?",
    )
    .bind(display_path)
    .bind(update.enabled.map(|value| if value { 1 } else { 0 }))
    .bind(id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        bail!("cloud root not found");
    }
    cloud_root(pool, id)
        .await?
        .context("cloud root disappeared after update")
}

pub async fn add_cloud_root(
    pool: &SqlitePool,
    source_id: &str,
    root: &CloudRootInput,
) -> Result<CloudRoot> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO cloud_roots (id, source_id, remote_id, remote_path, display_path, enabled) VALUES (?, ?, ?, ?, ?, 1)",
    )
    .bind(&id)
    .bind(source_id)
    .bind(&root.remote_id)
    .bind(&root.remote_path)
    .bind(&root.display_path)
    .execute(pool)
    .await?;
    Ok(CloudRoot {
        id,
        source_id: source_id.to_string(),
        remote_id: root.remote_id.clone(),
        remote_path: root.remote_path.clone(),
        display_path: root.display_path.clone(),
        enabled: true,
    })
}

pub async fn delete_cloud_root(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM cloud_roots WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn cloud_media(pool: &SqlitePool, media_id: &str) -> Result<Option<CloudMediaRow>> {
    let row = sqlx::query(
        "SELECT media_id, source_id, remote_id, remote_path, size_bytes, cache_path FROM cloud_media WHERE media_id = ? LIMIT 1",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| CloudMediaRow {
        media_id: row.get("media_id"),
        source_id: row.get("source_id"),
        remote_id: row.get("remote_id"),
        remote_path: row.get("remote_path"),
        size_bytes: row.get("size_bytes"),
        cache_path: row.get("cache_path"),
    }))
}

pub async fn set_cloud_media_cache(
    pool: &SqlitePool,
    media_id: &str,
    state: &str,
    cache_path: Option<&str>,
    cached_bytes: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE cloud_media SET cache_state = ?, cache_path = ?, cached_bytes = ?, last_access_at = ? WHERE media_id = ?",
    )
    .bind(state)
    .bind(cache_path)
    .bind(cached_bytes)
    .bind(now_ms())
    .bind(media_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn touch_cloud_media(pool: &SqlitePool, media_id: &str) -> Result<()> {
    sqlx::query("UPDATE cloud_media SET last_access_at = ? WHERE media_id = ?")
        .bind(now_ms())
        .bind(media_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn cached_cloud_media(pool: &SqlitePool) -> Result<Vec<(String, String, i64)>> {
    let rows = sqlx::query(
        "SELECT media_id, cache_path, cached_bytes FROM cloud_media WHERE cache_state = 'cached' AND cache_path IS NOT NULL ORDER BY COALESCE(last_access_at, 0) ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get("media_id"),
                row.get("cache_path"),
                row.get::<Option<i64>, _>("cached_bytes").unwrap_or(0),
            )
        })
        .collect())
}

#[derive(Debug, Clone)]
pub enum ScheduleDefinition {
    Classic(Vec<ClassicItemInput>),
    Blocks(Vec<BlockInput>),
}

#[derive(Debug, Clone, Serialize)]
pub struct ScheduleOverride {
    pub id: String,
    pub channel_id: String,
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub priority: i64,
    #[serde(skip)]
    pub schedule_yaml: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleOverrideInput {
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    #[serde(default)]
    pub priority: i64,
    pub schedule_yaml: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageSettings {
    #[serde(default)]
    pub audio_language: Option<String>,
    #[serde(default = "default_subtitle_mode")]
    pub subtitle_mode: String,
    #[serde(default)]
    pub subtitle_language: Option<String>,
}

pub async fn ensure_app_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO app_settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO NOTHING")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn app_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM app_settings WHERE key = ? LIMIT 1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| row.get("value")))
}

pub async fn language_settings(pool: &SqlitePool) -> Result<LanguageSettings> {
    let audio_language = app_setting(pool, "default_audio_language")
        .await?
        .and_then(non_empty)
        .or_else(|| Some("eng".into()));
    let subtitle_mode = app_setting(pool, "default_subtitle_mode")
        .await?
        .and_then(non_empty)
        .unwrap_or_else(|| "off".into());
    let subtitle_language = app_setting(pool, "default_subtitle_language")
        .await?
        .and_then(non_empty);
    Ok(LanguageSettings {
        audio_language,
        subtitle_mode,
        subtitle_language,
    })
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub fn parse_date(text: &str) -> Result<time::Date> {
    let parts: Vec<&str> = text.trim().split('-').collect();
    if parts.len() != 3 {
        bail!("dates must be YYYY-MM-DD: {text}");
    }
    let year: i32 = parts[0]
        .parse()
        .with_context(|| format!("invalid year: {text}"))?;
    let month: u8 = parts[1]
        .parse()
        .with_context(|| format!("invalid month: {text}"))?;
    let day: u8 = parts[2]
        .parse()
        .with_context(|| format!("invalid day: {text}"))?;
    time::Date::from_calendar_date(
        year,
        time::Month::try_from(month).with_context(|| format!("invalid month: {text}"))?,
        day,
    )
    .with_context(|| format!("invalid date: {text}"))
}

pub fn format_date(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

pub async fn list_schedule_overrides(
    pool: &SqlitePool,
    channel_id: &str,
) -> Result<Vec<ScheduleOverride>> {
    let rows = sqlx::query(
        "SELECT id, channel_id, name, start_date, end_date, priority, schedule_yaml FROM schedule_overrides WHERE channel_id = ? ORDER BY start_date, priority DESC",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ScheduleOverride {
            id: row.get("id"),
            channel_id: row.get("channel_id"),
            name: row.get("name"),
            start_date: row.get("start_date"),
            end_date: row.get("end_date"),
            priority: row.get("priority"),
            schedule_yaml: row.get("schedule_yaml"),
        })
        .collect())
}

pub async fn create_schedule_override(
    pool: &SqlitePool,
    channel_id: &str,
    input: &ScheduleOverrideInput,
) -> Result<ScheduleOverride> {
    let name = input.name.trim();
    if name.is_empty() {
        bail!("override name is required");
    }
    let start = parse_date(&input.start_date)?;
    let end = parse_date(&input.end_date)?;
    if end < start {
        bail!("override end date must be on or after its start date");
    }
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedule_overrides (id, channel_id, name, start_date, end_date, priority, schedule_yaml, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(channel_id)
    .bind(name)
    .bind(format_date(start))
    .bind(format_date(end))
    .bind(input.priority)
    .bind(&input.schedule_yaml)
    .bind(now_ms())
    .execute(pool)
    .await?;
    list_schedule_overrides(pool, channel_id)
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .context("override disappeared after creation")
}

pub async fn delete_schedule_override(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM schedule_overrides WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockTemplateSummary {
    pub id: String,
    pub name: String,
    pub block_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockTemplate {
    pub id: String,
    pub name: String,
    pub blocks: Vec<BlockInput>,
    pub graphics: GraphicsSettings,
}

pub async fn list_block_templates(pool: &SqlitePool) -> Result<Vec<BlockTemplateSummary>> {
    let rows = sqlx::query(
        "SELECT t.id, t.name, COUNT(b.id) AS block_count FROM schedule_templates t LEFT JOIN schedule_template_blocks b ON b.template_id = t.id GROUP BY t.id, t.name ORDER BY t.name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| BlockTemplateSummary {
            id: row.get("id"),
            name: row.get("name"),
            block_count: row.get("block_count"),
        })
        .collect())
}

pub async fn block_template(pool: &SqlitePool, id: &str) -> Result<Option<BlockTemplate>> {
    let row = sqlx::query(
        "SELECT id, name, graphics_text, graphics_position, graphics_opacity, graphics_size, watermark_media_id, watermark_position, watermark_opacity, watermark_scale, lower_third_text, lower_third_size, lower_third_opacity FROM schedule_templates WHERE id = ? LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let name: String = row.get("name");
    let graphics = graphics_from_row(&row);
    let blocks = sqlx::query(
        "SELECT days_mask, start_minute, end_minute, collection_id, filler_collection_id, play_order FROM schedule_template_blocks WHERE template_id = ? ORDER BY position",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| BlockInput {
        days_mask: row.get::<i64, _>("days_mask") as u32,
        start_minute: row.get::<i64, _>("start_minute") as u16,
        end_minute: row.get::<i64, _>("end_minute") as u16,
        collection_id: row.get("collection_id"),
        filler_collection_id: row.get("filler_collection_id"),
        order: row.get("play_order"),
    })
    .collect();
    Ok(Some(BlockTemplate {
        id: id.to_string(),
        name,
        blocks,
        graphics,
    }))
}

pub async fn create_block_template(
    pool: &SqlitePool,
    name: &str,
    blocks: &[BlockInput],
    graphics: &GraphicsSettings,
) -> Result<BlockTemplate> {
    let name = name.trim();
    if name.is_empty() {
        bail!("template name is required");
    }
    if blocks.is_empty() {
        bail!("add at least one block before saving a template");
    }
    let exists = sqlx::query("SELECT 1 FROM schedule_templates WHERE name = ? LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    if exists.is_some() {
        bail!("a template named '{name}' already exists");
    }
    let id = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO schedule_templates (id, name, graphics_text, graphics_position, graphics_opacity, graphics_size, watermark_media_id, watermark_position, watermark_opacity, watermark_scale, lower_third_text, lower_third_size, lower_third_opacity, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(&graphics.text)
    .bind(&graphics.position)
    .bind(graphics.opacity)
    .bind(graphics.size)
    .bind(&graphics.watermark_media_id)
    .bind(&graphics.watermark_position)
    .bind(graphics.watermark_opacity)
    .bind(graphics.watermark_scale)
    .bind(&graphics.lower_third_text)
    .bind(graphics.lower_third_size)
    .bind(graphics.lower_third_opacity)
    .bind(now_ms())
    .execute(&mut *tx)
    .await?;
    for (position, block) in blocks.iter().enumerate() {
        if block.end_minute > 1440 || block.start_minute >= block.end_minute {
            bail!("invalid block time range");
        }
        let collection = sqlx::query("SELECT 1 FROM collections WHERE id = ? LIMIT 1")
            .bind(&block.collection_id)
            .fetch_optional(&mut *tx)
            .await?;
        if collection.is_none() {
            bail!("collection not found: {}", block.collection_id);
        }
        let block_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO schedule_template_blocks (id, template_id, position, days_mask, start_minute, end_minute, collection_id, filler_collection_id, play_order) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&block_id)
        .bind(&id)
        .bind(position as i64)
        .bind(block.days_mask as i64)
        .bind(block.start_minute as i64)
        .bind(block.end_minute as i64)
        .bind(&block.collection_id)
        .bind(&block.filler_collection_id)
        .bind(&block.order)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    block_template(pool, &id)
        .await?
        .context("template disappeared after creation")
}

pub async fn delete_block_template(pool: &SqlitePool, id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM schedule_template_blocks WHERE template_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM schedule_templates WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassicItemInput {
    pub collection_id: String,
    pub mode: String,
    #[serde(default = "one")]
    pub count: i64,
    #[serde(default = "sixty")]
    pub duration_minutes: i64,
    #[serde(default)]
    pub filler_collection_id: Option<String>,
    #[serde(default)]
    pub mid_roll_interval_minutes: i64,
    #[serde(default)]
    pub mid_roll_duration_seconds: i64,
    #[serde(default = "default_true")]
    pub chapter_breaks: bool,
    #[serde(default)]
    pub break_markers: Vec<i64>,
    #[serde(default = "default_order")]
    pub order: String,
}

fn default_true() -> bool {
    true
}

fn one() -> i64 {
    1
}
fn sixty() -> i64 {
    60
}

#[derive(Debug, Clone, Serialize)]
pub struct ClassicItem {
    pub id: String,
    pub channel_id: String,
    pub collection_id: String,
    pub mode: String,
    pub count: i64,
    pub duration_minutes: i64,
    pub filler_collection_id: Option<String>,
    pub mid_roll_interval_minutes: i64,
    pub mid_roll_duration_seconds: i64,
    pub chapter_breaks: bool,
    pub break_markers: Vec<i64>,
    pub order: String,
    pub position: i64,
}

pub async fn set_classic_items(
    pool: &SqlitePool,
    channel_id: &str,
    items: &[ClassicItemInput],
) -> Result<Vec<ClassicItem>> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM channel_classic_items WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    for (position, item) in items.iter().enumerate() {
        if item.break_markers.iter().any(|marker| *marker < 0) {
            bail!("break markers cannot be negative");
        }
        if !item.break_markers.is_empty()
            && (item.mid_roll_duration_seconds <= 0 || item.filler_collection_id.is_none())
        {
            bail!("manual break markers need a filler collection and a break length");
        }
        let exists = sqlx::query("SELECT 1 FROM collections WHERE id = ? LIMIT 1")
            .bind(&item.collection_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            bail!("collection not found: {}", item.collection_id);
        }
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO channel_classic_items (id, channel_id, position, collection_id, mode, count, duration_minutes, filler_collection_id, mid_roll_interval_minutes, mid_roll_duration_seconds, chapter_breaks, break_markers, play_order) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(channel_id)
        .bind(position as i64)
        .bind(&item.collection_id)
        .bind(&item.mode)
        .bind(item.count)
        .bind(item.duration_minutes)
        .bind(&item.filler_collection_id)
        .bind(item.mid_roll_interval_minutes)
        .bind(item.mid_roll_duration_seconds)
        .bind(item.chapter_breaks)
        .bind(serde_json::to_string(&item.break_markers).unwrap_or_else(|_| "[]".into()))
        .bind(&item.order)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE channels SET schedule_mode = 'classic' WHERE id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    list_classic_items(pool, channel_id).await
}

pub async fn list_classic_items(pool: &SqlitePool, channel_id: &str) -> Result<Vec<ClassicItem>> {
    let rows = sqlx::query(
        "SELECT id, channel_id, position, collection_id, mode, count, duration_minutes, filler_collection_id, mid_roll_interval_minutes, mid_roll_duration_seconds, chapter_breaks, break_markers, play_order FROM channel_classic_items WHERE channel_id = ? ORDER BY position",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ClassicItem {
            id: row.get("id"),
            channel_id: row.get("channel_id"),
            collection_id: row.get("collection_id"),
            mode: row.get("mode"),
            count: row.get("count"),
            duration_minutes: row.get("duration_minutes"),
            filler_collection_id: row.get("filler_collection_id"),
            mid_roll_interval_minutes: row.get("mid_roll_interval_minutes"),
            mid_roll_duration_seconds: row.get("mid_roll_duration_seconds"),
            chapter_breaks: row.get("chapter_breaks"),
            break_markers: serde_json::from_str::<Vec<i64>>(&row.get::<String, _>("break_markers"))
                .unwrap_or_default(),
            order: row.get("play_order"),
            position: row.get("position"),
        })
        .collect())
}

pub async fn set_schedule_mode(pool: &SqlitePool, channel_id: &str, mode: &str) -> Result<()> {
    sqlx::query("UPDATE channels SET schedule_mode = ? WHERE id = ?")
        .bind(mode)
        .bind(channel_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn graphics_from_row(row: &sqlx::sqlite::SqliteRow) -> GraphicsSettings {
    GraphicsSettings {
        text: row.get("graphics_text"),
        position: row.get("graphics_position"),
        opacity: row.get("graphics_opacity"),
        size: row.get("graphics_size"),
        watermark_media_id: row.get("watermark_media_id"),
        watermark_position: row.get("watermark_position"),
        watermark_opacity: row.get("watermark_opacity"),
        watermark_scale: row.get("watermark_scale"),
        lower_third_text: row.get("lower_third_text"),
        lower_third_size: row.get("lower_third_size"),
        lower_third_opacity: row.get("lower_third_opacity"),
    }
}

fn stream_from_row(row: &sqlx::sqlite::SqliteRow) -> StreamSettings {
    StreamSettings {
        width: row.get("stream_width"),
        height: row.get("stream_height"),
        preset: row.get("stream_preset"),
        crf: row.get("stream_crf"),
        max_kbps: row.get("stream_max_kbps"),
        audio_kbps: row.get("stream_audio_kbps"),
        audio_language: row.get("audio_language"),
        subtitle_mode: row.get("subtitle_mode"),
        subtitle_language: row.get("subtitle_language"),
        hdr_mode: row.get("hdr_mode"),
        encoder: row.get("encoder"),
        segment_seconds: row.get("segment_seconds"),
        prebuffer_seconds: row.get("prebuffer_seconds"),
        cache_segments: row.get("cache_segments"),
        prewarm: row.get("prewarm"),
    }
}

pub async fn channel_graphics(pool: &SqlitePool, id: &str) -> Result<GraphicsSettings> {
    Ok(channel_by_id(pool, id)
        .await?
        .context("channel not found")?
        .graphics)
}

pub async fn set_channel_graphics(
    pool: &SqlitePool,
    id: &str,
    graphics: &GraphicsSettings,
) -> Result<()> {
    sqlx::query(
        "UPDATE channels SET graphics_text = ?, graphics_position = ?, graphics_opacity = ?, graphics_size = ?, watermark_media_id = ?, watermark_position = ?, watermark_opacity = ?, watermark_scale = ?, lower_third_text = ?, lower_third_size = ?, lower_third_opacity = ? WHERE id = ?",
    )
    .bind(&graphics.text)
    .bind(&graphics.position)
    .bind(graphics.opacity)
    .bind(graphics.size)
    .bind(&graphics.watermark_media_id)
    .bind(&graphics.watermark_position)
    .bind(graphics.watermark_opacity)
    .bind(graphics.watermark_scale)
    .bind(&graphics.lower_third_text)
    .bind(graphics.lower_third_size)
    .bind(graphics.lower_third_opacity)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn channel_stream(pool: &SqlitePool, id: &str) -> Result<StreamSettings> {
    Ok(channel_by_id(pool, id)
        .await?
        .context("channel not found")?
        .stream)
}

pub async fn set_all_channel_encoders(pool: &SqlitePool, encoder: &str) -> Result<usize> {
    let result = sqlx::query("UPDATE channels SET encoder = ?")
        .bind(encoder)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() as usize)
}

pub async fn set_channel_stream(
    pool: &SqlitePool,
    id: &str,
    stream: &StreamSettings,
) -> Result<()> {
    sqlx::query(
        "UPDATE channels SET stream_width = ?, stream_height = ?, stream_preset = ?, stream_crf = ?, stream_max_kbps = ?, stream_audio_kbps = ?, audio_language = ?, subtitle_mode = ?, subtitle_language = ?, hdr_mode = ?, encoder = ?, segment_seconds = ?, prebuffer_seconds = ?, cache_segments = ?, prewarm = ? WHERE id = ?",
    )
    .bind(stream.width)
    .bind(stream.height)
    .bind(&stream.preset)
    .bind(stream.crf)
    .bind(stream.max_kbps)
    .bind(stream.audio_kbps)
    .bind(&stream.audio_language)
    .bind(&stream.subtitle_mode)
    .bind(&stream.subtitle_language)
    .bind(&stream.hdr_mode)
    .bind(&stream.encoder)
    .bind(stream.segment_seconds)
    .bind(stream.prebuffer_seconds)
    .bind(stream.cache_segments)
    .bind(stream.prewarm)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn channel_by_id(pool: &SqlitePool, id: &str) -> Result<Option<Channel>> {
    let mut connection = pool.acquire().await?;
    channel_by_id_on(&mut connection, id).await
}

async fn channel_by_id_on(
    connection: &mut sqlx::SqliteConnection,
    id: &str,
) -> Result<Option<Channel>> {
    let row = sqlx::query(
        "SELECT id, number, name, start_ms, schedule_mode, graphics_text, graphics_position, graphics_opacity, graphics_size, watermark_media_id, watermark_position, watermark_opacity, watermark_scale, lower_third_text, lower_third_size, lower_third_opacity, stream_width, stream_height, stream_preset, stream_crf, stream_max_kbps, stream_audio_kbps, audio_language, subtitle_mode, subtitle_language, hdr_mode, encoder, segment_seconds, prebuffer_seconds, cache_segments, prewarm FROM channels WHERE id = ? LIMIT 1",
    )
    .bind(id)
    .fetch_optional(connection)
    .await?;
    Ok(row.map(|row| Channel {
        id: row.get("id"),
        number: row.get("number"),
        name: row.get("name"),
        start_ms: row.get("start_ms"),
        schedule_mode: row.get("schedule_mode"),
        graphics: graphics_from_row(&row),
        stream: stream_from_row(&row),
    }))
}

async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await?;
    if !rows
        .iter()
        .any(|row| row.get::<String, _>("name") == column)
    {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub fn now_ms() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp() * 1000
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub last_used_ip: Option<String>,
}

pub struct ApiTokenAuth {
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<i64>,
}

fn parse_scopes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect()
}

fn join_scopes(scopes: &[String]) -> String {
    scopes.join(",")
}

pub async fn create_api_token(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    token_hash: &str,
    scopes: &[String],
    expires_at: Option<i64>,
) -> Result<ApiToken> {
    let created_at = now_ms();
    sqlx::query(
        "INSERT INTO api_tokens (id, name, token_hash, scopes, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(name)
    .bind(token_hash)
    .bind(join_scopes(scopes))
    .bind(expires_at)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(ApiToken {
        id: id.to_string(),
        name: name.to_string(),
        scopes: scopes.to_vec(),
        expires_at,
        created_at,
        last_used_at: None,
        last_used_ip: None,
    })
}

pub async fn list_api_tokens(pool: &SqlitePool) -> Result<Vec<ApiToken>> {
    let rows = sqlx::query(
        "SELECT id, name, scopes, expires_at, created_at, last_used_at, last_used_ip FROM api_tokens ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ApiToken {
            id: row.get("id"),
            name: row.get("name"),
            scopes: parse_scopes(&row.get::<String, _>("scopes")),
            expires_at: row.get("expires_at"),
            created_at: row.get("created_at"),
            last_used_at: row.get("last_used_at"),
            last_used_ip: row.get("last_used_ip"),
        })
        .collect())
}

pub async fn delete_api_token(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM api_tokens WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn api_token_auth(pool: &SqlitePool, id: &str) -> Result<Option<ApiTokenAuth>> {
    let row =
        sqlx::query("SELECT token_hash, scopes, expires_at FROM api_tokens WHERE id = ? LIMIT 1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|row| ApiTokenAuth {
        token_hash: row.get("token_hash"),
        scopes: parse_scopes(&row.get::<String, _>("scopes")),
        expires_at: row.get("expires_at"),
    }))
}

pub async fn touch_api_token(
    pool: &SqlitePool,
    id: &str,
    now: i64,
    client_ip: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE api_tokens SET last_used_at = ?, last_used_ip = ? WHERE id = ? AND (last_used_at IS NULL OR last_used_at < ?)",
    )
    .bind(now)
    .bind(client_ip)
    .bind(id)
    .bind(now - 60_000)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn cloud_secret_blobs(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT id, secret_blob FROM cloud_sources WHERE secret_blob IS NOT NULL AND secret_blob != ''",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("id"), row.get("secret_blob")))
        .collect())
}

pub async fn update_cloud_secret_blob(
    pool: &SqlitePool,
    id: &str,
    secret_blob: &str,
) -> Result<()> {
    sqlx::query("UPDATE cloud_sources SET secret_blob = ? WHERE id = ?")
        .bind(secret_blob)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudSourceMetrics {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub media: i64,
    pub cached_media: i64,
    pub cached_bytes: i64,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudMetrics {
    pub sources: usize,
    pub enabled_sources: usize,
    pub media: i64,
    pub cached_media: i64,
    pub cached_bytes: i64,
    pub sources_with_errors: usize,
    pub by_source: Vec<CloudSourceMetrics>,
}

pub async fn cloud_metrics(pool: &SqlitePool) -> Result<CloudMetrics> {
    let rows = sqlx::query(
        "SELECT s.id, s.name, s.enabled, s.last_sync_at, s.last_error, \
            COUNT(m.media_id) AS media, \
            COALESCE(SUM(CASE WHEN m.cache_state = 'cached' THEN 1 ELSE 0 END), 0) AS cached_media, \
            COALESCE(SUM(CASE WHEN m.cache_state = 'cached' THEN m.cached_bytes ELSE 0 END), 0) AS cached_bytes \
         FROM cloud_sources s \
         LEFT JOIN cloud_media m ON m.source_id = s.id \
         GROUP BY s.id, s.name, s.enabled, s.last_sync_at, s.last_error \
         ORDER BY s.name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    let by_source: Vec<CloudSourceMetrics> = rows
        .into_iter()
        .map(|row| CloudSourceMetrics {
            id: row.get("id"),
            name: row.get("name"),
            enabled: row.get::<i64, _>("enabled") != 0,
            media: row.get("media"),
            cached_media: row.get("cached_media"),
            cached_bytes: row.get("cached_bytes"),
            last_sync_at: row.get("last_sync_at"),
            last_error: row.get("last_error"),
        })
        .collect();
    Ok(CloudMetrics {
        sources: by_source.len(),
        enabled_sources: by_source.iter().filter(|source| source.enabled).count(),
        media: by_source.iter().map(|source| source.media).sum(),
        cached_media: by_source.iter().map(|source| source.cached_media).sum(),
        cached_bytes: by_source.iter().map(|source| source.cached_bytes).sum(),
        sources_with_errors: by_source
            .iter()
            .filter(|source| source.last_error.is_some())
            .count(),
        by_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_merges_program_segments_across_short_breaks() {
        let item = |id: &str, kind: &str, start: i64, end: i64| ChannelItem {
            media_id: id.into(),
            title: id.into(),
            start_ms: start,
            end_ms: end,
            in_ms: 0,
            out_ms: Some(end - start),
            kind: kind.into(),
        };
        let mut continuation = item("movie", "program", 32_000, 62_000);
        continuation.in_ms = 30_000;
        continuation.out_ms = Some(60_000);
        let merged = merge_guide_blocks(vec![
            item("movie", "program", 0, 30_000),
            item("bumper", "filler", 30_000, 32_000),
            continuation,
            item("other", "program", 62_000, 80_000),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].media_id, "movie");
        assert_eq!(merged[0].start_ms, 0);
        assert_eq!(merged[0].end_ms, 62_000);
        assert_eq!(merged[1].media_id, "other");
    }

    #[test]
    fn guide_merge_requires_offset_continuity_and_valid_gap() {
        let item = |start, end, in_ms, out_ms| ChannelItem {
            media_id: "movie".into(),
            title: "movie".into(),
            start_ms: start,
            end_ms: end,
            in_ms,
            out_ms,
            kind: "program".into(),
        };
        for (next_start, next_in, first_out, expected) in [
            (100, 0, Some(100), 2),       // another broadcast
            (100, 100, None, 2),          // unknown source endpoint
            (99, 100, Some(100), 2),      // overlapping timeline
            (300_100, 100, Some(100), 1), // inclusive gap limit
            (300_101, 100, Some(100), 2),
        ] {
            let blocks = merge_guide_blocks(vec![
                item(0, 100, 0, first_out),
                item(next_start, next_start + 100, next_in, Some(next_in + 100)),
            ]);
            assert_eq!(
                blocks.len(),
                expected,
                "start={next_start}, in={next_in}, out={first_out:?}"
            );
        }
    }

    #[tokio::test]
    async fn precache_claim_rejects_changed_queued_generation() {
        let dir = std::env::temp_dir().join(format!("awesometv-claim-{}", Uuid::new_v4()));
        let pool = init(&dir).await.unwrap();
        upsert_precache(&pool, "m", "/source", 4, 1, "/cache/m", "copy", 1, 1)
            .await
            .unwrap();
        let old = precache_entry(&pool, "m").await.unwrap().unwrap();
        sqlx::query("UPDATE precache_entries SET source_mtime = 2 WHERE media_id = 'm'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!claim_queued_precache(&pool, &old).await.unwrap());
        let current = precache_entry(&pool, "m").await.unwrap().unwrap();
        assert_eq!(current.status, "queued");
        assert!(claim_queued_precache(&pool, &current).await.unwrap());
        assert!(!claim_queued_precache(&pool, &current).await.unwrap());
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn precache_snapshot_coherent_and_includes_beyond_horizon() {
        let dir = std::env::temp_dir().join(format!("awesometv-snapshot-{}", Uuid::new_v4()));
        let pool = init(&dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('m','/m','m','movie',100,0)").execute(&pool).await.unwrap();
        let channel = create_channel_shell(&pool, "99", "test").await.unwrap();
        let _gap = create_channel_shell(&pool, "98", "gap").await.unwrap();
        for (position, start, end) in [(0, 0, 100), (1, 100, 200), (2, 200, 300)] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,'m','m',?,?,0,100,'program')").bind(&channel.id).bind(position).bind(start).bind(end).execute(&pool).await.unwrap();
        }
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let before: i64 =
            sqlx::query_scalar("SELECT revision FROM renewal_input_revision WHERE id = 1")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        sqlx::query("UPDATE renewal_input_revision SET revision = revision + 1 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM channel_items WHERE channel_id = ?")
            .bind(&channel.id)
            .execute(&pool)
            .await
            .unwrap();
        let snapshot = precache_snapshot_on(&mut tx, 50, 50).await.unwrap();
        assert_eq!(snapshot.revisions.0, before);
        tx.commit().await.unwrap();
        assert_eq!(snapshot.now_ms, 50);
        assert_eq!(snapshot.channels.len(), 2);
        assert_eq!(
            snapshot
                .schedule
                .iter()
                .map(|r| r.start_ms)
                .collect::<Vec<_>>(),
            vec![0, 100, 200]
        );
        assert!(snapshot.entries.is_empty());
        let fresh = precache_snapshot(&pool, 50, 50).await.unwrap();
        assert!(fresh.schedule.is_empty());
        assert!(fresh.revisions.0 > snapshot.revisions.0);
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn upcoming_media_includes_current_but_not_finished_items() {
        let dir = std::env::temp_dir().join(format!("awesometv-db-test-{}", Uuid::new_v4()));
        let pool = init(&dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('m','/media/test','test','movie',10000,0)")
            .execute(&pool).await.unwrap();
        let channel = create_channel_shell(&pool, "99", "test").await.unwrap();
        for (position, start, end) in [(0, 0, 100), (1, 100, 200), (2, 200, 300)] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?, 'm','test',?,?,0,100,'program')")
                .bind(&channel.id).bind(position).bind(start).bind(end).execute(&pool).await.unwrap();
        }
        let rows = upcoming_media(&pool, 100, 200).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.start_ms).collect::<Vec<_>>(),
            vec![100, 200]
        );
        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn precache_can_defer_proxy_jobs_while_live() {
        let dir = std::env::temp_dir().join(format!("awesometv-db-test-{}", Uuid::new_v4()));
        let pool = init(&dir).await.unwrap();
        let now = now_ms();
        for (media_id, mode, deadline) in [
            ("proxy", "proxy", now + 1_000),
            ("copy", "copy", now + 2_000),
        ] {
            sqlx::query(
                "INSERT INTO precache_entries (media_id, source_path, source_size, source_mtime, mode, status, start_ms, created_ms, updated_ms, deadline_ms) VALUES (?, '/media/x', 10, 0, ?, 'queued', ?, ?, ?, ?)",
            )
            .bind(media_id)
            .bind(mode)
            .bind(now + 10_000)
            .bind(now)
            .bind(now)
            .bind(deadline)
            .execute(&pool)
            .await
            .unwrap();
        }
        let live = next_queued_precache(&pool, now, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.mode, "copy");
        let idle = next_queued_precache(&pool, now, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idle.mode, "proxy");
        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cloud_root_can_be_disabled_and_renamed() {
        let dir = std::env::temp_dir().join(format!("awesometv-db-test-{}", Uuid::new_v4()));
        let pool = init(&dir).await.unwrap();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO cloud_sources (id, provider, name, auth_type, policy, enabled, secret_blob, config_json, created_at) VALUES ('source', 'google', 'Test', 'access_token', 'auto', 1, 'x', '{}', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cloud_roots (id, source_id, remote_id, remote_path, display_path, enabled) VALUES ('root', 'source', 'remote', '/remote', 'Remote', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let updated = update_cloud_root(
            &pool,
            "root",
            &CloudRootUpdate {
                display_path: Some("Renamed".into()),
                enabled: Some(false),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.display_path, "Renamed");
        assert!(!updated.enabled);
        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn full_scan_reconciliation_removes_missing_cloud_media() {
        let dir = std::env::temp_dir().join(format!("awesometv-db-test-{}", Uuid::new_v4()));
        let pool = init(&dir).await.unwrap();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO cloud_sources (id, provider, name, auth_type, policy, enabled, secret_blob, config_json, created_at) VALUES ('source', 'google', 'Test', 'access_token', 'auto', 1, 'x', '{}', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        for (media_id, seen) in [("fresh", now), ("stale", now - 60_000)] {
            sqlx::query(
                "INSERT INTO cloud_media (media_id, source_id, remote_id, remote_path, cache_state, last_seen_at) VALUES (?, 'source', ?, ?, 'none', ?)",
            )
            .bind(media_id)
            .bind(media_id)
            .bind(format!("Movies/{media_id}.mkv"))
            .bind(seen)
            .execute(&pool)
            .await
            .unwrap();
        }

        let stale = stale_cloud_media(&pool, "source", now - 1_000)
            .await
            .unwrap();
        assert_eq!(stale, vec!["stale".to_string()]);
        delete_cloud_media(&pool, "stale").await.unwrap();
        assert!(cloud_media(&pool, "stale").await.unwrap().is_none());
        assert!(cloud_media(&pool, "fresh").await.unwrap().is_some());

        let metrics = cloud_metrics(&pool).await.unwrap();
        assert_eq!(metrics.sources, 1);
        assert_eq!(metrics.media, 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub id: String,
    pub at_ms: i64,
    pub actor: String,
    pub kind: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
    pub client_ip: Option<String>,
}

pub async fn record_audit(
    pool: &SqlitePool,
    actor: &str,
    kind: &str,
    action: &str,
    target: Option<&str>,
    detail: Option<&str>,
    client_ip: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO audit_events (id, at_ms, actor, kind, action, target, detail, client_ip) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(now)
    .bind(actor)
    .bind(kind)
    .bind(action)
    .bind(target)
    .bind(detail)
    .bind(client_ip)
    .execute(pool)
    .await?;
    let _ = sqlx::query("DELETE FROM audit_events WHERE at_ms < ?")
        .bind(now - 90 * 86_400_000)
        .execute(pool)
        .await;
    Ok(())
}

pub async fn list_audit(pool: &SqlitePool, limit: i64) -> Result<Vec<AuditEvent>> {
    let rows = sqlx::query(
        "SELECT id, at_ms, actor, kind, action, target, detail, client_ip FROM audit_events ORDER BY at_ms DESC LIMIT ?",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AuditEvent {
            id: row.get("id"),
            at_ms: row.get("at_ms"),
            actor: row.get("actor"),
            kind: row.get("kind"),
            action: row.get("action"),
            target: row.get("target"),
            detail: row.get("detail"),
            client_ip: row.get("client_ip"),
        })
        .collect())
}
