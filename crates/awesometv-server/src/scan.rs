use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::db::{Chapter, Media};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "ts", "mpg", "mpeg", "webm", "wmv", "flv",
];
const MUSIC_EXTENSIONS: &[&str] = &["flac", "mp3", "m4a", "aac", "ogg", "opus", "wav"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

#[derive(Debug, Serialize)]
pub struct ScanFileStatus {
    pub path: String,
    pub error: Option<String>,
}

pub struct ScanReport {
    pub media: Vec<Media>,
    pub files: Vec<ScanFileStatus>,
    pub complete: bool,
}

pub fn scan_path(root: &Path, limit: usize, kind: &str) -> Result<ScanReport> {
    if !root.is_dir() {
        bail!(
            "media directory does not exist or is unreadable: {}",
            root.display()
        );
    }
    let extensions = extensions_for(kind);
    let mut paths = Vec::new();
    let mut files = Vec::new();
    let mut complete = true;
    for entry in WalkDir::new(root).follow_links(false) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                let path = entry.into_path();
                if !is_ignored(&path)
                    && path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| extensions.contains(&e.to_ascii_lowercase().as_str()))
                        .unwrap_or(false)
                {
                    paths.push(path);
                }
            }
            Ok(_) => {}
            Err(error) => {
                complete = false;
                files.push(ScanFileStatus {
                    path: error
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    error: Some(error.to_string()),
                });
            }
        }
    }
    paths.sort();
    if limit > 0 && paths.len() > limit {
        paths.truncate(limit);
        complete = false;
        files.push(ScanFileStatus {
            path: root.display().to_string(),
            error: Some(format!(
                "scan limit {limit} reached; inventory is incomplete"
            )),
        });
    }
    let mut media = Vec::new();
    for path in paths {
        let error = match probe(&path, kind) {
            Ok(item) => {
                media.push(item);
                None
            }
            Err(error) => {
                complete = false;
                Some(format!("{error:#}"))
            }
        };
        files.push(ScanFileStatus {
            path: path.display().to_string(),
            error,
        });
    }
    Ok(ScanReport {
        media,
        files,
        complete,
    })
}

// Drain pipes concurrently to avoid a full pipe blocking the child, but retain bounded output.
fn bounded_probe_output(
    command: &mut Command,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start ffprobe")?;
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let remaining = (1024 * 1024usize).saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(remaining)]);
            }
            kept
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(anyhow::anyhow!("ffprobe timed out after 15 seconds"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.into());
            }
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    Ok((status?, out, err))
}

pub fn probe(path: &Path, kind: &str) -> Result<Media> {
    let (status, stdout, stderr) = bounded_probe_output(
        Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-print_format")
            .arg("json")
            .arg("-show_format")
            .arg("-show_streams")
            .arg("-show_chapters")
            .arg(path),
    )
    .with_context(|| format!("run ffprobe on {}", path.display()))?;
    if !status.success() {
        bail!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&stderr[..stderr.len().min(4096)])
        );
    }
    let value: Value = serde_json::from_slice(&stdout).context("parse ffprobe JSON")?;
    let duration_seconds = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(|duration| duration.as_str())
        .and_then(|duration| duration.parse::<f64>().ok())
        .unwrap_or(0.0);
    let mut duration_ms = (duration_seconds * 1000.0).round() as i64;
    if duration_ms <= 0 {
        if kind == "image" {
            duration_ms = 15_000;
        } else {
            bail!("no duration for {}", path.display());
        }
    }

    let streams = value
        .get("streams")
        .and_then(|streams| streams.as_array())
        .cloned()
        .unwrap_or_default();
    let video = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(|kind| kind.as_str()) == Some("video"));
    let audio = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(|kind| kind.as_str()) == Some("audio"));
    let width = video
        .and_then(|stream| stream.get("width"))
        .and_then(|width| width.as_i64());
    let height = video
        .and_then(|stream| stream.get("height"))
        .and_then(|height| height.as_i64());
    let video_codec = video
        .and_then(|stream| stream.get("codec_name"))
        .and_then(|codec| codec.as_str())
        .map(str::to_string);
    let audio_codec = audio
        .and_then(|stream| stream.get("codec_name"))
        .and_then(|codec| codec.as_str())
        .map(str::to_string);
    let size_bytes = value
        .get("format")
        .and_then(|format| format.get("size"))
        .and_then(|size| size.as_str())
        .and_then(|size| size.parse::<i64>().ok());

    let canonical = path.to_string_lossy().to_string();
    let id = Uuid::new_v5(&Uuid::NAMESPACE_URL, canonical.as_bytes()).to_string();
    let title = title_for(path, kind);

    Ok(Media {
        id,
        path: canonical,
        title,
        kind: kind.to_string(),
        duration_ms,
        width,
        height,
        video_codec,
        audio_codec,
        size_bytes,
        chapters: parse_chapters(&value),
    })
}

#[derive(Debug, Clone, Default)]
pub struct MediaProbe {
    pub duration_ms: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

pub fn probe_target(target: &str, kind: &str) -> Result<MediaProbe> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(target)
        .output()
        .with_context(|| format!("run ffprobe on {target}"))?;
    if !output.status.success() {
        bail!(
            "ffprobe failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout).context("parse ffprobe JSON")?;
    let duration_seconds = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(|duration| duration.as_str())
        .and_then(|duration| duration.parse::<f64>().ok())
        .unwrap_or(0.0);
    let mut duration_ms = (duration_seconds * 1000.0).round() as i64;
    if duration_ms <= 0 && kind == "image" {
        duration_ms = 15_000;
    }
    let streams = value
        .get("streams")
        .and_then(|streams| streams.as_array())
        .cloned()
        .unwrap_or_default();
    let video = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(|kind| kind.as_str()) == Some("video"));
    let audio = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(|kind| kind.as_str()) == Some("audio"));
    Ok(MediaProbe {
        duration_ms,
        width: video
            .and_then(|stream| stream.get("width"))
            .and_then(|width| width.as_i64()),
        height: video
            .and_then(|stream| stream.get("height"))
            .and_then(|height| height.as_i64()),
        video_codec: video
            .and_then(|stream| stream.get("codec_name"))
            .and_then(|codec| codec.as_str())
            .map(str::to_string),
        audio_codec: audio
            .and_then(|stream| stream.get("codec_name"))
            .and_then(|codec| codec.as_str())
            .map(str::to_string),
    })
}

pub fn probe_chapters(path: &Path) -> Result<Vec<Chapter>> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_chapters")
        .arg(path)
        .output()
        .with_context(|| format!("run ffprobe chapters on {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "ffprobe chapters failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout).context("parse ffprobe JSON")?;
    Ok(parse_chapters(&value))
}

fn parse_chapters(value: &Value) -> Vec<Chapter> {
    value
        .get("chapters")
        .and_then(|chapters| chapters.as_array())
        .map(|chapters| {
            chapters
                .iter()
                .filter_map(|chapter| {
                    let start = json_number(chapter, "start_time")?;
                    let end = json_number(chapter, "end_time").unwrap_or(start);
                    let title = chapter
                        .get("tags")
                        .and_then(|tags| tags.get("title"))
                        .and_then(|title| title.as_str())
                        .map(str::to_string);
                    Some(Chapter {
                        start_ms: (start * 1000.0).round() as i64,
                        end_ms: (end * 1000.0).round() as i64,
                        title,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn json_number(value: &Value, key: &str) -> Option<f64> {
    let field = value.get(key)?;
    field
        .as_str()
        .and_then(|text| text.parse::<f64>().ok())
        .or_else(|| field.as_f64())
}

fn extensions_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "music" => MUSIC_EXTENSIONS,
        "image" => IMAGE_EXTENSIONS,
        _ => VIDEO_EXTENSIONS,
    }
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.ends_with(".1") || name == "#recycle" || name == "@eaDir" || name.starts_with("._")
    })
}

fn title_for(path: &Path, kind: &str) -> String {
    if let Some(title) = nfo_title(path) {
        return title;
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Untitled");
    let mut title = stem.replace(['.', '_'], " ");
    if let Some((show, episode)) = title.split_once(" - S") {
        title = format!("{show} - S{episode}");
    }
    title = title.split_whitespace().collect::<Vec<_>>().join(" ");

    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if kind == "episode" {
        let show = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .replace(['.', '_'], " ");
        if !show.is_empty() && !show.eq_ignore_ascii_case("tv") {
            if let Some(episode) = episode_label(stem) {
                return format!("{show} — {episode} — {title}");
            }
            return format!("{show} — {title}");
        }
    }
    if kind == "movie" && !parent.is_empty() && !parent.to_ascii_lowercase().starts_with("season") {
        let without_year = parent
            .rfind(" (")
            .map(|index| &parent[..index])
            .unwrap_or(parent);
        let cleaned = without_year
            .replace(['.', '_'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    title
}

fn nfo_title(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let parent = path.parent()?;
    let episode_nfo = parent.join(format!("{stem}.nfo"));
    let movie_nfo = parent.join("movie.nfo");
    let show_title = find_ancestor_nfo(parent).and_then(|nfo| xml_text(&nfo, "title"));

    if episode_nfo.is_file() {
        let raw = std::fs::read_to_string(&episode_nfo).ok()?;
        let title = xml_text_from(&raw, "title");
        let show = xml_text_from(&raw, "showtitle").or(show_title);
        let episode = episode_label(stem);
        return match (show, title, episode) {
            (Some(show), Some(title), Some(episode)) => {
                Some(format!("{show} — {episode} — {title}"))
            }
            (Some(show), Some(title), None) => Some(format!("{show} — {title}")),
            (_, Some(title), _) => Some(title),
            _ => None,
        };
    }
    if movie_nfo.is_file() {
        return xml_text(&movie_nfo, "title");
    }
    if let Some(show) = show_title {
        if let Some(episode) = episode_label(stem) {
            return Some(format!("{show} — {episode}"));
        }
        return Some(show);
    }
    None
}

fn find_ancestor_nfo(start: &Path) -> Option<std::path::PathBuf> {
    for ancestor in start.ancestors().take(4) {
        let candidate = ancestor.join("tvshow.nfo");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn episode_label(stem: &str) -> Option<String> {
    let upper = stem.to_ascii_uppercase();
    for (pos, _) in upper.match_indices('S') {
        let rest = &upper[pos + 1..];
        let season: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if season.is_empty() {
            continue;
        }
        let after = &rest[season.len()..];
        if !after.starts_with('E') {
            continue;
        }
        let episode: String = after[1..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !episode.is_empty() {
            return Some(format!("S{season}E{episode}"));
        }
    }
    None
}

fn xml_text(path: &Path, tag: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    xml_text_from(&raw, tag)
}

fn xml_text_from(raw: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = raw.find(&open)? + open.len();
    let end = raw[start..].find(&close)? + start;
    let value = raw[start..end]
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod local_scan_tests {
    use super::*;
    use crate::db;

    fn wav(path: &Path, samples: usize) {
        let data_len = (samples * 2) as u32;
        let mut data = Vec::new();
        data.extend(b"RIFF");
        data.extend((36 + data_len).to_le_bytes());
        data.extend(b"WAVEfmt ");
        data.extend(16u32.to_le_bytes());
        data.extend(1u16.to_le_bytes());
        data.extend(1u16.to_le_bytes());
        data.extend(8000u32.to_le_bytes());
        data.extend(16000u32.to_le_bytes());
        data.extend(2u16.to_le_bytes());
        data.extend(16u16.to_le_bytes());
        data.extend(b"data");
        data.extend(data_len.to_le_bytes());
        data.resize(44 + data_len as usize, 0);
        std::fs::write(path, data).unwrap();
    }

    async fn persist(pool: &sqlx::SqlitePool, root: &Path, report: &ScanReport) -> Vec<String> {
        for item in &report.media {
            db::upsert_media(pool, item).await.unwrap();
        }
        db::reconcile_local_scan(
            pool,
            root.to_str().unwrap(),
            "music",
            &report.media,
            report.complete,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn local_scan_reconciles_only_complete_inventory() {
        let base = std::env::temp_dir().join(format!("awesome-scan-{}", Uuid::new_v4()));
        let root = base.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::init(&base.join("db")).await.unwrap();
        let a = root.join("a.wav");
        let b = root.join("b.wav");
        wav(&a, 8000);
        wav(&b, 8000);
        let initial = scan_path(&root, 0, "music").unwrap();
        assert!(initial.complete);
        assert_eq!(initial.files.len(), 2);
        assert!(persist(&pool, &root, &initial).await.is_empty());
        std::fs::remove_file(&b).unwrap();
        wav(&a, 16000);
        let c = root.join("c.wav");
        wav(&c, 8000);
        let corrupt = root.join("broken.wav");
        std::fs::write(&corrupt, b"invalid").unwrap();
        let partial = scan_path(&root, 0, "music").unwrap();
        assert!(!partial.complete);
        assert!(
            partial
                .files
                .iter()
                .any(|f| f.path == corrupt.to_string_lossy() && f.error.is_some())
        );
        assert!(persist(&pool, &root, &partial).await.is_empty());
        assert_eq!(db::list_media(&pool).await.unwrap().len(), 3);
        std::fs::remove_file(&corrupt).unwrap();
        let interrupted = scan_path(&root, 1, "music").unwrap();
        assert!(!interrupted.complete);
        assert!(
            interrupted
                .files
                .iter()
                .any(|f| f.error.as_deref().is_some_and(|e| e.contains("limit")))
        );
        assert!(persist(&pool, &root, &interrupted).await.is_empty());
        // Dropping a scan before persistence cannot commit missing markers.
        drop(scan_path(&root, 0, "music").unwrap());
        assert!(
            db::reconcile_local_scan(&pool, root.to_str().unwrap(), "music", &[], false)
                .await
                .unwrap()
                .is_empty()
        );
        let complete = scan_path(&root, 0, "music").unwrap();
        assert!(complete.complete);
        assert_eq!(
            persist(&pool, &root, &complete).await,
            vec![b.to_string_lossy().to_string()]
        );
        let items = db::list_media(&pool).await.unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items
                .iter()
                .find(|m| m.path == a.to_string_lossy())
                .unwrap()
                .duration_ms,
            2000
        );
        assert!(scan_path(&root.join("absent"), 0, "music").is_err());
        pool.close().await;
        std::fs::remove_dir_all(base).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_directory_preserves_missing_markers() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("awesome-scan-permission-{}", Uuid::new_v4()));
        let root = base.join("library");
        let blocked = root.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let pool = db::init(&base.join("db")).await.unwrap();
        let hidden = blocked.join("hidden.wav");
        wav(&hidden, 8000);
        let initial = scan_path(&root, 0, "music").unwrap();
        assert!(initial.complete);
        assert!(persist(&pool, &root, &initial).await.is_empty());
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0)).unwrap();
        let denial_enforced = std::fs::read_dir(&blocked).is_err();
        let report = scan_path(&root, 0, "music").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !denial_enforced {
            eprintln!("permission proof unavailable: directory remains readable");
            pool.close().await;
            std::fs::remove_dir_all(base).unwrap();
            return;
        }
        assert!(!report.complete);
        assert!(report.files.iter().any(|entry| entry.error.is_some()));
        assert!(persist(&pool, &root, &report).await.is_empty());
        assert_eq!(db::list_media(&pool).await.unwrap().len(), 1);
        pool.close().await;
        std::fs::remove_dir_all(base).unwrap();
    }
}
