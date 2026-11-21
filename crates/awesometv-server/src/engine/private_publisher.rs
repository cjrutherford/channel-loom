//! Private building blocks for the future channel publisher. Not wired to routes.
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

fn monotonic_ms() -> u128 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
}

const TICKS: i64 = 90_000;

#[derive(Debug)]
struct Segment {
    name: String,
    duration_ticks: i64,
}

// Convert the decimal directly, rounding once to the nearest 90 kHz tick.
fn decimal_ticks(value: &str) -> Result<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 9
    {
        bail!("invalid EXTINF duration");
    }
    let scale = 10_i128.pow(fraction.len() as u32);
    let whole: i128 = whole.parse()?;
    let fraction: i128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse()?
    };
    let ticks = (whole
        .checked_mul(scale)
        .and_then(|v| v.checked_add(fraction))
        .and_then(|v| v.checked_mul(TICKS as i128))
        .context("duration overflow")?
        + scale / 2)
        / scale;
    let ticks = i64::try_from(ticks)?;
    if ticks <= 0 {
        bail!("nonpositive EXTINF");
    }
    Ok(ticks)
}

// Validate a complete, bounded segment-muxer record before making payload visible.
// The worker supplies the expected sequence and previous end; FIFO names wrap at two.
#[allow(dead_code)]
fn fifo_record(record: &str, sequence: usize, previous_end: Option<i64>) -> Result<(i64, i64)> {
    if record.len() > 512 || !record.ends_with('\n') {
        bail!("incomplete or oversized segment record");
    }
    let mut fields = record.trim_end_matches('\n').split(',');
    let name = fields.next().context("missing segment name")?;
    if name != format!("slot_{}.ts", sequence % 2) {
        bail!("unexpected segment slot");
    }
    let parse_time = |value: &str| -> Result<i64> {
        if value.bytes().all(|b| b == b'0' || b == b'.') && value.contains('0') {
            // Still validate decimal syntax, including the zero value.
            let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
            if whole.is_empty() || fraction.len() > 9 || fraction.contains('.') {
                bail!("invalid timestamp");
            }
            Ok(0)
        } else {
            decimal_ticks(value)
        }
    };
    let start = parse_time(fields.next().context("missing start")?)?;
    let end = parse_time(fields.next().context("missing end")?)?;
    if fields.next().is_some() || end <= start || previous_end.is_some_and(|v| v != start) {
        bail!("invalid segment times");
    }
    Ok((start, end))
}

fn parse_private_playlist(text: &str) -> Result<Vec<Segment>> {
    let mut lines = text.lines();
    if lines.next() != Some("#EXTM3U") {
        bail!("missing playlist header");
    }
    let mut pending = None;
    let mut segments = Vec::new();
    for line in lines {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            if pending.is_some() {
                bail!("missing segment URI");
            }
            pending = Some(decimal_ticks(
                value.split_once(',').context("invalid EXTINF")?.0,
            )?);
        } else if !line.is_empty() && !line.starts_with('#') {
            let duration_ticks = pending.take().context("segment without EXTINF")?;
            let valid = line
                .strip_prefix("seg_")
                .and_then(|s| s.strip_suffix(".ts"))
                .is_some_and(|s| s.len() == 9 && s.bytes().all(|b| b.is_ascii_digit()));
            if !valid {
                bail!("invalid private segment name");
            }
            segments.push(Segment {
                name: line.to_owned(),
                duration_ticks,
            });
        }
    }
    if pending.is_some() {
        bail!("missing segment URI");
    }
    Ok(segments)
}

// A snapshot is bounded before parsing; a cursor older than the window is an
// error, never an implicit skip. The caller must keep unconsumed files and
// prevent the producer from advancing past the cursor.
fn private_window(
    text: &str,
    next: usize,
    max_bytes: usize,
    max_segments: usize,
) -> Result<(Vec<Segment>, usize)> {
    if text.len() > max_bytes
        || text
            .lines()
            .filter(|line| line.starts_with("#EXTINF:"))
            .count()
            > max_segments
    {
        bail!("private playlist exceeds window limit");
    }
    let mut sequence = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if sequence.is_some() {
                bail!("duplicate media sequence");
            }
            sequence = Some(value.parse::<usize>().context("invalid media sequence")?);
        }
    }
    let first = sequence.unwrap_or(0);
    let entries = parse_private_playlist(text)?;
    let end = first
        .checked_add(entries.len())
        .context("media sequence overflow")?;
    if next < first {
        bail!("private playlist skipped unconsumed segments");
    }
    if next > end {
        bail!("private playlist regressed");
    }
    for (offset, entry) in entries.iter().enumerate() {
        let number = first
            .checked_add(offset)
            .context("media sequence overflow")?;
        if entry.name != format!("seg_{number:09}.ts") {
            bail!("private playlist out of sequence");
        }
    }
    Ok((entries, next - first))
}

// Owns private encoder children and their generation directories. The caller must
// supply FFmpeg input/codec arguments; this prototype never writes public HLS.
struct PrivateSpool {
    root: PathBuf,
    items: Vec<PrivateItem>,
}

struct PrivateItem {
    dir: PathBuf,
    child: Option<std::process::Child>,
    #[cfg(unix)]
    fifo: Option<super::fifo_spool::FifoSpool>,
    _cache_lease: Option<super::InputLease>,
    next_segment: usize,
    playlist_seen: bool,
    paused: bool,
    last_progress: std::time::Instant,
    progress_timeout: std::time::Duration,
}

enum TakeReady {
    Pending,
    Segment(SegmentLease),
    End,
    Failed,
}

struct CompletedGeneration {
    dir: PathBuf,
    entries: Vec<Segment>,
}

// A taken segment lives outside its generation directory, so cancelling the
// encoder cannot unlink bytes while the publisher is using them. The future
// quota controller must keep these bytes charged until the lease is released.
struct SegmentLease {
    path: PathBuf,
    duration_ticks: i64,
    #[cfg(unix)]
    _fifo: Option<super::fifo_spool::Segment>,
}

impl SegmentLease {
    fn take(root: &Path, generation: &Path, segment: &Segment) -> Result<Self> {
        let path = root.join(format!("lease_{}.ts", uuid::Uuid::new_v4().simple()));
        std::fs::rename(generation.join(&segment.name), &path)
            .context("transfer completed private segment")?;
        Ok(Self {
            path,
            duration_ticks: segment.duration_ticks,
            #[cfg(unix)]
            _fifo: None,
        })
    }
}

impl Drop for SegmentLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for CompletedGeneration {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// A slot remains charged until waitpid confirms exit, even after cancellation.
const MAX_ENCODERS: usize = 16;
static ENCODERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn reserve_encoder(slots: &std::sync::atomic::AtomicUsize, limit: usize) -> bool {
    slots
        .fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| (n < limit).then_some(n + 1),
        )
        .is_ok()
}

fn preparation_cursor(anchor: i64, ticks: i64) -> i64 {
    anchor.saturating_add(ticks.saturating_add(89) / 90)
}

// 90 kHz transport clock, 30000/1001 frames per second: exactly 3003 ticks.
fn frame_grid_end(anchor_ms: i64, end_ms: i64) -> i64 {
    let ticks = (end_ms as i128 - anchor_ms as i128) * 90;
    (ticks.div_euclid(3003) * 3003
        + if ticks.rem_euclid(3003) >= 1502 {
            3003
        } else {
            0
        }) as i64
}

fn frames_to_end(actual_ticks: i64, end_ticks: i64) -> i64 {
    (end_ticks.saturating_sub(actual_ticks).max(0) + 1501) / 3003
}

struct RetiredEncoder {
    child: Option<std::process::Child>,
    #[cfg(unix)]
    worker: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    _lease: Option<super::InputLease>,
}

fn reaper() -> &'static std::sync::mpsc::Sender<RetiredEncoder> {
    static REAPER: std::sync::OnceLock<std::sync::mpsc::Sender<RetiredEncoder>> =
        std::sync::OnceLock::new();
    REAPER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<RetiredEncoder>();
        std::thread::Builder::new()
            .name("encoder-reaper".into())
            .spawn(move || {
                let mut retired: Vec<RetiredEncoder> = Vec::new();
                loop {
                    match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(child) => retired.push(child),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let mut i = 0;
                    while i < retired.len() {
                        if retired[i]
                            .child
                            .as_mut()
                            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
                            || {
                                #[cfg(unix)]
                                {
                                    retired[i]
                                        .worker
                                        .as_ref()
                                        .is_some_and(|worker| worker.is_finished())
                                }
                                #[cfg(not(unix))]
                                {
                                    false
                                }
                            }
                        {
                            #[cfg(unix)]
                            if let Some(worker) = retired[i].worker.take() {
                                let _ = worker.join();
                            }
                            let done = retired.swap_remove(i);
                            if !done.dir.as_os_str().is_empty() {
                                let _ = std::fs::remove_dir_all(&done.dir);
                            }
                            drop(done);
                            ENCODERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        } else {
                            i += 1;
                        }
                    }
                }
            })
            .expect("start encoder reaper");
        tx
    })
}

impl Drop for PrivateItem {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(fifo) = self.fifo.take() {
            let retired = RetiredEncoder {
                child: None,
                worker: Some(fifo.retire()),
                dir: PathBuf::new(),
                _lease: self._cache_lease.take(),
            };
            if let Err(error) = reaper().send(retired) {
                std::mem::forget(error.0);
            }
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        if !matches!(child.try_wait(), Ok(Some(_))) {
            #[cfg(unix)]
            unsafe {
                unsafe extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                let _ = kill(-(child.id() as i32), 9);
            }
            #[cfg(not(unix))]
            let _ = child.kill();
        }
        let retired = RetiredEncoder {
            child: Some(child),
            #[cfg(unix)]
            worker: None,
            dir: std::mem::take(&mut self.dir),
            _lease: self._cache_lease.take(),
        };
        // If the reaper dies, leak ownership and the admission slot rather than
        // release a lease or remove files while the child may still be alive.
        if let Err(error) = reaper().send(retired) {
            std::mem::forget(error.0);
        }
    }
}

#[cfg(unix)]
impl PrivateItem {
    fn start_fifo(
        dir: PathBuf,
        command: std::process::Command,
        lease: Option<super::InputLease>,
    ) -> Result<Self> {
        let _ = reaper();
        if !reserve_encoder(&ENCODERS, MAX_ENCODERS) {
            bail!("private encoder capacity exhausted; refusing new encoder");
        }
        let fifo = match super::fifo_spool::FifoSpool::spawn_command(&dir, command) {
            Ok(fifo) => fifo,
            Err(error) => {
                ENCODERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(error.into());
            }
        };
        Ok(Self {
            dir,
            child: None,
            fifo: Some(fifo),
            _cache_lease: lease,
            next_segment: 0,
            playlist_seen: false,
            paused: false,
            last_progress: std::time::Instant::now(),
            progress_timeout: std::time::Duration::from_secs(22),
        })
    }
    fn try_recv_fifo(
        &self,
    ) -> Result<std::io::Result<super::fifo_spool::Segment>, std::sync::mpsc::TryRecvError> {
        self.fifo.as_ref().expect("FIFO item").try_recv()
    }
}

impl PrivateSpool {
    fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            items: Vec::new(),
        })
    }

    fn start_bounded(
        &mut self,
        command: std::process::Command,
        segment_seconds: &str,
        duration_ms: u64,
    ) -> Result<()> {
        self.start_bounded_leased(command, segment_seconds, duration_ms, None)
    }

    fn start_bounded_leased(
        &mut self,
        mut command: std::process::Command,
        segment_seconds: &str,
        duration_ms: u64,
        lease: Option<super::InputLease>,
    ) -> Result<()> {
        if duration_ms == 0 {
            bail!("private encoding horizon must be positive");
        }
        if self.items.len() >= 2 {
            bail!("private current/next queue full");
        }
        #[cfg(unix)]
        {
            let _ = segment_seconds;
            let dir = self.root.join(uuid::Uuid::new_v4().simple().to_string());
            command
                .arg("-t")
                .arg(format!("{}.{:03}", duration_ms / 1000, duration_ms % 1000))
                .stdin(std::process::Stdio::null());
            let item = PrivateItem::start_fifo(dir, command, lease)?;
            self.items.push(item);
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            let _ = (command, segment_seconds, lease);
            bail!("private FIFO spool requires Unix")
        }
    }

    fn start_frames_leased(
        &mut self,
        mut command: std::process::Command,
        frames: i64,
        lease: Option<super::InputLease>,
    ) -> Result<()> {
        if frames <= 0 || self.items.len() >= 2 {
            bail!("invalid private frame horizon");
        }
        #[cfg(unix)]
        {
            let dir = self.root.join(uuid::Uuid::new_v4().simple().to_string());
            command
                .arg("-frames:v")
                .arg(frames.to_string())
                .stdin(std::process::Stdio::null());
            self.items
                .push(PrivateItem::start_fifo(dir, command, lease)?);
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (command, lease);
            bail!("private FIFO spool requires Unix")
        }
    }

    fn take_ready_segment(&mut self) -> Result<TakeReady> {
        let Some(item) = self.items.first_mut() else {
            return Ok(TakeReady::End);
        };
        #[cfg(unix)]
        if item.fifo.is_some() {
            return match item.try_recv_fifo() {
                Ok(Ok(segment)) => {
                    let start = (segment.start * TICKS as f64).round() as i64;
                    let end = (segment.end * TICKS as f64).round() as i64;
                    if !segment.start.is_finite() || !segment.end.is_finite() || end <= start {
                        bail!("invalid FIFO segment timestamps");
                    }
                    Ok(TakeReady::Segment(SegmentLease {
                        path: segment.path.clone(),
                        duration_ticks: end - start,
                        _fifo: Some(segment),
                    }))
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "private FIFO encoder failed");
                    Ok(TakeReady::Failed)
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => Ok(TakeReady::Pending),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.items.remove(0);
                    Ok(TakeReady::End)
                }
            };
        }
        let status = item
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .context("poll private encoder")?;
        let text = match std::fs::read_to_string(item.dir.join("index.m3u8")) {
            Ok(text) => text,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && status.is_some_and(|s| !s.success()) =>
            {
                return Ok(TakeReady::Failed);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && status.is_none() => {
                return Ok(if item.last_progress.elapsed() >= item.progress_timeout {
                    tracing::warn!(
                        pid = item.child.as_ref().unwrap().id(),
                        "private encoder stalled without a closed segment"
                    );
                    TakeReady::Failed
                } else {
                    TakeReady::Pending
                });
            }
            Err(error) => return Err(error).context("read private playlist"),
        };
        let (entries, offset) = private_window(&text, item.next_segment, usize::MAX, usize::MAX)?;
        if !item.playlist_seen {
            tracing::info!(
                monotonic_ms = monotonic_ms(),
                pid = item.child.as_ref().unwrap().id(),
                "private playlist atomic appearance observed"
            );
            item.playlist_seen = true;
        }
        if let Some(entry) = entries.get(offset) {
            tracing::info!(monotonic_ms = monotonic_ms(), pid = item.child.as_ref().unwrap().id(), segment = %entry.name, "closed private segment observed and taken");
            let lease = SegmentLease::take(&self.root, &item.dir, entry)?;
            item.next_segment += 1;
            item.last_progress = std::time::Instant::now();
            return Ok(TakeReady::Segment(lease));
        }
        if let Some(status) = status {
            if !status.success() {
                tracing::warn!(%status, "private encoder failed after its closed segments were consumed");
                return Ok(TakeReady::Failed);
            }
            if !text.lines().any(|line| line == "#EXT-X-ENDLIST") {
                bail!("unfinished private playlist");
            }
            self.items.remove(0);
            return Ok(TakeReady::End);
        }
        if item.last_progress.elapsed() >= item.progress_timeout {
            tracing::warn!(
                pid = item.child.as_ref().unwrap().id(),
                "private encoder stalled without a closed segment"
            );
            return Ok(TakeReady::Failed);
        }
        Ok(TakeReady::Pending)
    }

    // Only a completed, successful encoder's referenced segments are exposed.
    #[cfg(test)]
    fn finish_current(&mut self) -> Result<CompletedGeneration> {
        let dir = self
            .root
            .join(format!("collected_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir)?;
        let mut entries = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            match self.take_ready_segment()? {
                TakeReady::Segment(segment) => {
                    let name = format!("seg_{:09}.ts", entries.len());
                    std::fs::copy(&segment.path, dir.join(&name))?;
                    entries.push(Segment {
                        name,
                        duration_ticks: segment.duration_ticks,
                    });
                }
                TakeReady::Pending => {
                    if std::time::Instant::now() >= deadline {
                        bail!("FIFO drain timed out");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                TakeReady::End => return Ok(CompletedGeneration { dir, entries }),
                TakeReady::Failed => bail!("private encoder failed"),
            }
        }
    }
}

impl Drop for PrivateSpool {
    fn drop(&mut self) {
        self.items.clear();
    }
}

#[derive(Clone)]
struct Published {
    sequence: u64,
    name: String,
    duration_ticks: i64,
    date_ticks: i64,
    discontinuity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ScheduledProgramme {
    pub id: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum SegmentSource {
    Media(String),
    Slate,
    Unknown,
    ScheduledMedia {
        id: String,
        scheduled_programme: ScheduledProgramme,
    },
    Fallback {
        #[serde(default)]
        fallback_reason: Option<String>,
        #[serde(default)]
        scheduled_programme: Option<ScheduledProgramme>,
    },
}
impl SegmentSource {
    fn fallback(reason: &str, scheduled_programme: Option<ScheduledProgramme>) -> Self {
        Self::Fallback {
            fallback_reason: Some(reason.into()),
            scheduled_programme,
        }
    }
    fn is_slate(&self) -> bool {
        matches!(self, Self::Slate | Self::Fallback { .. })
    }
    fn programme(&self) -> Option<ScheduledProgramme> {
        match self {
            Self::ScheduledMedia {
                scheduled_programme,
                ..
            } => Some(scheduled_programme.clone()),
            Self::Fallback {
                scheduled_programme,
                ..
            } => scheduled_programme.clone(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SegmentIdentity {
    pub sequence: u64,
    pub start_pdt_ns: i128,
    pub end_pdt_ns: i128,
    pub start_90k: i128,
    pub end_90k: i128,
    pub source: SegmentSource,
}

// State is committed only after the playlist advertises the new segment.
#[derive(Clone)]
struct TransportCounters {
    last: [Option<u8>; 8192],
}
impl Default for TransportCounters {
    fn default() -> Self {
        Self { last: [None; 8192] }
    }
}
impl TransportCounters {
    fn normalize(&mut self, packet: &mut [u8]) -> Result<()> {
        if packet.len() != 188 || packet[0] != 0x47 {
            bail!("invalid TS framing or sync");
        }
        if packet[1] & 0x80 != 0 {
            bail!("TS transport error indicator");
        }
        let pid = (((packet[1] as usize) & 0x1f) << 8) | packet[2] as usize;
        let control = (packet[3] >> 4) & 3;
        if control == 0 {
            bail!("invalid TS adaptation control");
        }
        if control & 2 != 0 {
            let len = packet[4] as usize;
            if len > 183 || (control == 2 && len != 183) || (control == 3 && len > 182) {
                bail!("invalid TS adaptation length");
            }
            if len > 0 && packet[5] & 0x80 != 0 {
                self.last[pid] = None;
            }
        }
        // Null packets have no continuity requirement.
        if pid == 0x1fff {
            return Ok(());
        }
        let cc = match self.last[pid] {
            Some(last) if control & 1 != 0 => (last + 1) & 15,
            Some(last) => last,
            None => packet[3] & 15,
        };
        packet[3] = (packet[3] & 0xf0) | cc;
        self.last[pid] = Some(cc);
        Ok(())
    }
}

fn normalize_segment(path: &Path, counters: &mut TransportCounters) -> Result<()> {
    let mut bytes = std::fs::read(path)?;
    if bytes.is_empty() || bytes.len() % 188 != 0 {
        bail!("invalid TS segment length");
    }
    for packet in bytes.chunks_exact_mut(188) {
        counters.normalize(packet)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

pub(super) struct Publisher {
    dir: PathBuf,
    anchor_ms: i64,
    cursor: i64,
    next_sequence: u64,
    discontinuity_sequence: u64,
    keep: usize,
    entries: Vec<Published>,
    identities: VecDeque<SegmentIdentity>,
    retired: Vec<(String, i64)>,
    resume_boundary: bool,
    // Set when a resumed playlist only advertises an expired schedule interval.
    stale_prefix: bool,
    transport_counters: TransportCounters,
}

impl Publisher {
    fn new(dir: PathBuf, anchor_ms: i64, keep: usize) -> Self {
        Self {
            dir,
            anchor_ms,
            cursor: 0,
            next_sequence: 0,
            discontinuity_sequence: 0,
            keep: keep.clamp(4, 60),
            entries: Vec::new(),
            identities: VecDeque::new(),
            retired: Vec::new(),
            resume_boundary: false,
            stale_prefix: false,
            transport_counters: TransportCounters::default(),
        }
    }

    // Recover the exact advertised window before appending after a task restart.
    fn resume(dir: PathBuf, start_ms: i64, keep: usize) -> Result<Self> {
        let text = match std::fs::read_to_string(dir.join("index.m3u8")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::new(dir, start_ms, keep));
            }
            Err(e) => return Err(e.into()),
        };
        let mut sequence = None;
        let mut discontinuity_sequence = None;
        let mut date = None;
        let mut duration = None;
        let mut discontinuity = false;
        let mut publisher = Self::new(dir, 0, keep);
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                sequence = Some(v.parse::<u64>()?);
            } else if let Some(v) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
                discontinuity_sequence = Some(v.parse::<u64>()?);
            } else if line == "#EXT-X-DISCONTINUITY" {
                discontinuity = true;
            } else if let Some(v) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
                // FFmpeg emits ISO-8601 numeric offsets without the RFC 3339 colon.
                let mut normalized = v.to_owned();
                let len = normalized.len();
                if len >= 5
                    && matches!(normalized.as_bytes()[len - 5], b'+' | b'-')
                    && normalized.as_bytes()[len - 4..]
                        .iter()
                        .all(u8::is_ascii_digit)
                {
                    normalized.insert(len - 2, ':');
                }
                date = Some(
                    time::OffsetDateTime::parse(
                        &normalized,
                        &time::format_description::well_known::Rfc3339,
                    )?
                    .unix_timestamp_nanos(),
                );
            } else if let Some(v) = line.strip_prefix("#EXTINF:") {
                duration = Some(decimal_ticks(
                    v.split_once(',').context("invalid public EXTINF")?.0,
                )?);
            } else if !line.is_empty() && !line.starts_with('#') {
                let valid_legacy = line
                    .strip_prefix("seg_")
                    .and_then(|v| v.strip_suffix(".ts"))
                    .is_some_and(|v| {
                        !v.is_empty()
                            && v.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    });
                let valid_current = line
                    .strip_prefix("segment_")
                    .and_then(|v| v.strip_suffix(".ts"))
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok());
                if !valid_legacy && !valid_current {
                    bail!("invalid public segment name");
                }
                if !publisher.dir.join(line).is_file() {
                    bail!("advertised public segment missing");
                }
                let nanos = date.take().context("missing public PDT")?;
                let ticks = duration.take().context("missing public duration")?;
                if publisher.entries.is_empty() {
                    publisher.anchor_ms = i64::try_from(nanos / 1_000_000)?;
                }
                let expected = publisher.anchor_ms as i128 * 1_000_000
                    + (publisher.cursor as i128 * 1_000_000_000 + 89_999) / 90_000;
                // A restart may explicitly skip an expired schedule interval.
                // Only a forward jump at a discontinuity is valid; ordinary
                // segments still require continuous programme dates.
                if (nanos - expected).abs() > 1_000_000 && !(discontinuity && nanos > expected) {
                    bail!("public PDT is not continuous");
                }
                let seq = sequence
                    .context("missing public sequence")?
                    .checked_add(publisher.entries.len() as u64)
                    .context("sequence overflow")?;
                let relative_nanos = nanos - publisher.anchor_ms as i128 * 1_000_000;
                let date_ticks =
                    i64::try_from((relative_nanos * TICKS as i128 + 500_000_000) / 1_000_000_000)?;
                publisher.entries.push(Published {
                    sequence: seq,
                    name: line.into(),
                    duration_ticks: ticks,
                    date_ticks,
                    discontinuity,
                });
                publisher.cursor = publisher
                    .cursor
                    .max(date_ticks.checked_add(ticks).context("cursor overflow")?);
                discontinuity = false;
            }
        }
        if publisher.entries.is_empty() || date.is_some() || duration.is_some() {
            bail!("invalid public playlist");
        }
        publisher.next_sequence = publisher
            .entries
            .last()
            .unwrap()
            .sequence
            .checked_add(1)
            .context("sequence overflow")?;
        // RFC 8216 defines an omitted discontinuity sequence as zero, even
        // when a legacy sliding window has lost its earlier history. Preserve
        // that advertised numbering and start a distinct encoder boundary.
        publisher.discontinuity_sequence = discontinuity_sequence.unwrap_or(0);
        let persisted: std::collections::HashMap<String, SegmentSource> =
            std::fs::read(publisher.dir.join("segment-identities.json"))
                .ok()
                .and_then(|data| serde_json::from_slice(&data).ok())
                .unwrap_or_default();
        publisher.identities = publisher
            .entries
            .iter()
            .map(|entry| {
                publisher.identity(
                    entry,
                    persisted
                        .get(&entry.name)
                        .cloned()
                        .unwrap_or(SegmentSource::Unknown),
                )
            })
            .collect();
        publisher.resume_boundary = true;
        Ok(publisher)
    }

    fn identity(&self, entry: &Published, source: SegmentSource) -> SegmentIdentity {
        let pdt = |ticks: i64| {
            self.anchor_ms as i128 * 1_000_000
                + (ticks as i128 * 1_000_000_000 + TICKS as i128 - 1) / TICKS as i128
        };
        SegmentIdentity {
            sequence: entry.sequence,
            start_pdt_ns: pdt(entry.date_ticks),
            end_pdt_ns: pdt(entry.date_ticks + entry.duration_ticks),
            start_90k: self.anchor_ms as i128 * 90 + entry.date_ticks as i128,
            end_90k: self.anchor_ms as i128 * 90
                + entry.date_ticks as i128
                + entry.duration_ticks as i128,
            source,
        }
    }

    pub(super) fn snapshot(&self) -> Vec<SegmentIdentity> {
        self.identities.iter().cloned().collect()
    }

    // Caller supplies the persisted item's start, never the current wall time.
    // A disagreement above tolerance means continuity cannot be established.
    fn publish(
        &mut self,
        source: &Path,
        duration: i64,
        item_start_ms: Option<i64>,
        now_ms: i64,
        tolerance_ticks: i64,
        source_identity: SegmentSource,
    ) -> Result<bool> {
        if duration <= 0 || tolerance_ticks < 0 {
            bail!("invalid duration or tolerance");
        }
        let boundary = if let Some(start) = item_start_ms {
            let scheduled = start
                .checked_sub(self.anchor_ms)
                .and_then(|v| v.checked_mul(90))
                .context("schedule overflow")?;
            if (scheduled as i128 - self.cursor as i128).abs() > tolerance_ticks as i128 {
                bail!("item boundary differs from published cursor");
            }
            true
        } else {
            false
        };
        let end = self
            .cursor
            .checked_add(duration)
            .context("cursor overflow")?;
        let now_ticks = now_ms
            .checked_sub(self.anchor_ms)
            .and_then(|v| v.checked_mul(90))
            .context("clock overflow")?;
        if end > now_ticks {
            return Ok(false);
        }
        let name = format!("segment_{}.ts", uuid::Uuid::new_v4().simple());
        let tmp_segment = self.dir.join(format!(".{name}.tmp"));
        let final_segment = self.dir.join(&name);
        if let Err(error) = std::fs::copy(source, &tmp_segment) {
            let _ = std::fs::remove_file(&tmp_segment);
            return Err(error).context("copy closed private segment");
        }
        let mut counters = if boundary || self.resume_boundary {
            TransportCounters::default()
        } else {
            self.transport_counters.clone()
        };
        if let Err(error) = normalize_segment(&tmp_segment, &mut counters) {
            let _ = std::fs::remove_file(&tmp_segment);
            return Err(error).context("normalize closed TS segment");
        }
        if let Err(error) = std::fs::rename(&tmp_segment, &final_segment) {
            let _ = std::fs::remove_file(&tmp_segment);
            return Err(error.into());
        }
        tracing::info!(
            monotonic_ms = monotonic_ms(),
            deadline_ms = self.anchor_ms + end / 90,
            "public segment renamed"
        );
        let mut entries = self.entries.clone();
        entries.push(Published {
            sequence: self.next_sequence,
            name,
            duration_ticks: duration,
            date_ticks: self.cursor,
            discontinuity: (boundary || self.resume_boundary) && self.next_sequence != 0,
        });
        let mut discont_sequence = self.discontinuity_sequence;
        let mut retired = Vec::new();
        // Keep the complete advertised prefix on the first resumed publication.
        // Thereafter shrink oversized legacy windows gradually.
        // An expired prefix is hours old: a joining viewer would start inside
        // it, so retire all of it once the first current segment exists.
        let limit = if self.stale_prefix {
            usize::MAX
        } else if self.resume_boundary {
            0
        } else {
            2
        };
        let floor = if self.stale_prefix { 1 } else { self.keep };
        let mut removed = 0;
        while entries.len() > floor && removed < limit {
            let old = entries.remove(0);
            removed += 1;
            // Legacy files are retained through migration, including after eviction.
            if !old.name.starts_with("seg_") {
                retired.push(old.name);
            }
            if old.discontinuity {
                discont_sequence += 1;
            }
        }
        // Write identities first, retaining the current advertised window if the
        // subsequent playlist rename fails. Keys are unique segment filenames.
        let mut persisted: std::collections::HashMap<String, SegmentSource> = self
            .entries
            .iter()
            .zip(&self.identities)
            .map(|(entry, identity)| (entry.name.clone(), identity.source.clone()))
            .collect();
        persisted.insert(
            entries.last().unwrap().name.clone(),
            source_identity.clone(),
        );
        let identity_tmp = self.dir.join(".segment-identities.json.tmp");
        if let Err(error) = std::fs::write(&identity_tmp, serde_json::to_vec(&persisted)?)
            .and_then(|_| std::fs::rename(&identity_tmp, self.dir.join("segment-identities.json")))
        {
            let _ = std::fs::remove_file(&final_segment);
            return Err(error.into());
        }
        let result = self.write_playlist(&entries, discont_sequence);
        if let Err(error) = result {
            let _ = std::fs::remove_file(final_segment);
            return Err(error);
        }
        let advertised_ms = (entries.iter().map(|e| e.duration_ticks).sum::<i64>() + 89) / 90;
        self.retired.extend(retired.into_iter().map(|name| {
            (
                name,
                now_ms.saturating_add(30_000).saturating_add(advertised_ms),
            )
        }));
        let identity = self.identity(
            entries.last().expect("newly published segment"),
            source_identity,
        );
        self.identities.push_back(identity);
        while self
            .identities
            .front()
            .is_some_and(|identity| identity.sequence < entries[0].sequence)
        {
            self.identities.pop_front();
        }
        self.entries = entries;
        self.transport_counters = counters;
        self.resume_boundary = false;
        self.stale_prefix = false;
        self.discontinuity_sequence = discont_sequence;
        self.next_sequence += 1;
        self.cursor = end;
        Ok(true)
    }

    fn cleanup(&mut self, now_ms: i64) -> Result<()> {
        let mut index = 0;
        while index < self.retired.len() {
            let (name, deadline) = &self.retired[index];
            if now_ms >= *deadline && !self.entries.iter().any(|e| e.name == *name) {
                match std::fs::remove_file(self.dir.join(name)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                self.retired.remove(index);
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    fn write_playlist(&self, entries: &[Published], discont_sequence: u64) -> Result<()> {
        use std::fmt::Write;
        let target = entries
            .iter()
            .map(|e| (e.duration_ticks + TICKS - 1) / TICKS)
            .max()
            .unwrap_or(1);
        let mut text = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-DISCONTINUITY-SEQUENCE:{discont_sequence}\n",
            entries[0].sequence
        );
        for e in entries {
            if e.discontinuity {
                text.push_str("#EXT-X-DISCONTINUITY\n");
            }
            let nanos = (self.anchor_ms as i128 * 1_000_000)
                + ((e.date_ticks as i128 * 1_000_000_000 + TICKS as i128 - 1) / TICKS as i128);
            let date = time::OffsetDateTime::from_unix_timestamp_nanos(nanos)?
                .format(&time::format_description::well_known::Rfc3339)?;
            let seconds = e.duration_ticks / TICKS;
            let fraction = (e.duration_ticks % TICKS) * 1_000_000_000 / TICKS;
            writeln!(
                text,
                "#EXT-X-PROGRAM-DATE-TIME:{date}\n#EXTINF:{seconds}.{fraction:09},\n{}",
                e.name
            )?;
        }
        let tmp = self
            .dir
            .join(format!(".playlist-{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, self.dir.join("index.m3u8")));
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        } else {
            tracing::info!(monotonic_ms = monotonic_ms(), "public playlist renamed");
        }
        result.context("publish playlist")
    }
}

// Slate uses the same bounded spool and publication lifecycle as media inputs.
fn slate_command(channel: &crate::db::Channel) -> std::process::Command {
    let mut command = std::process::Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c=0x0b1120:s={}x{}:r=30000/1001", channel.stream.width, channel.stream.height))
        .args(["-f", "lavfi", "-i", "anullsrc=channel_layout=stereo:sample_rate=48000",
            "-vf", "drawtext=text='PLEASE STAND BY':fontcolor=white:fontsize=22:x=(w-text_w)/2:y=(h-text_h)/2",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "60",
            "-c:a", "aac", "-ar", "48000", "-ac", "2"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    command
}

fn preparation_allowed(cursor_ms: i64, now_ms: i64, prebuffer_seconds: i64) -> bool {
    let burst_ms = prebuffer_seconds.clamp(0, 30).saturating_mul(1000);
    cursor_ms.saturating_sub(now_ms) <= 10_000 + burst_ms
}

// The future is dropped on deadline or stop; cancellation prevents new cache admissions.
async fn resolve_with_deadline<T, F, Fut>(
    stop: &std::sync::atomic::AtomicBool,
    deadline: std::time::Duration,
    resolve: F,
) -> Option<T>
where
    F: FnOnce(crate::cloud::manager::ResolutionTicket) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    use std::sync::atomic::Ordering;
    if stop.load(Ordering::Relaxed) {
        return None;
    }
    let ticket = crate::cloud::manager::ResolutionTicket::new();
    struct CancelTicket(crate::cloud::manager::ResolutionTicket);
    impl Drop for CancelTicket {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel = CancelTicket(ticket.clone());
    let future = resolve(ticket.clone());
    tokio::pin!(future);
    let timer = tokio::time::sleep(deadline);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            biased;
            _ = &mut timer => break,
            result = &mut future => {
                if !stop.load(Ordering::Relaxed) { return result.ok(); }
                break;
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if stop.load(Ordering::Relaxed) { break; }
            }
        }
    }
    drop(future);
    ticket.cancel();
    None
}

async fn resolve_scheduled_input(
    pool: &sqlx::SqlitePool,
    cloud: &std::sync::Arc<crate::cloud::CloudManager>,
    precache: Option<&std::sync::Arc<crate::precache::Precache>>,
    media_id: &str,
    ticket: crate::cloud::manager::ResolutionTicket,
) -> Result<(
    crate::cloud::manager::ResolvedMedia,
    Option<crate::precache::CacheLease>,
)> {
    if let Some(cache) = precache
        && let Some(entry) = crate::db::precache_entry(&pool, media_id).await?
        && entry.status == "ready"
        && entry.mode == "copy"
        && entry.bytes == entry.source_size
        && crate::db::media_path(&pool, media_id).await?.as_deref()
            == Some(entry.source_path.as_str())
    {
        let identity = crate::precache::SourceIdentity::new(
            entry.source_path,
            entry.source_size,
            entry.source_mtime,
        );
        if let Some(lease) = cache.acquire_ready(media_id, &identity).await? {
            let kind = crate::db::media_kind(&pool, media_id)
                .await?
                .unwrap_or_else(|| "other".into());
            let location = lease.path().to_string_lossy().into_owned();
            tracing::info!(media = %media_id, path = %location, "scheduled precache hit");
            return Ok((
                crate::cloud::manager::ResolvedMedia { location, kind },
                Some(lease),
            ));
        }
    }
    let input = cloud
        .resolve_without_precache(media_id, Some(ticket))
        .await?;
    if !input.location.contains("://") && !cloud.is_file(PathBuf::from(&input.location)).await? {
        anyhow::bail!("source file is missing");
    }
    Ok((input, None))
}

#[cfg(test)]
static TEST_RESOLVER_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static TEST_ENCODER_STALLED_ONCE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static TEST_RESOLVER_STALLED_ONCE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const WARM_BYTES: u64 = 4 * 1024 * 1024;
const WARM_LOOKAHEAD_MS: i64 = 120_000;
const WARM_RECENT_MS: i64 = 10 * 60_000;
const WARM_MAX_INFLIGHT: usize = 2;
const SLATE_COMMIT_MARGIN_MS: i64 = 6_000;
/// Slate published for an unresolved source before resolution is retried.
const UNAVAILABLE_RETRY_MS: i64 = 15_000;

/// A source needed now gets 2 s before slate; one prepared ahead of air time
/// may use the slack, keeping a margin to start its encoder.
fn resolution_deadline(cursor_ms: i64, now_ms: i64) -> std::time::Duration {
    std::time::Duration::from_millis(
        cursor_ms
            .saturating_sub(now_ms)
            .saturating_sub(5_000)
            .clamp(2_000, 20_000) as u64,
    )
}

/// Bounds a source-unavailable slate to the retry interval, unless the
/// remainder after it would be too short to be worth a retry.
fn unavailable_slate_ms(remaining_ms: i64) -> i64 {
    if remaining_ms > UNAVAILABLE_RETRY_MS + 5_000 {
        UNAVAILABLE_RETRY_MS
    } else {
        remaining_ms
    }
}

/// Reads the first and last `WARM_BYTES` of a file, discarding the data, to
/// prime directory entries, disks and container headers on slow storage.
fn warm_file_head_tail(path: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    let mut drain = |file: &mut std::fs::File, mut remaining: u64| -> std::io::Result<()> {
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            remaining -= n as u64;
            total += n as u64;
        }
        Ok(())
    };
    drain(&mut file, WARM_BYTES.min(len))?;
    if len > WARM_BYTES * 2 {
        file.seek(SeekFrom::Start(len - WARM_BYTES))?;
        drain(&mut file, WARM_BYTES)?;
    }
    Ok(total)
}

/// Decides whether a media id needs warming and prunes stale entries so the
/// map stays bounded.
fn should_warm(
    warmed: &mut std::collections::HashMap<String, i64>,
    media_id: &str,
    now_ms: i64,
) -> bool {
    warmed.retain(|_, at| now_ms.saturating_sub(*at) < WARM_RECENT_MS);
    if warmed.contains_key(media_id) {
        return false;
    }
    warmed.insert(media_id.to_string(), now_ms);
    true
}

/// Looks at items starting soon and spawns detached, concurrency-capped
/// warm jobs. Never awaits a warm job.
async fn prewarm_upcoming(
    pool: &sqlx::SqlitePool,
    channel_id: &str,
    cursor_ms: i64,
    warmed: &mut std::collections::HashMap<String, i64>,
    inflight: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::Ordering;
    let rows: Vec<(String, i64)> = match sqlx::query_as(
        "SELECT media_id, start_ms FROM channel_items WHERE channel_id = ? AND end_ms > ? AND start_ms < ? ORDER BY start_ms LIMIT 8",
    ).bind(channel_id).bind(cursor_ms).bind(cursor_ms + WARM_LOOKAHEAD_MS).fetch_all(pool).await {
        Ok(rows) => rows,
        Err(error) => { tracing::debug!(%error, "prewarm lookahead failed"); return; }
    };
    let now = crate::db::now_ms();
    for (media_id, _) in rows {
        if inflight.load(Ordering::Relaxed) >= WARM_MAX_INFLIGHT {
            break;
        }
        if media_id.is_empty()
            || warmed
                .get(&media_id)
                .is_some_and(|at| now.saturating_sub(*at) < WARM_RECENT_MS)
        {
            continue;
        }
        let path = match crate::db::media_path(pool, &media_id).await {
            Ok(Some(path)) if !path.contains("://") => path,
            _ => {
                warmed.insert(media_id, now);
                continue;
            }
        };
        if !should_warm(warmed, &media_id, now) {
            continue;
        }
        inflight.fetch_add(1, Ordering::Relaxed);
        let inflight = inflight.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let id = media_id.clone();
            let result =
                tokio::task::spawn_blocking(move || warm_file_head_tail(Path::new(&path))).await;
            inflight.fetch_sub(1, Ordering::Relaxed);
            match result {
                Ok(Ok(bytes)) => {
                    tracing::info!(media_id = %id, bytes, elapsed_ms = started.elapsed().as_millis() as u64, "source prewarm finished")
                }
                Ok(Err(error)) => tracing::debug!(media_id = %id, %error, "source prewarm failed"),
                Err(error) => tracing::debug!(media_id = %id, %error, "source prewarm task failed"),
            }
        });
    }
}

struct ResolvedInput {
    media: crate::cloud::manager::ResolvedMedia,
    local_seek: i64,
    source_coverage_end: Option<i64>,
    original_identity: Option<crate::precache::SourceIdentity>,
    lease: Option<super::InputLease>,
}

struct ScheduledChunkResolver(Option<std::sync::Arc<crate::precache::chunks::ChunkIndex>>);
impl ScheduledChunkResolver {
    async fn new(precache: Option<&std::sync::Arc<crate::precache::Precache>>) -> Self {
        let chunks = if let Some(cache) = precache {
            match cache.shared_chunk_index().await {
                Ok(index) => index,
                Err(error) => {
                    tracing::warn!(%error, "chunk index unavailable");
                    None
                }
            }
        } else {
            None
        };
        Self(chunks)
    }

    async fn resolve(
        &self,
        pool: &sqlx::SqlitePool,
        precache: Option<&std::sync::Arc<crate::precache::Precache>>,
        id: &str,
        source_offset: i64,
    ) -> Result<Option<ResolvedInput>> {
        if let Some(index) = self.0.as_ref()
            && let Some(path) = crate::db::media_path(pool, id).await?
        {
            let identity = precache.and_then(|cache| cache.verified_rolling_source(id, &path));
            let identity = if identity.is_some() {
                identity
            } else {
                crate::db::precache_entry(pool, id)
                    .await?
                    .filter(|entry| entry.identity_known && entry.source_path == path)
                    .map(|entry| {
                        crate::precache::SourceIdentity::new(
                            entry.source_path,
                            entry.source_size,
                            entry.source_mtime,
                        )
                    })
            };
            if let Some(identity) = identity
                && let Some(chunk) = index.acquire(&identity, source_offset)
            {
                let kind = crate::db::media_kind(pool, id)
                    .await?
                    .unwrap_or_else(|| "other".into());
                let media = crate::cloud::manager::ResolvedMedia {
                    location: chunk.path.to_string_lossy().into_owned(),
                    kind,
                };
                return Ok(Some(ResolvedInput {
                    media,
                    local_seek: chunk.local_seek,
                    source_coverage_end: Some(chunk.source_coverage_end),
                    original_identity: Some(identity),
                    lease: Some(super::InputLease::Chunk(std::sync::Arc::new(chunk))),
                }));
            }
        }
        Ok(None)
    }
}

pub(super) async fn run_channel(
    pool: sqlx::SqlitePool,
    cloud: std::sync::Arc<crate::cloud::CloudManager>,
    precache: Option<std::sync::Arc<crate::precache::Precache>>,
    channel: crate::db::Channel,
    data_dir: PathBuf,
    out_dir: PathBuf,
    max_kbps: i64,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    start_ms: i64,
    shared: super::SnapshotHandle,
) -> Result<()> {
    let chunks = ScheduledChunkResolver::new(precache.as_ref()).await.0;
    run_channel_with_chunks(
        pool, cloud, precache, channel, data_dir, out_dir, max_kbps, stop, ready, start_ms, shared,
        chunks,
    )
    .await
}

async fn run_channel_with_chunks(
    pool: sqlx::SqlitePool,
    cloud: std::sync::Arc<crate::cloud::CloudManager>,
    precache: Option<std::sync::Arc<crate::precache::Precache>>,
    channel: crate::db::Channel,
    data_dir: PathBuf,
    out_dir: PathBuf,
    max_kbps: i64,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    start_ms: i64,
    shared: super::SnapshotHandle,
    chunks: Option<std::sync::Arc<crate::precache::chunks::ChunkIndex>>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let root = data_dir
        .join("private-hls")
        .join(uuid::Uuid::new_v4().simple().to_string());
    let mut spool = PrivateSpool::new(root.clone())?;
    let mut publisher = Publisher::resume(
        out_dir.clone(),
        start_ms,
        channel.stream.cache_segments.clamp(4, 60) as usize,
    )?;
    // A retained playlist can be hours old after an idle period. Preserve its
    // sequence and anchor, but resume at the current schedule with a discontinuity.
    let live_ticks = start_ms
        .saturating_sub(publisher.anchor_ms)
        .saturating_mul(90);
    if live_ticks.saturating_sub(publisher.cursor) > 10 * TICKS {
        publisher.cursor = live_ticks;
        publisher.resume_boundary = true;
        publisher.stale_prefix = true;
    }
    let start_ms = publisher.anchor_ms;
    let first_new_sequence = publisher.next_sequence;
    let mut cursor = preparation_cursor(start_ms, publisher.cursor);
    let mut queue: VecDeque<(Option<i64>, i64, SegmentSource)> = VecDeque::new();
    let mut lease: Option<SegmentLease> = None;
    // The NAS attribute cache lasts ~1s, so every resolution is a network
    // round trip. Reuse a lease-free resolution for later chunks of the same
    // airing; a vanished file still fails the encoder and falls back to slate.
    let mut airing_input: Option<(String, i64, crate::cloud::manager::ResolvedMedia)> = None;
    type Resolution = Option<ResolvedInput>;
    let mut pending_resolution: Option<(
        i64,
        String,
        std::time::Instant,
        std::pin::Pin<Box<dyn std::future::Future<Output = Resolution> + Send>>,
    )> = None;
    let minimum_ticks = channel.stream.prebuffer_seconds.clamp(0, 30).max(1) * TICKS;
    let watermark = match channel.graphics.watermark_media_id.as_deref() {
        Some(id) if !id.is_empty() => crate::db::media_path(&pool, id).await?,
        _ => None,
    };
    let mut warmed: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let warm_inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut last_warm_check = std::time::Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if last_warm_check.elapsed() >= Duration::from_secs(1) {
            last_warm_check = std::time::Instant::now();
            prewarm_upcoming(&pool, &channel.id, cursor, &mut warmed, &warm_inflight).await;
        }

        // Prepare the next scheduled airing while the current child is alive.
        let mut can_prepare = queue.is_empty();
        // A chunk input ends at its coverage edge, mid-programme. Preparing the
        // continuation only after it ends leaves no time to open the source.
        if queue.len() == 1 {
            can_prepare = true;
        }
        while can_prepare
            && preparation_allowed(
                cursor,
                crate::db::now_ms(),
                channel.stream.prebuffer_seconds,
            )
        {
            can_prepare = false;
            let mut lookup_ms = cursor;
            let mut item = crate::db::current_item(&pool, &channel.id, lookup_ms).await?;
            // Skip only a remainder which maps to no output frame.
            if let Some(current) = &item {
                if frame_grid_end(start_ms, current.end_ms) <= frame_grid_end(start_ms, cursor) {
                    lookup_ms = current.end_ms;
                    item = crate::db::current_item(&pool, &channel.id, lookup_ms).await?;
                }
            }
            let (duration, command, identity, lease) = if let Some(item) = item {
                let duration = item.end_ms - cursor;
                let programme = ScheduledProgramme {
                    id: item.media_id.clone(),
                    title: item.title.clone(),
                    start_ms: item.start_ms,
                    end_ms: item.end_ms,
                };
                let reused = airing_input
                    .as_ref()
                    .filter(|(id, start, _)| *id == item.media_id && *start == item.start_ms)
                    .map(|(_, _, input)| ResolvedInput {
                        media: input.clone(),
                        local_seek: item.in_ms + (cursor - item.start_ms).max(0),
                        source_coverage_end: None,
                        original_identity: None,
                        lease: None,
                    });
                let resolved = if let Some(reused) = reused {
                    Some(reused)
                } else {
                    if pending_resolution
                        .as_ref()
                        .is_some_and(|(at, id, _, _)| *at != cursor || *id != item.media_id)
                    {
                        pending_resolution = None;
                    }
                    if pending_resolution.is_none() {
                        let (pool, cloud, precache, stop, id, chunks) = (
                            pool.clone(),
                            cloud.clone(),
                            precache.clone(),
                            stop.clone(),
                            item.media_id.clone(),
                            chunks.clone(),
                        );
                        let source_offset = item.in_ms + (cursor - item.start_ms).max(0);
                        tracing::info!(channel = %channel.number, media_id = %id, cursor_ms = cursor, wall_ms = crate::db::now_ms(), "resolver started");
                        let deadline = resolution_deadline(cursor, crate::db::now_ms());
                        let future = async move {
                            resolve_with_deadline(&stop, deadline, |ticket| async {
                                #[cfg(test)]
                                if id.starts_with("test-resolver-stall-") {
                                    if id == "test-resolver-stall-progress" {
                                        TEST_RESOLVER_STARTED.store(true, Ordering::SeqCst);
                                    }
                                    return std::future::pending().await;
                                }
                                #[cfg(test)]
                                if id.starts_with("test-resolver-once-")
                                    && !TEST_RESOLVER_STALLED_ONCE.swap(true, Ordering::SeqCst)
                                {
                                    return std::future::pending().await;
                                }
                                if let Some(resolved) = ScheduledChunkResolver(chunks.clone())
                                    .resolve(&pool, precache.as_ref(), &id, source_offset)
                                    .await?
                                {
                                    return Ok(resolved);
                                }
                                let (media, lease) = resolve_scheduled_input(
                                    &pool,
                                    &cloud,
                                    precache.as_ref(),
                                    &id,
                                    ticket,
                                )
                                .await?;
                                Ok(ResolvedInput {
                                    media,
                                    local_seek: source_offset,
                                    source_coverage_end: None,
                                    original_identity: None,
                                    lease: lease.map(super::InputLease::Full),
                                })
                            })
                            .await
                        };
                        pending_resolution = Some((
                            cursor,
                            item.media_id.clone(),
                            std::time::Instant::now(),
                            Box::pin(future),
                        ));
                    }
                    let outcome = std::future::poll_fn(|cx| {
                        use std::task::Poll;
                        match pending_resolution.as_mut().unwrap().3.as_mut().poll(cx) {
                            Poll::Ready(value) => Poll::Ready(Some(value)),
                            Poll::Pending => Poll::Ready(None),
                        }
                    })
                    .await;
                    match outcome {
                        Some(value) => {
                            let (_, _, started, _) = pending_resolution.take().unwrap();
                            tracing::info!(channel = %channel.number, media_id = %item.media_id, cursor_ms = cursor, wall_ms = crate::db::now_ms(), elapsed_ms = started.elapsed().as_millis() as u64, resolved = value.is_some(), "resolver finished");
                            value
                        }
                        None => break,
                    }
                };
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // A slow lookup well before air time is retried on the next
                // tick instead of committing the interval to slate.
                if resolved.is_none() && cursor - crate::db::now_ms() > SLATE_COMMIT_MARGIN_MS {
                    tracing::info!(media_id = %item.media_id, "source resolution slow; retrying before air time");
                    if let Some(cache) = &precache {
                        cache.note_playback_pressure();
                    }
                    break;
                }
                airing_input = match &resolved {
                    Some(input) if input.lease.is_none() && input.media.kind != "slate" => {
                        Some((item.media_id.clone(), item.start_ms, input.media.clone()))
                    }
                    _ => None,
                };
                match resolved {
                    Some(input) if input.media.kind != "slate" => {
                        let source_offset = item.in_ms + (cursor - item.start_ms).max(0);
                        let duration = input.source_coverage_end.map_or(duration, |end| {
                            duration.min(end.saturating_sub(source_offset))
                        });
                        let offset = input.local_seek;
                        let _original_identity = input.original_identity;
                        let lease = input.lease;
                        let input = input.media;
                        let command = super::build_producer_command_leased(
                            &channel,
                            &format!("{}:{}", item.media_id, item.start_ms),
                            &item.media_id,
                            Path::new(&input.location),
                            offset,
                            0,
                            &out_dir,
                            &data_dir,
                            false,
                            &input.kind,
                            watermark.as_deref().map(Path::new),
                            max_kbps,
                            true,
                            true,
                            lease.as_ref(),
                        )?;
                        #[cfg(test)]
                        let command = if item.media_id.starts_with("test-encoder-stall-")
                            || (item.media_id.starts_with("test-encoder-once-")
                                && !TEST_ENCODER_STALLED_ONCE.swap(true, Ordering::SeqCst))
                        {
                            let mut stalled = std::process::Command::new("sh");
                            stalled.args(["-c", "exec sleep 60"]);
                            stalled
                        } else {
                            command
                        };
                        (
                            duration,
                            command,
                            SegmentSource::ScheduledMedia {
                                id: item.media_id.clone(),
                                scheduled_programme: programme.clone(),
                            },
                            lease,
                        )
                    }
                    _ => {
                        // Cover only a short interval, then resolve the same airing
                        // again: a cold source must not cost the rest of the programme.
                        let duration = unavailable_slate_ms(duration);
                        tracing::warn!(media_id = %item.media_id, slate_ms = duration, "source unavailable; publishing scheduled slate before retry");
                        if let Some(cache) = &precache {
                            cache.note_playback_pressure();
                        }
                        (
                            duration,
                            slate_command(&channel),
                            SegmentSource::fallback("source_unavailable", Some(programme)),
                            None,
                        )
                    }
                }
            } else {
                let next = crate::db::next_item_start(&pool, &channel.id, lookup_ms).await?;
                let duration = (next.unwrap_or(cursor + 10_000) - cursor).clamp(1, 10_000);
                (
                    duration,
                    slate_command(&channel),
                    SegmentSource::fallback("scheduled_gap", None),
                    None,
                )
            };
            if duration <= 0 {
                anyhow::bail!("invalid scheduled source duration");
            }
            let frames = (frame_grid_end(start_ms, cursor + duration)
                - frame_grid_end(start_ms, cursor))
                / 3003;
            if frames <= 0 {
                anyhow::bail!("scheduled interval contains no output frames");
            }
            spool.start_frames_leased(command, frames, lease)?;
            // Independent programme encoders restart timestamps.
            queue.push_back((Some(cursor), cursor + duration, identity));
            cursor += duration;
        }
        if lease.is_none() && !queue.is_empty() {
            match spool.take_ready_segment()? {
                TakeReady::Segment(segment) => {
                    let now = crate::db::now_ms();
                    let deadline = start_ms + (publisher.cursor + segment.duration_ticks) / 90;
                    let mono = monotonic_ms();
                    let wall_ms =
                        time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
                    let deadline_mono_ms = mono as i128 + deadline as i128 - wall_ms;
                    tracing::info!(channel = %channel.number, wall_ms = now, cursor_ticks = publisher.cursor, published_buffer_segments = publisher.entries.len(), queued_generations = queue.len(), monotonic_ms = mono, deadline_ms = deadline, deadline_monotonic_ms = deadline_mono_ms, deadline_in_ms = deadline - now, "private segment ready for scheduled publication");
                    lease = Some(segment)
                }
                TakeReady::End => {
                    let (_, end, identity) = queue.pop_front().expect("queued generation");
                    let target = frame_grid_end(start_ms, end);
                    let missing = frames_to_end(publisher.cursor, target);
                    if missing > 0 {
                        if identity.is_slate() {
                            anyhow::bail!("private fallback ended before its frame boundary");
                        }
                        spool.start_frames_leased(slate_command(&channel), missing, None)?;
                        let replacement = spool.items.pop().expect("replacement encoder");
                        spool.items.insert(0, replacement);
                        queue.push_front((
                            Some(preparation_cursor(start_ms, publisher.cursor)),
                            end,
                            SegmentSource::fallback("early_eof", identity.programme()),
                        ));
                    } else if publisher.cursor - target > 3003 {
                        // The next generation was prepared for an obsolete source offset.
                        spool.items.clear();
                        queue.clear();
                        cursor = preparation_cursor(start_ms, publisher.cursor);
                    } else if queue.is_empty() {
                        cursor = preparation_cursor(start_ms, publisher.cursor);
                    }
                }
                TakeReady::Failed => {
                    let (_, end, identity) = queue.front().expect("queued generation").clone();
                    if identity.is_slate() {
                        anyhow::bail!("private fallback encoder failed");
                    }
                    spool.items.remove(0);
                    let recovery_start = start_ms + (publisher.cursor + 89) / 90;
                    // Cover a short interval, then reopen the same airing: a slow
                    // read must not cost the rest of the programme.
                    let bounded = unavailable_slate_ms(end - recovery_start) < end - recovery_start;
                    let slate_end = if bounded {
                        recovery_start + UNAVAILABLE_RETRY_MS
                    } else {
                        end
                    };
                    let remaining =
                        frames_to_end(publisher.cursor, frame_grid_end(start_ms, slate_end));
                    tracing::warn!(channel = %channel.number, remaining_frames = remaining, retry = bounded,
                        "source encoder failed; publishing slate before retry");
                    if remaining > 0 {
                        spool.start_frames_leased(slate_command(&channel), remaining, None)?;
                        let replacement = spool.items.pop().expect("replacement encoder");
                        spool.items.insert(0, replacement);
                        *queue.front_mut().unwrap() = (
                            Some(recovery_start),
                            slate_end,
                            SegmentSource::fallback("encoder_stall", identity.programme()),
                        );
                        if bounded {
                            // Later generations were prepared after the failed interval.
                            spool.items.truncate(1);
                            queue.truncate(1);
                            cursor = slate_end;
                            pending_resolution = None;
                        }
                    } else {
                        queue.pop_front();
                        cursor = preparation_cursor(start_ms, publisher.cursor);
                    }
                }
                TakeReady::Pending => {}
            }
        }
        if let Some(segment) = lease.as_ref() {
            let boundary = queue.front().and_then(|entry| entry.0);
            let now = crate::db::now_ms();
            if publisher.publish(
                &segment.path,
                segment.duration_ticks,
                boundary,
                now,
                90_000,
                queue.front().expect("queued generation").2.clone(),
            )? {
                tracing::info!(channel = %channel.number, wall_ms = now, sequence = publisher.next_sequence - 1, published_buffer_segments = publisher.entries.len(), held_ready_segments = 1, publication_lag_ms = now - (start_ms + publisher.cursor / 90), queued_generations = queue.len(), segment_ms = segment.duration_ticks / 90, start_90k = %((publisher.anchor_ms as i128 * 90) + publisher.cursor as i128 - segment.duration_ticks as i128), end_90k = %((publisher.anchor_ms as i128 * 90) + publisher.cursor as i128), "private segment published");
                if let Some(first) = queue.front_mut() {
                    first.0 = None;
                }
                lease = None;
                // Only closed, advertised segments from this run count. Private
                // spool segments and expired historical publications cannot play.
                let available: i64 = publisher
                    .entries
                    .iter()
                    .filter(|entry| entry.sequence >= first_new_sequence)
                    .map(|entry| entry.duration_ticks)
                    .sum();
                {
                    let mut snapshot = shared.lock().unwrap();
                    snapshot.segments = publisher.snapshot();
                    snapshot.new_published_seconds = available as f64 / TICKS as f64;
                }
                if available >= minimum_ticks {
                    ready.store(true, Ordering::Release);
                }
            }
        }
        publisher.cleanup(crate::db::now_ms())?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(spool);
    let _ = std::fs::remove_dir(root);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn absolute_grid_does_not_accumulate_twenty_second_rounding() {
        let mut cursor: i64 = 0;
        for n in 1_i64..=100 {
            let end = super::frame_grid_end(0, n * 20_000);
            let frames = super::frames_to_end(cursor, end);
            cursor += frames * 3003;
            assert!((cursor - n * 20_000 * 90).abs() <= 3003);
        }
    }

    #[test]
    fn early_eof_requires_recovery_before_queued_successor() {
        let end = super::frame_grid_end(0, 20_000);
        let actual = end - 4 * 3003;
        assert_eq!(super::frames_to_end(actual, end), 4);
    }

    #[test]
    fn one_frame_slate_can_be_spooled() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("one-frame-{}", uuid::Uuid::new_v4()));
        let mut spool = super::PrivateSpool::new(root.clone()).unwrap();
        let mut command = std::process::Command::new("ffmpeg");
        command.args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=s=64x64:r=30000/1001",
            "-c:v",
            "libx264",
            "-threads",
            "1",
        ]);
        spool.start_frames_leased(command, 1, None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "one-frame encoder stalled");
            match spool.take_ready_segment().unwrap() {
                super::TakeReady::Segment(segment) => {
                    assert!((segment.duration_ticks - 3003).abs() <= 1);
                    break;
                }
                super::TakeReady::Pending => std::thread::sleep(Duration::from_millis(10)),
                _ => panic!("one-frame encoder ended without a segment"),
            }
        }
        drop(spool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn chunk_lease_survives_encoder_until_reaped() {
        use crate::precache::chunks::{ChunkIndex, Manifest};
        let root = std::env::temp_dir().join(format!("chunk-reaper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("part.mkv"), b"fixture").unwrap();
        let manifest = Manifest {
            version: 2,
            payload_bytes: 7,
            source_path: "/source".into(),
            source_size: 1,
            source_mtime: 2,
            chunk_file: "part.mkv".into(),
            source_start_ms: 0,
            source_end_ms: 1000,
            local_start_ms: 0,
        };
        let manifest_path = root.join("part.json");
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let index = ChunkIndex::open(&root).unwrap();
        let chunk = index
            .acquire(&crate::precache::SourceIdentity::new("/source", 1, 2), 500)
            .unwrap();
        let mut spool = super::PrivateSpool::new(root.join("spool")).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        spool
            .start_bounded_leased(
                command,
                "1",
                1000,
                Some(super::super::InputLease::Chunk(std::sync::Arc::new(chunk))),
            )
            .unwrap();
        assert!(!index.evict(&manifest_path).unwrap());
        drop(spool);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !index.evict(&manifest_path).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_shared_rolling_index_reaches_constructed_resolver() {
        use crate::precache::{
            Precache, SourceIdentity,
            chunks::{Airing, TickOutcome},
        };
        use std::sync::Arc;
        let root = std::env::temp_dir().join(format!("manager-resolver-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("origin.mkv");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=64x64:rate=10:duration=12",
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
        let meta = std::fs::metadata(&source).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let identity = SourceIdentity::new(source.to_string_lossy(), meta.len() as i64, mtime);
        let pool = crate::db::init(&root).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('chunk',?,'Chunk','movie',12000,0)").bind(source.to_str().unwrap()).execute(&pool).await.unwrap();
        // Rolling video does not materialize a whole-copy precache entry.
        let mut config = crate::config::Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        config.precache_max_gb = 1;
        config.precache_rolling_chunks = true;
        let manager = Arc::new(Precache::new(&config, pool.clone()));
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let engine = super::super::Engine::new(pool.clone(), root.clone(), cloud, 0)
            .with_precache(manager.clone());
        let resolver = super::ScheduledChunkResolver::new(engine.precache.as_ref()).await;
        assert!(Arc::ptr_eq(
            resolver.0.as_ref().unwrap(),
            &manager.shared_chunk_index().await.unwrap().unwrap()
        ));
        assert!(
            resolver
                .resolve(&pool, Some(&manager), "chunk", 3000)
                .await
                .unwrap()
                .is_none()
        );
        let airing = Airing {
            airing_id: "a".into(),
            channel_id: "c".into(),
            media_id: "chunk".into(),
            source: identity.clone(),
            in_ms: 0,
            start_ms: 0,
            end_ms: 10000,
        };
        assert_eq!(
            manager.test_rolling_tick(vec![airing], 0).await.unwrap(),
            TickOutcome::Completed
        );
        std::fs::remove_file(&source).unwrap();
        let resolved = resolver
            .resolve(&pool, Some(&manager), "chunk", 3000)
            .await
            .unwrap()
            .expect("new chunk visible to existing resolver");
        assert!(std::path::Path::new(&resolved.media.location).is_file());
        assert_eq!(resolved.local_seek, 3000);
        assert_eq!(resolved.original_identity, Some(identity.clone()));
        assert!(matches!(
            resolved.lease,
            Some(super::super::InputLease::Chunk(_))
        ));
        assert!(
            resolver
                .resolve(&pool, Some(&manager), "chunk", 11000)
                .await
                .unwrap()
                .is_none()
        );
        let mut changed = identity.clone();
        changed.size += 1;
        let changed_airing = Airing {
            airing_id: "a".into(),
            channel_id: "c".into(),
            media_id: "chunk".into(),
            source: changed,
            in_ms: 0,
            start_ms: 0,
            end_ms: 10000,
        };
        let _ = manager
            .test_rolling_tick(vec![changed_airing], 0)
            .await
            .unwrap();
        assert!(
            resolver
                .resolve(&pool, Some(&manager), "chunk", 3000)
                .await
                .unwrap()
                .is_none(),
            "an older generation must not be selected"
        );
        config.precache_rolling_chunks = false;
        let disabled = Arc::new(Precache::new(&config, pool.clone()));
        let disabled_resolver = super::ScheduledChunkResolver::new(Some(&disabled)).await;
        assert!(disabled_resolver.0.is_none());
        assert!(
            disabled_resolver
                .resolve(&pool, Some(&disabled), "chunk", 3000)
                .await
                .unwrap()
                .is_none()
        );
        drop(resolved);
        drop(resolver);
        drop(engine);
        drop(manager);
        drop(disabled);
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adjacent_chunks_publish_from_nonzero_source_offset() {
        use crate::precache::chunks::{ChunkIndex, Manifest};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("chunk-publisher-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("origin.mkv");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=red:s=160x90:r=30:d=3",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=blue:s=160x90:r=30:d=3",
                    "-filter_complex",
                    "[0:v][1:v]concat=n=2:v=1:a=0",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-g",
                    "30",
                    "-keyint_min",
                    "30",
                    "-sc_threshold",
                    "0",
                    "-y"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let size = std::fs::metadata(&source).unwrap().len() as i64;
        let dir = root.join("chunks");
        std::fs::create_dir(&dir).unwrap();
        for (name, start, end) in [("red", 0, 3000), ("blue", 3000, 6000)] {
            assert!(
                std::process::Command::new("ffmpeg")
                    .args([
                        "-v",
                        "error",
                        "-ss",
                        &(start as f64 / 1000.0).to_string(),
                        "-i"
                    ])
                    .arg(&source)
                    .args(["-t", "3", "-map", "0", "-c", "copy", "-y"])
                    .arg(dir.join(format!("{name}.mkv")))
                    .status()
                    .unwrap()
                    .success()
            );
            let manifest = Manifest {
                version: 2,
                payload_bytes: std::fs::metadata(dir.join(format!("{name}.mkv")))
                    .unwrap()
                    .len(),
                source_path: source.to_string_lossy().into_owned(),
                source_size: size,
                source_mtime: 123,
                chunk_file: format!("{name}.mkv"),
                source_start_ms: start,
                source_end_ms: end,
                local_start_ms: 0,
            };
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
        }
        let index = Arc::new(ChunkIndex::open(&dir).unwrap());
        let pool = crate::db::init(&root).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('chunk',?,'Chunk programme','movie',6000,0)").bind(source.to_str().unwrap()).execute(&pool).await.unwrap();
        crate::db::upsert_precache(
            &pool,
            "chunk",
            source.to_str().unwrap(),
            size,
            123,
            "",
            "copy",
            0,
            0,
        )
        .await
        .unwrap();
        std::fs::remove_file(&source).unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "998", "chunk")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.prebuffer_seconds = 8;
        channel.stream.cache_segments = 12;
        channel.stream.width = 160;
        channel.stream.height = 90;
        channel.stream.preset = "ultrafast".into();
        let start = crate::db::now_ms();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,'chunk','Chunk programme',?,?,1000,6000,'program')").bind(&channel.id).bind(start).bind(start+5000).execute(&pool).await.unwrap();
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let shared: super::super::SnapshotHandle = Default::default();
        let out = root.join("out");
        std::fs::create_dir(&out).unwrap();
        let task = tokio::spawn(super::run_channel_with_chunks(
            pool,
            cloud,
            None,
            channel,
            root.clone(),
            out.clone(),
            0,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
            start,
            shared.clone(),
            Some(index),
        ));
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if task.is_finished()
                    || shared
                        .lock()
                        .unwrap()
                        .segments
                        .iter()
                        .any(|s| s.end_pdt_ns >= (start as i128 + 4900) * 1_000_000)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let segments = shared.lock().unwrap().segments.clone();
        assert!(segments.len() >= 2);
        assert!(segments.iter().all(|s| matches!(&s.source, super::SegmentSource::ScheduledMedia { id, scheduled_programme } if id == "chunk" && scheduled_programme.title == "Chunk programme")));
        assert!((segments[0].start_pdt_ns - start as i128 * 1_000_000).abs() < 34_000_000);
        for pair in segments.windows(2) {
            assert_eq!(pair[0].end_pdt_ns, pair[1].start_pdt_ns);
        }
        let playlist = std::fs::read_to_string(out.join("index.m3u8")).unwrap();
        let files: Vec<_> = playlist
            .lines()
            .filter(|line| line.ends_with(".ts"))
            .collect();
        let mut colors = Vec::new();
        for file in files {
            let output = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(out.join(file))
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
            assert!(output.status.success());
            if output.stdout.len() >= 3 {
                colors.push(output.stdout);
            }
        }
        assert!(
            colors.iter().any(|c| c[0] > c[2].saturating_add(80)),
            "red marker missing: {:?}",
            colors
        );
        assert!(
            colors.iter().any(|c| c[2] > c[0].saturating_add(80)),
            "blue marker missing: {:?}",
            colors
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn early_eof_fills_before_prepared_successor_in_production() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("frame-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, seconds) in [("short", 1), ("next", 4)] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=s=160x90:r=30:d={seconds}"))
                    .args(["-f", "lavfi", "-i"])
                    .arg(format!("sine=duration={seconds}"))
                    .args([
                        "-c:v",
                        "libx264",
                        "-threads",
                        "1",
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
        let mut channel = crate::db::create_channel_shell(&pool, "999", "recovery")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.prebuffer_seconds = 8;
        channel.stream.width = 160;
        channel.stream.height = 90;
        channel.stream.preset = "ultrafast".into();
        let start = crate::db::now_ms();
        for (pos, id) in [(0_i64, "short"), (1, "next")] {
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,3000,'program')")
                .bind(&channel.id).bind(pos).bind(id).bind(id).bind(start + pos * 3000).bind(start + (pos + 1) * 3000).execute(&pool).await.unwrap();
        }
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let shared: super::super::SnapshotHandle = Default::default();
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let task = tokio::spawn(super::run_channel(
            pool,
            cloud,
            None,
            channel,
            root.clone(),
            out,
            0,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
            start,
            shared.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if task.is_finished() || shared.lock().unwrap().segments.iter().any(|s| matches!(&s.source, super::SegmentSource::ScheduledMedia { id, .. } if id == "next")) { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.unwrap();
        stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let snapshot = shared.lock().unwrap();
        let next = snapshot.segments.iter().position(|s| matches!(&s.source, super::SegmentSource::ScheduledMedia { id, .. } if id == "next")).expect("successor published");
        assert!(snapshot.segments[..next].iter().any(|s| matches!(&s.source, super::SegmentSource::Fallback { fallback_reason: Some(reason), .. } if reason == "early_eof")), "missing recovery slate");
        assert!(
            (snapshot.segments[next].start_pdt_ns - (start as i128 + 3000) * 1_000_000).abs()
                <= 33_366_667
        );
        drop(snapshot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_twenty_second_transitions_follow_frame_grid() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("grid-transitions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let path = root.join("source.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=s=160x90:r=30:d=20",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=20",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
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
        for id in ["grid-a", "grid-b", "grid-c"] {
            let item_path = root.join(format!("{id}.mp4"));
            std::fs::hard_link(&path, &item_path).unwrap();
            sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,?,'movie',20000,0)")
                .bind(id).bind(item_path.to_str().unwrap()).bind(id).execute(&pool).await.unwrap();
        }
        let mut channel = crate::db::create_channel_shell(&pool, "997", "grid")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.prebuffer_seconds = 8;
        channel.stream.cache_segments = 30;
        channel.stream.width = 160;
        channel.stream.height = 90;
        channel.stream.preset = "ultrafast".into();
        let start = crate::db::now_ms() - 8000;
        for (pos, id) in ["grid-a", "grid-b", "grid-c"].iter().enumerate() {
            let pos = pos as i64;
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,20000,'program')")
                .bind(&channel.id).bind(pos).bind(id).bind(id).bind(start + pos * 20000).bind(start + (pos + 1) * 20000).execute(&pool).await.unwrap();
        }
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let shared: super::super::SnapshotHandle = Default::default();
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let task = tokio::spawn(super::run_channel(
            pool,
            cloud,
            None,
            channel,
            root.clone(),
            out,
            0,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
            start,
            shared.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                if task.is_finished() || shared.lock().unwrap().segments.iter().any(|s| matches!(&s.source, super::SegmentSource::ScheduledMedia { id, .. } if id == "grid-c")) { break; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await.unwrap();
        stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let snapshot = shared.lock().unwrap();
        for (pos, id) in [(1_i128, "grid-b"), (2, "grid-c")] {
            let segment = snapshot.segments.iter().find(|s| matches!(&s.source, super::SegmentSource::ScheduledMedia { id: actual, .. } if actual == id)).expect("transition published");
            assert!(
                (segment.start_pdt_ns - (start as i128 + pos * 20_000) * 1_000_000).abs()
                    <= 33_366_667,
                "{id} transition drifted"
            );
        }
        drop(snapshot);
        std::fs::remove_dir_all(root).unwrap();
    }
    // Schedule intervals are half-open; an overlap needs adjacent, identified recovery.
    fn gate_interval_matches(
        channel: usize,
        epoch_ms: i64,
        start_ticks: i128,
        end_ticks: i128,
        source: &super::SegmentSource,
        neighbours: &[super::SegmentIdentity],
    ) -> bool {
        const SLOT: i128 = 20 * 90_000;
        const FRAME: i128 = 3003;
        if end_ticks <= start_ticks {
            return false;
        }
        let matches_slot = |pos: i128, src: &super::SegmentSource| {
            if pos < 0 {
                return false;
            }
            let id = if channel == 1 && pos == 2 {
                "test-resolver-stall-gate"
            } else if channel == 2 && pos == 2 {
                "test-encoder-stall-multichannel"
            } else if pos % 2 == 0 {
                "gate-red"
            } else {
                "gate-blue"
            };
            let context = |p: &super::ScheduledProgramme| {
                p.id == id
                    && p.title
                        == if (channel == 1 || channel == 2) && pos == 2 {
                            "stall"
                        } else {
                            id
                        }
                    && p.start_ms as i128 == epoch_ms as i128 + pos * 20_000
                    && p.end_ms as i128 == epoch_ms as i128 + (pos + 1) * 20_000
            };
            match src {
                super::SegmentSource::ScheduledMedia {
                    id: actual,
                    scheduled_programme,
                } => {
                    actual == id
                        && context(scheduled_programme)
                        && !(pos == 2 && (channel == 1 || channel == 2))
                }
                super::SegmentSource::Fallback {
                    fallback_reason: Some(reason),
                    scheduled_programme: Some(p),
                } => {
                    context(p)
                        && ((channel == 1 && pos == 2 && reason == "source_unavailable")
                            || (channel == 2 && pos == 2 && reason == "encoder_stall")
                            || (reason == "early_eof"
                                && !(pos == 2 && (channel == 1 || channel == 2))))
                }
                _ => false,
            }
        };
        let position = start_ticks.div_euclid(SLOT);
        (position.saturating_sub(1).max(0)..=position + 1).any(|pos| {
            if !matches_slot(pos, source) || start_ticks < pos * SLOT - FRAME || end_ticks > (pos + 1) * SLOT + FRAME { return false; }
            if start_ticks >= pos * SLOT && end_ticks <= (pos + 1) * SLOT { return true; }
            let recovery = |src: &super::SegmentSource, slot: i128| matches!(src, super::SegmentSource::Fallback { .. }) && matches_slot(slot, src);
            // The fallback itself carries the recovery proof. For media, the
            // adjacent fallback must identify the programme on the other side.
            if recovery(source, pos) {
                return !matches!(source, super::SegmentSource::Fallback { fallback_reason: Some(reason), .. } if reason == "early_eof") || (start_ticks >= pos * SLOT && end_ticks > (pos + 1) * SLOT);
            }
            neighbours.iter().any(|n| {
                let ns = n.start_90k - epoch_ms as i128 * 90;
                let ne = n.end_90k - epoch_ms as i128 * 90;
                // Ordinary transitions occur at the nearest absolute frame grid
                // point, and require the adjacent scheduled identity as proof.
                let quantized = |boundary: i128| ((boundary + FRAME / 2) / FRAME) * FRAME;
                let media = |s: &super::SegmentSource| matches!(s, super::SegmentSource::ScheduledMedia { .. });
                if media(source) && media(&n.source) {
                    if start_ticks < pos * SLOT && pos > 0 && start_ticks == quantized(pos * SLOT) && ne == start_ticks && ns < ne && matches_slot(pos - 1, &n.source) { return true; }
                    if end_ticks > (pos + 1) * SLOT && end_ticks == quantized((pos + 1) * SLOT) && ns == end_ticks && ne > ns && matches_slot(pos + 1, &n.source) { return true; }
                }
                (start_ticks < pos * SLOT && ne == start_ticks && pos > 0 && recovery(&n.source, pos - 1)) ||
                (end_ticks > (pos + 1) * SLOT && ns == end_ticks && recovery(&n.source, pos + 1))
            })
        })
    }
    #[test]
    fn identity_preserves_exact_frame_ticks() {
        let publisher = Publisher::new(std::path::PathBuf::new(), 10_000, 4);
        let entry = super::Published {
            sequence: 0,
            name: String::new(),
            duration_ticks: 3003,
            date_ticks: 3003,
            discontinuity: false,
        };
        let identity = publisher.identity(&entry, super::SegmentSource::Slate);
        assert_eq!(identity.start_90k, 903_003);
        assert_eq!(identity.end_90k, 906_006);
    }
    #[test]
    fn gate_accepts_only_proven_quantized_media_boundary() {
        use super::{ScheduledProgramme as P, SegmentIdentity as I, SegmentSource as S};
        let media = |id: &str, start, end| S::ScheduledMedia {
            id: id.into(),
            scheduled_programme: P {
                id: id.into(),
                title: id.into(),
                start_ms: start,
                end_ms: end,
            },
        };
        let boundary = 599 * 3003;
        let previous = I {
            sequence: 0,
            start_pdt_ns: 0,
            end_pdt_ns: 0,
            start_90k: boundary - 180180,
            end_90k: boundary,
            source: media("gate-red", 0, 20_000),
        };
        let next = media("gate-blue", 20_000, 40_000);
        assert!(gate_interval_matches(
            0,
            0,
            boundary,
            boundary + 180180,
            &next,
            &[previous.clone()]
        ));
        assert!(!gate_interval_matches(
            0,
            0,
            boundary - 1,
            boundary + 180180,
            &next,
            &[previous.clone()]
        ));
        assert!(!gate_interval_matches(
            0,
            0,
            boundary,
            boundary + 180180,
            &next,
            &[]
        ));
        let mut stale = previous;
        stale.source = media("gate-red", -20_000, 0);
        assert!(!gate_interval_matches(
            0,
            0,
            boundary,
            boundary + 180180,
            &next,
            &[stale]
        ));
    }
    #[test]
    fn gate_observer_checks_four_boundary_cases() {
        use super::{ScheduledProgramme as P, SegmentIdentity as I, SegmentSource as S};
        let media = |pos: i64| {
            let id = if pos % 2 == 0 {
                "gate-red"
            } else {
                "gate-blue"
            };
            S::ScheduledMedia {
                id: id.into(),
                scheduled_programme: P {
                    id: id.into(),
                    title: id.into(),
                    start_ms: pos * 20_000,
                    end_ms: (pos + 1) * 20_000,
                },
            }
        };
        let recovery = S::Fallback {
            fallback_reason: Some("early_eof".into()),
            scheduled_programme: Some(P {
                id: "gate-red".into(),
                title: "gate-red".into(),
                start_ms: 0,
                end_ms: 20_000,
            }),
        };
        let edge = 20 * 90_000;
        let neighbour = I {
            sequence: 0,
            start_pdt_ns: 0,
            end_pdt_ns: 0,
            start_90k: edge - 9000,
            end_90k: edge - 1,
            source: recovery.clone(),
        };
        assert!(gate_interval_matches(
            0,
            0,
            edge - 1,
            edge + 3003,
            &media(1),
            &[neighbour.clone()]
        ));
        assert!(!gate_interval_matches(
            0,
            0,
            edge - 3004,
            edge + 3003,
            &media(1),
            &[neighbour.clone()]
        ));
        assert!(!gate_interval_matches(
            0,
            0,
            edge - 9630,
            edge + 9000,
            &media(1),
            &[neighbour.clone()]
        ));
        assert!(!gate_interval_matches(
            0,
            0,
            edge - 1,
            edge + 3003,
            &media(1),
            &[]
        ));
        let mut wrong = neighbour.clone();
        wrong.source = S::Fallback {
            fallback_reason: Some("scheduled_gap".into()),
            scheduled_programme: recovery.programme(),
        };
        assert!(!gate_interval_matches(
            0,
            0,
            edge - 1,
            edge + 3003,
            &media(1),
            &[wrong]
        ));
        let mut stale = neighbour;
        if let S::Fallback {
            scheduled_programme: Some(ref mut p),
            ..
        } = stale.source
        {
            p.start_ms = -20_000;
        }
        assert!(!gate_interval_matches(
            0,
            0,
            edge - 1,
            edge + 3003,
            &media(1),
            &[stale]
        ));
        assert!(gate_interval_matches(
            0,
            0,
            edge,
            edge + 3003,
            &media(1),
            &[]
        ));
        assert!(!gate_interval_matches(
            0,
            0,
            edge,
            edge + 3003,
            &media(0),
            &[]
        ));
        // Media to recovery, recovery to media, and consecutive recovery slots.
        assert!(gate_interval_matches(
            0,
            0,
            edge - 3003,
            edge + 1,
            &recovery,
            &[]
        ));
        assert!(!gate_interval_matches(0, 0, -1, 3003, &recovery, &[]));
        let unavailable = S::Fallback {
            fallback_reason: Some("source_unavailable".into()),
            scheduled_programme: Some(P {
                id: "test-resolver-stall-gate".into(),
                title: "stall".into(),
                start_ms: 40_000,
                end_ms: 60_000,
            }),
        };
        assert!(gate_interval_matches(
            1,
            0,
            40 * 90_000 - 1,
            40 * 90_000 + 3003,
            &unavailable,
            &[]
        ));
        assert!(gate_interval_matches(
            1,
            0,
            40 * 90_000,
            40 * 90_000 + 3003,
            &unavailable,
            &[]
        ));
        let mut wrong_context = unavailable.clone();
        if let S::Fallback {
            scheduled_programme: Some(ref mut p),
            ..
        } = wrong_context
        {
            p.end_ms += 1;
        }
        assert!(!gate_interval_matches(
            1,
            0,
            40 * 90_000,
            40 * 90_000 + 3003,
            &wrong_context,
            &[]
        ));
        let mut wrong_reason = unavailable;
        if let S::Fallback {
            fallback_reason: Some(ref mut r),
            ..
        } = wrong_reason
        {
            *r = "encoder_stall".into();
        }
        assert!(!gate_interval_matches(
            1,
            0,
            40 * 90_000,
            40 * 90_000 + 3003,
            &wrong_reason,
            &[]
        ));
    }

    fn fixture_ts() -> [u8; 188] {
        let mut packet = [0xff; 188];
        packet[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10]);
        packet
    }

    #[test]
    fn transport_counters_preserve_payload_and_reject_bad_packets() {
        let packet = |pid: u16, control: u8, cc: u8| {
            let mut p = [0xff; 188];
            p[0] = 0x47;
            p[1] = (pid >> 8) as u8;
            p[2] = pid as u8;
            p[3] = control << 4 | cc;
            if control & 2 != 0 {
                p[4] = if control == 2 { 183 } else { 1 };
                p[5] = 0;
            }
            p
        };
        let mut state = super::TransportCounters::default();
        let mut a = packet(256, 1, 7);
        state.normalize(&mut a).unwrap();
        let mut b = packet(256, 1, 0);
        state.normalize(&mut b).unwrap();
        assert_eq!(b[3] & 15, 8);
        let mut c = packet(256, 2, 0);
        state.normalize(&mut c).unwrap();
        assert_eq!(c[3] & 15, 8);
        let mut d = packet(256, 3, 0);
        let original = d;
        state.normalize(&mut d).unwrap();
        assert_eq!(d[3] & 15, 9);
        assert_eq!(&d[4..], &original[4..]);
        let mut bad = packet(256, 1, 0);
        bad[1] |= 0x80;
        assert!(state.normalize(&mut bad).is_err());
        assert!(state.normalize(&mut [0; 187]).is_err());
    }

    #[test]
    fn invalid_transport_never_changes_publication_or_counter_state() {
        let root = std::env::temp_dir().join(format!("bad-ts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.ts");
        let mut publisher = super::Publisher::new(root.clone(), 0, 4);
        std::fs::write(&source, fixture_ts()).unwrap();
        publisher
            .publish(
                &source,
                90_000,
                None,
                100_000,
                0,
                super::SegmentSource::Slate,
            )
            .unwrap();
        let playlist = std::fs::read(root.join("index.m3u8")).unwrap();
        let mut invalid = fixture_ts();
        invalid[1] |= 0x80;
        std::fs::write(&source, invalid).unwrap();
        assert!(
            publisher
                .publish(
                    &source,
                    90_000,
                    None,
                    100_000,
                    0,
                    super::SegmentSource::Slate
                )
                .is_err()
        );
        assert_eq!(std::fs::read(root.join("index.m3u8")).unwrap(), playlist);
        assert_eq!(publisher.next_sequence, 1);
        std::fs::write(&source, fixture_ts()).unwrap();
        publisher
            .publish(
                &source,
                90_000,
                None,
                100_000,
                0,
                super::SegmentSource::Slate,
            )
            .unwrap();
        let published = std::fs::read(root.join(&publisher.entries.last().unwrap().name)).unwrap();
        assert_eq!(published[3] & 15, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn production_fifo_spool_drains_after_publication_releases_permits() {
        use std::time::{Duration, Instant};
        let root =
            std::env::temp_dir().join(format!("private-fifo-production-{}", uuid::Uuid::new_v4()));
        let public = root.join("public");
        std::fs::create_dir_all(&public).unwrap();
        let mut spool = super::PrivateSpool::new(root.join("spool")).unwrap();
        let mut command = std::process::Command::new("ffmpeg");
        command.args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=s=64x64:r=25",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-g",
            "25",
            "-sc_threshold",
            "0",
        ]);
        spool.start_bounded(command, "1", 7000).unwrap();
        let mut held = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while held.len() < 3 && Instant::now() < deadline {
            match spool.take_ready_segment().unwrap() {
                super::TakeReady::Segment(segment) => held.push(segment),
                super::TakeReady::Pending => std::thread::sleep(Duration::from_millis(10)),
                _ => panic!("encoder ended before three segments"),
            }
        }
        assert_eq!(held.len(), 3);
        std::thread::sleep(Duration::from_millis(200));
        assert!(matches!(
            spool.take_ready_segment().unwrap(),
            super::TakeReady::Pending
        ));
        let mut publisher = super::Publisher::new(public, 1_700_000_000_000, 10);
        let mut count = 0;
        loop {
            if !held.is_empty() {
                let segment = held.remove(0);
                assert!(
                    publisher
                        .publish(
                            &segment.path,
                            segment.duration_ticks,
                            None,
                            publisher.anchor_ms + 100_000,
                            0,
                            super::SegmentSource::Slate
                        )
                        .unwrap()
                );
                count += 1;
            } else {
                match spool.take_ready_segment().unwrap() {
                    super::TakeReady::Segment(segment) => held.push(segment),
                    super::TakeReady::Pending => std::thread::sleep(Duration::from_millis(10)),
                    super::TakeReady::End => break,
                    super::TakeReady::Failed => panic!("FIFO encoder failed"),
                }
            }
            assert!(Instant::now() < deadline, "drain timed out");
        }
        assert!(count > 3, "only {count} segments published");
        assert_eq!(publisher.entries.len(), count);
        drop(spool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fifo_record_validates_slot_and_times() {
        assert_eq!(
            super::fifo_record("slot_0.ts,0.000000,2.002000\n", 0, None).unwrap(),
            (0, 180180)
        );
        assert_eq!(
            super::fifo_record("slot_1.ts,2.002000,4.000000\n", 1, Some(180180)).unwrap(),
            (180180, 360000)
        );
        for record in [
            "slot_1.ts,0,2\n",
            "../slot_0.ts,0,2\n",
            "slot_0.ts,2,1\n",
            "slot_0.ts,0,2",
            "slot_0.ts,0,2,3\n",
        ] {
            assert!(super::fifo_record(record, 0, None).is_err(), "{record}");
        }
        assert!(super::fifo_record("slot_1.ts,2.1,4\n", 1, Some(180180)).is_err());
        assert!(
            super::fifo_record(&format!("slot_0.ts,0,{}\n", "1".repeat(512)), 0, None).is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn fifo_private_item_runs_existing_slate_codec_and_retires() {
        use std::time::Instant;
        let root = std::env::temp_dir().join(format!("fifo-item-{}", uuid::Uuid::new_v4()));
        let channel = crate::db::Channel {
            id: "fifo".into(),
            number: "99".into(),
            name: "fifo".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics: Default::default(),
            stream: Default::default(),
        };
        let mut command = super::slate_command(&channel);
        command.args(["-t", "6"]);
        let item = super::PrivateItem::start_fifo(root.clone(), command, None).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        let segment = loop {
            match item.try_recv_fifo() {
                Ok(Ok(segment)) => break segment,
                Ok(Err(error)) => panic!("FIFO error: {error}"),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("FIFO ended before segment")
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "segment deadline");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        };
        assert!(std::fs::metadata(&segment.path).unwrap().len() > 0);
        let before = Instant::now();
        drop(item);
        assert!(before.elapsed() < std::time::Duration::from_secs(1));
        assert!(segment.path.exists());
        drop(segment);
        while root.exists() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn throttle_counts_unpublished_lease_and_resumes() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("capacity-{}", uuid::Uuid::new_v4()));
        let mut spool = super::PrivateSpool::new(root.clone()).unwrap();
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=s=64x64:r=25",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-g",
            "25",
            "-sc_threshold",
            "0",
        ]);
        spool.start_bounded(cmd, "1", 7000).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut held = Vec::new();
        while held.len() < 3 {
            assert!(Instant::now() < deadline);
            match spool.take_ready_segment().unwrap() {
                super::TakeReady::Segment(segment) => held.push(segment),
                super::TakeReady::Pending => std::thread::yield_now(),
                _ => panic!("ended before capacity"),
            }
        }
        assert!(matches!(
            spool.take_ready_segment().unwrap(),
            super::TakeReady::Pending
        ));
        drop(held.remove(0));
        loop {
            assert!(Instant::now() < deadline, "capacity did not resume");
            match spool.take_ready_segment().unwrap() {
                super::TakeReady::Segment(_) => break,
                super::TakeReady::Pending => std::thread::yield_now(),
                _ => panic!("ended before resuming"),
            }
        }
        drop(held);
        drop(spool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lookahead_uses_closed_durations_including_current_lease() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("lookahead-{}", uuid::Uuid::new_v4()));
        let mut spool = super::PrivateSpool::new(root.clone()).unwrap();
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=s=64x64:r=25",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-g",
            "25",
            "-sc_threshold",
            "0",
        ]);
        spool.start_bounded(cmd, "1", 2000).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut durations = Vec::new();
        let mut current = None;
        loop {
            assert!(Instant::now() < deadline);
            match spool.take_ready_segment().unwrap() {
                super::TakeReady::Segment(segment) => {
                    durations.push(segment.duration_ticks);
                    if current.is_none() {
                        current = Some(segment);
                    }
                }
                super::TakeReady::Pending => std::thread::yield_now(),
                super::TakeReady::End => break,
                super::TakeReady::Failed => panic!("encoder failed"),
            }
        }
        assert_eq!(durations.len(), 2);
        assert_eq!(current.as_ref().unwrap().duration_ticks, durations[0]);
        let published = 180_000;
        let end = published + durations.iter().sum::<i64>();
        assert_eq!(
            super::preparation_cursor(1_000_000, end),
            1_000_000 + (end + 89) / 90
        );
        assert_eq!(end, 360_000);
        drop(current);
        drop(spool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn regression_admission_remains_charged_until_reaped() {
        let slots = std::sync::atomic::AtomicUsize::new(0);
        assert!(super::reserve_encoder(&slots, 1));
        assert!(!super::reserve_encoder(&slots, 1));
        slots.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        assert!(super::reserve_encoder(&slots, 1));
    }

    #[test]
    fn regression_cancelled_encoder_is_reaped_without_blocking_caller() {
        let root = std::env::temp_dir().join(format!("encoder-reap-{}", uuid::Uuid::new_v4()));
        let mut spool = super::PrivateSpool::new(root.clone()).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        spool.start_bounded(command, "1", 1000).unwrap();
        let dir = spool.items[0].dir.clone();
        let started = std::time::Instant::now();
        spool.items.remove(0);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while dir.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!dir.exists(), "reaper did not clean exited child");
        drop(spool);
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn regression_reaper_retains_live_child_directory_and_cache_lease() {
        let root = std::env::temp_dir().join(format!("reaper-live-{}", uuid::Uuid::new_v4()));
        let cache = crate::precache::LeaseTestCache::new(root.join("cache"), &["one"]);
        let mut spool = super::PrivateSpool::new(root.join("spool")).unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exec sleep 60"]);
        spool
            .start_bounded_leased(
                cmd,
                "1",
                1000,
                Some(super::super::InputLease::Full(cache.lease("one"))),
            )
            .unwrap();
        let dir = spool.items[0].dir.clone();
        let fifo = spool.items[0].fifo.as_ref().unwrap();
        let pid = fifo.pid();
        assert!(!fifo.worker_finished());
        assert!(dir.exists());
        assert!(!cache.pressure());
        drop(spool);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cache.pressure() {
            assert!(
                std::time::Instant::now() < deadline,
                "lease not released after worker reaping"
            );
            std::thread::yield_now();
        }
        assert!(!dir.exists());
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn regression_preparation_tracks_actual_encoded_clock() {
        let root = std::env::temp_dir().join(format!("cursor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("input.ts");
        std::fs::write(&source, fixture_ts()).unwrap();
        let anchor = 1_000_000;
        let mut publisher = super::Publisher::new(root.clone(), anchor, 4);
        for generation in 0..101 {
            let start = super::preparation_cursor(anchor, publisher.cursor);
            assert_eq!(start, anchor + generation * 10_010);
            assert!(
                publisher
                    .publish(
                        &source,
                        10_010 * 90,
                        Some(start),
                        anchor + 2_000_000,
                        90_000,
                        super::SegmentSource::Slate
                    )
                    .unwrap()
            );
        }
        assert_eq!(
            super::preparation_cursor(anchor, publisher.cursor),
            anchor + 1_011_010
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn warm_reads_small_and_large_files_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("warm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small");
        std::fs::write(&small, vec![1u8; 1000]).unwrap();
        assert_eq!(super::warm_file_head_tail(&small).unwrap(), 1000);
        let large = dir.join("large");
        std::fs::write(&large, vec![2u8; 10 * 1024 * 1024]).unwrap();
        assert_eq!(
            super::warm_file_head_tail(&large).unwrap(),
            2 * super::WARM_BYTES
        );
        assert!(super::warm_file_head_tail(&dir.join("missing")).is_err());
        let mut warmed = std::collections::HashMap::new();
        assert!(super::should_warm(&mut warmed, "a", 0));
        assert!(!super::should_warm(&mut warmed, "a", 1000));
        assert!(super::should_warm(
            &mut warmed,
            "a",
            super::WARM_RECENT_MS + 1
        ));
        assert!(super::should_warm(
            &mut warmed,
            "b",
            super::WARM_RECENT_MS + 2
        ));
        assert_eq!(warmed.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[tokio::test]
    async fn scheduled_cache_resolution_pins_complete_copy_and_falls_back() {
        use std::sync::Arc;
        let root = std::env::temp_dir().join(format!("scheduled-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let mut config = crate::config::Config::from_env();
        config.data_dir = root.clone();
        config.precache_dir = root.join("cache");
        let cache = Arc::new(crate::precache::Precache::new(&config, pool.clone()));
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let source = root.join("nas-source");
        let copy = config.precache_dir.join("copy");
        std::fs::write(&copy, b"data").unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('item',?,'item','movie',1000,0)")
            .bind(source.to_str().unwrap()).execute(&pool).await.unwrap();
        crate::db::upsert_precache(
            &pool,
            "item",
            source.to_str().unwrap(),
            4,
            1,
            copy.to_str().unwrap(),
            "copy",
            1,
            1,
        )
        .await
        .unwrap();
        crate::db::set_precache_status(&pool, "item", "ready", 4, None)
            .await
            .unwrap();
        let (input, lease) = super::resolve_scheduled_input(
            &pool,
            &cloud,
            Some(&cache),
            "item",
            crate::cloud::manager::ResolutionTicket::new(),
        )
        .await
        .unwrap();
        assert_eq!(input.location, copy.to_str().unwrap());
        assert!(cache.remove("item").await.is_err());
        let new_source = root.join("new-source");
        std::fs::write(&new_source, b"new source").unwrap();
        sqlx::query("UPDATE media_items SET path = ? WHERE id = 'item'")
            .bind(new_source.to_str().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        let (new_input, new_lease) = super::resolve_scheduled_input(
            &pool,
            &cloud,
            Some(&cache),
            "item",
            crate::cloud::manager::ResolutionTicket::new(),
        )
        .await
        .unwrap();
        assert_eq!(new_input.location, new_source.to_str().unwrap());
        assert!(new_lease.is_none());
        assert_eq!(
            std::fs::read(lease.as_ref().unwrap().path()).unwrap(),
            b"data"
        );
        sqlx::query("UPDATE media_items SET path = ? WHERE id = 'item'")
            .bind(source.to_str().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        drop(lease);
        cache.remove("item").await.unwrap();
        assert!(!copy.exists());
        std::fs::write(&source, b"source").unwrap();
        let (input, lease) = super::resolve_scheduled_input(
            &pool,
            &cloud,
            Some(&cache),
            "item",
            crate::cloud::manager::ResolutionTicket::new(),
        )
        .await
        .unwrap();
        assert_eq!(input.location, source.to_str().unwrap());
        assert!(lease.is_none());
        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_encoder_start_releases_cache_lease() {
        let root = std::env::temp_dir().join(format!("cache-start-{}", uuid::Uuid::new_v4()));
        let cache = crate::precache::LeaseTestCache::new(root.join("cache"), &["one"]);
        let mut spool = super::PrivateSpool::new(root.join("spool")).unwrap();
        let cmd = std::process::Command::new(root.join("missing-executable"));
        assert!(
            spool
                .start_bounded_leased(
                    cmd,
                    "2",
                    1000,
                    Some(super::super::InputLease::Full(cache.lease("one")))
                )
                .is_err()
        );
        assert!(cache.pressure());
        assert!(!root.join("cache/one").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn queued_cache_leases_survive_until_children_reaped() {
        let root = std::env::temp_dir().join(format!("cache-children-{}", uuid::Uuid::new_v4()));
        let cache = crate::precache::LeaseTestCache::new(root.join("cache"), &["one", "two"]);
        let mut spool = super::PrivateSpool::new(root.join("spool")).unwrap();
        for name in ["one", "two"] {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "exec sleep 60"]);
            spool
                .start_bounded_leased(
                    cmd,
                    "2",
                    2000,
                    Some(super::super::InputLease::Full(cache.lease(name))),
                )
                .unwrap();
        }
        assert!(!cache.pressure());
        assert!(root.join("cache/one").exists() && root.join("cache/two").exists());
        let first_dir = spool.items[0].dir.clone();
        drop(spool.items.remove(0));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while first_dir.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!first_dir.exists());
        while !cache.pressure() {
            assert!(
                std::time::Instant::now() < deadline,
                "first lease not released"
            );
            std::thread::yield_now();
        }
        assert!(!root.join("cache/one").exists());
        assert!(root.join("cache/two").exists());
        let second_dir = spool.items[0].dir.clone();
        drop(spool);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while second_dir.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!second_dir.exists());
        while !cache.pressure_all() {
            assert!(
                std::time::Instant::now() < deadline,
                "second lease not released"
            );
            std::thread::yield_now();
        }
        assert!(!root.join("cache/two").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    // Captured rollback HTTP playlist with only the channel URI prefix removed.
    const RETAINED_PLAYLIST: &str = r#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-DISCONTINUITY
#EXT-X-INDEPENDENT-SEGMENTS
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:26.253+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00000.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:30.257+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00001.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:34.261+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00002.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:38.265+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00003.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:42.269+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00004.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:46.273+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00005.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:50.277+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00006.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:54.281+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00007.ts
#EXTINF:3.970633,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:58.285+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00008.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:04:02.256+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00009.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:04:06.260+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00010.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:04:10.264+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00011.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:04:14.268+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00012.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:04:18.272+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00013.ts
#EXTINF:4.004000,
#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:04:22.276+0000
seg_bd145537-78ac-5c2c-a645-dd4e34164bd0_1795093393190_e9d2735be5884cc79e8b88aed9d4db13_00000_00014.ts
"#;

    #[test]
    fn retained_legacy_playlist_resumes_without_rewind() {
        let root = std::env::temp_dir().join(format!("legacy-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for name in RETAINED_PLAYLIST.lines().filter(|s| s.starts_with("seg_")) {
            std::fs::write(root.join(name), b"retained").unwrap();
        }
        std::fs::write(root.join("index.m3u8"), RETAINED_PLAYLIST).unwrap();
        let mut publisher = super::Publisher::resume(root.clone(), 0, 4).unwrap();
        assert_eq!(publisher.entries.len(), 15);
        assert_eq!(publisher.next_sequence, 15);
        assert_eq!(publisher.discontinuity_sequence, 0);
        assert!(publisher.entries[0].discontinuity);
        let previous = publisher.entries.clone();
        let end = publisher.cursor;
        let source = root.join("source");
        std::fs::write(&source, fixture_ts()).unwrap();
        assert!(
            publisher
                .publish(
                    &source,
                    180_000,
                    None,
                    2_000_000_000_000,
                    0,
                    super::SegmentSource::Unknown
                )
                .unwrap()
        );
        assert_eq!(publisher.entries.len(), 16);
        for (old, current) in previous.iter().zip(&publisher.entries) {
            assert_eq!(
                (&old.name, old.sequence, old.date_ticks),
                (&current.name, current.sequence, current.date_ticks)
            );
        }
        let next = publisher.entries.last().unwrap();
        assert_eq!(next.sequence, 15);
        assert!(next.discontinuity);
        assert!(next.date_ticks >= end);
        assert!(
            next.date_ticks
                >= previous.last().unwrap().date_ticks + previous.last().unwrap().duration_ticks
        );
        let playlist = std::fs::read_to_string(root.join("index.m3u8")).unwrap();
        assert!(playlist.contains("2026-11-19T13:03:26.253Z"));
        assert!(playlist.contains(&previous[0].name));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_sliding_legacy_window_retains_discontinuity_numbers() {
        let root = std::env::temp_dir().join(format!("legacy-window-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut text = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:42\n");
        for i in 0..4 {
            let name = format!("seg_old_{i}.ts");
            std::fs::write(root.join(&name), b"old").unwrap();
            if i == 0 || i == 2 {
                text.push_str("#EXT-X-DISCONTINUITY\n");
            }
            text.push_str(&format!(
                "#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:{:02}Z\n#EXTINF:4,\n{name}\n",
                26 + 4 * i
            ));
        }
        std::fs::write(root.join("index.m3u8"), text).unwrap();
        let mut p = super::Publisher::resume(root.clone(), 0, 4).unwrap();
        assert_eq!(p.discontinuity_sequence, 0);
        let source = root.join("source");
        std::fs::write(&source, fixture_ts()).unwrap();
        p.publish(
            &source,
            360_000,
            None,
            2_000_000_000_000,
            0,
            super::SegmentSource::Unknown,
        )
        .unwrap();
        assert_eq!(p.entries[4].sequence, 46);
        assert!(p.entries[4].discontinuity);
        p.publish(
            &source,
            360_000,
            None,
            2_000_000_000_000,
            0,
            super::SegmentSource::Unknown,
        )
        .unwrap();
        assert_eq!(p.entries[0].sequence, 44);
        assert_eq!(p.discontinuity_sequence, 1);
        assert!(p.entries[0].discontinuity);
        let restored = super::Publisher::resume(root.clone(), 0, 4).unwrap();
        assert_eq!(restored.discontinuity_sequence, 1);
        assert_eq!(restored.next_sequence, 48);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_resumed_prefix_is_retired_on_first_publication() {
        let root = std::env::temp_dir().join(format!("stale-window-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut text = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:42\n");
        for i in 0..4 {
            let name = format!("seg_old_{i}.ts");
            std::fs::write(root.join(&name), b"old").unwrap();
            if i == 0 || i == 2 {
                text.push_str("#EXT-X-DISCONTINUITY\n");
            }
            text.push_str(&format!(
                "#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:{:02}Z\n#EXTINF:4,\n{name}\n",
                26 + 4 * i
            ));
        }
        std::fs::write(root.join("index.m3u8"), text).unwrap();
        let mut p = super::Publisher::resume(root.clone(), 0, 4).unwrap();
        p.cursor += 3_600 * super::TICKS;
        p.stale_prefix = true;
        let source = root.join("source");
        std::fs::write(&source, fixture_ts()).unwrap();
        let end = p.cursor + 360_000;
        p.publish(
            &source,
            end,
            None,
            2_000_000_000_000,
            0,
            super::SegmentSource::Unknown,
        )
        .unwrap();
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].sequence, 46);
        assert!(p.entries[0].discontinuity);
        assert_eq!(p.discontinuity_sequence, 2);
        assert!(!p.stale_prefix);
        let playlist = std::fs::read_to_string(root.join("index.m3u8")).unwrap();
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:46") && !playlist.contains("seg_old_"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_rejects_unsafe_legacy_paths() {
        let root = std::env::temp_dir().join(format!("unsafe-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for name in [
            "../seg_bad.ts",
            "seg_bad/evil.ts",
            "seg_bad%2fts",
            "seg_bad?.ts",
        ] {
            let text = format!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PROGRAM-DATE-TIME:2026-11-19T13:03:26Z\n#EXTINF:4,\n{name}\n"
            );
            std::fs::write(root.join("index.m3u8"), text).unwrap();
            assert!(
                super::Publisher::resume(root.clone(), 0, 4).is_err(),
                "{name}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    // Long-running release-gate fixture; run explicitly with --ignored --nocapture.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn sustained_multichannel_publication_gate() {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter("awesometv_server::engine::private_publisher=info")
            .try_init();
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("publication-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        for (id, color) in [("gate-red", "red"), ("gate-blue", "blue")] {
            let path = root.join(format!("{id}.mp4"));
            assert!(
                std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=c={color}:s=160x90:r=30:d=20"))
                    .args([
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:sample_rate=48000:duration=20",
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
        }
        let stall = "test-resolver-stall-gate";
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,'stall','movie',20000,0)")
            .bind(stall).bind(root.join("missing").to_str().unwrap()).execute(&pool).await.unwrap();
        let encoder_stall = "test-encoder-stall-multichannel";
        std::fs::copy(root.join("gate-red.mp4"), root.join("stall.mp4")).unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,'stall','movie',20000,0)")
            .bind(encoder_stall).bind(root.join("stall.mp4").to_str().unwrap()).execute(&pool).await.unwrap();
        let start = crate::db::now_ms();
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let mut tasks = Vec::new();
        let mut snapshots = Vec::new();
        for n in 0..3 {
            let mut channel =
                crate::db::create_channel_shell(&pool, &format!("{}", 910 + n), "gate")
                    .await
                    .unwrap();
            channel.stream.segment_seconds = 2;
            channel.stream.cache_segments = 12;
            channel.stream.prebuffer_seconds = 8;
            channel.stream.preset = "ultrafast".into();
            channel.stream.width = 160;
            channel.stream.height = 90;
            for pos in 0..10_i64 {
                let id = if n == 1 && pos == 2 {
                    stall
                } else if n == 2 && pos == 2 {
                    encoder_stall
                } else if pos % 2 == 0 {
                    "gate-red"
                } else {
                    "gate-blue"
                };
                sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,20000,'program')")
                    .bind(&channel.id).bind(pos).bind(id).bind(id).bind(start+pos*20000).bind(start+(pos+1)*20000).execute(&pool).await.unwrap();
            }
            let out = root.join(format!("out-{n}"));
            std::fs::create_dir_all(&out).unwrap();
            let shared: super::super::SnapshotHandle = Default::default();
            let ready = Arc::new(AtomicBool::new(false));
            tasks.push(tokio::spawn(super::run_channel(
                pool.clone(),
                cloud.clone(),
                None,
                channel,
                root.clone(),
                out,
                0,
                stop.clone(),
                ready,
                start,
                shared.clone(),
            )));
            snapshots.push(shared);
        }
        let begun = Instant::now();
        let mut max_lag = [0_i64; 3];
        let mut starved = [0_u32; 3];
        let mut mismatches = [0_u32; 3];
        let mut last_seq = [None; 3];
        let mut seen = [0_u32; 3];
        let mut slate = [0_u32; 3];
        while begun.elapsed() < Duration::from_secs(185) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if tasks.iter().any(|task| task.is_finished()) {
                eprintln!("GATE publisher exited early");
                break;
            }
            for (n, shared) in snapshots.iter().enumerate() {
                let guard = shared.lock().unwrap();
                if let Some(last) = guard.segments.last() {
                    let now = crate::db::now_ms();
                    let lag = now - (last.end_pdt_ns / 1_000_000) as i64;
                    max_lag[n] = max_lag[n].max(lag);
                    if begun.elapsed() > Duration::from_secs(12) && lag > 4000 {
                        starved[n] += 1;
                        eprintln!(
                            "GATE starvation channel={n} elapsed_ms={} age_ms={lag} newest_seq={} newest_source={:?}",
                            begun.elapsed().as_millis(),
                            last.sequence,
                            last.source
                        );
                    }
                    for seg in guard
                        .segments
                        .iter()
                        .take(guard.segments.len().saturating_sub(1))
                    {
                        if last_seq[n].is_some_and(|seq| seg.sequence <= seq) {
                            continue;
                        }
                        seen[n] += 1;
                        last_seq[n] = Some(seg.sequence);
                        if gate_interval_matches(
                            n,
                            start,
                            seg.start_90k - start as i128 * 90,
                            seg.end_90k - start as i128 * 90,
                            &seg.source,
                            &guard.segments,
                        ) {
                            if seg.source.is_slate() {
                                slate[n] += 1;
                            }
                        } else {
                            mismatches[n] += 1;
                            eprintln!(
                                "GATE mismatch channel={n} seq={} start_90k={} end_90k={} source={:?}",
                                seg.sequence,
                                seg.start_90k - start as i128 * 90,
                                seg.end_90k - start as i128 * 90,
                                seg.source
                            );
                        }
                    }
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        for task in tasks {
            eprintln!(
                "GATE task result: {:?}",
                tokio::time::timeout(Duration::from_secs(5), task).await
            );
        }
        eprintln!(
            "GATE duration_s={} max_lag_ms={max_lag:?} starvation_samples={starved:?} segments={seen:?} label_mismatches={mismatches:?} stall_slate={slate:?}",
            begun.elapsed().as_secs()
        );
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            begun.elapsed() >= Duration::from_secs(185),
            "publisher exited early"
        );
        assert_eq!(starved, [0; 3], "publication starvation");
        assert_eq!(mismatches, [0; 3], "labels differ from scheduled source");
        assert!(slate[1] > 0 && slate[2] > 0 && seen.iter().all(|n| *n > 60));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_waits_for_closed_public_media_and_continues() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("startup-watermark-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "998", "startup")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 8;
        channel.stream.prebuffer_seconds = 4;
        channel.stream.preset = "ultrafast".into();
        channel.stream.width = 160;
        channel.stream.height = 90;
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let now = crate::db::now_ms();
        let source = root.join("old.ts");
        std::fs::write(&source, fixture_ts()).unwrap();
        let mut old = super::Publisher::new(out.clone(), now - 3_600_000, 8);
        old.publish(
            &source,
            2 * super::TICKS,
            None,
            now,
            0,
            super::SegmentSource::Unknown,
        )
        .unwrap();
        let playlist = out.join("index.m3u8");
        let shared: super::super::SnapshotHandle = Default::default();
        let task = tokio::spawn(super::run_channel(
            pool.clone(),
            cloud,
            None,
            channel,
            root.clone(),
            out.clone(),
            0,
            stop.clone(),
            ready.clone(),
            now,
            shared.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let text = tokio::fs::read_to_string(&playlist).await.unwrap();
                // The hour-old prefix is retired by the first current segment.
                if text.contains("#EXT-X-MEDIA-SEQUENCE:1\n") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !ready.load(Ordering::Acquire),
            "one segment must not satisfy four seconds"
        );
        super::super::wait_for_private_ready(&playlist, &ready, Duration::from_secs(12))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&playlist).unwrap();
        let segments: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("segment_"))
            .collect();
        assert!(segments.len() >= 2 && !text.contains(&old.entries[0].name));
        {
            let snapshot = shared.lock().unwrap();
            assert!(snapshot.new_published_seconds >= 4.0);
            assert!(snapshot.segments.iter().all(|s| matches!(&s.source, super::SegmentSource::Fallback { fallback_reason: Some(reason), scheduled_programme: None } if reason == "scheduled_gap")));
            assert!(snapshot.segments.len() <= 8);
        }
        assert!(text.contains("#EXT-X-DISCONTINUITY"));
        let dates: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .collect();
        let latest = time::OffsetDateTime::parse(
            dates.last().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
        .unix_timestamp_nanos()
            / 1_000_000;
        assert!(
            (latest - now as i128).abs() < 10_000,
            "resumed segment must follow the current schedule"
        );
        for segment in &segments {
            assert!(
                std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(out.join(segment))
                    .args(["-frames:v", "1", "-f", "null", "-"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let current = tokio::fs::read_to_string(&playlist).await.unwrap();
                if current
                    .lines()
                    .filter(|line| line.starts_with("segment_"))
                    .count()
                    > segments.len()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("publication continues after readiness");
        stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let resumed = super::Publisher::resume(out.clone(), crate::db::now_ms(), 8).unwrap();
        assert!(
            resumed.next_sequence >= 3,
            "resume retains advertised sequence after the jump"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn encoder_stall_recovers_within_playable_buffer() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::{Duration, Instant};
        let root =
            std::env::temp_dir().join(format!("encoder-stall-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let id = "test-encoder-stall-gate";
        let path = root.join("source.mp4");
        std::fs::write(&path, b"present").unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,'stall','movie',20000,0)")
            .bind(id).bind(path.to_str().unwrap()).execute(&pool).await.unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "996", "stall")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 8;
        channel.stream.prebuffer_seconds = 4;
        channel.stream.preset = "ultrafast".into();
        channel.stream.width = 160;
        channel.stream.height = 90;
        let start = crate::db::now_ms();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,'stall',?,?,0,20000,'program')")
            .bind(&channel.id).bind(id).bind(start).bind(start+20000).execute(&pool).await.unwrap();
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let shared: super::super::SnapshotHandle = Default::default();
        let task = tokio::spawn(super::run_channel(
            pool.clone(),
            cloud,
            None,
            channel,
            root.clone(),
            out,
            0,
            stop.clone(),
            ready,
            start,
            shared.clone(),
        ));
        let begun = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(35), async {
            loop {
                if shared
                    .lock()
                    .unwrap()
                    .segments
                    .iter()
                    .any(|s| s.source == super::SegmentSource::Slate)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let elapsed = begun.elapsed();
        stop.store(true, Ordering::Relaxed);
        let task_result = tokio::time::timeout(Duration::from_secs(5), task).await;
        eprintln!(
            "STALL recovery_ms={} result={result:?} task={task_result:?}",
            elapsed.as_millis()
        );
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            result.is_ok() && elapsed < Duration::from_secs(8),
            "fallback exceeded playable buffer"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_stall_publishes_slate() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("resolver-slate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let id = format!("test-resolver-stall-{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,'test','movie',10000,0)")
            .bind(&id).bind(root.join("missing").to_str().unwrap()).execute(&pool).await.unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "997", "test")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.cache_segments = 4;
        channel.stream.prebuffer_seconds = 0;
        channel.stream.preset = "ultrafast".into();
        channel.stream.width = 160;
        channel.stream.height = 90;
        let start = crate::db::now_ms();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,'test',?,?,0,10000,'program')")
            .bind(&channel.id).bind(&id).bind(start).bind(start + 10000).execute(&pool).await.unwrap();
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let shared: super::super::SnapshotHandle = Default::default();
        let task = tokio::spawn(super::run_channel(
            pool.clone(),
            cloud,
            None,
            channel,
            root.clone(),
            out.clone(),
            0,
            stop.clone(),
            ready,
            start,
            shared.clone(),
        ));
        let begun = Instant::now();
        let segment = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if let Ok(playlist) = tokio::fs::read_to_string(out.join("index.m3u8")).await {
                    if let Some(name) = playlist.lines().find(|line| line.starts_with("segment_")) {
                        break out.join(name);
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("slate segment published");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !shared.lock().unwrap().segments.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(&shared.lock().unwrap().segments[0].source, super::SegmentSource::Fallback { fallback_reason: Some(reason), scheduled_programme: Some(programme) } if reason == "source_unavailable" && programme.id == id)
        );
        assert!(begun.elapsed() >= Duration::from_secs(2));
        assert!(
            begun.elapsed() < Duration::from_secs(4),
            "slate publication exceeded deadline allowance"
        );
        let video = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(segment)
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
        assert!(
            video.status.success()
                && video.stdout.len() == 3
                && video.stdout.iter().all(|v| *v < 80),
            "slate: {:?}",
            video.stdout
        );
        stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolution_deadline_uses_slack() {
        use std::time::Duration;
        assert_eq!(
            super::resolution_deadline(1_000, 1_000),
            Duration::from_secs(2)
        );
        assert_eq!(
            super::resolution_deadline(12_000, 1_000),
            Duration::from_secs(6)
        );
        assert_eq!(
            super::resolution_deadline(100_000, 0),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn unavailable_slate_is_bounded() {
        assert_eq!(
            super::unavailable_slate_ms(30 * 60_000),
            super::UNAVAILABLE_RETRY_MS
        );
        assert_eq!(
            super::unavailable_slate_ms(super::UNAVAILABLE_RETRY_MS + 5_000),
            super::UNAVAILABLE_RETRY_MS + 5_000
        );
        assert_eq!(super::unavailable_slate_ms(4_000), 4_000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missed_resolution_recovers_within_programme() {
        super::TEST_RESOLVER_STALLED_ONCE.store(false, std::sync::atomic::Ordering::SeqCst);
        recovers_within_programme("test-resolver-once-", "source_unavailable").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn encoder_stall_recovers_within_programme() {
        super::TEST_ENCODER_STALLED_ONCE.store(false, std::sync::atomic::Ordering::SeqCst);
        recovers_within_programme("test-encoder-once-", "encoder_stall").await;
    }

    async fn recovers_within_programme(prefix: &str, reason: &str) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("resolver-recover-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let path = root.join("source.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=white:s=160x90:r=30:d=40",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=40",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
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
        let id = format!("{prefix}{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES (?,?,'recover','movie',40000,0)")
            .bind(&id).bind(path.to_str().unwrap()).execute(&pool).await.unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "995", "recover")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.prebuffer_seconds = 0;
        channel.stream.cache_segments = 30;
        channel.stream.width = 160;
        channel.stream.height = 90;
        channel.stream.preset = "ultrafast".into();
        let start = crate::db::now_ms();
        sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,0,?,'recover',?,?,0,40000,'program')")
            .bind(&channel.id).bind(&id).bind(start).bind(start + 40000).execute(&pool).await.unwrap();
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let shared: super::super::SnapshotHandle = Default::default();
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let task = tokio::spawn(super::run_channel(
            pool.clone(),
            cloud,
            None,
            channel,
            root.clone(),
            out,
            0,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
            start,
            shared.clone(),
        ));
        let recovered = tokio::time::timeout(Duration::from_secs(30), async { loop {
            if shared.lock().unwrap().segments.iter().any(|s| matches!(&s.source, super::SegmentSource::ScheduledMedia { id: m, .. } if *m == id)) { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }}).await;
        let segments = shared.lock().unwrap().segments.clone();
        stop.store(true, Ordering::Relaxed);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            recovered.is_ok(),
            "programme never left slate after a missed resolution"
        );
        assert!(
            matches!(&segments[0].source, super::SegmentSource::Fallback { fallback_reason: Some(r), .. } if r == reason)
        );
        let slate_ms: i128 = segments
            .iter()
            .take_while(|s| s.source.is_slate())
            .map(|s| (s.end_pdt_ns - s.start_pdt_ns) / 1_000_000)
            .sum();
        assert!(
            slate_ms <= (super::UNAVAILABLE_RETRY_MS + 2_000) as i128,
            "slate lasted {slate_ms} ms"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_successor_resolution_does_not_block_publication() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        super::TEST_RESOLVER_STARTED.store(false, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("resolver-progress-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = crate::db::init(&root).await.unwrap();
        let path = root.join("source.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=s=160x90:r=30:d=12",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=12",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
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
        sqlx::query("INSERT INTO media_items (id,path,title,kind,duration_ms,created_at) VALUES ('progress',?,'progress','movie',12000,0)").bind(path.to_str().unwrap()).execute(&pool).await.unwrap();
        let mut channel = crate::db::create_channel_shell(&pool, "996", "progress")
            .await
            .unwrap();
        channel.stream.segment_seconds = 2;
        channel.stream.prebuffer_seconds = 8;
        channel.stream.cache_segments = 30;
        channel.stream.width = 160;
        channel.stream.height = 90;
        channel.stream.preset = "ultrafast".into();
        let start = crate::db::now_ms();
        for (pos, id) in ["progress", "test-resolver-stall-progress"]
            .iter()
            .enumerate()
        {
            let pos = pos as i64;
            sqlx::query("INSERT INTO channel_items (channel_id,position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind) VALUES (?,?,?,?,?,?,0,12000,'program')")
                .bind(&channel.id).bind(pos).bind(id).bind(id).bind(start + pos * 12000).bind(start + (pos + 1) * 12000).execute(&pool).await.unwrap();
        }
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let shared: super::super::SnapshotHandle = Default::default();
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let task = tokio::spawn(super::run_channel(
            pool.clone(),
            cloud,
            None,
            channel,
            root.clone(),
            out,
            0,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
            start,
            shared.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !super::TEST_RESOLVER_STARTED.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let before = shared.lock().unwrap().segments.last().map(|s| s.sequence);
        tokio::time::timeout(Duration::from_millis(1900), async { loop {
            if shared.lock().unwrap().segments.last().is_some_and(|s| before.is_none_or(|n| s.sequence > n) && matches!(&s.source, super::SegmentSource::ScheduledMedia { id, .. } if id == "progress")) { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.expect("active publication stalled during successor resolution");
        tokio::time::timeout(Duration::from_secs(20), async { loop {
            if shared.lock().unwrap().segments.iter().any(|s| matches!(&s.source, super::SegmentSource::Fallback { fallback_reason: Some(reason), scheduled_programme: Some(p) } if reason == "source_unavailable" && p.id == "test-resolver-stall-progress")) { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }}).await.expect("correctly labelled fallback not published");
        stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_deadline_keeps_blocking_capacity_until_exit() {
        use crate::cloud::manager::{ResolvedMedia, admitted_blocking, publish_cache};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        };
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("resolver-deadline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = sqlx::SqlitePool::connect(&format!(
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
        let admission = Arc::new(tokio::sync::Semaphore::new(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let gate = admission.clone();
        let count = calls.clone();
        let temp = dir.join("stale.part");
        let target = dir.join("target.mp4");
        tokio::fs::write(&temp, b"stale").await.unwrap();
        let cache_pool = pool.clone();
        let cache_temp = temp.clone();
        let cache_target = target.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let start = std::time::Instant::now();
        let result =
            super::resolve_with_deadline(&stop, Duration::from_millis(80), |ticket| async move {
                tokio::spawn(async move {
                    admitted_blocking(gate, move || {
                        count.fetch_add(1, Ordering::SeqCst);
                        let _ = entered_tx.send(());
                        release_rx.recv().unwrap();
                    })
                    .await
                    .unwrap();
                    let result = publish_cache(
                        &cache_pool,
                        "item",
                        &cache_temp,
                        &cache_target,
                        5,
                        Some(&ticket),
                    )
                    .await;
                    let _ = done_tx.send(result.is_err());
                });
                std::future::pending::<anyhow::Result<ResolvedMedia>>().await
            })
            .await;
        entered_rx.await.unwrap();
        assert!(result.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(admission.available_permits(), 0);
        for _ in 0..5 {
            let gate = admission.clone();
            let count = calls.clone();
            assert!(
                super::resolve_with_deadline(&stop, Duration::from_millis(20), |_| async move {
                    admitted_blocking(gate, move || {
                        count.fetch_add(1, Ordering::SeqCst);
                    })
                    .await?;
                    Ok(ResolvedMedia {
                        location: "unexpected".into(),
                        kind: "video".into(),
                    })
                })
                .await
                .is_none()
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        let permit =
            tokio::time::timeout(Duration::from_secs(1), admission.clone().acquire_owned())
                .await
                .unwrap()
                .unwrap();
        drop(permit);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), done_rx)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(!target.exists());
        let state: String =
            sqlx::query_scalar("SELECT cache_state FROM cloud_media WHERE media_id='item'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "none");
        let fresh = super::resolve_with_deadline(&stop, Duration::from_secs(1), |_| async {
            admitted_blocking(admission, || ()).await?;
            Ok(ResolvedMedia {
                location: "fresh".into(),
                kind: "video".into(),
            })
        })
        .await
        .unwrap();
        assert_eq!(fresh.location, "fresh");
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_encoder_retains_closed_segments_before_replacing_remainder() {
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("spool-failed-{}", uuid::Uuid::new_v4()));
        let mut spool = PrivateSpool::new(root.clone()).unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "ffmpeg -v error -f lavfi -i testsrc2=s=64x64:r=5 -c:v libx264 -threads 1 -tune zerolatency -g 5 -sc_threshold 0 \"$@\"; exit 7", "sh"]);
        spool.start_bounded(cmd, "1", 2000).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut count = 0;
        loop {
            assert!(std::time::Instant::now() < deadline);
            match spool.take_ready_segment().unwrap() {
                TakeReady::Pending => std::thread::sleep(Duration::from_millis(10)),
                TakeReady::Segment(segment) => {
                    assert!(std::fs::metadata(&segment.path).unwrap().len() > 0);
                    count += 1;
                }
                TakeReady::Failed => break,
                TakeReady::End => panic!("failed encoder reported success"),
            }
        }
        assert_eq!(count, 2, "closed segments must precede failure");
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stalled_encoder_is_failed() {
        let root = std::env::temp_dir().join(format!("spool-stall-{}", uuid::Uuid::new_v4()));
        let mut spool = PrivateSpool::new(root.clone()).unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exec sleep 40"]);
        spool.start_bounded(cmd, "2", 2000).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(31));
        assert!(matches!(
            spool.take_ready_segment().unwrap(),
            TakeReady::Failed
        ));
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn restart_reconstructs_the_advertised_window() {
        let root = std::env::temp_dir().join(format!("publisher-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        std::fs::write(&source, fixture_ts()).unwrap();
        let mut original = Publisher::new(root.clone(), 1_700_000_000_000, 4);
        for i in 0..6 {
            original
                .publish(
                    &source,
                    180_000,
                    Some(1_700_000_000_000 + i * 2000),
                    1_700_000_100_000,
                    0,
                    super::SegmentSource::Unknown,
                )
                .unwrap();
        }
        let before = std::fs::read_to_string(root.join("index.m3u8")).unwrap();
        let mut resumed = Publisher::resume(root.clone(), 1_800_000_000_000, 4).unwrap();
        assert_eq!(resumed.next_sequence, original.next_sequence);
        assert_eq!(
            resumed.discontinuity_sequence,
            original.discontinuity_sequence
        );
        assert_eq!(
            resumed.anchor_ms + resumed.cursor / 90,
            original.anchor_ms + original.cursor / 90
        );
        assert_eq!(
            std::fs::read_to_string(root.join("index.m3u8")).unwrap(),
            before
        );
        resumed
            .publish(
                &source,
                180_000,
                Some(1_700_000_012_000),
                1_700_000_100_000,
                0,
                super::SegmentSource::Unknown,
            )
            .unwrap();
        assert_eq!(resumed.next_sequence, 7);
        assert_eq!(resumed.entries.last().unwrap().sequence, 6);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_burst_limits_ahead_preparation() {
        assert!(super::preparation_allowed(10_000, 0, 0));
        assert!(!super::preparation_allowed(10_001, 0, 0));
        assert!(super::preparation_allowed(18_000, 0, 8));
        assert!(!super::preparation_allowed(18_001, 0, 8));
        assert!(super::preparation_allowed(22_000, 0, 12));
        assert!(!super::preparation_allowed(22_001, 0, 12));
        assert!(super::preparation_allowed(40_000, 0, i64::MAX));
        assert!(!super::preparation_allowed(40_001, 0, i64::MAX));
    }
    use super::*;

    #[test]
    fn remote_image_and_watermark_use_private_command() {
        let root = std::env::temp_dir().join(format!("private-command-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let channel = crate::db::Channel {
            id: "test".into(),
            number: "99".into(),
            name: "test".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics: Default::default(),
            stream: Default::default(),
        };
        let command = super::super::build_producer_command(
            &channel,
            "item",
            "media",
            Path::new("http://127.0.0.1:1234/media?token=test"),
            0,
            0,
            &root,
            &root,
            false,
            "image",
            Some(Path::new("/tmp/logo.png")),
            0,
            true,
            true,
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .any(|v| v == "http://127.0.0.1:1234/media?token=test")
        );
        assert!(args.iter().any(|v| v.contains("logo.png")));
        assert!(!args.iter().any(|v| v.contains("delete_segments")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_music_fifo_publishes_cached_audio() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("music-fifo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("public")).unwrap();
        let origin = root.join("origin.wav");
        let cached = root.join("cached.wav");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000:duration=5"
                ])
                .arg(&origin)
                .status()
                .unwrap()
                .success()
        );
        std::fs::copy(&origin, &cached).unwrap();
        std::fs::remove_file(&origin).unwrap();
        let mut channel = crate::db::Channel {
            id: "music".into(),
            number: "11".into(),
            name: "music".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics: Default::default(),
            stream: Default::default(),
        };
        channel.stream.segment_seconds = 2;
        let command = super::super::build_producer_command(
            &channel,
            "music",
            "music",
            &cached,
            0,
            0,
            &root.join("public"),
            &root,
            false,
            "music",
            None,
            0,
            true,
            true,
        )
        .unwrap();
        let mut spool = PrivateSpool::new(root.join("private")).unwrap();
        let mut publisher = Publisher::new(root.join("public"), 1_700_000_000_000, 10);
        spool.start_bounded(command, "2", 5000).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut count = 0;
        let mut tone_segments = 0;
        let mut ticks = 0;
        let programme = ScheduledProgramme {
            id: "show".into(),
            title: "Music Mix".into(),
            start_ms: 1_700_000_000_000,
            end_ms: 1_700_000_005_000,
        };
        loop {
            assert!(Instant::now() < deadline, "music spool timed out");
            match spool.take_ready_segment().unwrap() {
                TakeReady::Pending => std::thread::sleep(Duration::from_millis(10)),
                TakeReady::End => break,
                TakeReady::Failed => panic!("production music spool failed"),
                TakeReady::Segment(lease) => {
                    let decoded = Command::new("ffmpeg")
                        .args(["-v", "error", "-i"])
                        .arg(&lease.path)
                        .args(["-f", "s16le", "-ac", "1", "-"])
                        .output()
                        .unwrap();
                    assert!(
                        decoded.status.success(),
                        "decode: {}",
                        String::from_utf8_lossy(&decoded.stderr)
                    );
                    if decoded
                        .stdout
                        .chunks_exact(2)
                        .any(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() > 100)
                    {
                        tone_segments += 1;
                    }
                    ticks += lease.duration_ticks;
                    assert!(
                        publisher
                            .publish(
                                &lease.path,
                                lease.duration_ticks,
                                if count == 0 {
                                    Some(1_700_000_000_000)
                                } else {
                                    None
                                },
                                1_700_000_010_000,
                                90_000,
                                SegmentSource::ScheduledMedia {
                                    id: "music".into(),
                                    scheduled_programme: programme.clone()
                                }
                            )
                            .unwrap()
                    );
                    count += 1;
                }
            }
        }
        assert!(count > 3, "only {count} music segments");
        assert!(tone_segments >= 3, "tone in only {tone_segments} segments");
        assert!(
            (ticks - 450_000).abs() <= 4000,
            "music duration ticks {ticks}"
        );
        let identities = publisher.snapshot();
        assert_eq!(identities.len(), count);
        for identity in &identities {
            assert_eq!(
                identity.source,
                SegmentSource::ScheduledMedia {
                    id: "music".into(),
                    scheduled_programme: programme.clone()
                }
            );
            assert!(identity.end_pdt_ns > identity.start_pdt_ns);
        }
        for pair in identities.windows(2) {
            assert_eq!(pair[0].end_pdt_ns, pair[1].start_pdt_ns);
        }
        let playlist = std::fs::read_to_string(root.join("public/index.m3u8")).unwrap();
        assert_eq!(playlist.matches("#EXTINF:").count(), count);
        assert_eq!(playlist.matches("#EXT-X-PROGRAM-DATE-TIME:").count(), count);
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_command_produces_ahead_of_three_publication_deadlines() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("awesome-timing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("public")).unwrap();
        let source = root.join("source.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=320x180:r=30:d=8",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=8",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-c:a",
                    "aac",
                    "-shortest"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let mut channel = crate::db::Channel {
            id: "timing".into(),
            number: "99".into(),
            name: "timing".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics: Default::default(),
            stream: Default::default(),
        };
        channel.stream.width = 320;
        channel.stream.height = 240;
        channel.stream.preset = "ultrafast".into();
        channel.stream.segment_seconds = 2;
        let command = super::super::build_producer_command(
            &channel,
            "timing",
            "timing",
            &source,
            0,
            0,
            &root.join("public"),
            &root,
            false,
            "video",
            None,
            0,
            true,
            true,
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert!(!args.iter().any(|arg| *arg == "-readrate"));
        let start_ms = crate::db::now_ms();
        let start = Instant::now();
        let mut spool = PrivateSpool::new(root.join("private")).unwrap();
        let mut publisher = Publisher::new(root.join("public"), start_ms, 8);
        spool.start_bounded(command, "2", 8000).unwrap();
        let mut samples = Vec::new();
        while samples.len() < 3 {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timing test timeout"
            );
            match spool.take_ready_segment().unwrap() {
                TakeReady::Pending => std::thread::sleep(Duration::from_millis(10)),
                TakeReady::End | TakeReady::Failed => panic!("ended before three segments"),
                TakeReady::Segment(lease) => {
                    let ready = start.elapsed().as_millis();
                    let deadline = start_ms + (publisher.cursor + lease.duration_ticks) / 90;
                    while crate::db::now_ms() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let now = crate::db::now_ms();
                    assert!(
                        publisher
                            .publish(
                                &lease.path,
                                lease.duration_ticks,
                                if samples.is_empty() {
                                    Some(start_ms)
                                } else {
                                    None
                                },
                                now,
                                90_000,
                                super::SegmentSource::Unknown
                            )
                            .unwrap()
                    );
                    let lag = crate::db::now_ms() - deadline;
                    samples.push((ready, lag));
                    assert!(lag < 1500, "publication backlog: {samples:?}");
                }
            }
        }
        eprintln!("(ready elapsed ms, publication lag ms): {samples:?}");
        assert!(
            samples[0].0 < 1500,
            "first segment was not preproduced: {samples:?}"
        );
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_paced_segments_and_cancellation() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        let root =
            std::env::temp_dir().join(format!("awesome-incremental-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.mkv");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=64x64:r=5:d=8",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-qp",
                    "0"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let decode = |path: &Path| {
            let output = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let reference = decode(&source);
        let frame_size = 64 * 64 * 3 / 2;
        assert_eq!(reference.len(), 40 * frame_size);
        assert_eq!(
            reference
                .chunks_exact(frame_size)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            40
        );
        let start = |spool: &mut PrivateSpool| {
            let mut command = Command::new("ffmpeg");
            command
                .args(["-v", "error", "-re", "-i"])
                .arg(&source)
                .args([
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-qp",
                    "0",
                    "-tune",
                    "zerolatency",
                    "-g",
                    "5",
                    "-sc_threshold",
                    "0",
                ]);
            spool.start_bounded(command, "1", 8_000).unwrap();
        };
        let mut spool = PrivateSpool::new(root.join("spool")).unwrap();
        start(&mut spool);
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut actual = Vec::new();
        let mut count = 0;
        loop {
            assert!(Instant::now() < deadline, "incremental stream timed out");
            match spool.take_ready_segment().unwrap() {
                TakeReady::Pending => std::thread::sleep(Duration::from_millis(20)),
                TakeReady::Segment(lease) => {
                    if count == 0 {
                        assert!(
                            !spool.items[0].fifo.as_ref().unwrap().worker_finished(),
                            "encoder already exited"
                        );
                    }
                    actual.extend(decode(&lease.path));
                    count += 1;
                }
                TakeReady::End => break,
                TakeReady::Failed => panic!("encoder failed"),
            }
        }
        assert_eq!(count, 8);
        assert_eq!(actual, reference, "frame identities or head/tail changed");
        start(&mut spool);
        let deadline = Instant::now() + Duration::from_secs(10);
        let lease = loop {
            assert!(Instant::now() < deadline, "cancellation setup timed out");
            match spool.take_ready_segment().unwrap() {
                TakeReady::Pending => std::thread::sleep(Duration::from_millis(20)),
                TakeReady::Segment(lease) => break lease,
                TakeReady::End | TakeReady::Failed => panic!("ended before cancellation"),
            }
        };
        assert!(!spool.items[0].fifo.as_ref().unwrap().worker_finished());
        let path = lease.path.clone();
        drop(spool);
        assert!(!decode(&path).is_empty());
        drop(lease);
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_reports_encoder_failure_and_unfinished_playlist() {
        use std::time::{Duration, Instant};
        let root =
            std::env::temp_dir().join(format!("awesome-poll-errors-{}", uuid::Uuid::new_v4()));
        let mut spool = PrivateSpool::new(root.clone()).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exit 7"]);
        spool.start_bounded(command, "1", 1000).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline);
            match spool.take_ready_segment() {
                Ok(TakeReady::Pending) => std::thread::sleep(Duration::from_millis(10)),
                Ok(TakeReady::Failed) => break,
                _ => panic!("failed encoder reported success"),
            }
        }
        spool.items.clear();
        let mut command = std::process::Command::new("sh");
        // An encoder exiting with a truncated CSV record must fail, even if its exit status is zero.
        command.args(["-c", "printf 'slot_0.ts,0,1'", "sh"]);
        spool.start_bounded(command, "1", 1000).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline);
            match spool.take_ready_segment() {
                Ok(TakeReady::Pending) => std::thread::sleep(Duration::from_millis(10)),
                Ok(TakeReady::Failed) => break,
                _ => panic!("truncated FIFO record reported success"),
            }
        }
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transferred_segment_survives_generation_cleanup_until_lease_drop() {
        let root = std::env::temp_dir().join(format!("awesome-lease-{}", uuid::Uuid::new_v4()));
        let dir = root.join("generation");
        std::fs::create_dir_all(&dir).unwrap();
        let segment = Segment {
            name: "seg_000000000.ts".into(),
            duration_ticks: 90_000,
        };
        std::fs::write(dir.join(&segment.name), b"closed segment").unwrap();
        let generation = CompletedGeneration {
            dir: dir.clone(),
            entries: vec![],
        };
        let lease = SegmentLease::take(&root, &dir, &segment).unwrap();
        assert_eq!(lease.duration_ticks, 90_000);
        drop(generation);
        assert_eq!(std::fs::read(&lease.path).unwrap(), b"closed segment");
        let path = lease.path.clone();
        drop(lease);
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_generation_is_removed_on_drop() {
        let root =
            std::env::temp_dir().join(format!("awesome-generation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.join("generation");
        std::fs::create_dir(&dir).unwrap();
        {
            let generation = CompletedGeneration {
                dir: dir.clone(),
                entries: Vec::new(),
            };
            assert!(generation.dir.exists());
        }
        assert!(!dir.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_unbounded_or_excessive_encoding_duration() {
        let root = std::env::temp_dir().join(format!("awesome-horizon-{}", uuid::Uuid::new_v4()));
        let mut spool = PrivateSpool::new(root.clone()).unwrap();
        assert!(
            spool
                .start_bounded(std::process::Command::new("ffmpeg"), "1", 0)
                .is_err()
        );
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exec sleep 60"]);
        spool.start_bounded(cmd, "1", 3_600_000).unwrap();
        assert_eq!(spool.items.len(), 1);
        drop(spool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_terminates_descendant_group() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("awesome-group-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pidfile = root.join("descendant.pid");
        let mut spool = PrivateSpool::new(root.join("private")).unwrap();
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 60 & echo $! > \"$1\"; wait")
            .arg("sh")
            .arg(&pidfile);
        spool.start_bounded(command, "1", 10_000).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pidfile.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(spool);
        let stat = format!("/proc/{pid}/stat");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let state = std::fs::read_to_string(&stat).unwrap_or_default();
            if state.is_empty()
                || state
                    .rsplit_once(") ")
                    .is_some_and(|(_, s)| s.starts_with('Z'))
            {
                std::fs::remove_dir_all(root).unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("descendant {pid} still running after cancellation");
    }

    #[test]
    fn local_image_is_supported_as_next_private_generation() {
        let root = std::env::temp_dir().join(format!("awesome-image-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("public")).unwrap();
        let image = root.join("image.ppm");
        let mut ppm = b"P6\n32 32\n255\n".to_vec();
        ppm.extend(std::iter::repeat_n(128, 32 * 32 * 3));
        std::fs::write(&image, ppm).unwrap();
        assert!(image.is_file());
        let mut channel = crate::db::Channel {
            id: "image".into(),
            number: "99".into(),
            name: "image".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics: Default::default(),
            stream: Default::default(),
        };
        channel.stream.width = 320;
        channel.stream.height = 240;
        channel.stream.preset = "ultrafast".into();
        let command = super::super::build_producer_command(
            &channel,
            "image",
            "image",
            &image,
            0,
            0,
            &root.join("public"),
            &root,
            false,
            "image",
            None,
            0,
            true,
            true,
        )
        .unwrap();
        let mut spool = PrivateSpool::new(root.join("spool")).unwrap();
        spool.start_bounded(command, "2", 2_000).unwrap();
        assert!(!spool.finish_current().unwrap().entries.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_ten_second_chunks_preserve_audio_markers() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("awesome-audio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("public")).unwrap();
        let mut channel = crate::db::Channel {
            id: "audio".into(),
            number: "99".into(),
            name: "audio".into(),
            start_ms: 0,
            schedule_mode: "blocks".into(),
            graphics: Default::default(),
            stream: Default::default(),
        };
        channel.stream.width = 320;
        channel.stream.height = 240;
        channel.stream.preset = "ultrafast".into();
        channel.stream.segment_seconds = 2;
        let mut spool = PrivateSpool::new(root.join("spool")).unwrap();
        let mut publisher = Publisher::new(root.join("public"), 0, 60);
        for (index, frequency) in [440, 550, 660, 770].iter().enumerate() {
            let source = root.join(format!("source-{index}.mp4"));
            assert!(
                Command::new("ffmpeg")
                    .args([
                        "-v",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc2=s=320x240:r=30:d=10",
                        "-f",
                        "lavfi",
                        "-i"
                    ])
                    .arg(format!(
                        "sine=frequency={frequency}:sample_rate=48000:duration=10"
                    ))
                    .args([
                        "-c:v",
                        "libx264",
                        "-preset",
                        "ultrafast",
                        "-threads",
                        "1",
                        "-c:a",
                        "aac",
                        "-shortest"
                    ])
                    .arg(&source)
                    .status()
                    .unwrap()
                    .success()
            );
            let command = super::super::build_producer_command(
                &channel,
                "audio",
                "audio",
                &source,
                0,
                0,
                &root.join("public"),
                &root,
                false,
                "video",
                None,
                0,
                true,
                true,
            )
            .unwrap();
            spool.start_bounded(command, "2", 10_000).unwrap();
            let generation = spool.finish_current().unwrap();
            let mut samples = Vec::new();
            for (segment_index, entry) in generation.entries.iter().enumerate() {
                let path = generation.dir.join(&entry.name);
                let decoded = Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&path)
                    .args([
                        "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "s16le", "-",
                    ])
                    .output()
                    .unwrap();
                assert!(
                    decoded.status.success(),
                    "audio decode: {}",
                    String::from_utf8_lossy(&decoded.stderr)
                );
                samples.extend(
                    decoded
                        .stdout
                        .chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]])),
                );
                assert!(
                    publisher
                        .publish(
                            &path,
                            entry.duration_ticks,
                            if segment_index == 0 {
                                Some(index as i64 * 10_000)
                            } else {
                                None
                            },
                            100_000,
                            90_000,
                            super::SegmentSource::Unknown
                        )
                        .unwrap()
                );
            }
            let seconds = samples.len() as f64 / 48_000.0;
            assert!(
                (9.8..10.3).contains(&seconds),
                "chunk {index} decoded duration {seconds}"
            );
            for window in [&samples[..24_000], &samples[samples.len() - 24_000..]] {
                let crossings = window.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
                assert!(
                    (crossings as i32 - frequency / 2).abs() < 20,
                    "chunk {index} marker {crossings}"
                );
            }
            assert!(spool.items.len() <= 2);
        }
        assert!(
            (publisher.cursor - 40 * TICKS).abs() < TICKS,
            "published duration {}",
            publisher.cursor
        );
        let playlist = std::fs::read_to_string(root.join("public/index.m3u8")).unwrap();
        assert_eq!(playlist.matches("#EXT-X-DISCONTINUITY\n").count(), 3);
        assert_eq!(
            playlist.matches("#EXT-X-PROGRAM-DATE-TIME:").count(),
            publisher.entries.len()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn decoded_frames_survive_three_item_boundaries() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("awesome-frames-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.mkv");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=32x32:r=5:d=4",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-qp",
                    "0"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let decode = |path: &Path| {
            let output = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let reference = decode(&source);
        let frame_size = 32 * 32 * 3 / 2;
        assert_eq!(reference.len(), 20 * frame_size);
        assert_eq!(
            reference
                .chunks_exact(frame_size)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            20
        );
        let mut spool = PrivateSpool::new(root.join("spool")).unwrap();
        let mut actual = Vec::new();
        for second in 0..4 {
            let mut command = Command::new("ffmpeg");
            command
                .args(["-v", "error", "-ss"])
                .arg(second.to_string())
                .arg("-i")
                .arg(&source)
                .args([
                    "-t",
                    "1",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-qp",
                    "0",
                    "-g",
                    "5",
                    "-sc_threshold",
                    "0",
                ]);
            spool.start_bounded(command, "1", 1_000).unwrap();
            let generation = spool.finish_current().unwrap();
            for entry in &generation.entries {
                actual.extend(decode(&generation.dir.join(&entry.name)));
            }
        }
        assert_eq!(actual.len(), reference.len());
        for boundary in [5, 10, 15] {
            for frame in [boundary - 1, boundary] {
                let range = frame * frame_size..(frame + 1) * frame_size;
                assert_eq!(&actual[range.clone()], &reference[range], "frame {frame}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_window_tracks_sequence_and_enforces_limits() {
        let snapshot = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:4\n#EXTINF:1,\nseg_000000004.ts\n#EXTINF:1,\nseg_000000005.ts\n";
        let (entries, offset) = private_window(snapshot, 5, 200, 2).unwrap();
        assert_eq!(entries[offset].name, "seg_000000005.ts");
        assert!(private_window(snapshot, 3, 200, 2).is_err());
        assert!(private_window(snapshot, 7, 200, 2).is_err());
        assert!(private_window(snapshot, 4, 200, 1).is_err());
        assert!(private_window(snapshot, 4, 10, 2).is_err());
        assert!(private_window(&snapshot.replace("000000005", "000000006"), 4, 200, 2).is_err());
    }
    #[test]
    fn private_playlist_rejects_incomplete_entries_and_parses_ticks() {
        let entries =
            parse_private_playlist("#EXTM3U\n#EXTINF:1.234567,\nseg_000000000.ts\n").unwrap();
        assert_eq!(entries[0].duration_ticks, 111_111);
        assert!(parse_private_playlist("#EXTM3U\n#EXTINF:1.0,\n../bad.ts\n").is_err());
        assert!(parse_private_playlist("#EXTM3U\n#EXTINF:1.0,\n").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn generation_boundaries_decode_cleanly_as_published_hls() {
        use std::io::Write;
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("awesome-boundary-{}", uuid::Uuid::new_v4()));
        let public = root.join("public");
        std::fs::create_dir_all(&public).unwrap();
        let mut publisher = Publisher::new(public.clone(), 1_700_000_000_000, 60);
        let mut spool = PrivateSpool::new(root.join("spool")).unwrap();
        let mut expected_hashes = [Vec::new(), Vec::new()];
        for (index, (color, frequency)) in [("red", 440), ("blue", 660)].iter().enumerate() {
            let mut command = Command::new("ffmpeg");
            command
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("color=c={color}:s=64x64:r=25:d=12"))
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "sine=frequency={frequency}:sample_rate=48000:duration=12"
                ))
                .args([
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-g",
                    "25",
                    "-sc_threshold",
                    "0",
                    "-c:a",
                    "aac",
                ]);
            spool.start_bounded(command, "1", 12_000).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut held = std::collections::VecDeque::new();
            while held.len() < 3 {
                match spool.take_ready_segment().unwrap() {
                    super::TakeReady::Segment(segment) => held.push_back(segment),
                    super::TakeReady::Pending => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    _ => panic!("encoder ended before backpressure"),
                }
                assert!(std::time::Instant::now() < deadline);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(matches!(
                spool.take_ready_segment().unwrap(),
                super::TakeReady::Pending
            ));
            let epoch = root.join(format!("epoch-{index}.ts"));
            let mut output = std::fs::File::create(&epoch).unwrap();
            let mut segment_count = 0;
            let mut counter_check = super::TransportCounters::default();
            loop {
                let segment = if let Some(segment) = held.pop_front() {
                    segment
                } else {
                    match spool.take_ready_segment().unwrap() {
                        super::TakeReady::Segment(segment) => segment,
                        super::TakeReady::Pending => {
                            assert!(std::time::Instant::now() < deadline);
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            continue;
                        }
                        super::TakeReady::End => break,
                        super::TakeReady::Failed => panic!("encoder failed"),
                    }
                };
                let boundary =
                    (segment_count == 0).then_some(publisher.anchor_ms + publisher.cursor / 90);
                let original = std::fs::read(&segment.path).unwrap();
                assert!(
                    publisher
                        .publish(
                            &segment.path,
                            segment.duration_ticks,
                            boundary,
                            publisher.anchor_ms + 100_000,
                            0,
                            super::SegmentSource::Media(color.to_string())
                        )
                        .unwrap()
                );
                let published =
                    std::fs::read(public.join(&publisher.entries.last().unwrap().name)).unwrap();
                assert_eq!(original.len(), published.len());
                for (before, after) in original.chunks_exact(188).zip(published.chunks_exact(188)) {
                    assert_eq!(&before[..3], &after[..3]);
                    assert_eq!(before[3] & 0xf0, after[3] & 0xf0);
                    assert_eq!(&before[4..], &after[4..]);
                    let mut checked = after.to_vec();
                    counter_check.normalize(&mut checked).unwrap();
                    assert_eq!(checked, after, "counter discontinuity in epoch {index}");
                }
                output.write_all(&published).unwrap();
                segment_count += 1;
            }
            assert!(segment_count > 3);
            drop(output);
            for (i, stream) in ["v:0", "a:0"].iter().enumerate() {
                let probe = Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        stream,
                        "-show_packets",
                        "-show_data_hash",
                        "sha256",
                        "-show_entries",
                        "packet=data_hash",
                        "-of",
                        "csv=p=0",
                    ])
                    .arg(&epoch)
                    .output()
                    .unwrap();
                assert!(probe.status.success());
                expected_hashes[i].extend(
                    String::from_utf8_lossy(&probe.stdout)
                        .lines()
                        .filter(|line| line.starts_with("SHA256:"))
                        .map(|line| line.split(',').next().unwrap().to_owned()),
                );
            }
            let video = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&epoch)
                .args(["-map", "0:v:0", "-pix_fmt", "rgb24", "-f", "rawvideo", "-"])
                .output()
                .unwrap();
            assert!(
                video.status.success(),
                "{}",
                String::from_utf8_lossy(&video.stderr)
            );
            let frames: Vec<_> = video.stdout.chunks_exact(64 * 64 * 3).collect();
            assert_eq!(frames.len(), 300, "{color} frames");
            for frame in &frames {
                let px = &frame[..3];
                assert!(
                    if *color == "red" {
                        px[0] > 200 && px[2] < 30
                    } else {
                        px[2] > 200 && px[0] < 30
                    },
                    "wrong marker {px:?}"
                );
            }
            let audio = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&epoch)
                .args([
                    "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "s16le", "-",
                ])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples: Vec<i16> = audio
                .stdout
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(
                (576_000..=578_048).contains(&samples.len()),
                "{color} samples {}",
                samples.len()
            );
            for window in [
                &samples[4096..28096],
                &samples[samples.len() - 28096..samples.len() - 4096],
            ] {
                let crossings = window.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
                assert!(
                    (crossings as i32 - frequency / 2).abs() < 10,
                    "{color} audio marker {crossings}"
                );
            }
            for stream in ["v:0", "a:0"] {
                let probe = Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        stream,
                        "-show_entries",
                        "packet=dts_time,duration_time",
                        "-of",
                        "csv=p=0",
                    ])
                    .arg(&epoch)
                    .output()
                    .unwrap();
                assert!(probe.status.success());
                let mut previous: Option<(f64, f64)> = None;
                for line in String::from_utf8_lossy(&probe.stdout).lines() {
                    let fields: Vec<_> = line.split(',').collect();
                    if let (Some(Ok(dts)), Some(Ok(duration))) = (
                        fields.first().map(|v| v.parse::<f64>()),
                        fields.get(1).map(|v| v.parse::<f64>()),
                    ) {
                        if let Some((last, length)) = previous {
                            assert!(
                                (dts - last - length).abs() <= 0.00003,
                                "{color} {stream} packet gap or overlap {last}+{length} -> {dts}"
                            );
                        }
                        previous = Some((dts, duration));
                    }
                }
                assert!(previous.is_some());
            }
            eprintln!(
                "{color}: {} segments, {} frames, {} samples",
                segment_count,
                frames.len(),
                samples.len()
            );
        }
        let playlist = std::fs::read_to_string(public.join("index.m3u8")).unwrap();
        assert_eq!(playlist.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        assert_eq!(
            playlist.matches("#EXT-X-PROGRAM-DATE-TIME:").count(),
            publisher.entries.len()
        );
        for (entry, identity) in publisher.entries.iter().zip(&publisher.identities) {
            assert_eq!(
                identity.start_pdt_ns,
                (publisher.anchor_ms as i128 * 90 + entry.date_ticks as i128) * 1_000_000 / 90
            );
            assert_eq!(
                identity.source,
                super::SegmentSource::Media(
                    if entry.date_ticks < 12 * TICKS {
                        "red"
                    } else {
                        "blue"
                    }
                    .into()
                )
            );
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(public.join("index.m3u8"))
            .unwrap()
            .write_all(b"#EXT-X-ENDLIST\n")
            .unwrap();
        let decoded = Command::new("ffmpeg")
            .args(["-v", "warning", "-i"])
            .arg(public.join("index.m3u8"))
            .args([
                "-map",
                "0:v:0",
                "-fps_mode",
                "passthrough",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .unwrap();
        eprintln!(
            "whole HLS video diagnostics: {}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert!(decoded.status.success());
        let frames: Vec<_> = decoded.stdout.chunks_exact(64 * 64 * 3).collect();
        assert_eq!(frames.len(), 600, "whole HLS video frames");
        for (i, frame) in frames.iter().enumerate() {
            let p = &frame[..3];
            assert!(
                if i < 300 {
                    p[0] > 200 && p[2] < 30
                } else {
                    p[2] > 200 && p[0] < 30
                },
                "frame {i}: {p:?}"
            );
        }
        let decoded = Command::new("ffmpeg")
            .args(["-v", "warning", "-i"])
            .arg(public.join("index.m3u8"))
            .args([
                "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "s16le", "-",
            ])
            .output()
            .unwrap();
        eprintln!(
            "whole HLS audio diagnostics: {}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert!(decoded.status.success());
        let samples: Vec<i16> = decoded
            .stdout
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        eprintln!("whole HLS samples: {}", samples.len());
        assert_eq!(samples.len(), 1_155_072);
        for (start, frequency) in [(4_096, 440), (577_536 + 4_096, 660)] {
            let window = &samples[start..start + 24_000];
            let crossings = window.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
            assert!(
                (crossings as i32 - frequency / 2).abs() < 10,
                "whole HLS audio marker {crossings}"
            );
        }
        // Per-epoch DTS checks above remain strict. Across the declared discontinuity
        // AAC encoder priming/padding leaves a known 32 ms mapped coverage overlap.
        for (i, stream) in ["v:0", "a:0"].iter().enumerate() {
            let probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    stream,
                    "-show_packets",
                    "-show_data_hash",
                    "sha256",
                    "-show_entries",
                    "packet=data_hash",
                    "-of",
                    "csv=p=0",
                ])
                .arg(public.join("index.m3u8"))
                .output()
                .unwrap();
            assert!(probe.status.success());
            let actual: Vec<_> = String::from_utf8_lossy(&probe.stdout)
                .lines()
                .filter(|line| line.starts_with("SHA256:"))
                .map(|line| line.split(',').next().unwrap().to_owned())
                .collect();
            assert_eq!(actual.len(), if i == 0 { 600 } else { 1128 });
            assert!(
                actual == expected_hashes[i],
                "packet loss or corruption in HLS {stream}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_ffmpeg_segments_publish_in_item_order() {
        use std::process::Command;
        let root =
            std::env::temp_dir().join(format!("awesome-private-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let public = root.join("public");
        std::fs::create_dir(&public).unwrap();
        let mut publisher = Publisher::new(public.clone(), 1_700_000_000_000, 4);
        let mut count = 0;
        let mut queue = PrivateSpool::new(root.join("spool")).unwrap();
        for color in ["red", "blue", "green", "yellow"].iter() {
            let mut command = Command::new("ffmpeg");
            command
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("color=c={color}:s=64x64:r=10:d=1.2"))
                .args([
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-g",
                    "5",
                    "-sc_threshold",
                    "0",
                ]);
            queue.start_bounded(command, "0.5", 1_200).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut durations = Vec::new();
            loop {
                match queue.take_ready_segment().unwrap() {
                    super::TakeReady::Segment(segment) => {
                        let boundary = if durations.is_empty() {
                            Some(publisher.anchor_ms + publisher.cursor / 90)
                        } else {
                            None
                        };
                        durations.push(segment.duration_ticks);
                        assert!(
                            publisher
                                .publish(
                                    &segment.path,
                                    segment.duration_ticks,
                                    boundary,
                                    publisher.anchor_ms + 100_000,
                                    0,
                                    super::SegmentSource::Unknown
                                )
                                .unwrap()
                        );
                        count += 1;
                    }
                    super::TakeReady::Pending => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    super::TakeReady::End => break,
                    super::TakeReady::Failed => panic!("FIFO encoder failed"),
                }
                assert!(std::time::Instant::now() < deadline, "FIFO drain timed out");
            }
            assert_eq!(durations.len(), 2);
            assert_eq!(durations.iter().sum::<i64>(), 108_000);
        }
        let text = std::fs::read_to_string(public.join("index.m3u8")).unwrap();
        assert_eq!(count, 8);
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:4\n"));
        assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
        assert_eq!(text.matches("#EXT-X-PROGRAM-DATE-TIME:").count(), 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_cancellation_reaps_children_and_retention_respects_deadline() {
        let root = std::env::temp_dir().join(format!("awesome-spool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let dirs;
        let pids;
        {
            let mut spool = PrivateSpool::new(root.join("private")).unwrap();
            for _ in 0..2 {
                let mut cmd = std::process::Command::new("ffmpeg");
                cmd.args([
                    "-v",
                    "error",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=s=64x64:r=10",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                ]);
                spool.start_bounded(cmd, "1", 10_000).unwrap();
            }
            assert!(
                spool
                    .start_bounded(std::process::Command::new("ffmpeg"), "1", 10_000)
                    .is_err()
            );
            dirs = spool
                .items
                .iter()
                .map(|i| i.dir.clone())
                .collect::<Vec<_>>();
            pids = spool
                .items
                .iter()
                .map(|i| i.fifo.as_ref().unwrap().pid())
                .collect::<Vec<_>>();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while dirs.iter().any(|dir| dir.exists()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for dir in dirs {
            assert!(!dir.exists());
        }
        #[cfg(target_os = "linux")]
        for pid in pids {
            assert!(!Path::new(&format!("/proc/{pid}")).exists());
        }
        let source = root.join("source");
        std::fs::write(&source, fixture_ts()).unwrap();
        let mut publisher = Publisher::new(root.clone(), 0, 4);
        for _ in 0..5 {
            publisher
                .publish(
                    &source,
                    90_000,
                    None,
                    100_000,
                    0,
                    super::SegmentSource::Unknown,
                )
                .unwrap();
        }
        let old = root.join(&publisher.retired[0].0);
        let deadline = publisher.retired[0].1;
        publisher.cleanup(deadline - 1).unwrap();
        assert!(old.exists());
        publisher.cleanup(deadline).unwrap();
        assert!(!old.exists());
        assert!(
            publisher
                .entries
                .iter()
                .all(|e| root.join(&e.name).exists())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pacing_and_failure_leave_the_last_snapshot_intact() {
        let root =
            std::env::temp_dir().join(format!("awesome-publisher-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.ts");
        std::fs::write(&source, fixture_ts()).unwrap();
        let mut p = Publisher::new(root.clone(), 10_000, 4);
        assert!(
            !p.publish(
                &source,
                90_000,
                Some(10_000),
                10_999,
                0,
                super::SegmentSource::Unknown
            )
            .unwrap()
        );
        assert!(
            p.publish(
                &source,
                90_000,
                Some(10_000),
                11_000,
                0,
                super::SegmentSource::Unknown
            )
            .unwrap()
        );
        let snapshot = std::fs::read(root.join("index.m3u8")).unwrap();
        assert!(
            p.publish(
                &source,
                90_000,
                Some(12_000),
                20_000,
                0,
                super::SegmentSource::Unknown
            )
            .is_err()
        );
        assert!(
            p.publish(
                &root.join("missing"),
                90_000,
                None,
                20_000,
                0,
                super::SegmentSource::Unknown
            )
            .is_err()
        );
        assert_eq!(snapshot, std::fs::read(root.join("index.m3u8")).unwrap());
        assert_eq!(p.cursor, 90_000);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn segment_identity_follows_committed_playlist_and_restart() {
        use super::{SegmentIdentity, SegmentSource};
        let root = std::env::temp_dir().join(format!("identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.ts");
        std::fs::write(&source, fixture_ts()).unwrap();
        let mut p = Publisher::new(root.clone(), 10_000, 4);
        for identity in [
            SegmentSource::Media("A".into()),
            SegmentSource::Slate,
            SegmentSource::Media("B".into()),
        ] {
            assert!(
                p.publish(&source, 90_000, None, 100_000, 0, identity)
                    .unwrap()
            );
        }
        assert_eq!(
            p.snapshot(),
            vec![
                SegmentIdentity {
                    sequence: 0,
                    start_pdt_ns: 10_000_000_000,
                    end_pdt_ns: 11_000_000_000,
                    start_90k: 900_000,
                    end_90k: 990_000,
                    source: SegmentSource::Media("A".into())
                },
                SegmentIdentity {
                    sequence: 1,
                    start_pdt_ns: 11_000_000_000,
                    end_pdt_ns: 12_000_000_000,
                    start_90k: 990_000,
                    end_90k: 1_080_000,
                    source: SegmentSource::Slate
                },
                SegmentIdentity {
                    sequence: 2,
                    start_pdt_ns: 12_000_000_000,
                    end_pdt_ns: 13_000_000_000,
                    start_90k: 1_080_000,
                    end_90k: 1_170_000,
                    source: SegmentSource::Media("B".into())
                },
            ]
        );
        let before = p.snapshot();
        std::fs::remove_file(root.join("index.m3u8")).unwrap();
        std::fs::create_dir(root.join("index.m3u8")).unwrap();
        assert!(
            p.publish(&source, 90_000, None, 100_000, 0, SegmentSource::Slate)
                .is_err()
        );
        assert_eq!(p.snapshot(), before);
        std::fs::remove_dir(root.join("index.m3u8")).unwrap();
        p.write_playlist(&p.entries, p.discontinuity_sequence)
            .unwrap();
        let mut p = Publisher::resume(root.clone(), 10_000, 4).unwrap();
        assert_eq!(p.snapshot(), before);
        for _ in 0..8 {
            p.publish(&source, 90_000, None, 100_000, 0, SegmentSource::Slate)
                .unwrap();
        }
        assert_eq!(p.snapshot().len(), 4);
        assert!(
            p.snapshot()
                .iter()
                .all(|entry| entry.source == SegmentSource::Slate)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn fallback_publication_restart_playout_roundtrip() {
        use super::{ScheduledProgramme, SegmentSource};
        use crate::engine::{Engine, PrivateRuntime, SnapshotHandle};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicI64},
        };
        let root =
            std::env::temp_dir().join(format!("fallback-roundtrip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.ts");
        std::fs::write(&source, fixture_ts()).unwrap();
        let programme = ScheduledProgramme {
            id: "media-1".into(),
            title: "Example".into(),
            start_ms: 10_000,
            end_ms: 20_000,
        };
        let mut publisher = Publisher::new(root.clone(), 10_000, 4);
        assert!(
            publisher
                .publish(
                    &source,
                    90_000,
                    None,
                    100_000,
                    0,
                    SegmentSource::fallback("early_eof", Some(programme))
                )
                .unwrap()
        );
        let snapshot: SnapshotHandle = Default::default();
        snapshot.lock().unwrap().segments = Publisher::resume(root.clone(), 10_000, 4)
            .unwrap()
            .snapshot();
        let pool = crate::db::init(&root).await.unwrap();
        let cloud = Arc::new(
            crate::cloud::CloudManager::new(pool.clone(), root.clone(), "127.0.0.1", 0).unwrap(),
        );
        let engine = Engine::new(pool, root.clone(), cloud, 0);
        engine.private_runtimes.lock().await.insert(
            "channel".into(),
            PrivateRuntime {
                generation: "test".into(),
                snapshot,
                ready: Arc::new(AtomicBool::new(true)),
                failed_at_ms: Arc::new(AtomicI64::new(0)),
                stop: Arc::new(AtomicBool::new(false)),
                task: tokio::spawn(std::future::pending()),
                last_access_ms: 0,
                channel_number: "1".into(),
                channel_name: "test".into(),
                playlist: root.join("index.m3u8"),
            },
        );
        let value = serde_json::to_value(engine.playout_status("channel").await).unwrap();
        let segment = &value["segments"][0];
        assert_eq!(segment["source"], "slate");
        assert!(segment["media_id"].is_null());
        assert_eq!(segment["fallback_reason"], "early_eof");
        assert_eq!(
            segment["scheduled_programme"],
            serde_json::json!({"id":"media-1", "title":"Example", "start_ms":10000, "end_ms":20000})
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
