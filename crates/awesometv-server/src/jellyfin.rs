use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
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
    sync_with_timeout(
        base_url,
        api_key,
        movies_root,
        tv_root,
        music_root,
        limit,
        Duration::from_secs(30),
    )
}

fn sync_with_timeout(
    base_url: &str,
    api_key: &str,
    movies_root: &Path,
    tv_root: &Path,
    music_root: &Path,
    limit: usize,
    timeout: Duration,
) -> Result<(SyncReport, Vec<Media>)> {
    if api_key.trim().is_empty() {
        bail!("media server API key is not set");
    }
    let base = base_url.trim_end_matches('/');
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(0)
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
    let mut expected_total = None;
    let target = limit.max(1);

    loop {
        let url = format!(
            "{base}/Items?Recursive=true&IncludeItemTypes=Movie,Episode&Fields=Path,RunTimeTicks,SeriesName,ParentIndexNumber,IndexNumber,ProductionYear,MediaSources&Limit={PAGE_SIZE}&StartIndex={start}"
        );
        let response = (0..3)
            .find_map(
                |attempt| match agent.get(&url).set("Authorization", &auth).call() {
                    Ok(response) => Some(Ok(response)),
                    Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {
                        if attempt == 2 {
                            Some(Err(anyhow::anyhow!(
                                "Jellyfin request failed (HTTP {code})"
                            )))
                        } else {
                            None
                        }
                    }
                    Err(ureq::Error::Status(code, _)) => Some(Err(anyhow::anyhow!(
                        "Jellyfin request failed (HTTP {code})"
                    ))),
                    Err(_) if attempt == 2 => Some(Err(anyhow::anyhow!("Jellyfin request failed"))),
                    Err(_) => None,
                },
            )
            .expect("bounded attempts")?;
        let value: Value = response
            .into_json()
            .map_err(|_| anyhow::anyhow!("invalid Jellyfin response"))?;
        let total = value
            .get("TotalRecordCount")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| anyhow::anyhow!("Jellyfin response missing total"))?;
        if expected_total.is_some_and(|previous| previous != total) {
            bail!("Jellyfin listing changed during refresh");
        }
        expected_total = Some(total);
        if total > target {
            bail!("Jellyfin listing exceeds refresh limit");
        }
        let items = value
            .get("Items")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("Jellyfin response missing items"))?;
        if items.len() > PAGE_SIZE
            || start + items.len() > total
            || (items.is_empty() && start < total)
        {
            bail!("incomplete Jellyfin listing");
        }
        for item in items {
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
        start += items.len();
        if start == total {
            break;
        }
    }

    Ok((report, imported))
}

/// Publish a fully staged listing in one transaction. Stale rows are retained.
pub async fn commit(pool: &sqlx::SqlitePool, media: &[Media]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for item in media {
        sqlx::query(r#"INSERT INTO media_items (id, path, title, kind, duration_ms, width, height, video_codec, audio_codec, size_bytes, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(path) DO UPDATE SET title=excluded.title, kind=excluded.kind,
            duration_ms=excluded.duration_ms, width=excluded.width, height=excluded.height,
            video_codec=excluded.video_codec, audio_codec=excluded.audio_codec, size_bytes=excluded.size_bytes"#)
            .bind(&item.id).bind(&item.path).bind(&item.title).bind(&item.kind)
            .bind(item.duration_ms).bind(item.width).bind(item.height).bind(&item.video_codec)
            .bind(&item.audio_codec).bind(item.size_bytes).bind(crate::db::now_ms())
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
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

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn server(responses: Vec<&'static str>) -> (String, thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for (i, body) in responses.iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0; 4096];
                let _ = stream.read(&mut buf);
                let status = if *body == "401" {
                    "401 Unauthorized"
                } else if *body == "500" {
                    "500 Error"
                } else {
                    "200 OK"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                if *body == "401" {
                    return i + 1;
                }
            }
            responses.len()
        });
        (format!("http://{addr}"), handle)
    }
    fn run(url: &str, limit: usize) -> Result<(SyncReport, Vec<Media>)> {
        sync(
            url,
            "secret-token",
            Path::new("/tmp"),
            Path::new("/tmp"),
            Path::new("/tmp"),
            limit,
        )
    }
    #[test]
    fn http_refresh_boundaries_and_failures() {
        let (url, h) = server(vec!["500", r#"{"TotalRecordCount":0,"Items":[]}"#]);
        assert!(run(&url, 1).is_ok());
        assert_eq!(h.join().unwrap(), 2);
        let (url, h) = server(vec!["401"]);
        let error = run(&url, 1).unwrap_err().to_string();
        assert!(error.contains("401") && !error.contains("secret-token") && !error.contains(&url));
        assert_eq!(h.join().unwrap(), 1);
        let (url, h) = server(vec![r#"{"TotalRecordCount":2,"Items":[]}"#]);
        assert!(run(&url, 1).is_err());
        h.join().unwrap();
        let (url, h) = server(vec![
            r#"{"TotalRecordCount":2,"Items":[{}]}"#,
            "500",
            "500",
            "500",
        ]);
        assert!(run(&url, 2).is_err());
        assert_eq!(h.join().unwrap(), 4);
        let (url, h) = server(vec![r#"{"TotalRecordCount":1,"Items":[{}]}"#]);
        assert!(run(&url, 1).is_ok());
        h.join().unwrap();
    }
    #[test]
    fn timeout_is_bounded_and_sanitized() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                thread::sleep(Duration::from_millis(80));
                drop(stream);
            }
        });
        let error = sync_with_timeout(
            &url,
            "secret-token",
            Path::new("/tmp"),
            Path::new("/tmp"),
            Path::new("/tmp"),
            1,
            Duration::from_millis(20),
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("secret-token") && !error.contains(&url));
        handle.join().unwrap();
    }
    #[tokio::test]
    async fn transaction_rolls_back_on_second_item_failure() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE media_items (id TEXT, path TEXT UNIQUE, title TEXT CHECK(title != 'bad'), kind TEXT, duration_ms INTEGER, width INTEGER, height INTEGER, video_codec TEXT, audio_codec TEXT, size_bytes INTEGER, created_at INTEGER)").execute(&pool).await.unwrap();
        let media = |path: &str, title: &str| Media {
            id: path.into(),
            path: path.into(),
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
        commit(&pool, &[media("old", "old")]).await.unwrap();
        assert!(
            commit(&pool, &[media("old", "new"), media("bad", "bad")])
                .await
                .is_err()
        );
        let title: String = sqlx::query_scalar("SELECT title FROM media_items WHERE path='old'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "old");
    }
}
