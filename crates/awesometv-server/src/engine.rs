use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::cloud::CloudManager;
use crate::db::{Channel, ChannelItem, current_item, media_path, next_item_start, now_ms};

const FPS: &str = "30000/1001";

pub struct Engine {
    pool: SqlitePool,
    data_dir: PathBuf,
    runners: Arc<Mutex<HashMap<String, Runner>>>,
    supervisors: Arc<Mutex<HashSet<String>>>,
    private_runtimes: Arc<Mutex<HashMap<String, PrivateRuntime>>>,
    shutdown: Arc<AtomicBool>,
    initial_burst: bool,
    started_at: Instant,
    catchups: Arc<Mutex<HashMap<String, CatchupSession>>>,
    dvr: Arc<Mutex<HashMap<String, DvrSession>>>,
    cloud: Arc<CloudManager>,
    max_kbps: i64,
    precache: Option<Arc<crate::precache::Precache>>,
}

#[derive(Default)]
pub(super) struct PublishedSnapshot {
    segments: Vec<private_publisher::SegmentIdentity>,
    new_published_seconds: f64,
}

type SnapshotHandle = Arc<std::sync::Mutex<PublishedSnapshot>>;

#[derive(serde::Serialize)]
pub struct PlayoutStatus {
    pub channel_id: String,
    pub generation: Option<String>,
    pub state: &'static str,
    pub new_published_seconds: f64,
    pub segments: Vec<PlayoutSegment>,
}

#[derive(serde::Serialize)]
pub struct PlayoutSegment {
    pub sequence: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub start_90k: String,
    pub end_90k: String,
    pub source: &'static str,
    pub media_id: Option<String>,
    pub title: Option<String>,
    pub fallback_reason: Option<String>,
    pub scheduled_programme: Option<private_publisher::ScheduledProgramme>,
}

struct PrivateRuntime {
    generation: String,
    snapshot: SnapshotHandle,
    ready: Arc<AtomicBool>,
    failed_at_ms: Arc<AtomicI64>,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    last_access_ms: i64,
    channel_number: String,
    channel_name: String,
    playlist: PathBuf,
}

struct CatchupSession {
    channel_id: String,
    child: Child,
    playlist: PathBuf,
    dir: PathBuf,
    expires_ms: i64,
}

struct DvrSession {
    channel_id: String,
    dir: PathBuf,
    playlist: PathBuf,
    stop: Arc<AtomicBool>,
    last_access: Arc<AtomicI64>,
}

struct DvrSegment {
    file: String,
    duration_ms: i64,
    discontinuity: bool,
}

struct DvrState {
    segments: Vec<DvrSegment>,
    media_sequence: u64,
    discontinuity_sequence: u64,
    window_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CatchupInfo {
    pub session: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DvrInfo {
    pub playlist_url: String,
    pub start_ms: i64,
    pub window_seconds: i64,
}

struct Runner {
    child: Child,
    channel_number: String,
    channel_name: String,
    item_key: String,
    playlist: PathBuf,
    startup_ms: u128,
    last_access_ms: i64,
    telemetry_stop: TelemetryStop,
    channel_id: String,
    telemetry_origin: Instant,
}

struct TelemetryStop(Arc<AtomicBool>);

impl Drop for TelemetryStop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProducerSnapshot {
    pub channel: String,
    pub name: String,
    pub item: String,
    pub segments: usize,
    pub bytes: u64,
    pub startup_ms: u128,
}

impl Engine {
    pub fn new(
        pool: SqlitePool,
        data_dir: PathBuf,
        cloud: Arc<CloudManager>,
        max_kbps: i64,
    ) -> Self {
        Self {
            pool,
            data_dir,
            runners: Arc::new(Mutex::new(HashMap::new())),
            supervisors: Arc::new(Mutex::new(HashSet::new())),
            private_runtimes: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            initial_burst: ffmpeg_supports_initial_burst(),
            started_at: Instant::now(),
            catchups: Arc::new(Mutex::new(HashMap::new())),
            dvr: Arc::new(Mutex::new(HashMap::new())),
            cloud,
            max_kbps,
            precache: None,
        }
    }

    pub fn with_precache(mut self, precache: Arc<crate::precache::Precache>) -> Self {
        self.precache = Some(precache);
        self
    }

    /// Stop producers that have drifted too far behind the wall clock. The next
    /// playlist request restarts them at the current schedule offset. Cached
    /// items stay around 8–12 s behind and never trip this.
    pub async fn resync_lagging(&self, max_lag_ms: i64) -> usize {
        let mut runners = self.runners.lock().await;
        let now = now_ms();
        let stale: Vec<(String, String, i64)> = runners
            .iter()
            .filter_map(|(id, runner)| {
                // Only resync channels someone is actually watching; idle
                // producers are allowed to drift.
                if now - runner.last_access_ms > 120_000 {
                    return None;
                }
                let lag = playlist_lag_ms(&runner.playlist)?;
                (lag > max_lag_ms).then(|| (id.clone(), runner.channel_number.clone(), lag))
            })
            .collect();
        let mut restarted = 0;
        for (channel_id, number, lag) in stale {
            if let Some(mut runner) = runners.remove(&channel_id) {
                tracing::warn!(
                    channel = %number,
                    lag_ms = lag,
                    "producer lag guard: restarting at the live offset"
                );
                stop(&mut runner);
                if let Some(dir) = runner.playlist.parent() {
                    clear_hls_dir(dir);
                }
                restarted += 1;
            }
        }
        drop(runners);
        // Private publication drains ordered segments; restarting it would discard
        // the old tail. Report lag while allowing the queue to drain instead.
        for runtime in self.private_runtimes.lock().await.values() {
            if now - runtime.last_access_ms <= 120_000
                && let Some(lag) = playlist_lag_ms(&runtime.playlist)
                && lag > max_lag_ms
            {
                tracing::warn!(channel = %runtime.channel_number, lag_ms = lag,
                            "private producer lag guard: preserving queued segments");
            }
        }
        restarted
    }

    pub fn uptime_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }

    pub async fn producer_snapshots(&self) -> Vec<ProducerSnapshot> {
        let runners = self.runners.lock().await;
        let mut snapshots: Vec<_> = runners
            .values()
            .map(|runner| {
                let mut segments = 0usize;
                let mut bytes = 0u64;
                if let Some(dir) = runner.playlist.parent()
                    && let Ok(entries) = std::fs::read_dir(dir)
                {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|ext| ext.to_str()) == Some("ts") {
                            segments += 1;
                            bytes += std::fs::metadata(&path)
                                .map(|metadata| metadata.len())
                                .unwrap_or(0);
                        }
                    }
                }
                ProducerSnapshot {
                    channel: runner.channel_number.clone(),
                    name: runner.channel_name.clone(),
                    item: runner.item_key.clone(),
                    segments,
                    bytes,
                    startup_ms: runner.startup_ms,
                }
            })
            .collect();
        drop(runners);
        for runtime in self.private_runtimes.lock().await.values() {
            let mut segments = 0;
            let mut bytes = 0;
            if let Some(dir) = runtime.playlist.parent()
                && let Ok(entries) = std::fs::read_dir(dir)
            {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("ts") {
                        segments += 1;
                        bytes += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
            snapshots.push(ProducerSnapshot {
                channel: runtime.channel_number.clone(),
                name: runtime.channel_name.clone(),
                item: if runtime.task.is_finished() {
                    "private publisher failed"
                } else {
                    "private publisher"
                }
                .into(),
                segments,
                bytes,
                startup_ms: 0, // unavailable for the asynchronous private runtime
            });
        }
        snapshots
    }

    pub async fn refresh(&self, channel_id: &str) {
        self.stop_dvr_for_channel(channel_id).await;
        if let Some(runtime) = self.private_runtimes.lock().await.remove(channel_id) {
            runtime.stop.store(true, Ordering::Relaxed);
            let _ = runtime.task.await;
        }
        let runner = {
            let mut runners = self.runners.lock().await;
            runners.remove(channel_id)
        };
        if let Some(mut runner) = runner {
            stop(&mut runner);
        }
        // Keep the advertised window and its segment URLs alive. The private
        // publisher reconstructs sequence and PDT from it on the next request.
        // Legacy producers clear their own output when starting a fresh run.
    }

    pub async fn prewarm(self: &Arc<Self>) {
        let Ok(channels) = crate::db::list_channels(&self.pool).await else {
            return;
        };
        for channel in channels {
            if !channel.stream.prewarm {
                continue;
            }
            if let Err(error) = self.ensure(&channel).await {
                tracing::warn!(channel = %channel.number, %error, "prewarm failed");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn stop_all(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let runtimes = std::mem::take(&mut *self.private_runtimes.lock().await);
        for (_, runtime) in runtimes {
            runtime.stop.store(true, Ordering::Relaxed);
            let _ = runtime.task.await;
        }
        let mut runners = self.runners.lock().await;
        for runner in runners.values_mut() {
            stop(runner);
        }
        runners.clear();
        let mut catchups = self.catchups.lock().await;
        for session in catchups.values_mut() {
            stop_child(&mut session.child);
            let _ = std::fs::remove_dir_all(&session.dir);
        }
        catchups.clear();
        let mut dvr = self.dvr.lock().await;
        for session in dvr.values() {
            session.stop.store(true, Ordering::Relaxed);
        }
        dvr.clear();
    }

    async fn resolved_channel(&self, channel: &Channel) -> Result<Channel> {
        let mut channel = crate::db::channel_by_id(&self.pool, &channel.id)
            .await?
            .unwrap_or_else(|| channel.clone());
        let defaults = crate::db::language_settings(&self.pool).await?;
        if channel
            .stream
            .audio_language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            channel.stream.audio_language = defaults.audio_language;
        }
        if channel.stream.subtitle_mode == "inherit" {
            channel.stream.subtitle_mode = defaults.subtitle_mode;
        }
        if channel
            .stream
            .subtitle_language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            channel.stream.subtitle_language = defaults.subtitle_language;
        }
        Ok(channel)
    }

    pub async fn start_catchup(&self, channel: &Channel) -> Result<CatchupInfo> {
        self.prune_catchups().await;
        let channel = self.resolved_channel(channel).await?;
        let now = now_ms();
        let item = current_item(&self.pool, &channel.id, now)
            .await?
            .context("channel has no current item")?;
        let media_item = crate::db::media_by_reference(&self.pool, &item.media_id)
            .await?
            .context("current media item is missing from the catalog")?;
        let resolved = self.cloud.resolve_media_input(&item.media_id).await?;
        let media = resolved.location;
        let media_kind = resolved.kind;
        let (in_ms, duration_ms) = if item.kind == "program" && media_item.duration_ms > 0 {
            (0, media_item.duration_ms)
        } else {
            let out_ms = item
                .out_ms
                .unwrap_or(item.in_ms + (item.end_ms - item.start_ms).max(0));
            if out_ms <= item.in_ms {
                bail!("current item has no catch-up window");
            }
            (item.in_ms, out_ms - item.in_ms)
        };
        let watermark = match channel.graphics.watermark_media_id.as_deref() {
            Some(media_id) if !media_id.is_empty() => media_path(&self.pool, media_id).await?,
            _ => None,
        };
        let watermark_path = watermark.as_deref().map(Path::new);
        let session = uuid::Uuid::new_v4().simple().to_string();
        let out_dir = self.data_dir.join("hls").join("catchup").join(&session);
        tokio::fs::create_dir_all(&out_dir).await?;
        let (playlist, child) = start_producer(
            &channel,
            &format!("catchup_{session}"),
            &item.media_id,
            Path::new(&media),
            in_ms,
            duration_ms,
            &out_dir,
            &self.data_dir,
            self.initial_burst,
            &media_kind,
            watermark_path,
            self.max_kbps,
            false,
        )?;
        wait_for_ready(&playlist, Duration::from_secs(20), 1).await?;
        let mut catchups = self.catchups.lock().await;
        let stale: Vec<String> = catchups
            .iter()
            .filter(|(_, entry)| entry.channel_id == channel.id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(mut entry) = catchups.remove(&key) {
                stop_child(&mut entry.child);
                let _ = std::fs::remove_dir_all(&entry.dir);
            }
        }
        catchups.insert(
            session.clone(),
            CatchupSession {
                channel_id: channel.id.clone(),
                child,
                playlist,
                dir: out_dir,
                expires_ms: now + duration_ms + 60_000,
            },
        );
        Ok(CatchupInfo {
            session,
            title: item.title,
            start_ms: item.start_ms,
            end_ms: item.start_ms + duration_ms,
        })
    }

    pub async fn catchup_playlist(&self, session: &str) -> Result<String> {
        self.prune_catchups().await;
        let mut catchups = self.catchups.lock().await;
        let entry = catchups
            .get_mut(session)
            .with_context(|| format!("catch-up session not found: {session}"))?;
        let body = tokio::fs::read_to_string(&entry.playlist).await?;
        let ended = entry.child.try_wait().ok().flatten().is_some();
        let mut body = body
            .lines()
            .map(|line| {
                if line.ends_with(".ts") {
                    format!("/catchup/{session}/{line}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        body.push('\n');
        if ended && !body.contains("#EXT-X-ENDLIST") {
            body.push_str("#EXT-X-ENDLIST\n");
        }
        Ok(body)
    }

    pub async fn catchup_segment_path(&self, session: &str, file: &str) -> Result<PathBuf> {
        if file.contains('/') || file.contains('\\') || file.contains("..") {
            bail!("invalid catch-up segment name");
        }
        let catchups = self.catchups.lock().await;
        let entry = catchups
            .get(session)
            .with_context(|| format!("catch-up session not found: {session}"))?;
        Ok(entry.dir.join(file))
    }

    async fn prune_catchups(&self) {
        let now = now_ms();
        let mut catchups = self.catchups.lock().await;
        let expired: Vec<String> = catchups
            .iter()
            .filter(|(_, entry)| entry.expires_ms < now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            if let Some(mut entry) = catchups.remove(&key) {
                stop_child(&mut entry.child);
                let _ = std::fs::remove_dir_all(&entry.dir);
            }
        }
    }

    pub async fn start_dvr(&self, channel: &Channel, window_seconds: i64) -> Result<DvrInfo> {
        let channel = self.resolved_channel(channel).await?;
        let now = now_ms();
        let current = current_item(&self.pool, &channel.id, now)
            .await?
            .context("channel has no current item")?;
        self.stop_dvr(&channel.number).await;
        let window_seconds = window_seconds.clamp(30, 14_400);
        let dir = self.data_dir.join("hls").join("dvr").join(&channel.number);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let playlist = dir.join("index.m3u8");
        let stop = Arc::new(AtomicBool::new(false));
        let last_access = Arc::new(AtomicI64::new(now));
        let pool = self.pool.clone();
        let data_dir = self.data_dir.clone();
        let channel_for_task = channel.clone();
        let stop_task = stop.clone();
        let access_task = last_access.clone();
        let current_for_task = current.clone();
        let dir_for_task = dir.clone();
        let playlist_for_task = playlist.clone();
        let cloud_for_task = self.cloud.clone();
        let max_kbps = self.max_kbps;
        tokio::spawn(async move {
            if let Err(error) = run_dvr(
                pool,
                data_dir,
                channel_for_task,
                current_for_task,
                window_seconds * 1000,
                stop_task,
                access_task,
                dir_for_task,
                playlist_for_task,
                cloud_for_task,
                max_kbps,
            )
            .await
            {
                tracing::warn!(%error, "DVR session ended with an error");
            }
        });
        self.dvr.lock().await.insert(
            channel.number.clone(),
            DvrSession {
                channel_id: channel.id.clone(),
                dir: dir.clone(),
                playlist: playlist.clone(),
                stop,
                last_access,
            },
        );
        let _ = wait_for_ready(&playlist, Duration::from_secs(6), 1).await;
        Ok(DvrInfo {
            playlist_url: format!("/dvr/{}.m3u8", channel.number),
            start_ms: current.start_ms,
            window_seconds,
        })
    }

    pub async fn dvr_playlist(&self, number: &str) -> Result<String> {
        let session = {
            let dvr = self.dvr.lock().await;
            dvr.get(number)
                .map(|session| (session.playlist.clone(), session.last_access.clone()))
        };
        let (playlist, last_access) = session.context("DVR session not found")?;
        last_access.store(now_ms(), Ordering::Relaxed);
        Ok(tokio::fs::read_to_string(&playlist).await?)
    }

    pub async fn dvr_segment_path(&self, number: &str, file: &str) -> Result<PathBuf> {
        if file.contains('/') || file.contains('\\') || file.contains("..") {
            bail!("invalid DVR segment name");
        }
        let session = {
            let dvr = self.dvr.lock().await;
            dvr.get(number)
                .map(|session| (session.dir.clone(), session.last_access.clone()))
        };
        let (dir, last_access) = session.context("DVR session not found")?;
        last_access.store(now_ms(), Ordering::Relaxed);
        Ok(dir.join(file))
    }

    pub async fn stop_dvr(&self, number: &str) {
        let session = self.dvr.lock().await.remove(number);
        if let Some(session) = session {
            session.stop.store(true, Ordering::Relaxed);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = tokio::fs::remove_dir_all(&session.dir).await;
            });
        }
    }

    pub async fn stop_dvr_for_channel(&self, channel_id: &str) {
        let numbers: Vec<String> = {
            let dvr = self.dvr.lock().await;
            dvr.iter()
                .filter(|(_, session)| session.channel_id == channel_id)
                .map(|(number, _)| number.clone())
                .collect()
        };
        for number in numbers {
            self.stop_dvr(&number).await;
        }
    }

    async fn start_supervisor(self: &Arc<Self>, channel: Channel) {
        let already_running = {
            let mut supervisors = self.supervisors.lock().await;
            !supervisors.insert(channel.id.clone())
        };
        if already_running {
            return;
        }
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if engine.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(error) = engine.ensure_inner(&channel).await {
                    tracing::warn!(channel = %channel.number, %error, "supervisor ensure failed");
                }
                if !channel.stream.prewarm {
                    let runtime = {
                        let mut runtimes = engine.private_runtimes.lock().await;
                        if runtimes
                            .get(&channel.id)
                            .is_some_and(|r| now_ms() - r.last_access_ms > 120_000)
                        {
                            runtimes.remove(&channel.id)
                        } else {
                            None
                        }
                    };
                    if let Some(runtime) = runtime {
                        runtime.stop.store(true, Ordering::Relaxed);
                        let _ = runtime.task.await;
                        engine.supervisors.lock().await.remove(&channel.id);
                        return;
                    }
                }
                let idle = {
                    let runners = engine.runners.lock().await;
                    runners
                        .get(&channel.id)
                        .is_some_and(|runner| now_ms() - runner.last_access_ms > 120_000)
                };
                if idle && !channel.stream.prewarm {
                    let mut runners = engine.runners.lock().await;
                    if let Some(mut runner) = runners.remove(&channel.id) {
                        tracing::info!(channel = %channel.number, "stopped idle producer");
                        stop(&mut runner);
                    }
                    drop(runners);
                    engine.supervisors.lock().await.remove(&channel.id);
                    return;
                }
            }
        });
    }

    /// Ensure a live HLS producer is running for the channel's current item.
    /// Phase 1 chains one FFmpeg process per item; the next playlist request
    /// starts the next item. The production engine will use a segment cache and
    /// explicit boundary handling.
    #[cfg(test)]
    pub(crate) async fn install_playout_fixture(&self, channel_id: &str) {
        use private_publisher::{SegmentIdentity, SegmentSource};
        let snapshot: SnapshotHandle = Default::default();
        snapshot.lock().unwrap().segments = [
            SegmentSource::Unknown,
            SegmentSource::Media("m".into()),
            SegmentSource::Slate,
            SegmentSource::Media("iso".into()),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, source)| SegmentIdentity {
            sequence: i as u64,
            start_pdt_ns: (10000 + i as i128 * 1000) * 1_000_000,
            end_pdt_ns: (11000 + i as i128 * 1000) * 1_000_000,
            start_90k: (10000 + i as i128 * 1000) * 90,
            end_90k: (11000 + i as i128 * 1000) * 90,
            source,
        })
        .collect();
        snapshot.lock().unwrap().new_published_seconds = 3.0;
        self.private_runtimes.lock().await.insert(
            channel_id.into(),
            PrivateRuntime {
                generation: "fixture-generation".into(),
                snapshot,
                ready: Arc::new(AtomicBool::new(true)),
                failed_at_ms: Arc::new(AtomicI64::new(0)),
                stop: Arc::new(AtomicBool::new(false)),
                task: tokio::spawn(std::future::pending()),
                last_access_ms: 0,
                channel_number: "fixture".into(),
                channel_name: "fixture".into(),
                playlist: self.data_dir.join("secret-token"),
            },
        );
    }

    pub async fn playout_status(&self, channel_id: &str) -> PlayoutStatus {
        let runtimes = self.private_runtimes.lock().await;
        let Some(runtime) = runtimes.get(channel_id) else {
            return PlayoutStatus {
                channel_id: channel_id.into(),
                generation: None,
                state: "preparing",
                new_published_seconds: 0.0,
                segments: vec![],
            };
        };
        let state = if runtime.task.is_finished() {
            "failed"
        } else if runtime.ready.load(Ordering::Acquire) {
            "ready"
        } else {
            "preparing"
        };
        let snapshot = runtime.snapshot.lock().unwrap();
        let segments = snapshot
            .segments
            .iter()
            .filter_map(|segment| {
                const MAX_SAFE: i64 = 9_007_199_254_740_991;
                let start_ms = i64::try_from(segment.start_pdt_ns.div_euclid(1_000_000)).ok()?;
                let end_ms = i64::try_from(segment.end_pdt_ns.div_euclid(1_000_000)).ok()?;
                if !(-MAX_SAFE..=MAX_SAFE).contains(&start_ms)
                    || !(-MAX_SAFE..=MAX_SAFE).contains(&end_ms)
                {
                    return None;
                }
                let (source, media_id) = match &segment.source {
                    private_publisher::SegmentSource::Media(id)
                    | private_publisher::SegmentSource::ScheduledMedia { id, .. } => {
                        ("media", Some(id.clone()))
                    }
                    private_publisher::SegmentSource::Slate
                    | private_publisher::SegmentSource::Fallback { .. } => ("slate", None),
                    private_publisher::SegmentSource::Unknown => ("unknown", None),
                };
                Some(PlayoutSegment {
                    sequence: segment.sequence.to_string(),
                    start_ms,
                    end_ms,
                    start_90k: segment.start_90k.to_string(),
                    end_90k: segment.end_90k.to_string(),
                    source,
                    media_id,
                    title: None,
                    fallback_reason: match &segment.source {
                        private_publisher::SegmentSource::Fallback {
                            fallback_reason, ..
                        } => fallback_reason.clone(),
                        _ => None,
                    },
                    scheduled_programme: match &segment.source {
                        private_publisher::SegmentSource::Fallback {
                            scheduled_programme,
                            ..
                        } => scheduled_programme.clone(),
                        _ => None,
                    },
                })
            })
            .collect();
        PlayoutStatus {
            channel_id: channel_id.into(),
            generation: Some(runtime.generation.clone()),
            state,
            new_published_seconds: snapshot.new_published_seconds,
            segments,
        }
    }

    pub async fn ensure(self: &Arc<Self>, channel: &Channel) -> Result<PathBuf> {
        self.start_supervisor(channel.clone()).await;
        let playlist = self.ensure_inner(channel).await?;
        let mut runners = self.runners.lock().await;
        if let Some(runner) = runners.get_mut(&channel.id) {
            runner.last_access_ms = now_ms();
        }
        drop(runners);
        if let Some(runtime) = self.private_runtimes.lock().await.get_mut(&channel.id) {
            runtime.last_access_ms = now_ms();
        }
        Ok(playlist)
    }

    async fn ensure_inner(&self, channel: &Channel) -> Result<PathBuf> {
        let channel = self.resolved_channel(channel).await?;
        let out_dir = self
            .data_dir
            .join("hls")
            .join(channel.number.replace('.', "_"));
        {
            let mut runtimes = self.private_runtimes.lock().await;
            if let Some(runtime) = runtimes.get(&channel.id).filter(|r| r.task.is_finished()) {
                if private_restart_backoff(&runtime.failed_at_ms, now_ms()) {
                    bail!("private publisher failed; restart backoff active");
                }
                runtimes.remove(&channel.id);
            }
            if let Some(runtime) = runtimes.get(&channel.id) {
                let ready = runtime.ready.clone();
                drop(runtimes);
                wait_for_private_ready(
                    &out_dir.join("index.m3u8"),
                    &ready,
                    Duration::from_secs(45),
                )
                .await?;
                return Ok(out_dir.join("index.m3u8"));
            }
            let now = now_ms();
            if !self.runners.lock().await.contains_key(&channel.id) {
                tokio::fs::create_dir_all(&out_dir).await?;
                let stop = Arc::new(AtomicBool::new(false));
                let task_stop = stop.clone();
                let ready = Arc::new(AtomicBool::new(false));
                let task_ready = ready.clone();
                let failed_at_ms = Arc::new(AtomicI64::new(0));
                let task_failed_at = failed_at_ms.clone();
                let pool = self.pool.clone();
                let cloud = self.cloud.clone();
                let precache = self.precache.clone();
                let task_channel = channel.clone();
                let data_dir = self.data_dir.clone();
                let task_out = out_dir.clone();
                let max_kbps = self.max_kbps;
                let generation = uuid::Uuid::new_v4().to_string();
                let snapshot: SnapshotHandle =
                    Arc::new(std::sync::Mutex::new(PublishedSnapshot::default()));
                let task_snapshot = snapshot.clone();
                let task = tokio::spawn(async move {
                    if let Err(error) = private_publisher::run_channel(
                        pool,
                        cloud,
                        precache,
                        task_channel,
                        data_dir,
                        task_out,
                        max_kbps,
                        task_stop,
                        task_ready.clone(),
                        now,
                        task_snapshot,
                    )
                    .await
                    {
                        tracing::warn!(%error, "private channel runtime failed");
                        task_failed_at.store(now_ms(), Ordering::Release);
                    }
                    task_ready.store(false, Ordering::Release);
                });
                runtimes.insert(
                    channel.id.clone(),
                    PrivateRuntime {
                        generation,
                        snapshot,
                        ready: ready.clone(),
                        failed_at_ms,
                        stop,
                        task,
                        last_access_ms: now,
                        channel_number: channel.number.clone(),
                        channel_name: channel.name.clone(),
                        playlist: out_dir.join("index.m3u8"),
                    },
                );
                drop(runtimes);
                wait_for_private_ready(
                    &out_dir.join("index.m3u8"),
                    &ready,
                    Duration::from_secs(45),
                )
                .await?;
                return Ok(out_dir.join("index.m3u8"));
            }
        }
        let mut runners = self.runners.lock().await;
        let now = now_ms();
        let item = current_item(&self.pool, &channel.id, now).await?;
        let boundary = item.as_ref().map(|item| (item.start_ms, item.end_ms));
        let (item_key, media_id, media, offset_ms, duration_ms, slate_duration, media_kind) =
            match item {
                Some(item) => {
                    let resolved = self.cloud.resolve_media_input(&item.media_id).await?;
                    (
                        format!("{}:{}", item.media_id, item.start_ms),
                        item.media_id.clone(),
                        Some(resolved.location),
                        trim_window(&item, now).0,
                        trim_window(&item, now).1,
                        None,
                        resolved.kind,
                    )
                }
                None => {
                    let next = next_item_start(&self.pool, &channel.id, now).await?;
                    let gap_end = next.unwrap_or(now + 60_000).min(now + 60_000);
                    let duration = (gap_end - now).clamp(1_000, 60_000);
                    (
                        format!("slate:{}", now / 60_000),
                        String::new(),
                        None,
                        0,
                        None,
                        Some(duration),
                        "slate".to_string(),
                    )
                }
            };
        let out_dir = self
            .data_dir
            .join("hls")
            .join(channel.number.replace('.', "_"));
        tokio::fs::create_dir_all(&out_dir).await?;

        let mut preserve_playlist = false;
        if let Some(runner) = runners.get_mut(&channel.id) {
            preserve_playlist = true;
            if runner.item_key == item_key {
                match runner.child.try_wait()? {
                    None => return Ok(runner.playlist.clone()),
                    Some(status) => {
                        tracing::info!(event = "producer_exit", channel_id = %channel.id, item = %runner.item_key, pid = runner.child.id(), %status, wall_ms = telemetry_wall_ms(), monotonic_ms = self.started_at.elapsed().as_millis() as u64)
                    }
                }
            }
            tracing::info!(event = "schedule_boundary", channel_id = %channel.id, old_item = %runner.item_key, new_item = %item_key, scheduled_start_ms = boundary.map(|b| b.0), scheduled_end_ms = boundary.map(|b| b.1), wall_ms = telemetry_wall_ms(), monotonic_ms = self.started_at.elapsed().as_millis() as u64);
            stop(runner);
            runners.remove(&channel.id);
        }

        let watermark = match channel.graphics.watermark_media_id.as_deref() {
            Some(media_id) if !media_id.is_empty() => media_path(&self.pool, media_id).await?,
            _ => None,
        };
        let watermark_path = watermark.as_deref().map(Path::new);
        let started = Instant::now();
        let (playlist, child) = if let Some(duration) = slate_duration {
            start_slate_producer(&channel, &item_key, duration, &out_dir, &self.data_dir)?
        } else {
            let path = media.context("current media item is missing from the catalog")?;
            start_producer(
                &channel,
                &item_key,
                &media_id,
                Path::new(&path),
                offset_ms,
                duration_ms.unwrap_or(0),
                &out_dir,
                &self.data_dir,
                self.initial_burst,
                &media_kind,
                watermark_path,
                self.max_kbps,
                preserve_playlist,
            )?
        };
        let telemetry_stop = TelemetryStop(Arc::new(AtomicBool::new(false)));
        tracing::info!(event = "producer_spawn", channel_id = %channel.id, item = %item_key, pid = child.id(), scheduled_start_ms = boundary.map(|b| b.0), scheduled_end_ms = boundary.map(|b| b.1), wall_ms = telemetry_wall_ms(), monotonic_ms = self.started_at.elapsed().as_millis() as u64);
        observe_publication(
            playlist.clone(),
            channel.id.clone(),
            item_key.clone(),
            child.id(),
            self.started_at,
            telemetry_stop.0.clone(),
        );
        let segment_seconds = channel.stream.segment_seconds.clamp(2, 10);
        let min_segments = prebuffer_segments(channel.stream.prebuffer_seconds, segment_seconds);
        wait_for_ready(&playlist, Duration::from_secs(20), min_segments).await?;
        let startup_ms = started.elapsed().as_millis();
        runners.insert(
            channel.id.clone(),
            Runner {
                child,
                channel_number: channel.number.clone(),
                channel_name: channel.name.clone(),
                item_key,
                playlist: playlist.clone(),
                startup_ms,
                last_access_ms: now_ms(),
                telemetry_stop,
                channel_id: channel.id.clone(),
                telemetry_origin: self.started_at,
            },
        );
        Ok(playlist)
    }
}

/// Continue segment numbering and MPEG-TS timestamps across producer
/// restarts. Without this, HLS.js sees the media sequence/PTS reset when an
/// item boundary or supervisor restart creates a new ffmpeg process.
fn next_hls_segment_number(playlist: &Path) -> i64 {
    let Ok(body) = std::fs::read_to_string(playlist) else {
        return 0;
    };
    let mut next = 0i64;
    for line in body.lines() {
        let Some(name) = line.strip_suffix(".ts") else {
            continue;
        };
        let Some((_, number)) = name.rsplit_once('_') else {
            continue;
        };
        if let Ok(number) = number.parse::<i64>() {
            next = next.max(number.saturating_add(1));
        }
    }
    next
}

#[allow(clippy::too_many_arguments)]
fn start_producer(
    channel: &Channel,
    item_key: &str,
    media_id: &str,
    media: &Path,
    offset_ms: i64,
    duration_ms: i64,
    out_dir: &Path,
    data_dir: &Path,
    initial_burst: bool,
    media_kind: &str,
    watermark: Option<&Path>,
    max_kbps: i64,
    preserve_playlist: bool,
) -> Result<(PathBuf, Child)> {
    let playlist = out_dir.join("index.m3u8");
    let mut command = build_producer_command(
        channel,
        item_key,
        media_id,
        media,
        offset_ms,
        duration_ms,
        out_dir,
        data_dir,
        initial_burst,
        media_kind,
        watermark,
        max_kbps,
        preserve_playlist,
        false,
    )?;
    let child = command
        .spawn()
        .with_context(|| format!("spawn ffmpeg for channel {}", channel.number))?;
    tracing::debug!(channel = %channel.number, offset_ms, "started producer");
    Ok((playlist, child))
}

#[allow(clippy::too_many_arguments)]
fn build_producer_command(
    channel: &Channel,
    item_key: &str,
    media_id: &str,
    media: &Path,
    offset_ms: i64,
    duration_ms: i64,
    out_dir: &Path,
    data_dir: &Path,
    initial_burst: bool,
    media_kind: &str,
    watermark: Option<&Path>,
    max_kbps: i64,
    preserve_playlist: bool,
    private: bool,
) -> Result<Command> {
    build_producer_command_leased(
        channel,
        item_key,
        media_id,
        media,
        offset_ms,
        duration_ms,
        out_dir,
        data_dir,
        initial_burst,
        media_kind,
        watermark,
        max_kbps,
        preserve_playlist,
        private,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
#[derive(Clone)]
enum InputLease {
    #[allow(dead_code)]
    Full(crate::precache::CacheLease),
    #[allow(dead_code)]
    Chunk(std::sync::Arc<crate::precache::chunks::ChunkLease>),
}

#[allow(clippy::too_many_arguments)]
fn build_producer_command_leased(
    channel: &Channel,
    item_key: &str,
    media_id: &str,
    media: &Path,
    offset_ms: i64,
    duration_ms: i64,
    out_dir: &Path,
    data_dir: &Path,
    initial_burst: bool,
    media_kind: &str,
    watermark: Option<&Path>,
    max_kbps: i64,
    preserve_playlist: bool,
    private: bool,
    lease: Option<&InputLease>,
) -> Result<Command> {
    let playlist = out_dir.join("index.m3u8");
    // On an item transition keep the existing playlist so the next producer can
    // append with a discontinuity and receivers never see a reset.
    if !private && !preserve_playlist {
        clear_hls_dir(out_dir);
    }
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("channel-{}.log", channel.number));
    let log = if private {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
    } else {
        File::create(&log_path)
    }
    .with_context(|| format!("create {}", log_path.display()))?;
    // A fresh run id keeps segment URLs unique across producer restarts, so a
    // client or service-worker cache can never serve a stale segment.
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let segment_prefix = format!("seg_{}_{}_{:05}", item_key.replace(':', "_"), run_id, 0);
    let segment_pattern = out_dir.join(format!("{segment_prefix}_%05d.ts"));
    let probe = if matches!(media_kind, "image" | "slate") {
        StreamProbe::default()
    } else {
        probe_streams(media, &channel.stream, lease.cloned())
    };
    let subtitle_path = if channel.stream.subtitle_mode == "burn"
        && !matches!(media_kind, "image" | "music" | "slate")
    {
        probe.subtitle_ordinal.and_then(|ordinal| {
            ensure_subtitle_file(media, media_id, ordinal, data_dir, lease.cloned()).and_then(
                |path| shifted_subtitle_file(&path, offset_ms, data_dir, media_id, ordinal),
            )
        })
    } else {
        None
    };
    let tonemap = channel.stream.hdr_mode == "tonemap" && probe.hdr;
    let hardware = channel.stream.encoder == "vaapi" && !matches!(media_kind, "music" | "image");
    let next_segment = next_hls_segment_number(&playlist);
    let segment_offset = next_segment as f64 * channel.stream.segment_seconds.clamp(2, 10) as f64;
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y");
    if offset_ms > 0 && media_kind != "image" {
        command
            .arg("-ss")
            .arg(format!("{:.3}", offset_ms as f64 / 1000.0));
    }
    // Private generations are bounded and publication is paced separately.
    // Reading them in real time adds encoder startup latency to every segment.
    if !private {
        command.arg("-readrate").arg("1.0");
    }
    #[cfg(test)]
    if private && data_dir.join("slow-private-input").exists() {
        command.arg("-readrate").arg("1.0");
    }
    if !private && initial_burst && channel.stream.prebuffer_seconds > 0 {
        command
            .arg("-readrate_initial_burst")
            .arg(channel.stream.prebuffer_seconds.to_string());
    }
    if hardware {
        command.arg("-vaapi_device").arg("/dev/dri/renderD128");
    }
    if media_kind == "image" {
        command.arg("-loop").arg("1").arg("-framerate").arg("30");
    }
    command.arg("-i").arg(media_input(media));
    let animated_watermark = watermark.filter(|path| is_animated_watermark(path));
    if let Some(watermark) = animated_watermark {
        command
            .arg("-stream_loop")
            .arg("-1")
            .arg("-i")
            .arg(watermark);
    }
    if duration_ms > 0 {
        command
            .arg("-t")
            .arg(format!("{:.3}", duration_ms as f64 / 1000.0));
    }
    if media_kind == "music" {
        if let Some(index) = probe.audio_index {
            command.arg("-map").arg(format!("0:{index}"));
        } else {
            command.arg("-map").arg("0:a:0?");
        }
        command.arg("-vn");
    } else {
        if media_kind == "image" {
            command.arg("-an");
        } else {
            if animated_watermark.is_none() {
                command.arg("-map").arg("0:v:0");
            }
            if let Some(index) = probe.audio_index {
                command.arg("-map").arg(format!("0:{index}"));
            } else {
                // The probe may have timed out; keep the first audio track
                // instead of silently producing a video-only stream.
                command.arg("-map").arg("0:a:0?");
            }
        }
        if animated_watermark.is_some() {
            command
                .arg("-filter_complex")
                .arg(animated_watermark_filter(
                    channel,
                    subtitle_path.as_deref(),
                    tonemap,
                    hardware,
                ))
                .arg("-map")
                .arg("[vout]");
        } else {
            command.arg("-vf").arg(video_filter(
                channel,
                watermark,
                subtitle_path.as_deref(),
                tonemap,
                hardware,
            ));
        }
        let bitrate_cap = if channel.stream.max_kbps > 0 {
            channel.stream.max_kbps
        } else {
            max_kbps
        };
        if hardware {
            command.arg("-c:v").arg("h264_vaapi");
            if bitrate_cap > 0 {
                // VAAPI constant-QP mode ignores -maxrate, so switch to CBR
                // when a ceiling is configured.
                command
                    .arg("-rc_mode")
                    .arg("CBR")
                    .arg("-b:v")
                    .arg(format!("{bitrate_cap}k"))
                    .arg("-maxrate")
                    .arg(format!("{bitrate_cap}k"))
                    .arg("-bufsize")
                    .arg(format!("{}k", bitrate_cap.saturating_mul(2)));
            } else {
                command.arg("-qp").arg(channel.stream.crf.to_string());
            }
        } else {
            command
                .arg("-c:v")
                .arg("libx264")
                .arg("-preset")
                .arg(&channel.stream.preset)
                .arg("-crf")
                .arg(channel.stream.crf.to_string())
                .arg("-pix_fmt")
                .arg("yuv420p");
            if bitrate_cap > 0 {
                // Cap the VBV buffer as well as the peak rate so a complex
                // scene cannot burst past what the upload path can sustain.
                command
                    .arg("-maxrate")
                    .arg(format!("{bitrate_cap}k"))
                    .arg("-bufsize")
                    .arg(format!("{}k", bitrate_cap.saturating_mul(2)));
            }
        }
        let segment_seconds = channel.stream.segment_seconds.clamp(2, 10);
        let keyint = ((30_000.0 / 1001.0) * segment_seconds as f64).round() as i64;
        command
            .arg("-g")
            .arg(keyint.to_string())
            .arg("-keyint_min")
            .arg(keyint.to_string())
            .arg("-sc_threshold")
            .arg("0")
            .arg("-force_key_frames")
            .arg(format!("expr:gte(t,n_forced*{segment_seconds})"));
    }
    // Always encode mapped audio. When ffprobe times out the audio index is
    // unknown, but the optional `0:a:0?` map still lets FFmpeg pick the source
    // track; without these args a high-bitrate source audio stream can blow
    // past the public bitrate budget even though video is capped.
    if media_kind != "image" {
        command
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg(format!("{}k", channel.stream.audio_kbps))
            .arg("-ar")
            .arg("48000")
            .arg("-ac")
            .arg("2");
    }
    if private {
        command
            .stdout(Stdio::null())
            .stderr(if media.to_string_lossy().contains("://") {
                Stdio::null()
            } else {
                Stdio::from(log)
            });
        return Ok(command);
    }
    command
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(channel.stream.segment_seconds.clamp(2, 10).to_string())
        .arg("-hls_list_size")
        .arg(channel.stream.cache_segments.clamp(4, 60).to_string())
        .arg("-hls_flags")
        .arg("delete_segments+omit_endlist+program_date_time+independent_segments+discont_start")
        .arg("-start_number")
        .arg(next_segment.to_string())
        .arg("-output_ts_offset")
        .arg(format!("{segment_offset:.3}"))
        .arg("-hls_segment_filename")
        .arg(&segment_pattern)
        .arg(&playlist)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));

    Ok(command)
}

fn ffmpeg_supports_initial_burst() -> bool {
    Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-h")
        .arg("full")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains("readrate_initial_burst"))
        .unwrap_or(false)
}

fn start_slate_producer(
    channel: &Channel,
    item_key: &str,
    duration_ms: i64,
    out_dir: &Path,
    data_dir: &Path,
) -> Result<(PathBuf, Child)> {
    let playlist = out_dir.join("index.m3u8");
    // Read the numbering before clearing, so a slate that fills a schedule gap
    // continues the channel timeline instead of resetting it to zero.
    let next_segment = next_hls_segment_number(&playlist);
    let segment_offset = next_segment as f64 * channel.stream.segment_seconds.clamp(2, 10) as f64;
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("channel-{}.log", channel.number));
    let log = File::create(&log_path).with_context(|| format!("create {}", log_path.display()))?;
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let segment_prefix = format!("seg_{}_{}_{:05}", item_key.replace(':', "_"), run_id, 0);
    let segment_pattern = out_dir.join(format!("{segment_prefix}_%05d.ts"));
    let seconds = (duration_ms as f64 / 1000.0).max(1.0);
    let font = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
    let text = if Path::new(font).is_file() {
        format!(
            "drawtext=fontfile={font}:text='AWESOME TV':fontcolor=0x7ee7c7:fontsize=42:x=(w-text_w)/2:y=(h-text_h)/2-20,drawtext=fontfile={font}:text='PLEASE STAND BY':fontcolor=0xf3efe6:fontsize=22:x=(w-text_w)/2:y=(h-text_h)/2+30"
        )
    } else {
        "null".to_string()
    };
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-readrate")
        .arg("1.0")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "color=c=0x0b1120:s={}x{}:r={FPS}:d={seconds}",
            channel.stream.width, channel.stream.height
        ))
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("anullsrc=channel_layout=stereo:sample_rate=48000")
        .arg("-vf")
        .arg(text)
        .arg("-t")
        .arg(format!("{seconds}"))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-crf")
        .arg("28")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-g")
        .arg("60")
        .arg("-keyint_min")
        .arg("60")
        .arg("-sc_threshold")
        .arg("0")
        .arg("-force_key_frames")
        .arg("expr:gte(t,n_forced*4)")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("96k")
        .arg("-ar")
        .arg("48000")
        .arg("-ac")
        .arg("2")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(channel.stream.segment_seconds.clamp(2, 10).to_string())
        .arg("-hls_list_size")
        .arg(channel.stream.cache_segments.clamp(4, 60).to_string())
        .arg("-hls_flags")
        .arg("delete_segments+omit_endlist+program_date_time+independent_segments+discont_start")
        .arg("-start_number")
        .arg(next_segment.to_string())
        .arg("-output_ts_offset")
        .arg(format!("{segment_offset:.3}"))
        .arg("-hls_segment_filename")
        .arg(&segment_pattern)
        .arg(&playlist)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    let child = command
        .spawn()
        .with_context(|| format!("spawn slate for channel {}", channel.number))?;
    tracing::info!(channel = %channel.number, duration_ms, "started slate");
    Ok((playlist, child))
}

fn prebuffer_segments(seconds: i64, segment_seconds: i64) -> usize {
    let segment_seconds = segment_seconds.clamp(2, 10);
    (seconds.max(0).saturating_add(segment_seconds - 1) / segment_seconds).clamp(1, 4) as usize
}

fn private_restart_backoff(failed_at: &AtomicI64, now: i64) -> bool {
    let _ = failed_at.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
    now.saturating_sub(failed_at.load(Ordering::Acquire)) < 5_000
}

async fn wait_for_private_ready(
    playlist: &Path,
    ready: &AtomicBool,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if ready.load(Ordering::Acquire) {
            let remaining = timeout.saturating_sub(started.elapsed());
            return wait_for_ready(playlist, remaining, 1).await;
        }
        if started.elapsed() >= timeout {
            bail!(
                "timed out waiting for private prebuffer at {}",
                playlist.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_ready(playlist: &Path, timeout: Duration, min_segments: usize) -> Result<()> {
    let started = Instant::now();
    loop {
        if let Ok(body) = tokio::fs::read_to_string(playlist).await {
            let segments = body
                .lines()
                .filter(|line| line.starts_with("#EXTINF:"))
                .count();
            // Phase 1 starts as soon as one segment exists; short fixture clips
            // may not produce three. Production waits for a configurable count.
            if segments >= min_segments {
                return Ok(());
            }
        }
        if started.elapsed() > timeout {
            bail!(
                "timed out waiting for HLS segments at {}",
                playlist.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn stop(runner: &mut Runner) {
    runner.telemetry_stop.0.store(true, Ordering::Relaxed);
    tracing::info!(event = "producer_stop", channel_id = %runner.channel_id, item = %runner.item_key, pid = runner.child.id(), wall_ms = telemetry_wall_ms(), monotonic_ms = runner.telemetry_origin.elapsed().as_millis() as u64);
    stop_child(&mut runner.child);
}

fn telemetry_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// Observation only: FFmpeg owns the files. The segment mtime is its last write,
// while observation time bounds when the playlist replacement became visible.
fn observe_publication(
    playlist: PathBuf,
    channel_id: String,
    item: String,
    pid: u32,
    origin: Instant,
    stop: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut previous = String::new();
        let mut seen = HashSet::new();
        while !stop.load(Ordering::Relaxed) {
            if let Ok(body) = tokio::fs::read_to_string(&playlist).await
                && body != previous
            {
                let wall_ms = telemetry_wall_ms();
                let monotonic_ms = origin.elapsed().as_millis() as u64;
                tracing::info!(event = "playlist_observed", %channel_id, %item, pid, wall_ms, monotonic_ms);
                for line in body
                    .lines()
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                {
                    // Only log safe local segment names, never paths or URLs.
                    if line.contains('/')
                        || line.contains('\\')
                        || line.contains('?')
                        || line.contains(':')
                    {
                        continue;
                    }
                    if seen.insert(line.to_owned()) {
                        let modified_ms = playlist
                            .parent()
                            .and_then(|dir| std::fs::metadata(dir.join(line)).ok())
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64);
                        tracing::info!(event = "segment_published", %channel_id, observer_item = %item, observer_pid = pid, segment = line, modified_ms, wall_ms, monotonic_ms);
                    }
                }
                previous = body;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_dvr(
    pool: SqlitePool,
    data_dir: PathBuf,
    channel: Channel,
    current: ChannelItem,
    window_ms: i64,
    stop: Arc<AtomicBool>,
    last_access: Arc<AtomicI64>,
    dir: PathBuf,
    playlist: PathBuf,
    cloud: Arc<CloudManager>,
    max_kbps: i64,
) -> Result<()> {
    let items = crate::db::channel_items_from(&pool, &channel.id, current.start_ms).await?;
    let mut state = DvrState {
        segments: Vec::new(),
        media_sequence: 0,
        discontinuity_sequence: 0,
        window_ms,
    };
    let mut sequence = 0usize;
    for item in items {
        if stop.load(Ordering::Relaxed) || now_ms() - last_access.load(Ordering::Relaxed) > 120_000
        {
            break;
        }
        let resolved = match cloud.resolve_media_input(&item.media_id).await {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(%error, "DVR media resolution failed");
                continue;
            }
        };
        let out_ms = item
            .out_ms
            .unwrap_or(item.in_ms + (item.end_ms - item.start_ms).max(0));
        if out_ms <= item.in_ms {
            continue;
        }
        let in_ms = item.in_ms;
        let duration_ms = out_ms - in_ms;
        let item_dir = dir.join(format!("item_{sequence}"));
        sequence += 1;
        std::fs::create_dir_all(&item_dir)?;
        let watermark = match channel.graphics.watermark_media_id.as_deref() {
            Some(media_id) if !media_id.is_empty() => {
                crate::db::media_path(&pool, media_id).await?
            }
            _ => None,
        };
        let watermark_path = watermark.as_deref().map(Path::new);
        let (item_playlist, mut child) = match start_producer(
            &channel,
            &format!("dvr_{number}_{sequence}", number = channel.number),
            &item.media_id,
            Path::new(&resolved.location),
            in_ms,
            duration_ms,
            &item_dir,
            &data_dir,
            true,
            &resolved.kind,
            watermark_path,
            max_kbps,
            false,
        ) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "DVR item producer failed");
                let _ = std::fs::remove_dir_all(&item_dir);
                continue;
            }
        };
        if wait_for_ready(&item_playlist, Duration::from_secs(20), 1)
            .await
            .is_err()
        {
            stop_child(&mut child);
            let _ = std::fs::remove_dir_all(&item_dir);
            continue;
        }
        let mut processed: HashSet<String> = HashSet::new();
        let mut boundary = true;
        loop {
            if stop.load(Ordering::Relaxed)
                || now_ms() - last_access.load(Ordering::Relaxed) > 120_000
            {
                stop_child(&mut child);
                break;
            }
            if let Ok(body) = tokio::fs::read_to_string(&item_playlist).await {
                for (duration_seconds, source) in parse_hls_entries(&body) {
                    if !processed.insert(source.clone()) {
                        continue;
                    }
                    let source_path = item_dir.join(&source);
                    if !source_path.is_file() {
                        continue;
                    }
                    let file = format!(
                        "seg_{:06}.ts",
                        state.segments.len() + state.media_sequence as usize
                    );
                    let target = dir.join(&file);
                    if std::fs::copy(&source_path, &target).is_err() {
                        continue;
                    }
                    state.segments.push(DvrSegment {
                        file,
                        duration_ms: (duration_seconds * 1000.0).round() as i64,
                        discontinuity: boundary,
                    });
                    boundary = false;
                    prune_dvr(&mut state, &dir);
                    write_dvr_playlist(&state, &playlist)?;
                }
            }
            if child.try_wait().ok().flatten().is_some() {
                if let Ok(body) = tokio::fs::read_to_string(&item_playlist).await {
                    for (duration_seconds, source) in parse_hls_entries(&body) {
                        if !processed.insert(source.clone()) {
                            continue;
                        }
                        let source_path = item_dir.join(&source);
                        if !source_path.is_file() {
                            continue;
                        }
                        let file = format!(
                            "seg_{:06}.ts",
                            state.segments.len() + state.media_sequence as usize
                        );
                        let target = dir.join(&file);
                        if std::fs::copy(&source_path, &target).is_ok() {
                            state.segments.push(DvrSegment {
                                file,
                                duration_ms: (duration_seconds * 1000.0).round() as i64,
                                discontinuity: boundary,
                            });
                            boundary = false;
                            prune_dvr(&mut state, &dir);
                            write_dvr_playlist(&state, &playlist)?;
                        }
                    }
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        stop_child(&mut child);
        let _ = std::fs::remove_dir_all(&item_dir);
    }
    Ok(())
}

fn parse_hls_entries(body: &str) -> Vec<(f64, String)> {
    let mut entries = Vec::new();
    let mut duration = None;
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            duration = value
                .split(',')
                .next()
                .and_then(|value| value.parse::<f64>().ok());
        } else if !line.starts_with('#')
            && line.trim_end().ends_with(".ts")
            && let Some(duration) = duration.take()
        {
            entries.push((duration, line.trim().to_string()));
        }
    }
    entries
}

fn prune_dvr(state: &mut DvrState, dir: &Path) {
    let mut total: i64 = state
        .segments
        .iter()
        .map(|segment| segment.duration_ms)
        .sum();
    while total > state.window_ms && state.segments.len() > 1 {
        let removed = state.segments.remove(0);
        state.media_sequence += 1;
        if removed.discontinuity {
            state.discontinuity_sequence += 1;
        }
        total -= removed.duration_ms;
        let _ = std::fs::remove_file(dir.join(&removed.file));
    }
}

fn write_dvr_playlist(state: &DvrState, playlist: &Path) -> Result<()> {
    let target = state
        .segments
        .iter()
        .map(|segment| (segment.duration_ms as f64 / 1000.0).ceil() as i64)
        .max()
        .unwrap_or(4)
        .max(1);
    let mut body = format!(
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-DISCONTINUITY-SEQUENCE:{}\n",
        state.media_sequence, state.discontinuity_sequence
    );
    for segment in &state.segments {
        if segment.discontinuity {
            body.push_str("#EXT-X-DISCONTINUITY\n");
        }
        body.push_str(&format!(
            "#EXTINF:{:.3},\n{}\n",
            segment.duration_ms as f64 / 1000.0,
            segment.file
        ));
    }
    let temp = playlist.with_extension("m3u8.tmp");
    std::fs::write(&temp, body)?;
    std::fs::rename(temp, playlist)?;
    Ok(())
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn playlist_lag_ms(path: &Path) -> Option<i64> {
    let body = std::fs::read_to_string(path).ok()?;
    let target_ms = body
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-TARGETDURATION:"))
        .and_then(|value| value.trim().parse::<f64>().ok())?
        * 1000.0;
    let last = body
        .lines()
        .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
        .next_back()?
        .trim();
    let normalized =
        if last.len() >= 5 && matches!(&last[last.len() - 5..last.len() - 4], "+" | "-") {
            let base = &last[..last.len() - 5];
            let sign = &last[last.len() - 5..last.len() - 4];
            let hours = &last[last.len() - 4..last.len() - 2];
            let minutes = &last[last.len() - 2..];
            format!("{base}{sign}{hours}:{minutes}")
        } else {
            last.to_string()
        };
    let parsed =
        time::OffsetDateTime::parse(&normalized, &time::format_description::well_known::Rfc3339)
            .ok()?;
    let age_ms = (time::OffsetDateTime::now_utc() - parsed).whole_milliseconds() as i64;
    Some(age_ms + target_ms as i64)
}

fn clear_hls_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("ts") | Some("m3u8")
        ) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn trim_window(item: &ChannelItem, now: i64) -> (i64, Option<i64>) {
    let elapsed = (now - item.start_ms).max(0);
    let in_point = (item.in_ms + elapsed).max(0);
    (in_point, item.out_ms.map(|out| (out - in_point).max(0)))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn playout_snapshot_is_generation_scoped_and_safe_to_serialize() {
        use super::*;
        use private_publisher::{SegmentIdentity, SegmentSource};
        let root = std::env::temp_dir().join(format!("playout-status-{}", uuid::Uuid::new_v4()));
        let pool = crate::db::init(&root).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Engine::new(pool, root.clone(), cloud, 0);
        let none = serde_json::to_value(engine.playout_status("channel").await).unwrap();
        assert_eq!(none["generation"], serde_json::Value::Null);
        assert_eq!(none["state"], "preparing");
        assert_eq!(none["segments"], serde_json::json!([]));
        let old: SnapshotHandle = Default::default();
        let current: SnapshotHandle = Default::default();
        let sources = [
            SegmentSource::Unknown,
            SegmentSource::Media("A".into()),
            SegmentSource::Slate,
            SegmentSource::Media("B".into()),
        ];
        current.lock().unwrap().segments = sources
            .into_iter()
            .enumerate()
            .map(|(i, source)| SegmentIdentity {
                sequence: i as u64,
                start_pdt_ns: (10_000 + i as i128 * 1000) * 1_000_000,
                end_pdt_ns: (11_000 + i as i128 * 1000) * 1_000_000,
                start_90k: (10_000 + i as i128 * 1000) * 90,
                end_90k: (11_000 + i as i128 * 1000) * 90,
                source,
            })
            .collect();
        current.lock().unwrap().new_published_seconds = 3.0;
        engine.private_runtimes.lock().await.insert(
            "channel".into(),
            PrivateRuntime {
                generation: "new".into(),
                snapshot: current,
                ready: Arc::new(AtomicBool::new(true)),
                failed_at_ms: Arc::new(AtomicI64::new(0)),
                stop: Arc::new(AtomicBool::new(false)),
                task: tokio::spawn(std::future::pending()),
                last_access_ms: 0,
                channel_number: "1".into(),
                channel_name: "test".into(),
                playlist: root.join("secret-token"),
            },
        );
        old.lock().unwrap().segments.push(SegmentIdentity {
            sequence: 99,
            start_pdt_ns: 0,
            end_pdt_ns: 1,
            start_90k: 0,
            end_90k: 1,
            source: SegmentSource::Slate,
        });
        let value = serde_json::to_value(engine.playout_status("channel").await).unwrap();
        assert_eq!(value["generation"], "new");
        assert_eq!(value["state"], "ready");
        assert_eq!(value["new_published_seconds"], 3.0);
        assert_eq!(value["segments"].as_array().unwrap().len(), 4);
        assert_eq!(value["segments"][0]["source"], "unknown");
        assert_eq!(value["segments"][1]["media_id"], "A");
        assert_eq!(value["segments"][2]["source"], "slate");
        assert_eq!(value["segments"][3]["media_id"], "B");
        assert_eq!(value["segments"][1]["sequence"], "1");
        assert_eq!(value["segments"][1]["start_ms"], 11000);
        assert!(!value.to_string().contains("secret-token"));
        engine
            .private_runtimes
            .lock()
            .await
            .remove("channel")
            .unwrap()
            .task
            .abort();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn subtitle_worker_keeps_cache_until_ffmpeg_exits() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("cache-subtitle-{}", uuid::Uuid::new_v4()));
        let cache = crate::precache::LeaseTestCache::new(root.join("cache"), &["one"]);
        let fifo = root.join("input.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            super::ensure_subtitle_file(
                &fifo,
                "item",
                0,
                &root,
                Some(InputLease::Full(cache.lease("one")))
            )
            .is_none()
        );
        assert!(!cache.pressure());
        std::thread::spawn(move || {
            let _ = std::fs::write(fifo, b"not media");
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while root.join("subtitles/item-0.lock").exists() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        while !cache.pressure() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!root.join("cache/one").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_probe_keeps_cache_until_real_process_exits() {
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("cache-probe-{}", uuid::Uuid::new_v4()));
        let cache = crate::precache::LeaseTestCache::new(root.join("cache"), &["one"]);
        let fifo = root.join("input.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let input = fifo.clone();
        let lease = cache.lease("one");
        super::probe_with_deadline(
            Some(InputLease::Full(lease)),
            Duration::from_millis(150),
            move || {
                let mut child = std::process::Command::new("ffprobe")
                    .args(["-v", "quiet", "-i"])
                    .arg(input)
                    .spawn()
                    .unwrap();
                started_tx.send(()).unwrap();
                let _ = child.wait();
                done_tx.send(()).unwrap();
                super::StreamProbe::default()
            },
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!cache.pressure());
        std::thread::spawn(move || {
            let _ = std::fs::write(fifo, b"not media");
        });
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The worker may still be returning after signalling process completion.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !cache.pressure() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!root.join("cache/one").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    use super::*;

    #[test]
    fn private_prebuffer_config_thresholds() {
        assert_eq!(prebuffer_segments(8, 4), 2);
        assert_eq!(prebuffer_segments(0, 2), 1);
        assert_eq!(prebuffer_segments(-1, 2), 1);
        assert_eq!(prebuffer_segments(5, 2), 3);
        assert_eq!(prebuffer_segments(i64::MAX, 2), 4);
    }

    #[tokio::test]
    async fn private_prebuffer_wait_is_bounded_and_requires_playlist() {
        let dir = std::env::temp_dir().join(format!("prebuffer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let playlist = dir.join("index.m3u8");
        std::fs::write(&playlist, "#EXTM3U\n#EXTINF:2,\nseg.ts\n").unwrap();
        let ready = AtomicBool::new(false);
        assert!(
            wait_for_private_ready(&playlist, &ready, Duration::from_millis(120))
                .await
                .is_err()
        );
        ready.store(true, Ordering::Release);
        assert!(
            wait_for_private_ready(&playlist, &ready, Duration::from_secs(1))
                .await
                .is_ok()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn scheduled_precache_restart_plays_without_sources() {
        let root = std::env::temp_dir().join(format!("cache-playback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "991", "cache proof")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 20;
        channel.stream.prebuffer_seconds = 0;
        channel.stream.preset = "ultrafast".into();
        channel.stream.width = 160;
        channel.stream.height = 90;
        let future = now_ms() + 60_000;
        for (position, id, color, frequency) in
            [(0_i64, "first", "red", 440), (1, "second", "blue", 880)]
        {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30:d=20"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=20"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',20000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,20000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(future + position * 20000)
                .bind(future + (position + 1) * 20000).execute(&pool).await.unwrap();
        }
        let mut config = crate::config::Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        config.precache_enabled = true;
        config.precache_max_gb = 1;
        let cache = Arc::new(
            crate::precache::Precache::new(&config, pool.clone()).isolated_from_host_readers(),
        );
        cache.start();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let one = crate::db::precache_entry(&pool, "first").await.unwrap();
                let two = crate::db::precache_entry(&pool, "second").await.unwrap();
                if [one, two]
                    .iter()
                    .all(|e| e.as_ref().is_some_and(|e| e.status == "ready"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("actual precache worker did not finish");
        // Simulate persisted legacy rows, then let the actual restarted worker migrate them
        // while a reader holds a lease. The sources are generated disposable fixtures.
        sqlx::query(
            "UPDATE precache_entries SET source_mtime = 0 WHERE media_id IN ('first','second')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let restarted = Arc::new(
            crate::precache::Precache::new(&config, pool.clone()).isolated_from_host_readers(),
        );
        let row = crate::db::precache_entry(&pool, "first")
            .await
            .unwrap()
            .unwrap();
        let old = crate::precache::SourceIdentity::new(&row.source_path, row.source_size, 0);
        let lease = restarted
            .acquire_ready("first", &old)
            .await
            .unwrap()
            .expect("legacy lease");
        restarted.start();
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let first = crate::db::precache_entry(&pool, "first")
                    .await
                    .unwrap()
                    .unwrap();
                let second = crate::db::precache_entry(&pool, "second")
                    .await
                    .unwrap()
                    .unwrap();
                if first.source_mtime > 0 && second.source_mtime > 0 {
                    let identity = crate::precache::SourceIdentity::new(
                        &first.source_path,
                        first.source_size,
                        first.source_mtime,
                    );
                    assert!(lease.path().exists());
                    assert!(
                        restarted
                            .acquire_ready("first", &identity)
                            .await
                            .unwrap()
                            .is_some(),
                        "new lease after legacy migration"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("worker did not migrate legacy metadata");
        for id in ["first", "second"] {
            std::fs::remove_file(root.join(format!("{id}.mp4"))).unwrap();
        }
        let start = now_ms();
        sqlx::query("UPDATE channel_items SET start_ms = ? + position * 20000, end_ms = ? + (position + 1) * 20000 WHERE channel_id = ?")
            .bind(start).bind(start).bind(&channel.id).execute(&pool).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine =
            Arc::new(Engine::new(pool.clone(), root.clone(), cloud, 0).with_precache(restarted));
        let playlist = engine.ensure(&channel).await.unwrap();
        let text = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let text = std::fs::read_to_string(&playlist).unwrap_or_default();
                let duration: f64 = text
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("#EXTINF:")
                            .and_then(|v| v.split(',').next())
                            .and_then(|v| v.parse::<f64>().ok())
                    })
                    .sum();
                if duration >= 30.0 && text.contains("#EXT-X-DISCONTINUITY") {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("cached playlist did not progress");
        let duration: f64 = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("#EXTINF:")
                    .and_then(|v| v.split(',').next())
                    .and_then(|v| v.parse::<f64>().ok())
            })
            .sum();
        assert!(duration >= 30.0, "playlist duration: {duration}");
        let mut colors = Vec::new();
        let mut frequencies = Vec::new();
        for name in text
            .lines()
            .filter(|l| l.starts_with("segment_") && l.ends_with(".ts"))
        {
            let segment = playlist.parent().unwrap().join(name);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&segment)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            colors.push(video.stdout);
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&segment)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success() && audio.stdout.len() > 40000);
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            frequencies.push(samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count());
        }
        assert!(
            colors.iter().any(|c| c[0] > c[2].saturating_add(50))
                && colors.iter().any(|c| c[2] > c[0].saturating_add(50)),
            "colors: {colors:?}"
        );
        assert!(
            colors
                .iter()
                .all(|c| c[0] > c[2].saturating_add(50) || c[2] > c[0].saturating_add(50)),
            "unexpected slate/video: {colors:?}"
        );
        assert!(
            frequencies
                .iter()
                .all(|f| (350..530).contains(f) || (750..1000).contains(f)),
            "unexpected slate/audio: {frequencies:?}"
        );
        assert!(
            frequencies.iter().any(|f| (350..530).contains(f))
                && frequencies.iter().any(|f| (750..1000).contains(f)),
            "frequencies: {frequencies:?}"
        );
        engine.stop_all().await;
        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn channel_runtime_publishes_local_slate_local_in_order() {
        let root = std::env::temp_dir().join(format!("channel-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color, frequency) in [("first", "red", 440), ("second", "blue", 880)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30000/1001:d=4.2"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=4.2"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',4000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "991", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 12;
        channel.stream.preset = "ultrafast".into();
        sqlx::query("UPDATE channels SET stream_width=160, stream_height=90, stream_preset='ultrafast', segment_seconds=2, cache_segments=12 WHERE id=?")
            .bind(&channel.id).execute(&pool).await.unwrap();
        let start = now_ms();
        for (position, id) in [(0_i64, "first"), (1, "second")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,4000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(start + position * 8000)
                .bind(start + (position * 2 + 1) * 4000).execute(&pool).await.unwrap();
        }
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Arc::new(Engine::new(pool, root.clone(), cloud, 0));
        let playlist = engine.ensure(&channel).await.unwrap();
        let mut previous = 0_u64;
        let mut observed = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&playlist).await {
                    let sequence = text
                        .lines()
                        .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    assert!(sequence >= previous);
                    previous = sequence;
                    if text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:")
                        && text.matches("#EXT-X-DISCONTINUITY\n").count() >= 2
                        && text.matches("#EXTINF:").count() >= 5
                    {
                        observed = text;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let before = observed.split("#EXT-X-DISCONTINUITY\n").next().unwrap();
        assert!(
            before.matches("#EXTINF:").count() >= 2,
            "first item tail missing: {observed}"
        );
        let dates: Vec<_> = observed
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .collect();
        assert!(dates.len() >= 5 && dates.windows(2).all(|pair| pair[0] < pair[1]));
        for segment in observed
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            assert!(
                std::fs::metadata(playlist.parent().unwrap().join(segment))
                    .unwrap()
                    .len()
                    > 0
            );
        }
        // Decode the last segment before each boundary and the first after it.
        let groups: Vec<Vec<&str>> = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .map(|group| {
                group
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect()
            })
            .collect();
        assert!(groups.len() >= 3 && groups.iter().take(3).all(|g| !g.is_empty()));
        for (name, expected) in [
            (groups[0].last().unwrap(), 0),
            (&groups[1][0], 1),
            (groups[1].last().unwrap(), 1),
            (&groups[2][0], 2),
        ] {
            let path = playlist.parent().unwrap().join(name);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            let rgb = &video.stdout;
            match expected {
                0 => assert!(rgb[0] > rgb[2] + 50, "red: {rgb:?}"),
                2 => assert!(rgb[2] > rgb[0] + 50, "blue: {rgb:?}"),
                _ => assert!(rgb[0] < 80 && rgb[2] < 80, "slate: {rgb:?}"),
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(samples.len() > 20000);
            let crossings = samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            let peak = samples.iter().map(|v| (*v as i32).abs()).max().unwrap();
            match expected {
                0 => assert!((350..530).contains(&crossings), "440 Hz: {crossings}"),
                2 => assert!((750..1000).contains(&crossings), "880 Hz: {crossings}"),
                _ => assert!(peak < 100, "slate audio: {peak}"),
            }
        }
        let retained: Vec<_> = observed
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .map(|name| {
                (
                    name.to_owned(),
                    std::fs::read(playlist.parent().unwrap().join(name)).unwrap(),
                )
            })
            .collect();
        engine.refresh(&channel.id).await;
        let stopped_playlist = std::fs::read_to_string(&playlist).unwrap();
        for (name, _) in &retained {
            assert!(stopped_playlist.contains(name));
        }
        for (name, bytes) in &retained {
            assert_eq!(
                &std::fs::read(playlist.parent().unwrap().join(name)).unwrap(),
                bytes
            );
        }
        let restarted = engine.ensure(&channel).await.unwrap();
        assert_eq!(restarted, playlist);
        let after = std::fs::read_to_string(&playlist).unwrap();
        let before_sequence = observed
            .lines()
            .find(|l| l.starts_with("#EXT-X-MEDIA-SEQUENCE:"))
            .unwrap();
        let sequence = |line: &str| line.split_once(':').unwrap().1.parse::<u64>().unwrap();
        assert!(
            sequence(
                after
                    .lines()
                    .find(|l| l.starts_with("#EXT-X-MEDIA-SEQUENCE:"))
                    .unwrap()
            ) >= sequence(before_sequence)
        );
        for (name, bytes) in &retained {
            assert_eq!(
                &std::fs::read(playlist.parent().unwrap().join(name)).unwrap(),
                bytes
            );
        }
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn channel_runtime_replaces_failed_encoder_with_slate_and_resumes() {
        let root = std::env::temp_dir().join(format!("channel-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color, frequency) in [("first", "red", 440), ("second", "blue", 880)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30000/1001:d=4.2"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=4.2"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',4000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "991", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 12;
        channel.stream.preset = "ultrafast".into();
        sqlx::query("UPDATE channels SET stream_width=160, stream_height=90, stream_preset='ultrafast', segment_seconds=2, cache_segments=12 WHERE id=?")
            .bind(&channel.id).execute(&pool).await.unwrap();
        let start = now_ms();
        for (position, id) in [(0_i64, "first"), (1, "second")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,4000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(start + position * 8000)
                .bind(start + (position * 2 + 1) * 4000).execute(&pool).await.unwrap();
        }
        let broken = root.join("broken.mp4");
        std::fs::write(&broken, b"not a media file").unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('broken',?,'broken','movie',4000,0)")
            .bind(broken.to_str().unwrap()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,2,'broken','broken',?,?,0,4000,'program')")
            .bind(&channel.id).bind(start + 4000).bind(start + 8000).execute(&pool).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Arc::new(Engine::new(pool, root.clone(), cloud, 0));
        let playlist = engine.ensure(&channel).await.unwrap();
        let mut previous = 0_u64;
        let mut observed = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&playlist).await {
                    let sequence = text
                        .lines()
                        .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    assert!(sequence >= previous);
                    previous = sequence;
                    if text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:")
                        && text.matches("#EXT-X-DISCONTINUITY\n").count() >= 2
                        && text.matches("#EXTINF:").count() >= 5
                    {
                        observed = text;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let before = observed.split("#EXT-X-DISCONTINUITY\n").next().unwrap();
        assert!(
            before.matches("#EXTINF:").count() >= 2,
            "first item tail missing: {observed}"
        );
        let dates: Vec<_> = observed
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .collect();
        assert!(dates.len() >= 5 && dates.windows(2).all(|pair| pair[0] < pair[1]));
        for segment in observed
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            assert!(
                std::fs::metadata(playlist.parent().unwrap().join(segment))
                    .unwrap()
                    .len()
                    > 0
            );
        }
        // Decode the last segment before each boundary and the first after it.
        let groups: Vec<Vec<&str>> = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .map(|group| {
                group
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect()
            })
            .collect();
        assert!(groups.len() >= 3 && groups.iter().take(3).all(|g| !g.is_empty()));
        let guide = crate::db::guide_blocks(&engine.pool, &channel.id, start, start + 12_000)
            .await
            .unwrap();
        assert_eq!(
            guide
                .iter()
                .map(|item| item.media_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "broken", "second"]
        );
        assert_eq!(
            guide.iter().map(|item| item.start_ms).collect::<Vec<_>>(),
            vec![start, start + 4_000, start + 8_000]
        );
        let lag = now_ms() - (start + 8_000);
        assert!(
            (0..8_000).contains(&lag),
            "resume publication lag: {lag} ms"
        );
        eprintln!("scheduled resume publication lag: {lag} ms");
        for (index, expected) in [(1, start + 4_000), (2, start + 8_000)] {
            let group = observed.split("#EXT-X-DISCONTINUITY\n").nth(index).unwrap();
            let pdt = group
                .lines()
                .find_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
                .unwrap();
            let actual =
                time::OffsetDateTime::parse(pdt, &time::format_description::well_known::Rfc3339)
                    .unwrap()
                    .unix_timestamp_nanos()
                    / 1_000_000;
            assert!(
                (actual - expected as i128).abs() <= 1000,
                "scheduled boundary {expected}, playlist {actual}"
            );
        }
        for (name, expected) in [(&groups[0][0], 0), (&groups[1][0], 1), (&groups[2][0], 2)] {
            let path = playlist.parent().unwrap().join(name);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            let rgb = &video.stdout;
            match expected {
                0 => assert!(rgb[0] > rgb[2] + 50, "red: {rgb:?}"),
                2 => assert!(rgb[2] > rgb[0] + 50, "blue: {rgb:?}"),
                _ => assert!(rgb[0] < 80 && rgb[2] < 80, "slate: {rgb:?}"),
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(samples.len() > 20000);
            let crossings = samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            let peak = samples.iter().map(|v| (*v as i32).abs()).max().unwrap();
            match expected {
                0 => assert!((350..530).contains(&crossings), "440 Hz: {crossings}"),
                2 => assert!((750..1000).contains(&crossings), "880 Hz: {crossings}"),
                _ => assert!(peak < 100, "slate audio: {peak}"),
            }
        }
        assert!(
            !engine.private_runtimes.lock().await[&channel.id]
                .task
                .is_finished()
        );
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn channel_runtime_recovers_from_removed_scheduled_file() {
        let root =
            std::env::temp_dir().join(format!("channel-missing-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color, frequency) in [("first", "red", 440), ("second", "blue", 880)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30000/1001:d=4.2"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=4.2"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',4000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "991", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 12;
        channel.stream.preset = "ultrafast".into();
        sqlx::query("UPDATE channels SET stream_width=160, stream_height=90, stream_preset='ultrafast', segment_seconds=2, cache_segments=12 WHERE id=?")
            .bind(&channel.id).execute(&pool).await.unwrap();
        let start = now_ms();
        for (position, id) in [(0_i64, "first"), (1, "second")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,4000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(start + position * 8000)
                .bind(start + (position * 2 + 1) * 4000).execute(&pool).await.unwrap();
        }
        let broken = root.join("broken.mp4");
        std::fs::write(&broken, b"fixture").unwrap();
        std::fs::remove_file(&broken).unwrap();
        let failure_at = now_ms();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('broken',?,'broken','movie',4000,0)")
            .bind(broken.to_str().unwrap()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,2,'broken','broken',?,?,0,4000,'program')")
            .bind(&channel.id).bind(start + 4000).bind(start + 8000).execute(&pool).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Arc::new(Engine::new(pool, root.clone(), cloud, 0));
        let playlist = engine.ensure(&channel).await.unwrap();
        let mut previous = 0_u64;
        let mut observed = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&playlist).await {
                    let sequence = text
                        .lines()
                        .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    assert!(sequence >= previous);
                    previous = sequence;
                    if text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:")
                        && text.matches("#EXT-X-DISCONTINUITY\n").count() >= 2
                        && text.matches("#EXTINF:").count() >= 5
                    {
                        observed = text;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let before = observed.split("#EXT-X-DISCONTINUITY\n").next().unwrap();
        assert!(
            before.matches("#EXTINF:").count() >= 2,
            "first item tail missing: {observed}"
        );
        let dates: Vec<_> = observed
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .collect();
        assert!(dates.len() >= 5 && dates.windows(2).all(|pair| pair[0] < pair[1]));
        for segment in observed
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            assert!(
                std::fs::metadata(playlist.parent().unwrap().join(segment))
                    .unwrap()
                    .len()
                    > 0
            );
        }
        // Decode the last segment before each boundary and the first after it.
        let groups: Vec<Vec<&str>> = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .map(|group| {
                group
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect()
            })
            .collect();
        assert!(groups.len() >= 3 && groups.iter().take(3).all(|g| !g.is_empty()));
        let guide = crate::db::guide_blocks(&engine.pool, &channel.id, start, start + 12_000)
            .await
            .unwrap();
        assert_eq!(
            guide
                .iter()
                .map(|item| item.media_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "broken", "second"]
        );
        assert_eq!(
            guide.iter().map(|item| item.start_ms).collect::<Vec<_>>(),
            vec![start, start + 4_000, start + 8_000]
        );
        eprintln!(
            "missing file to resumed publication: {} ms",
            now_ms() - failure_at
        );
        let lag = now_ms() - (start + 8_000);
        assert!(
            (0..8_000).contains(&lag),
            "resume publication lag: {lag} ms"
        );
        eprintln!("scheduled resume publication lag: {lag} ms");
        for (index, expected) in [(1, start + 4_000), (2, start + 8_000)] {
            let group = observed.split("#EXT-X-DISCONTINUITY\n").nth(index).unwrap();
            let pdt = group
                .lines()
                .find_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
                .unwrap();
            let actual =
                time::OffsetDateTime::parse(pdt, &time::format_description::well_known::Rfc3339)
                    .unwrap()
                    .unix_timestamp_nanos()
                    / 1_000_000;
            assert!(
                (actual - expected as i128).abs() <= 1000,
                "scheduled boundary {expected}, playlist {actual}"
            );
        }
        for (name, expected) in [(&groups[0][0], 0), (&groups[1][0], 1), (&groups[2][0], 2)] {
            let path = playlist.parent().unwrap().join(name);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            let rgb = &video.stdout;
            match expected {
                0 => assert!(rgb[0] > rgb[2] + 50, "red: {rgb:?}"),
                2 => assert!(rgb[2] > rgb[0] + 50, "blue: {rgb:?}"),
                _ => assert!(rgb[0] < 80 && rgb[2] < 80, "slate: {rgb:?}"),
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(samples.len() > 20000);
            let crossings = samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            let peak = samples.iter().map(|v| (*v as i32).abs()).max().unwrap();
            match expected {
                0 => assert!((350..530).contains(&crossings), "440 Hz: {crossings}"),
                2 => assert!((750..1000).contains(&crossings), "880 Hz: {crossings}"),
                _ => assert!(peak < 100, "slate audio: {peak}"),
            }
        }
        assert!(
            !engine.private_runtimes.lock().await[&channel.id]
                .task
                .is_finished()
        );
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn channel_runtime_recovers_from_resolved_remote_failure() {
        let root =
            std::env::temp_dir().join(format!("channel-remote-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color, frequency) in [("first", "red", 440), ("second", "blue", 880)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30000/1001:d=4.2"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=4.2"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',4000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "991", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 12;
        channel.stream.preset = "ultrafast".into();
        sqlx::query("UPDATE channels SET stream_width=160, stream_height=90, stream_preset='ultrafast', segment_seconds=2, cache_segments=12 WHERE id=?")
            .bind(&channel.id).execute(&pool).await.unwrap();
        let start = now_ms();
        for (position, id) in [(0_i64, "first"), (1, "second")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,4000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(start + position * 8000)
                .bind(start + (position * 2 + 1) * 4000).execute(&pool).await.unwrap();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicBool::new(false));
        let seen_request = seen.clone();
        let failure_at = Arc::new(AtomicI64::new(0));
        let failure_time = failure_at.clone();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut request = [0_u8; 1024];
                    let _ = stream.read(&mut request);
                    failure_time.store(now_ms(), Ordering::Release);
                    seen_request.store(true, Ordering::Release);
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('broken',?,'broken','movie',4000,0)")
            .bind(format!("http://{address}/gone.mp4")).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,2,'broken','broken',?,?,0,4000,'program')")
            .bind(&channel.id).bind(start + 4000).bind(start + 8000).execute(&pool).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Arc::new(Engine::new(pool, root.clone(), cloud, 0));
        let playlist = engine.ensure(&channel).await.unwrap();
        let mut previous = 0_u64;
        let mut observed = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&playlist).await {
                    let sequence = text
                        .lines()
                        .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    assert!(sequence >= previous);
                    previous = sequence;
                    if text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:")
                        && text.matches("#EXT-X-DISCONTINUITY\n").count() >= 2
                        && text.matches("#EXTINF:").count() >= 5
                    {
                        observed = text;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let before = observed.split("#EXT-X-DISCONTINUITY\n").next().unwrap();
        assert!(
            before.matches("#EXTINF:").count() >= 2,
            "first item tail missing: {observed}"
        );
        let dates: Vec<_> = observed
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .collect();
        assert!(dates.len() >= 5 && dates.windows(2).all(|pair| pair[0] < pair[1]));
        for segment in observed
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            assert!(
                std::fs::metadata(playlist.parent().unwrap().join(segment))
                    .unwrap()
                    .len()
                    > 0
            );
        }
        // Decode the last segment before each boundary and the first after it.
        let groups: Vec<Vec<&str>> = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .map(|group| {
                group
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect()
            })
            .collect();
        assert!(groups.len() >= 3 && groups.iter().take(3).all(|g| !g.is_empty()));
        let guide = crate::db::guide_blocks(&engine.pool, &channel.id, start, start + 12_000)
            .await
            .unwrap();
        assert_eq!(
            guide
                .iter()
                .map(|item| item.media_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "broken", "second"]
        );
        assert_eq!(
            guide.iter().map(|item| item.start_ms).collect::<Vec<_>>(),
            vec![start, start + 4_000, start + 8_000]
        );
        assert!(failure_at.load(Ordering::Acquire) > 0);
        eprintln!(
            "remote failure to resumed publication: {} ms",
            now_ms() - failure_at.load(Ordering::Acquire)
        );
        let lag = now_ms() - (start + 8_000);
        assert!(
            (0..8_000).contains(&lag),
            "resume publication lag: {lag} ms"
        );
        eprintln!("scheduled resume publication lag: {lag} ms");
        for (index, expected) in [(1, start + 4_000), (2, start + 8_000)] {
            let group = observed.split("#EXT-X-DISCONTINUITY\n").nth(index).unwrap();
            let pdt = group
                .lines()
                .find_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
                .unwrap();
            let actual =
                time::OffsetDateTime::parse(pdt, &time::format_description::well_known::Rfc3339)
                    .unwrap()
                    .unix_timestamp_nanos()
                    / 1_000_000;
            assert!(
                (actual - expected as i128).abs() <= 1000,
                "scheduled boundary {expected}, playlist {actual}"
            );
        }
        for (name, expected) in [(&groups[0][0], 0), (&groups[1][0], 1), (&groups[2][0], 2)] {
            let path = playlist.parent().unwrap().join(name);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            let rgb = &video.stdout;
            match expected {
                0 => assert!(rgb[0] > rgb[2] + 50, "red: {rgb:?}"),
                2 => assert!(rgb[2] > rgb[0] + 50, "blue: {rgb:?}"),
                _ => assert!(rgb[0] < 80 && rgb[2] < 80, "slate: {rgb:?}"),
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(samples.len() > 20000);
            let crossings = samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            let peak = samples.iter().map(|v| (*v as i32).abs()).max().unwrap();
            match expected {
                0 => assert!((350..530).contains(&crossings), "440 Hz: {crossings}"),
                2 => assert!((750..1000).contains(&crossings), "880 Hz: {crossings}"),
                _ => assert!(peak < 100, "slate audio: {peak}"),
            }
        }
        assert!(
            !engine.private_runtimes.lock().await[&channel.id]
                .task
                .is_finished()
        );
        assert!(
            seen.load(Ordering::Acquire),
            "FFmpeg never reached the resolved remote URL"
        );
        server.join().unwrap();
        let before = std::fs::read_to_string(&playlist).unwrap();
        {
            let runtimes = engine.private_runtimes.lock().await;
            runtimes[&channel.id].task.abort();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while !engine.private_runtimes.lock().await[&channel.id]
                .task
                .is_finished()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            engine.producer_snapshots().await[0].item,
            "private publisher failed"
        );
        engine.private_runtimes.lock().await[&channel.id]
            .failed_at_ms
            .store(now_ms(), Ordering::Release);
        for _ in 0..3 {
            assert!(
                engine
                    .ensure(&channel)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("backoff")
            );
            assert!(
                engine.private_runtimes.lock().await[&channel.id]
                    .task
                    .is_finished()
            );
        }
        engine.private_runtimes.lock().await[&channel.id]
            .failed_at_ms
            .store(now_ms() - 5_001, Ordering::Release);
        engine.ensure(&channel).await.unwrap();
        let after = std::fs::read_to_string(&playlist).unwrap();
        let old_entries: Vec<_> = before
            .lines()
            .filter(|line| line.starts_with("segment_"))
            .collect();
        let new_entries: Vec<_> = after
            .lines()
            .filter(|line| line.starts_with("segment_"))
            .collect();
        assert!(
            new_entries.starts_with(&old_entries),
            "restart replaced advertised entries"
        );
        assert!(new_entries.len() > old_entries.len());
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn channel_runtime_recovers_partial_generation_after_real_stall() {
        let root = std::env::temp_dir().join(format!("wave7-stall-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("slow-private-input"), b"").unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color, freq, seconds) in [("first", "red", 440, 10), ("second", "blue", 880, 10)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30:d={seconds}"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={freq}:sample_rate=48000:duration={seconds}"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',10000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "991", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 60;
        channel.stream.prebuffer_seconds = 0;
        channel.stream.preset = "ultrafast".into();
        sqlx::query("UPDATE channels SET stream_width=160, stream_height=90, stream_preset='ultrafast', segment_seconds=2, cache_segments=60, prebuffer_seconds=0 WHERE id=?")
            .bind(&channel.id).execute(&pool).await.unwrap();
        let start = now_ms();
        for (position, id) in [(0_i64, "first"), (1, "second")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,10000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(start + position * 10000)
                .bind(start + (position + 1) * 10000).execute(&pool).await.unwrap();
        }
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Arc::new(Engine::new(pool, root.clone(), cloud, 0));
        let playlist = engine.ensure(&channel).await.unwrap();
        let target = root.join("first.mp4");
        let pid = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let text = std::fs::read_to_string(&playlist).unwrap_or_default();
                if text.lines().any(|l| l.starts_with("segment_")) {
                    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                            continue;
                        };
                        if let Ok(args) = std::fs::read(entry.path().join("cmdline"))
                            && args
                                .split(|b| *b == 0)
                                .any(|a| a == target.as_os_str().as_encoded_bytes())
                        {
                            return pid;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("first segment and live encoder");
        let task_id = engine.private_runtimes.lock().await[&channel.id].task.id();
        let before = std::fs::read_to_string(&playlist).unwrap();
        let retained_name = before
            .lines()
            .find(|l| l.starts_with("segment_"))
            .unwrap()
            .to_owned();
        let retained_bytes =
            std::fs::read(playlist.parent().unwrap().join(&retained_name)).unwrap();
        assert!(
            Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let injected = std::time::Instant::now();
        let observed = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let text = std::fs::read_to_string(&playlist).unwrap_or_default();
                if text.matches("#EXT-X-DISCONTINUITY\n").count() >= 2 {
                    break text;
                }
                assert!(
                    !engine.private_runtimes.lock().await[&channel.id]
                        .task
                        .is_finished(),
                    "runtime terminated during recovery"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("slate and next source publication");
        let elapsed = injected.elapsed();
        eprintln!(
            "stall to next-source publication: {} ms",
            elapsed.as_millis()
        );
        assert!(elapsed >= Duration::from_secs(21)); // FIFO stall timeout is 22s
        assert!(observed.lines().any(|line| line == retained_name));
        assert_eq!(
            std::fs::read(playlist.parent().unwrap().join(&retained_name)).unwrap(),
            retained_bytes
        );
        assert!(observed.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        let mut previous_end: Option<i128> = None;
        let mut date: Option<i128> = None;
        let mut duration: Option<i128> = None;
        for line in observed.lines() {
            if let Some(v) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
                date = Some(
                    time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339)
                        .unwrap()
                        .unix_timestamp_nanos(),
                );
            } else if let Some(v) = line.strip_prefix("#EXTINF:") {
                duration = Some(
                    (v.trim_end_matches(',').parse::<f64>().unwrap() * 1_000_000_000.0).round()
                        as i128,
                );
            } else if line.starts_with("segment_") {
                let current = date.take().unwrap();
                if let Some(end) = previous_end {
                    assert!((current - end).abs() <= 2_000_000, "PDT gap");
                }
                previous_end = Some(current + duration.take().unwrap());
            }
        }
        let groups: Vec<Vec<_>> = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .map(|g| g.lines().filter(|l| l.starts_with("segment_")).collect())
            .collect();
        assert!(
            groups.len() >= 3 && groups.iter().take(3).all(|g| !g.is_empty()),
            "{observed}"
        );
        for (index, expected) in [(0, 0), (1, 1), (2, 2)] {
            let path = playlist.parent().unwrap().join(groups[index][0]);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            let rgb = &video.stdout;
            match expected {
                0 => assert!(rgb[0] > rgb[2] + 50, "red {rgb:?}"),
                1 => assert!(rgb[0] < 80 && rgb[2] < 80, "slate {rgb:?}"),
                _ => assert!(rgb[2] > rgb[0] + 50, "blue {rgb:?}"),
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(samples.len() > 20000);
            let crossings = samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            let peak = samples.iter().map(|v| (*v as i32).abs()).max().unwrap();
            match expected {
                0 => assert!((350..530).contains(&crossings)),
                1 => assert!(peak < 100),
                _ => assert!((750..1000).contains(&crossings), "{crossings}"),
            }
        }
        let guide = crate::db::guide_blocks(&engine.pool, &channel.id, start, start + 20000)
            .await
            .unwrap();
        assert_eq!(
            guide
                .iter()
                .map(|i| i.media_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(guide[1].start_ms, start + 10000);
        let pdt = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .nth(2)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .unwrap();
        let date = time::OffsetDateTime::parse(pdt, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .unix_timestamp_nanos()
            / 1_000_000;
        assert!(
            (date - guide[1].start_ms as i128).abs() <= 1000,
            "guide {} PDT {date}",
            guide[1].start_ms
        );
        {
            let runtimes = engine.private_runtimes.lock().await;
            let runtime = &runtimes[&channel.id];
            assert_eq!(runtime.task.id(), task_id, "runtime restarted");
            assert!(!runtime.task.is_finished());
            assert!(runtime.ready.load(Ordering::Acquire));
            assert_eq!(runtime.failed_at_ms.load(Ordering::Acquire), 0);
        }
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn channel_runtime_recovers_after_encoder_kill() {
        let root =
            std::env::temp_dir().join(format!("channel-kill-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color, frequency) in [("first", "red", 440), ("second", "blue", 880)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30000/1001:d=4.2"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=4.2"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
                        "-pix_fmt",
                        "yuv420p",
                        "-c:a",
                        "aac",
                        "-shortest",
                        "-y"
                    ])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',4000,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "991", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 12;
        channel.stream.prebuffer_seconds = 0;
        channel.stream.preset = "ultrafast".into();
        sqlx::query("UPDATE channels SET stream_width=160, stream_height=90, stream_preset='ultrafast', segment_seconds=2, cache_segments=12, prebuffer_seconds=0 WHERE id=?")
            .bind(&channel.id).execute(&pool).await.unwrap();
        let start = now_ms();
        for (position, id) in [(0_i64, "first"), (1, "second")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,4000,'program')")
                .bind(&channel.id).bind(position).bind(id).bind(id).bind(start + position * 8000)
                .bind(start + (position * 2 + 1) * 4000).execute(&pool).await.unwrap();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicBool::new(false));
        let seen_request = seen.clone();
        let server = std::thread::spawn(move || {
            use std::io::Read;
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut request = [0_u8; 1024];
                    let _ = stream.read(&mut request);
                    seen_request.store(true, Ordering::Release);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                    let _ = stream.read(&mut request);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('broken',?,'broken','movie',4000,0)")
            .bind(format!("http://{address}/gone.mp4")).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,2,'broken','broken',?,?,0,4000,'program')")
            .bind(&channel.id).bind(start + 4000).bind(start + 8000).execute(&pool).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Arc::new(Engine::new(pool, root.clone(), cloud, 0));
        let playlist = engine.ensure(&channel).await.unwrap();
        let target = format!("http://{address}/gone.mp4");
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                    let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                        continue;
                    };
                    if let Ok(args) = std::fs::read(entry.path().join("cmdline"))
                        && args.split(|b| *b == 0).any(|arg| arg == target.as_bytes())
                    {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("remote encoder was not started");
        assert!(
            Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let failure_at = now_ms();
        let mut previous = 0_u64;
        let mut observed = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&playlist).await {
                    let sequence = text
                        .lines()
                        .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    assert!(sequence >= previous);
                    previous = sequence;
                    if text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:")
                        && text.matches("#EXT-X-DISCONTINUITY\n").count() >= 2
                        && text.matches("#EXTINF:").count() >= 5
                    {
                        observed = text;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let before = observed.split("#EXT-X-DISCONTINUITY\n").next().unwrap();
        assert!(
            before.matches("#EXTINF:").count() >= 2,
            "first item tail missing: {observed}"
        );
        let dates: Vec<_> = observed
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .collect();
        assert!(dates.len() >= 5 && dates.windows(2).all(|pair| pair[0] < pair[1]));
        for segment in observed
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            assert!(
                std::fs::metadata(playlist.parent().unwrap().join(segment))
                    .unwrap()
                    .len()
                    > 0
            );
        }
        // Decode the last segment before each boundary and the first after it.
        let groups: Vec<Vec<&str>> = observed
            .split("#EXT-X-DISCONTINUITY\n")
            .map(|group| {
                group
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect()
            })
            .collect();
        assert!(groups.len() >= 3 && groups.iter().take(3).all(|g| !g.is_empty()));
        let guide = crate::db::guide_blocks(&engine.pool, &channel.id, start, start + 12_000)
            .await
            .unwrap();
        assert_eq!(
            guide
                .iter()
                .map(|item| item.media_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "broken", "second"]
        );
        assert_eq!(
            guide.iter().map(|item| item.start_ms).collect::<Vec<_>>(),
            vec![start, start + 4_000, start + 8_000]
        );
        eprintln!(
            "encoder kill to resumed publication: {} ms",
            now_ms() - failure_at
        );
        let lag = now_ms() - (start + 8_000);
        assert!(
            (0..8_000).contains(&lag),
            "resume publication lag: {lag} ms"
        );
        eprintln!("scheduled resume publication lag: {lag} ms");
        for (index, expected) in [(1, start + 4_000), (2, start + 8_000)] {
            let group = observed.split("#EXT-X-DISCONTINUITY\n").nth(index).unwrap();
            let pdt = group
                .lines()
                .find_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
                .unwrap();
            let actual =
                time::OffsetDateTime::parse(pdt, &time::format_description::well_known::Rfc3339)
                    .unwrap()
                    .unix_timestamp_nanos()
                    / 1_000_000;
            assert!(
                (actual - expected as i128).abs() <= 1000,
                "scheduled boundary {expected}, playlist {actual}"
            );
        }
        for (name, expected) in [(&groups[0][0], 0), (&groups[1][0], 1), (&groups[2][0], 2)] {
            let path = playlist.parent().unwrap().join(name);
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=1:1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(video.status.success() && video.stdout.len() == 3);
            let rgb = &video.stdout;
            match expected {
                0 => assert!(rgb[0] > rgb[2] + 50, "red: {rgb:?}"),
                2 => assert!(rgb[2] > rgb[0] + 50, "blue: {rgb:?}"),
                _ => assert!(rgb[0] < 80 && rgb[2] < 80, "slate: {rgb:?}"),
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-t", "1", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(samples.len() > 20000);
            let crossings = samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            let peak = samples.iter().map(|v| (*v as i32).abs()).max().unwrap();
            match expected {
                0 => assert!((350..530).contains(&crossings), "440 Hz: {crossings}"),
                2 => assert!((750..1000).contains(&crossings), "880 Hz: {crossings}"),
                _ => assert!(peak < 100, "slate audio: {peak}"),
            }
        }
        assert!(
            !engine.private_runtimes.lock().await[&channel.id]
                .task
                .is_finished()
        );
        assert!(
            seen.load(Ordering::Acquire),
            "FFmpeg never reached the hanging remote URL"
        );
        server.join().unwrap();
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_runtime_without_timestamp_observes_backoff() {
        let failed = AtomicI64::new(0);
        assert!(private_restart_backoff(&failed, 100_000));
        assert!(private_restart_backoff(&failed, 104_999));
        assert!(!private_restart_backoff(&failed, 105_000));
    }

    #[tokio::test]
    async fn private_snapshot_and_lag_guard_preserve_running_task() {
        let root = std::env::temp_dir().join(format!("private-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let cloud =
            Arc::new(CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap());
        let engine = Engine::new(pool, root.clone(), cloud, 0);
        let playlist = root.join("index.m3u8");
        std::fs::write(&playlist, "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-PROGRAM-DATE-TIME:2020-01-01T00:00:00Z\n#EXTINF:2,\na.ts\n").unwrap();
        std::fs::write(root.join("a.ts"), b"abc").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(std::future::pending::<()>());
        engine.private_runtimes.lock().await.insert(
            "id".into(),
            PrivateRuntime {
                generation: "test".into(),
                snapshot: Arc::new(std::sync::Mutex::new(PublishedSnapshot::default())),
                ready: Arc::new(AtomicBool::new(true)),
                failed_at_ms: Arc::new(AtomicI64::new(0)),
                stop: stop.clone(),
                task,
                last_access_ms: now_ms(),
                channel_number: "9".into(),
                channel_name: "status".into(),
                playlist: playlist.clone(),
            },
        );
        let first = engine.producer_snapshots().await;
        assert_eq!((first.len(), first[0].segments, first[0].bytes), (1, 1, 3));
        assert_eq!(first[0].item, "private publisher");
        std::fs::write(root.join("b.ts"), b"12345").unwrap();
        let second = engine.producer_snapshots().await;
        assert_eq!((second[0].segments, second[0].bytes), (2, 8));
        let original = std::fs::read(&playlist).unwrap();
        assert!(playlist_lag_ms(&playlist).unwrap() > 1000);
        assert_eq!(engine.resync_lagging(1000).await, 0);
        assert_eq!(std::fs::read(&playlist).unwrap(), original);
        assert!(!stop.load(Ordering::Relaxed));
        assert!(
            !engine.private_runtimes.lock().await["id"]
                .task
                .is_finished()
        );
        engine.private_runtimes.lock().await["id"].task.abort();
        engine.stop_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    fn item(in_ms: i64, out_ms: Option<i64>, start_ms: i64) -> ChannelItem {
        ChannelItem {
            media_id: "m".into(),
            title: "Media".into(),
            start_ms,
            end_ms: start_ms + out_ms.unwrap_or(0) - in_ms,
            in_ms,
            out_ms,
            kind: "program".into(),
        }
    }

    #[test]
    fn segment_numbering_continues_from_the_existing_playlist() {
        let dir = std::env::temp_dir().join(format!(
            "awesometv-hls-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let playlist = dir.join("index.m3u8");
        assert_eq!(
            next_hls_segment_number(&playlist),
            0,
            "missing playlist starts at zero"
        );
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:41\n#EXTINF:4.0,\n1/seg_x_00041.ts\n#EXTINF:4.0,\n1/seg_x_00042.ts\n",
        )
        .unwrap();
        assert_eq!(next_hls_segment_number(&playlist), 43);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn trims_a_program_segment_from_its_in_point() {
        let item = item(300_000, Some(600_000), 1_000);
        let (offset, duration) = trim_window(&item, 1_000);
        assert_eq!(offset, 300_000);
        assert_eq!(duration, Some(300_000));
    }

    #[test]
    fn trims_a_break_to_its_remaining_time() {
        let item = item(0, Some(20_000), 1_000);
        let (offset, duration) = trim_window(&item, 6_000);
        assert_eq!(offset, 5_000);
        assert_eq!(duration, Some(15_000));
    }

    #[test]
    fn clamps_after_the_item_has_ended() {
        let item = item(0, Some(20_000), 1_000);
        let (offset, duration) = trim_window(&item, 60_000);
        assert_eq!(offset, 59_000);
        assert_eq!(duration, Some(0));
    }
}

#[derive(Debug, Default)]
struct StreamProbe {
    audio_index: Option<i64>,
    subtitle_ordinal: Option<i64>,
    hdr: bool,
}

fn media_input(path: &Path) -> String {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("iso"))
    {
        format!("bluray:{}", path.display())
    } else {
        path.display().to_string()
    }
}

fn probe_streams(
    path: &Path,
    stream: &crate::db::StreamSettings,
    lease: Option<InputLease>,
) -> StreamProbe {
    let source = path.to_path_buf();
    let settings = stream.clone();
    probe_with_deadline(lease, Duration::from_secs(6), move || {
        probe_streams_blocking(&source, &settings)
    })
}

fn probe_with_deadline<F>(lease: Option<InputLease>, timeout: Duration, probe: F) -> StreamProbe
where
    F: FnOnce() -> StreamProbe + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _lease = lease;
        let _ = sender.send(probe());
    });
    match receiver.recv_timeout(timeout) {
        Ok(probe) => probe,
        Err(_) => {
            tracing::warn!("ffprobe timed out; using defaults");
            StreamProbe::default()
        }
    }
}

fn probe_streams_blocking(path: &Path, stream: &crate::db::StreamSettings) -> StreamProbe {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg(media_input(path))
        .output();
    let Ok(output) = output else {
        return StreamProbe::default();
    };
    if !output.status.success() {
        return StreamProbe::default();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return StreamProbe::default();
    };
    let streams = value
        .get("streams")
        .and_then(|streams| streams.as_array())
        .cloned()
        .unwrap_or_default();

    let audio_language = stream
        .audio_language
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let audio_index = choose_audio_index(&streams, audio_language);

    let subtitle_language = stream
        .subtitle_language
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut subtitle_ordinal = None;
    let mut ordinal = 0i64;
    for item in &streams {
        if item.get("codec_type").and_then(|value| value.as_str()) != Some("subtitle") {
            continue;
        }
        let codec = item
            .get("codec_name")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let text = matches!(
            codec,
            "subrip" | "ass" | "ssa" | "mov_text" | "webvtt" | "text"
        );
        if text && subtitle_language.is_none_or(|_| language_matches(item, subtitle_language)) {
            subtitle_ordinal = Some(ordinal);
            break;
        }
        ordinal += 1;
    }

    let hdr = streams.iter().any(|item| {
        item.get("codec_type").and_then(|value| value.as_str()) == Some("video")
            && matches!(
                item.get("color_transfer").and_then(|value| value.as_str()),
                Some("smpte2084") | Some("arib-std-b67")
            )
    });

    StreamProbe {
        audio_index,
        subtitle_ordinal,
        hdr,
    }
}

fn choose_audio_index(streams: &[serde_json::Value], wanted: Option<&str>) -> Option<i64> {
    let mut matching = Vec::new();
    let mut fallback = Vec::new();
    let mut has_language_tags = false;
    for item in streams {
        if item.get("codec_type").and_then(|value| value.as_str()) != Some("audio") {
            continue;
        }
        let Some(index) = item.get("index").and_then(|value| value.as_i64()) else {
            continue;
        };
        let language = item
            .get("tags")
            .and_then(|tags| tags.get("language"))
            .and_then(|language| language.as_str());
        if language.is_some() {
            has_language_tags = true;
        }
        let default = item
            .get("disposition")
            .and_then(|disposition| disposition.get("default"))
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
            == 1;
        let commentary = item
            .get("disposition")
            .and_then(|disposition| disposition.get("commentary"))
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
            == 1
            || item
                .get("tags")
                .and_then(|tags| tags.get("title"))
                .and_then(|title| title.as_str())
                .is_some_and(|title| title.to_ascii_lowercase().contains("commentary"));
        let language_ok = wanted.is_none_or(|wanted| {
            language.is_some_and(|language| language_matches_code(language, wanted))
        });
        if language_ok && !commentary {
            matching.push((default, index));
        }
        if !commentary {
            fallback.push((default, index));
        }
    }
    if wanted.is_some() && matching.is_empty() && has_language_tags {
        tracing::warn!(
            wanted = wanted.unwrap_or_default(),
            "no audio track matched the requested language"
        );
    }
    matching
        .iter()
        .max_by_key(|(default, index)| (*default, std::cmp::Reverse(*index)))
        .or_else(|| {
            fallback
                .iter()
                .max_by_key(|(default, index)| (*default, std::cmp::Reverse(*index)))
        })
        .map(|(_, index)| *index)
}

fn language_matches(stream: &serde_json::Value, wanted: Option<&str>) -> bool {
    let Some(wanted) = wanted.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    stream
        .get("tags")
        .and_then(|tags| tags.get("language"))
        .and_then(|language| language.as_str())
        .is_some_and(|language| language_matches_code(language, wanted))
}

fn language_matches_code(language: &str, wanted: &str) -> bool {
    normalize_language(language) == normalize_language(wanted)
}

fn normalize_language(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    let base = value.split(['-', '_']).next().unwrap_or(&value);
    match base {
        "en" | "eng" | "english" => "eng".into(),
        "de" | "deu" | "ger" | "german" => "ger".into(),
        "fr" | "fra" | "fre" | "french" => "fre".into(),
        "es" | "spa" | "spanish" => "spa".into(),
        "it" | "ita" | "italian" => "ita".into(),
        "pt" | "por" | "portuguese" => "por".into(),
        "ja" | "jpn" | "japanese" => "jpn".into(),
        "ko" | "kor" | "korean" => "kor".into(),
        "zh" | "zho" | "chi" | "chinese" => "zho".into(),
        "ru" | "rus" | "russian" => "rus".into(),
        "hi" | "hin" | "hindi" => "hin".into(),
        "ar" | "ara" | "arabic" => "ara".into(),
        "nl" | "nld" | "dut" | "dutch" => "nld".into(),
        "sv" | "swe" | "swedish" => "swe".into(),
        "no" | "nor" | "norwegian" => "nor".into(),
        "da" | "dan" | "danish" => "dan".into(),
        "fi" | "fin" | "finnish" => "fin".into(),
        "pl" | "pol" | "polish" => "pol".into(),
        "tr" | "tur" | "turkish" => "tur".into(),
        other => other.to_string(),
    }
}

fn ensure_subtitle_file(
    media: &Path,
    media_id: &str,
    ordinal: i64,
    data_dir: &Path,
    lease: Option<InputLease>,
) -> Option<PathBuf> {
    if media_id.is_empty() {
        return None;
    }
    let dir = data_dir.join("subtitles");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{media_id}-{ordinal}.srt"));
    if path.is_file()
        && std::fs::metadata(&path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
    {
        return Some(path);
    }
    let lock = dir.join(format!("{media_id}-{ordinal}.lock"));
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .is_err()
    {
        return None;
    }
    let media = media.to_path_buf();
    let output = path.clone();
    let lock_path = lock.clone();
    std::thread::spawn(move || {
        let _lease = lease;
        let status = Command::new("ffmpeg")
            .arg("-v")
            .arg("error")
            .arg("-y")
            .arg("-i")
            .arg(&media)
            .arg("-map")
            .arg(format!("0:s:{ordinal}"))
            .arg("-c:s")
            .arg("srt")
            .arg(&output)
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => {
                if std::fs::metadata(&output)
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
                {
                    tracing::info!(ordinal, "extracted subtitle cache");
                }
            }
            Ok(status) => {
                tracing::warn!(ordinal, %status, "subtitle extraction failed");
            }
            Err(error) => {
                tracing::warn!(ordinal, %error, "subtitle extraction failed");
            }
        }
        let _ = std::fs::remove_file(lock_path);
    });
    None
}

fn shifted_subtitle_file(
    source: &Path,
    offset_ms: i64,
    data_dir: &Path,
    media_id: &str,
    ordinal: i64,
) -> Option<PathBuf> {
    if offset_ms <= 0 {
        return Some(source.to_path_buf());
    }
    let path = data_dir
        .join("subtitles")
        .join(format!("{media_id}-{ordinal}-{offset_ms}.srt"));
    if path.is_file() {
        return Some(path);
    }
    let raw = std::fs::read_to_string(source).ok()?;
    let shifted = shift_srt(&raw, offset_ms);
    std::fs::write(&path, shifted).ok()?;
    Some(path)
}

fn shift_srt(raw: &str, offset_ms: i64) -> String {
    let mut output = String::new();
    let blocks = raw
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut index = 1;
    for block in blocks {
        let mut lines = block.lines();
        let Some(number) = lines.next() else { continue };
        let Some(times) = lines.next() else { continue };
        let Some((start, end)) = parse_srt_range(times) else {
            continue;
        };
        let start = start - offset_ms;
        let end = end - offset_ms;
        if end <= 0 {
            continue;
        }
        let start = start.max(0);
        let text: Vec<&str> = lines.collect();
        if text.is_empty() {
            continue;
        }
        let _ = number;
        output.push_str(&format!(
            "{index}\n{} --> {}\n{}\n\n",
            format_srt_time(start),
            format_srt_time(end),
            text.join("\n")
        ));
        index += 1;
    }
    output
}

fn parse_srt_range(line: &str) -> Option<(i64, i64)> {
    let (start, end) = line.split_once("-->")?;
    Some((
        parse_srt_time(start.trim())?,
        parse_srt_time(end.split_whitespace().next()?)?,
    ))
}

fn parse_srt_time(text: &str) -> Option<i64> {
    let (clock, millis) = text.split_once(',')?;
    let parts: Vec<i64> = clock
        .split(':')
        .map(|part| part.parse::<i64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let (hours, minutes, seconds) = match parts.as_slice() {
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        [minutes, seconds] => (0, *minutes, *seconds),
        _ => return None,
    };
    Some(((hours * 3600 + minutes * 60 + seconds) * 1000) + millis.parse::<i64>().ok()?)
}

fn format_srt_time(ms: i64) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

fn video_filter(
    channel: &Channel,
    watermark: Option<&Path>,
    subtitle: Option<&Path>,
    tonemap: bool,
    hardware: bool,
) -> String {
    let chain = base_chain(channel, subtitle, tonemap);
    let hardware_suffix = if hardware {
        ",format=nv12,hwupload"
    } else {
        ""
    };
    if let Some(path) = watermark {
        let width = even(channel.stream.width);
        let graphics = &channel.graphics;
        let scale_width = even(width * graphics.watermark_scale.clamp(3, 50) / 100).max(16);
        let opacity = (graphics.watermark_opacity.clamp(0, 100) as f64) / 100.0;
        let (x, y) = match graphics.watermark_position.as_str() {
            "top-left" => ("24", "24".to_string()),
            "top-right" => ("W-w-24", "24".to_string()),
            "bottom-left" => ("24", "H-h-24".to_string()),
            _ => ("W-w-24", "H-h-24".to_string()),
        };
        return format!(
            "[0:v]{chain}[base];movie='{}',format=rgba,colorchannelmixer=aa={opacity:.2},scale={scale_width}:-1[wm];[base][wm]overlay={x}:{y}{hardware_suffix}",
            escape_filter_path(&path.to_string_lossy())
        );
    }

    format!("{chain}{hardware_suffix}")
}

fn animated_watermark_filter(
    channel: &Channel,
    subtitle: Option<&Path>,
    tonemap: bool,
    hardware: bool,
) -> String {
    let chain = base_chain(channel, subtitle, tonemap);
    let width = even(channel.stream.width);
    let graphics = &channel.graphics;
    let scale_width = even(width * graphics.watermark_scale.clamp(3, 50) / 100).max(16);
    let opacity = (graphics.watermark_opacity.clamp(0, 100) as f64) / 100.0;
    let (x, y) = match graphics.watermark_position.as_str() {
        "top-left" => ("24", "24".to_string()),
        "top-right" => ("W-w-24", "24".to_string()),
        "bottom-left" => ("24", "H-h-24".to_string()),
        _ => ("W-w-24", "H-h-24".to_string()),
    };
    let hardware_suffix = if hardware {
        ",format=nv12,hwupload"
    } else {
        ""
    };
    format!(
        "[0:v]{chain}[base];[1:v]format=rgba,colorchannelmixer=aa={opacity:.2},scale={scale_width}:-1[wm];[base][wm]overlay={x}:{y}{hardware_suffix}[vout]"
    )
}

fn base_chain(channel: &Channel, subtitle: Option<&Path>, tonemap: bool) -> String {
    let width = even(channel.stream.width);
    let height = even(channel.stream.height);
    // Scale before tonemapping. Tonemapping a 4K HDR source at full resolution
    // is far too expensive for real-time encoding; doing it at the output size
    // keeps the producer at 1x.
    let mut chain = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={FPS}"
    );
    if tonemap {
        chain.push_str(
            ",zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p",
        );
    }
    if let Some(subtitle) = subtitle {
        chain.push_str(&format!(
            ",subtitles='{}':force_style='FontSize=24,Outline=2'",
            escape_filter_path(&subtitle.to_string_lossy())
        ));
    }
    let graphics = &channel.graphics;

    if !graphics.lower_third_text.trim().is_empty() {
        let opacity = (graphics.lower_third_opacity.clamp(0, 100) as f64) / 100.0;
        let size = graphics.lower_third_size.clamp(8, 96);
        chain.push_str(&format!(
            ",drawtext=text='{}':fontcolor=white@{opacity:.2}:fontsize={size}:x=24:y=h-th-24:box=1:boxcolor=black@0.45:boxborderw=10",
            escape_drawtext(graphics.lower_third_text.trim())
        ));
    }

    let text = graphics.text.trim();
    if !text.is_empty() {
        let (x, y) = match graphics.position.as_str() {
            "top-left" => ("24", "24".to_string()),
            "top-right" => ("w-tw-24", "24".to_string()),
            "bottom-left" => ("24", "h-th-24".to_string()),
            _ => ("w-tw-24", "h-th-24".to_string()),
        };
        let opacity = (graphics.opacity.clamp(0, 100) as f64) / 100.0;
        let size = graphics.size.clamp(8, 96);
        chain.push_str(&format!(
            ",drawtext=text='{}':fontcolor=white@{opacity:.2}:fontsize={size}:x={x}:y={y}:box=1:boxcolor=black@0.35:boxborderw=8",
            escape_drawtext(text)
        ));
    }
    chain
}

fn is_animated_watermark(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("gif") | Some("mp4") | Some("webm") | Some("mov") | Some("mkv") | Some("avi")
    )
}

fn escape_filter_path(path: &str) -> String {
    path.replace('\\', "\\\\").replace('\'', "\\'")
}

fn even(value: i64) -> i64 {
    value.max(2) & !1
}

fn escape_drawtext(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace(':', "\\:")
        .replace('%', "\\%")
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use crate::db::{GraphicsSettings, StreamSettings};

    fn channel(graphics: GraphicsSettings) -> Channel {
        Channel {
            id: "c".into(),
            number: "1".into(),
            name: "Test".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics,
            stream: StreamSettings::default(),
        }
    }

    #[test]
    fn base_filter_has_no_drawtext() {
        let filter = video_filter(
            &channel(GraphicsSettings::default()),
            None,
            None,
            false,
            false,
        );
        assert!(filter.contains("scale=1280:720"));
        assert!(!filter.contains("drawtext"));
    }

    #[test]
    fn graphics_text_adds_a_positioned_overlay() {
        let filter = video_filter(
            &channel(GraphicsSettings {
                text: "ChannelLoom".into(),
                position: "top-left".into(),
                opacity: 50,
                size: 32,
                ..GraphicsSettings::default()
            }),
            None,
            None,
            false,
            false,
        );
        assert!(filter.contains("drawtext"));
        assert!(filter.contains("fontsize=32"));
        assert!(filter.contains("x=24:y=24"));
        assert!(filter.contains("white@0.50"));
    }

    #[test]
    fn lower_third_and_watermark_are_composed() {
        let graphics = GraphicsSettings {
            lower_third_text: "Now playing".into(),
            watermark_media_id: Some("logo".into()),
            ..GraphicsSettings::default()
        };
        let filter = video_filter(
            &channel(graphics),
            Some(Path::new("/media/logo.png")),
            None,
            false,
            false,
        );
        assert!(filter.contains("Now playing"));
        assert!(filter.contains("movie='/media/logo.png'"));
        assert!(filter.contains("overlay=W-w-24:24"));
    }

    #[test]
    fn subtitles_shift_with_the_trim_window() {
        let shifted = shift_srt(
            "1\n00:00:09,262 --> 00:00:10,850\nHello\n\n2\n00:02:39,262 --> 00:02:40,000\nLater\n",
            150_000,
        );
        assert!(!shifted.contains("Hello"));
        assert!(shifted.contains("00:00:09,262 --> 00:00:10,000"));
        assert!(shifted.contains("Later"));
    }

    #[test]
    fn hardware_encoding_appends_upload() {
        let filter = video_filter(
            &channel(GraphicsSettings::default()),
            None,
            None,
            false,
            true,
        );
        assert!(filter.ends_with(",format=nv12,hwupload"));
    }

    #[test]
    fn drawtext_special_characters_are_escaped() {
        assert_eq!(escape_drawtext("a:b'c%d"), "a\\:b\\'c\\%d");
    }
}

#[cfg(test)]
mod language_tests {
    use super::*;

    #[test]
    fn matches_iso_aliases() {
        for wanted in ["eng", "en", "English", "en-US"] {
            for language in ["eng", "en", "English", "en-GB"] {
                assert!(
                    language_matches_code(language, wanted),
                    "{language} != {wanted}"
                );
            }
        }
        assert!(language_matches_code("ger", "de"));
        assert!(language_matches_code("deu", "german"));
        assert!(language_matches_code("jpn", "ja"));
        assert!(!language_matches_code("ger", "eng"));
        assert!(!language_matches_code("und", "eng"));
    }

    fn audio(index: i64, language: &str, default: bool, commentary: bool) -> serde_json::Value {
        serde_json::json!({
            "index": index,
            "codec_type": "audio",
            "tags": {"language": language},
            "disposition": {"default": if default {1} else {0}, "commentary": if commentary {1} else {0}}
        })
    }

    #[test]
    fn selects_the_requested_audio_language_even_when_it_is_not_first() {
        let streams = vec![audio(1, "ger", true, false), audio(2, "eng", false, false)];
        assert_eq!(choose_audio_index(&streams, Some("eng")), Some(2));
    }

    #[test]
    fn prefers_default_and_skips_commentary_tracks() {
        let streams = vec![
            audio(1, "eng", false, false),
            audio(2, "eng", true, false),
            audio(3, "eng", true, true),
        ];
        assert_eq!(choose_audio_index(&streams, Some("eng")), Some(2));
    }

    #[test]
    fn falls_back_when_the_language_is_absent() {
        let streams = vec![audio(1, "fra", true, false), audio(2, "spa", false, false)];
        assert_eq!(choose_audio_index(&streams, Some("eng")), Some(1));
    }
}

#[allow(dead_code)]
mod private_publisher;

#[cfg(unix)]
#[allow(dead_code)]
mod fifo_spool;
