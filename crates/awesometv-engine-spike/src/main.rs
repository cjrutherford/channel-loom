//! Phase 0 HLS spike.
//!
//! Takes heterogeneous clips, normalizes them into one 720p H.264/AAC HLS
//! channel, and reports startup/total time and playlist facts. This is a
//! measurement tool, not the production engine.

use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
struct Options {
    out: PathBuf,
    width: u32,
    height: u32,
    fps: String,
    crf: u32,
    preset: String,
    vaapi: Option<String>,
    seconds: Option<u32>,
    clips: Vec<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let options = parse_args()?;
    fs::create_dir_all(&options.out)
        .with_context(|| format!("create output dir {}", options.out.display()))?;

    for clip in &options.clips {
        if !clip.is_file() {
            bail!("missing input clip: {}", clip.display());
        }
    }

    let filter = build_filter(&options);
    let playlist = options.out.join("index.m3u8");
    let segment_pattern = options.out.join("seg_%05d.ts");
    let log_path = options.out.join("ffmpeg.log");
    let log = File::create(&log_path)?;

    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y");
    for clip in &options.clips {
        if let Some(seconds) = options.seconds {
            command.arg("-t").arg(seconds.to_string());
        }
        command.arg("-i").arg(clip);
    }
    command
        .arg("-filter_complex")
        .arg(&filter)
        .arg("-map")
        .arg(if options.vaapi.is_some() {
            "[vhw]"
        } else {
            "[vout]"
        })
        .arg("-map")
        .arg("[aout]");

    if let Some(device) = &options.vaapi {
        command
            .arg("-vaapi_device")
            .arg(device)
            .arg("-c:v")
            .arg("h264_vaapi")
            .arg("-qp")
            .arg(options.crf.to_string())
            .arg("-g")
            .arg("60")
            .arg("-force_key_frames")
            .arg("expr:gte(t,n_forced*4)");
    } else {
        command
            .arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg(&options.preset)
            .arg("-crf")
            .arg(options.crf.to_string())
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-profile:v")
            .arg("high")
            .arg("-g")
            .arg("60")
            .arg("-keyint_min")
            .arg("60")
            .arg("-sc_threshold")
            .arg("0")
            .arg("-force_key_frames")
            .arg("expr:gte(t,n_forced*4)");
    }

    command
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("192k")
        .arg("-ar")
        .arg("48000")
        .arg("-ac")
        .arg("2")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg("4")
        .arg("-hls_list_size")
        .arg("0")
        .arg("-hls_flags")
        .arg("independent_segments+program_date_time+discont_start")
        .arg("-hls_segment_filename")
        .arg(&segment_pattern)
        .arg(&playlist)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));

    let started = Instant::now();
    let mut child = command.spawn().context("spawn ffmpeg")?;
    let first_segment = wait_for_first_segment(&options.out, &mut child, Duration::from_secs(30))?;
    let status = child.wait().context("wait for ffmpeg")?;
    let total = started.elapsed();

    if !status.success() {
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        bail!("ffmpeg exited with {status}\n{log}");
    }

    let manifest =
        fs::read_to_string(&playlist).with_context(|| format!("read {}", playlist.display()))?;
    let entries: Vec<&str> = manifest
        .lines()
        .filter(|line| line.starts_with("#EXTINF:"))
        .collect();
    let duration: f64 = entries
        .iter()
        .filter_map(|line| {
            line.trim_start_matches("#EXTINF:")
                .split(',')
                .next()
                .and_then(|value| value.parse::<f64>().ok())
        })
        .sum();
    let has_pdt = manifest
        .lines()
        .any(|line| line.starts_with("#EXT-X-PROGRAM-DATE-TIME:"));
    let has_discontinuity = manifest.lines().any(|line| line == "#EXT-X-DISCONTINUITY");

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "out": options.out,
            "inputs": options.clips.len(),
            "first_segment_ms": first_segment.as_millis(),
            "total_ms": total.as_millis(),
            "segments": entries.len(),
            "playlist_duration_seconds": duration,
            "has_program_date_time": has_pdt,
            "has_discontinuity": has_discontinuity,
            "log": log_path,
            "vaapi": options.vaapi,
            "seconds_per_input": options.seconds,
        }))?
    );

    Ok(())
}

fn parse_args() -> Result<Options> {
    let mut out = PathBuf::from("target/hls-spike");
    let mut width = 1280;
    let mut height = 720;
    let mut fps = "30000/1001".to_string();
    let mut crf = 20;
    let mut preset = "veryfast".to_string();
    let mut vaapi = None;
    let mut seconds = None;
    let mut clips = Vec::new();

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = PathBuf::from(args.next().context("--out needs a path")?),
            "--width" => width = args.next().context("--width needs a value")?.parse()?,
            "--height" => height = args.next().context("--height needs a value")?.parse()?,
            "--fps" => fps = args.next().context("--fps needs a value")?,
            "--crf" => crf = args.next().context("--crf needs a value")?.parse()?,
            "--preset" => preset = args.next().context("--preset needs a value")?,
            "--vaapi" => vaapi = Some(args.next().context("--vaapi needs a device path")?),
            "--seconds" => seconds = Some(args.next().context("--seconds needs a value")?.parse()?),
            "--help" | "-h" => {
                println!(
                    "usage: awesometv-engine-spike [--out DIR] [--width N] [--height N] [--fps N/D] [--crf N] [--preset NAME] [--vaapi DEVICE] [--seconds N] CLIP..."
                );
                std::process::exit(0);
            }
            other if other.starts_with('-') => bail!("unknown argument: {other}"),
            other => clips.push(PathBuf::from(other)),
        }
    }

    if clips.is_empty() {
        bail!("provide at least one input clip");
    }

    Ok(Options {
        out,
        width,
        height,
        fps,
        crf,
        preset,
        vaapi,
        seconds,
        clips,
    })
}

fn build_filter(options: &Options) -> String {
    let mut parts = Vec::new();
    for (index, _) in options.clips.iter().enumerate() {
        parts.push(format!(
            "[{index}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}[v{index}]",
            w = options.width,
            h = options.height,
            fps = options.fps,
        ));
        parts.push(format!(
            "[{index}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[a{index}]"
        ));
    }
    let inputs: String = (0..options.clips.len())
        .map(|i| format!("[v{i}][a{i}]"))
        .collect();
    parts.push(format!(
        "{inputs}concat=n={}:v=1:a=1[vout][aout]",
        options.clips.len()
    ));
    if options.vaapi.is_some() {
        parts.push("[vout]format=nv12,hwupload[vhw]".to_string());
    }
    parts.join(";")
}

fn wait_for_first_segment(
    out: &Path,
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<Duration> {
    let started = Instant::now();
    loop {
        if let Ok(entries) = fs::read_dir(out) {
            let found = entries.flatten().any(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value == "ts")
            });
            if found {
                return Ok(started.elapsed());
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!("ffmpeg exited before producing a segment: {status}");
        }
        if started.elapsed() > timeout {
            bail!("timed out waiting for the first segment");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
