use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db;
use crate::programming::{self, RebuildReport};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptedSchedule {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub items: Vec<ScriptedItem>,
    #[serde(default)]
    pub blocks: Vec<ScriptedBlock>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptedItem {
    pub collection: String,
    #[serde(default = "default_item_mode")]
    pub mode: String,
    #[serde(default = "one")]
    pub count: i64,
    #[serde(default = "sixty")]
    pub duration_minutes: i64,
    #[serde(default)]
    pub filler_collection: Option<String>,
    #[serde(default)]
    pub mid_roll_interval_minutes: i64,
    #[serde(default)]
    pub mid_roll_duration_seconds: i64,
    #[serde(default = "default_true")]
    pub chapter_breaks: bool,
    #[serde(default, deserialize_with = "deserialize_break_markers")]
    pub break_markers: Vec<i64>,
    #[serde(default = "default_order")]
    pub order: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptedBlock {
    #[serde(default = "default_days")]
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
    pub collection: String,
    #[serde(default)]
    pub filler_collection: Option<String>,
    #[serde(default = "default_order")]
    pub order: String,
}

pub struct AppliedSchedule {
    pub mode: String,
    pub rebuild: RebuildReport,
}

fn default_item_mode() -> String {
    "multiple".into()
}
fn one() -> i64 {
    1
}
fn sixty() -> i64 {
    60
}
fn default_true() -> bool {
    true
}
fn default_order() -> String {
    "sequential".into()
}
fn default_days() -> Vec<String> {
    vec!["daily".into()]
}

pub struct ParsedSchedule {
    pub mode: String,
    pub definition: db::ScheduleDefinition,
}

pub async fn parse_definition(pool: &SqlitePool, yaml: &str) -> Result<ParsedSchedule> {
    let script: ScriptedSchedule = serde_yaml::from_str(yaml).context("parse schedule YAML")?;
    if !script.items.is_empty() && !script.blocks.is_empty() {
        bail!("choose classic items or weekly blocks, not both");
    }
    let mode = script
        .mode
        .clone()
        .unwrap_or_else(|| {
            if script.items.is_empty() {
                "blocks".into()
            } else {
                "classic".into()
            }
        })
        .to_ascii_lowercase();
    let definition = match mode.as_str() {
        "classic" => db::ScheduleDefinition::Classic(classic_inputs(pool, &script.items).await?),
        "blocks" => db::ScheduleDefinition::Blocks(block_inputs(pool, &script.blocks).await?),
        other => bail!("unknown schedule mode: {other}"),
    };
    Ok(ParsedSchedule { mode, definition })
}

pub async fn apply(pool: &SqlitePool, channel_id: &str, yaml: &str) -> Result<AppliedSchedule> {
    let parsed = parse_definition(pool, yaml).await?;
    match &parsed.definition {
        db::ScheduleDefinition::Classic(items) => {
            db::set_classic_items(pool, channel_id, items).await?;
        }
        db::ScheduleDefinition::Blocks(blocks) => {
            db::set_channel_blocks(pool, channel_id, blocks).await?;
            db::set_schedule_mode(pool, channel_id, "blocks").await?;
        }
    }
    let rebuild = programming::rebuild_channel(pool, channel_id).await?;
    Ok(AppliedSchedule {
        mode: parsed.mode,
        rebuild,
    })
}

async fn classic_inputs(
    pool: &SqlitePool,
    items: &[ScriptedItem],
) -> Result<Vec<db::ClassicItemInput>> {
    if items.is_empty() {
        bail!("classic mode needs at least one item");
    }
    let mut inputs = Vec::new();
    for item in items {
        if !matches!(item.mode.as_str(), "one" | "multiple" | "duration") {
            bail!(
                "item mode must be one, multiple, or duration: {}",
                item.mode
            );
        }
        validate_order(&item.order)?;
        if item.count < 1 {
            bail!("item count must be at least 1");
        }
        if item.duration_minutes < 1 {
            bail!("item duration_minutes must be at least 1");
        }
        if item.mid_roll_interval_minutes < 0 || item.mid_roll_duration_seconds < 0 {
            bail!("mid-roll values cannot be negative");
        }
        if !item.break_markers.is_empty() {
            if item.filler_collection.is_none() {
                bail!("break_markers need a filler_collection");
            }
            if item.mid_roll_duration_seconds <= 0 {
                bail!("break_markers need mid_roll_duration_seconds");
            }
        }
        let collection_id = resolve_collection(pool, &item.collection).await?;
        let filler_collection_id = match &item.filler_collection {
            Some(filler) => Some(resolve_collection(pool, filler).await?),
            None => None,
        };
        inputs.push(db::ClassicItemInput {
            collection_id,
            mode: item.mode.clone(),
            count: item.count,
            duration_minutes: item.duration_minutes,
            filler_collection_id,
            mid_roll_interval_minutes: item.mid_roll_interval_minutes,
            mid_roll_duration_seconds: item.mid_roll_duration_seconds,
            chapter_breaks: item.chapter_breaks,
            break_markers: item.break_markers.clone(),
            order: item.order.clone(),
        });
    }
    Ok(inputs)
}

async fn block_inputs(pool: &SqlitePool, blocks: &[ScriptedBlock]) -> Result<Vec<db::BlockInput>> {
    if blocks.is_empty() {
        bail!("blocks mode needs at least one block");
    }
    let mut inputs = Vec::new();
    for block in blocks {
        validate_order(&block.order)?;
        let days_mask = parse_days(&block.days)?;
        let start_minute = parse_time(&block.start)?;
        let end_minute = parse_time(&block.end)?;
        if start_minute >= end_minute {
            bail!("block start must be before block end");
        }
        if end_minute > 1440 {
            bail!("block end cannot be after 24:00");
        }
        let collection_id = resolve_collection(pool, &block.collection).await?;
        let filler_collection_id = match &block.filler_collection {
            Some(filler) => Some(resolve_collection(pool, filler).await?),
            None => None,
        };
        inputs.push(db::BlockInput {
            days_mask,
            start_minute,
            end_minute,
            collection_id,
            filler_collection_id,
            order: block.order.clone(),
        });
    }
    Ok(inputs)
}

pub async fn export(pool: &SqlitePool, channel_id: &str) -> Result<String> {
    let channel = db::channel_by_id(pool, channel_id)
        .await?
        .context("channel not found")?;
    let names: HashMap<String, String> = db::list_collections(pool)
        .await?
        .into_iter()
        .map(|collection| (collection.id, collection.name))
        .collect();
    let mut script = ScriptedSchedule {
        mode: Some(channel.schedule_mode.clone()),
        items: Vec::new(),
        blocks: Vec::new(),
    };
    if channel.schedule_mode == "classic" {
        for item in db::list_classic_items(pool, channel_id).await? {
            script.items.push(ScriptedItem {
                collection: names
                    .get(&item.collection_id)
                    .cloned()
                    .unwrap_or(item.collection_id.clone()),
                mode: item.mode,
                count: item.count,
                duration_minutes: item.duration_minutes,
                filler_collection: item
                    .filler_collection_id
                    .as_ref()
                    .map(|id| names.get(id).cloned().unwrap_or_else(|| id.clone())),
                mid_roll_interval_minutes: item.mid_roll_interval_minutes,
                mid_roll_duration_seconds: item.mid_roll_duration_seconds,
                chapter_breaks: item.chapter_breaks,
                break_markers: item.break_markers,
                order: item.order,
            });
        }
    } else {
        for block in db::list_channel_blocks(pool, channel_id).await? {
            script.blocks.push(ScriptedBlock {
                days: days_from_mask(block.days_mask),
                start: format_time(block.start_minute),
                end: format_time(block.end_minute),
                collection: names
                    .get(&block.collection_id)
                    .cloned()
                    .unwrap_or(block.collection_id.clone()),
                filler_collection: block
                    .filler_collection_id
                    .as_ref()
                    .map(|id| names.get(id).cloned().unwrap_or_else(|| id.clone())),
                order: block.order,
            });
        }
    }
    Ok(serde_yaml::to_string(&script)?)
}

async fn resolve_collection(pool: &SqlitePool, reference: &str) -> Result<String> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("collection name cannot be empty");
    }
    if db::collection_detail(pool, reference).await?.is_some() {
        return Ok(reference.to_string());
    }
    db::collection_id_by_name(pool, reference)
        .await?
        .with_context(|| format!("collection not found: {reference}"))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MarkerValue {
    Number(f64),
    Text(String),
}

fn deserialize_break_markers<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<MarkerValue>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| match value {
            MarkerValue::Number(value) => {
                if value < 0.0 {
                    Err(serde::de::Error::custom("break markers cannot be negative"))
                } else {
                    Ok(value.round() as i64)
                }
            }
            MarkerValue::Text(text) => parse_break_marker(&text).map_err(serde::de::Error::custom),
        })
        .collect()
}

fn parse_break_marker(text: &str) -> Result<i64> {
    let text = text.trim();
    let parts: Vec<&str> = text.split(':').collect();
    let seconds = match parts.as_slice() {
        [seconds] => seconds
            .parse::<i64>()
            .with_context(|| format!("invalid break marker: {text}"))?,
        [minutes, seconds] => {
            minutes
                .parse::<i64>()
                .with_context(|| format!("invalid break marker: {text}"))?
                * 60
                + seconds
                    .parse::<i64>()
                    .with_context(|| format!("invalid break marker: {text}"))?
        }
        [hours, minutes, seconds] => {
            hours
                .parse::<i64>()
                .with_context(|| format!("invalid break marker: {text}"))?
                * 3600
                + minutes
                    .parse::<i64>()
                    .with_context(|| format!("invalid break marker: {text}"))?
                    * 60
                + seconds
                    .parse::<i64>()
                    .with_context(|| format!("invalid break marker: {text}"))?
        }
        _ => bail!("invalid break marker: {text}"),
    };
    if seconds < 0 {
        bail!("break markers cannot be negative");
    }
    Ok(seconds)
}

fn validate_order(order: &str) -> Result<()> {
    if matches!(order, "sequential" | "shuffle") {
        Ok(())
    } else {
        bail!("order must be sequential or shuffle: {order}")
    }
}

fn parse_days(days: &[String]) -> Result<u32> {
    if days.is_empty() {
        bail!("block needs at least one day");
    }
    let mut mask = 0u32;
    for day in days {
        let key = day.trim().to_ascii_lowercase();
        if matches!(key.as_str(), "daily" | "everyday" | "every-day" | "all") {
            return Ok(127);
        }
        let bit = match key.as_str() {
            "mon" | "monday" => 0,
            "tue" | "tues" | "tuesday" => 1,
            "wed" | "wednesday" => 2,
            "thu" | "thur" | "thurs" | "thursday" => 3,
            "fri" | "friday" => 4,
            "sat" | "saturday" => 5,
            "sun" | "sunday" => 6,
            other => bail!("unknown day: {other}"),
        };
        mask |= 1 << bit;
    }
    Ok(mask)
}

fn days_from_mask(mask: u32) -> Vec<String> {
    let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let days: Vec<String> = names
        .iter()
        .enumerate()
        .filter(|(index, _)| mask & (1 << index) != 0)
        .map(|(_, name)| (*name).to_string())
        .collect();
    if days.len() == 7 {
        vec!["daily".into()]
    } else {
        days
    }
}

fn parse_time(text: &str) -> Result<u16> {
    let (hours, minutes) = text
        .split_once(':')
        .with_context(|| format!("time must be HH:MM: {text}"))?;
    let hours: u16 = hours
        .trim()
        .parse()
        .with_context(|| format!("invalid hour: {text}"))?;
    let minutes: u16 = minutes
        .trim()
        .parse()
        .with_context(|| format!("invalid minute: {text}"))?;
    if hours > 24 || minutes > 59 || (hours == 24 && minutes > 0) {
        bail!("invalid time: {text}");
    }
    Ok(hours * 60 + minutes)
}

fn format_time(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_classic_script() {
        let script: ScriptedSchedule = serde_yaml::from_str(
            r#"
mode: classic
items:
  - collection: Adventure Time
    mode: multiple
    count: 3
    order: shuffle
    filler_collection: Bumpers
    mid_roll_interval_minutes: 10
    mid_roll_duration_seconds: 20
"#,
        )
        .unwrap();
        assert_eq!(script.items.len(), 1);
        assert_eq!(script.items[0].collection, "Adventure Time");
        assert_eq!(script.items[0].count, 3);
        assert!(script.items[0].chapter_breaks);
    }

    #[test]
    fn parses_a_block_script() {
        let script: ScriptedSchedule = serde_yaml::from_str(
            r#"
blocks:
  - days: [mon, tue, wed, thu, fri]
    start: "18:00"
    end: "22:00"
    collection: Movie Night
    order: shuffle
"#,
        )
        .unwrap();
        assert_eq!(script.blocks.len(), 1);
        assert_eq!(parse_days(&script.blocks[0].days).unwrap(), 0b0011111);
        assert_eq!(parse_time(&script.blocks[0].start).unwrap(), 1080);
    }

    #[test]
    fn parses_manual_break_markers_as_numbers_or_timecodes() {
        let script: ScriptedSchedule = serde_yaml::from_str(
            r#"
items:
  - collection: Adventure Time
    break_markers: [600, "12:30", "1:02:03"]
"#,
        )
        .unwrap();
        assert_eq!(script.items[0].break_markers, vec![600, 750, 3723]);
    }

    #[test]
    fn rejects_negative_break_markers() {
        let result: Result<ScriptedSchedule, _> =
            serde_yaml::from_str("items:\n  - collection: x\n    break_markers: [-5]\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let result: Result<ScriptedSchedule, _> =
            serde_yaml::from_str("items:\n  - collection: x\n    mystery: true\n");
        assert!(result.is_err());
    }

    #[test]
    fn accepts_daily_and_24_hour_end() {
        assert_eq!(parse_days(&["daily".into()]).unwrap(), 127);
        assert_eq!(parse_time("24:00").unwrap(), 1440);
    }
}
