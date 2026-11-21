//! Standalone, opt-in interval planning and chunk metadata. Not wired to playback.
use super::SourceIdentity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const HOUR: i64 = 3_600_000;
const SHORT: i64 = 900_000;
const CHUNK: i64 = 300_000;

#[derive(Clone, Debug)]
pub struct Airing {
    pub airing_id: String,
    pub channel_id: String,
    pub media_id: String,
    pub source: SourceIdentity,
    pub in_ms: i64,
    pub start_ms: i64,
    pub end_ms: i64,
}
#[derive(Clone, Debug)]
pub struct Target {
    pub airing: Airing,
    pub source_start_ms: i64,
    pub source_end_ms: i64,
    pub tier: u8,
}
#[derive(Clone, Debug)]
pub struct Request {
    pub source: SourceIdentity,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Real encodes carry small timestamp gaps (dropped or variable-rate frames);
/// the stream copy keeps them and the publisher holds the last frame across
/// them. A gap beyond this suggests lost data, and the chunk is rejected.
const MAX_PACKET_GAP_MS: i64 = 1_000;
/// A remux may legitimately end this far before the requested end when the
/// source's real duration is shorter than its catalogued runtime. The manifest
/// records the real end, and the planner ignores a trailing gap this small.
const END_TOLERANCE_MS: i64 = 2_000;
/// Failure backoff doubles per consecutive failure of a source, up to this cap.
const MAX_BACKOFF_MS: i64 = 30 * 60_000;
/// The worker fills only coverage that airs within this window, plus the head
/// of each channel's next programme. One serial worker on a slow share cannot
/// fill whole programmes for every channel; this bound keeps every channel a
/// little ahead instead of a few channels far ahead.
const LOOKAHEAD_MS: i64 = 10 * 60_000;
/// A source that keeps failing (usually a remux timeout on a high-bitrate file)
/// is retried with halved chunks, down to this floor.
const MIN_RETRY_CHUNK_MS: i64 = 60_000;
/// After live playback struggles to open a source, the worker stays off the
/// share for this long. Playback always wins over cache fill.
const YIELD_MS: i64 = 120_000;

/// Signals from live playback that the shared media source is struggling.
/// The chunk worker refuses to start while a recent signal stands, and kills
/// an in-flight remux when a new one arrives.
#[derive(Debug)]
pub struct PlaybackPressure {
    last_ms: std::sync::atomic::AtomicI64,
    events: std::sync::atomic::AtomicU64,
}
impl Default for PlaybackPressure {
    fn default() -> Self {
        Self {
            last_ms: std::sync::atomic::AtomicI64::new(i64::MIN),
            events: Default::default(),
        }
    }
}
impl PlaybackPressure {
    pub fn note(&self, now: i64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.last_ms.fetch_max(now, Relaxed);
        self.events.fetch_add(1, Relaxed);
    }
    fn recent(&self, now: i64) -> bool {
        let last = self.last_ms.load(std::sync::atomic::Ordering::Relaxed);
        last != i64::MIN && last > now.saturating_sub(YIELD_MS)
    }
    fn events(&self) -> u64 {
        self.events.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Current, next, and conditional third targets; each interval is capped to its airing.
pub fn targets(rows: &[Airing], now: i64) -> Vec<Target> {
    let mut channels: BTreeMap<&str, Vec<&Airing>> = BTreeMap::new();
    for row in rows {
        channels.entry(&row.channel_id).or_default().push(row);
    }
    let mut result = Vec::new();
    for items in channels.values_mut() {
        items.sort_by_key(|x| x.start_ms);
        let Some(i) = items
            .iter()
            .position(|x| x.start_ms <= now && now < x.end_ms)
        else {
            continue;
        };
        let current = items[i];
        let mut selected = vec![(current, 0)];
        if let Some(next) = items
            .get(i + 1)
            .filter(|x| x.start_ms <= now.saturating_add(HOUR))
        {
            selected.push((*next, 1));
            if current.end_ms.saturating_sub(current.start_ms) < SHORT
                && next.end_ms.saturating_sub(next.start_ms) < SHORT
            {
                if let Some(third) = items.get(i + 2) {
                    selected.push((*third, 2));
                }
            }
        }
        for (a, tier) in selected {
            let elapsed = if tier == 0 {
                now.saturating_sub(a.start_ms)
            } else {
                0
            };
            let start = a.in_ms.saturating_add(elapsed);
            let end = a.in_ms.saturating_add(a.end_ms.saturating_sub(a.start_ms));
            let end = end.min(start.saturating_add(HOUR));
            if a.in_ms >= 0 && start < end {
                result.push(Target {
                    airing: a.clone(),
                    source_start_ms: start,
                    source_end_ms: end,
                    tier,
                });
            }
        }
    }
    result
}

/// First current chunk for every channel precedes all extensions; overlapping
/// requests are deduplicated without advancing another channel's first chunk.
pub fn requests(targets: &[Target]) -> Vec<Request> {
    requests_with_chunk(targets, CHUNK)
}

fn requests_with_chunk(targets: &[Target], chunk: i64) -> Vec<Request> {
    let mut pending: Vec<(u8, usize, &Target)> = Vec::new();
    for t in targets {
        let count = ((t.source_end_ms - t.source_start_ms + chunk - 1) / chunk) as usize;
        for n in 0..count {
            pending.push((if t.tier == 0 && n == 0 { 0 } else { t.tier + 1 }, n, t));
        }
    }
    pending.sort_by_key(|(tier, n, t)| (*tier, *n, t.airing.channel_id.clone()));
    let mut out: Vec<Request> = Vec::new();
    for (_, n, t) in pending {
        let start = t.source_start_ms + n as i64 * chunk;
        let end = (start + chunk).min(t.source_end_ms);
        // Subtract already scheduled coverage; keep first-request priority stable.
        let mut pieces = vec![(start, end)];
        for old in out.iter().filter(|r| r.source == t.airing.source) {
            pieces = pieces
                .into_iter()
                .flat_map(|(a, b)| {
                    if old.end_ms <= a || old.start_ms >= b {
                        vec![(a, b)]
                    } else {
                        let mut v = Vec::new();
                        if a < old.start_ms {
                            v.push((a, old.start_ms));
                        }
                        if old.end_ms < b {
                            v.push((old.end_ms, b));
                        }
                        v
                    }
                })
                .collect();
        }
        for (a, b) in pieces {
            out.push(Request {
                source: t.airing.source.clone(),
                start_ms: a,
                end_ms: b,
            });
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime: i64,
    pub chunk_file: String,
    pub payload_bytes: u64,
    pub source_start_ms: i64,
    pub source_end_ms: i64,
    pub local_start_ms: i64,
}
impl Manifest {
    /// Returns local seek only when identity and the entire requested span are covered.
    pub fn resolve(
        &self,
        identity: &SourceIdentity,
        start: i64,
        end: i64,
        directory: &Path,
    ) -> Option<(PathBuf, i64)> {
        if self.version != 2
            || self.source_path != identity.path
            || self.source_size != identity.size
            || self.source_mtime != identity.mtime
            || start < self.source_start_ms
            || end > self.source_end_ms
            || end <= start
            || self.source_end_ms <= self.source_start_ms
        {
            return None;
        }
        let file = Path::new(&self.chunk_file);
        if file.components().count() != 1
            || !matches!(
                file.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return None;
        }
        let path = directory.join(file);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| {
            m.file_type().is_file() && self.payload_bytes > 0 && m.len() == self.payload_bytes
        }) {
            return None;
        }
        Some((
            path,
            self.local_start_ms
                .checked_add(start.checked_sub(self.source_start_ms)?)?,
        ))
    }
}

/// A process-local index rebuilt solely from local manifests and payloads.
/// All eviction of indexed chunks must go through this index.
#[derive(Clone)]
pub struct ChunkIndex {
    directory: PathBuf,
    state: std::sync::Arc<std::sync::Mutex<Vec<IndexedChunk>>>,
}
struct IndexedChunk {
    manifest_path: PathBuf,
    manifest: Manifest,
    leases: usize,
    deleting: bool,
    deletion_started: bool,
}

pub struct ChunkLease {
    state: std::sync::Arc<std::sync::Mutex<Vec<IndexedChunk>>>,
    manifest_path: PathBuf,
    pub path: PathBuf,
    pub local_seek: i64,
    pub source_coverage_end: i64,
}
impl Drop for ChunkLease {
    fn drop(&mut self) {
        let mut entries = self.state.lock().unwrap();
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.manifest_path == self.manifest_path)
        {
            entry.leases -= 1;
        }
    }
}
impl ChunkIndex {
    pub fn open(directory: &Path) -> std::io::Result<Self> {
        type Registry = std::collections::HashMap<
            PathBuf,
            std::sync::Weak<std::sync::Mutex<Vec<IndexedChunk>>>,
        >;
        static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Registry>> =
            std::sync::OnceLock::new();
        let directory = std::fs::canonicalize(directory)?;
        let mut registry = REGISTRY.get_or_init(Default::default).lock().unwrap();
        if let Some(state) = registry.get(&directory).and_then(|w| w.upgrade()) {
            return Ok(Self { directory, state });
        }
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&directory)? {
            let path = item?.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(manifest) = (|| -> Option<Manifest> {
                let metadata = std::fs::symlink_metadata(&path).ok()?;
                if !metadata.file_type().is_file() || metadata.len() > 65_536 {
                    return None;
                }
                let m: Manifest = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
                if m.version != 2
                    || m.source_start_ms < 0
                    || m.local_start_ms < 0
                    || m.source_end_ms <= m.source_start_ms
                {
                    return None;
                }
                let payload = Path::new(&m.chunk_file);
                if payload.components().count() != 1
                    || !matches!(
                        payload.components().next(),
                        Some(std::path::Component::Normal(_))
                    )
                    || payload.file_stem() != path.file_stem()
                {
                    return None;
                }
                let meta = std::fs::symlink_metadata(directory.join(payload)).ok()?;
                if !meta.file_type().is_file()
                    || m.payload_bytes == 0
                    || meta.len() != m.payload_bytes
                {
                    return None;
                }
                Some(m)
            })() else {
                continue;
            };
            entries.push(IndexedChunk {
                manifest_path: path,
                manifest,
                leases: 0,
                deleting: false,
                deletion_started: false,
            });
        }
        let state = std::sync::Arc::new(std::sync::Mutex::new(entries));
        registry.insert(directory.clone(), std::sync::Arc::downgrade(&state));
        Ok(Self { directory, state })
    }
    /// Acquires one covered interval without checking the source filesystem.
    pub fn acquire(&self, identity: &SourceIdentity, start: i64) -> Option<ChunkLease> {
        let mut entries = self.state.lock().unwrap();
        let entry = entries
            .iter_mut()
            .filter(|e| {
                !e.deleting
                    && e.manifest
                        .resolve(
                            identity,
                            start,
                            start.checked_add(1).unwrap_or(start),
                            &self.directory,
                        )
                        .is_some()
            })
            .max_by_key(|e| e.manifest.source_end_ms)?;
        let (path, local_seek) =
            entry
                .manifest
                .resolve(identity, start, start.checked_add(1)?, &self.directory)?;
        entry.leases += 1;
        Some(ChunkLease {
            state: self.state.clone(),
            manifest_path: entry.manifest_path.clone(),
            path,
            local_seek,
            source_coverage_end: entry.manifest.source_end_ms,
        })
    }
    /// Source identities recorded by indexed manifests, newest mtime first per path.
    /// Lets a restarted process recognise cached sources whose origin is offline.
    pub fn indexed_identities(&self) -> Vec<SourceIdentity> {
        let entries = self.state.lock().unwrap();
        let mut newest: std::collections::HashMap<&str, &Manifest> =
            std::collections::HashMap::new();
        for entry in entries.iter().filter(|e| !e.deleting) {
            let m = &entry.manifest;
            let slot = newest.entry(m.source_path.as_str()).or_insert(m);
            if m.source_mtime > slot.source_mtime {
                *slot = m;
            }
        }
        newest
            .values()
            .map(|m| SourceIdentity::new(m.source_path.clone(), m.source_size, m.source_mtime))
            .collect()
    }
    /// Rejects gaps and stale identities; does not imply that the range is leased.
    pub fn covers(&self, identity: &SourceIdentity, mut start: i64, end: i64) -> bool {
        if end <= start {
            return false;
        }
        while start < end {
            let Some(lease) = self.acquire(identity, start) else {
                return false;
            };
            start = lease.source_coverage_end;
        }
        true
    }
    pub fn verified_ranges(
        &self,
        identity: &SourceIdentity,
        start: i64,
        end: i64,
    ) -> Vec<(i64, i64)> {
        let entries = self.state.lock().unwrap();
        let mut ranges: Vec<_> = entries
            .iter()
            .filter_map(|entry| {
                if entry.deleting {
                    return None;
                }
                let m = &entry.manifest;
                let a = start.max(m.source_start_ms);
                let b = end.min(m.source_end_ms);
                (a < b && m.resolve(identity, a, b, &self.directory).is_some()).then_some((a, b))
            })
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (a, b) in ranges {
            if let Some(last) = merged.last_mut().filter(|last| a <= last.1) {
                last.1 = last.1.max(b);
            } else {
                merged.push((a, b));
            }
        }
        merged
    }
    /// Startup requires the first five minutes (or the shorter assigned range).
    /// Rolling readiness requires continuous coverage of the entire required range.
    pub fn readiness(&self, identity: &SourceIdentity, start: i64, end: i64) -> (bool, bool) {
        if end <= start {
            return (false, false);
        }
        let startup_end = end.min(start.saturating_add(CHUNK));
        (
            self.covers(identity, start, startup_end),
            self.covers(identity, start, end),
        )
    }
    /// Delete only indexed files, keeping failed removals unavailable for retry.
    pub fn evict(&self, manifest_path: &Path) -> std::io::Result<bool> {
        self.remove_one(manifest_path, &|_, _| {})
    }
    fn remove_one(
        &self,
        manifest_path: &Path,
        release: &impl Fn(&Path, &Path),
    ) -> std::io::Result<bool> {
        let mut entries = self.state.lock().unwrap();
        let Some(i) = entries
            .iter()
            .position(|e| e.manifest_path == manifest_path)
        else {
            return Ok(false);
        };
        if entries[i].leases != 0 {
            return Ok(false);
        }
        let payload = self.directory.join(&entries[i].manifest.chunk_file);
        entries[i].deleting = true;
        if !entries[i].deletion_started {
            let valid = std::fs::symlink_metadata(&entries[i].manifest_path)
                .is_ok_and(|m| m.file_type().is_file() && m.len() <= 65_536)
                && std::fs::read(&entries[i].manifest_path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
                    .as_ref()
                    == Some(&entries[i].manifest)
                && std::fs::symlink_metadata(&payload).is_ok_and(|m| {
                    m.file_type().is_file() && m.len() == entries[i].manifest.payload_bytes
                });
            if !valid {
                return Err(std::io::Error::other(
                    "indexed chunk changed; expiry refused",
                ));
            }
        }
        entries[i].deletion_started = true;
        for path in [&entries[i].manifest_path, &payload] {
            match std::fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_file() => std::fs::remove_file(path)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => return Err(std::io::Error::other("chunk path is not a regular file")),
                Err(e) => return Err(e),
            }
        }
        release(&entries[i].manifest_path, &payload);
        entries.remove(i);
        Ok(true)
    }
    fn expire<B: ChunkBudget>(&self, required: &[Target], budget: &B) {
        const GRACE: i64 = 60_000;
        const LIMIT: usize = 8;
        // Rotate inspected entries so repeated failures cannot starve later candidates.
        let candidates = {
            let mut entries = self.state.lock().unwrap();
            let mut paths = Vec::new();
            let count = entries.len();
            for entry in entries.iter() {
                let m = &entry.manifest;
                let protected = required.iter().any(|t| {
                    m.source_path == t.airing.source.path
                        && m.source_size == t.airing.source.size
                        && m.source_mtime == t.airing.source.mtime
                        && m.source_start_ms < t.source_end_ms
                        && m.source_end_ms > t.source_start_ms.saturating_sub(GRACE)
                });
                if paths.len() < LIMIT && entry.leases == 0 && !protected {
                    paths.push(entry.manifest_path.clone());
                }
            }
            if !entries.is_empty() {
                entries.rotate_left(LIMIT.min(count) % count);
            }
            paths
        };
        for path in candidates {
            if let Err(error) = self.remove_one(&path, &|manifest, payload| {
                budget.release(manifest, payload)
            }) {
                tracing::warn!(?path, %error, "chunk expiry deferred");
            }
        }
    }
}

/// One tick runs at most one remux. Failed intervals are deferred independently.
pub struct WorkerControl<B> {
    pub budget: B,
    pub directory: PathBuf,
    pub max_bytes: u64,
    pub timeout: std::time::Duration,
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub chunk_ms: i64,
    failures: std::sync::Mutex<Vec<(Request, i64)>>,
    strikes: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    pub pressure: std::sync::Arc<PlaybackPressure>,
    active: std::sync::Mutex<()>,
}
impl<B> WorkerControl<B> {
    pub fn new(
        budget: B,
        directory: PathBuf,
        max_bytes: u64,
        timeout: std::time::Duration,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            budget,
            directory,
            max_bytes,
            timeout,
            cancelled,
            chunk_ms: CHUNK,
            failures: Default::default(),
            strikes: Default::default(),
            pressure: Default::default(),
            active: Default::default(),
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum TickOutcome {
    Idle,
    Completed,
    Failed,
}
/// Missing coverage only, earliest deadline first across all channels. A gap's
/// deadline is the wall time playback reaches it, so an uncovered head precedes
/// every extension and a next programme's head is ordered by its air time.
/// Planning from verified coverage (not a grid anchored at the moving playhead)
/// makes covered heads extend sequentially instead of being re-cut each tick.
fn missing_requests(targets: &[Target], index: &ChunkIndex, chunk: i64, now: i64) -> Vec<Request> {
    let mut pending: Vec<(i64, String, Request)> = Vec::new();
    for t in targets {
        let (start, end) = (t.source_start_ms, t.source_end_ms);
        let mut cursor = start;
        let mut gaps = Vec::new();
        for (a, b) in index.verified_ranges(&t.airing.source, start, end) {
            if a > cursor {
                gaps.push((cursor, a));
            }
            cursor = cursor.max(b);
        }
        if end - cursor >= END_TOLERANCE_MS.min(chunk) {
            gaps.push((cursor, end));
        }
        for (a, b) in gaps {
            let mut s = a;
            while s < b {
                let e = (s + chunk).min(b);
                let deadline = t
                    .airing
                    .start_ms
                    .saturating_add(s.saturating_sub(t.airing.in_ms));
                let next_head = t.tier > 0 && s < t.airing.in_ms.saturating_add(LOOKAHEAD_MS);
                if deadline > now.saturating_add(LOOKAHEAD_MS) && !next_head {
                    break;
                }
                pending.push((
                    deadline,
                    t.airing.channel_id.clone(),
                    Request {
                        source: t.airing.source.clone(),
                        start_ms: s,
                        end_ms: e,
                    },
                ));
                s = e;
            }
        }
    }
    pending.sort_by(|x, y| (x.0, &x.1).cmp(&(y.0, &y.1)));
    let mut out: Vec<Request> = Vec::new();
    for (_, _, r) in pending {
        // The same source may air on two channels; never cut one interval twice.
        if out
            .iter()
            .any(|o| o.source == r.source && o.start_ms < r.end_ms && r.start_ms < o.end_ms)
        {
            continue;
        }
        out.push(r);
    }
    out
}

pub fn rolling_worker_tick<B: ChunkBudget>(
    schedule_snapshot: &[Airing],
    now: i64,
    index: &ChunkIndex,
    control: &WorkerControl<B>,
) -> TickOutcome {
    let Ok(_active) = control.active.try_lock() else {
        return TickOutcome::Idle;
    };
    if control.cancelled.load(std::sync::atomic::Ordering::Relaxed)
        || control.chunk_ms <= 0
        || control.chunk_ms > CHUNK
    {
        return TickOutcome::Idle;
    }
    if control.pressure.recent(now) {
        return TickOutcome::Idle;
    }
    let required = targets(schedule_snapshot, now);
    index.expire(&required, &control.budget);
    let mut failures = control.failures.lock().unwrap();
    failures.retain(|(_, until)| *until > now);
    let candidate = missing_requests(&required, index, control.chunk_ms, now)
        .into_iter()
        .find(|r| !failures.iter().any(|(old, _)| old.source == r.source));
    drop(failures);
    let Some(mut request) = candidate else {
        return TickOutcome::Idle;
    };
    let prior = control
        .strikes
        .lock()
        .unwrap()
        .get(&request.source.path)
        .copied()
        .unwrap_or(0);
    if prior > 0 {
        let shrunk =
            (control.chunk_ms >> prior.min(8)).max(MIN_RETRY_CHUNK_MS.min(control.chunk_ms));
        request.end_ms = request.end_ms.min(request.start_ms.saturating_add(shrunk));
    }
    let started = std::time::Instant::now();
    let baseline = control.pressure.events();
    let yielded = || control.pressure.events() != baseline;
    let stop = || control.cancelled.load(std::sync::atomic::Ordering::Relaxed) || yielded();
    if let Some(manifest) = write_chunk_controlled(
        &request,
        index,
        &control.directory,
        &control.budget,
        control.max_bytes,
        control.timeout,
        &stop,
    ) {
        control.strikes.lock().unwrap().remove(&request.source.path);
        tracing::info!(source = %request.source.path, start_ms = manifest.source_start_ms, end_ms = manifest.source_end_ms, bytes = manifest.payload_bytes, elapsed_ms = started.elapsed().as_millis() as u64, "rolling chunk written");
        TickOutcome::Completed
    } else {
        if yielded() {
            tracing::info!(source = %request.source.path, start_ms = request.start_ms, "rolling chunk abandoned; yielding to live playback");
            return TickOutcome::Idle;
        }
        if !control.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            let strikes = {
                let mut m = control.strikes.lock().unwrap();
                let n = m.entry(request.source.path.clone()).or_insert(0);
                *n = n.saturating_add(1);
                *n
            };
            let backoff = 60_000i64
                .saturating_mul(1i64 << strikes.saturating_sub(1).min(5))
                .min(MAX_BACKOFF_MS);
            tracing::warn!(source = %request.source.path, strikes, backoff_ms = backoff, "rolling chunk unavailable; using source fallback, whole-copy job suppressed");
            control
                .failures
                .lock()
                .unwrap()
                .push((request, now.saturating_add(backoff)));
        }
        TickOutcome::Failed
    }
}

/// Caller holds admission and eviction protection for the duration of this guard.
pub trait ChunkCharge {
    fn commit(&mut self, ready_paths: &[PathBuf]);
}
pub trait ChunkBudget {
    type Guard: ChunkCharge;
    fn reserve(&self, request: &Request, paths: &[PathBuf], total_cap: u64) -> Option<Self::Guard>;
    fn release(&self, _manifest: &Path, _payload: &Path) {}
}

// FFmpeg's -fs can overshoot by a packet. The OS limit applies before writes.
#[cfg(unix)]
fn limit_child_file_size(command: &mut std::process::Command, bytes: u64) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let limit: libc::rlim_t = bytes.try_into().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file limit too large")
    })?;
    unsafe {
        command.pre_exec(move || {
            let limits = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limits) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            Ok(())
        });
    }
    Ok(())
}
#[cfg(not(unix))]
fn limit_child_file_size(_: &mut std::process::Command, _: u64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "hard file limit unavailable",
    ))
}

/// Returns None for unsupported inputs or unprovable timestamps. The caller may
/// use the existing full-copy/source resolver in that case. No production wiring.
pub fn write_chunk<B: ChunkBudget>(
    request: &Request,
    directory: &Path,
    budget: &B,
    max_bytes: u64,
    timeout: std::time::Duration,
) -> Option<Manifest> {
    let index = ChunkIndex::open(directory).ok()?;
    write_chunk_controlled(
        request,
        &index,
        directory,
        budget,
        max_bytes,
        timeout,
        &|| false,
    )
}
/// Log a failed remux with exit status/signal and a bounded (~2KB) tail of ffmpeg stderr.
fn warn_remux_failure(request: &Request, status: &std::process::ExitStatus, stderr_path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut tail = String::new();
    if let Ok(mut file) = std::fs::File::open(stderr_path) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(len.saturating_sub(2048)));
        let mut bytes = Vec::new();
        let _ = file.take(2048).read_to_end(&mut bytes);
        tail = String::from_utf8_lossy(&bytes).into_owned();
    }
    tracing::warn!(source=%request.source.path, start_ms=request.start_ms, end_ms=request.end_ms, status=%status, signal=?signal, stderr_tail=%tail.trim(), "chunk remux failed");
}
fn write_chunk_controlled<B: ChunkBudget>(
    request: &Request,
    index: &ChunkIndex,
    directory: &Path,
    budget: &B,
    max_bytes: u64,
    timeout: std::time::Duration,
    cancelled: &dyn Fn() -> bool,
) -> Option<Manifest> {
    use std::process::{Command, Stdio};
    use std::time::Instant;
    if request.start_ms < 0
        || request.end_ms <= request.start_ms
        || request.end_ms - request.start_ms > CHUNK
        || max_bytes == 0
    {
        return None;
    }
    let matches_source = || {
        std::fs::metadata(&request.source.path)
            .ok()
            .is_some_and(|m| {
                m.is_file()
                    && m.len() as i64 == request.source.size
                    && super::metadata_mtime(&m) == request.source.mtime
            })
    };
    if !matches_source() {
        return None;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let temporary = directory.join(format!(".{id}.partial.mkv"));
    let published = format!("{id}.mkv");
    let metadata_tmp = directory.join(format!(".{id}.json.tmp"));
    let metadata_final = directory.join(format!("{id}.json"));
    let paths = vec![
        temporary.clone(),
        directory.join(&published),
        metadata_tmp.clone(),
        metadata_final.clone(),
        directory.join(format!(".{id}.probe")),
        directory.join(format!(".{id}.source")),
        directory.join(format!(".{id}.format")),
    ];
    let total_cap = max_bytes.checked_add(8_000_000 + 65_536 + 1_024 + 65_536)?;
    let Some(mut guard) = budget.reserve(request, &paths, total_cap) else {
        tracing::warn!(source=%request.source.path, start_ms=request.start_ms, end_ms=request.end_ms, stage="budget_admission", max_bytes, total_cap, "chunk writer failed");
        return None;
    };
    let stderr_path = directory.join(format!(".{id}.stderr"));
    macro_rules! bail { ($($a:tt)*) => {{ tracing::warn!(source=%request.source.path, start_ms=request.start_ms, end_ms=request.end_ms, "chunk validation failed: {}", format!($($a)*)); return None; }} }
    let stage = std::cell::Cell::new("remux");
    let mut run = || -> Option<Manifest> {
        // All streams are mapped: unsupported container/codec combinations fail.
        // ffmpeg's file size limit bounds output; timeout kills and reaps child.
        let mut command = Command::new("ffmpeg");
        command
            .args([
                "-nostdin",
                "-v",
                "error",
                "-ss",
                &format!("{:.3}", request.start_ms as f64 / 1000.0),
                "-copyts",
                "-i",
            ])
            .arg(&request.source.path)
            .args([
                "-t",
                &format!("{:.3}", request.end_ms as f64 / 1000.0),
                "-map",
                "0",
                "-c",
                "copy",
                "-avoid_negative_ts",
                "disabled",
                "-fs",
                &max_bytes.to_string(),
                "-f",
                "matroska",
                "-y",
            ])
            .arg(&temporary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).ok()?));
        limit_child_file_size(&mut command, max_bytes).ok()?;
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now().checked_add(timeout)?;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        warn_remux_failure(request, &status, &stderr_path);
                        return None;
                    }
                    break;
                }
                Ok(None) if !cancelled() && Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(source=%request.source.path, start_ms=request.start_ms, "chunk remux cancelled or timed out");
                    return None;
                }
            }
        }
        if std::fs::metadata(&temporary).ok()?.len() >= max_bytes {
            bail!("remux output reached max_bytes {max_bytes}");
        }
        // ffprobe output is redirected to a bounded local file, never an unbounded pipe.
        stage.set("packet_probe");
        let probe = directory.join(format!(".{id}.probe"));
        let result = (|| -> Option<Manifest> {
            let output = std::fs::File::create(&probe).ok()?;
            let mut command = Command::new("ffprobe");
            command
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "packet=pts_time,duration_time,flags",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&temporary)
                .stdout(Stdio::from(output))
                .stderr(Stdio::null());
            limit_child_file_size(&mut command, 8_000_000).ok()?;
            let mut child = command.spawn().ok()?;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() {
                            return None;
                        }
                        break;
                    }
                    Ok(None)
                        if !cancelled()
                            && Instant::now() < deadline
                            && std::fs::metadata(&probe).is_ok_and(|m| m.len() < 8_000_000) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            }
            if std::fs::metadata(&probe).ok()?.len() >= 8_000_000 {
                return None;
            }
            let data = std::fs::read_to_string(&probe).ok()?;
            let mut packets = Vec::new();
            for line in data.lines() {
                let mut fields = line.split(',');
                let pts: f64 = fields.next()?.parse().ok()?;
                let duration: f64 = fields.next()?.parse().ok()?;
                let flags = fields.next()?;
                if !pts.is_finite() || !duration.is_finite() || duration <= 0.0 {
                    return None;
                }
                packets.push((
                    (pts * 1000.0).round() as i64,
                    (duration * 1000.0).round() as i64,
                    flags.contains('K'),
                ));
            }
            // Audio-only and missing timing explicitly fall back. Require the first
            // packet to be a keyframe and contiguous presentation timestamps.
            stage.set("packet_validation");
            let first = *packets.first()?;
            if !first.2 || first.0 < 0 || first.0 > request.start_ms {
                bail!(
                    "first packet pts={} keyframe={} requested_start={}",
                    first.0,
                    first.2,
                    request.start_ms
                );
            }
            packets.sort_by_key(|p| p.0);
            let mut end = first.0;
            for (pts, dur, _) in packets {
                if pts > end + MAX_PACKET_GAP_MS {
                    bail!("packet gap: pts={pts} after end={end}");
                }
                end = end.max(pts.checked_add(dur)?);
            }
            if end < request.end_ms - END_TOLERANCE_MS.min((request.end_ms - request.start_ms) / 4)
            {
                bail!("output ends at {end}ms, short of {}ms", request.end_ms);
            }
            let end = end.min(request.end_ms);
            // Confirm the retained keyframe matches the source, rather than assuming
            // a remuxer preserved timestamps for every input/container combination.
            stage.set("source_keyframe_probe");
            let source_probe = directory.join(format!(".{id}.source"));
            let source_result = (|| -> Option<i64> {
                let output = std::fs::File::create(&source_probe).ok()?;
                // Probe at the chunk's retained keyframe: ffmpeg may seek to the keyframe before an exact-keyframe start.
                let interval = format!("{:.3}%+0.1", first.0 as f64 / 1000.0);
                let mut command = Command::new("ffprobe");
                command
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-read_intervals",
                        &interval,
                        "-show_entries",
                        "packet=pts_time,flags",
                        "-of",
                        "csv=p=0",
                    ])
                    .arg(&request.source.path)
                    .stdout(Stdio::from(output))
                    .stderr(Stdio::null());
                limit_child_file_size(&mut command, 65_536).ok()?;
                let mut child = command.spawn().ok()?;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if !status.success() {
                                return None;
                            }
                            break;
                        }
                        Ok(None)
                            if !cancelled()
                                && Instant::now() < deadline
                                && std::fs::metadata(&source_probe)
                                    .is_ok_and(|m| m.len() < 65_536) =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(20))
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return None;
                        }
                    }
                }
                if std::fs::metadata(&source_probe).ok()?.len() >= 65_536 {
                    return None;
                }
                let content = std::fs::read_to_string(&source_probe).ok()?;
                let mut fields = content.lines().next()?.split(',');
                let pts: f64 = fields.next()?.parse().ok()?;
                if !pts.is_finite() || !fields.next()?.contains('K') {
                    return None;
                }
                Some((pts * 1000.0).round() as i64)
            })();
            let _ = std::fs::remove_file(source_probe);
            let source_pts = source_result;
            if source_pts != Some(first.0) {
                bail!(
                    "source keyframe {:?} != chunk first pts {}",
                    source_pts,
                    first.0
                );
            }
            // The output container's start time defines FFmpeg's default relative seek origin.
            stage.set("format_probe");
            let format_probe = directory.join(format!(".{id}.format"));
            let format_result = (|| -> Option<i64> {
                let output = std::fs::File::create(&format_probe).ok()?;
                let mut command = Command::new("ffprobe");
                command
                    .args([
                        "-v",
                        "error",
                        "-show_entries",
                        "format=start_time",
                        "-of",
                        "default=nw=1:nk=1",
                    ])
                    .arg(&temporary)
                    .stdout(Stdio::from(output))
                    .stderr(Stdio::null());
                limit_child_file_size(&mut command, 1024).ok()?;
                let mut child = command.spawn().ok()?;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if !status.success() {
                                return None;
                            }
                            break;
                        }
                        Ok(None) if !cancelled() && Instant::now() < deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(20))
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return None;
                        }
                    }
                }
                if std::fs::metadata(&format_probe).ok()?.len() > 1024 {
                    return None;
                }
                let start: f64 = std::fs::read_to_string(&format_probe)
                    .ok()?
                    .trim()
                    .parse()
                    .ok()?;
                if !start.is_finite() {
                    return None;
                }
                Some((start * 1000.0).round() as i64)
            })();
            let _ = std::fs::remove_file(format_probe);
            let format_start = format_result?;
            if format_start > first.0 {
                bail!("format start {format_start} > first pts {}", first.0);
            }
            stage.set("publication");
            if cancelled() || !matches_source() {
                return None;
            }
            let payload = std::fs::symlink_metadata(&temporary).ok()?;
            if !payload.file_type().is_file() || payload.len() == 0 || payload.len() >= max_bytes {
                return None;
            }
            let manifest = Manifest {
                version: 2,
                payload_bytes: payload.len(),
                source_path: request.source.path.clone(),
                source_size: request.source.size,
                source_mtime: request.source.mtime,
                chunk_file: published.clone(),
                source_start_ms: first.0,
                source_end_ms: end,
                local_start_ms: first.0.checked_sub(format_start)?,
            };
            let metadata = serde_json::to_vec(&manifest).ok()?;
            if metadata.len() > 65_536 {
                return None;
            }
            std::fs::write(&metadata_tmp, metadata).ok()?;
            let final_payload = std::fs::symlink_metadata(&temporary).ok()?;
            if !final_payload.file_type().is_file() || final_payload.len() != manifest.payload_bytes
            {
                return None;
            }
            std::fs::rename(&temporary, directory.join(&published)).ok()?;
            if std::fs::rename(&metadata_tmp, &metadata_final).is_err() {
                let _ = std::fs::remove_file(directory.join(&published));
                return None;
            }
            let mut state = index.state.lock().unwrap();
            guard.commit(&[directory.join(&published), metadata_final.clone()]);
            state.push(IndexedChunk {
                manifest_path: metadata_final.clone(),
                manifest: manifest.clone(),
                leases: 0,
                deleting: false,
                deletion_started: false,
            });
            Some(manifest)
        })();
        let _ = std::fs::remove_file(probe);
        result
    };
    let result = run();
    if result.is_none() {
        tracing::warn!(source=%request.source.path, start_ms=request.start_ms, end_ms=request.end_ms, stage=stage.get(), max_bytes, "chunk writer failed");
    }
    let _ = std::fs::remove_file(stderr_path);
    let _ = std::fs::remove_file(temporary);
    let _ = std::fs::remove_file(metadata_tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn child_output_cannot_exceed_reserved_file_limit() {
        let dir = std::env::temp_dir().join(format!("chunk-limit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("output");
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("head -c 8192 /dev/zero > \"$1\"")
            .arg("writer")
            .arg(&output);
        limit_child_file_size(&mut command, 1024).unwrap();
        let _ = command.status().unwrap();
        assert!(std::fs::metadata(&output).unwrap().len() <= 1024);
        std::fs::remove_dir_all(dir).unwrap();
    }
    fn a(c: &str, id: &str, start: i64, end: i64, offset: i64) -> Airing {
        Airing {
            airing_id: id.into(),
            channel_id: c.into(),
            media_id: id.into(),
            source: SourceIdentity::new(id, 84_000_000_000, 1),
            in_ms: offset,
            start_ms: start,
            end_ms: end,
        }
    }
    #[test]
    fn offset_cap_fairness_and_overlap() {
        let rows = vec![
            a("1", "x", 0, 2_700_000, 600_000),
            a("2", "y", 0, 1_680_000, 0),
            a("3", "x", 0, 2_700_000, 600_000),
        ];
        let t = targets(&rows, 600_000);
        assert_eq!(
            (t[0].source_start_ms, t[0].source_end_ms),
            (1_200_000, 3_300_000)
        );
        let r = requests(&t);
        assert_eq!(r[0].source.path, "x");
        assert_eq!(r[1].source.path, "y");
        assert_eq!(
            r.iter()
                .filter(|x| x.source.path == "x")
                .map(|x| x.end_ms - x.start_ms)
                .sum::<i64>(),
            2_100_000
        );
    }
    #[test]
    fn short_policy_and_horizon() {
        let rows = vec![
            a("1", "a", 0, 600_000, 0),
            a("1", "b", 600_000, 1_200_000, 0),
            a("1", "c", 1_200_000, 1_800_000, 0),
            a("2", "d", 0, 900_000, 0),
            a("2", "e", 900_000, 1_000_000, 0),
            a("2", "f", 1_000_000, 1_100_000, 0),
        ];
        let t = targets(&rows, 1);
        assert_eq!(t.iter().filter(|t| t.tier == 2).count(), 1);
        let distant = vec![
            a("1", "a", 0, 4_000_000, 0),
            a("1", "b", 4_000_000, 5_000_000, 0),
        ];
        assert_eq!(targets(&distant, 1).len(), 1);
        assert_eq!(targets(&distant, 1)[0].source_end_ms, 3_600_001);
    }
    struct Budget;
    impl ChunkCharge for () {
        fn commit(&mut self, _: &[PathBuf]) {}
    }
    impl ChunkBudget for Budget {
        type Guard = ();
        fn reserve(&self, _: &Request, _: &[PathBuf], _: u64) -> Option<()> {
            Some(())
        }
    }
    #[test]
    fn worker_yields_to_recent_playback_pressure() {
        use std::sync::{Arc, atomic::AtomicBool};
        let dir = std::env::temp_dir().join(format!("atv-yield-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let index = ChunkIndex::open(&dir).unwrap();
        let rows = vec![a("1", "missing", 0, 600_000, 0)];
        let control = WorkerControl::new(
            Budget,
            dir.clone(),
            1_000_000,
            std::time::Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
        );
        control.pressure.note(1_000);
        assert_eq!(
            rolling_worker_tick(&rows, 1_000 + YIELD_MS - 1, &index, &control),
            TickOutcome::Idle,
            "no share access while playback struggles"
        );
        assert!(control.strikes.lock().unwrap().is_empty());
        // Once the window passes, the worker resumes (and this missing source fails).
        assert_eq!(
            rolling_worker_tick(&rows, 1_000 + YIELD_MS, &index, &control),
            TickOutcome::Failed
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rolling_tick_prioritizes_channels_skips_coverage_and_replenishes() {
        use std::process::Command;
        use std::sync::{Arc, atomic::AtomicBool};
        let dir = std::env::temp_dir().join(format!("atv-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source.mkv");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=64x64:rate=10:duration=20",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-g",
                    "20",
                    "-bf",
                    "0",
                    "-y"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let mut rows = Vec::new();
        for n in 1..=3 {
            let path = dir.join(format!("source{n}.mkv"));
            std::fs::hard_link(&source, &path).unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            rows.push(Airing {
                airing_id: n.to_string(),
                channel_id: n.to_string(),
                media_id: n.to_string(),
                source: SourceIdentity::new(
                    path.to_str().unwrap(),
                    meta.len() as i64,
                    super::super::metadata_mtime(&meta),
                ),
                in_ms: 0,
                start_ms: 0,
                end_ms: 6_000,
            });
        }
        let mut bad = rows[0].clone();
        bad.channel_id = "0".into();
        bad.source.path = dir.join("missing").to_string_lossy().into_owned();
        rows.insert(0, bad);
        let index = ChunkIndex::open(&dir).unwrap();
        let mut control = WorkerControl::new(
            Budget,
            dir.clone(),
            10_000_000,
            std::time::Duration::from_secs(15),
            Arc::new(AtomicBool::new(false)),
        );
        control.chunk_ms = 2_000;
        assert_eq!(
            rolling_worker_tick(&rows, 0, &index, &control),
            TickOutcome::Failed
        );
        for n in 1..=3 {
            assert_eq!(
                rolling_worker_tick(&rows, 0, &index, &control),
                TickOutcome::Completed
            );
            assert!(index.covers(&rows[n].source, 0, 2_000));
            for row in &rows[1..] {
                assert!(!index.covers(&row.source, 2_000, 4_000));
            }
        }
        assert_eq!(
            rolling_worker_tick(&rows, 0, &index, &control),
            TickOutcome::Completed
        );
        assert!(index.covers(&rows[1].source, 2_000, 4_000));
        let lease = index.acquire(&rows[1].source, 2_001).unwrap();
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-count_packets",
                "-show_entries",
                "stream=nb_read_packets",
                "-of",
                "csv=p=0",
            ])
            .arg(&lease.path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let packets: usize = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            packets <= 22,
            "remux exceeded assigned two-second interval: {packets} packets"
        );
        drop(lease);
        assert!(!index.covers(&rows[1].source, 6_000, 8_000));
        for row in &rows[2..] {
            assert_eq!(
                rolling_worker_tick(&rows, 0, &index, &control),
                TickOutcome::Completed
            );
            assert!(index.covers(&row.source, 2_000, 4_000));
        }
        assert_eq!(
            rolling_worker_tick(&rows, 4_000, &index, &control),
            TickOutcome::Completed
        );
        assert!(index.covers(&rows[1].source, 4_000, 6_000));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn local_ffmpeg_remux_and_audio_only_fallback() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("atv-chunks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source.mkv");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=64x64:rate=10:duration=12",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=12",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-g",
                    "20",
                    "-bf",
                    "0",
                    "-c:a",
                    "aac",
                    "-y"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let meta = std::fs::metadata(&source).unwrap();
        let identity = SourceIdentity::new(
            source.to_str().unwrap(),
            meta.len() as i64,
            super::super::metadata_mtime(&meta),
        );
        let req = Request {
            source: identity.clone(),
            start_ms: 3_000,
            end_ms: 6_000,
        };
        let index = ChunkIndex::open(&dir).unwrap();
        let manifest = write_chunk(
            &req,
            &dir,
            &Budget,
            10_000_000,
            std::time::Duration::from_secs(15),
        )
        .expect("remux");
        assert!(
            index.covers(&identity, 3_000, 6_000),
            "published chunk must be visible to an already open resolver"
        );
        assert!(manifest.source_start_ms <= 3_000 && manifest.source_start_ms >= 1_900);
        let (path, seek) = manifest.resolve(&identity, 3_000, 6_000, &dir).unwrap();
        assert_eq!(manifest.version, 2);
        assert_eq!(
            manifest.payload_bytes,
            std::fs::metadata(&path).unwrap().len()
        );
        assert!(seek >= 900 && seek <= 1_100);
        assert!(manifest.resolve(&identity, 5_999, 6_001, &dir).is_none());
        assert!(
            manifest
                .resolve(
                    &SourceIdentity::new(identity.path.clone(), identity.size, 2),
                    3_000,
                    4_000,
                    &dir
                )
                .is_none()
        );
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-ss",
                    &format!("{:.3}", seek as f64 / 1000.0),
                    "-i"
                ])
                .arg(&path)
                .args([
                    "-t", "0.2", "-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"
                ])
                .status()
                .unwrap()
                .success()
        );
        let frame_hash = |file: &Path, at: f64| {
            let output = Command::new("ffmpeg")
                .args(["-v", "error", "-ss", &format!("{at:.3}"), "-i"])
                .arg(file)
                .args([
                    "-map",
                    "0:v:0",
                    "-frames:v",
                    "1",
                    "-f",
                    "hash",
                    "-hash",
                    "md5",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        assert_eq!(
            frame_hash(&source, 3.0),
            frame_hash(&path, seek as f64 / 1000.0)
        );
        let audio = dir.join("audio.mka");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=5",
                    "-c:a",
                    "aac",
                    "-y"
                ])
                .arg(&audio)
                .status()
                .unwrap()
                .success()
        );
        let meta = std::fs::metadata(&audio).unwrap();
        let audio_req = Request {
            source: SourceIdentity::new(
                audio.to_str().unwrap(),
                meta.len() as i64,
                super::super::metadata_mtime(&meta),
            ),
            start_ms: 1000,
            end_ms: 3000,
        };
        assert!(
            write_chunk(
                &audio_req,
                &dir,
                &Budget,
                10_000_000,
                std::time::Duration::from_secs(15)
            )
            .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_requests_extend_covered_heads_earliest_deadline_first() {
        let dir = std::env::temp_dir().join(format!("atv-edf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let a = SourceIdentity::new("/never-stat/a", 10, 2);
        let b = SourceIdentity::new("/never-stat/b", 10, 2);
        // Channel A's head was cut last tick at playhead 1_000_000 for 300 s.
        std::fs::write(dir.join("a1.mkv"), b"payload").unwrap();
        let m = Manifest {
            version: 2,
            payload_bytes: 7,
            source_path: a.path.clone(),
            source_size: 10,
            source_mtime: 2,
            chunk_file: "a1.mkv".into(),
            source_start_ms: 1_000_000,
            source_end_ms: 1_300_000,
            local_start_ms: 0,
        };
        std::fs::write(dir.join("a1.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        let index = ChunkIndex::open(&dir).unwrap();
        let air = |ch: &str, src: &SourceIdentity, start: i64, end: i64, in_ms: i64| Airing {
            airing_id: ch.into(),
            channel_id: ch.into(),
            media_id: ch.into(),
            source: src.clone(),
            in_ms,
            start_ms: start,
            end_ms: end,
        };
        let now = 10_060_000;
        // A: playhead advanced 60 s into source (1_060_000). B: head uncovered.
        let c = SourceIdentity::new("/never-stat/c", 10, 2);
        let rows = vec![
            air("a", &a, 9_000_000, 12_000_000, 0),
            air("b", &b, 10_000_000, 13_000_000, 0),
            air("b", &c, 13_000_000, 16_000_000, 0),
        ];
        let reqs = missing_requests(&targets(&rows, now), &index, 300_000, now);
        // Lookahead: nothing that airs more than 10 min out, except the next head.
        assert!(
            reqs.iter().all(|r| r.source != a || r.start_ms < 1_660_000),
            "current programme capped at lookahead"
        );
        let next: Vec<_> = reqs
            .iter()
            .filter(|r| r.source == c)
            .map(|r| (r.start_ms, r.end_ms))
            .collect();
        assert_eq!(
            next,
            vec![(0, 300_000), (300_000, 600_000)],
            "next programme head only"
        );
        assert!(
            reqs.iter().position(|r| r.source == c).unwrap()
                > reqs.iter().position(|r| r.source == a).unwrap(),
            "next head after current window"
        );
        assert_eq!(
            (reqs[0].source.path.as_str(), reqs[0].start_ms),
            ("/never-stat/b", 60_000),
            "uncovered head first"
        );
        let ext = reqs.iter().find(|r| r.source == a).unwrap();
        assert_eq!(
            (ext.start_ms, ext.end_ms),
            (1_300_000, 1_600_000),
            "covered head extends from its end, not the playhead"
        );
        assert!(
            reqs.iter()
                .all(|r| !(r.source == a && r.start_ms < 1_300_000))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_index_leases_and_gaps() {
        let dir = std::env::temp_dir().join(format!("atv-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let id = SourceIdentity::new("/never-stat/source", 10, 2);
        for (name, start, end) in [
            ("one", 1000, 2000),
            ("two", 2000, 3000),
            ("gap", 3100, 4000),
        ] {
            std::fs::write(dir.join(format!("{name}.mkv")), b"payload").unwrap();
            let m = Manifest {
                version: 2,
                payload_bytes: 7,
                source_path: id.path.clone(),
                source_size: id.size,
                source_mtime: id.mtime,
                chunk_file: format!("{name}.mkv"),
                source_start_ms: start,
                source_end_ms: end,
                local_start_ms: 0,
            };
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_vec(&m).unwrap(),
            )
            .unwrap();
        }
        let index = ChunkIndex::open(&dir).unwrap();
        assert!(index.covers(&id, 1500, 3000));
        assert!(!index.covers(&id, 1500, 3200));
        assert!(!index.covers(&SourceIdentity::new(&id.path, 10, 3), 1500, 2000));
        let lease = index.acquire(&id, 1500).unwrap();
        assert_eq!((lease.local_seek, lease.source_coverage_end), (500, 2000));
        assert!(
            !ChunkIndex::open(&dir)
                .unwrap()
                .evict(&dir.join("one.json"))
                .unwrap()
        );
        assert!(dir.join("one.mkv").exists());
        drop(lease);
        assert!(index.evict(&dir.join("one.json")).unwrap());
        drop(index);
        let restarted = ChunkIndex::open(&dir).unwrap();
        assert!(!restarted.covers(&id, 1500, 3000));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn readiness_distinguishes_startup_from_continuous_window() {
        let dir = std::env::temp_dir().join(format!("atv-readiness-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let id = SourceIdentity::new("source", 10, 2);
        std::fs::write(dir.join("one.mkv"), b"payload").unwrap();
        let manifest = Manifest {
            version: 2,
            payload_bytes: 7,
            source_path: id.path.clone(),
            source_size: id.size,
            source_mtime: id.mtime,
            chunk_file: "one.mkv".into(),
            source_start_ms: 600_000,
            source_end_ms: 900_000,
            local_start_ms: 0,
        };
        std::fs::write(dir.join("one.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
        let index = ChunkIndex::open(&dir).unwrap();
        assert_eq!(index.readiness(&id, 600_000, 1_200_000), (true, false));
        assert_eq!(index.readiness(&id, 600_000, 900_000), (true, true));
        assert_eq!(
            index.readiness(&SourceIdentity::new("source", 10, 3), 600_000, 900_000),
            (false, false)
        );
        std::fs::write(dir.join("one.mkv"), b"short").unwrap();
        assert_eq!(index.readiness(&id, 600_000, 900_000), (false, false));
        std::fs::remove_file(dir.join("one.mkv")).unwrap();
        assert_eq!(index.readiness(&id, 600_000, 900_000), (false, false));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_rejects_legacy_and_truncated_payloads() {
        let dir = std::env::temp_dir().join(format!("atv-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("old.mkv"), b"payload").unwrap();
        std::fs::write(dir.join("old.json"), br#"{"version":1,"source_path":"source","source_size":1,"source_mtime":2,"chunk_file":"old.mkv","source_start_ms":0,"source_end_ms":1000,"local_start_ms":0}"#).unwrap();
        let mut m = Manifest {
            version: 2,
            payload_bytes: 7,
            source_path: "source".into(),
            source_size: 1,
            source_mtime: 2,
            chunk_file: "new.mkv".into(),
            source_start_ms: 0,
            source_end_ms: 1000,
            local_start_ms: 0,
        };
        std::fs::write(dir.join("new.mkv"), b"short").unwrap();
        std::fs::write(dir.join("new.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        let id = SourceIdentity::new("source", 1, 2);
        let index = ChunkIndex::open(&dir).unwrap();
        assert!(!index.covers(&id, 0, 1000));
        drop(index);
        m.payload_bytes = 5;
        std::fs::write(dir.join("new.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(ChunkIndex::open(&dir).unwrap().covers(&id, 0, 1000));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn adjacent_boundaries_and_gap_are_rejected() {
        let dir = std::env::temp_dir().join(format!("atv-manifest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("one.mkv"), b"x").unwrap();
        let id = SourceIdentity::new("source", 1, 2);
        let m = Manifest {
            version: 2,
            payload_bytes: 1,
            source_path: "source".into(),
            source_size: 1,
            source_mtime: 2,
            chunk_file: "one.mkv".into(),
            source_start_ms: 1000,
            source_end_ms: 2000,
            local_start_ms: 0,
        };
        assert_eq!(m.resolve(&id, 1999, 2000, &dir).unwrap().1, 999);
        assert!(m.resolve(&id, 2000, 2001, &dir).is_none());
        assert!(m.resolve(&id, 1999, 2001, &dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
