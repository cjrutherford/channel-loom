use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub size: u64,
    pub created_ms: i64,
}

pub async fn create(pool: &SqlitePool, data_dir: &Path) -> Result<BackupInfo> {
    let dir = data_dir.join("backups");
    tokio::fs::create_dir_all(&dir).await?;
    let now = OffsetDateTime::now_utc();
    let name = format!(
        "awesometv-{:04}{:02}{:02}-{:02}{:02}{:02}-{}.sqlite",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        uuid::Uuid::new_v4()
    );
    let final_path = dir.join(&name);
    let path = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    private_empty_file(&path)?;
    let result: Result<()> = async {
        let sql = format!(
            "VACUUM INTO '{}'",
            path.display().to_string().replace('\'', "''")
        );
        sqlx::query(&sql)
            .execute(pool)
            .await
            .context("create SQLite backup")?;
        let snapshot = open_snapshot(&path).await?;
        sqlx::query("DROP TABLE IF EXISTS awesometv_backup_metadata")
            .execute(&snapshot)
            .await?;
        sqlx::query("CREATE TABLE awesometv_backup_metadata (format_version INTEGER NOT NULL)")
            .execute(&snapshot)
            .await?;
        sqlx::query("INSERT INTO awesometv_backup_metadata VALUES (1)")
            .execute(&snapshot)
            .await?;
        snapshot.close().await;
        validate(&path).await?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&path, &final_path).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error.into());
    }
    let meta = tokio::fs::metadata(&final_path).await?;
    Ok(BackupInfo {
        name,
        size: meta.len(),
        created_ms: now.unix_timestamp() * 1000,
    })
}

pub async fn list(data_dir: &Path) -> Result<Vec<BackupInfo>> {
    let dir = data_dir.join("backups");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = tokio::fs::read_dir(&dir).await?;
    let mut backups = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sqlite") {
            continue;
        }
        let meta = entry.metadata().await?;
        let name = entry.file_name().to_string_lossy().to_string();
        let created_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        backups.push(BackupInfo {
            name,
            size: meta.len(),
            created_ms,
        });
    }
    backups.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(backups)
}

pub fn backup_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    Some(data_dir.join("backups").join(name))
}

fn private_empty_file(path: &Path) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    Ok(())
}

async fn open_snapshot(path: &Path) -> Result<SqlitePool> {
    Ok(SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await?)
}

/// Validate before staging and again before applying: a pending file can be damaged after staging.
pub async fn validate(path: &Path) -> Result<()> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path).read_only(true))
        .await
        .context("open backup")?;
    let result: Result<()> = async {
        let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&pool)
            .await?;
        anyhow::ensure!(integrity == ["ok"], "backup integrity check failed");
        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT format_version FROM awesometv_backup_metadata")
                .fetch_all(&pool)
                .await
                .context("missing backup format metadata")?;
        anyhow::ensure!(versions == [1], "unsupported backup format");
        for table in [
            "media_items",
            "app_settings",
            "channels",
            "channel_items",
            "generated_timeline_state",
            "cloud_sources",
        ] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?",
            )
            .bind(table)
            .fetch_one(&pool)
            .await?;
            anyhow::ensure!(exists == 1, "backup missing required table {table}");
        }
        Ok(())
    }
    .await;
    pool.close().await;
    result
}

pub async fn stage_restore(data_dir: &Path, name: &str) -> Result<()> {
    let source =
        backup_path(data_dir, name).ok_or_else(|| anyhow::anyhow!("invalid backup name"))?;
    anyhow::ensure!(source.is_file(), "backup not found: {name}");
    let pending = data_dir.join("restore.pending");
    let temporary = data_dir.join(format!(".restore-{}.tmp", uuid::Uuid::new_v4()));
    private_empty_file(&temporary)?;
    let result: Result<()> = async {
        // Copy through the already-private file: fs::copy can copy the source's
        // permissions and briefly expose the unencrypted snapshot.
        let mut input = tokio::fs::File::open(&source).await?;
        let mut output = tokio::fs::OpenOptions::new().write(true).open(&temporary).await?;
        tokio::io::copy(&mut input, &mut output).await?;
        drop(output);
        validate(&temporary).await?;
        tokio::fs::rename(&temporary, &pending).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn backup_rejects_corruption_without_replacing_pending() {
        let dir =
            std::env::temp_dir().join(format!("awesome-backup-test-{}", uuid::Uuid::new_v4()));
        let pool = crate::db::init(&dir).await.unwrap();
        let info = create(&pool, &dir).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("backups").join(&info.name), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        stage_restore(&dir, &info.name).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("restore.pending")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let original = tokio::fs::read(dir.join("restore.pending")).await.unwrap();
        tokio::fs::write(dir.join("backups/bad.sqlite"), b"truncated")
            .await
            .unwrap();
        assert!(stage_restore(&dir, "bad.sqlite").await.is_err());
        assert_eq!(
            tokio::fs::read(dir.join("restore.pending")).await.unwrap(),
            original
        );
        pool.close().await;
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }
    #[tokio::test]
    async fn incompatible_and_damaged_pending_do_not_replace_database() {
        let dir =
            std::env::temp_dir().join(format!("awesome-backup-test-{}", uuid::Uuid::new_v4()));
        let pool = crate::db::init(&dir).await.unwrap();
        let info = create(&pool, &dir).await.unwrap();
        let path = dir.join("backups").join(&info.name);
        let snapshot = open_snapshot(&path).await.unwrap();
        sqlx::query("UPDATE awesometv_backup_metadata SET format_version=999")
            .execute(&snapshot)
            .await
            .unwrap();
        snapshot.close().await;
        assert!(stage_restore(&dir, &info.name).await.is_err());
        pool.close().await;
        let before = tokio::fs::read(dir.join("awesometv.db")).await.unwrap();
        tokio::fs::write(dir.join("operator-config"), b"unchanged").await.unwrap();
        tokio::fs::write(dir.join("restore.pending"), b"interrupted")
            .await
            .unwrap();
        assert!(crate::db::init(&dir).await.is_err());
        assert_eq!(
            before,
            tokio::fs::read(dir.join("awesometv.db")).await.unwrap()
        );
        assert_eq!(tokio::fs::read(dir.join("operator-config")).await.unwrap(), b"unchanged");
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn wal_roundtrip_retains_state_and_encrypted_credentials() {
        let root =
            std::env::temp_dir().join(format!("awesome-backup-test-{}", uuid::Uuid::new_v4()));
        let source = root.join("source");
        let dest = root.join("dest");
        let pool = crate::db::init(&source).await.unwrap();
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO generated_timeline_state VALUES ('channel', 'v1', 123, '{\"cursor\":7}')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let key = crate::cloud::secret::SecretKey::from_bytes([42; 32]);
        let cipher = key.encrypt(b"fixture-credential").unwrap();
        sqlx::query("INSERT INTO cloud_sources (id, provider, name, auth_type, policy, enabled, secret_blob, config_json, created_at) VALUES ('s', 'google', 'fixture', 'access_token', 'auto', 1, ?, '{}', 0)").bind(&cipher).execute(&pool).await.unwrap();
        let info = create(&pool, &source).await.unwrap();
        let bytes = tokio::fs::read(source.join("backups").join(&info.name))
            .await
            .unwrap();
        assert!(
            !bytes
                .windows(b"fixture-credential".len())
                .any(|x| x == b"fixture-credential")
        );
        tokio::fs::create_dir_all(dest.join("backups"))
            .await
            .unwrap();
        tokio::fs::write(dest.join("backups").join(&info.name), bytes)
            .await
            .unwrap();
        stage_restore(&dest, &info.name).await.unwrap();
        let restored = crate::db::init(&dest).await.unwrap();
        let cursor: String = sqlx::query_scalar(
            "SELECT continuation_json FROM generated_timeline_state WHERE channel_id='channel'",
        )
        .fetch_one(&restored)
        .await
        .unwrap();
        assert_eq!(cursor, "{\"cursor\":7}");
        let encrypted: String =
            sqlx::query_scalar("SELECT secret_blob FROM cloud_sources WHERE id='s'")
                .fetch_one(&restored)
                .await
                .unwrap();
        assert_eq!(key.decrypt(&encrypted).unwrap(), b"fixture-credential");
        assert!(
            crate::cloud::secret::SecretKey::from_bytes([43; 32])
                .decrypt(&encrypted)
                .is_err()
        );
        // A restored database must itself be backuppable.
        create(&restored, &dest).await.unwrap();
        restored.close().await;
        pool.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
