use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::db;

const WEEK_MS: i64 = 7 * 86_400_000;

#[derive(Debug, Deserialize)]
pub struct ExternalPlayout {
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub start_ms: Option<i64>,
    #[serde(default, rename = "loop")]
    pub loop_playout: bool,
    #[serde(default)]
    pub items: Vec<ExternalItem>,
}

#[derive(Debug, Deserialize)]
pub struct ExternalItem {
    #[serde(default)]
    pub media_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub in_seconds: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

pub struct AppliedPlayout {
    pub items: usize,
    pub start_ms: i64,
    pub end_ms: i64,
}

struct ResolvedItem {
    media: db::Media,
    in_ms: i64,
    duration_ms: i64,
    kind: String,
}

pub async fn apply(pool: &SqlitePool, channel_id: &str, json: &str) -> Result<AppliedPlayout> {
    let playout: ExternalPlayout =
        serde_json::from_str(json).context("parse external playout JSON")?;
    if playout.items.is_empty() {
        bail!("playout needs at least one item");
    }
    let start_ms = resolve_start(&playout)?;

    let mut resolved = Vec::new();
    for item in &playout.items {
        let reference = match (&item.media_id, &item.title) {
            (Some(media_id), _) if !media_id.trim().is_empty() => media_id.trim(),
            (_, Some(title)) if !title.trim().is_empty() => title.trim(),
            _ => bail!("each playout item needs media_id or title"),
        };
        let media = db::media_by_reference(pool, reference)
            .await?
            .with_context(|| format!("media not found: {reference}"))?;
        let in_ms = (item.in_seconds.unwrap_or(0.0).max(0.0) * 1000.0).round() as i64;
        if in_ms >= media.duration_ms {
            bail!("in_seconds is past the end of '{}'", media.title);
        }
        let duration_ms = match item.duration_seconds {
            Some(seconds) => {
                let duration = (seconds * 1000.0).round() as i64;
                if duration <= 0 {
                    bail!("duration_seconds must be positive");
                }
                duration
            }
            None => media.duration_ms - in_ms,
        };
        let kind = match item.kind.as_deref() {
            None | Some("program") => "program",
            Some("filler") => "filler",
            Some(other) => bail!("unknown playout item type: {other}"),
        }
        .to_string();
        resolved.push(ResolvedItem {
            media,
            in_ms,
            duration_ms,
            kind,
        });
    }

    let window_end = start_ms + WEEK_MS;
    let mut writes = Vec::new();
    let mut cursor = start_ms;
    let mut guard = 0usize;
    loop {
        for item in &resolved {
            if cursor >= window_end {
                break;
            }
            guard += 1;
            if guard > 100_000 {
                bail!("playout expands past 100,000 items");
            }
            let end_ms = cursor + item.duration_ms;
            writes.push(db::TimelineWrite {
                media_id: item.media.id.clone(),
                title: item.media.title.clone(),
                start_ms: cursor,
                end_ms,
                in_ms: item.in_ms,
                out_ms: item.in_ms + item.duration_ms,
                kind: item.kind.clone(),
            });
            cursor = end_ms;
        }
        if !playout.loop_playout || cursor >= window_end {
            break;
        }
    }
    if writes.is_empty() {
        bail!("playout produced no timeline items");
    }
    db::replace_channel_timeline(pool, channel_id, &writes, start_ms).await?;
    db::set_schedule_mode(pool, channel_id, "external").await?;
    Ok(AppliedPlayout {
        items: writes.len(),
        start_ms,
        end_ms: cursor,
    })
}

fn resolve_start(playout: &ExternalPlayout) -> Result<i64> {
    if let Some(start_ms) = playout.start_ms {
        if start_ms < 0 {
            bail!("start_ms cannot be negative");
        }
        return Ok(start_ms);
    }
    if let Some(start) = &playout.start {
        let parsed = OffsetDateTime::parse(start.trim(), &Rfc3339)
            .with_context(|| format!("invalid start timestamp: {start}"))?;
        return Ok(parsed.unix_timestamp() * 1000);
    }
    bail!("playout needs a start timestamp or start_ms")
}

#[derive(Serialize)]
struct PlayoutExport {
    start: String,
    start_ms: i64,
    items: Vec<PlayoutExportItem>,
}

#[derive(Serialize)]
struct PlayoutExportItem {
    media_id: String,
    title: String,
    in_seconds: f64,
    duration_seconds: f64,
    #[serde(rename = "type")]
    kind: String,
}

pub async fn export(pool: &SqlitePool, channel_id: &str) -> Result<String> {
    let channel = db::channel_by_id(pool, channel_id)
        .await?
        .context("channel not found")?;
    let items = db::all_channel_items(pool, channel_id).await?;
    if items.is_empty() {
        bail!("channel has no timeline items");
    }
    let start_ms = items
        .first()
        .map(|item| item.start_ms)
        .unwrap_or(channel.start_ms);
    let start = OffsetDateTime::from_unix_timestamp(start_ms / 1000)
        .context("invalid start timestamp")?
        .format(&Rfc3339)?;
    let export = PlayoutExport {
        start,
        start_ms,
        items: items
            .into_iter()
            .map(|item| PlayoutExportItem {
                media_id: item.media_id,
                title: item.title,
                in_seconds: item.in_ms as f64 / 1000.0,
                duration_seconds: (item.end_ms - item.start_ms) as f64 / 1000.0,
                kind: item.kind,
            })
            .collect(),
    };
    Ok(serde_json::to_string_pretty(&export)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_external_playout() {
        let playout: ExternalPlayout = serde_json::from_str(
            r#"{
                "start": "2026-11-15T00:00:00Z",
                "loop": true,
                "items": [
                    {"media_id": "a", "duration_seconds": 120},
                    {"title": "Bumper", "in_seconds": 2}
                ]
            }"#,
        )
        .unwrap();
        assert!(playout.loop_playout);
        assert_eq!(playout.items.len(), 2);
        assert_eq!(playout.items[1].title.as_deref(), Some("Bumper"));
    }

    #[test]
    fn parses_rfc3339_start() {
        let playout = ExternalPlayout {
            start: Some("2026-11-15T01:00:00Z".into()),
            start_ms: None,
            loop_playout: false,
            items: Vec::new(),
        };
        assert_eq!(resolve_start(&playout).unwrap(), 1_794_704_400_000);
    }

    #[test]
    fn start_is_required() {
        let playout = ExternalPlayout {
            start: None,
            start_ms: None,
            loop_playout: false,
            items: Vec::new(),
        };
        assert!(resolve_start(&playout).is_err());
    }
}
