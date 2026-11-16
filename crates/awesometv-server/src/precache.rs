use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::Config;
use crate::db;

/// Copies (or transcode-proxies) media that is about to air onto local disk so
/// the producer does not have to read it from a slow network mount in real
/// time. Entries are scheduled from the channel guide and bounded by a byte
/// budget with LRU eviction.
pub struct Precache {
    pool: SqlitePool,
    dir: PathBuf,
    log_dir: PathBuf,
    max_bytes: i64,
    ahead_ms: i64,
    proxy_kbps: i64,
    proxy_height: i64,
    copy_bytes_per_sec: i64,
    enabled: bool,
    running: AtomicBool,
    paused: AtomicBool,
    shutdown: AtomicBool,
}

impl Precache {
    pub fn new(config: &Config, pool: SqlitePool) -> Self {
        let _ = std::fs::create_dir_all(&config.precache_dir);
        let _ = std::fs::create_dir_all(config.data_dir.join("logs"));
        Self {
            pool,
            dir: config.precache_dir.clone(),
            log_dir: config.data_dir.join("logs"),
            max_bytes: config.precache_max_gb.max(0) * 1_000_000_000,
            ahead_ms: config.precache_ahead_hours.max(1) * 3_600_000,
            proxy_kbps: config.precache_proxy_kbps.clamp(500, 20_000),
            proxy_height: config.precache_proxy_height.clamp(240, 2160),
            copy_bytes_per_sec: config.precache_copy_mbps.max(1) * 1_000_000,
            enabled: config.precache_enabled,
            running: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn start(self: &Arc<Self>) {
        if !self.enabled {
            tracing::info!("precache disabled");
            return;
        }
        self.cleanup_partials();
        tracing::info!(enabled = self.enabled, "precache service starting");
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = db::reset_running_precache(&service.pool).await {
                tracing::warn!(%error, "precache: could not reset running entries");
            }
            loop {
                if service.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                tracing::debug!("precache tick start");
                if let Err(error) = service.tick().await {
                    tracing::warn!(%error, "precache tick failed");
                }
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
        });
    }

    pub async fn remove(&self, media_id: &str) -> Result<()> {
        if let Some(path) = db::delete_precache(&self.pool, media_id).await? {
            let _ = tokio::fs::remove_file(&path).await;
            let _ = tokio::fs::remove_file(format!("{path}.part")).await;
        }
        Ok(())
    }

    fn cleanup_partials(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".part") {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    async fn tick(self: &Arc<Self>) -> Result<()> {
        // Give metadata lookups priority over the bulk copy for a moment; the
        // copy loop yields while this is set.
        self.paused.store(true, Ordering::Relaxed);
        let result = self.tick_inner().await;
        self.paused.store(false, Ordering::Relaxed);
        result
    }

    async fn tick_inner(self: &Arc<Self>) -> Result<()> {
        let now = db::now_ms();
        let stale = db::prune_precache_queue(&self.pool, now).await?;
        if stale > 0 {
            tracing::debug!(stale, "precache dropped expired queue entries");
        }
        let trimmed = db::trim_precache_queue(&self.pool, 32).await?;
        if trimmed > 0 {
            tracing::debug!(trimmed, "precache trimmed the far-future queue");
        }
        let rows = db::upcoming_media(&self.pool, now, now + self.ahead_ms).await?;
        tracing::debug!(rows = rows.len(), "precache schedule loaded");
        let keep: HashSet<String> = rows.iter().map(|row| row.media_id.clone()).collect();

        // Keep the first occurrence of each media item; later rows are useful
        // for deciding whether there is a feasible airing to prepare for.
        let mut groups: Vec<Vec<db::UpcomingMedia>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in rows {
            if let Some(position) = index.get(&row.media_id).copied() {
                groups[position].push(row);
            } else {
                index.insert(row.media_id.clone(), groups.len());
                groups.push(vec![row]);
            }
        }

        let statuses = db::precache_statuses(&self.pool).await?;
        let active = statuses
            .values()
            .filter(|(status, _)| status == "queued" || status == "running")
            .count();
        // Keep the queue focused; the worker is a single reader on a slow mount.
        let mut started = if active >= 32 { 8 } else { 0 };
        for group in groups {
            if started >= 8 {
                break;
            }
            let Some(first) = group.first() else { continue };
            if first.path.starts_with("cloud://") {
                continue;
            }
            // Avoid touching the slow source mount unless there is work to do.
            if let Some((status, updated_ms)) = statuses.get(&first.media_id) {
                if matches!(status.as_str(), "ready" | "running" | "queued") {
                    continue;
                }
                if status == "failed" && now - updated_ms < 3_600_000 {
                    continue;
                }
            }
            let (size, mtime) = if first.size_bytes > 0 {
                (first.size_bytes, 0)
            } else {
                let Ok(Ok(meta)) =
                    tokio::time::timeout(Duration::from_secs(8), tokio::fs::metadata(&first.path))
                        .await
                else {
                    tracing::warn!(path = %first.path, "precache source lookup timed out");
                    continue;
                };
                let size = meta.len() as i64;
                if size <= 0 {
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                let _ = db::set_media_size(&self.pool, &first.media_id, size).await;
                (size, mtime)
            };

            let mode = self.mode_for(size, first.duration_ms);
            let estimate_bytes = if mode == "copy" {
                size
            } else {
                self.proxy_bytes(first.duration_ms)
            };
            if estimate_bytes <= 0 || estimate_bytes > self.max_bytes.max(1) {
                continue;
            }
            let estimate_ms = (self.estimate_seconds(size, mode) * 1000.0) as i64 + 120_000;
            let Some(start) = group
                .iter()
                .find(|row| row.start_ms >= now + estimate_ms)
                .map(|row| row.start_ms)
            else {
                // Nothing in the window can finish in time; leave it uncached
                // rather than blocking feasible jobs.
                continue;
            };
            if !self.budget_allows(estimate_bytes, &keep).await? {
                continue;
            }
            let cache_path = self.cache_path_for(&first.media_id, mode, &first.path);
            let deadline = start - estimate_ms;
            db::upsert_precache(
                &self.pool,
                &first.media_id,
                &first.path,
                size,
                mtime,
                &cache_path,
                mode,
                start,
                deadline,
            )
            .await?;
            started += 1;
            tracing::debug!(
                media = %first.media_id,
                mode,
                size,
                start,
                "precache queued"
            );
        }

        tracing::debug!(started, "precache scan complete");
        self.spawn_worker();
        Ok(())
    }

    fn spawn_worker(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.run_jobs().await {
                tracing::warn!(%error, "precache worker stopped");
            }
            service.running.store(false, Ordering::SeqCst);
        });
    }

    /// Do not compete with a live producer that is still reading the source
    /// mount directly; let it finish, then fill the cache.
    fn direct_reader_active(&self) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .chars()
                .all(|character| character.is_ascii_digit())
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let command = bytes
                .split(|byte| *byte == 0)
                .map(|part| String::from_utf8_lossy(part))
                .collect::<Vec<_>>()
                .join(" ");
            let media_path = command.contains("/media/") && !command.contains("/precache/");
            let is_live =
                command.contains("ffmpeg") && command.contains("-i /media/") && media_path;
            let is_probe = command.contains("ffprobe") && media_path;
            if is_live || is_probe {
                return true;
            }
        }
        false
    }

    async fn run_jobs(&self) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            if self.direct_reader_active() {
                tokio::time::sleep(Duration::from_secs(20)).await;
                continue;
            }
            let Some(entry) = db::next_queued_precache(&self.pool, db::now_ms()).await? else {
                return Ok(());
            };
            db::set_precache_status(&self.pool, &entry.media_id, "running", entry.bytes, None)
                .await?;
            match self.process(&entry).await {
                Ok(bytes) => {
                    db::set_precache_status(&self.pool, &entry.media_id, "ready", bytes, None)
                        .await?;
                    tracing::info!(media = %entry.media_id, mode = %entry.mode, bytes, "precache ready");
                }
                Err(error) => {
                    if let Some(path) = &entry.cache_path {
                        let _ = tokio::fs::remove_file(format!("{path}.part")).await;
                    }
                    let message = format!("{error:#}");
                    if message.contains("preempted by live playback") {
                        db::set_precache_status(&self.pool, &entry.media_id, "queued", 0, None)
                            .await?;
                        tracing::info!(media = %entry.media_id, "precache yielded to live playback");
                    } else {
                        db::set_precache_status(
                            &self.pool,
                            &entry.media_id,
                            "failed",
                            0,
                            Some(&message),
                        )
                        .await?;
                        tracing::warn!(media = %entry.media_id, %error, "precache failed");
                    }
                }
            }
        }
    }

    async fn process(&self, entry: &db::PrecacheEntry) -> Result<i64> {
        let cache_path = entry
            .cache_path
            .as_deref()
            .context("precache entry has no cache path")?;
        let partial = format!("{cache_path}.part");
        let _ = tokio::fs::remove_file(&partial).await;
        if entry.mode == "copy" {
            self.copy_source(entry, &partial).await?;
        } else {
            self.transcode_source(entry, &partial).await?;
        }
        let bytes = tokio::fs::metadata(&partial).await?.len() as i64;
        if bytes <= 0 {
            bail!("precache produced an empty file");
        }
        tokio::fs::rename(&partial, cache_path).await?;
        Ok(bytes)
    }

    async fn copy_source(&self, entry: &db::PrecacheEntry, partial: &str) -> Result<()> {
        let mut input = tokio::fs::File::open(&entry.source_path).await?;
        let mut output = tokio::fs::File::create(partial).await?;
        let mut buffer = vec![0u8; 4 * 1024 * 1024];
        let mut total = 0i64;
        let mut next_report = 64i64 * 1024 * 1024;
        loop {
            while self.paused.load(Ordering::Relaxed) && !self.shutdown.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let live = self.direct_reader_active();
            let read = input.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read]).await?;
            total += read as i64;
            if live {
                // Share the slow mount with the live producer instead of
                // stopping the cache completely.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            if total >= next_report {
                db::set_precache_progress(&self.pool, &entry.media_id, total).await?;
                next_report += 64 * 1024 * 1024;
            }
        }
        output.flush().await?;
        if total != entry.source_size {
            bail!(
                "copied {total} of {} bytes from {}",
                entry.source_size,
                entry.source_path
            );
        }
        Ok(())
    }

    async fn transcode_source(&self, entry: &db::PrecacheEntry, partial: &str) -> Result<()> {
        let mut filter = format!("scale=-2:{}", self.proxy_height);
        if self.is_hdr(&entry.source_path).await.unwrap_or(false) {
            filter.push_str(
                ",zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p",
            );
        }
        let kbps = self.proxy_kbps;
        let log_path = self.log_dir.join("precache.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut child = tokio::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-y",
                "-i",
                &source_input(&entry.source_path),
                "-map",
                "0:v:0",
                "-map",
                "0:a:0?",
                "-vf",
                &filter,
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-b:v",
                &format!("{kbps}k"),
                "-maxrate",
                &format!("{kbps}k"),
                "-bufsize",
                &format!("{}k", kbps.saturating_mul(2)),
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                partial,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()?;
        loop {
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    if !status.success() {
                        bail!("ffmpeg exited with {status}");
                    }
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {
                    if self.direct_reader_active() {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        bail!("preempted by live playback");
                    }
                }
            }
        }
    }

    async fn is_hdr(&self, path: &str) -> Result<bool> {
        let output = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=color_transfer",
                "-of",
                "default=nw=1:nk=1",
                &source_input(path),
            ])
            .output()
            .await?;
        let transfer = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_ascii_lowercase();
        Ok(matches!(transfer.as_str(), "smpte2084" | "arib-std-b67"))
    }

    fn mode_for(&self, size: i64, duration_ms: i64) -> &'static str {
        choose_mode(size, duration_ms, self.copy_bytes_per_sec)
    }

    fn estimate_seconds(&self, size: i64, mode: &str) -> f64 {
        let read_seconds = size as f64 / self.copy_bytes_per_sec as f64;
        if mode == "copy" {
            read_seconds
        } else {
            read_seconds * 1.1
        }
    }

    fn proxy_bytes(&self, duration_ms: i64) -> i64 {
        let seconds = duration_ms.max(0) as f64 / 1000.0;
        (seconds * self.proxy_kbps as f64 * 1000.0 / 8.0) as i64
    }

    fn cache_path_for(&self, media_id: &str, mode: &str, source: &str) -> String {
        let extension = std::path::Path::new(source)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin");
        let name = if mode == "proxy" {
            format!("{media_id}.proxy.mp4")
        } else {
            format!("{media_id}.{extension}")
        };
        self.dir.join(name).to_string_lossy().to_string()
    }

    async fn budget_allows(&self, estimate: i64, keep: &HashSet<String>) -> Result<bool> {
        if self.max_bytes <= 0 {
            return Ok(true);
        }
        let mut sizes = db::ready_precache_sizes(&self.pool).await?;
        let mut total: i64 = sizes.iter().map(|(_, bytes, _)| *bytes).sum();
        if total + estimate <= self.max_bytes {
            return Ok(true);
        }
        sizes.sort_by_key(|(_, _, last_used)| *last_used);
        let now = db::now_ms();
        for (media_id, bytes, last_used) in sizes {
            if total + estimate <= self.max_bytes {
                break;
            }
            if keep.contains(&media_id) || now - last_used < 3 * 3_600_000 {
                continue;
            }
            if let Some(path) = db::delete_precache(&self.pool, &media_id).await? {
                let _ = tokio::fs::remove_file(path).await;
                total -= bytes;
                tracing::info!(media = %media_id, bytes, "precache evicted");
            }
        }
        Ok(total + estimate <= self.max_bytes)
    }
}

fn source_input(path: &str) -> String {
    if std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("iso"))
    {
        format!("bluray:{path}")
    } else {
        path.to_string()
    }
}

fn choose_mode(size: i64, duration_ms: i64, copy_bytes_per_sec: i64) -> &'static str {
    if duration_ms <= 0 {
        return "copy";
    }
    let bytes_per_sec = size as f64 * 1000.0 / duration_ms as f64;
    if bytes_per_sec <= copy_bytes_per_sec as f64 * 0.75 {
        "copy"
    } else {
        "proxy"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_copy_for_low_bitrate_sources() {
        // 10 Mbps over 1 hour with a 6 MB/s copy path.
        let size = 10_000_000 * 3600 / 8;
        assert_eq!(choose_mode(size, 3_600_000, 6_000_000), "copy");
    }

    #[test]
    fn chooses_proxy_for_4k_remux() {
        // 80 Mbps over 2 hours cannot be copied in real time.
        let size = 80_000_000 * 7200 / 8;
        assert_eq!(choose_mode(size, 7_200_000, 6_000_000), "proxy");
    }
}
