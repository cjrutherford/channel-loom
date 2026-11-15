use std::path::{Path, PathBuf};
use std::process::Command;

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

pub fn scan_path(root: &Path, limit: usize, kind: &str) -> Result<Vec<Media>> {
    if !root.exists() {
        bail!("media path does not exist: {}", root.display());
    }
    let extensions = extensions_for(kind);
    let mut paths: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| !is_ignored(path))
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| extensions.contains(&ext.to_ascii_lowercase().as_str()))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    if limit > 0 {
        paths.truncate(limit);
    }

    let mut media = Vec::new();
    for path in paths {
        match probe(&path, kind) {
            Ok(item) => media.push(item),
            Err(error) => tracing::warn!(path = %path.display(), %error, "probe failed"),
        }
    }
    Ok(media)
}

pub fn probe(path: &Path, kind: &str) -> Result<Media> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg("-show_chapters")
        .arg(path)
        .output()
        .with_context(|| format!("run ffprobe on {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "ffprobe failed for {}: {}",
            path.display(),
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
