use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use super::google::RemoteEntry;
use super::provider::{ProviderHead, ProviderInfo, ProviderRegistry};
use super::secret::SecretKey;
use crate::db;
use crate::scan;

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "ts", "mpg", "mpeg", "webm", "wmv", "flv",
];
const MUSIC_EXTENSIONS: &[&str] = &["flac", "mp3", "m4a", "aac", "ogg", "opus", "wav"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];
const SIDECAR_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt", "nfo"];

#[derive(Default)]
struct ScanCancel {
    cancelled: AtomicBool,
    publish: Mutex<()>,
}

/// Cancellation prevents new cache commits without waiting for an admitted one.
/// An admitted commit may complete after cancellation; its cache-job permit is
/// retained until all of that job's work has actually exited.
#[derive(Clone, Default)]
pub struct ResolutionTicket {
    cancelled: Arc<AtomicBool>,
    gate: Arc<Mutex<()>>,
}

impl ResolutionTicket {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    async fn admit(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.gate.lock().await;
        if self.cancelled.load(Ordering::SeqCst) {
            None
        } else {
            Some(guard)
        }
    }
}

pub(crate) async fn admitted_blocking<F, T>(admission: Arc<Semaphore>, work: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = admission.acquire_owned().await?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await?)
}

struct CachedToken {
    token: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub location: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudScanReport {
    pub scanned: usize,
    pub probed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub removed: usize,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudScanJob {
    pub id: String,
    pub source_id: String,
    pub source_name: String,
    pub status: String,
    pub full: bool,
    pub limit: usize,
    pub scanned: usize,
    pub probed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub removed: usize,
    pub complete: bool,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub message: Option<String>,
}

pub struct CloudManager {
    pool: SqlitePool,
    data_dir: PathBuf,
    secret: Arc<SecretKey>,
    providers: ProviderRegistry,
    internal_base: String,
    tokens: Mutex<HashMap<String, CachedToken>>,
    scan_jobs: Mutex<HashMap<String, CloudScanJob>>,
    scan_cancels: Mutex<HashMap<String, Arc<ScanCancel>>>,
    blocking_admission: Arc<Semaphore>,
    cache_admission: Arc<Semaphore>,
    cache_budget_bytes: i64,
    auto_cache_max_bytes: i64,
}

impl CloudManager {
    pub fn new(pool: SqlitePool, data_dir: PathBuf, bind: &str, port: u16) -> Result<Self> {
        Self::with_providers(pool, data_dir, bind, port, ProviderRegistry::new()?)
    }

    pub(crate) fn with_providers(
        pool: SqlitePool,
        data_dir: PathBuf,
        bind: &str,
        port: u16,
        providers: ProviderRegistry,
    ) -> Result<Self> {
        let secret = Arc::new(SecretKey::load(&data_dir)?);
        let cache_budget_gb = std::env::var("CLOUD_CACHE_MAX_GB")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(500)
            .max(1);
        let auto_cache_mb = std::env::var("CLOUD_AUTO_CACHE_MAX_MB")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(2048)
            .max(1);
        Ok(Self {
            pool,
            data_dir,
            secret,
            providers,
            internal_base: format!(
                "http://{}:{port}",
                if bind == "0.0.0.0" || bind == "::" {
                    "127.0.0.1"
                } else {
                    bind
                }
            ),
            tokens: Mutex::new(HashMap::new()),
            scan_jobs: Mutex::new(HashMap::new()),
            scan_cancels: Mutex::new(HashMap::new()),
            blocking_admission: Arc::new(Semaphore::new(4)),
            cache_admission: Arc::new(Semaphore::new(4)),
            cache_budget_bytes: cache_budget_gb * 1024 * 1024 * 1024,
            auto_cache_max_bytes: auto_cache_mb * 1024 * 1024,
        })
    }

    pub(crate) fn validate_source(&self, input: &db::CloudSourceInput) -> Result<()> {
        if input.name.trim().is_empty() {
            bail!("cloud source name is required");
        }
        if !matches!(input.policy.as_str(), "auto" | "cache_all" | "stream_all") {
            bail!("invalid cloud policy");
        }
        if !input.config.is_null()
            && !input
                .config
                .as_object()
                .is_some_and(|config| config.is_empty())
        {
            bail!("config must be an empty object");
        }
        let secret = input.secret.as_deref().unwrap_or("").trim();
        if secret.is_empty() {
            bail!("cloud source secret is required");
        }
        self.providers
            .require(&input.provider)?
            .validate_secret(&input.auth_type, secret)
            .map_err(|_| anyhow::anyhow!("secret: invalid credentials or token_uri"))?;
        for root in &input.roots {
            Self::validate_root(&input.provider, root)?;
        }
        Ok(())
    }

    pub(crate) fn validate_root(provider: &str, root: &db::CloudRootInput) -> Result<()> {
        if root.remote_id.trim().is_empty() || root.remote_id.chars().any(char::is_control) {
            bail!("root remote_id is required");
        }
        if provider == "google"
            && (root.remote_id.contains('/') || root.remote_id == "." || root.remote_id == "..")
        {
            bail!("invalid Google folder ID");
        }
        for (field, path) in [
            ("remote_path", &root.remote_path),
            ("display_path", &root.display_path),
        ] {
            if path.trim().is_empty()
                || path.chars().any(char::is_control)
                || path.split('/').any(|part| part == "..")
            {
                bail!("invalid root {field}");
            }
        }
        Ok(())
    }

    pub async fn create_source(&self, input: &db::CloudSourceInput) -> Result<db::CloudSource> {
        self.validate_source(input)?;
        let secret = input.secret.as_deref().unwrap_or("").trim();
        let encrypted = self.secret.encrypt(secret.as_bytes())?;
        db::create_cloud_source(&self.pool, input, &encrypted).await
    }

    pub async fn delete_source(&self, id: &str) -> Result<()> {
        let cache_prefix = self.data_dir.join("cloud-cache").join(id);
        let _ = tokio::fs::remove_dir_all(cache_prefix).await;
        db::delete_cloud_source(&self.pool, id).await
    }

    /// Rewrite secrets that were encrypted with a previous key. Enable this by
    /// setting the new primary `AWESOMETV_SECRET_KEY` and the old key in
    /// `AWESOMETV_SECRET_KEY_PREVIOUS`, then restarting once.
    pub async fn rotate_secrets(&self) -> Result<usize> {
        let mut rotated = 0;
        for (id, blob) in db::cloud_secret_blobs(&self.pool).await? {
            match self.secret.rotate(&blob) {
                Ok(Some(updated)) => {
                    db::update_cloud_secret_blob(&self.pool, &id, &updated).await?;
                    rotated += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, source = %id, "cloud secret could not be re-encrypted");
                }
            }
        }
        Ok(rotated)
    }

    pub async fn list_sources(&self) -> Result<Vec<db::CloudSource>> {
        db::list_cloud_sources(&self.pool).await
    }

    pub async fn update_source(
        &self,
        id: &str,
        update: &db::CloudSourceUpdate,
    ) -> Result<db::CloudSource> {
        db::update_cloud_source(&self.pool, id, update).await
    }

    pub async fn metrics(&self) -> Result<db::CloudMetrics> {
        db::cloud_metrics(&self.pool).await
    }

    pub async fn browse(&self, source_id: &str, folder_id: &str) -> Result<Vec<RemoteEntry>> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        let token = self.access_token(&source).await?;
        let provider = self.providers.require(&source.provider)?;
        let (entries, _) = provider.list_folder(&token, folder_id, None)?;
        Ok(entries)
    }

    pub async fn shared_drives(&self, source_id: &str) -> Result<Vec<super::google::DriveEntry>> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        let token = self.access_token(&source).await?;
        let provider = self.providers.require(&source.provider)?;
        provider.list_drives(&token)
    }

    pub async fn add_root(
        &self,
        source_id: &str,
        root: &db::CloudRootInput,
    ) -> Result<db::CloudRoot> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        Self::validate_root(&source.provider, root)?;
        db::add_cloud_root(&self.pool, source_id, root).await
    }

    pub async fn start_scan(
        self: &Arc<Self>,
        source_id: &str,
        limit: usize,
        full: bool,
    ) -> Result<CloudScanJob> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        if !source.enabled {
            bail!("cloud source is disabled");
        }
        if !db::list_cloud_roots(&self.pool, source_id)
            .await?
            .iter()
            .any(|root| root.enabled)
        {
            bail!("add at least one cloud folder root before scanning");
        }
        self.prune_scan_jobs().await;
        let id = Uuid::new_v4().to_string();
        let job = CloudScanJob {
            id: id.clone(),
            source_id: source_id.to_string(),
            source_name: source.name,
            status: "running".into(),
            full,
            limit,
            scanned: 0,
            probed: 0,
            skipped: 0,
            errors: 0,
            removed: 0,
            complete: false,
            started_at: db::now_ms(),
            finished_at: None,
            message: None,
        };
        {
            let mut jobs = self.scan_jobs.lock().await;
            if jobs
                .values()
                .any(|job| job.source_id == source_id && job.status == "running")
            {
                bail!("cloud scan already running");
            }
            jobs.insert(id.clone(), job.clone());
        }
        let cancel = Arc::new(ScanCancel::default());
        self.scan_cancels
            .lock()
            .await
            .insert(id.clone(), cancel.clone());
        let manager = self.clone();
        let source_id = source_id.to_string();
        let job_id = id.clone();
        tokio::spawn(async move {
            let result = manager
                .scan_source_controlled(
                    &source_id,
                    limit,
                    full,
                    Some(&job_id),
                    Some(cancel.clone()),
                )
                .await;
            {
                let _publication = cancel.publish.lock().await;
                if !cancel.cancelled.load(Ordering::SeqCst)
                    && let Some(message) = result
                        .as_ref()
                        .err()
                        .map(|_| "remote scan failed".to_string())
                {
                    let _ = sqlx::query("UPDATE cloud_sources SET last_error = ? WHERE id = ?")
                        .bind(&message)
                        .bind(&source_id)
                        .execute(&manager.pool)
                        .await;
                }
            }
            let mut jobs = manager.scan_jobs.lock().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.finished_at = Some(db::now_ms());
                if cancel.cancelled.load(Ordering::SeqCst) {
                    job.status = "cancelled".into();
                    job.complete = false;
                    job.message = Some("scan cancelled".into());
                } else {
                    match result {
                        Ok(report) => {
                            job.scanned = report.scanned;
                            job.probed = report.probed;
                            job.skipped = report.skipped;
                            job.errors = report.errors;
                            job.removed = report.removed;
                            job.complete = report.complete;
                            job.status = if report.complete {
                                "completed"
                            } else {
                                "failed"
                            }
                            .into();
                            if !report.complete {
                                job.message =
                                    Some("scan incomplete; stale media was not reconciled".into());
                            }
                        }
                        Err(_) => {
                            let message = "remote scan failed".to_string();
                            let _ =
                                sqlx::query("UPDATE cloud_sources SET last_error = ? WHERE id = ?")
                                    .bind(&message)
                                    .bind(&source_id)
                                    .execute(&manager.pool)
                                    .await;
                            job.status = "failed".into();
                            job.complete = false;
                            job.message = Some(message);
                        }
                    }
                }
            }
            manager.scan_cancels.lock().await.remove(&job_id);
        });
        Ok(job)
    }

    pub async fn scan_jobs(&self) -> Vec<CloudScanJob> {
        let mut jobs: Vec<CloudScanJob> = self.scan_jobs.lock().await.values().cloned().collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.started_at));
        jobs
    }

    pub async fn scan_job(&self, id: &str) -> Option<CloudScanJob> {
        self.scan_jobs.lock().await.get(id).cloned()
    }

    pub async fn cancel_scan(&self, id: &str) -> Option<CloudScanJob> {
        let job = self.scan_jobs.lock().await.get(id).cloned()?;
        if job.status == "running"
            && let Some(cancel) = self.scan_cancels.lock().await.get(id)
        {
            cancel.cancelled.store(true, Ordering::SeqCst);
            let _publication = cancel.publish.lock().await;
        }
        Some(job)
    }

    async fn prune_scan_jobs(&self) {
        let cutoff = db::now_ms() - 3_600_000;
        let mut jobs = self.scan_jobs.lock().await;
        jobs.retain(|_, job| {
            job.status == "running" || job.finished_at.is_some_and(|at| at >= cutoff)
        });
    }

    async fn update_scan_job(&self, job_id: Option<&str>, report: &CloudScanReport) {
        let Some(job_id) = job_id else {
            return;
        };
        if let Some(job) = self.scan_jobs.lock().await.get_mut(job_id) {
            job.scanned = report.scanned;
            job.probed = report.probed;
            job.skipped = report.skipped;
            job.errors = report.errors;
            job.removed = report.removed;
        }
    }

    async fn scan_source_controlled(
        &self,
        source_id: &str,
        limit: usize,
        full: bool,
        job_id: Option<&str>,
        cancel: Option<Arc<ScanCancel>>,
    ) -> Result<CloudScanReport> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        if !source.enabled {
            bail!("cloud source is disabled");
        }
        let roots: Vec<db::CloudRoot> = db::list_cloud_roots(&self.pool, source_id)
            .await?
            .into_iter()
            .filter(|root| root.enabled)
            .collect();
        if roots.is_empty() {
            bail!("add at least one enabled cloud folder root before scanning");
        }
        let token = self.access_token(&source).await?;
        let provider = self.providers.require(&source.provider)?;
        let limit = limit.clamp(1, 50_000);
        let mut staged = Vec::new();
        let mut pages = 0usize;
        let mut complete = true;
        let mut report = CloudScanReport {
            scanned: 0,
            probed: 0,
            skipped: 0,
            errors: 0,
            removed: 0,
            complete: true,
        };
        let mut queue: Vec<(String, String)> = roots
            .iter()
            .map(|root| (root.remote_id.clone(), root.display_path.clone()))
            .collect();
        let mut seen_folders = HashSet::new();
        let mut progress_tick = 0usize;
        while let Some((folder_id, folder_path)) = queue.pop() {
            if cancelled(&cancel) {
                bail!("scan cancelled");
            }
            if report.scanned >= limit {
                complete = false;
                break;
            }
            if !seen_folders.insert(folder_id.clone()) {
                continue;
            }
            let mut page = None;
            let mut seen_pages = HashSet::new();
            loop {
                if cancelled(&cancel) {
                    bail!("scan cancelled");
                }
                pages += 1;
                if pages > 10_000 {
                    complete = false;
                    break;
                }
                let mut attempt = 0;
                let listing = loop {
                    attempt += 1;
                    let provider = provider.clone();
                    let token = token.clone();
                    let folder_id = folder_id.clone();
                    let page = page.clone();
                    let result = match tokio::time::timeout(
                        std::time::Duration::from_secs(65),
                        tokio::task::spawn_blocking(move || {
                            provider.list_folder(&token, &folder_id, page.as_deref())
                        }),
                    )
                    .await
                    {
                        Ok(Ok(result)) => result,
                        _ => Err(anyhow::anyhow!("cloud listing timed out")),
                    };
                    if result.is_ok()
                        || result
                            .as_ref()
                            .err()
                            .is_some_and(|e| e.is::<crate::cloud::provider::AuthenticationError>())
                        || attempt == 3
                    {
                        break result;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
                };
                let (entries, next) = match listing {
                    Ok(value) => value,
                    Err(_) => {
                        report.errors += 1;
                        tracing::warn!("cloud folder listing failed");
                        break;
                    }
                };
                for entry in entries {
                    if cancelled(&cancel) {
                        bail!("scan cancelled");
                    }
                    if report.scanned >= limit {
                        complete = false;
                        break;
                    }
                    if entry.is_folder {
                        queue.push((entry.id.clone(), format!("{folder_path}/{}", entry.name)));
                        continue;
                    }
                    let Some(kind) = kind_for(&entry.name, &entry.mime_type) else {
                        report.skipped += 1;
                        continue;
                    };
                    let media_id = cloud_media_id(source_id, &entry.id);
                    let path = format!("cloud://{source_id}/{}", entry.id);
                    let title = cloud_title(&entry.name);
                    let probe = if kind == "subtitle" || kind == "sidecar" {
                        None
                    } else {
                        let url = self.proxy_url(source_id, &entry.id, 3600);
                        let kind_for_probe = kind.clone();
                        match tokio::time::timeout(
                            Duration::from_secs(65),
                            tokio::task::spawn_blocking(move || {
                                scan::probe_target(&url, &kind_for_probe)
                            }),
                        )
                        .await
                        {
                            Ok(Ok(Ok(probe))) => {
                                report.probed += 1;
                                Some(probe)
                            }
                            Ok(Ok(Err(error))) => {
                                report.errors += 1;
                                let _ = error;
                                tracing::warn!("cloud probe failed");
                                None
                            }
                            _ => {
                                report.errors += 1;
                                tracing::warn!("cloud probe task failed or timed out");
                                None
                            }
                        }
                    };
                    let duration_ms = probe.as_ref().map(|probe| probe.duration_ms).unwrap_or(0);
                    let media = db::Media {
                        id: media_id.clone(),
                        path: path.clone(),
                        title,
                        kind: kind.clone(),
                        duration_ms,
                        width: probe.as_ref().and_then(|probe| probe.width),
                        height: probe.as_ref().and_then(|probe| probe.height),
                        video_codec: probe.as_ref().and_then(|probe| probe.video_codec.clone()),
                        audio_codec: probe.as_ref().and_then(|probe| probe.audio_codec.clone()),
                        size_bytes: None,
                        chapters: Vec::new(),
                    };
                    let remote_path = format!("{folder_path}/{}", entry.name);
                    staged.push((
                        media,
                        db::NewCloudMedia {
                            media_id,
                            source_id: source_id.to_string(),
                            remote_id: entry.id,
                            remote_path,
                            size_bytes: entry.size,
                            mime_type: Some(entry.mime_type),
                            etag: entry.md5,
                            remote_modified_at: entry.modified_at,
                        },
                        probe.is_some(),
                    ));
                    report.scanned += 1;
                    progress_tick += 1;
                    if progress_tick.is_multiple_of(10) {
                        self.update_scan_job(job_id, &report).await;
                    }
                }
                if report.scanned >= limit {
                    // A continuation or queued folder means the inventory is truncated.
                    complete = false;
                    break;
                }
                if next.is_none() {
                    break;
                }
                if let Some(ref token) = next {
                    if !seen_pages.insert(token.clone()) {
                        complete = false;
                        break;
                    }
                }
                page = next;
            }
        }
        self.update_scan_job(job_id, &report).await;
        report.complete = complete && report.errors == 0;
        if cancelled(&cancel) {
            bail!("scan cancelled");
        }
        let _publication = match &cancel {
            Some(cancel) => Some(cancel.publish.lock().await),
            None => None,
        };
        if cancelled(&cancel) {
            bail!("scan cancelled");
        }
        if report.complete {
            // Full scans currently retain stale entries: deletions need a single atomic
            // reconciliation across media, collections, channels and cache bookkeeping.
            let _ = full;
            self.commit_scan(&staged, source_id, &cancel).await?;
        }
        let error = if report.complete {
            None
        } else {
            Some("scan incomplete; stale media was not reconciled")
        };
        if error.is_some() {
            sqlx::query("UPDATE cloud_sources SET last_error = ? WHERE id = ?")
                .bind(error)
                .bind(source_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(report)
    }

    async fn commit_scan(
        &self,
        staged: &[(db::Media, db::NewCloudMedia, bool)],
        source_id: &str,
        cancel: &Option<Arc<ScanCancel>>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (media, remote, probed) in staged {
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,width,height,video_codec,audio_codec,size_bytes,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(path) DO UPDATE SET title=excluded.title,kind=excluded.kind,duration_ms=excluded.duration_ms,width=excluded.width,height=excluded.height,video_codec=excluded.video_codec,audio_codec=excluded.audio_codec,size_bytes=excluded.size_bytes")
                .bind(&media.id).bind(&media.path).bind(&media.title).bind(&media.kind).bind(media.duration_ms).bind(media.width).bind(media.height).bind(&media.video_codec).bind(&media.audio_codec).bind(media.size_bytes).bind(db::now_ms()).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO cloud_media (media_id,source_id,remote_id,remote_path,size_bytes,mime_type,etag,remote_modified_at,cache_state,last_seen_at) VALUES (?,?,?,?,?,?,?,?,'none',?) ON CONFLICT(media_id) DO UPDATE SET remote_path=excluded.remote_path,size_bytes=excluded.size_bytes,mime_type=excluded.mime_type,etag=excluded.etag,remote_modified_at=excluded.remote_modified_at,last_seen_at=excluded.last_seen_at")
                .bind(&remote.media_id).bind(&remote.source_id).bind(&remote.remote_id).bind(&remote.remote_path).bind(remote.size_bytes).bind(&remote.mime_type).bind(&remote.etag).bind(remote.remote_modified_at).bind(db::now_ms()).execute(&mut *tx).await?;
            if *probed {
                sqlx::query("UPDATE cloud_media SET last_probe_at = ? WHERE media_id = ?")
                    .bind(db::now_ms())
                    .bind(&remote.media_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        sqlx::query("UPDATE cloud_sources SET last_sync_at = ?, last_error = NULL WHERE id = ?")
            .bind(db::now_ms())
            .bind(source_id)
            .execute(&mut *tx)
            .await?;
        if cancelled(cancel) {
            bail!("scan cancelled");
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn head_proxy(&self, source_id: &str, remote_id: &str) -> Result<ProviderHead> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        if !source.enabled {
            bail!("cloud source is disabled");
        }
        let token = self.access_token(&source).await?;
        let provider = self.providers.require(&source.provider)?;
        provider.head(&token, remote_id)
    }

    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.providers.list()
    }

    pub fn supports_provider(&self, id: &str) -> bool {
        self.providers.get(id).is_some()
    }

    pub(crate) async fn is_file(&self, path: PathBuf) -> Result<bool> {
        admitted_blocking(self.blocking_admission.clone(), move || path.is_file()).await
    }

    pub async fn resolve_media_input(self: &Arc<Self>, media_id: &str) -> Result<ResolvedMedia> {
        self.resolve_media_input_with_ticket(media_id, None).await
    }

    pub async fn resolve_media_input_with_ticket(
        self: &Arc<Self>,
        media_id: &str,
        ticket: Option<ResolutionTicket>,
    ) -> Result<ResolvedMedia> {
        self.resolve_media_input_inner(media_id, ticket).await
    }

    pub async fn resolve_without_precache(
        self: &Arc<Self>,
        media_id: &str,
        ticket: Option<ResolutionTicket>,
    ) -> Result<ResolvedMedia> {
        self.resolve_media_input_inner(media_id, ticket).await
    }

    async fn resolve_media_input_inner(
        self: &Arc<Self>,
        media_id: &str,
        ticket: Option<ResolutionTicket>,
    ) -> Result<ResolvedMedia> {
        let kind = db::media_kind(&self.pool, media_id)
            .await?
            .unwrap_or_else(|| "other".into());
        // Schedule precache paths require playback leases; generic consumers bypass them.
        let Some(cloud) = db::cloud_media(&self.pool, media_id).await? else {
            let path = db::media_path(&self.pool, media_id)
                .await?
                .context("media item is missing from the catalog")?;
            return Ok(ResolvedMedia {
                location: path,
                kind,
            });
        };
        if let Some(cache_path) = cloud.cache_path.as_deref()
            && self.is_file(PathBuf::from(cache_path)).await?
        {
            let _ = db::touch_cloud_media(&self.pool, media_id).await;
            return Ok(ResolvedMedia {
                location: cache_path.to_string(),
                kind,
            });
        }
        let source = db::cloud_source(&self.pool, &cloud.source_id)
            .await?
            .context("cloud source not found")?;
        if !source.enabled {
            bail!("cloud source is disabled");
        }
        let should_cache = match source.policy.as_str() {
            "cache_all" => true,
            "auto" => cloud
                .size_bytes
                .map(|size| size <= self.auto_cache_max_bytes)
                .unwrap_or(false),
            _ => false,
        };
        if should_cache {
            let manager = self.clone();
            let source_id = cloud.source_id.clone();
            let media_id = cloud.media_id.clone();
            let remote_id = cloud.remote_id.clone();
            let extension = Path::new(&cloud.remote_path)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("bin")
                .to_string();
            if let Ok(permit) = self.cache_admission.clone().try_acquire_owned() {
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = manager
                        .cache_media_with_ticket(
                            &source_id,
                            &media_id,
                            &remote_id,
                            &extension,
                            ticket.as_ref(),
                        )
                        .await
                    {
                        tracing::warn!(%error, %media_id, "cloud cache failed");
                    }
                });
            }
        }
        Ok(ResolvedMedia {
            location: self.proxy_url(&cloud.source_id, &cloud.remote_id, 3600),
            kind,
        })
    }

    pub fn sign(&self, data: &[u8]) -> String {
        self.secret.sign(data)
    }

    pub fn verify(&self, data: &[u8], signature: &str) -> bool {
        self.secret.verify(data, signature)
    }

    pub fn proxy_url(&self, source_id: &str, remote_id: &str, expires_in: i64) -> String {
        let expires = time::OffsetDateTime::now_utc().unix_timestamp() + expires_in;
        let payload = format!("{source_id}:{remote_id}:{expires}");
        let signature = self.secret.sign(payload.as_bytes());
        format!(
            "{}/cloud/{}/{}?expires={}&sig={}",
            self.internal_base, source_id, remote_id, expires, signature
        )
    }

    pub fn verify_proxy(
        &self,
        source_id: &str,
        remote_id: &str,
        expires: i64,
        signature: &str,
    ) -> bool {
        if expires < time::OffsetDateTime::now_utc().unix_timestamp() {
            return false;
        }
        let payload = format!("{source_id}:{remote_id}:{expires}");
        self.secret.verify(payload.as_bytes(), signature)
    }

    pub async fn open_proxy(
        &self,
        source_id: &str,
        remote_id: &str,
        range: Option<&str>,
    ) -> Result<ureq::Response> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        if !source.enabled {
            bail!("cloud source is disabled");
        }
        let token = self.access_token(&source).await?;
        let provider = self.providers.require(&source.provider)?;
        provider.download(&token, remote_id, range)
    }

    #[cfg(test)]
    pub async fn cache_media(
        &self,
        source_id: &str,
        media_id: &str,
        remote_id: &str,
        extension: &str,
    ) -> Result<()> {
        self.cache_media_with_ticket(source_id, media_id, remote_id, extension, None)
            .await
    }

    async fn cache_media_with_ticket(
        &self,
        source_id: &str,
        media_id: &str,
        remote_id: &str,
        extension: &str,
        ticket: Option<&ResolutionTicket>,
    ) -> Result<()> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        if !source.enabled {
            bail!("cloud source is disabled");
        }
        let token = self.access_token(&source).await?;
        let provider = self.providers.require(&source.provider)?;
        let remote_id = remote_id.to_owned();
        let dir = self.data_dir.join("cloud-cache").join(source_id);
        tokio::fs::create_dir_all(&dir).await?;
        let target = dir.join(format!("{media_id}.{extension}"));
        let temp = dir.join(format!("{media_id}.{}.part", Uuid::new_v4()));
        let temp_for_write = temp.clone();
        let permit = self.blocking_admission.clone().acquire_owned().await?;
        let bytes = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut reader = provider.download(&token, &remote_id, None)?.into_reader();
            use std::io::Write;
            let mut file = std::fs::File::create(&temp_for_write)?;
            let copied = std::io::copy(&mut reader, &mut file)?;
            file.flush()?;
            Ok::<u64, anyhow::Error>(copied)
        })
        .await
        .map_err(|error| anyhow::anyhow!("cache copy task failed: {error}"))??;
        publish_cache(&self.pool, media_id, &temp, &target, bytes, ticket).await?;
        // Eviction is separately admitted, so cancellation also prevents late
        // bookkeeping after a completed publication.
        let _guard = if let Some(ticket) = ticket {
            match ticket.admit().await {
                Some(guard) => Some(guard),
                None => return Ok(()),
            }
        } else {
            None
        };
        self.evict_cache().await?;
        Ok(())
    }

    pub async fn evict_cache(&self) -> Result<()> {
        let entries = db::cached_cloud_media(&self.pool).await?;
        let mut total: i64 = entries.iter().map(|(_, _, bytes)| *bytes).sum();
        for (media_id, path, bytes) in entries {
            if total <= self.cache_budget_bytes {
                break;
            }
            let _ = tokio::fs::remove_file(&path).await;
            db::set_cloud_media_cache(&self.pool, &media_id, "none", None, None).await?;
            total -= bytes;
        }
        Ok(())
    }

    async fn access_token(&self, source: &db::CloudSource) -> Result<String> {
        {
            let tokens = self.tokens.lock().await;
            if let Some(cached) = tokens.get(&source.id)
                && cached.expires_at > Instant::now()
            {
                return Ok(cached.token.clone());
            }
        }
        let encrypted = db::cloud_source_secret(&self.pool, &source.id)
            .await?
            .context("cloud source secret is missing")?;
        let plaintext = self.secret.decrypt(&encrypted)?;
        let secret = String::from_utf8(plaintext).context("cloud secret is not UTF-8")?;
        let provider = self.providers.require(&source.provider)?;
        let auth_type = source.auth_type.clone();
        let permit = self.blocking_admission.clone().acquire_owned().await?;
        let token = tokio::time::timeout(
            Duration::from_secs(65),
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                provider.access_token(&auth_type, &secret)
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("cloud authentication timed out"))???;
        let mut tokens = self.tokens.lock().await;
        tokens.insert(
            source.id.clone(),
            CachedToken {
                token: token.clone(),
                expires_at: Instant::now() + Duration::from_secs(3300),
            },
        );
        Ok(token)
    }
}

pub(crate) async fn publish_cache(
    pool: &SqlitePool,
    media_id: &str,
    temp: &Path,
    target: &Path,
    bytes: u64,
    ticket: Option<&ResolutionTicket>,
) -> Result<()> {
    let _guard = if let Some(ticket) = ticket {
        match ticket.admit().await {
            Some(guard) => Some(guard),
            None => {
                tokio::fs::remove_file(temp).await?;
                bail!("resolution cancelled before cache publication");
            }
        }
    } else {
        None
    };
    publish_cache_admitted(pool, media_id, temp, target, bytes).await
}

async fn publish_cache_admitted(
    pool: &SqlitePool,
    media_id: &str,
    temp: &Path,
    target: &Path,
    bytes: u64,
) -> Result<()> {
    tokio::fs::rename(temp, target).await?;
    db::set_cloud_media_cache(
        pool,
        media_id,
        "cached",
        Some(target.to_string_lossy().as_ref()),
        Some(bytes as i64),
    )
    .await
}

fn cloud_media_id(source_id: &str, remote_id: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("cloud://{source_id}/{remote_id}").as_bytes(),
    )
    .to_string()
}

fn cancelled(cancel: &Option<Arc<ScanCancel>>) -> bool {
    cancel
        .as_ref()
        .is_some_and(|cancel| cancel.cancelled.load(Ordering::SeqCst))
}

fn kind_for(name: &str, mime_type: &str) -> Option<String> {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if VIDEO_EXTENSIONS.contains(&extension.as_str()) || mime_type.starts_with("video/") {
        if name.to_ascii_uppercase().contains("S") && name.to_ascii_uppercase().contains("E") {
            Some("episode".into())
        } else {
            Some("movie".into())
        }
    } else if MUSIC_EXTENSIONS.contains(&extension.as_str()) || mime_type.starts_with("audio/") {
        Some("music".into())
    } else if IMAGE_EXTENSIONS.contains(&extension.as_str()) || mime_type.starts_with("image/") {
        Some("image".into())
    } else if SIDECAR_EXTENSIONS.contains(&extension.as_str()) {
        if extension == "srt" || extension == "ass" || extension == "ssa" || extension == "vtt" {
            Some("subtitle".into())
        } else {
            Some("sidecar".into())
        }
    } else {
        None
    }
}

fn cloud_title(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    stem.replace(['.', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generic_resolution_bypasses_schedule_precache() {
        let dir = std::env::temp_dir().join(format!("precache-bypass-{}", Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        let source = dir.join("original.mp4");
        let cached = dir.join("cached.mp4");
        std::fs::write(&source, b"original").unwrap();
        std::fs::write(&cached, b"cached").unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('item',?,'item','movie',1000,0)")
            .bind(source.to_str().unwrap()).execute(&pool).await.unwrap();
        db::upsert_precache(
            &pool,
            "item",
            source.to_str().unwrap(),
            8,
            1,
            cached.to_str().unwrap(),
            "copy",
            1,
            1,
        )
        .await
        .unwrap();
        db::set_precache_status(&pool, "item", "ready", 6, None)
            .await
            .unwrap();
        let manager =
            Arc::new(CloudManager::new(pool.clone(), dir.clone(), "127.0.0.1", 8421).unwrap());
        assert_eq!(
            manager.resolve_media_input("item").await.unwrap().location,
            source.to_str().unwrap()
        );
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn resolution_ticket_publication() {
        let dir = std::env::temp_dir().join(format!("resolution-ticket-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.join("test.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("CREATE TABLE cloud_media (media_id TEXT PRIMARY KEY, cache_state TEXT, cache_path TEXT, cached_bytes INTEGER, last_access_at INTEGER)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO cloud_media VALUES ('item', 'none', NULL, NULL, NULL)")
            .execute(&pool)
            .await
            .unwrap();
        let temp = dir.join("item.part");
        let target = dir.join("item.mp4");
        tokio::fs::write(&target, b"old").await.unwrap();
        tokio::fs::write(&temp, b"new").await.unwrap();
        let ticket = ResolutionTicket::new();
        ticket.cancel();
        assert!(
            publish_cache(&pool, "item", &temp, &target, 3, Some(&ticket))
                .await
                .is_err()
        );
        assert!(!temp.exists());
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"old");
        let row: (String, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as("SELECT cache_state, cache_path, cached_bytes, last_access_at FROM cloud_media WHERE media_id = 'item'").fetch_one(&pool).await.unwrap();
        assert_eq!(row, ("none".into(), None, None, None));

        tokio::fs::write(&temp, b"new").await.unwrap();
        let ticket = ResolutionTicket::new();
        let guard = ticket.admit().await.unwrap();
        let cancel_ticket = ticket.clone();
        let cancellation = tokio::spawn(async move { cancel_ticket.cancel() });
        tokio::time::timeout(Duration::from_millis(200), cancellation)
            .await
            .expect("cancellation must not wait for admitted publication")
            .unwrap();
        publish_cache_admitted(&pool, "item", &temp, &target, 3)
            .await
            .unwrap();
        drop(guard);
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"new");
        let row: (String, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT cache_state, cache_path, cached_bytes FROM cloud_media WHERE media_id = 'item'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "cached".into(),
                Some(target.to_string_lossy().into_owned()),
                Some(3)
            )
        );
        assert!(ticket.admit().await.is_none());
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn classifies_media() {
        assert_eq!(
            kind_for("movie.mkv", "video/x-matroska").as_deref(),
            Some("movie")
        );
        assert_eq!(
            kind_for("track.flac", "audio/flac").as_deref(),
            Some("music")
        );
        assert_eq!(
            kind_for("poster.png", "image/png").as_deref(),
            Some("image")
        );
        assert_eq!(
            kind_for("subs.srt", "text/plain").as_deref(),
            Some("subtitle")
        );
        assert_eq!(kind_for("notes.txt", "text/plain"), None);
    }

    #[test]
    fn builds_cloud_titles() {
        assert_eq!(cloud_title("My.Movie.2024.mkv"), "My Movie 2024");
    }

    use crate::cloud::google::{DriveEntry, RemoteEntry};
    use crate::cloud::provider::conformance::MockProvider;
    use crate::cloud::provider::{AuthenticationError, CloudProvider, ProviderCapabilities};
    use std::sync::atomic::AtomicUsize;

    struct FaultProvider {
        inner: MockProvider,
        mode: AtomicUsize,
        calls: AtomicUsize,
    }
    impl CloudProvider for FaultProvider {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            self.inner.capabilities()
        }
        fn validate_secret(&self, a: &str, s: &str) -> Result<()> {
            self.inner.validate_secret(a, s)
        }
        fn access_token(&self, a: &str, s: &str) -> Result<String> {
            self.inner.access_token(a, s)
        }
        fn list_folder(
            &self,
            t: &str,
            f: &str,
            p: Option<&str>,
        ) -> Result<(Vec<RemoteEntry>, Option<String>)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            while self.mode.load(Ordering::SeqCst) == 3 {
                std::thread::park_timeout(std::time::Duration::from_millis(5));
            }
            match self.mode.load(Ordering::SeqCst) {
                1 => Err(AuthenticationError.into()),
                2 if p.is_some() => Err(anyhow::anyhow!("simulated later page timeout secret")),
                2 => {
                    let (entries, _) = self.inner.list_folder(t, f, p)?;
                    Ok((entries, Some("next".into())))
                }
                _ => self.inner.list_folder(t, f, p),
            }
        }
        fn list_drives(&self, t: &str) -> Result<Vec<DriveEntry>> {
            self.inner.list_drives(t)
        }
        fn download(&self, t: &str, r: &str, range: Option<&str>) -> Result<ureq::Response> {
            self.inner.download(t, r, range)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_failures_preserve_last_success_and_recover() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mode = Arc::new(AtomicUsize::new(0));
        let server_mode = mode.clone();
        let server = std::thread::spawn(move || {
            loop {
                if server_mode.load(Ordering::SeqCst) == 9 {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]);
                        let current = server_mode.load(Ordering::SeqCst);
                        if current == 2 {
                            std::thread::sleep(Duration::from_millis(180));
                            continue;
                        }
                        let later = request.contains("pageToken=");
                        let fail = current == 1 || (current == 3 && later);
                        let (status, body) = if fail {
                            ("401 Unauthorized", "sentinel-credential")
                        } else if current == 3 {
                            (
                                "200 OK",
                                r#"{"files":[{"id":"new","name":"new.srt","mimeType":"text/plain"}],"nextPageToken":"next"}"#,
                            )
                        } else {
                            (
                                "200 OK",
                                r#"{"files":[{"id":"old","name":"old.srt","mimeType":"text/plain"}]}"#,
                            )
                        };
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let dir = std::env::temp_dir().join(format!("awesometv-http-{}", Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        let provider = Arc::new(crate::cloud::google::GoogleClient::for_test(
            base,
            Duration::from_millis(60),
        ));
        let manager = CloudManager::with_providers(
            pool.clone(),
            dir,
            "127.0.0.1",
            8421,
            ProviderRegistry::from_providers(vec![provider]),
        )
        .unwrap();
        let source = manager
            .create_source(&db::CloudSourceInput {
                provider: "google".into(),
                name: "HTTP".into(),
                auth_type: "access_token".into(),
                policy: "auto".into(),
                config: serde_json::json!({}),
                secret: Some("sentinel-credential".into()),
                roots: vec![],
            })
            .await
            .unwrap();
        manager
            .add_root(
                &source.id,
                &db::CloudRootInput {
                    remote_id: "root".into(),
                    remote_path: "/".into(),
                    display_path: "/".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            manager
                .scan_source_controlled(&source.id, 100, true, None, None)
                .await
                .unwrap()
                .complete
        );
        let before = db::list_media(&pool).await.unwrap();
        assert_eq!(before.len(), 1);
        let success = db::cloud_source(&pool, &source.id)
            .await
            .unwrap()
            .unwrap()
            .last_sync_at;
        for fault in [1, 2, 3] {
            mode.store(fault, Ordering::SeqCst);
            assert!(
                !manager
                    .scan_source_controlled(&source.id, 100, true, None, None)
                    .await
                    .unwrap()
                    .complete
            );
            let after = db::list_media(&pool).await.unwrap();
            assert_eq!(after.len(), 1);
            assert_eq!(after[0].id, before[0].id);
            let state = db::cloud_source(&pool, &source.id).await.unwrap().unwrap();
            assert_eq!(state.last_sync_at, success);
            assert!(!state.last_error.unwrap().contains("sentinel-credential"));
        }
        mode.store(0, Ordering::SeqCst);
        assert!(
            manager
                .scan_source_controlled(&source.id, 100, true, None, None)
                .await
                .unwrap()
                .complete
        );
        assert!(
            db::cloud_source(&pool, &source.id)
                .await
                .unwrap()
                .unwrap()
                .last_error
                .is_none()
        );
        mode.store(9, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn mock_provider_lifecycle_conformance() {
        let dir = std::env::temp_dir().join(format!("awesometv-cloud-test-{}", Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        let provider = Arc::new(FaultProvider {
            inner: MockProvider::media_tree(),
            mode: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let registry = ProviderRegistry::from_providers(vec![provider.clone()]);
        let manager = Arc::new(
            CloudManager::with_providers(pool.clone(), dir.clone(), "127.0.0.1", 8421, registry)
                .unwrap(),
        );
        let source = manager
            .create_source(&db::CloudSourceInput {
                provider: "mock".into(),
                name: "Mock".into(),
                auth_type: "access_token".into(),
                policy: "auto".into(),
                config: serde_json::json!({}),
                secret: Some("mock".into()),
                roots: Vec::new(),
            })
            .await
            .unwrap();
        manager
            .add_root(
                &source.id,
                &db::CloudRootInput {
                    remote_id: "root".into(),
                    remote_path: "/".into(),
                    display_path: "/".into(),
                },
            )
            .await
            .unwrap();
        let report = manager
            .scan_source_controlled(&source.id, 100, false, None, None)
            .await
            .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.errors, 0);
        assert!(report.complete);
        let before = db::list_media(&pool).await.unwrap();
        let success_at = db::cloud_source(&pool, &source.id)
            .await
            .unwrap()
            .unwrap()
            .last_sync_at;
        let limited = manager
            .scan_source_controlled(&source.id, 1, true, None, None)
            .await
            .unwrap();
        assert!(!limited.complete);
        assert_eq!(limited.removed, 0);
        assert_eq!(db::list_media(&pool).await.unwrap().len(), before.len());
        let status = db::cloud_source(&pool, &source.id).await.unwrap().unwrap();
        assert_eq!(status.last_sync_at, success_at);
        assert!(status.last_error.is_some());
        for mode in [1, 2] {
            provider.mode.store(mode, Ordering::SeqCst);
            provider.calls.store(0, Ordering::SeqCst);
            let failed = manager
                .scan_source_controlled(&source.id, 100, true, None, None)
                .await
                .unwrap();
            assert!(!failed.complete);
            if mode == 1 {
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            }
            assert_eq!(db::list_media(&pool).await.unwrap().len(), before.len());
            let status = db::cloud_source(&pool, &source.id).await.unwrap().unwrap();
            assert_eq!(status.last_sync_at, success_at);
            assert!(!status.last_error.unwrap().contains("secret"));
        }
        provider.mode.store(0, Ordering::SeqCst);
        assert!(
            manager
                .scan_source_controlled(&source.id, 100, true, None, None)
                .await
                .unwrap()
                .complete
        );
        // A database failure after earlier staged writes must roll back all of them.
        sqlx::query("UPDATE media_items SET title = 'prior title' WHERE path LIKE '%sample-srt'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER fail_cloud_update BEFORE UPDATE ON cloud_media BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
            .execute(&pool).await.unwrap();
        assert!(
            manager
                .scan_source_controlled(&source.id, 100, true, None, None)
                .await
                .is_err()
        );
        let prior: String =
            sqlx::query_scalar("SELECT title FROM media_items WHERE path LIKE '%sample-srt'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(prior, "prior title");
        sqlx::query("DROP TRIGGER fail_cloud_update")
            .execute(&pool)
            .await
            .unwrap();

        provider.calls.store(0, Ordering::SeqCst);
        provider.mode.store(3, Ordering::SeqCst);
        let job = manager.start_scan(&source.id, 100, true).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while provider.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(manager.start_scan(&source.id, 100, true).await.is_err());
        provider.mode.store(0, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if manager.scan_job(&job.id).await.unwrap().status != "running" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.scan_job(&job.id).await.unwrap().status, "completed");
        // A detached blocking provider call may finish after cancellation, but cannot publish.
        provider.calls.store(0, Ordering::SeqCst);
        provider.mode.store(3, Ordering::SeqCst);
        let cancelled_job = manager.start_scan(&source.id, 100, true).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let before_cancel = db::cloud_source(&pool, &source.id)
            .await
            .unwrap()
            .unwrap()
            .last_sync_at;
        let before_title: String =
            sqlx::query_scalar("SELECT title FROM media_items WHERE path LIKE '%sample-srt'")
                .fetch_one(&pool)
                .await
                .unwrap();
        manager.cancel_scan(&cancelled_job.id).await.unwrap();
        provider.mode.store(0, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), async {
            while manager.scan_job(&cancelled_job.id).await.unwrap().status == "running" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager.scan_job(&cancelled_job.id).await.unwrap().status,
            "cancelled"
        );
        assert_eq!(
            db::cloud_source(&pool, &source.id)
                .await
                .unwrap()
                .unwrap()
                .last_sync_at,
            before_cancel
        );
        let after_title: String =
            sqlx::query_scalar("SELECT title FROM media_items WHERE path LIKE '%sample-srt'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after_title, before_title);
        let stale = Some(Arc::new(ScanCancel::default()));
        stale
            .as_ref()
            .unwrap()
            .cancelled
            .store(true, Ordering::SeqCst);
        assert!(manager.commit_scan(&[], &source.id, &stale).await.is_err());
        assert_eq!(
            db::cloud_source(&pool, &source.id)
                .await
                .unwrap()
                .unwrap()
                .last_sync_at,
            before_cancel
        );
        let next = manager.start_scan(&source.id, 100, true).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if manager.scan_job(&next.id).await.unwrap().status != "running" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager.scan_job(&next.id).await.unwrap().status,
            "completed"
        );

        let media = db::list_media(&pool)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.path.contains("sample-srt"))
            .expect("sample media indexed");
        manager
            .cache_media(&source.id, &media.id, "sample-srt", "srt")
            .await
            .unwrap();
        let resolved = manager.resolve_media_input(&media.id).await.unwrap();
        assert!(resolved.location.ends_with(".srt"));
        assert!(std::path::Path::new(&resolved.location).is_file());

        manager.delete_source(&source.id).await.unwrap();
        assert!(db::cloud_source(&pool, &source.id).await.unwrap().is_none());
        assert!(db::cloud_media(&pool, &media.id).await.unwrap().is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
