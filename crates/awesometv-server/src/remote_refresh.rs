//! Bounded remote refresh coordination. Stale media is deliberately retained.
use crate::{cloud::CloudManager, config::Config, jellyfin};
use anyhow::{Result, bail};
use sqlx::SqlitePool;
use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, watch};

const LIMIT: usize = 20_000;
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const DEADLINE: Duration = Duration::from_secs(30 * 60);
static JELLYFIN: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
static EMBY: OnceLock<Arc<Mutex<()>>> = OnceLock::new();

#[allow(clippy::too_many_arguments)]
pub async fn sync_library(
    pool: &SqlitePool,
    kind: &str,
    url: String,
    key: String,
    movies: PathBuf,
    tv: PathBuf,
    music: PathBuf,
    limit: usize,
) -> Result<jellyfin::SyncReport> {
    sync_library_until(pool, kind, url, key, movies, tv, music, limit, None).await
}

#[allow(clippy::too_many_arguments)]
async fn sync_library_until(
    pool: &SqlitePool,
    kind: &str,
    url: String,
    key: String,
    movies: PathBuf,
    tv: PathBuf,
    music: PathBuf,
    limit: usize,
    mut stop: Option<watch::Receiver<bool>>,
) -> Result<jellyfin::SyncReport> {
    let lock = match kind {
        "jellyfin" => &JELLYFIN,
        "emby" => &EMBY,
        _ => bail!("unknown remote source"),
    };
    let guard = lock
        .get_or_init(|| Arc::new(Mutex::new(())))
        .clone()
        .try_lock_owned()
        .map_err(|_| anyhow::anyhow!("remote refresh already running"))?;
    let task = tokio::task::spawn_blocking(move || {
        let result = jellyfin::sync(&url, &key, &movies, &tv, &music, limit);
        (guard, result)
    });
    let listing = tokio::time::timeout(DEADLINE, task);
    let (_guard, result) = if let Some(ref mut stop) = stop {
        tokio::select! {
            biased;
            _ = stop.changed() => bail!("remote refresh cancelled"),
            result = listing => result.map_err(|_| anyhow::anyhow!("remote refresh timed out"))??,
        }
    } else {
        listing
            .await
            .map_err(|_| anyhow::anyhow!("remote refresh timed out"))??
    };
    let (report, media) = result?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = CancelOnDrop(cancelled.clone());
    let pool = pool.clone();
    let publication_stop = stop.clone();
    let mut publication = tokio::spawn(async move {
        let _guard = _guard;
        publish(&pool, &media, &cancelled, publication_stop.as_ref()).await
    });
    let result = if let Some(ref mut stop) = stop {
        tokio::select! {
            biased;
            _ = stop.changed() => {
                drop(cancellation);
                let _ = publication.await;
                bail!("remote refresh cancelled");
            }
            result = &mut publication => result,
        }
    } else {
        publication.await
    };
    result?.map_err(|_| anyhow::anyhow!("remote refresh commit failed"))?;
    Ok(report)
}

// The cancellation check is the publication linearization boundary. A transaction
// admitted here is allowed to commit, and shutdown waits for it to finish.
async fn publish(
    pool: &SqlitePool,
    media: &[crate::db::Media],
    cancelled: &AtomicBool,
    stop: Option<&watch::Receiver<bool>>,
) -> Result<()> {
    if cancelled.load(Ordering::SeqCst) || stop.is_some_and(|s| *s.borrow()) {
        bail!("remote refresh cancelled");
    }
    jellyfin::commit(pool, media).await
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub fn start(
    pool: SqlitePool,
    config: Config,
    cloud: Arc<CloudManager>,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop, mut rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        run_worker(&mut rx, |stop| {
            let pool = pool.clone();
            let config = config.clone();
            let cloud = cloud.clone();
            async move { refresh(&pool, &config, &cloud, &mut stop.clone()).await }
        })
        .await;
    });
    (stop, task)
}

async fn run_worker<F, Fut>(rx: &mut watch::Receiver<bool>, mut attempt: F)
where
    F: FnMut(watch::Receiver<bool>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut backoff = Duration::from_secs(5 * 60);
    loop {
        if *rx.borrow() {
            break;
        }
        let success = attempt(rx.clone()).await;
        if *rx.borrow() {
            break;
        }
        let delay = if success {
            backoff = Duration::from_secs(5 * 60);
            INTERVAL
        } else {
            let delay = backoff;
            backoff = (backoff * 2).min(INTERVAL);
            delay
        };
        tokio::select! { _ = rx.changed() => break, _ = tokio::time::sleep(delay) => {} }
    }
}

async fn refresh(
    pool: &SqlitePool,
    config: &Config,
    cloud: &Arc<CloudManager>,
    stop: &mut watch::Receiver<bool>,
) -> bool {
    let mut success = true;
    for (kind, url, key) in [
        (
            "jellyfin",
            config.jellyfin_url.clone(),
            std::env::var("JELLYFIN_API_KEY").unwrap_or_default(),
        ),
        ("emby", config.emby_url.clone(), config.emby_api_key.clone()),
    ] {
        if key.trim().is_empty() {
            continue;
        }
        if *stop.borrow() {
            return false;
        }
        let result = sync_library_until(
            pool,
            kind,
            url,
            key,
            config.movies_root.clone(),
            config.tv_root.clone(),
            config.music_root.clone(),
            LIMIT,
            Some(stop.clone()),
        )
        .await;
        if *stop.borrow() {
            return false;
        }
        if result.is_ok() {
            tracing::info!(source = kind, "remote refresh succeeded");
        } else {
            success = false;
            tracing::warn!(source = kind, "remote refresh failed");
        }
    }
    let sources = match cloud.list_sources().await {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!("cloud source listing failed");
            return false;
        }
    };
    for source in sources.into_iter().filter(|s| s.enabled) {
        let roots = match crate::db::list_cloud_roots(pool, &source.id).await {
            Ok(roots) => roots,
            Err(_) => {
                success = false;
                tracing::warn!("cloud root listing failed");
                continue;
            }
        };
        if !roots.iter().any(|root| root.enabled) {
            continue;
        }
        let job = match cloud.start_scan(&source.id, LIMIT, true).await {
            Ok(job) => job,
            Err(_) => {
                success = false;
                tracing::warn!("cloud refresh could not start");
                continue;
            }
        };
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            tokio::select! {
                _ = stop.changed() => { cloud.cancel_scan(&job.id).await; return false; }
                _ = tokio::time::sleep_until(deadline) => { cloud.cancel_scan(&job.id).await; success = false; tracing::warn!("cloud refresh timed out"); break; }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {
                    if let Some(current) = cloud.scan_job(&job.id).await {
                        if current.status != "running" { success &= current.status == "completed" && current.complete; break; }
                    } else { success = false; break; }
                }
            }
        }
    }
    success
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn cancellation_at_pending_publication_preserves_last_good_and_recovers() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE media_items (id TEXT, path TEXT UNIQUE, title TEXT, kind TEXT, duration_ms INTEGER, width INTEGER, height INTEGER, video_codec TEXT, audio_codec TEXT, size_bytes INTEGER, created_at INTEGER)").execute(&pool).await.unwrap();
        let item = |title: &str| crate::db::Media {
            id: "id".into(),
            path: "path".into(),
            title: title.into(),
            kind: "movie".into(),
            duration_ms: 1,
            width: None,
            height: None,
            video_codec: None,
            audio_codec: None,
            size_bytes: None,
            chapters: vec![],
        };
        jellyfin::commit(&pool, &[item("last good")]).await.unwrap();
        let (stop, rx) = watch::channel(false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let pending_pool = pool.clone();
        let pending_cancelled = cancelled.clone();
        let pending = tokio::spawn(async move {
            publish(
                &pending_pool,
                &[item("staged")],
                &pending_cancelled,
                Some(&rx),
            )
            .await
        });
        stop.send(true).unwrap();
        assert!(pending.await.unwrap().is_err());
        let title: String = sqlx::query_scalar("SELECT title FROM media_items")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "last good");
        publish(&pool, &[item("recovered")], &cancelled, None)
            .await
            .unwrap();
        let title: String = sqlx::query_scalar("SELECT title FROM media_items")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "recovered");
    }

    #[tokio::test]
    async fn provider_lock_excludes_overlap_and_recovers() {
        let lock = JELLYFIN.get_or_init(|| Arc::new(Mutex::new(()))).clone();
        let guard = lock.clone().lock_owned().await;
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let result = sync_library(
            &pool,
            "jellyfin",
            String::new(),
            String::new(),
            PathBuf::new(),
            PathBuf::new(),
            PathBuf::new(),
            1,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("already running"));
        let other = EMBY.get_or_init(|| Arc::new(Mutex::new(()))).clone();
        assert!(other.try_lock_owned().is_ok());
        drop(guard);
        assert!(lock.try_lock_owned().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn worker_cadence_backoff_and_shutdown() {
        let (stop, mut rx) = watch::channel(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let task = tokio::spawn(async move {
            run_worker(&mut rx, move |_| {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                async move { n == 0 || n >= 10 }
            })
            .await;
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(INTERVAL - Duration::from_secs(1)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        for (i, minutes) in [5, 10, 20, 40, 80, 160, 320, 360, 360]
            .into_iter()
            .enumerate()
        {
            tokio::time::advance(Duration::from_secs(minutes * 60 - 1)).await;
            assert_eq!(calls.load(Ordering::SeqCst), i + 2);
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(calls.load(Ordering::SeqCst), i + 3);
        }
        tokio::time::advance(INTERVAL - Duration::from_secs(1)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 11);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 12);
        stop.send(true).unwrap();
        task.await.unwrap();
        tokio::time::advance(INTERVAL).await;
        assert_eq!(calls.load(Ordering::SeqCst), 12);
    }
}
