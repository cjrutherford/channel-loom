pub(crate) mod chunks;
mod resume;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sqlx::{SqlitePool, Row};

use crate::config::Config;
use crate::db;

// The next-start horizon is measured from the planning time, inclusive.
// Overlapping rows follow playback's earliest-start ordering. A gap has no current target.
fn mandatory_targets(rows: &[db::UpcomingMedia], now: i64) -> Vec<Vec<String>> {
    let mut channels: Vec<(&str, Vec<&db::UpcomingMedia>)> = Vec::new();
    for row in rows {
        if let Some((_, items)) = channels.iter_mut().find(|(id, _)| *id == row.channel_id) {
            items.push(row);
        } else {
            channels.push((&row.channel_id, vec![row]));
        }
    }
    let mut tiers = vec![Vec::new(), Vec::new(), Vec::new()];
    for (_, mut items) in channels {
        items.sort_by_key(|row| row.start_ms);
        let Some(i) = items.iter().position(|row| row.start_ms <= now && row.end_ms > now) else { continue };
        let current = items[i];
        tiers[0].push(current.media_id.clone());
        let Some(next) = items.get(i + 1) else { continue };
        if next.start_ms > now.saturating_add(3_600_000) { continue }
        tiers[1].push(next.media_id.clone());
        if current.end_ms - current.start_ms < 900_000 && next.end_ms - next.start_ms < 900_000 {
            if let Some(third) = items.get(i + 2) { tiers[2].push(third.media_id.clone()); }
        }
    }
    tiers
}

fn unfinished_tier(tiers: &[Vec<String>], statuses: &HashMap<String, (String, i64)>) -> Option<usize> {
    tiers.iter().position(|tier| tier.iter().any(|id| !statuses.get(id).is_some_and(|(status, _)| status == "ready")))
}

fn eligible_tier(tiers: &[Vec<String>], queued: &HashSet<String>, attempted: &HashSet<String>) -> Option<Vec<String>> {
    tiers.iter().map(|tier| tier.iter().filter(|id| queued.contains(*id) && !attempted.contains(*id)).cloned().collect::<Vec<_>>()).find(|ids| !ids.is_empty())
}

// A timeout cannot cancel a blocking filesystem syscall. Hold the global permit
// in that syscall's closure until it really returns, including after cancellation.
async fn bounded_blocking<T: Send + 'static>(permits: Arc<tokio::sync::Semaphore>, timeout: Duration, operation: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let permit = permits.try_acquire_owned().ok()?;
    let task = tokio::task::spawn_blocking(move || { let _permit = permit; operation() });
    tokio::time::timeout(timeout, task).await.ok()?.ok()
}

async fn source_metadata(path: &str) -> Option<std::fs::Metadata> {
    static PERMITS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let path = path.to_owned();
    bounded_blocking(PERMITS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4))).clone(), Duration::from_secs(8), move || std::fs::metadata(path)).await?.ok()
}

const FREE_SPACE_FLOOR: u64 = 10_000_000_000;

fn space_allows(available: u64, floor: u64, required: u64) -> bool {
    available.checked_sub(floor).is_some_and(|remaining| remaining >= required)
}

#[cfg(unix)]
fn available_space(path: &std::path::Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs writes the complete structure on success.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(not(unix))]
fn available_space(_path: &std::path::Path) -> Result<u64> {
    bail!("precache free-space check is unavailable on this platform")
}

/// Identity of the source from which a cached file was produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    pub path: String,
    pub size: i64,
    pub mtime: i64,
}

impl SourceIdentity {
    pub fn new(path: impl Into<String>, size: i64, mtime: i64) -> Self {
        Self { path: path.into(), size, mtime }
    }
}

struct ReadyFile {
    identity: SourceIdentity,
    path: PathBuf,
    bytes: i64,
    leases: usize,
    unused: u64,
    refresh_pending: bool,
}

#[derive(Default)]
struct IndexState {
    ready: HashMap<String, ReadyFile>,
    copying: HashMap<String, (i64, PathBuf, SourceIdentity)>,
    clock: u64,
    extra: HashMap<ExtraKey, ExtraCharge>,
    evicting: HashMap<String, ReadyFile>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum ExtraKey { Chunk { path: String, size: i64, mtime: i64, start: i64, end: i64 }, OrphanPath(PathBuf) }
struct ExtraCharge { reserved: i128, paths: Vec<PathBuf>, active: bool }
impl ExtraCharge {
    fn remaining(&self) -> i128 {
        if !self.active { return 0; }
        let written: i128 = self.paths.iter().map(|p| std::fs::symlink_metadata(p).ok().filter(|m| m.file_type().is_file()).map(|m| m.len() as i128).unwrap_or(0)).sum();
        (self.reserved - written).max(0)
    }
}
impl IndexState {
    fn occupied_bytes(&self) -> i128 {
        self.ready.values().map(|e| e.bytes as i128).sum::<i128>() + self.copying.values().map(|(n, _, _)| *n as i128).sum::<i128>() + self.evicting.values().map(|e| e.bytes as i128).sum::<i128>() + self.extra.values().map(|e| e.reserved).sum::<i128>()
    }
}

struct CacheIndexInner {
    state: Mutex<IndexState>,
    budget: i64,
    eviction: bool,
}

#[derive(Clone)]
struct CacheIndex(Arc<CacheIndexInner>);

/// Keeps a complete cached file pinned until the last clone is dropped.
pub struct CacheLease {
    index: CacheIndex,
    id: String,
    path: PathBuf,
}

impl CacheLease {
    pub fn path(&self) -> &std::path::Path { &self.path }
}

impl Clone for CacheLease {
    fn clone(&self) -> Self {
        let mut state = self.index.0.state.lock().unwrap();
        state.ready.get_mut(&self.id).expect("leased entry remains indexed").leases += 1;
        Self { index: self.index.clone(), id: self.id.clone(), path: self.path.clone() }
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut state = self.index.0.state.lock().unwrap();
        state.clock = state.clock.saturating_add(1);
        let clock = state.clock;
        let entry = state.ready.get_mut(&self.id).expect("leased entry remains indexed");
        entry.leases -= 1;
        if entry.leases == 0 { entry.unused = clock; }
    }
}

#[cfg(test)]
pub(crate) struct LeaseTestCache(CacheIndex, PathBuf);
#[cfg(test)]
impl LeaseTestCache {
    pub(crate) fn new(root: PathBuf, names: &[&str]) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        let index = CacheIndex::new((names.len() * 4) as i64, true);
        for name in names {
            let path = root.join(name);
            std::fs::write(&path, b"data").unwrap();
            index.0.state.lock().unwrap().ready.insert((*name).into(), ReadyFile {
                identity: SourceIdentity::new(*name, 4, 1), path, bytes: 4, leases: 0, unused: 0, refresh_pending: false,
            });
        }
        Self(index, root)
    }
    pub(crate) fn lease(&self, name: &str) -> CacheLease {
        self.0.acquire_ready(name, &SourceIdentity::new(name, 4, 1)).unwrap()
    }
    pub(crate) fn pressure(&self) -> bool {
        self.0.reserve("pressure", SourceIdentity::new("pressure", 4, 1), self.1.join("pressure"), 4).is_some()
    }
    pub(crate) fn pressure_all(&self) -> bool {
        let bytes = self.0.0.budget;
        self.0.reserve("pressure", SourceIdentity::new("pressure", bytes, 1), self.1.join("pressure"), bytes).is_some()
    }
}

// Chunk expiry releases only charges for files successfully removed.
struct SharedChunkBudget { index: CacheIndex, admission: Arc<tokio::sync::Mutex<()>>, directory: PathBuf, floor: u64 }
struct SharedChunkCharge { index: CacheIndex, key: ExtraKey }
impl SharedChunkCharge {
    fn measure(&mut self) {
        let mut state = self.index.0.state.lock().unwrap();
        let Some(charge) = state.extra.get_mut(&self.key) else { return };
        let mut total = 0i128;
        for path in &charge.paths {
            match std::fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_file() => total += m.len() as i128,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                _ => { charge.active = false; return; }
            }
        }
        charge.reserved = total;
        charge.active = false;
        if total == 0 { state.extra.remove(&self.key); }
    }
}
impl chunks::ChunkCharge for SharedChunkCharge {
    fn commit(&mut self, _: &[PathBuf]) { self.measure(); }
}
impl Drop for SharedChunkCharge { fn drop(&mut self) { self.measure(); } }
impl chunks::ChunkBudget for SharedChunkBudget {
    type Guard = SharedChunkCharge;
    fn release(&self, manifest: &std::path::Path, payload: &std::path::Path) {
        let mut state = self.index.0.state.lock().unwrap();
        state.extra.retain(|key, charge| {
            if charge.active { return true; }
            match key {
                ExtraKey::OrphanPath(path) => path != manifest && path != payload,
                ExtraKey::Chunk { .. } => {
                    !(charge.paths.contains(&manifest.to_path_buf()) && charge.paths.contains(&payload.to_path_buf())
                        && charge.paths.iter().all(|p| p == manifest || p == payload || matches!(std::fs::symlink_metadata(p), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)))
                }
            }
        });
    }
    fn reserve(&self, request: &chunks::Request, paths: &[PathBuf], cap: u64) -> Option<Self::Guard> {
        let _admission = self.admission.blocking_lock();
        if paths.is_empty() || paths.iter().any(|p| p.parent() != Some(self.directory.as_path()) || p.file_name().is_none() || !matches!(std::fs::symlink_metadata(p), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)) { return None; }
        let free = available_space(&self.directory).ok()? as i128;
        let key = ExtraKey::Chunk { path: request.source.path.clone(), size: request.source.size, mtime: request.source.mtime, start: request.start_ms, end: request.end_ms };
        let mut state = self.index.0.state.lock().ok()?;
        let cap = cap as i128;
        let remaining: i128 = state.copying.values().map(|(n, path, _)| *n as i128 - partial_progress(path, *n)).sum::<i128>() + state.extra.values().map(ExtraCharge::remaining).sum::<i128>();
        if state.extra.contains_key(&key) || state.occupied_bytes() + cap > self.index.0.budget as i128 || free < self.floor.max(FREE_SPACE_FLOOR) as i128 + remaining + cap { return None; }
        state.extra.insert(key.clone(), ExtraCharge { reserved: cap, paths: paths.to_vec(), active: true });
        Some(SharedChunkCharge { index: self.index.clone(), key })
    }
}

struct CacheReservation {
    index: CacheIndex,
    id: String,
    identity: SourceIdentity,
    path: PathBuf,
    partial: PathBuf,
    active: bool,
    delete_partial: bool,
}

fn rolling_video_eligible(kind: &str, video: Option<&str>, audio: Option<&str>) -> bool {
    !kind.eq_ignore_ascii_case("music") && !kind.eq_ignore_ascii_case("audio")
        && !(video.filter(|v| !v.is_empty()).is_none() && audio.filter(|a| !a.is_empty()).is_some())
}

fn partial_progress(path: &std::path::Path, reserved: i64) -> i128 {
    let mut partial = path.as_os_str().to_os_string();
    partial.push(".part");
    std::fs::metadata(PathBuf::from(partial)).map(|m| m.len().min(reserved.max(0) as u64) as i128).unwrap_or(0)
}

impl CacheIndex {
    fn new(budget: i64, eviction: bool) -> Self {
        Self(Arc::new(CacheIndexInner { state: Mutex::new(IndexState::default()), budget, eviction }))
    }

    fn acquire_ready(&self, id: &str, identity: &SourceIdentity) -> Option<CacheLease> {
        let mut state = self.0.state.lock().unwrap();
        let entry = state.ready.get_mut(id)?;
        if entry.refresh_pending || &entry.identity != identity || std::fs::metadata(&entry.path).map(|m| !m.is_file() || m.len() != entry.bytes as u64).unwrap_or(true) { return None; }
        entry.leases += 1;
        Some(CacheLease { index: self.clone(), id: id.to_owned(), path: entry.path.clone() })
    }

    fn reserve(&self, id: &str, identity: SourceIdentity, path: PathBuf, bytes: i64) -> Option<CacheReservation> {
        let mut state = self.0.state.lock().unwrap();
        if bytes <= 0 || bytes > self.0.budget || state.ready.contains_key(id) || state.copying.contains_key(id) || state.evicting.contains_key(id)
            || state.ready.values().any(|entry| entry.path == path)
            || state.copying.values().any(|(_, existing, _)| *existing == path)
            || state.evicting.values().any(|entry| entry.path == path) { return None; }
        loop {
            let used: i128 = state.occupied_bytes();
            if used + bytes as i128 <= self.0.budget as i128 { break; }
            if !self.0.eviction { return None; }
            let victim = state.ready.iter().filter(|(_, e)| e.leases == 0)
                .min_by_key(|(_, e)| e.unused).map(|(id, _)| id.clone())?;
            let entry = state.ready.get(&victim).unwrap();
            if let Err(error) = std::fs::remove_file(&entry.path) {
                if error.kind() != std::io::ErrorKind::NotFound { return None; }
            }
            state.ready.remove(&victim);
        }
        let mut partial = path.as_os_str().to_os_string();
        partial.push(".part");
        state.copying.insert(id.to_owned(), (bytes, path.clone(), identity.clone()));
        Some(CacheReservation { index: self.clone(), id: id.to_owned(), identity, path, partial: partial.into(), active: true, delete_partial: true })
    }
}

impl CacheReservation {
    fn retain_for_resume(mut self) {
        self.active = false;
    }

    fn release_without_deleting_partial(mut self) {
        self.index.0.state.lock().unwrap().copying.remove(&self.id);
        self.active = false;
    }

    #[cfg(test)]
    fn partial_path(&self) -> &std::path::Path { &self.partial }
    fn publish(mut self, bytes: i64) -> Result<()> {
        let mut state = self.index.0.state.lock().unwrap();
        let reserved = state.copying.get(&self.id).map(|(bytes, _, _)| *bytes).context("reservation missing")?;
        if bytes <= 0 || bytes > reserved { bail!("cached file exceeds its reservation"); }
        if std::fs::metadata(&self.partial)?.len() != bytes as u64 { bail!("cached file size changed before publication"); }
        if self.path.exists() { bail!("cache destination already exists"); }
        std::fs::rename(&self.partial, &self.path)?;
        state.copying.remove(&self.id);
        state.clock = state.clock.saturating_add(1);
        let unused = state.clock;
        state.ready.insert(self.id.clone(), ReadyFile { identity: self.identity.clone(), path: self.path.clone(), bytes, leases: 0, unused, refresh_pending: false });
        self.active = false;
        Ok(())
    }
}

impl Drop for CacheReservation {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.index.0.state.lock().unwrap();
            if self.delete_partial { let _ = std::fs::remove_file(&self.partial); }
            state.copying.remove(&self.id);
        }
    }
}

/// Copies (or transcode-proxies) media that is about to air onto local disk so
/// the producer does not have to read it from a slow network mount in real
/// time. Entries are scheduled from the channel guide and bounded by a byte
/// budget with LRU eviction.
const COPY_QUANTUM_BYTES: u64 = 16 * 1024 * 1024;
const COPY_QUANTUM_TIME: Duration = Duration::from_secs(20);

#[derive(Debug, PartialEq, Eq)]
enum CopyOutcome { Complete, Yielded }

#[derive(Debug, PartialEq, Eq)]
enum ProcessOutcome { Complete(i64), Yielded }

pub struct Precache {
    pool: SqlitePool,
    dir: PathBuf,
    log_dir: PathBuf,
    max_bytes: i64,
    free_space_floor: u64,
    ahead_ms: i64,
    proxy_kbps: i64,
    proxy_height: i64,
    copy_bytes_per_sec: i64,
    enabled: bool,
    rolling_chunks: bool,
    running: AtomicBool,
    hydrating: AtomicBool,
    paused: AtomicBool,
    shutdown: Arc<AtomicBool>,
    scan_cursor: AtomicUsize,
    index: CacheIndex,
    index_initialized: tokio::sync::OnceCell<()>,
    shared_chunk_index: tokio::sync::OnceCell<Arc<chunks::ChunkIndex>>,
    verified_sources: std::sync::Mutex<HashMap<(String, String), SourceIdentity>>,
    eviction_lock: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    ignore_host_readers: bool,
    #[cfg(test)]
    copy_quantum_bytes: u64,
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
            free_space_floor: FREE_SPACE_FLOOR,
            ahead_ms: config.precache_ahead_hours.max(1) * 3_600_000,
            proxy_kbps: config.precache_proxy_kbps.clamp(500, 20_000),
            proxy_height: config.precache_proxy_height.clamp(240, 2160),
            copy_bytes_per_sec: config.precache_copy_mbps.max(1) * 1_000_000,
            enabled: config.precache_enabled,
            rolling_chunks: config.precache_rolling_chunks,
            running: AtomicBool::new(false),
            hydrating: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scan_cursor: AtomicUsize::new(0),
            index: CacheIndex::new(config.precache_max_gb.max(0) * 1_000_000_000, false),
            index_initialized: tokio::sync::OnceCell::new(),
            shared_chunk_index: tokio::sync::OnceCell::new(),
            verified_sources: std::sync::Mutex::new(HashMap::new()),
            eviction_lock: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            ignore_host_readers: false,
            #[cfg(test)]
            copy_quantum_bytes: COPY_QUANTUM_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn isolated_from_host_readers(mut self) -> Self {
        self.ignore_host_readers = true;
        self
    }

    pub(crate) async fn shared_chunk_index(&self) -> Result<Option<Arc<chunks::ChunkIndex>>> {
        if !self.rolling_chunks { return Ok(None); }
        let index = self.shared_chunk_index.get_or_try_init(|| async {
            let cache_dir = self.dir.clone();
            tokio::task::spawn_blocking(move || -> Result<_> {
                let cache_dir = std::fs::canonicalize(cache_dir)?;
                let directory = cache_dir.join("chunks");
                std::fs::create_dir_all(&directory)?;
                anyhow::ensure!(std::fs::symlink_metadata(&directory)?.file_type().is_dir(), "chunk directory must be a local directory");
                let directory = std::fs::canonicalize(directory)?;
                anyhow::ensure!(directory.parent() == Some(cache_dir.as_path()), "chunk directory outside cache");
                Ok(Arc::new(chunks::ChunkIndex::open(&directory)?))
            }).await?
        }).await?;
        Ok(Some(index.clone()))
    }

    #[cfg(test)]
    pub(crate) async fn test_publish_rolling_snapshot(&self, now: i64) -> Result<Vec<chunks::Airing>> {
        let snapshot = self.rolling_snapshot(now).await?;
        self.publish_verified_sources(&snapshot);
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(crate) async fn test_rolling_tick(&self, snapshot: Vec<chunks::Airing>, now: i64) -> Result<chunks::TickOutcome> {
        self.initialize_index().await?;
        let index = self.shared_chunk_index().await?.expect("rolling enabled");
        let directory = std::fs::canonicalize(self.dir.join("chunks"))?;
        let budget = SharedChunkBudget { index: self.index.clone(), admission: self.eviction_lock.clone(), directory: directory.clone(), floor: 0 };
        self.publish_verified_sources(&snapshot);
        let control = chunks::WorkerControl::new(budget, directory, 10_000_000, Duration::from_secs(15), self.shutdown.clone());
        Ok(tokio::task::spawn_blocking(move || chunks::rolling_worker_tick(&snapshot, now, &index, &control)).await?)
    }

    async fn initialize_index(&self) -> Result<()> {
        self.index_initialized.get_or_try_init(|| async {
            let mut entries = db::list_precache(&self.pool).await?;
            entries.sort_by_key(|entry| entry.last_used_ms);
            // Finish an eviction interrupted after its database transition.
            let cache_dir = std::fs::canonicalize(&self.dir)?;
            for entry in entries.iter().filter(|e| e.status == "failed" && e.error.as_deref() == Some("evicted")) {
                if let Some(path) = entry.cache_path.as_ref() {
                    let path = std::path::Path::new(path);
                    if path.parent().and_then(|p| std::fs::canonicalize(p).ok()).as_ref() != Some(&cache_dir) {
                        bail!("evicted cache path is outside cache directory");
                    }
                    match std::fs::remove_file(path) {
                        Ok(()) => {},
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            for entry in entries.iter().filter(|entry| entry.status == "ready") {
                let valid = entry.bytes > 0 && entry.cache_path.as_ref().is_some_and(|path|
                    std::fs::metadata(path).map(|m| m.is_file() && m.len() == entry.bytes as u64).unwrap_or(false));
                if !valid {
                    db::set_precache_status(&self.pool, &entry.media_id, "failed", 0, Some("missing or incomplete cached file")).await?;
                }
            }
            let chunk_dir = self.dir.join("chunks");
            std::fs::create_dir_all(&chunk_dir)?;
            anyhow::ensure!(std::fs::symlink_metadata(&chunk_dir)?.file_type().is_dir(), "chunk directory must be a local directory");
            let chunk_dir = std::fs::canonicalize(&chunk_dir)?;
            anyhow::ensure!(chunk_dir.parent() == Some(cache_dir.as_path()), "chunk directory outside cache");
            let mut orphans = Vec::new();
            for item in std::fs::read_dir(&chunk_dir)? {
                let item = item?;
                let meta = std::fs::symlink_metadata(item.path())?;
                if meta.file_type().is_file() { orphans.push((item.path(), meta.len() as i128)); }
            }
            let mut state = self.index.0.state.lock().unwrap();
            for (path, bytes) in orphans { state.extra.insert(ExtraKey::OrphanPath(path.clone()), ExtraCharge { reserved: bytes, paths: vec![path], active: false }); }
            for entry in entries.into_iter().filter(|entry| entry.status == "ready" && entry.bytes > 0) {
                if let Some(path) = entry.cache_path {
                    if !std::fs::metadata(&path).map(|m| m.is_file() && m.len() == entry.bytes as u64).unwrap_or(false) { continue; }
                    state.clock = state.clock.saturating_add(1);
                    let unused = state.clock;
                    state.ready.insert(entry.media_id, ReadyFile {
                        identity: SourceIdentity::new(entry.source_path, entry.source_size, entry.source_mtime),
                        path: path.into(), bytes: entry.bytes, leases: 0, unused, refresh_pending: matches!(entry.error.as_deref(), Some("refresh pending") | Some("refresh pending: source path changed")),
                    });
                }
            }
            Ok::<(), anyhow::Error>(())
        }).await?;
        Ok(())
    }

    /// Atomically acquire a complete file matching the source identity.
    pub async fn acquire_ready(&self, id: &str, source: &SourceIdentity) -> Result<Option<CacheLease>> {
        self.initialize_index().await?;
        let _guard = self.eviction_lock.lock().await;
        Ok(self.index.acquire_ready(id, source))
    }

    // The detached transition completes even if the scanning caller is cancelled. The
    // admission lock prevents leases between the committed DB update and index update.
    async fn revalidate_ready(&self, id: &str, catalog_path: &str) -> Result<bool> {
        self.initialize_index().await?;
        let pool = self.pool.clone();
        let index = self.index.clone();
        let admission = self.eviction_lock.clone();
        let id = id.to_owned();
        let catalog_path = catalog_path.to_owned();
        tokio::spawn(async move {
            let metadata = source_metadata(&catalog_path).await;
            let current = match metadata {
                Some(meta) if meta.is_file() => Some(SourceIdentity::new(&catalog_path, meta.len() as i64, metadata_mtime(&meta))),
                _ => None,
            };
            let _guard = admission.lock().await;
            let Some(row) = db::precache_entry(&pool, &id).await? else { return Ok(false) };
            if row.status != "ready" || !index.0.state.lock().unwrap().ready.contains_key(&id) { return Ok(false); }
            let old = SourceIdentity::new(&row.source_path, row.source_size, row.source_mtime);
            let path_changed = catalog_path != row.source_path;
            let legacy = !path_changed && row.source_mtime == 0 && current.as_ref().is_some_and(|v| v.size == row.source_size);
            let changed = path_changed || (!legacy && current.as_ref().is_some_and(|v| v != &old));
            let status = if path_changed { Some("refresh pending: source path changed") }
                else if changed { Some("refresh pending: stale source") }
                else if current.is_none() { Some("refresh deferred: source unavailable") } else { None };
            let mtime = if legacy { current.as_ref().unwrap().mtime } else { row.source_mtime };
            let result = sqlx::query("UPDATE precache_entries SET source_mtime = ?, error = ?, updated_ms = ? WHERE media_id = ? AND status = 'ready' AND source_path = ? AND source_size = ? AND source_mtime = ?")
                .bind(mtime).bind(status).bind(db::now_ms()).bind(&id).bind(&row.source_path).bind(row.source_size).bind(row.source_mtime)
                .execute(&pool).await?;
            if result.rows_affected() != 1 { anyhow::bail!("ready cache row changed during validation"); }
            let mut state = index.0.state.lock().unwrap();
            if let Some(entry) = state.ready.get_mut(&id) {
                entry.identity.mtime = mtime;
                entry.refresh_pending = path_changed;
            }
            // Changed replacements are deliberately deferred until safe staging exists.
            Ok(false)
        }).await?
    }

    pub fn start(self: &Arc<Self>) {
        if !self.enabled {
            tracing::info!("precache disabled");
            return;
        }
        tracing::info!(enabled = self.enabled, "precache service starting");
        if self.rolling_chunks {
            let service = self.clone();
            tokio::spawn(async move { service.rolling_loop().await; });
        }
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.initialize_index().await.and_then(|_| Ok(())) {
                tracing::warn!(%error, "precache: index initialization failed");
                return;
            }
            if let Err(error) = service.cleanup_partials().await {
                tracing::warn!(%error, "precache: partial recovery failed");
                return;
            }
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

    async fn rolling_loop(self: Arc<Self>) {
        // Fail closed until chunk allocations and whole-copy allocations share accounting.
        if let Err(error) = self.initialize_index().await { tracing::warn!(%error, "chunk accounting unavailable"); return; }
        let directory = match std::fs::canonicalize(self.dir.join("chunks")) { Ok(path) => path, Err(_) => return };
        let index = match self.shared_chunk_index().await { Ok(Some(index)) => index, Ok(None) => return, Err(error) => { tracing::warn!(%error, "chunk index unavailable"); return; } };
        let budget = SharedChunkBudget { index: self.index.clone(), admission: self.eviction_lock.clone(), directory: directory.clone(), floor: self.free_space_floor };
        let control = Arc::new(chunks::WorkerControl::new(budget, directory, 1_000_000_000, Duration::from_secs(120), self.shutdown.clone()));
        while !self.shutdown.load(Ordering::Relaxed) {
            let now = db::now_ms();
            match self.rolling_snapshot(now).await {
                Ok(snapshot) => {
                    self.publish_verified_sources(&snapshot);
                    let index = index.clone();
                    let control = control.clone();
                    if tokio::time::timeout(Duration::from_secs(125), tokio::task::spawn_blocking(move || chunks::rolling_worker_tick(&snapshot, now, &index, &control))).await.is_err() { tracing::warn!("chunk worker did not retire; stopping rolling loop"); return; }
                }
                Err(error) => tracing::warn!(%error, "rolling schedule unavailable"),
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }

    // Use catalog classification for both scheduling paths. Unknown codecs are
    // conservatively treated as video; a remux failure must not schedule a copy.
    async fn rolling_video_ids(&self) -> Result<HashSet<String>> {
        if !self.rolling_chunks { return Ok(HashSet::new()); }
        let rows = sqlx::query("SELECT ids.media_id AS id, m.kind, m.video_codec, m.audio_codec FROM (SELECT media_id FROM channel_items UNION SELECT media_id FROM precache_entries) ids LEFT JOIN media_items m ON m.id=ids.media_id")
            .fetch_all(&self.pool).await?;
        Ok(rows.into_iter().filter_map(|row| {
            let kind: Option<String> = row.get("kind");
            let video: Option<String> = row.get("video_codec");
            let audio: Option<String> = row.get("audio_codec");
            kind.as_deref().is_none_or(|kind| rolling_video_eligible(kind, video.as_deref(), audio.as_deref())).then(|| row.get("id"))
        }).collect())
    }

    fn publish_verified_sources(&self, snapshot: &[chunks::Airing]) {
        let mut sources = self.verified_sources.lock().unwrap();
        for a in snapshot {
            sources.insert((a.media_id.clone(), a.source.path.clone()), a.source.clone());
        }
    }

    pub(crate) fn verified_rolling_source(&self, media_id: &str, path: &str) -> Option<SourceIdentity> {
        self.verified_sources.lock().unwrap().get(&(media_id.to_owned(), path.to_owned())).cloned()
    }

    async fn rolling_snapshot(&self, now: i64) -> Result<Vec<chunks::Airing>> {
        let rows = sqlx::query("SELECT ci.channel_id, ci.position, ci.media_id, ci.in_ms, ci.start_ms, ci.end_ms, m.path, m.kind, m.video_codec, m.audio_codec FROM channel_items ci JOIN media_items m ON m.id = ci.media_id WHERE ci.end_ms > ? AND m.kind != 'image' ORDER BY ci.channel_id, ci.start_ms, ci.position")
            .bind(now).fetch_all(&self.pool).await?;
        let mut snapshot = Vec::new();
        let mut last_channel = String::new();
        let mut count = 0;
        for row in rows {
            let channel: String = row.get("channel_id");
            if channel != last_channel { last_channel = channel; count = 0; }
            count += 1;
            if count > 3 { continue; }
            let kind: String = row.get("kind");
            let video: Option<String> = row.get("video_codec");
            let audio: Option<String> = row.get("audio_codec");
            if !rolling_video_eligible(&kind, video.as_deref(), audio.as_deref()) {
                tracing::debug!(media = %row.get::<String, _>("media_id"), "rolling chunks skipped audio-only media; whole-copy fallback eligible");
                continue;
            }
            let path: String = row.get("path");
            let media_id: String = row.get("media_id");
            let source = match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => SourceIdentity::new(&path, meta.len() as i64, metadata_mtime(&meta)),
                Ok(_) => continue,
                Err(_) => match self.verified_rolling_source(&media_id, &path) {
                    Some(identity) => identity,
                    None => continue,
                },
            };
            let channel_id: String = row.get("channel_id");
            let position: i64 = row.get("position");
            snapshot.push(chunks::Airing { airing_id: format!("{channel_id}:{position}"), channel_id, media_id, source, in_ms: row.get("in_ms"), start_ms: row.get("start_ms"), end_ms: row.get("end_ms") });
        }
        Ok(snapshot)
    }

    pub async fn remove(&self, media_id: &str) -> Result<()> {
        self.initialize_index().await?;
        let _guard = self.eviction_lock.lock().await;
        let entry = db::precache_entry(&self.pool, media_id).await?;
        {
            let mut state = self.index.0.state.lock().unwrap();
            if state.copying.contains_key(media_id) || state.evicting.contains_key(media_id)
                || state.ready.get(media_id).is_some_and(|entry| entry.leases > 0) {
                bail!("cache entry is in use");
            }
            if let Some(entry) = state.ready.remove(media_id) { state.evicting.insert(media_id.to_owned(), entry); }
        }
        if let Some(entry) = entry {
            db::set_precache_status(&self.pool, media_id, "failed", 0, Some("removed")).await?;
            if let Some(path) = entry.cache_path {
                match std::fs::remove_file(&path) {
                    Ok(()) => {},
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error.into()),
                }
                let _ = std::fs::remove_file(format!("{path}.part"));
            }
            db::delete_precache(&self.pool, media_id).await?;
        }
        self.index.0.state.lock().unwrap().evicting.remove(media_id);
        Ok(())
    }

    async fn cleanup_partials(&self) -> Result<()> {
        // Only remove partials associated with indexed cache entries. The cache
        // directory may contain files belonging to another producer.
        let dir = tokio::fs::canonicalize(&self.dir).await?;
        for entry in db::list_precache(&self.pool).await? {
            // Ready-file accounting belongs to the completed file, not a partial.
            if entry.status == "ready" { continue; }
            let Some(ref path) = entry.cache_path else { continue };
            let path = std::path::Path::new(&path);
            let Some(parent) = path.parent() else { continue };
            if tokio::fs::canonicalize(parent).await.ok().as_ref() != Some(&dir) {
                continue;
            }
            let Some(name) = path.file_name() else { continue };
            let mut partial = name.to_os_string();
            partial.push(".part");
            let partial = dir.join(partial);
            let manifest = PathBuf::from(format!("{}.resume", partial.display()));
            let identity = resume::Identity { path: entry.source_path.clone(), size: entry.source_size.max(0) as u64, mtime: entry.source_mtime };
            let valid = matches!(entry.status.as_str(), "running" | "queued") && entry.mode == "copy" && entry.identity_known
                && resume::load(&manifest, &partial, &entry.media_id, &entry.generation, &identity).is_ok();
            if valid {
                if let Some(mut reservation) = self.index.reserve(&entry.media_id, SourceIdentity::new(&entry.source_path, entry.source_size, entry.source_mtime), path.to_path_buf(), entry.source_size) {
                    let updated = sqlx::query("UPDATE precache_entries SET status='queued', error=NULL WHERE media_id=? AND generation=? AND status=? AND source_path=? AND source_size=? AND source_mtime=? AND cache_path IS ?")
                        .bind(&entry.media_id).bind(&entry.generation).bind(&entry.status).bind(&entry.source_path).bind(entry.source_size).bind(entry.source_mtime).bind(&entry.cache_path).execute(&self.pool).await?;
                    if updated.rows_affected() == 1 { reservation.active = false; continue; }
                    reservation.release_without_deleting_partial();
                    continue;
                }
            }
            let unchanged = sqlx::query("UPDATE precache_entries SET bytes=0 WHERE media_id=? AND generation=? AND status=? AND source_path=? AND source_size=? AND source_mtime=? AND cache_path IS ?")
                .bind(&entry.media_id).bind(&entry.generation).bind(&entry.status).bind(&entry.source_path).bind(entry.source_size).bind(entry.source_mtime).bind(&entry.cache_path).execute(&self.pool).await?;
            if unchanged.rows_affected() != 1 { continue; }
            match tokio::fs::remove_file(&partial).await {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
            let _ = tokio::fs::remove_file(manifest).await;
        }
        Ok(())
    }

    // Serialize against claims: a queued reservation or any on-disk partial owns its row.
    async fn prune_queue(&self, now: i64, cap: i64, keep: &HashSet<String>) -> Result<(u64, u64)> {
        let _guard = self.eviction_lock.lock().await;
        let mut protected = keep.clone();
        protected.extend(self.index.0.state.lock().unwrap().copying.keys().cloned());
        for entry in db::list_precache(&self.pool).await? {
            if let Some(path) = entry.cache_path {
                let partial = format!("{path}.part");
                if !matches!(std::fs::symlink_metadata(partial), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound) {
                    protected.insert(entry.media_id);
                }
            }
        }
        let stale = db::prune_precache_queue(&self.pool, now, &protected).await?;
        let trimmed = db::trim_precache_queue(&self.pool, cap, &protected).await?;
        Ok((stale, trimmed))
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
        let rows = db::upcoming_media(&self.pool, now, now + self.ahead_ms).await?;
        tracing::debug!(rows = rows.len(), "precache schedule loaded");
        let video_ids = self.rolling_video_ids().await?;
        let mut tiers = mandatory_targets(&rows, now);
        for tier in &mut tiers { tier.retain(|id| !video_ids.contains(id)); }
        let keep: HashSet<String> = tiers.iter().flatten().cloned().collect();
        let starts: HashMap<String, i64> = rows.iter().filter(|r| r.end_ms > now && keep.contains(&r.media_id))
            .fold(HashMap::new(), |mut starts, r| { starts.entry(r.media_id.clone()).and_modify(|s| *s = (*s).min(r.start_ms)).or_insert(r.start_ms); starts });
        self.migrate_required_proxies(&starts).await?;
        // Paused partials retain their full-source reservation until a later reclaim policy.
        let mut prune_keep = keep.clone();
        prune_keep.extend(video_ids.iter().cloned());
        let (stale, trimmed) = self.prune_queue(now, 32, &prune_keep).await?;
        if stale > 0 { tracing::debug!(stale, "precache dropped expired queue entries"); }
        if trimmed > 0 { tracing::debug!(trimmed, "precache trimmed the far-future queue"); }

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

        // Materialize every missing required item before the bounded optional scan.
        // A zero mtime (and size, when unknown) records that source identity still
        // needs validation; planning must not block on a slow source mount.
        let statuses = db::precache_statuses(&self.pool).await?;
        let mut materialized = HashSet::new();
        for id in tiers.iter().flatten() {
            if !materialized.insert(id) || statuses.contains_key(id) { continue; }
            let Some(&position) = index.get(id) else { continue };
            let first = &groups[position][0];
            let size = first.size_bytes.max(0);
            let start = first.start_ms;
            let estimate_ms = (self.estimate_seconds(size, "copy") * 1000.0) as i64;
            let cache_path = self.cache_path_for(id, "copy", &first.path);
            // Insert only if still absent; never replace a concurrent ready generation.
            sqlx::query("INSERT OR IGNORE INTO precache_entries (media_id, source_path, source_size, source_mtime, identity_known, cache_path, mode, status, bytes, error, start_ms, created_ms, updated_ms, last_used_ms, deadline_ms, generation) VALUES (?, ?, ?, 0, 0, ?, 'copy', 'queued', 0, 'source identity pending', ?, ?, ?, 0, ?, lower(hex(randomblob(16))))")
                .bind(id).bind(&first.path).bind(size).bind(&cache_path).bind(start).bind(now).bind(now)
                .bind(start.saturating_sub(estimate_ms)).execute(&self.pool).await?;
        }

        // Retry required failures independently of the optional scan. Serialize with
        // eviction so an evicted payload is removed before it can be recreated.
        {
            self.initialize_index().await?;
            let _guard = self.eviction_lock.lock().await;
            for id in &materialized {
                sqlx::query("UPDATE precache_entries SET status = 'queued', identity_known = 0, error = 'source identity pending', generation = CASE WHEN generation = '' THEN lower(hex(randomblob(16))) ELSE generation END, updated_ms = ? WHERE media_id = ? AND status = 'failed' AND ? - updated_ms >= CASE WHEN error = 'evicted' THEN 30000 ELSE MIN(300000, 30000 * (1 << MIN(MAX(retry_count - 1, 0), 4))) END")
                    .bind(now).bind(id).bind(now).execute(&self.pool).await?;
            }
        }

        self.spawn_required_hydration(tiers.iter().flatten().cloned().collect());

        if !groups.is_empty() {
            let offset = self.scan_cursor.fetch_add(8, Ordering::Relaxed) % groups.len();
            groups.rotate_left(offset);
            groups.sort_by_key(|group| tiers.iter().position(|tier| tier.contains(&group[0].media_id)).unwrap_or(3));
        }
        let statuses = db::precache_statuses(&self.pool).await?;
        let active = statuses
            .values()
            .filter(|(status, _)| status == "queued" || status == "running")
            .count();
        // Keep the queue focused; the worker is a single reader on a slow mount.
        let mut started = if active >= 32 { 8 } else { 0 };
        let mut checked = 0;
        for group in groups {
            if checked >= 8 {
                break;
            }
            let Some(first) = group.first() else { continue };
            if video_ids.contains(&first.media_id) || keep.contains(&first.media_id) || first.path.starts_with("cloud://") {
                continue;
            }
            checked += 1;
            // Ready entries are revalidated only by this bounded background scan.
            if statuses.get(&first.media_id).is_some_and(|(status, _)| status == "ready") {
                if !self.revalidate_ready(&first.media_id, &first.path).await? { continue; }
            }
            if started >= 8 { continue; }
            // Avoid touching the slow source mount unless there is work to do.
            if let Some((status, updated_ms)) = statuses.get(&first.media_id) {
                if matches!(status.as_str(), "running" | "queued") {
                    continue;
                }
                if status == "failed" && now - updated_ms < 3_600_000
                    && db::precache_entry(&self.pool, &first.media_id).await?
                        .is_none_or(|entry| entry.error.as_deref() != Some("evicted")) {
                    continue;
                }
            }
            // Always sample the source identity; catalog size alone can be stale.
            let Some(meta) = source_metadata(&first.path).await else {
                tracing::warn!(path = %first.path, "precache source lookup failed or timed out");
                continue;
            };
            if !meta.is_file() { continue; }
            let size = meta.len() as i64;
            let mtime = metadata_mtime(&meta);
            if size <= 0 || size > self.max_bytes { continue; }
            if first.size_bytes != size {
                let _ = db::set_media_size(&self.pool, &first.media_id, size).await;
            }
            tracing::debug!(media = %first.media_id, previous_policy = self.mode_for(size, first.duration_ms),
                proxy_estimate = self.proxy_bytes(first.duration_ms), "precache selected original copy");
            let mode = "copy";
            let estimate_bytes = size;
            let start = first.start_ms;
            let estimate_ms = (self.estimate_seconds(size, mode) * 1000.0) as i64;
            if start < now + estimate_ms {
                tracing::warn!(media = %first.media_id, start, "precache behind schedule; direct source fallback may be needed");
            }
            if !self.budget_allows(estimate_bytes, &keep).await? {
                continue;
            }
            let cache_path = self.cache_path_for(&first.media_id, mode, &first.path);
            let deadline = start.saturating_sub(estimate_ms);
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

    async fn migrate_required_proxies(&self, required: &HashMap<String, i64>) -> Result<()> {
        for entry in db::list_precache(&self.pool).await?.into_iter()
            .filter(|e| e.status == "queued" && e.mode == "proxy" && required.contains_key(&e.media_id)) {
            let path = self.cache_path_for(&entry.media_id, "copy", &entry.source_path);
            let start = required[&entry.media_id];
            let deadline = start.saturating_sub((self.estimate_seconds(entry.source_size, "copy") * 1000.0) as i64);
            // Do not alter a claimed or replaced generation, or any ready file.
            sqlx::query("UPDATE precache_entries SET mode='copy', cache_path=?, bytes=0, start_ms=?, deadline_ms=?, generation=lower(hex(randomblob(16))), updated_ms=? WHERE media_id=? AND status='queued' AND mode='proxy' AND source_path=? AND cache_path IS ? AND updated_ms=?")
                .bind(path).bind(start).bind(deadline).bind(db::now_ms()).bind(&entry.media_id).bind(&entry.source_path)
                .bind(&entry.cache_path).bind(entry.updated_ms).execute(&self.pool).await?;
        }
        Ok(())
    }

    fn spawn_required_hydration(self: &Arc<Self>, ids: Vec<String>) {
        if self.hydrating.swap(true, Ordering::SeqCst) { return; }
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.hydrate_required_metadata(&ids).await {
                tracing::warn!(%error, "required metadata hydration stopped");
            }
            service.hydrating.store(false, Ordering::SeqCst);
        });
    }

    async fn hydrate_required_metadata(&self, ids: &[String]) -> Result<()> {
        let wanted: HashSet<_> = ids.iter().collect();
        let entries: Vec<_> = db::list_precache(&self.pool).await?.into_iter()
            .filter(|entry| wanted.contains(&entry.media_id) && entry.status == "queued" && !entry.identity_known).collect();
        for batch in entries.chunks(4) {
            if self.shutdown.load(Ordering::Relaxed) { break; }
            let mut jobs = tokio::task::JoinSet::new();
            for entry in batch.iter().cloned() {
                jobs.spawn(async move { let metadata = source_metadata(&entry.source_path).await; (entry, metadata) });
            }
            while let Some(result) = jobs.join_next().await {
                let (entry, metadata) = result?;
                if let Some(meta) = metadata.filter(|meta| meta.is_file() && meta.len() > 0 && meta.len() <= i64::MAX as u64 && meta.modified().is_ok()) {
                    db::hydrate_precache_identity(&self.pool, &entry, meta.len() as i64, metadata_mtime(&meta)).await?;
                }
            }
        }
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
        #[cfg(test)]
        if self.ignore_host_readers { return false; }
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
        let mut attempted = HashSet::new();
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            let live = self.direct_reader_active();
            let now = db::now_ms();
            let rows = db::upcoming_media(&self.pool, now, now.saturating_add(self.ahead_ms)).await?;
            let video_ids = self.rolling_video_ids().await?;
            let mut tiers = mandatory_targets(&rows, now);
            for tier in &mut tiers { tier.retain(|id| !video_ids.contains(id)); }
            let mut statuses = db::precache_statuses(&self.pool).await?;
            statuses.retain(|id, _| !video_ids.contains(id));
            let queued: HashSet<String> = statuses.iter().filter(|(_, (status, _))| status == "queued").map(|(id, _)| id.clone()).collect();
            let entry = if let Some(ids) = eligible_tier(&tiers, &queued, &attempted) {
                db::next_queued_precache_in(&self.pool, &ids, !live).await?
            } else if unfinished_tier(&tiers, &statuses).is_none() {
                db::next_queued_precache_in(&self.pool, &queued.difference(&attempted).cloned().collect::<Vec<_>>(), !live).await?
            } else {
                None
            };
            let Some(mut entry) = entry else {
                if live {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    continue;
                }
                return Ok(());
            };
            attempted.insert(entry.media_id.clone());
            if entry.generation.is_empty() {
                // Repair legacy queued rows only if the observed job is still unchanged.
                let repaired = sqlx::query("UPDATE precache_entries SET generation=lower(hex(randomblob(16))) WHERE media_id=? AND status='queued' AND generation='' AND source_path=? AND source_size=? AND source_mtime=? AND cache_path IS ? AND mode=? AND updated_ms=?")
                    .bind(&entry.media_id).bind(&entry.source_path).bind(entry.source_size).bind(entry.source_mtime).bind(&entry.cache_path).bind(&entry.mode).bind(entry.updated_ms).execute(&self.pool).await?;
                if repaired.rows_affected() != 1 { continue; }
                let Some(current) = db::precache_entry(&self.pool, &entry.media_id).await? else { continue };
                entry = current;
            }
            if !entry.identity_known {
                let metadata = source_metadata(&entry.source_path).await;
                let identity = match metadata {
                    Some(meta) if meta.is_file() && meta.len() > 0 && meta.len() <= i64::MAX as u64 && meta.modified().is_ok() => Some((meta.len() as i64, metadata_mtime(&meta))),
                    _ => None,
                };
                let Some((size, mtime)) = identity else {
                    db::fail_precache_identity(&self.pool, &entry, "source metadata unavailable, invalid, or timed out; retry after cooldown").await?;
                    continue;
                };
                let _ = db::hydrate_precache_identity(&self.pool, &entry, size, mtime).await?;
                let Some(hydrated) = db::precache_entry(&self.pool, &entry.media_id).await? else { continue };
                if hydrated.status != "queued" || hydrated.source_path != entry.source_path
                    || hydrated.source_size != size || hydrated.source_mtime != mtime || !hydrated.identity_known {
                    continue;
                }
                entry = hydrated;
            }
            self.initialize_index().await?;
            let Some(reservation) = self.admit_and_claim(&entry).await? else { continue };
            let result = self.process(&entry).await;
            let result = match result {
                Ok(ProcessOutcome::Yielded) => {
                    reservation.retain_for_resume();
                    Ok(ProcessOutcome::Yielded)
                }
                Ok(ProcessOutcome::Complete(bytes)) => reservation.publish(bytes).map(|()| {
                    if entry.mode == "copy" {
                        if let Some(path) = &entry.cache_path {
                            let _ = std::fs::remove_file(format!("{path}.part.resume"));
                        }
                    }
                    ProcessOutcome::Complete(bytes)
                }),
                Err(error) => {
                    if entry.mode == "copy" && self.shutdown.load(Ordering::Relaxed) {
                        reservation.release_without_deleting_partial();
                    }
                    Err(error)
                }
            };
            if result.is_err() && entry.mode == "copy" && !self.shutdown.load(Ordering::Relaxed) {
                if let Some(path) = &entry.cache_path {
                    let _ = std::fs::remove_file(format!("{path}.part.resume"));
                }
            }
            match result {
                Ok(ProcessOutcome::Yielded) => {
                    sqlx::query("UPDATE precache_entries SET status='queued', error=NULL WHERE media_id=? AND generation=? AND status='running'")
                        .bind(&entry.media_id).bind(&entry.generation).execute(&self.pool).await?;
                }
                Ok(ProcessOutcome::Complete(bytes)) => {
                    db::set_precache_status(&self.pool, &entry.media_id, "ready", bytes, None)
                        .await?;
                    tracing::info!(media = %entry.media_id, mode = %entry.mode, bytes, "precache ready");
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if self.shutdown.load(Ordering::Relaxed) && entry.mode == "copy" {
                        sqlx::query("UPDATE precache_entries SET status='queued', error=NULL WHERE media_id=? AND generation=? AND status='running'")
                            .bind(&entry.media_id).bind(&entry.generation).execute(&self.pool).await?;
                    } else if message.contains("preempted by live playback") {
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

    // The mutex serializes index admission and eviction. The database snapshot is
    // coherent at read time; later schedule changes are not locked by this mutex.
    async fn admit_and_claim(&self, entry: &db::PrecacheEntry) -> Result<Option<CacheReservation>> {
        let _guard = self.eviction_lock.lock().await;
        let recovered = {
            let state = self.index.0.state.lock().unwrap();
            state.copying.get(&entry.media_id).filter(|(size, path, identity)|
                *size == entry.source_size && Some(path.to_string_lossy().as_ref()) == entry.cache_path.as_deref()
                && *identity == SourceIdentity::new(&entry.source_path, entry.source_size, entry.source_mtime)).cloned()
        };
        if let Some((_, path, identity)) = recovered {
            let partial = PathBuf::from(format!("{}.part", path.display()));
            let manifest = PathBuf::from(format!("{}.resume", partial.display()));
            let checkpoint_identity = resume::Identity { path: entry.source_path.clone(), size: entry.source_size.max(0) as u64, mtime: entry.source_mtime };
            if resume::load(&manifest, &partial, &entry.media_id, &entry.generation, &checkpoint_identity).is_err() {
                self.index.0.state.lock().unwrap().copying.remove(&entry.media_id);
                return Ok(None);
            }
            if !db::claim_queued_precache(&self.pool, entry).await? { return Ok(None); }
            return Ok(Some(CacheReservation { index: self.index.clone(), id: entry.media_id.clone(), identity, path, partial, active: true, delete_partial: true }));
        }
        let now = db::now_ms();
        let mut snapshot = db::precache_snapshot(&self.pool, now, now.saturating_add(self.ahead_ms)).await?;
        let targets = |snapshot: &db::PrecacheSnapshot| {
            let rows: Vec<_> = snapshot.schedule.iter().map(|r| db::UpcomingMedia {
                channel_id: r.channel_id.clone(), media_id: r.media_id.clone(), start_ms: r.start_ms, end_ms: r.end_ms,
                path: r.path.clone().unwrap_or_default(), duration_ms: r.duration_ms.unwrap_or(0), size_bytes: r.size_bytes.unwrap_or(0),
            }).collect();
            mandatory_targets(&rows, now)
        };
        let tiers = targets(&snapshot);
        let keep: HashSet<String> = tiers.iter().flatten().cloned().collect();
        // Reclaim only for actual occupancy, using the existing protected victims.
        if !self.budget_allows_locked(entry.source_size, &keep).await? {
            db::defer_queued_precache(&self.pool, entry, if self.rolling_chunks { "cache reservation unavailable; paused partials retain full-source capacity" } else { "cache reservation unavailable" }).await?;
            return Ok(None);
        }
        // Eviction changes occupancy and can take time: recompute from a fresh snapshot.
        snapshot = db::precache_snapshot(&self.pool, now, now.saturating_add(self.ahead_ms)).await?;
        let tiers = targets(&snapshot);
        let entries: HashMap<_, _> = snapshot.entries.iter().map(|e| (e.media_id.as_str(), e)).collect();
        let demands: Vec<_> = tiers.iter().enumerate().flat_map(|(priority, ids)| ids.iter().map(move |id| (priority, id)))
            .map(|(priority, id)| {
                let e = entries.get(id.as_str());
                let row = snapshot.schedule.iter().find(|r| &r.media_id == id);
                MandatoryDemand { media_id: id.clone(), priority,
                    identity: e.filter(|e| e.identity_known).map(|e| SourceIdentity::new(&e.source_path, e.source_size, e.source_mtime)),
                    bytes: e.filter(|e| e.identity_known && e.source_size > 0).map(|e| e.source_size as i128)
                        .or_else(|| row.and_then(|r| r.size_bytes.filter(|n| *n > 0)).map(i128::from)) }
            }).collect();
        let incoming = MandatoryDemand { media_id: entry.media_id.clone(), priority: 0,
            identity: Some(SourceIdentity::new(&entry.source_path, entry.source_size, entry.source_mtime)), bytes: Some(entry.source_size as i128) };
        let mandatory = demands.iter().any(|d| d.media_id == entry.media_id);
        let cache = {
            let state = self.index.0.state.lock().unwrap();
            let mut cache: Vec<_> = state.ready.iter().map(|(id, e)| AccountedCache::Ready { media_id: id.clone(), identity: e.identity.clone(), bytes: e.bytes as i128,
                valid: !e.refresh_pending && std::fs::metadata(&e.path).map(|m| m.is_file() && m.len() == e.bytes as u64).unwrap_or(false) }).collect();
            cache.extend(state.copying.iter().map(|(id, (bytes, path, identity))| AccountedCache::Copying { media_id: id.clone(), identity: identity.clone(), reserved: *bytes as i128, completed: partial_progress(path, *bytes) }));
            cache.extend(state.evicting.values().map(|e| AccountedCache::Evicting { bytes: e.bytes as i128 }));
            cache.extend(state.extra.values().map(|e| AccountedCache::Extra { reserved: e.reserved, remaining: e.remaining() }));
            cache
        };
        let free = available_space(&self.dir)? as i128;
        let accounting = AdmissionAccounting::calculate(&demands, &cache, if mandatory { None } else { Some(&incoming) }, self.max_bytes as i128, free, self.free_space_floor as i128)?;
        let allowed = if mandatory {
            accounting.occupied + entry.source_size as i128 <= self.max_bytes as i128
                && free >= self.free_space_floor as i128 + accounting.running_remaining + entry.source_size as i128
        } else { accounting.optional_allowed };
        if !allowed {
            let reason = if free < self.free_space_floor as i128 + accounting.running_remaining + entry.source_size as i128 {
                "insufficient local free space"
            } else { "cache reservation unavailable" };
            db::defer_queued_precache(&self.pool, entry, reason).await?;
            return Ok(None);
        }
        let Some(path) = entry.cache_path.as_ref() else { bail!("precache entry has no cache path") };
        let Some(mut reservation) = self.index.reserve(&entry.media_id, incoming.identity.unwrap(), PathBuf::from(path), entry.source_size) else {
            db::defer_queued_precache(&self.pool, entry, "cache reservation unavailable").await?;
            return Ok(None)
        };
        // A cancelled or failed claim must not touch a partial it never owned.
        reservation.delete_partial = false;
        match db::claim_queued_precache(&self.pool, entry).await {
            Ok(true) => {
                reservation.delete_partial = true;
                Ok(Some(reservation))
            },
            result => {
                // Until the database claim succeeds this reservation has no
                // authority to remove an existing generation's partial.
                reservation.release_without_deleting_partial();
                result.map(|_| None)
            }
        }
    }

    async fn process(&self, entry: &db::PrecacheEntry) -> Result<ProcessOutcome> {
        let cache_path = entry
            .cache_path
            .as_deref()
            .context("precache entry has no cache path")?;
        let partial = format!("{cache_path}.part");
        if entry.mode == "copy" {
            if self.copy_source(entry, &partial).await? == CopyOutcome::Yielded {
                return Ok(ProcessOutcome::Yielded);
            }
        } else {
            let _ = tokio::fs::remove_file(&partial).await;
            self.transcode_source(entry, &partial).await?;
        }
        let bytes = tokio::fs::metadata(&partial).await?.len() as i64;
        if bytes <= 0 {
            bail!("precache produced an empty file");
        }
        Ok(ProcessOutcome::Complete(bytes))
    }

    async fn copy_source(&self, entry: &db::PrecacheEntry, partial: &str) -> Result<CopyOutcome> {
        let source = tokio::fs::canonicalize(&entry.source_path).await?;
        let cache_dir = tokio::fs::canonicalize(&self.dir).await?;
        if source.starts_with(&cache_dir) { bail!("precache source is inside cache directory"); }
        let input = tokio::fs::File::open(&source).await?;
        let source_meta = input.metadata().await?;
        if source_meta.len() as i64 != entry.source_size || metadata_mtime(&source_meta) != entry.source_mtime || entry.source_size <= 0 {
            bail!("precache source identity changed before copy");
        }
        let input = input.into_std().await;
        let identity = resume::Identity { path: entry.source_path.clone(), size: entry.source_size as u64, mtime: entry.source_mtime };
        let partial_path = PathBuf::from(partial);
        let manifest = PathBuf::from(format!("{partial}.resume"));
        let owner = entry.media_id.clone();
        let generation = entry.generation.clone();
        let (mut total, partial_path, manifest, identity, owner, generation) = tokio::task::spawn_blocking(move || -> Result<_> {
            let offset = if manifest.exists() {
                resume::load(&manifest, &partial_path, &owner, &generation, &identity)?
            } else {
                std::fs::File::create(&partial_path)?;
                resume::save(&manifest, &partial_path, &owner, &generation, &identity, 0)?;
                0
            };
            Ok((offset, partial_path, manifest, identity, owner, generation))
        }).await??;
        let initial = total;
        #[cfg(test)]
        let quantum_bytes = self.copy_quantum_bytes.max(1);
        #[cfg(not(test))]
        let quantum_bytes = COPY_QUANTUM_BYTES;
        let mut next_report = ((total / (64 * 1024 * 1024)) + 1) * (64 * 1024 * 1024);
        let started = Instant::now();
        while total < identity.size {
            while self.paused.load(Ordering::Relaxed) && !self.shutdown.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if self.shutdown.load(Ordering::Relaxed) { bail!("precache copy cancelled"); }
            let live = self.direct_reader_active();
            let chunk = (identity.size - total).min(4 * 1024 * 1024).min(quantum_bytes.saturating_sub(total - initial).max(1));
            if !space_allows(available_space(&self.dir)?, self.free_space_floor, chunk) {
                bail!("insufficient local free space during precache copy");
            }
            let mut slice_input = input.try_clone()?;
            let partial_copy = partial_path.clone();
            let manifest_copy = manifest.clone();
            let identity_copy = identity.clone();
            let owner_copy = owner.clone();
            let generation_copy = generation.clone();
            total = tokio::task::spawn_blocking(move || -> Result<u64> {
                let mut output = std::fs::OpenOptions::new().write(true).open(&partial_copy)?;
                let end = resume::copy_slice(&mut slice_input, &mut output, total, identity_copy.size, chunk)?;
                resume::save(&manifest_copy, &partial_copy, &owner_copy, &generation_copy, &identity_copy, end)?;
                Ok(end)
            }).await??;
            // Throttle only bytes copied in this invocation.
            let target = if live { (self.copy_bytes_per_sec / 8).max(256_000) } else { self.copy_bytes_per_sec };
            let expected = (total - initial) as f64 / target as f64;
            let elapsed = started.elapsed().as_secs_f64();
            if expected > elapsed { tokio::time::sleep(Duration::from_secs_f64(expected - elapsed)).await; }
            if self.shutdown.load(Ordering::Relaxed) { bail!("precache copy cancelled"); }
            if total < identity.size && (total - initial >= quantum_bytes || started.elapsed() >= COPY_QUANTUM_TIME) {
                db::set_precache_progress(&self.pool, &entry.media_id, &entry.generation, total as i64).await?;
                return Ok(CopyOutcome::Yielded);
            }
            if total >= next_report {
                db::set_precache_progress(&self.pool, &entry.media_id, &entry.generation, total as i64).await?;
                next_report += 64 * 1024 * 1024;
            }
        }
        if self.shutdown.load(Ordering::Relaxed) { bail!("precache copy cancelled"); }
        let after = input.metadata()?;
        let path_after = source_metadata(&entry.source_path).await.context("source metadata unavailable after copy")?;
        if !same_source(&source_meta, &after) || !same_source(&after, &path_after) {
            bail!("precache source identity changed during copy");
        }
        if total != entry.source_size as u64 {
            bail!(
                "copied {total} of {} bytes from {}",
                entry.source_size,
                entry.source_path
            );
        }
        Ok(CopyOutcome::Complete)
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
                    if !space_allows(available_space(&self.dir).unwrap_or(0), self.free_space_floor, 0) {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        bail!("insufficient local free space during precache proxy");
                    }
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
        if estimate <= 0 || estimate > self.max_bytes { return Ok(false); }
        self.initialize_index().await?;
        let _guard = self.eviction_lock.lock().await;
        self.budget_allows_locked(estimate, keep).await
    }

    async fn budget_allows_locked(&self, estimate: i64, keep: &HashSet<String>) -> Result<bool> {
        if estimate <= 0 || estimate > self.max_bytes { return Ok(false); }
        loop {
            let victim = {
                let mut state = self.index.0.state.lock().unwrap();
                let used: i128 = state.occupied_bytes();
                if used + estimate as i128 <= self.max_bytes as i128 { return Ok(true); }
                let id = state.ready.iter().filter(|(id, e)| e.leases == 0 && !keep.contains(*id))
                    .min_by_key(|(_, e)| e.unused).map(|(id, _)| id.clone());
                id.map(|id| {
                    let entry = state.ready.remove(&id).unwrap();
                    let path = entry.path.clone();
                    state.evicting.insert(id.clone(), entry);
                    (id, path)
                })
            };
            let Some((id, path)) = victim else { return Ok(false); };
            // Persist the invalidation before touching the file. A cancelled task
            // leaves the entry unavailable until restart, never leased mid-unlink.
            if !db::invalidate_ready_precache(&self.pool, &id).await? { bail!("eviction victim is no longer ready"); }
            match std::fs::remove_file(&path) {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
            self.index.0.state.lock().unwrap().evicting.remove(&id);
        }
    }

}

#[cfg(unix)]
fn same_source(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino() && a.len() == b.len()
        && a.mtime() == b.mtime() && a.mtime_nsec() == b.mtime_nsec()
}

#[cfg(not(unix))]
fn same_source(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

fn metadata_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64).unwrap_or(0)
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

#[derive(serde::Serialize)]
pub struct CoverageTarget {
    role: &'static str, media_id: String, title: String, start_ms: i64, end_ms: i64,
    required: bool, waiver: Option<&'static str>, cache_state: String,
    failure_reason: Option<String>, local_file: &'static str, ready: bool,
    mode: &'static str, required_source_interval: Option<(i64, i64)>,
    verified_ranges: Vec<(i64, i64)>, missing_ranges: Vec<(i64, i64)>,
    startup_buffer_ready: bool, window_ready: bool,

}
#[derive(serde::Serialize)]
pub struct CoverageChannel {
    id: String, number: String, name: String, gap: bool, targets: Vec<CoverageTarget>,
}
#[derive(serde::Serialize)]
pub struct CoverageReport {
    snapshot_ms: i64, channels: Vec<CoverageChannel>, required_media_count: usize,
    known_size_bytes: u64, unknown_size_count: usize, budget_bytes: u64,
    free_headroom_bytes: Option<u64>, additional_known_bytes: u64, fit: Option<bool>,
    known_pending_bytes: Option<u64>, budget_deficit_bytes: Option<u64>, free_deficit_bytes: Option<u64>,
    headroom_reason: &'static str,
    startup_ready_count: usize, window_ready_count: usize,
}

// Only inspect direct children of the configured local cache; never follow a payload symlink.
fn local_ready_file(dir: &std::path::Path, path: Option<&str>, bytes: i64) -> &'static str {
    let Some(path) = path.map(std::path::Path::new) else { return "missing_path" };
    if path.parent() != Some(dir) || path.file_name().is_none() { return "invalid_path" }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => "invalid_file",
        Ok(meta) if bytes <= 0 || meta.len() != bytes as u64 => "length_mismatch",
        Ok(_) => "verified",
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unreadable",
    }
}

impl Precache {
    pub async fn coverage(&self, now: i64) -> Result<CoverageReport> {
        let _guard = self.eviction_lock.lock().await;
        let snapshot = db::precache_snapshot(&self.pool, now, now.saturating_add(self.ahead_ms)).await?;
        let entries: HashMap<_, _> = snapshot.entries.iter().map(|e| (e.media_id.clone(), e)).collect();
        let mut required = HashMap::<String, (u64, bool, bool)>::new();
        let mut channels = Vec::new();
        let chunk_index = self.shared_chunk_index().await?;
        let sources = self.verified_sources.lock().unwrap().clone();
        let mut startup_ready_count = 0;
        let mut window_ready_count = 0;
        for channel in &snapshot.channels {
            let items: Vec<_> = snapshot.schedule.iter().filter(|r| r.channel_id == channel.id).take(3).collect();
            let policy: Vec<db::UpcomingMedia> = items.iter().map(|r| db::UpcomingMedia {
                channel_id: channel.id.clone(), media_id: r.media_id.clone(), path: r.path.clone().unwrap_or_default(), duration_ms: r.duration_ms.unwrap_or(0), size_bytes: r.size_bytes.unwrap_or(0), start_ms: r.start_ms, end_ms: r.end_ms,
            }).collect();
            let tiers = mandatory_targets(&policy, now);
            let current = policy.iter().position(|r| r.start_ms <= now && r.end_ms > now);
            let mut targets = Vec::new();
            {
                let i = current.unwrap_or(0);
                for (offset, row) in items.iter().skip(i).take(if current.is_some() { 3 } else { 2 }).enumerate() {
                    let role_index = offset + usize::from(current.is_none());
                    let item = &policy[i + offset];
                    let required_target = current.is_some() && tiers[role_index].contains(&item.media_id);
                    let entry = entries.get(&item.media_id);
                    let file = entry.filter(|e| e.status == "ready").map(|e| local_ready_file(&self.dir, e.cache_path.as_deref(), e.bytes)).unwrap_or("not_checked");
                    let ready = file == "verified";
                    let copy_verified = ready && entry.is_some_and(|e| e.mode == "copy" && e.identity_known && e.source_path == item.path && {
                        let state = self.index.0.state.lock().unwrap();
                        let db_identity = SourceIdentity::new(&e.source_path, e.source_size, e.source_mtime);
                        state.ready.get(&item.media_id).is_some_and(|cached| !cached.refresh_pending && cached.identity == db_identity)
                            && sources.get(&(item.media_id.clone(), e.source_path.clone())).is_none_or(|verified| *verified == db_identity)
                    });
                    let rolling = self.rolling_chunks && !copy_verified && rolling_video_eligible(row.media_kind.as_deref().unwrap_or(""), row.video_codec.as_deref(), row.audio_codec.as_deref());
                    let mode = if rolling { "rolling_chunks" } else if self.rolling_chunks && !copy_verified { "full_copy_audio_fallback" } else { "full_copy" };
                    let start = row.in_ms.saturating_add(if role_index == 0 { now.saturating_sub(row.start_ms) } else { 0 });
                    let end = row.in_ms.saturating_add(row.end_ms.saturating_sub(row.start_ms)).min(start.saturating_add(3_600_000));
                    let interval = (row.in_ms >= 0 && start < end).then_some((start, end));
                    let identity = row.path.as_ref().and_then(|path| sources.get(&(item.media_id.clone(), path.clone())));
                    let ranges = if rolling { interval.and_then(|(a,b)| identity.zip(chunk_index.as_ref()).map(|(id,index)| index.verified_ranges(id,a,b))).unwrap_or_default() } else if copy_verified { interval.into_iter().collect() } else { Vec::new() };
                    let mut missing = Vec::new();
                    if let Some((a,b)) = interval { let mut cursor = a; for &(x,y) in &ranges { if cursor < x { missing.push((cursor,x)); } cursor = cursor.max(y); } if cursor < b { missing.push((cursor,b)); } }
                    let startup = if rolling { interval.is_some_and(|(a,b)| ranges.first().is_some_and(|&(x,y)| x == a && y >= b.min(a.saturating_add(300_000)))) } else if self.rolling_chunks { copy_verified } else { ready };
                    let window = if rolling { interval.is_some() && missing.is_empty() } else if self.rolling_chunks { copy_verified } else { ready };
                    if required_target { startup_ready_count += usize::from(startup); window_ready_count += usize::from(window); }
                    if required_target {
                        // Completed payload sizes are exact; other sizes are catalog source estimates.
                        // Estimates do not establish proxy output size or feasibility.
                        let size = if ready { entry.unwrap().bytes.max(0) as u64 } else {
                            entry.filter(|e| e.mode == "copy" && e.identity_known && e.source_path == item.path && e.source_size > 0)
                                .map(|e| e.source_size as u64).unwrap_or(item.size_bytes.max(0) as u64)
                        };
                        let exact = size > 0;
                        required.entry(item.media_id.clone()).or_insert((size, exact, ready));
                    }
                    targets.push(CoverageTarget { role: ["current", "next", "third"][role_index], media_id: item.media_id.clone(), title: row.title.clone(), start_ms: item.start_ms, end_ms: item.end_ms, required: required_target,
                        waiver: if current.is_none() { Some("schedule_gap") } else if role_index == 1 && item.start_ms > now.saturating_add(3_600_000) { Some("next_beyond_hour") } else if !required_target { Some("policy_not_required") } else { None },
                        cache_state: entry.map(|e| e.status.clone()).unwrap_or_else(|| "pending_unindexed".into()), failure_reason: entry.filter(|e| e.status == "failed").and_then(|e| e.error.clone()), local_file: file, ready: window, mode, required_source_interval: interval, verified_ranges: ranges, missing_ranges: missing, startup_buffer_ready: startup, window_ready: window });
                }
            }
            channels.push(CoverageChannel { id: channel.id.clone(), number: channel.number.clone(), name: channel.name.clone(), gap: current.is_none(), targets });
        }
        // Capture the same schedule and index inputs as admission while holding its lock.
        let schedule: Vec<_> = snapshot.schedule.iter().map(|r| db::UpcomingMedia { channel_id: r.channel_id.clone(), media_id: r.media_id.clone(), start_ms: r.start_ms, end_ms: r.end_ms, path: r.path.clone().unwrap_or_default(), duration_ms: r.duration_ms.unwrap_or(0), size_bytes: r.size_bytes.unwrap_or(0) }).collect();
        let tiers = mandatory_targets(&schedule, now);
        let indexed: HashMap<_, _> = snapshot.entries.iter().map(|e| (e.media_id.as_str(), e)).collect();
        let demands: Vec<_> = tiers.iter().enumerate().flat_map(|(priority, ids)| ids.iter().map(move |id| (priority, id))).map(|(priority, id)| {
            let e = indexed.get(id.as_str());
            let row = snapshot.schedule.iter().find(|r| &r.media_id == id);
            MandatoryDemand { media_id: id.clone(), priority, identity: e.filter(|e| e.identity_known).map(|e| SourceIdentity::new(&e.source_path, e.source_size, e.source_mtime)), bytes: e.filter(|e| e.identity_known && e.source_size > 0).map(|e| e.source_size as i128).or_else(|| row.and_then(|r| r.size_bytes.filter(|n| *n > 0)).map(i128::from)) }
        }).collect();
        let cache = {
            let state = self.index.0.state.lock().unwrap();
            let mut cache: Vec<_> = state.ready.iter().map(|(id, e)| AccountedCache::Ready { media_id: id.clone(), identity: e.identity.clone(), bytes: e.bytes as i128, valid: !e.refresh_pending && local_ready_file(&self.dir, e.path.to_str(), e.bytes) == "verified" }).collect();
            cache.extend(state.copying.iter().map(|(id, (bytes, path, identity))| AccountedCache::Copying { media_id: id.clone(), identity: identity.clone(), reserved: *bytes as i128, completed: partial_progress(path, *bytes) }));
            cache.extend(state.evicting.values().map(|e| AccountedCache::Evicting { bytes: e.bytes as i128 }));
            cache.extend(state.extra.values().map(|e| AccountedCache::Extra { reserved: e.reserved, remaining: e.remaining() }));
            cache
        };
        let free = available_space(&self.dir).ok();
        let accounting = free.filter(|_| self.index_initialized.get().is_some()).map(|free| AdmissionAccounting::calculate(&demands, &cache, None, self.max_bytes.max(0) as i128, free as i128, self.free_space_floor as i128)).transpose()?;
        let known_size_bytes = required.values().filter(|(_, exact, _)| *exact).map(|(size, _, _)| *size).fold(0u64, u64::saturating_add);
        let unknown_size_count = required.values().filter(|(_, exact, _)| !*exact).count();
        let additional_known_bytes = required.values().filter(|(_, exact, ready)| *exact && !*ready).map(|(size, _, _)| *size).fold(0u64, u64::saturating_add);
        let budget_bytes = self.max_bytes.max(0) as u64;
        let free_headroom_bytes = available_space(&self.dir).ok().map(|space| space.saturating_sub(self.free_space_floor));
        let ready_bytes = required.values().filter(|(_, _, ready)| *ready).map(|(size, _, _)| *size).fold(0u64, u64::saturating_add);
        let fit = if ready_bytes > budget_bytes { Some(false) } else if unknown_size_count == 0 && additional_known_bytes == 0 { Some(true) } else { None };
        let known_pending_bytes = accounting.as_ref().map(|a| a.pending.min(u64::MAX as i128) as u64);
        let budget_deficit_bytes = accounting.as_ref().map(|a| a.budget_deficit.min(u64::MAX as i128) as u64);
        let free_deficit_bytes = accounting.as_ref().map(|a| a.free_deficit.min(u64::MAX as i128) as u64);
        let headroom_reason = match accounting.as_ref() { None if self.index_initialized.get().is_none() => "cache_index_unavailable", None => "free_space_unavailable", Some(a) if a.unknown => "unknown_required_size", Some(a) if a.budget_deficit > 0 && a.free_deficit > 0 => "budget_and_free_space", Some(a) if a.budget_deficit > 0 => "budget", Some(a) if a.free_deficit > 0 => "free_space_floor", Some(_) => "sufficient" };
        Ok(CoverageReport { snapshot_ms: now, channels, required_media_count: required.len(), known_size_bytes, unknown_size_count, budget_bytes, free_headroom_bytes, additional_known_bytes, fit, known_pending_bytes, budget_deficit_bytes, free_deficit_bytes, headroom_reason, startup_ready_count, window_ready_count })
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn required_legacy_proxy_migrates_only_queued_generation() {
        let root = std::env::temp_dir().join(format!("proxy-migration-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone());
        for id in ["queued", "ready", "running", "optional"] {
            db::upsert_precache(&pool, id, &format!("/media/{id}.mkv"), 10, 1, &format!("{}/{}.proxy.mp4", config.precache_dir.display(), id), "proxy", 0, 0).await.unwrap();
        }
        db::set_precache_status(&pool, "ready", "ready", 10, None).await.unwrap();
        db::set_precache_status(&pool, "running", "running", 0, None).await.unwrap();
        service.migrate_required_proxies(&HashMap::from([("queued".into(), 12345), ("ready".into(), 12345), ("running".into(), 12345)])).await.unwrap();
        let queued = db::precache_entry(&pool, "queued").await.unwrap().unwrap();
        assert_eq!(queued.mode, "copy");
        assert_eq!(queued.start_ms, 12345);
        assert!(queued.cache_path.unwrap().ends_with("queued.mkv"));
        for id in ["ready", "running", "optional"] { assert_eq!(db::precache_entry(&pool, id).await.unwrap().unwrap().mode, "proxy"); }
    }
    #[tokio::test]
    async fn metadata_timeout_retains_permit_until_blocking_operation_finishes() {
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let result = bounded_blocking(permits.clone(), std::time::Duration::from_millis(30), move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }).await;
        started_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert!(result.is_none());
        assert_eq!(permits.available_permits(), 0);
        assert!(bounded_blocking(permits.clone(), std::time::Duration::from_millis(30), || ()).await.is_none());
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while permits.available_permits() == 0 { tokio::task::yield_now().await; }
        }).await.unwrap();
    }
    use super::*;

    #[tokio::test]
    async fn required_metadata_hydrates_past_missing_source_without_copy_worker() {
        let root = std::env::temp_dir().join(format!("hydrate-fair-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone());
        let mut ids = Vec::new();
        for n in 0..9 {
            let id = format!("hydrate-{n}"); let path = root.join(format!("source-{n}"));
            if n != 0 { std::fs::write(&path, vec![1u8; n + 1]).unwrap(); }
            db::upsert_precache(&pool, &id, path.to_str().unwrap(), 0, 0, root.join(&id).to_str().unwrap(), "copy", 1, 1).await.unwrap();
            sqlx::query("UPDATE precache_entries SET identity_known=0 WHERE media_id=?").bind(&id).execute(&pool).await.unwrap();
            ids.push(id);
        }
        service.hydrate_required_metadata(&ids).await.unwrap();
        assert!(!db::precache_entry(&pool, &ids[0]).await.unwrap().unwrap().identity_known);
        for (n, id) in ids.iter().enumerate().skip(1) {
            let entry = db::precache_entry(&pool, id).await.unwrap().unwrap();
            assert!(entry.identity_known, "{id}"); assert_eq!(entry.source_size, (n + 1) as i64);
        }
        pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn mandatory_targets_materialize_in_one_cycle_beyond_optional_queue_cap() {
        let root = std::env::temp_dir().join(format!("mandatory-planner-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Arc::new(Precache::new(&config, pool.clone()));
        service.running.store(true, Ordering::SeqCst);
        let now = db::now_ms();
        for n in 0..12 {
            let channel = format!("channel-{n}");
            sqlx::query("INSERT INTO channels (id,number,name,start_ms,created_at) VALUES (?,?,?,0,0)").bind(&channel).bind(n.to_string()).bind(&channel).execute(&pool).await.unwrap();
            for role in 0..3 {
                let id = if n == 1 && role == 0 { "media-0-0".to_string() } else { format!("media-{n}-{role}") };
                sqlx::query("INSERT OR IGNORE INTO media_items (id,path,title,duration_ms,size_bytes,created_at) VALUES (?,?,?,60000,?,0)").bind(&id).bind(format!("/never-stat/{id}")).bind(&id).bind(if n == 11 && role == 2 { None } else { Some(4_i64) }).execute(&pool).await.unwrap();
                let start = now - 30_000 + role * 60_000;
                sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms) VALUES (?,?,?,?,?,?)").bind(&channel).bind(role).bind(&id).bind(&id).bind(start).bind(start + 60_000).execute(&pool).await.unwrap();
            }
        }
        let cached = config.precache_dir.join("ready"); std::fs::write(&cached, b"abcd").unwrap();
        db::upsert_precache(&pool, "media-0-0", "/never-stat/media-0-0", 4, 1, cached.to_str().unwrap(), "copy", now, now).await.unwrap();
        db::set_precache_status(&pool, "media-0-0", "ready", 4, None).await.unwrap();
        for n in 0..32 {
            let id = format!("optional-{n}");
            sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,size_bytes,created_at) VALUES (?,?,?,60000,4,0)").bind(&id).bind(format!("/never-stat/{id}")).bind(&id).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms) VALUES ('channel-0',?,?,?,?,?)").bind(3+n).bind(&id).bind(&id).bind(now + 600_000 + n * 60_000).bind(now + 660_000 + n * 60_000).execute(&pool).await.unwrap();
            db::upsert_precache(&pool, &id, "/never-stat/optional", 4, 1, "unused", "copy", now, now).await.unwrap();
        }
        for (id, age, retries, reason) in [("media-2-0", 31_000_i64, 1_i64, "source unavailable"), ("media-3-0", 31_000, 2, "source unavailable"), ("media-4-0", 31_000, 0, "evicted")] {
            db::upsert_precache(&pool, id, "/never-stat/source", 4, 1, config.precache_dir.join(id).to_str().unwrap(), "copy", now, now).await.unwrap();
            sqlx::query("UPDATE precache_entries SET status='failed', error=?, updated_ms=?, retry_count=? WHERE media_id=?").bind(reason).bind(now-age).bind(retries).bind(id).execute(&pool).await.unwrap();
        }
        service.tick_inner().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "media-2-0").await.unwrap().unwrap().status, "queued");
        assert_eq!(db::precache_entry(&pool, "media-3-0").await.unwrap().unwrap().status, "failed");
        assert_eq!(db::precache_entry(&pool, "media-4-0").await.unwrap().unwrap().status, "queued");
        let rows = db::upcoming_media(&pool, now, now + service.ahead_ms).await.unwrap();
        let required: HashSet<_> = mandatory_targets(&rows, now).into_iter().flatten().collect();
        assert_eq!(required.len(), 35);
        for id in &required { let entry = db::precache_entry(&pool, id).await.unwrap().expect("missing required entry"); assert!(!entry.generation.is_empty(), "empty generation for {id}"); }
        assert_eq!(db::precache_entry(&pool, "media-0-0").await.unwrap().unwrap().status, "ready");
        assert_eq!(db::precache_entry(&pool, "media-11-2").await.unwrap().unwrap().source_size, 0);
        service.tick_inner().await.unwrap();
        for id in &required { assert!(db::precache_entry(&pool, id).await.unwrap().is_some(), "trimmed mandatory {id}"); }
        assert!(db::precache_entry(&pool, "optional-0").await.unwrap().is_none());
        drop(service); pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn mandatory_generation_first_quantum_and_legacy_repair() {
        for legacy in [false, true] {
            let root = std::env::temp_dir().join(format!("generation-{}", uuid::Uuid::new_v4()));
            let pool = db::init(&root).await.unwrap();
            let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
            let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
            service.free_space_floor = 0; service.copy_quantum_bytes = 1024; service.copy_bytes_per_sec = 100_000_000;
            let service = Arc::new(service); service.running.store(true, Ordering::SeqCst);
            let source = root.join("source"); std::fs::write(&source, vec![42u8; 2048]).unwrap();
            let now = db::now_ms();
            sqlx::query("INSERT INTO channels (id,number,name,start_ms,created_at) VALUES ('c','1','c',0,0)").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,size_bytes,created_at) VALUES ('m',?,'m',60000,2048,0)").bind(source.to_str().unwrap()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms) VALUES ('c',0,'m','m',?,?)").bind(now-1000).bind(now+59000).execute(&pool).await.unwrap();
            service.tick_inner().await.unwrap();
            let first = db::precache_entry(&pool, "m").await.unwrap().unwrap();
            assert!(!first.generation.is_empty());
            if legacy { sqlx::query("DROP TRIGGER precache_generation_update").execute(&pool).await.unwrap(); sqlx::query("UPDATE precache_entries SET generation='' WHERE media_id='m'").execute(&pool).await.unwrap(); }
            service.run_jobs().await.unwrap();
            let yielded = db::precache_entry(&pool, "m").await.unwrap().unwrap();
            assert_eq!(yielded.status, "queued"); assert_eq!(yielded.bytes, 1024); assert!(yielded.error.is_none());
            assert!(!yielded.generation.is_empty());
            if !legacy { assert_eq!(yielded.generation, first.generation); }
            service.tick_inner().await.unwrap();
            let planned = db::precache_entry(&pool, "m").await.unwrap().unwrap();
            assert_eq!(planned.generation, yielded.generation); assert_eq!(planned.bytes, 1024);
            service.run_jobs().await.unwrap();
            let ready = db::precache_entry(&pool, "m").await.unwrap().unwrap();
            assert_eq!(ready.status, "ready"); assert_eq!(ready.generation, yielded.generation);
            drop(service); pool.close().await; std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn unknown_identity_worker_copies_and_skips_unavailable() {
        let root = std::env::temp_dir().join(format!("identity-worker-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers(); service.free_space_floor = 0;
        let service = Arc::new(service); service.running.store(true, Ordering::SeqCst);
        let source = root.join("source"); std::fs::write(&source, b"real source bytes").unwrap();
        let now = db::now_ms();
        for (n, path) in [(0, root.join("missing")), (1, source.clone())] {
            let channel = format!("identity-channel-{n}"); let id = format!("identity-{n}");
            sqlx::query("INSERT INTO channels (id,number,name,start_ms,created_at) VALUES (?,?,?,0,0)").bind(&channel).bind(n.to_string()).bind(&channel).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,size_bytes,created_at) VALUES (?,?,?,60000,NULL,0)").bind(&id).bind(path.to_str().unwrap()).bind(&id).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms) VALUES (?,0,?,?,?,?)").bind(&channel).bind(&id).bind(&id).bind(now-1000+n*100).bind(now+59000).execute(&pool).await.unwrap();
        }
        service.tick_inner().await.unwrap();
        assert!(!db::precache_entry(&pool, "identity-1").await.unwrap().unwrap().identity_known);
        service.run_jobs().await.unwrap();
        let missing = db::precache_entry(&pool, "identity-0").await.unwrap().unwrap();
        assert_eq!(missing.status, "failed"); assert!(missing.error.unwrap().contains("metadata"));
        let ready = db::precache_entry(&pool, "identity-1").await.unwrap().unwrap();
        assert_eq!(ready.status, "ready"); assert!(ready.identity_known);
        assert_eq!(std::fs::read(ready.cache_path.unwrap()).unwrap(), b"real source bytes");
        std::fs::write(root.join("missing"), b"recovered").unwrap();
        sqlx::query("UPDATE precache_entries SET updated_ms = ? WHERE media_id = 'identity-0'").bind(db::now_ms() - 31_000).execute(&pool).await.unwrap();
        service.tick_inner().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "identity-0").await.unwrap().unwrap().status, "queued");
        service.run_jobs().await.unwrap();
        let recovered = db::precache_entry(&pool, "identity-0").await.unwrap().unwrap();
        assert_eq!(recovered.status, "ready");
        assert_eq!(std::fs::read(recovered.cache_path.unwrap()).unwrap(), b"recovered");
        drop(service); pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn hydrated_identity_rejects_source_change_before_copy() {
        let root = std::env::temp_dir().join(format!("identity-change-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone());
        let source = root.join("source"); std::fs::write(&source, b"first").unwrap();
        let dest = config.precache_dir.join("copy");
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), 0, 0, dest.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        sqlx::query("UPDATE precache_entries SET identity_known = 0 WHERE media_id = 'item'").execute(&pool).await.unwrap();
        let entry = db::precache_entry(&pool, "item").await.unwrap().unwrap(); let meta = std::fs::metadata(&source).unwrap();
        assert!(db::hydrate_precache_identity(&pool, &entry, meta.len() as i64, metadata_mtime(&meta)).await.unwrap());
        let hydrated = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        std::fs::write(&source, b"changed length").unwrap();
        assert!(service.process(&hydrated).await.unwrap_err().to_string().contains("identity changed"));
        assert!(!dest.exists());
        db::set_precache_status(&pool, "item", "ready", 5, None).await.unwrap();
        assert!(!db::hydrate_precache_identity(&pool, &entry, 99, 0).await.unwrap());
        let preserved = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!(preserved.status, "ready"); assert_eq!(preserved.source_size, 5);
        pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn coverage_reports_policy_dedup_and_gaps_from_database() {
        let root = std::env::temp_dir().join(format!("coverage-db-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone());
        for (id, number) in [("a", "1"), ("b", "2"), ("gap", "3"), ("waive", "4")] {
            sqlx::query("INSERT INTO channels (id,number,name,start_ms,created_at) VALUES (?,?,?,0,0)").bind(id).bind(number).bind(id).execute(&pool).await.unwrap();
        }
        for id in ["shared", "next", "third", "late"] {
            sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,size_bytes,created_at) VALUES (?,?,?,60000,4,0)").bind(id).bind(format!("/never-stat/{id}")).bind(id).execute(&pool).await.unwrap();
        }
        for (ch, pos, id, start, end) in [("a",0,"shared",-60_000,60_000), ("a",1,"next",60_000,120_000), ("a",2,"third",120_000,180_000), ("b",0,"shared",-60_000,60_000), ("gap",0,"late",60_000,120_000), ("waive",0,"shared",-60_000,60_000), ("waive",1,"late",3_600_001,3_660_001)] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms) VALUES (?,?,?,?,?,?)").bind(ch).bind(pos).bind(id).bind(id).bind(start).bind(end).execute(&pool).await.unwrap();
        }
        sqlx::query("UPDATE media_items SET size_bytes = NULL WHERE id = 'third'").execute(&pool).await.unwrap();
        db::upsert_precache(&pool, "third", "/never-stat/third", 0, 0, config.precache_dir.join("third").to_str().unwrap(), "copy", 0, 0).await.unwrap();
        sqlx::query("UPDATE precache_entries SET identity_known = 0 WHERE media_id = 'third'").execute(&pool).await.unwrap();
        db::set_precache_status(&pool, "third", "failed", 0, Some("explicit failure")).await.unwrap();
        let cached = config.precache_dir.join("shared"); std::fs::write(&cached, b"abcd").unwrap();
        db::upsert_precache(&pool, "shared", "/never-stat/shared", 4, 1, cached.to_str().unwrap(), "copy", 0, 0).await.unwrap();
        db::set_precache_status(&pool, "shared", "ready", 4, None).await.unwrap();
        let unavailable = service.coverage(0).await.unwrap();
        assert_eq!(unavailable.headroom_reason, "cache_index_unavailable");
        assert_eq!(unavailable.known_pending_bytes, None);
        service.initialize_index().await.unwrap();
        let report = service.coverage(0).await.unwrap();
        assert_eq!(report.known_pending_bytes, Some(4));
        assert_eq!(report.headroom_reason, "unknown_required_size");
        assert_eq!(report.required_media_count, 3); assert_eq!(report.known_size_bytes, 8); assert_eq!(report.unknown_size_count, 1); assert_eq!(report.fit, None);
        db::set_precache_status(&pool, "third", "queued", 0, None).await.unwrap();
        let unknown = db::precache_entry(&pool, "third").await.unwrap().unwrap();
        assert!(db::hydrate_precache_identity(&pool, &unknown, 13, 1).await.unwrap());
        let hydrated_report = service.coverage(0).await.unwrap();
        assert_eq!(hydrated_report.unknown_size_count, 0);
        assert_eq!(hydrated_report.known_size_bytes, 21);
        let reservation = service.index.reserve("third", SourceIdentity::new("/never-stat/third", 13, 1), config.precache_dir.join("third"), 13).unwrap();
        db::set_precache_status(&pool, "third", "running", 0, None).await.unwrap();
        let running = service.coverage(0).await.unwrap();
        assert_eq!(running.known_pending_bytes, Some(4));
        std::fs::write(reservation.partial_path(), b"abcde").unwrap();
        assert_eq!(partial_progress(&config.precache_dir.join("third"), 13), 5);
        sqlx::query("UPDATE precache_entries SET source_size=14, source_mtime=2 WHERE media_id='third'").execute(&pool).await.unwrap();
        assert_eq!(service.coverage(0).await.unwrap().known_pending_bytes, Some(18));
        drop(reservation);
        sqlx::query("UPDATE precache_entries SET source_size = 0, source_mtime = 0, identity_known = 0 WHERE media_id = 'third'").execute(&pool).await.unwrap();
        let a = report.channels.iter().find(|c| c.id == "a").unwrap();
        assert!(a.targets.iter().all(|t| t.required)); assert!(a.targets[0].ready); assert_eq!(a.targets[2].role, "third");
        assert_eq!(a.targets[1].cache_state, "pending_unindexed"); assert_eq!(a.targets[2].failure_reason.as_deref(), Some("explicit failure"));
        let gap = report.channels.iter().find(|c| c.id == "gap").unwrap(); assert!(gap.gap); assert_eq!(gap.targets[0].waiver, Some("schedule_gap"));
        let waived = report.channels.iter().find(|c| c.id == "waive").unwrap(); assert_eq!(waived.targets[1].waiver, Some("next_beyond_hour"));
        std::fs::write(&cached, b"abc").unwrap();
        let damaged = service.coverage(0).await.unwrap();
        assert!(!damaged.channels[0].targets[0].ready);
        assert_eq!(damaged.known_pending_bytes, Some(8));
        drop(service); pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn coverage_checks_real_local_file_length() {
        let dir = std::env::temp_dir().join(format!("coverage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("payload");
        assert_eq!(local_ready_file(&dir, Some(file.to_str().unwrap()), 4), "missing");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(local_ready_file(&dir, Some(file.to_str().unwrap()), 4), "length_mismatch");
        std::fs::write(&file, b"abcd").unwrap();
        assert_eq!(local_ready_file(&dir, Some(file.to_str().unwrap()), 4), "verified");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mandatory_targets_use_now_hour_and_full_scheduled_durations() {
        let item = |channel: &str, id: &str, start: i64, end: i64| db::UpcomingMedia {
            channel_id: channel.into(), media_id: id.into(), path: id.into(),
            duration_ms: end - start, size_bytes: 1, start_ms: start, end_ms: end,
        };
        let minute = 60_000;
        let rows = vec![
            item("a", "current", -14 * minute, minute),
            item("a", "next", 60 * minute, 61 * minute),
            item("a", "third", 62 * minute, 63 * minute),
            item("b", "short", -minute, minute),
            item("b", "short-next", 2 * minute, 3 * minute),
            item("b", "extra", 4 * minute, 5 * minute),
            item("c", "gap-current", -minute, minute),
            item("c", "too-late", 60 * minute + 1, 61 * minute),
        ];
        let tiers = mandatory_targets(&rows, 0);
        assert_eq!(tiers, vec![vec!["current", "short", "gap-current"], vec!["next", "short-next"], vec!["extra"]]);
    }

    #[test]
    fn accounting_reserves_mandatory_and_remaining_disk() {
        let id = |name: &str| SourceIdentity::new(name, 1, 1);
        let demand = |name: &str, bytes| MandatoryDemand { media_id: name.into(), priority: 0, identity: Some(id(name)), bytes: Some(bytes) };
        let cache = vec![AccountedCache::Ready { media_id: "ready".into(), identity: id("ready"), bytes: 60, valid: true }, AccountedCache::Copying { media_id: "running".into(), identity: id("running"), reserved: 10, completed: 4 }];
        let required = vec![demand("ready",60), demand("running",10), demand("pending",20), demand("pending",20), demand("pending",20)];
        let a = AdmissionAccounting::calculate(&required, &cache, Some(&demand("optional",10)), 100, 46, 10).unwrap();
        assert_eq!((a.projected, a.additional, a.optional_allowed), (100,36,true));
        let a = AdmissionAccounting::calculate(&[demand("a",80), demand("b",30)], &[], None, 100, 200, 10).unwrap();
        assert_eq!((a.pending,a.budget_deficit), (110,10));
        let stale = vec![AccountedCache::Ready { media_id: "a".into(), identity: id("old"), bytes: 12, valid: true }, AccountedCache::Evicting { bytes: 3 }];
        let a = AdmissionAccounting::calculate(&[demand("a",80)], &stale, Some(&demand("a",80)), 100, 200, 10).unwrap();
        assert_eq!((a.occupied,a.pending,a.optional), (15,80,0));
        let mut unknown = demand("x",1); unknown.bytes = None;
        assert!(!AdmissionAccounting::calculate(&[unknown], &[], None, 100, 200, 10).unwrap().optional_allowed);
        assert!(AdmissionAccounting::calculate(&[demand("x",i128::MAX), demand("y",1)], &[], None, 100, 200, 10).is_err());
    }

    #[test]
    fn mandatory_third_accepts_fourteen_minutes_each() {
        let item = |id: &str, start, end| db::UpcomingMedia { channel_id: "a".into(), media_id: id.into(), path: id.into(), duration_ms: end-start, size_bytes: 1, start_ms: start, end_ms: end };
        let rows = vec![item("a", -60_000, 13*60_000), item("b",13*60_000,27*60_000), item("c",27*60_000,28*60_000)];
        assert_eq!(mandatory_targets(&rows, 0)[2], vec!["c"]);
    }

    #[test]
    fn unfinished_current_blocks_next_and_optional_jobs() {
        let tiers = vec![vec!["a".to_string(), "b".to_string()], vec!["c".to_string()], vec![]];
        let mut statuses = HashMap::new();
        statuses.insert("a".to_string(), ("ready".to_string(), 0));
        statuses.insert("b".to_string(), ("queued".to_string(), 0));
        statuses.insert("c".to_string(), ("queued".to_string(), 0));
        assert_eq!(unfinished_tier(&tiers, &statuses), Some(0));
        statuses.insert("b".to_string(), ("ready".to_string(), 0));
        assert_eq!(unfinished_tier(&tiers, &statuses), Some(1));
    }

    #[test]
    fn eligible_tier_skips_unavailable_and_attempted_targets() {
        let tiers = vec![vec!["unavailable".into(), "large".into(), "small".into()], vec!["next".into()], vec![]];
        let queued = HashSet::from(["large".to_string(), "small".to_string(), "next".to_string()]);
        let attempted = HashSet::from(["large".to_string()]);
        assert_eq!(eligible_tier(&tiers, &queued, &attempted), Some(vec!["small".to_string()]));
        let attempted = HashSet::from(["large".to_string(), "small".to_string()]);
        assert_eq!(eligible_tier(&tiers, &queued, &attempted), Some(vec!["next".to_string()]));
    }

    #[test]
    fn free_space_guard_reserves_headroom_and_copy_size() {
        assert!(!space_allows(10_000, 5_000, 5_001));
        assert!(space_allows(10_000, 5_000, 5_000));
        assert!(!space_allows(4_999, 5_000, 0));
    }

    #[tokio::test]
    async fn insufficient_free_space_keeps_job_queued() {
        let root = std::env::temp_dir().join(format!("space-guard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.free_space_floor = u64::MAX;
        let source = root.join("source.mp4"); let cached = config.precache_dir.join("item.mp4");
        std::fs::write(&source, b"data").unwrap();
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), 4, 1, cached.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        service.run_jobs().await.unwrap();
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!(row.status, "queued");
        assert_eq!(row.error.as_deref(), Some("insufficient local free space"));
        assert!(!cached.exists());
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_zero_mtime_keeps_complete_cache_leaseable_during_validation() {
        let root = std::env::temp_dir().join(format!("legacy-refresh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let source = root.join("source.mp4");
        std::fs::write(&source, b"data").unwrap();
        let cached = config.precache_dir.join("item.mp4");
        std::fs::write(&cached, b"data").unwrap();
        let path = source.to_str().unwrap();
        db::upsert_precache(&pool, "item", path, 4, 0, cached.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "item", "ready", 4, None).await.unwrap();
        let original = SourceIdentity::new(path, 4, 0);
        let lease = service.acquire_ready("item", &original).await.unwrap().unwrap();
        assert!(!service.revalidate_ready("item", path).await.unwrap());
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        let current = SourceIdentity::new(path, row.source_size, row.source_mtime);
        assert!(service.acquire_ready("item", &current).await.unwrap().is_some(), "complete legacy cache became unavailable during refresh");
        assert!(row.source_mtime > 0, "legacy metadata was not migrated");
        assert_eq!(std::fs::read(lease.path()).unwrap(), b"data");
        drop(lease); pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_transition_failure_preserves_index_and_row() {
        let root = std::env::temp_dir().join(format!("legacy-fail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let source = root.join("source"); let cached = config.precache_dir.join("item");
        std::fs::write(&source, b"data").unwrap(); std::fs::write(&cached, b"data").unwrap();
        let identity = SourceIdentity::new(source.to_str().unwrap(), 4, 0);
        db::upsert_precache(&pool, "item", &identity.path, 4, 0, cached.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "item", "ready", 4, None).await.unwrap();
        service.initialize_index().await.unwrap();
        sqlx::query("CREATE TRIGGER reject_validation BEFORE UPDATE ON precache_entries BEGIN SELECT RAISE(FAIL, 'injected'); END").execute(&pool).await.unwrap();
        assert!(service.revalidate_ready("item", &identity.path).await.is_err());
        assert_eq!(db::precache_entry(&pool, "item").await.unwrap().unwrap().source_mtime, 0);
        assert!(service.acquire_ready("item", &identity).await.unwrap().is_some());
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_transition_serializes_acquire_and_survives_cancellation() {
        let root = std::env::temp_dir().join(format!("legacy-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Arc::new(Precache::new(&config, pool.clone()).isolated_from_host_readers());
        let source = root.join("source"); let cached = config.precache_dir.join("item");
        std::fs::write(&source, b"data").unwrap(); std::fs::write(&cached, b"data").unwrap();
        let identity = SourceIdentity::new(source.to_str().unwrap(), 4, 0);
        db::upsert_precache(&pool, "item", &identity.path, 4, 0, cached.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "item", "ready", 4, None).await.unwrap(); service.initialize_index().await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await.unwrap();
        let worker = service.clone(); let path = identity.path.clone();
        let task = tokio::spawn(async move { worker.revalidate_ready("item", &path).await });
        tokio::time::timeout(Duration::from_secs(3), async { loop {
            if service.eviction_lock.try_lock().is_err() { break; } tokio::task::yield_now().await;
        }}).await.unwrap();
        task.abort();
        let reader = service.clone(); let old = identity.clone();
        let acquisition = tokio::spawn(async move { reader.acquire_ready("item", &old).await.unwrap() });
        sqlx::query("COMMIT").execute(&mut *connection).await.unwrap();
        assert!(acquisition.await.unwrap().is_none());
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert!(row.source_mtime > 0);
        assert!(service.acquire_ready("item", &SourceIdentity::new(&identity.path, 4, row.source_mtime)).await.unwrap().is_some());
        drop(connection); pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_defers_replacement_preserving_ready_bytes() {
        let root = std::env::temp_dir().join(format!("refresh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.max_bytes = 4; service.index = CacheIndex::new(4, false);
        let service = Arc::new(service);
        let source = root.join("source.mp4");
        std::fs::write(&source, b"old!").unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let identity = SourceIdentity::new(source.to_str().unwrap(), 4, metadata_mtime(&meta));
        let cached = config.precache_dir.join("item.mp4");
        std::fs::write(&cached, b"old!").unwrap();
        db::upsert_precache(&pool, "item", &identity.path, 4, identity.mtime, cached.to_str().unwrap(), "copy", db::now_ms()+60000, db::now_ms()+60000).await.unwrap();
        db::set_precache_status(&pool, "item", "ready", 4, None).await.unwrap();
        let lease = service.acquire_ready("item", &identity).await.unwrap().unwrap();
        std::fs::write(&source, b"new bytes").unwrap();
        assert!(!service.revalidate_ready("item", &identity.path).await.unwrap());
        assert_eq!(std::fs::read(lease.path()).unwrap(), b"old!");
        assert!(service.acquire_ready("item", &identity).await.unwrap().is_some());
        assert_eq!(db::precache_entry(&pool, "item").await.unwrap().unwrap().error.as_deref(), Some("refresh pending: stale source"));
        drop(lease);
        assert!(!service.revalidate_ready("item", &identity.path).await.unwrap());
        service.run_jobs().await.unwrap();
        assert_eq!(std::fs::read(&cached).unwrap(), b"old!");
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.source_size), ("ready", 4));
        assert_eq!(row.error.as_deref(), Some("refresh pending: stale source"));
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_unavailable_and_path_change_are_safe() {
        let root = std::env::temp_dir().join(format!("refresh-missing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let source = root.join("missing"); let cached = config.precache_dir.join("item");
        std::fs::write(&cached, b"old!").unwrap();
        let identity = SourceIdentity::new(source.to_str().unwrap(), 4, 1);
        db::upsert_precache(&pool, "item", &identity.path, 4, 1, cached.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "item", "ready", 4, None).await.unwrap();
        assert!(!service.revalidate_ready("item", &identity.path).await.unwrap());
        assert!(service.acquire_ready("item", &identity).await.unwrap().is_some());
        assert_eq!(db::precache_entry(&pool, "item").await.unwrap().unwrap().error.as_deref(), Some("refresh deferred: source unavailable"));
        let other = root.join("other");
        assert!(!service.revalidate_ready("item", other.to_str().unwrap()).await.unwrap());
        assert!(service.acquire_ready("item", &identity).await.unwrap().is_none());
        assert!(cached.exists());
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_scheduled_scan_discovers_change() {
        let root = std::env::temp_dir().join(format!("refresh-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let channel = db::create_channel_shell(&pool, "998", "refresh").await.unwrap();
        let source = root.join("source.mp4"); std::fs::write(&source, b"old!").unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let identity = SourceIdentity::new(source.to_str().unwrap(), 4, metadata_mtime(&meta));
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',8000,0)")
            .bind("item").bind(&identity.path).bind("item").execute(&pool).await.unwrap();
        let future = db::now_ms()+60000;
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,'item','item',?,?,0,8000,'program')")
            .bind(&channel.id).bind(future).bind(future+8000).execute(&pool).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
        let service = Arc::new(Precache::new(&config, pool.clone()).isolated_from_host_readers());
        let cached = config.precache_dir.join("item.mp4"); std::fs::write(&cached, b"old!").unwrap();
        db::upsert_precache(&pool, "item", &identity.path, 4, identity.mtime, cached.to_str().unwrap(), "copy", future, future).await.unwrap();
        db::set_precache_status(&pool, "item", "ready", 4, None).await.unwrap();
        let lease = service.acquire_ready("item", &identity).await.unwrap().unwrap();
        std::fs::write(&source, b"new bytes").unwrap();
        service.tick().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "item").await.unwrap().unwrap().error.as_deref(), Some("refresh pending: stale source"));
        assert_eq!(std::fs::read(lease.path()).unwrap(), b"old!");
        drop(lease); service.tick().await.unwrap();
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.source_size), ("ready", 4));
        assert_eq!(std::fs::read(&cached).unwrap(), b"old!");
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_failure_states_are_non_ready() {
        let root = std::env::temp_dir().join(format!("refresh-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        config.precache_max_gb = 0;
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let source = root.join("missing"); let cached = config.precache_dir.join("item");
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), 4, 1, cached.to_str().unwrap(), "copy", db::now_ms()+60000, db::now_ms()).await.unwrap();
        service.run_jobs().await.unwrap();
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!(row.status, "queued"); assert_eq!(row.error.as_deref(), Some("cache reservation unavailable"));
        config.precache_max_gb = 1;
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.run_jobs().await.unwrap();
        let row = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!(row.status, "failed"); assert!(row.error.is_some()); assert!(!cached.exists());
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_preserves_unrelated_part_files() {
        let root = std::env::temp_dir().join(format!("precache-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let cached = config.precache_dir.join("item.mp4");
        db::upsert_precache(&pool, "item", "/source", 4, 1,
            cached.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let partial = config.precache_dir.join("item.mp4.part");
        let unrelated = config.precache_dir.join("advertised.ts.part");
        std::fs::write(&partial, b"partial").unwrap();
        std::fs::write(&unrelated, b"unrelated").unwrap();
        service.cleanup_partials().await.unwrap();
        assert!(!partial.exists());
        assert!(unrelated.exists());
        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn startup_cleanup_preserves_ready_accounting() {
        let root = std::env::temp_dir().join(format!("precache-ready-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let dest = config.precache_dir.join("ready");
        std::fs::write(&dest, b"data").unwrap();
        db::upsert_precache(&pool, "ready", "/source", 4, 1, dest.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "ready", "ready", 4, None).await.unwrap();
        service.cleanup_partials().await.unwrap();
        let entry = db::precache_entry(&pool, "ready").await.unwrap().unwrap();
        assert_eq!(entry.bytes, 4);
        assert_eq!(std::fs::read(dest).unwrap(), b"data");
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn slow_copy_publishes_complete_file_and_rejects_changed_source() {
        let root = std::env::temp_dir().join(format!("awesometv-precache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.copy_bytes_per_sec = 1_000_000;
        let source = root.join("source.mp4");
        let bytes = vec![42u8; 1_100_000];
        tokio::fs::write(&source, &bytes).await.unwrap();
        let meta = tokio::fs::metadata(&source).await.unwrap();
        let destination = root.join("cache/result.mp4");
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), bytes.len() as i64,
            metadata_mtime(&meta), destination.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let entry = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        let started = Instant::now();
        let reservation = service.index.reserve("item", SourceIdentity::new(source.to_str().unwrap(), bytes.len() as i64, metadata_mtime(&meta)), destination.clone(), bytes.len() as i64).unwrap();
        let copied = service.process(&entry).await.unwrap();
        assert_eq!(copied, ProcessOutcome::Complete(bytes.len() as i64));
        reservation.publish(bytes.len() as i64).unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
        tokio::fs::remove_file(&source).await.unwrap();
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), bytes);
        tokio::fs::write(&source, b"changed").await.unwrap();
        assert!(service.process(&entry).await.is_err());
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), bytes);
        // Cancellation must never publish the new destination.
        let cancelled = root.join("cache/cancelled.mp4");
        let meta = tokio::fs::metadata(&source).await.unwrap();
        db::upsert_precache(&pool, "cancelled", source.to_str().unwrap(), meta.len() as i64,
            metadata_mtime(&meta), cancelled.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let cancelled_entry = db::precache_entry(&pool, "cancelled").await.unwrap().unwrap();
        service.shutdown.store(true, Ordering::Relaxed);
        assert!(service.process(&cancelled_entry).await.is_err());
        assert!(!cancelled.exists());
        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn copy_quantum_yields_and_resumes_with_reservation() {
        let root = std::env::temp_dir().join(format!("precache-quantum-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.free_space_floor = 0; service.copy_quantum_bytes = 1024; service.copy_bytes_per_sec = 100_000_000;
        let source = root.join("source");
        let bytes: Vec<u8> = (0..1500).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let destination = config.precache_dir.join("result");
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), bytes.len() as i64,
            metadata_mtime(&meta), destination.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let entry = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        service.run_jobs().await.unwrap();
        let queued = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        assert_eq!(queued.status, "queued"); assert_eq!(queued.generation, entry.generation);
        assert_eq!(queued.bytes, 1024);
        let partial = format!("{}.part", destination.display());
        assert_eq!(std::fs::read(&partial).unwrap(), bytes[..1024]);
        assert!(std::path::Path::new(&format!("{partial}.resume")).exists());
        assert_eq!(service.index.0.state.lock().unwrap().copying.get("item").unwrap().0, bytes.len() as i64);
        service.run_jobs().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "item").await.unwrap().unwrap().status, "ready");
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        let tiny = root.join("tiny"); std::fs::write(&tiny, b"tiny").unwrap();
        let tiny_meta = std::fs::metadata(&tiny).unwrap();
        let tiny_dest = config.precache_dir.join("tiny-result");
        db::upsert_precache(&pool, "tiny", tiny.to_str().unwrap(), 4, metadata_mtime(&tiny_meta), tiny_dest.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let tiny_entry = db::precache_entry(&pool, "tiny").await.unwrap().unwrap();
        assert_eq!(service.copy_source(&tiny_entry, &format!("{}.part", tiny_dest.display())).await.unwrap(), CopyOutcome::Complete);
        pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn prune_preserves_checkpoint_and_charge_across_restart() {
        let root = std::env::temp_dir().join(format!("prune-checkpoint-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let dest = config.precache_dir.join("saved");
        db::upsert_precache(&pool, "saved", "/source", 100, 1, dest.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let entry = db::precache_entry(&pool, "saved").await.unwrap().unwrap();
        let partial = PathBuf::from(format!("{}.part", dest.display()));
        std::fs::write(&partial, b"data").unwrap();
        resume::save(&PathBuf::from(format!("{}.resume", partial.display())), &partial, "saved", &entry.generation, &resume::Identity { path: "/source".into(), size: 100, mtime: 1 }, 4).unwrap();
        db::upsert_precache(&pool, "discard", "/other", 100, 1, config.precache_dir.join("discard").to_str().unwrap(), "copy", 2, 2).await.unwrap();
        let channel = db::create_channel_shell(&pool, "991", "cap").await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('discard','/other','discard','movie',1000,0)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,'discard','discard',0,1000,0,1000,'program')").bind(&channel.id).execute(&pool).await.unwrap();
        let (stale, trimmed) = service.prune_queue(100, 0, &HashSet::new()).await.unwrap();
        assert_eq!((stale, trimmed), (0, 1));
        assert!(db::precache_entry(&pool, "saved").await.unwrap().is_some());
        assert!(db::precache_entry(&pool, "discard").await.unwrap().is_none());
        drop(service);
        let restarted = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        restarted.initialize_index().await.unwrap(); restarted.cleanup_partials().await.unwrap();
        assert_eq!(restarted.index.0.state.lock().unwrap().copying["saved"].0, 100);
        assert_eq!(std::fs::read(&partial).unwrap(), b"data");
        pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rolling_snapshot_excludes_audio_but_whole_copy_remains_eligible() {
        let root = std::env::temp_dir().join(format!("rolling-audio-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.precache_dir = root.join("cache");
        config.precache_rolling_chunks = false;
        let off = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        assert!(off.shared_chunk_index().await.unwrap().is_none());
        for (i, (id, kind, video, audio)) in [("song", "music", None, Some("aac")), ("audio", "movie", None, Some("aac")), ("video", "movie", Some("h264"), Some("aac"))].iter().enumerate() {
            let path = root.join(id); std::fs::write(&path, b"data").unwrap();
            let channel = db::create_channel_shell(&pool, &(980+i).to_string(), id).await.unwrap();
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,video_codec,audio_codec,created_at) VALUES (?,?,?,?,10000,?,?,0)")
                .bind(id).bind(path.to_str().unwrap()).bind(id).bind(kind).bind(video).bind(audio).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,?,0,10000,0,10000,'program')")
                .bind(&channel.id).bind(id).bind(id).execute(&pool).await.unwrap();
        }
        let upcoming = db::upcoming_media(&pool, 1, 10000).await.unwrap();
        assert_eq!(upcoming.len(), 3);
        assert!(mandatory_targets(&upcoming, 1).iter().flatten().any(|id| id == "song"));
        config.precache_rolling_chunks = true;
        let on = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let snapshot = on.rolling_snapshot(1).await.unwrap();
        let requests = chunks::requests(&chunks::targets(&snapshot, 1));
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source.path, root.join("video").to_str().unwrap());
        on.publish_verified_sources(&snapshot);
        std::fs::remove_file(root.join("video")).unwrap();
        let offline = on.rolling_snapshot(1).await.unwrap();
        assert_eq!(offline.len(), 1, "a temporarily unavailable source must retain its protected window");
        assert_eq!(offline[0].source, snapshot[0].source);
        pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rolling_manager_background_publishes_three_channels_and_copies_music() {
        let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).try_init();
        let root = std::env::temp_dir().join(format!("rolling-manager-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let video = root.join("template.mkv");
        assert!(std::process::Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=size=64x64:rate=5:duration=100", "-f", "lavfi", "-i", "sine=frequency=440:duration=100", "-c:v", "libx264", "-threads", "1", "-g", "10", "-bf", "0", "-c:a", "aac", "-y"]).arg(&video).status().unwrap().success());
        let music = root.join("music.m4a");
        assert!(std::process::Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi", "-i", "sine=frequency=880:duration=90", "-c:a", "aac", "-y"]).arg(&music).status().unwrap().success());
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        config.precache_enabled = true; config.precache_rolling_chunks = true; config.precache_max_gb = 2;
        let now = db::now_ms();
        let mut identities = Vec::new();
        for i in 0..4 {
            let id = format!("item-{i}");
            let path = if i == 3 { music.clone() } else { let p = root.join(format!("{id}.mkv")); std::fs::copy(&video, &p).unwrap(); p };
            let meta = std::fs::metadata(&path).unwrap();
            if i < 3 { identities.push(SourceIdentity::new(path.to_string_lossy(), meta.len() as i64, metadata_mtime(&meta))); }
            let channel = db::create_channel_shell(&pool, &format!("{}", 910 + i), &id).await.unwrap();
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,video_codec,audio_codec,created_at) VALUES (?,?,?,?,100000,?,?,0)")
                .bind(&id).bind(path.to_str().unwrap()).bind(&id).bind(if i == 3 { "music" } else { "movie" }).bind(if i == 3 { None } else { Some("h264") }).bind("aac").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,?,?,?,2000,92000,'program')")
                .bind(&channel.id).bind(&id).bind(&id).bind(now).bind(now + 90_000).execute(&pool).await.unwrap();
        }
        let service = Arc::new(Precache::new(&config, pool.clone()).isolated_from_host_readers());
        service.start();
        let index = service.shared_chunk_index().await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(75), async {
            loop {
                let offset = 2000 + (db::now_ms() - now).max(0);
                let videos_ready = identities.iter().all(|id| index.acquire(id, offset).is_some());
                let music_ready = db::precache_entry(&pool, "item-3").await.unwrap().is_some_and(|entry| entry.status == "ready");
                if videos_ready && music_ready { break; }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }).await.expect("background manager must publish all three chunks and copy music");
        service.shutdown.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(25), async { while Arc::strong_count(&service) > 1 { tokio::time::sleep(Duration::from_millis(100)).await; } }).await.expect("manager loops must stop");
        drop(index); drop(service); pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rolling_coordination_preserves_video_and_runs_audio() {
        let root = std::env::temp_dir().join(format!("rolling-coordination-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        config.precache_rolling_chunks = true;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.free_space_floor = 0; service.copy_bytes_per_sec = 100_000_000;
        let now = db::now_ms();
        for (i, id) in ["video", "audio", "newvideo", "ready"].iter().enumerate() {
            let source = root.join(id); std::fs::write(&source, b"source data").unwrap();
            let channel = db::create_channel_shell(&pool, &format!("{}", 700+i), id).await.unwrap();
            let kind = if *id == "audio" { "music" } else { "movie" };
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,?,60000,0)")
                .bind(id).bind(source.to_str().unwrap()).bind(id).bind(kind).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,?,?,?,0,60000,'program')")
                .bind(&channel.id).bind(id).bind(id).bind(now-1000).bind(now+59000).execute(&pool).await.unwrap();
            if *id != "newvideo" {
                let meta = std::fs::metadata(&source).unwrap();
                let dest = config.precache_dir.join(id);
                db::upsert_precache(&pool, id, source.to_str().unwrap(), meta.len() as i64, metadata_mtime(&meta), dest.to_str().unwrap(), "copy", i as i64, 1).await.unwrap();
            }
        }
        let video = db::precache_entry(&pool, "video").await.unwrap().unwrap();
        let part = config.precache_dir.join("video.part"); std::fs::write(&part, b"partial").unwrap();
        let ready_path = config.precache_dir.join("ready"); std::fs::write(&ready_path, b"source data").unwrap();
        db::set_precache_status(&pool, "ready", "ready", 11, None).await.unwrap();
        service.run_jobs().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "audio").await.unwrap().unwrap().status, "ready");
        let after = db::precache_entry(&pool, "video").await.unwrap().unwrap();
        assert_eq!((after.status.as_str(), &after.generation, after.bytes), ("queued", &video.generation, video.bytes));
        assert_eq!(std::fs::read(&part).unwrap(), b"partial");
        let ready = db::precache_entry(&pool, "ready").await.unwrap().unwrap();
        let identity = SourceIdentity::new(&ready.source_path, ready.source_size, ready.source_mtime);
        assert!(service.acquire_ready("ready", &identity).await.unwrap().is_some());
        let service = Arc::new(service);
        for _ in 0..2 { service.tick_inner().await.unwrap(); }
        assert!(db::precache_entry(&pool, "newvideo").await.unwrap().is_none());
        assert_eq!(db::precache_entry(&pool, "video").await.unwrap().unwrap().generation, video.generation);
        assert_eq!(std::fs::read(&part).unwrap(), b"partial");
        service.shutdown.store(true, Ordering::Relaxed);
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mandatory_copy_rotation_survives_restart() {
        let root = std::env::temp_dir().join(format!("precache-rotation-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
        config.precache_rolling_chunks = false;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.free_space_floor = 0; service.copy_quantum_bytes = 1024; service.copy_bytes_per_sec = 100_000_000;
        let now = db::now_ms();
        let giant: Vec<u8> = (0..1500).map(|i| (i % 251) as u8).collect();
        for (i, id) in ["giant", "small-a", "small-b"].iter().enumerate() {
            let bytes = if i == 0 { giant.clone() } else { vec![i as u8; 12] };
            let source = root.join(format!("source-{id}")); std::fs::write(&source, &bytes).unwrap();
            let meta = std::fs::metadata(&source).unwrap();
            let channel = db::create_channel_shell(&pool, &format!("{}", 900+i), id).await.unwrap();
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',60000,0)")
                .bind(id).bind(source.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,?,?, ?,0,60000,'program')")
                .bind(&channel.id).bind(id).bind(id).bind(now-1000).bind(now+59000).execute(&pool).await.unwrap();
            let destination = config.precache_dir.join(id);
            db::upsert_precache(&pool, id, source.to_str().unwrap(), bytes.len() as i64, metadata_mtime(&meta), destination.to_str().unwrap(), "copy", i as i64, 1).await.unwrap();
            if i == 0 {
                service.run_jobs().await.unwrap();
                assert_eq!(db::precache_entry(&pool, id).await.unwrap().unwrap().status, "queued");
                assert_eq!(service.index.0.state.lock().unwrap().copying[*id].0, giant.len() as i64);
            }
        }
        drop(service);
        let mut restarted = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        restarted.free_space_floor = 0; restarted.copy_quantum_bytes = 1024; restarted.copy_bytes_per_sec = 100_000_000;
        restarted.initialize_index().await.unwrap(); restarted.cleanup_partials().await.unwrap();
        assert_eq!(restarted.index.0.state.lock().unwrap().copying["giant"].0, giant.len() as i64);
        restarted.run_jobs().await.unwrap();
        let order: Vec<(String, i64)> = sqlx::query_as("SELECT media_id, last_attempt_order FROM precache_entries ORDER BY last_attempt_order").fetch_all(&pool).await.unwrap();
        assert_eq!(order.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(), vec!["small-a", "small-b", "giant"]);
        for id in ["giant", "small-a", "small-b"] { assert_eq!(db::precache_entry(&pool, id).await.unwrap().unwrap().status, "ready"); }
        assert_eq!(std::fs::read(config.precache_dir.join("giant")).unwrap(), giant);
        pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn copy_job_resumes_checkpoint_after_cancellation() {
        let root = std::env::temp_dir().join(format!("precache-resume-job-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.copy_bytes_per_sec = 20_000_000;
        let source = root.join("source");
        let bytes: Vec<u8> = (0..5_000_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let destination = config.precache_dir.join("result");
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), bytes.len() as i64,
            metadata_mtime(&meta), destination.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let entry = db::precache_entry(&pool, "item").await.unwrap().unwrap();
        let partial = format!("{}.part", destination.display());
        let shutdown = &service.shutdown;
        let manifest = format!("{partial}.resume");
        let interrupt = async {
            for _ in 0..200 {
                if std::fs::metadata(&partial).is_ok_and(|m| m.len() > 0) && std::path::Path::new(&manifest).exists() {
                    shutdown.store(true, Ordering::Relaxed); return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("checkpoint not written");
        };
        let identity = SourceIdentity::new(source.to_str().unwrap(), bytes.len() as i64, metadata_mtime(&meta));
        let reservation = service.index.reserve("item", identity.clone(), destination.clone(), bytes.len() as i64).unwrap();
        let (first, ()) = tokio::join!(service.copy_source(&entry, &partial), interrupt);
        assert!(first.is_err());
        reservation.release_without_deleting_partial();
        let prefix = std::fs::metadata(&partial).unwrap().len();
        assert!(prefix > 0 && prefix < bytes.len() as u64);
        service.shutdown.store(false, Ordering::Relaxed);
        let reservation = service.index.reserve("item", identity, destination.clone(), bytes.len() as i64).unwrap();
        service.copy_source(&entry, &partial).await.unwrap();
        reservation.publish(bytes.len() as i64).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn startup_rejects_stale_generation_and_invalid_checkpoint() {
        let root = std::env::temp_dir().join(format!("precache-invalid-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        for id in ["stale", "invalid"] {
            let dest = config.precache_dir.join(id);
            db::upsert_precache(&pool, id, "/never-stat/source", 10, 1, dest.to_str().unwrap(), "copy", 1, 1).await.unwrap();
            let entry = db::precache_entry(&pool, id).await.unwrap().unwrap();
            let partial = PathBuf::from(format!("{}.part", dest.display()));
            let manifest = PathBuf::from(format!("{}.resume", partial.display()));
            std::fs::write(&partial, b"12345").unwrap();
            let identity = resume::Identity { path: entry.source_path, size: 10, mtime: 1 };
            resume::save(&manifest, &partial, id, &entry.generation, &identity, 5).unwrap();
            if id == "stale" {
                db::upsert_precache(&pool, id, "/never-stat/source", 10, 2, dest.to_str().unwrap(), "copy", 1, 1).await.unwrap();
            } else { std::fs::write(&manifest, b"invalid").unwrap(); }
        }
        service.initialize_index().await.unwrap();
        service.cleanup_partials().await.unwrap();
        for id in ["stale", "invalid"] {
            assert!(!config.precache_dir.join(format!("{id}.part")).exists());
            assert!(!service.index.0.state.lock().unwrap().copying.contains_key(id));
        }
        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn startup_retains_interrupted_worker_checkpoint() {
        let root = std::env::temp_dir().join(format!("precache-restart-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        config.precache_enabled = true;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.free_space_floor = 0;
        service.copy_bytes_per_sec = 20_000_000;
        let source = root.join("source");
        let bytes = vec![42u8; 5_000_000];
        std::fs::write(&source, &bytes).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let destination = config.precache_dir.join("item");
        let partial = PathBuf::from(format!("{}.part", destination.display()));
        db::upsert_precache(&pool, "item", source.to_str().unwrap(), bytes.len() as i64,
            metadata_mtime(&metadata), destination.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        let interrupt = async {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if std::fs::metadata(&partial).is_ok_and(|m| m.len() > 0)
                        && PathBuf::from(format!("{}.resume", partial.display())).exists() {
                        service.shutdown.store(true, Ordering::Relaxed);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.unwrap();
        };
        let (result, ()) = tokio::join!(service.run_jobs(), interrupt);
        result.unwrap();
        let prefix = std::fs::metadata(&partial).unwrap().len();
        assert!(prefix > 0 && prefix < bytes.len() as u64);
        drop(service);
        let restarted = Arc::new(Precache::new(&config, pool.clone()).isolated_from_host_readers());
        restarted.shutdown.store(true, Ordering::Relaxed);
        restarted.start();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !partial.exists() { break; }
                if restarted.index.0.state.lock().unwrap().copying.contains_key("item") { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert_eq!(std::fs::metadata(&partial).unwrap().len(), prefix);
        assert_eq!(restarted.index.0.state.lock().unwrap().copying["item"].0, bytes.len() as i64);
        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

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

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[tokio::test]
    async fn admission_reserves_mandatory_capacity_and_releases_failed_claim() {
        let root = std::env::temp_dir().join(format!("admission-accounting-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let mut service = Precache::new(&config, pool.clone());
        service.max_bytes = 7; service.index = CacheIndex::new(7, false); service.free_space_floor = 0;
        let now = db::now_ms();
        sqlx::query("INSERT INTO channels (id,number,name,start_ms,created_at) VALUES ('c','1','c',0,0)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,size_bytes,created_at) VALUES ('required','source','required',60000,4,0)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms) VALUES ('c',0,'required','required',?,?)").bind(now-1000).bind(now+59000).execute(&pool).await.unwrap();
        for id in ["required", "optional"] {
            let path = config.precache_dir.join(id);
            db::upsert_precache(&pool, id, id, 4, 1, path.to_str().unwrap(), "copy", now, now).await.unwrap();
        }
        service.initialize_index().await.unwrap();
        let optional = db::precache_entry(&pool, "optional").await.unwrap().unwrap();
        assert!(service.admit_and_claim(&optional).await.unwrap().is_none());
        let required = db::precache_entry(&pool, "required").await.unwrap().unwrap();
        let reservation = service.admit_and_claim(&required).await.unwrap().expect("mandatory admission");
        drop(reservation);
        db::set_precache_status(&pool, "required", "queued", 0, None).await.unwrap();
        let stale = db::precache_entry(&pool, "required").await.unwrap().unwrap();
        db::set_precache_status(&pool, "required", "failed", 0, None).await.unwrap();
        let foreign_partial = config.precache_dir.join("required.part");
        std::fs::write(&foreign_partial, b"keep").unwrap();
        assert!(service.admit_and_claim(&stale).await.unwrap().is_none());
        assert_eq!(std::fs::read(&foreign_partial).unwrap(), b"keep");
        assert!(service.index.0.state.lock().unwrap().copying.is_empty());
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn queued_copy_reclaims_unleased_capacity_at_admission() {
        let root = std::env::temp_dir().join(format!("queued-admission-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.max_bytes = 4; service.index = CacheIndex::new(4, false); service.free_space_floor = 0;
        let old = config.precache_dir.join("old"); std::fs::write(&old, b"old!").unwrap();
        db::upsert_precache(&pool, "old", "old-source", 4, 1, old.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "old", "ready", 4, None).await.unwrap();
        let source = root.join("source"); std::fs::write(&source, b"new!").unwrap();
        let meta = std::fs::metadata(&source).unwrap(); let dest = config.precache_dir.join("new");
        db::upsert_precache(&pool, "new", source.to_str().unwrap(), 4, metadata_mtime(&meta), dest.to_str().unwrap(), "copy", db::now_ms()+60000, 1).await.unwrap();
        let lease = service.acquire_ready("old", &SourceIdentity::new("old-source", 4, 1)).await.unwrap().unwrap();
        service.run_jobs().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "new").await.unwrap().unwrap().status, "queued");
        assert_eq!(std::fs::read(lease.path()).unwrap(), b"old!");
        drop(lease);
        service.run_jobs().await.unwrap();
        assert_eq!(db::precache_entry(&pool, "new").await.unwrap().unwrap().status, "ready");
        assert_eq!(std::fs::read(&dest).unwrap(), b"new!");
        assert!(!old.exists());
        pool.close().await; let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistent_eviction_failures_and_restart() {
        let root = std::env::temp_dir().join(format!("eviction-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        config.precache_max_gb = 1;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        service.max_bytes = 4;
        let path = config.precache_dir.join("one");
        std::fs::write(&path, b"data").unwrap();
        db::upsert_precache(&pool, "one", "source", 4, 1, path.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "one", "ready", 4, None).await.unwrap();
        let identity = SourceIdentity::new("source", 4, 1);
        let lease = service.acquire_ready("one", &identity).await.unwrap().unwrap();
        assert!(!service.budget_allows(4, &HashSet::new()).await.unwrap());
        assert!(path.exists());
        drop(lease);
        assert!(service.budget_allows(4, &HashSet::new()).await.unwrap());
        assert!(!path.exists());
        assert_ne!(db::precache_entry(&pool, "one").await.unwrap().unwrap().status, "ready");
        assert!(service.acquire_ready("one", &identity).await.unwrap().is_none());

        // A crash after invalidation but before unlink is completed on restart.
        std::fs::write(&path, b"data").unwrap();
        let recovered = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        recovered.initialize_index().await.unwrap();
        assert!(!path.exists());
        // A stale ready row from a previous crash is invalidated on initialization.
        db::set_precache_status(&pool, "one", "ready", 4, None).await.unwrap();
        let restarted = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        assert!(restarted.acquire_ready("one", &identity).await.unwrap().is_none());
        assert_ne!(db::precache_entry(&pool, "one").await.unwrap().unwrap().status, "ready");

        std::fs::write(&path, b"data").unwrap();
        db::set_precache_status(&pool, "one", "ready", 4, None).await.unwrap();
        let mut failing = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        failing.max_bytes = 4;
        failing.initialize_index().await.unwrap();
        sqlx::query("CREATE TRIGGER reject_eviction BEFORE UPDATE ON precache_entries BEGIN SELECT RAISE(FAIL, 'injected'); END").execute(&pool).await.unwrap();
        assert!(failing.budget_allows(4, &HashSet::new()).await.is_err());
        assert!(path.exists());
        assert_eq!(db::precache_entry(&pool, "one").await.unwrap().unwrap().status, "ready");
        sqlx::query("DROP TRIGGER reject_eviction").execute(&pool).await.unwrap();

        let mut unlink = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        unlink.max_bytes = 4;
        unlink.initialize_index().await.unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(unlink.budget_allows(4, &HashSet::new()).await.is_err());
        assert_ne!(db::precache_entry(&pool, "one").await.unwrap().unwrap().status, "ready");
        assert!(unlink.acquire_ready("one", &identity).await.unwrap().is_none());
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn acquire_cannot_race_persistent_eviction() {
        let root = std::env::temp_dir().join(format!("eviction-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone(); config.precache_dir = root.join("cache"); config.precache_max_gb = 1;
        let mut service = Precache::new(&config, pool.clone()).isolated_from_host_readers(); service.max_bytes = 4;
        let path = config.precache_dir.join("one");
        std::fs::write(&path, b"data").unwrap();
        db::upsert_precache(&pool, "one", "source", 4, 1, path.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "one", "ready", 4, None).await.unwrap();
        service.initialize_index().await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await.unwrap();
        let service = Arc::new(service);
        let worker = service.clone();
        let task = tokio::spawn(async move { worker.budget_allows(4, &HashSet::new()).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if service.index.0.state.lock().unwrap().evicting.contains_key("one") { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert!(path.exists());
        let reader = service.clone();
        let acquisition = tokio::spawn(async move { reader.acquire_ready("one", &SourceIdentity::new("source", 4, 1)).await.unwrap() });
        sqlx::query("COMMIT").execute(&mut *connection).await.unwrap();
        assert!(task.await.unwrap().unwrap());
        assert!(acquisition.await.unwrap().is_none());
        assert!(!path.exists());
        assert_ne!(db::precache_entry(&pool, "one").await.unwrap().unwrap().status, "ready");
        drop(connection); pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leases_block_eviction_until_last_clone_drops() {
        let root = std::env::temp_dir().join(format!("lease-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let index = CacheIndex::new(4, true);
        let path = root.join("one");
        let identity = SourceIdentity::new("source", 4, 1);
        let reservation = index.reserve("one", identity.clone(), path.clone(), 4).unwrap();
        std::fs::write(reservation.partial_path(), b"data").unwrap();
        reservation.publish(4).unwrap();
        let lease = index.acquire_ready("one", &identity).unwrap();
        let clone = lease.clone();
        drop(lease);
        let other = index.clone();
        let other_path = root.join("two");
        assert!(std::thread::spawn(move || other.reserve("two", SourceIdentity::new("other", 4, 1), other_path, 4).is_none()).join().unwrap());
        assert_eq!(std::fs::read(clone.path()).unwrap(), b"data");
        drop(clone);
        assert!(index.reserve("two", SourceIdentity::new("other", 4, 1), root.join("two"), 4).is_some());
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn startup_ready_entries_are_pinned_and_not_evicted() {
        let root = std::env::temp_dir().join(format!("startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone()).isolated_from_host_readers();
        let path = config.precache_dir.join("one");
        std::fs::write(&path, b"data").unwrap();
        db::upsert_precache(&pool, "one", "source", 4, 1, path.to_str().unwrap(), "copy", 1, 1).await.unwrap();
        db::set_precache_status(&pool, "one", "ready", 4, None).await.unwrap();
        let identity = SourceIdentity::new("source", 4, 1);
        let lease = service.acquire_ready("one", &identity).await.unwrap().unwrap();
        assert!(service.remove("one").await.is_err());
        assert_eq!(std::fs::read(lease.path()).unwrap(), b"data");
        drop(lease);
        assert!(service.index.reserve("huge", identity, root.join("huge"), service.max_bytes).is_none());
        assert!(path.exists());
        service.remove("one").await.unwrap();
        assert!(!path.exists());
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_acquire_and_eviction_are_atomic() {
        for _ in 0..20 {
            let root = std::env::temp_dir().join(format!("race-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let index = CacheIndex::new(4, true);
            let source = SourceIdentity::new("source", 4, 1);
            let initial = index.reserve("one", source.clone(), root.join("one"), 4).unwrap();
            std::fs::write(initial.partial_path(), b"data").unwrap();
            initial.publish(4).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let contender = index.clone();
            let barrier2 = barrier.clone();
            let target = root.join("two");
            let thread = std::thread::spawn(move || {
                barrier2.wait();
                contender.reserve("two", SourceIdentity::new("other", 4, 1), target, 4)
            });
            barrier.wait();
            let lease = index.acquire_ready("one", &source);
            let reservation = thread.join().unwrap();
            assert_ne!(lease.is_some(), reservation.is_some());
            if let Some(lease) = lease { assert_eq!(std::fs::read(lease.path()).unwrap(), b"data"); }
            drop(reservation);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn failed_publication_preserves_pinned_file_and_releases_capacity() {
        let root = std::env::temp_dir().join(format!("publish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let index = CacheIndex::new(8, false);
        let source = SourceIdentity::new("source", 4, 1);
        let first = index.reserve("one", source.clone(), root.join("one"), 4).unwrap();
        std::fs::write(first.partial_path(), b"data").unwrap();
        first.publish(4).unwrap();
        assert!(index.acquire_ready("one", &SourceIdentity::new("source", 4, 2)).is_none());
        let lease = index.acquire_ready("one", &source).unwrap();
        let second = index.reserve("two", source.clone(), root.join("two"), 4).unwrap();
        std::fs::write(second.partial_path(), b"data").unwrap();
        std::fs::write(root.join("two"), b"existing").unwrap();
        assert!(second.publish(4).is_err());
        assert_eq!(std::fs::read(lease.path()).unwrap(), b"data");
        assert!(index.reserve("three", source, root.join("three"), 4).is_some());
        drop(lease);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_task_releases_reservation_and_partial() {
        let root = std::env::temp_dir().join(format!("cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let index = CacheIndex::new(4, false);
        let source = SourceIdentity::new("source", 4, 1);
        let reservation = index.reserve("one", source.clone(), root.join("one"), 4).unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            std::fs::write(reservation.partial_path(), b"data").unwrap();
            let _ = started.send(());
            std::future::pending::<()>().await;
            drop(reservation);
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!root.join("one.part").exists());
        assert!(index.reserve("two", source, root.join("two"), 4).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reservations_are_bounded_deduplicated_and_cleaned_on_drop() {
        let root = std::env::temp_dir().join(format!("reserve-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let index = CacheIndex::new(5, false);
        let identity = SourceIdentity::new("source", 4, 1);
        let path = root.join("one");
        let reservation = index.reserve("one", identity.clone(), path.clone(), 4).unwrap();
        std::fs::write(reservation.partial_path(), b"data").unwrap();
        let unrelated = root.join("unrelated.part");
        std::fs::write(&unrelated, b"keep").unwrap();
        assert!(index.reserve("one", identity.clone(), path.clone(), 4).is_none());
        assert!(index.reserve("two", identity.clone(), root.join("two"), 2).is_none());
        drop(reservation);
        assert!(!root.join("one.part").exists());
        assert!(unrelated.exists());
        assert!(index.reserve("two", identity, root.join("two"), 5).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}

// Pure admission inputs: capture index and copy progress before measuring free space.
#[derive(Clone, Debug)]
pub(crate) struct MandatoryDemand {
    pub media_id: String,
    pub priority: usize,
    pub identity: Option<SourceIdentity>,
    pub bytes: Option<i128>,
}
#[derive(Clone, Debug)]
pub(crate) enum AccountedCache {
    Ready { media_id: String, identity: SourceIdentity, bytes: i128, valid: bool },
    Copying { media_id: String, identity: SourceIdentity, reserved: i128, completed: i128 },
    Evicting { bytes: i128 },
    Extra { reserved: i128, remaining: i128 },
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AdmissionAccounting {
    pub occupied: i128,
    pub pending: i128,
    pub running_remaining: i128,
    pub optional: i128,
    pub unknown: bool,
    pub projected: i128,
    pub additional: i128,
    pub budget_deficit: i128,
    pub free_deficit: i128,
    pub optional_allowed: bool,
}
impl AdmissionAccounting {
    pub(crate) fn calculate(demands: &[MandatoryDemand], cache: &[AccountedCache], optional: Option<&MandatoryDemand>, budget: i128, free: i128, floor: i128) -> Result<Self> {
        fn add(a: i128, b: i128) -> Result<i128> { anyhow::ensure!(b >= 0, "negative byte count"); a.checked_add(b).context("byte accounting overflow") }
        anyhow::ensure!(budget >= 0 && free >= 0 && floor >= 0, "negative capacity");
        let mut occupied = 0; let mut remaining = 0;
        for item in cache {
            match item {
                AccountedCache::Ready { bytes, .. } | AccountedCache::Evicting { bytes } => occupied = add(occupied, *bytes)?,
                AccountedCache::Extra { reserved, remaining: left } => {
                    anyhow::ensure!(*left >= 0 && left <= reserved, "invalid extra progress");
                    occupied = add(occupied, *reserved)?; remaining = add(remaining, *left)?;
                }
                AccountedCache::Copying { reserved, completed, .. } => {
                    anyhow::ensure!(*completed >= 0 && completed <= reserved, "invalid copy progress");
                    occupied = add(occupied, *reserved)?;
                    remaining = add(remaining, reserved - completed)?;
                }
            }
        }
        let mut unique: HashMap<&str, &MandatoryDemand> = HashMap::new();
        for demand in demands {
            unique.entry(&demand.media_id).and_modify(|old| { if demand.priority < old.priority { *old = demand; } }).or_insert(demand);
        }
        let satisfied = |d: &MandatoryDemand| cache.iter().any(|item| match item {
            AccountedCache::Ready { media_id, identity, valid, .. } => *valid && media_id == &d.media_id && d.identity.as_ref() == Some(identity),
            AccountedCache::Copying { media_id, identity, .. } => media_id == &d.media_id && d.identity.as_ref() == Some(identity),
            _ => false,
        });
        let mut pending = 0; let mut unknown = false;
        for demand in unique.values() {
            if !satisfied(demand) { match demand.bytes { Some(n) => pending = add(pending, n)?, None => unknown = true } }
        }
        let mut optional_bytes = 0;
        if let Some(d) = optional {
            if !unique.contains_key(d.media_id.as_str()) && !satisfied(d) {
                match d.bytes { Some(n) => optional_bytes = add(0, n)?, None => unknown = true }
            }
        }
        let projected = add(add(occupied, pending)?, optional_bytes)?;
        let additional = add(add(pending, remaining)?, optional_bytes)?;
        let required_free = add(floor, additional)?;
        let budget_deficit = projected.saturating_sub(budget).max(0);
        let free_deficit = required_free.saturating_sub(free).max(0);
        Ok(Self { occupied, pending, running_remaining: remaining, optional: optional_bytes, unknown, projected, additional, budget_deficit, free_deficit, optional_allowed: !unknown && budget_deficit == 0 && free_deficit == 0 })
    }
}

#[cfg(test)]
mod extra_accounting_tests {
    use super::*;
    #[test]
    fn rolling_expiry_preserves_windows_leases_and_restart_charges() {
        let dir = std::env::temp_dir().join(format!("expiry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = SourceIdentity::new("source", 10, 1);
        for (name, start, end) in [("old", 0, 300_000), ("required", 300_000, 600_000)] {
            std::fs::write(dir.join(format!("{name}.mkv")), b"payload").unwrap();
            let m = chunks::Manifest { version: 2, source_path: id.path.clone(), source_size: id.size, source_mtime: id.mtime, chunk_file: format!("{name}.mkv"), payload_bytes: 7, source_start_ms: start, source_end_ms: end, local_start_ms: 0 };
            std::fs::write(dir.join(format!("{name}.json")), serde_json::to_vec(&m).unwrap()).unwrap();
        }
        let index = chunks::ChunkIndex::open(&dir).unwrap();
        let cache = CacheIndex::new(1_000_000, false);
        for item in std::fs::read_dir(&dir).unwrap() {
            let path = item.unwrap().path(); let size = std::fs::metadata(&path).unwrap().len() as i128;
            cache.0.state.lock().unwrap().extra.insert(ExtraKey::OrphanPath(path.clone()), ExtraCharge { reserved: size, paths: vec![path], active: false });
        }
        let unknown = dir.join("unknown"); std::fs::write(&unknown, b"unknown").unwrap();
        cache.0.state.lock().unwrap().extra.insert(ExtraKey::OrphanPath(unknown.clone()), ExtraCharge { reserved: 7, paths: vec![unknown], active: false });
        let budget = SharedChunkBudget { index: cache.clone(), admission: Arc::new(tokio::sync::Mutex::new(())), directory: dir.clone(), floor: 0 };
        let control = chunks::WorkerControl::new(budget, dir.clone(), 1000, Duration::from_secs(1), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let rows = vec![chunks::Airing { airing_id: "a".into(), channel_id: "a".into(), media_id: "m".into(), source: id.clone(), in_ms: 0, start_ms: 0, end_ms: 600_000 }, chunks::Airing { airing_id: "b".into(), channel_id: "b".into(), media_id: "m".into(), source: id.clone(), in_ms: 0, start_ms: 0, end_ms: 600_000 }];
        let lease = index.acquire(&id, 1).unwrap();
        let before = cache.0.state.lock().unwrap().occupied_bytes();
        chunks::rolling_worker_tick(&rows, 400_000, &index, &control);
        assert!(dir.join("old.mkv").exists());
        assert!(dir.join("required.mkv").exists());
        drop(lease);
        let mut overlap = rows.clone();
        overlap[1].start_ms = 300_000; overlap[1].end_ms = 900_000;
        chunks::rolling_worker_tick(&overlap, 400_000, &index, &control);
        assert!(dir.join("old.mkv").exists(), "another channel still needs this source interval");
        chunks::rolling_worker_tick(&rows, 350_000, &index, &control);
        assert!(dir.join("old.mkv").exists(), "behind-playhead grace protects the chunk");
        chunks::rolling_worker_tick(&rows, 400_000, &index, &control);
        assert!(!dir.join("old.mkv").exists());
        assert!(dir.join("required.mkv").exists());
        assert!(cache.0.state.lock().unwrap().occupied_bytes() < before);
        assert!(cache.0.state.lock().unwrap().extra.contains_key(&ExtraKey::OrphanPath(dir.join("unknown"))));
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let held = cache.0.state.lock().unwrap().occupied_bytes();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            chunks::rolling_worker_tick(&[], 700_000, &index, &control);
            assert_eq!(cache.0.state.lock().unwrap().occupied_bytes(), held);
            assert!(index.acquire(&id, 400_000).is_none());
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            chunks::rolling_worker_tick(&[], 700_000, &index, &control);
            assert_eq!(cache.0.state.lock().unwrap().occupied_bytes(), 7);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn real_chunk_writer_commits_shared_charge_and_blocks_whole_copy() {
        use chunks::ChunkBudget;
        let root = std::env::temp_dir().join(format!("shared-chunk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.mp4");
        assert!(std::process::Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=size=64x64:rate=10:duration=12", "-c:v", "libx264", "-threads", "1", "-g", "20", "-bf", "0", "-y"]).arg(&source).status().unwrap().success());
        let dir = root.join("chunks"); std::fs::create_dir(&dir).unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let identity = SourceIdentity::new(source.to_string_lossy(), meta.len() as i64, metadata_mtime(&meta));
        let req = chunks::Request { source: identity.clone(), start_ms: 0, end_ms: 4000 };
        let index = CacheIndex::new(20_000_000, false);
        let budget = SharedChunkBudget { index: index.clone(), admission: Arc::new(tokio::sync::Mutex::new(())), directory: dir.clone(), floor: 0 };
        let paths = vec![dir.join("active")];
        let guard = budget.reserve(&req, &paths, 19_000_000).unwrap();
        assert!(index.reserve("whole", identity.clone(), root.join("whole"), 2_000_000).is_none());
        drop(guard);
        let manifest = chunks::write_chunk(&req, &dir, &budget, 10_000_000, Duration::from_secs(15)).expect("real chunk");
        let lease = chunks::ChunkIndex::open(&dir).unwrap().acquire(&identity, 0).expect("local lease");
        assert!(lease.path.exists());
        let actual: i128 = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().metadata().unwrap().len() as i128).sum();
        assert_eq!(index.0.state.lock().unwrap().occupied_bytes(), actual);
        assert!(actual > 0 && !manifest.chunk_file.is_empty());
        assert!(index.reserve("whole", identity, root.join("whole"), 20_000_000).is_none());
        drop(lease);
        let control = chunks::WorkerControl::new(budget, dir.clone(), 10_000_000, Duration::from_secs(1), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        chunks::rolling_worker_tick(&[], 0, &chunks::ChunkIndex::open(&dir).unwrap(), &control);
        assert_eq!(index.0.state.lock().unwrap().occupied_bytes(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn restart_accounts_invalid_manifest_and_residual_files() {
        let root = std::env::temp_dir().join(format!("chunk-orphan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&root).await.unwrap();
        let mut config = Config::from_env(); config.data_dir = root.clone(); config.precache_dir = root.join("cache");
        let service = Precache::new(&config, pool.clone());
        let dir = config.precache_dir.join("chunks"); std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.json"), b"bad").unwrap();
        std::fs::write(dir.join(".residual"), b"abcd").unwrap();
        service.initialize_index().await.unwrap();
        assert_eq!(service.index.0.state.lock().unwrap().occupied_bytes(), 7);
        drop(service); pool.close().await; std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unmeasurable_residual_retains_reservation() {
        use chunks::ChunkBudget;
        let root = std::env::temp_dir().join(format!("chunk-residual-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let index = CacheIndex::new(100, false);
        let budget = SharedChunkBudget { index: index.clone(), admission: Arc::new(tokio::sync::Mutex::new(())), directory: root.clone(), floor: 0 };
        let request = chunks::Request { source: SourceIdentity::new("source", 1, 1), start_ms: 0, end_ms: 1 };
        let path = root.join("residual");
        let guard = budget.reserve(&request, &[path.clone()], 50).unwrap();
        std::fs::create_dir(&path).unwrap(); drop(guard);
        assert_eq!(index.0.state.lock().unwrap().occupied_bytes(), 50);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn extras_count_as_occupied_and_remaining_without_satisfying_demand() {
        let demand = MandatoryDemand { media_id: "a".into(), priority: 0, identity: None, bytes: Some(10) };
        let result = AdmissionAccounting::calculate(&[demand], &[AccountedCache::Extra { reserved: 20, remaining: 7 }], None, 100, 100, 0).unwrap();
        assert_eq!((result.occupied, result.running_remaining, result.pending), (20, 7, 10));
    }
}
