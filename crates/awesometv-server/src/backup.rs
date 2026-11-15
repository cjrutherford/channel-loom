use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;
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
        "awesometv-{:04}{:02}{:02}-{:02}{:02}{:02}.sqlite",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let path = dir.join(&name);
    let sql = format!(
        "VACUUM INTO '{}'",
        path.display().to_string().replace('\'', "''")
    );
    sqlx::query(&sql)
        .execute(pool)
        .await
        .context("create SQLite backup")?;
    let meta = tokio::fs::metadata(&path).await?;
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

pub async fn stage_restore(data_dir: &Path, name: &str) -> Result<()> {
    let source =
        backup_path(data_dir, name).ok_or_else(|| anyhow::anyhow!("invalid backup name"))?;
    if !source.is_file() {
        anyhow::bail!("backup not found: {name}");
    }
    let pending = data_dir.join("restore.pending");
    tokio::fs::copy(&source, &pending).await?;
    Ok(())
}
