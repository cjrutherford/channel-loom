use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::db::Media;

const PAGE_SIZE: usize = 500;

#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub total: usize,
    pub imported: usize,
    pub skipped: usize,
    pub errors: usize,
    pub missing_paths: Vec<String>,
}

pub fn sync(
    base_url: &str,
    api_key: &str,
    movies_root: &Path,
    tv_root: &Path,
    music_root: &Path,
    limit: usize,
) -> Result<(SyncReport, Vec<Media>)> {
    if api_key.trim().is_empty() {
        bail!("media server API key is not set");
    }
    let base = base_url.trim_end_matches('/');
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let auth = format!("MediaBrowser Token={api_key}");
    let mut report = SyncReport {
        total: 0,
        imported: 0,
        skipped: 0,
        errors: 0,
        missing_paths: Vec::new(),
    };
    let mut imported = Vec::new();
    let mut start = 0usize;
    let target = limit.max(1);

    loop {
        let url = format!(
            "{base}/Items?Recursive=true&IncludeItemTypes=Movie,Episode&Fields=Path,RunTimeTicks,SeriesName,ParentIndexNumber,IndexNumber,ProductionYear,MediaSources&Limit={PAGE_SIZE}&StartIndex={start}"
        );
        let response = agent
            .get(&url)
            .set("Authorization", &auth)
            .call()
            .with_context(|| format!("query Jellyfin items at {start}"))?;
        let value: Value = response.into_json().context("parse Jellyfin items")?;
        let items = value
            .get("Items")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            break;
        }
        for item in &items {
            if report.total >= target {
                break;
            }
            report.total += 1;
            match process_item(item, movies_root, tv_root, music_root) {
                Ok(Some(media)) => {
                    report.imported += 1;
                    imported.push(media);
                }
                Ok(None) => {
                    report.skipped += 1;
                    if report.missing_paths.len() < 10
                        && let Some(path) = item_path(item)
                    {
                        report.missing_paths.push(path);
                    }
                }
                Err(error) => {
                    report.errors += 1;
                    tracing::warn!(%error, "jellyfin item failed");
                }
            }
        }
        if report.total >= target || items.len() < PAGE_SIZE {
            break;
        }
        start += PAGE_SIZE;
    }

    Ok((report, imported))
}

fn process_item(
    item: &Value,
    movies_root: &Path,
    tv_root: &Path,
    music_root: &Path,
) -> Result<Option<Media>> {
    let item_type = item
        .get("Type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let kind = match item_type {
        "Movie" => "movie",
        "Episode" => "episode",
        _ => return Ok(None),
    };
    let Some(path) = item_path(item) else {
        return Ok(None);
    };
    let Some(local) = map_local_path(&path, movies_root, tv_root, music_root) else {
        return Ok(None);
    };
    if !local.is_file() {
        return Ok(None);
    }
    let title = jellyfin_title(item, "Untitled");
    let duration_ms = item
        .get("RunTimeTicks")
        .and_then(|value| value.as_i64())
        .map(|ticks| ticks / 10_000)
        .unwrap_or(0);
    if duration_ms <= 0 {
        return Ok(None);
    }
    let streams = item
        .get("MediaSources")
        .and_then(|value| value.as_array())
        .and_then(|sources| sources.first())
        .and_then(|source| source.get("MediaStreams"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let video = streams
        .iter()
        .find(|stream| stream.get("Type").and_then(|value| value.as_str()) == Some("Video"));
    let audio = streams
        .iter()
        .find(|stream| stream.get("Type").and_then(|value| value.as_str()) == Some("Audio"));
    let media = Media {
        id: uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            local.to_string_lossy().as_bytes(),
        )
        .to_string(),
        path: local.to_string_lossy().to_string(),
        title,
        kind: kind.to_string(),
        duration_ms,
        width: video
            .and_then(|stream| stream.get("Width"))
            .and_then(|value| value.as_i64()),
        height: video
            .and_then(|stream| stream.get("Height"))
            .and_then(|value| value.as_i64()),
        video_codec: video
            .and_then(|stream| stream.get("Codec"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
        audio_codec: audio
            .and_then(|stream| stream.get("Codec"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
        size_bytes: None,
        chapters: Vec::new(),
    };
    Ok(Some(media))
}

fn jellyfin_title(item: &Value, fallback: &str) -> String {
    let name = item
        .get("Name")
        .and_then(|value| value.as_str())
        .unwrap_or(fallback);
    let series = item
        .get("SeriesName")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let season = item
        .get("ParentIndexNumber")
        .and_then(|value| value.as_i64());
    let episode = item.get("IndexNumber").and_then(|value| value.as_i64());
    match (series.is_empty(), season, episode) {
        (false, Some(season), Some(episode)) => {
            format!("{series} — S{season:02}E{episode:02} — {name}")
        }
        (false, _, _) => format!("{series} — {name}"),
        _ => name.to_string(),
    }
}

fn item_path(item: &Value) -> Option<String> {
    item.get("Path")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| {
            item.get("MediaSources")
                .and_then(|value| value.as_array())
                .and_then(|sources| sources.first())
                .and_then(|source| source.get("Path"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn map_local_path(
    path: &str,
    movies_root: &Path,
    tv_root: &Path,
    music_root: &Path,
) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("/data/movies/") {
        return Some(movies_root.join(rest));
    }
    if let Some(rest) = path.strip_prefix("/data/tvshows/") {
        return Some(tv_root.join(rest));
    }
    if let Some(rest) = path.strip_prefix("/data/music/") {
        return Some(music_root.join(rest));
    }
    let direct = PathBuf::from(path);
    direct.is_file().then_some(direct)
}
