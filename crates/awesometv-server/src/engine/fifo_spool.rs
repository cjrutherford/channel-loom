//! Private, bounded FIFO segment spool. Unix only.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt},
        io::{AsRawFd, FromRawFd},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const MAX_SEGMENT: u64 = 32 * 1024 * 1024;
const CAPACITY: usize = 3;
const RECORD_LIMIT: usize = 512;

struct Shared {
    dir: PathBuf,
    cancelled: AtomicBool,
    permits: Mutex<usize>,
    wake: Condvar,
}
impl Drop for Shared {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
struct Permit(Arc<Shared>);
impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.permits.lock().unwrap() -= 1;
        self.0.wake.notify_all();
    }
}

/// A segment remains admitted until this lease is dropped; dropping deletes its file.
pub struct Segment {
    pub path: PathBuf,
    pub sequence: u64,
    pub start: f64,
    pub end: f64,
    _permit: Permit,
}
impl Drop for Segment {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            if e.kind() != io::ErrorKind::NotFound {
                self._permit.0.cancelled.store(true, Ordering::Release);
                self._permit.0.wake.notify_all();
            }
        }
    }
}

/// The receiver and cancellation handle. Dropping it requests cancellation without waiting.
pub struct FifoSpool {
    shared: Arc<Shared>,
    rx: mpsc::Receiver<io::Result<Segment>>,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    pid: u32,
}
impl FifoSpool {
    /// Creates a new private directory and starts FFmpeg. `input_args` precede the segment output options.
    pub fn spawn(dir: impl AsRef<Path>, input_args: &[String]) -> io::Result<Self> {
        Self::spawn_with_limit(dir, input_args, MAX_SEGMENT)
    }
    fn spawn_with_limit(
        dir: impl AsRef<Path>,
        input_args: &[String],
        limit: u64,
    ) -> io::Result<Self> {
        let mut command = Command::new("ffmpeg");
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]).args(input_args);
        Self::spawn_command_with_setup(dir, command, limit, set_nonblocking)
    }
    pub fn spawn_command(dir: impl AsRef<Path>, command: Command) -> io::Result<Self> {
        Self::spawn_command_with_setup(dir, command, MAX_SEGMENT, set_nonblocking)
    }
    fn spawn_command_with_setup(
        dir: impl AsRef<Path>, mut command: Command, limit: u64,
        setup: impl FnOnce(i32) -> io::Result<()>,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let shared = Arc::new(Shared {
            dir: dir.clone(),
            cancelled: AtomicBool::new(false),
            permits: Mutex::new(0),
            wake: Condvar::new(),
        });
        for i in 0..2 {
            let p = dir.join(format!("slot_{i}.ts"));
            let c = std::ffi::CString::new(p.as_os_str().as_bytes())
                .map_err(|_| io::Error::other("NUL in FIFO path"))?;
            if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // Prepare the read end before spawning: any setup error leaves no child behind.
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 { return Err(io::Error::last_os_error()); }
        let stdout = unsafe { File::from_raw_fd(fds[0]) };
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        for fd in fds {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        setup(stdout.as_raw_fd())?;
        let mut child = command.process_group(0).args([
                "-f", "segment", "-segment_format", "mpegts", "-segment_time", "1",
                "-segment_wrap", "2", "-reset_timestamps", "0", "-segment_list_type", "csv",
                "-segment_list_size", "0", "-segment_list", "pipe:1",
            ])
            .arg(dir.join("slot_%01d.ts"))
            .stdout(Stdio::from(writer))
            .stderr(Stdio::null())
            .spawn()?;
        #[cfg(test)]
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        let worker = thread::spawn(move || {
            let result = run(&s, &mut child, stdout, limit, &tx);
            if result.is_err() || s.cancelled.load(Ordering::Acquire) {
                // The encoder may have spawned helpers that still hold the FIFO or
                // stdout open. Retire the entire private process group.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
            }
            // Reaping is confined to this owned worker; callers never synchronously wait on Child.
            let status = child.wait();
            if let Err(e) = result {
                let _ = tx.send(Err(e));
            } else if !s.cancelled.load(Ordering::Acquire) {
                match status {
                    Ok(x) if x.success() => (),
                    Ok(x) => {
                        let _ = tx.send(Err(io::Error::other(format!("ffmpeg exited {x}"))));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
        });
        Ok(Self {
            shared,
            rx,
            worker: Some(worker),
            #[cfg(test)] pid,
        })
    }
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<io::Result<Segment>, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
    pub fn try_recv(&self) -> Result<io::Result<Segment>, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
    #[cfg(test)]
    pub fn pid(&self) -> u32 { self.pid }
    #[cfg(test)]
    pub fn worker_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(|worker| worker.is_finished())
    }
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
    /// Transfers the worker handle to a reaper, which can join it away from latency-sensitive callers.
    pub fn retire(mut self) -> JoinHandle<()> {
        self.cancel();
        self.worker.take().unwrap()
    }
}
impl Drop for FifoSpool {
    fn drop(&mut self) {
        self.cancel();
    }
}
fn set_nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn admit(s: &Arc<Shared>) -> Option<Permit> {
    let mut n = s.permits.lock().unwrap();
    while *n == CAPACITY && !s.cancelled.load(Ordering::Acquire) {
        n = s.wake.wait(n).unwrap();
    }
    if s.cancelled.load(Ordering::Acquire) {
        return None;
    }
    *n += 1;
    Some(Permit(s.clone()))
}
fn run(
    s: &Arc<Shared>,
    child: &mut Child,
    mut stdout: File,
    limit: u64,
    tx: &mpsc::Sender<io::Result<Segment>>,
) -> io::Result<()> {
    let mut seq = 0u64;
    let mut csv = Vec::new();
    loop {
        let Some(permit) = admit(s) else {
            return Ok(());
        };
        let slot = format!("slot_{}.ts", seq % 2);
        let mut fifo = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(s.dir.join(&slot))?;
        let part = s.dir.join(format!("{seq}.part"));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&part)?;
        let mut size = 0u64;
        let mut done = false;
        let mut line = None;
        let mut stdout_eof = false;
        let mut last_progress = std::time::Instant::now();
        loop {
            if s.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut progress = false;
            let mut buf = [0u8; 16384];
            if !done {
                loop {
                    match fifo.read(&mut buf) {
                        Ok(0) => {
                            if size > 0 {
                                done = true;
                            }
                            break;
                        }
                        Ok(n) => {
                            progress = true;
                            size += n as u64;
                            if size > limit {
                                return Err(io::Error::other("segment exceeds spool limit"));
                            }
                            output.write_all(&buf[..n])?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
            }
            loop {
                let mut b = [0u8; 512];
                match stdout.read(&mut b) {
                    Ok(0) => {
                        stdout_eof = true;
                        break;
                    }
                    Ok(n) => {
                        progress = true;
                        csv.extend_from_slice(&b[..n]);
                        if csv.len() > RECORD_LIMIT {
                            return Err(io::Error::other("segment CSV exceeds limit"));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            if let Some(pos) = csv.iter().position(|b| *b == b'\n') {
                line = Some(csv.drain(..=pos).collect::<Vec<_>>());
            }
            if done && line.is_some() {
                break;
            }
            if stdout_eof && size == 0 {
                if !csv.is_empty() || line.is_some() {
                    return Err(io::Error::other("unexpected segment CSV"));
                }
                let _ = fs::remove_file(&part);
                return Ok(());
            }
            if stdout_eof && line.is_none() {
                return Err(io::Error::other("missing segment CSV"));
            }
            if progress { last_progress = std::time::Instant::now(); }
            if last_progress.elapsed() >= Duration::from_secs(22) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "active FIFO encoder stalled"));
            }
            if !progress {
                let _ = child.try_wait()?;
                let mut fds = [
                    libc::pollfd {
                        fd: if done { -1 } else { fifo.as_raw_fd() },
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: stdout.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 10) } < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
        let record = String::from_utf8(line.unwrap())
            .map_err(|_| io::Error::other("invalid segment CSV"))?;
        let fields: Vec<_> = record.trim_end_matches(['\r', '\n']).split(',').collect();
        if fields.len() != 3
            || Path::new(fields[0]).file_name().and_then(|p| p.to_str()) != Some(&slot)
            || (fields[0] != slot && Path::new(fields[0]) != s.dir.join(&slot))
        {
            return Err(io::Error::other("unexpected segment path"));
        }
        let start: f64 = fields[1]
            .parse()
            .map_err(|_| io::Error::other("invalid segment start"))?;
        let end: f64 = fields[2]
            .parse()
            .map_err(|_| io::Error::other("invalid segment end"))?;
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
            return Err(io::Error::other("invalid segment duration"));
        }
        drop(output);
        let path = s.dir.join(format!("{seq}.ts"));
        fs::rename(part, &path)?;
        let (start, end) = presentation_bounds(s, &path)?;
        if tx
            .send(Ok(Segment {
                path,
                sequence: seq,
                start,
                end,
                _permit: permit,
            }))
            .is_err()
        {
            return Ok(());
        }
        seq += 1;
    }
}

// Probe the selected presentation stream, including reordered presentation packets. CSV timestamps
// are retained for validation only: their first range includes decode preroll.
fn presentation_bounds(s: &Shared, path: &Path) -> io::Result<(f64, f64)> {
    let probe_path = path.with_extension("probe");
    let output = File::create(&probe_path)?;
    let mut child = Command::new("ffprobe")
        .args(["-v", "error", "-f", "mpegts", "-show_entries",
            "stream=index,codec_type:packet=stream_index,pts_time,duration_time", "-of", "json"])
        .arg(path).stdout(Stdio::from(output)).stderr(Stdio::null()).spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let result = loop {
        if s.cancelled.load(Ordering::Acquire) || std::time::Instant::now() >= deadline
            || fs::metadata(&probe_path)?.len() > 8 * 1024 * 1024 {
            let _ = child.kill();
            // Reaping must not hold up the spool worker if the process is stuck.
            thread::spawn(move || { let _ = child.wait(); });
            break Err(io::Error::new(io::ErrorKind::TimedOut, "presentation packet probe cancelled or exceeded limit"));
        }
        match child.try_wait()? {
            Some(status) if status.success() => break Ok(()),
            Some(_) => break Err(io::Error::other("presentation packet probe failed")),
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let result = result.and_then(|()| {
        let data = fs::read(&probe_path)?;
        let json: serde_json::Value = serde_json::from_slice(&data)
            .map_err(|_| io::Error::other("invalid presentation packet probe"))?;
        let packets = json["packets"].as_array().ok_or_else(|| io::Error::other("missing presentation packets"))?;
        let streams = json["streams"].as_array().ok_or_else(|| io::Error::other("missing streams"))?;
        let stream = ["video", "audio"].iter().find_map(|kind| streams.iter().find(|s| s["codec_type"].as_str() == Some(kind)).and_then(|s| s["index"].as_u64()))
            .ok_or_else(|| io::Error::other("missing presentation stream"))?;
        let mut first = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        for packet in packets.iter().filter(|p| p["stream_index"].as_u64() == Some(stream)) {
            let parse = |key: &str| packet[key].as_str().and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite()).ok_or_else(|| io::Error::other("missing presentation packet timing"));
            let pts = parse("pts_time")?;
            let duration = parse("duration_time")?;
            if duration <= 0.0 { return Err(io::Error::other("invalid presentation packet duration")); }
            first = first.min(pts);
            end = end.max(pts + duration);
        }
        if !first.is_finite() || !end.is_finite() || end <= first {
            return Err(io::Error::other("empty presentation packet timing"));
        }
        Ok((first, end))
    });
    let _ = fs::remove_file(probe_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    fn setup_failure_does_not_spawn_command() {
        let root = std::env::temp_dir().join(format!("fifo-setup-{}", uuid::Uuid::new_v4()));
        // A missing executable would return NotFound if spawn were attempted first.
        let command = Command::new(root.join("missing-executable"));
        let result = FifoSpool::spawn_command_with_setup(&root, command, MAX_SEGMENT, |_| Err(io::Error::other("injected setup failure")));
        assert_eq!(result.err().unwrap().to_string(), "injected setup failure");
        assert!(!root.exists());
    }
    #[test]
    fn fifo_presentation_duration_across_segments() {
        for (rate, frames, bframes, audio) in [(10, 12, true, false), (25, 100, true, true), (25, 100, false, false)] {
            let dir = std::env::temp_dir().join(format!("fifo-timing-{}", uuid::Uuid::new_v4()));
            let mut command = Command::new("ffmpeg");
            command.args(["-v", "error", "-nostdin", "-f", "lavfi", "-i"])
                .arg(format!("testsrc2=size=160x90:rate={rate}"));
            if audio { command.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-c:a", "aac"]); }
            command.args(["-frames:v", &frames.to_string(), "-c:v", "libx264", "-threads", "1",
                "-preset", "veryfast", "-crf", "28", "-pix_fmt", "yuv420p", "-g", &rate.to_string(),
                "-keyint_min", &rate.to_string(), "-sc_threshold", "0", "-force_key_frames", "expr:gte(t,n_forced*1)"]);
            if !bframes { command.args(["-bf", "0"]); }
            let spool = FifoSpool::spawn_command(&dir, command).unwrap();
            let mut ticks = 0i64;
            let mut count = 0;
            loop {
                match spool.recv_timeout(Duration::from_secs(15)) {
                    Ok(Ok(segment)) => {
                        ticks += (segment.end * 90000.0).round() as i64 - (segment.start * 90000.0).round() as i64;
                        count += 1;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    other => panic!("segment failed: {}", match other { Ok(Err(e)) => e.to_string(), _ => "timeout".into() }),
                }
            }
            assert!(count >= 2, "rate {rate}: {count} segments");
            assert_eq!(ticks, frames * 90000 / rate, "rate {rate}, bframes {bframes}, audio {audio}");
        }
    }
    fn fixture(limit: u64) -> FifoSpool {
        let dir = std::env::temp_dir().join(format!(
            "awesometv-fifo-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let args = [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=25:duration=12",
            "-c:v",
            "mpeg2video",
            "-g",
            "25",
            "-bf",
            "0",
        ]
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>();
        FifoSpool::spawn_with_limit(dir, &args, limit).unwrap()
    }
    fn next(s: &FifoSpool) -> io::Result<Segment> {
        s.recv_timeout(Duration::from_secs(15))
            .expect("segment timeout")
    }
    #[test]
    fn fifo_spool_backpressure_and_decode() {
        let s = fixture(MAX_SEGMENT);
        let mut held = vec![next(&s).unwrap(), next(&s).unwrap(), next(&s).unwrap()];
        let count = || fs::read_dir(&s.shared.dir).unwrap().count();
        assert_eq!(count(), 5);
        let sizes: Vec<_> = held
            .iter()
            .map(|segment| fs::metadata(&segment.path).unwrap().len())
            .collect();
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            count(),
            5,
            "worker opened another segment while admission was full"
        );
        assert_eq!(
            sizes,
            held.iter()
                .map(|segment| fs::metadata(&segment.path).unwrap().len())
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            s.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let output = s.shared.dir.with_extension("joined.ts");
        let mut joined = File::create(&output).unwrap();
        let mut segments = 0;
        loop {
            let segment = if !held.is_empty() {
                held.remove(0)
            } else {
                match s.recv_timeout(Duration::from_secs(15)) {
                    Ok(x) => x.unwrap(),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(e) => panic!("{e}"),
                }
            };
            assert_eq!(segment.sequence, segments);
            segments += 1;
            io::copy(&mut File::open(&segment.path).unwrap(), &mut joined).unwrap();
        }
        drop(joined);
        assert!(segments >= 12, "{segments} segments");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&output)
            .args(["-f", "null", "-"])
            .status()
            .unwrap();
        assert!(status.success());
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=nw=1:nk=1",
            ])
            .arg(&output)
            .output()
            .unwrap();
        let duration: f64 = String::from_utf8(probe.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(duration >= 11.5 && duration <= 12.5, "duration {duration}");
        fs::remove_file(output).unwrap();
    }
    #[test]
    fn fifo_spool_cap_never_exposes_partial_segment() {
        let s = fixture(100);
        assert!(next(&s).is_err());
        assert!(!s.shared.dir.join("0.ts").exists());
    }
    #[test]
    fn fifo_spool_cancel_admission_is_prompt() {
        let s = fixture(MAX_SEGMENT);
        let held = [next(&s).unwrap(), next(&s).unwrap(), next(&s).unwrap()];
        let start = Instant::now();
        let worker = s.retire();
        worker.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
    }
}
