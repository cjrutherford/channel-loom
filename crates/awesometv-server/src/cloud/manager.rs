use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::google::{GoogleClient, GoogleCredentials, RemoteEntry};
use super::secret::SecretKey;
use crate::db;
use crate::scan;

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "ts", "mpg", "mpeg", "webm", "wmv", "flv",
];
const MUSIC_EXTENSIONS: &[&str] = &["flac", "mp3", "m4a", "aac", "ogg", "opus", "wav"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];
const SIDECAR_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt", "nfo"];

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
}

pub struct CloudManager {
    pool: SqlitePool,
    data_dir: PathBuf,
    secret: Arc<SecretKey>,
    google: GoogleClient,
    internal_base: String,
    tokens: Mutex<HashMap<String, CachedToken>>,
    cache_budget_bytes: i64,
    auto_cache_max_bytes: i64,
}

impl CloudManager {
    pub fn new(pool: SqlitePool, data_dir: PathBuf, bind: &str, port: u16) -> Result<Self> {
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
            google: GoogleClient::new()?,
            internal_base: format!(
                "http://{}:{port}",
                if bind == "0.0.0.0" || bind == "::" {
                    "127.0.0.1"
                } else {
                    bind
                }
            ),
            tokens: Mutex::new(HashMap::new()),
            cache_budget_bytes: cache_budget_gb * 1024 * 1024 * 1024,
            auto_cache_max_bytes: auto_cache_mb * 1024 * 1024,
        })
    }

    pub async fn create_source(&self, input: &db::CloudSourceInput) -> Result<db::CloudSource> {
        if input.provider != "google" {
            bail!("only Google Drive is implemented in this milestone");
        }
        let secret = input.secret.as_deref().unwrap_or("").trim();
        if secret.is_empty() {
            bail!("cloud source secret is required");
        }
        validate_google_secret(&input.auth_type, secret)?;
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

    pub async fn browse(&self, source_id: &str, folder_id: &str) -> Result<Vec<RemoteEntry>> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        let token = self.access_token(&source).await?;
        let (entries, _) = self.google.list_folder(&token, folder_id, None)?;
        Ok(entries)
    }

    pub async fn shared_drives(&self, source_id: &str) -> Result<Vec<super::google::DriveEntry>> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        let token = self.access_token(&source).await?;
        self.google.list_shared_drives(&token)
    }

    pub async fn add_root(
        &self,
        source_id: &str,
        root: &db::CloudRootInput,
    ) -> Result<db::CloudRoot> {
        db::add_cloud_root(&self.pool, source_id, root).await
    }

    pub async fn scan_source(&self, source_id: &str, limit: usize) -> Result<CloudScanReport> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        let roots = db::list_cloud_roots(&self.pool, source_id).await?;
        if roots.is_empty() {
            bail!("add at least one cloud folder root before scanning");
        }
        let token = self.access_token(&source).await?;
        let limit = limit.clamp(1, 50_000);
        let mut report = CloudScanReport {
            scanned: 0,
            probed: 0,
            skipped: 0,
            errors: 0,
        };
        let mut queue: Vec<(String, String)> = roots
            .iter()
            .map(|root| (root.remote_id.clone(), root.display_path.clone()))
            .collect();
        let mut seen_folders = HashSet::new();
        while let Some((folder_id, folder_path)) = queue.pop() {
            if report.scanned >= limit {
                break;
            }
            if !seen_folders.insert(folder_id.clone()) {
                continue;
            }
            let mut page = None;
            loop {
                let (entries, next) =
                    match self.google.list_folder(&token, &folder_id, page.as_deref()) {
                        Ok(value) => value,
                        Err(error) => {
                            report.errors += 1;
                            tracing::warn!(%error, %folder_id, "cloud folder listing failed");
                            break;
                        }
                    };
                for entry in entries {
                    if report.scanned >= limit {
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
                        match tokio::task::spawn_blocking(move || {
                            scan::probe_target(&url, &kind_for_probe)
                        })
                        .await
                        {
                            Ok(Ok(probe)) => {
                                report.probed += 1;
                                Some(probe)
                            }
                            Ok(Err(error)) => {
                                report.errors += 1;
                                tracing::warn!(%error, path = %entry.name, "cloud probe failed");
                                None
                            }
                            Err(error) => {
                                report.errors += 1;
                                tracing::warn!(%error, path = %entry.name, "cloud probe task failed");
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
                    if let Err(error) = db::upsert_media(&self.pool, &media).await {
                        report.errors += 1;
                        tracing::warn!(%error, path = %entry.name, "cloud media upsert failed");
                        continue;
                    }
                    let remote_path = format!("{folder_path}/{}", entry.name);
                    if let Err(error) = db::upsert_cloud_media(
                        &self.pool,
                        &db::NewCloudMedia {
                            media_id: media_id.clone(),
                            source_id: source_id.to_string(),
                            remote_id: entry.id.clone(),
                            remote_path,
                            size_bytes: entry.size,
                            mime_type: Some(entry.mime_type.clone()),
                            etag: entry.md5.clone(),
                            remote_modified_at: entry.modified_at,
                        },
                    )
                    .await
                    {
                        report.errors += 1;
                        tracing::warn!(%error, path = %entry.name, "cloud media metadata failed");
                        continue;
                    }
                    if probe.is_some() {
                        let _ = db::set_cloud_media_probed(&self.pool, &media_id).await;
                    }
                    report.scanned += 1;
                }
                if report.scanned >= limit || next.is_none() {
                    break;
                }
                page = next;
            }
        }
        db::record_cloud_sync(&self.pool, source_id, None).await?;
        Ok(report)
    }

    pub async fn resolve_media_input(self: &Arc<Self>, media_id: &str) -> Result<ResolvedMedia> {
        let kind = db::media_kind(&self.pool, media_id)
            .await?
            .unwrap_or_else(|| "other".into());
        // Prefer a locally precached copy so playback does not depend on the
        // source mount's real-time read throughput.
        if let Some(path) = db::precache_ready_path(&self.pool, media_id).await? {
            if tokio::fs::metadata(&path).await.is_ok() {
                let _ = db::touch_precache(&self.pool, media_id).await;
                return Ok(ResolvedMedia {
                    location: path,
                    kind,
                });
            }
            let _ = db::set_precache_status(
                &self.pool,
                media_id,
                "failed",
                0,
                Some("cached file is missing"),
            )
            .await;
        }
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
            && Path::new(cache_path).is_file()
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
            tokio::spawn(async move {
                if let Err(error) = manager
                    .cache_media(&source_id, &media_id, &remote_id, &extension)
                    .await
                {
                    tracing::warn!(%error, %media_id, "cloud cache failed");
                }
            });
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
        let token = self.access_token(&source).await?;
        self.google.download(&token, remote_id, range)
    }

    pub async fn cache_media(
        &self,
        source_id: &str,
        media_id: &str,
        remote_id: &str,
        extension: &str,
    ) -> Result<()> {
        let source = db::cloud_source(&self.pool, source_id)
            .await?
            .context("cloud source not found")?;
        let token = self.access_token(&source).await?;
        let response = self.google.download(&token, remote_id, None)?;
        let dir = self.data_dir.join("cloud-cache").join(source_id);
        tokio::fs::create_dir_all(&dir).await?;
        let target = dir.join(format!("{media_id}.{extension}"));
        let temp = dir.join(format!("{media_id}.part"));
        let mut reader = response.into_reader();
        let temp_for_write = temp.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = std::fs::File::create(&temp_for_write)?;
            let copied = std::io::copy(&mut reader, &mut file)?;
            file.flush()?;
            Ok::<u64, std::io::Error>(copied)
        })
        .await
        .map_err(|error| anyhow::anyhow!("cache copy task failed: {error}"))??;
        tokio::fs::rename(&temp, &target).await?;
        db::set_cloud_media_cache(
            &self.pool,
            media_id,
            "cached",
            Some(target.to_string_lossy().as_ref()),
            Some(bytes as i64),
        )
        .await?;
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
        let credentials = parse_google_credentials(&source.auth_type, &secret)?;
        let token = self.google.access_token(&credentials)?;
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

fn cloud_media_id(source_id: &str, remote_id: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("cloud://{source_id}/{remote_id}").as_bytes(),
    )
    .to_string()
}

fn validate_google_secret(auth_type: &str, secret: &str) -> Result<()> {
    match auth_type {
        "service_account" => {
            let value: Value = serde_json::from_str(secret).context("service account JSON")?;
            for key in ["client_email", "private_key"] {
                if value.get(key).and_then(|value| value.as_str()).is_none() {
                    bail!("service account JSON is missing {key}");
                }
            }
        }
        "access_token" | "oauth" => {}
        other => bail!("unsupported Google auth type: {other}"),
    }
    Ok(())
}

fn parse_google_credentials(auth_type: &str, secret: &str) -> Result<GoogleCredentials> {
    match auth_type {
        "service_account" => {
            let value: Value = serde_json::from_str(secret).context("service account JSON")?;
            Ok(GoogleCredentials::ServiceAccount {
                client_email: value
                    .get("client_email")
                    .and_then(|value| value.as_str())
                    .context("service account client_email")?
                    .to_string(),
                private_key: value
                    .get("private_key")
                    .and_then(|value| value.as_str())
                    .context("service account private_key")?
                    .to_string(),
                token_uri: value
                    .get("token_uri")
                    .and_then(|value| value.as_str())
                    .map(|uri| super::google::validate_https_endpoint(uri, "token_uri"))
                    .transpose()?,
            })
        }
        "access_token" => Ok(GoogleCredentials::AccessToken {
            access_token: secret.trim().to_string(),
        }),
        "oauth" => {
            let value: Value = serde_json::from_str(secret).context("OAuth JSON")?;
            Ok(GoogleCredentials::OAuth {
                refresh_token: value
                    .get("refresh_token")
                    .and_then(|value| value.as_str())
                    .context("OAuth refresh_token")?
                    .to_string(),
                client_id: value
                    .get("client_id")
                    .and_then(|value| value.as_str())
                    .context("OAuth client_id")?
                    .to_string(),
                client_secret: value
                    .get("client_secret")
                    .and_then(|value| value.as_str())
                    .context("OAuth client_secret")?
                    .to_string(),
                token_uri: value
                    .get("token_uri")
                    .and_then(|value| value.as_str())
                    .map(|uri| super::google::validate_https_endpoint(uri, "token_uri"))
                    .transpose()?,
            })
        }
        other => bail!("unsupported Google auth type: {other}"),
    }
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
}
