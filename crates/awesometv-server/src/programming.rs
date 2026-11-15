use anyhow::{Context, Result, bail};
use awesometv_scheduler::rng::Rng;
use awesometv_scheduler::timeline::{ItemKind, TimelineItem};
use awesometv_scheduler::{
    ContentItem, Daypart, FillMode, FillerItem, Order, Plan, Slot, Source, build_week,
};
use serde::Serialize;
use sqlx::SqlitePool;
use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Time, Weekday};

use crate::db;

#[derive(Debug, Serialize)]
pub struct RebuildReport {
    pub items: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone)]
struct PlayoutPart {
    title: String,
    start_ms: i64,
    end_ms: i64,
    media_id: String,
    in_ms: i64,
    out_ms: i64,
    kind: ItemKind,
}

impl PlayoutPart {
    fn from_timeline(item: &TimelineItem) -> Self {
        Self {
            title: item.title.clone(),
            start_ms: item.start_ms,
            end_ms: item.end_ms,
            media_id: item.source_id.clone(),
            in_ms: 0,
            out_ms: (item.end_ms - item.start_ms).max(0),
            kind: item.kind.clone(),
        }
    }
}

pub async fn rebuild_channel(pool: &SqlitePool, channel_id: &str) -> Result<RebuildReport> {
    let channel = db::channel_by_id(pool, channel_id)
        .await?
        .context("channel not found")?;
    if channel.schedule_mode == "external" {
        let items = db::count_channel_items(pool, channel_id).await?;
        if items == 0 {
            bail!("external channel has no timeline; import playout JSON first");
        }
        return Ok(RebuildReport { items, warnings: 0 });
    }
    let start_date: Date = OffsetDateTime::now_utc().date();
    let week_start = PrimitiveDateTime::new(start_date, Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp()
        * 1000;
    let (base_parts, warnings) = build_base_parts(pool, &channel).await?;
    let parts = apply_overrides(pool, &channel, base_parts, start_date, week_start).await?;
    if parts.is_empty() {
        bail!("the schedule produced no playout items");
    }
    persist_timeline(pool, &channel.id, &parts, week_start).await?;
    Ok(RebuildReport {
        items: parts.len(),
        warnings,
    })
}

async fn build_base_parts(
    pool: &SqlitePool,
    channel: &db::Channel,
) -> Result<(Vec<PlayoutPart>, usize)> {
    let classic = db::list_classic_items(pool, &channel.id).await?;
    if channel.schedule_mode == "classic" && !classic.is_empty() {
        let inputs: Vec<db::ClassicItemInput> = classic.iter().map(classic_input).collect();
        return Ok((build_classic_parts(pool, channel, &inputs).await?, 0));
    }
    let blocks = db::list_channel_blocks(pool, &channel.id).await?;
    if blocks.is_empty() {
        bail!("add at least one programming block first");
    }
    let inputs: Vec<db::BlockInput> = blocks.iter().map(block_input).collect();
    build_block_parts(pool, channel, &inputs).await
}

fn classic_input(item: &db::ClassicItem) -> db::ClassicItemInput {
    db::ClassicItemInput {
        collection_id: item.collection_id.clone(),
        mode: item.mode.clone(),
        count: item.count,
        duration_minutes: item.duration_minutes,
        filler_collection_id: item.filler_collection_id.clone(),
        mid_roll_interval_minutes: item.mid_roll_interval_minutes,
        mid_roll_duration_seconds: item.mid_roll_duration_seconds,
        chapter_breaks: item.chapter_breaks,
        break_markers: item.break_markers.clone(),
        order: item.order.clone(),
    }
}

fn block_input(block: &db::ChannelBlock) -> db::BlockInput {
    db::BlockInput {
        days_mask: block.days_mask,
        start_minute: block.start_minute,
        end_minute: block.end_minute,
        collection_id: block.collection_id.clone(),
        filler_collection_id: block.filler_collection_id.clone(),
        order: block.order.clone(),
    }
}

async fn build_block_parts(
    pool: &SqlitePool,
    channel: &db::Channel,
    blocks: &[db::BlockInput],
) -> Result<(Vec<PlayoutPart>, usize)> {
    let mut dayparts = Vec::new();
    let mut fallback_media = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let detail = db::collection_detail(pool, &block.collection_id)
            .await?
            .with_context(|| format!("collection not found: {}", block.collection_id))?;
        if detail.items.is_empty() {
            bail!("collection '{}' is empty", detail.name);
        }
        let source_items: Vec<ContentItem> = detail
            .items
            .iter()
            .map(|media| ContentItem {
                id: media.id.clone(),
                title: media.title.clone(),
                duration_ms: media.duration_ms.max(0) as u64,
                series: None,
            })
            .collect();
        fallback_media.extend(detail.items.clone());
        let filler = if let Some(filler_id) = &block.filler_collection_id {
            if filler_id.is_empty() {
                Vec::new()
            } else {
                db::collection_detail(pool, filler_id)
                    .await?
                    .map(|detail| {
                        detail
                            .items
                            .iter()
                            .map(|media| FillerItem {
                                id: media.id.clone(),
                                title: media.title.clone(),
                                duration_ms: media.duration_ms.max(0) as u64,
                                weight: 10,
                                cooldown_ms: 0,
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            }
        } else {
            Vec::new()
        };
        let duration_ms = (block.end_minute as u64 - block.start_minute as u64) * 60_000;
        dayparts.push(Daypart {
            name: detail.name.clone(),
            days: mask_to_weekdays(block.days_mask),
            start_minute: block.start_minute,
            end_minute: block.end_minute,
            priority: index as i32,
            slots: vec![Slot {
                name: format!("{}-{}", channel.id, index),
                source: Source {
                    items: source_items,
                },
                order: if block.order == "shuffle" {
                    Order::Shuffle
                } else {
                    Order::Sequential
                },
                mode: FillMode::Duration { duration_ms },
                filler,
                epg_title: Some(detail.name.clone()),
            }],
        });
    }

    fallback_media.sort_by_key(|media| media.id.clone());
    fallback_media.dedup_by_key(|media| media.id.clone());
    let fallback = fallback_media
        .iter()
        .map(|media| FillerItem {
            id: media.id.clone(),
            title: media.title.clone(),
            duration_ms: media.duration_ms.max(0) as u64,
            weight: 10,
            cooldown_ms: 0,
        })
        .collect::<Vec<_>>();

    let plan = Plan {
        name: channel.name.clone(),
        seed: 0xA11CE,
        utc_offset_minutes: 0,
        dayparts,
        fallback,
    };
    let start_date: Date = OffsetDateTime::now_utc().date();
    let result = build_week(&plan, &Default::default(), start_date);
    if result.items.is_empty() {
        bail!("the programming blocks produced no playout items");
    }
    Ok((
        result
            .items
            .iter()
            .map(PlayoutPart::from_timeline)
            .collect(),
        result.warnings.len(),
    ))
}

async fn build_classic_parts(
    pool: &SqlitePool,
    _channel: &db::Channel,
    items: &[db::ClassicItemInput],
) -> Result<Vec<PlayoutPart>> {
    let start_date: Date = OffsetDateTime::now_utc().date();
    let week_start = PrimitiveDateTime::new(start_date, Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp()
        * 1000;
    let week_end = week_start + 7 * 86_400_000;
    let mut cursor = week_start;
    let mut out: Vec<PlayoutPart> = Vec::new();
    let mut rng = Rng::new(0xC1A551C);
    let mut guard = 0usize;
    let chapter_map = db::chapter_starts(pool).await?;

    while cursor < week_end && guard < 100_000 {
        guard += 1;
        let cycle_start = cursor;
        for item in items {
            if cursor >= week_end {
                break;
            }
            let detail = db::collection_detail(pool, &item.collection_id)
                .await?
                .with_context(|| format!("collection not found: {}", item.collection_id))?;
            if detail.items.is_empty() {
                bail!("collection '{}' is empty", detail.name);
            }
            let filler = match &item.filler_collection_id {
                Some(filler_id) if !filler_id.is_empty() => db::collection_detail(pool, filler_id)
                    .await?
                    .map(|detail| detail.items)
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let interval_ms = item.mid_roll_interval_minutes.max(0) * 60_000;
            let break_ms = item.mid_roll_duration_seconds.max(0) * 1_000;
            let breaks_enabled = (interval_ms > 0 || !item.break_markers.is_empty())
                && break_ms > 0
                && !filler.is_empty();
            let mut filler_index = 0usize;

            let mut candidates = detail.items.clone();
            if item.order == "shuffle" {
                rng.shuffle(&mut candidates);
            }
            let count = item.count.max(1) as usize;
            let duration_target = cursor + item.duration_minutes.max(1) * 60_000;
            let mut index = 0usize;
            let mut placed = 0usize;
            loop {
                if cursor >= week_end {
                    break;
                }
                if item.mode == "one" && placed >= 1 {
                    break;
                }
                if item.mode == "multiple" && placed >= count {
                    break;
                }
                if item.mode == "duration" && cursor >= duration_target {
                    break;
                }
                let media = &candidates[index % candidates.len()];
                index += 1;
                let duration = media.duration_ms.max(0);
                if duration == 0 {
                    if index > candidates.len() * 2 {
                        break;
                    }
                    continue;
                }
                let chapter_points = if item.chapter_breaks {
                    chapter_map.get(&media.id).map(Vec::as_slice)
                } else {
                    None
                };
                let parts = if breaks_enabled {
                    build_program_parts(
                        media,
                        cursor,
                        interval_ms,
                        break_ms,
                        &filler,
                        &mut filler_index,
                        chapter_points,
                        &item.break_markers,
                    )
                } else {
                    vec![PlayoutPart {
                        title: media.title.clone(),
                        start_ms: cursor,
                        end_ms: cursor + duration,
                        media_id: media.id.clone(),
                        in_ms: 0,
                        out_ms: duration,
                        kind: ItemKind::Program,
                    }]
                };
                let total: i64 = parts.iter().map(|part| part.end_ms - part.start_ms).sum();
                if cursor + total > week_end {
                    cursor = week_end;
                    break;
                }
                if item.mode == "duration" && cursor + total > duration_target {
                    break;
                }
                out.extend(parts);
                cursor += total;
                placed += 1;
            }
        }
        if cursor == cycle_start {
            break;
        }
    }

    if out.is_empty() {
        bail!("the classic sequence produced no playout items");
    }
    Ok(out)
}

struct OverrideParts {
    start: Date,
    end: Date,
    priority: i64,
    parts: Vec<PlayoutPart>,
}

async fn apply_overrides(
    pool: &SqlitePool,
    channel: &db::Channel,
    base: Vec<PlayoutPart>,
    start_date: Date,
    week_start: i64,
) -> Result<Vec<PlayoutPart>> {
    let overrides = db::list_schedule_overrides(pool, &channel.id).await?;
    if overrides.is_empty() {
        return Ok(base);
    }
    let week_end_date = start_date + Duration::days(6);
    let mut built = Vec::new();
    for row in overrides {
        let start = db::parse_date(&row.start_date)?;
        let end = db::parse_date(&row.end_date)?;
        if end < start_date || start > week_end_date {
            continue;
        }
        let parsed = crate::scripted::parse_definition(pool, &row.schedule_yaml)
            .await
            .with_context(|| format!("override '{}'", row.name))?;
        let parts = match parsed.definition {
            db::ScheduleDefinition::Classic(items) => {
                build_classic_parts(pool, channel, &items).await?
            }
            db::ScheduleDefinition::Blocks(blocks) => {
                build_block_parts(pool, channel, &blocks).await?.0
            }
        };
        built.push(OverrideParts {
            start,
            end,
            priority: row.priority,
            parts,
        });
    }
    if built.is_empty() {
        return Ok(base);
    }

    let mut out = Vec::new();
    for day in 0..7 {
        let date = start_date + Duration::days(day);
        let day_start = week_start + day * 86_400_000;
        let day_end = day_start + 86_400_000;
        let chosen = built
            .iter()
            .filter(|item| item.start <= date && date <= item.end)
            .max_by_key(|item| item.priority);
        let source = chosen.map(|item| &item.parts).unwrap_or(&base);
        out.extend(
            source
                .iter()
                .filter(|part| part.start_ms >= day_start && part.start_ms < day_end)
                .cloned(),
        );
    }
    out.sort_by_key(|part| (part.start_ms, part.end_ms));
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn build_program_parts(
    media: &db::Media,
    start_ms: i64,
    interval_ms: i64,
    break_ms: i64,
    filler: &[db::Media],
    filler_index: &mut usize,
    chapter_starts: Option<&[i64]>,
    manual_markers: &[i64],
) -> Vec<PlayoutPart> {
    let duration = media.duration_ms.max(0);
    let mut parts = Vec::new();
    if duration <= 0 {
        return parts;
    }
    if break_ms <= 0 || filler.is_empty() || (interval_ms <= 0 && manual_markers.is_empty()) {
        parts.push(PlayoutPart {
            title: media.title.clone(),
            start_ms,
            end_ms: start_ms + duration,
            media_id: media.id.clone(),
            in_ms: 0,
            out_ms: duration,
            kind: ItemKind::Program,
        });
        return parts;
    }

    let break_points = if manual_markers.is_empty() {
        break_offsets(duration, interval_ms, chapter_starts)
    } else {
        manual_break_offsets(duration, manual_markers)
    };
    if break_points.is_empty() {
        parts.push(PlayoutPart {
            title: media.title.clone(),
            start_ms,
            end_ms: start_ms + duration,
            media_id: media.id.clone(),
            in_ms: 0,
            out_ms: duration,
            kind: ItemKind::Program,
        });
        return parts;
    }

    let mut cursor = start_ms;
    let mut offset = 0i64;
    for point in break_points {
        let segment_ms = point - offset;
        if segment_ms > 0 {
            parts.push(PlayoutPart {
                title: media.title.clone(),
                start_ms: cursor,
                end_ms: cursor + segment_ms,
                media_id: media.id.clone(),
                in_ms: offset,
                out_ms: point,
                kind: ItemKind::Program,
            });
            cursor += segment_ms;
            offset = point;
        }
        let mut remaining = break_ms;
        let mut guard = 0usize;
        while remaining > 0 && !filler.is_empty() && guard < filler.len() * 4 + 8 {
            guard += 1;
            let candidate = &filler[*filler_index % filler.len()];
            *filler_index += 1;
            let candidate_ms = candidate.duration_ms.max(0);
            if candidate_ms <= 0 {
                continue;
            }
            let take = candidate_ms.min(remaining);
            parts.push(PlayoutPart {
                title: candidate.title.clone(),
                start_ms: cursor,
                end_ms: cursor + take,
                media_id: candidate.id.clone(),
                in_ms: 0,
                out_ms: take,
                kind: ItemKind::Filler,
            });
            cursor += take;
            remaining -= take;
        }
    }
    if offset < duration {
        parts.push(PlayoutPart {
            title: media.title.clone(),
            start_ms: cursor,
            end_ms: cursor + (duration - offset),
            media_id: media.id.clone(),
            in_ms: offset,
            out_ms: duration,
            kind: ItemKind::Program,
        });
    }
    parts
}

fn break_offsets(duration_ms: i64, interval_ms: i64, chapters: Option<&[i64]>) -> Vec<i64> {
    if duration_ms <= 0 || interval_ms <= 0 {
        return Vec::new();
    }
    let tolerance = (interval_ms / 3).max(30_000);
    let min_spacing = (interval_ms / 2).max(30_000);
    let mut points: Vec<i64> = Vec::new();
    let mut target = interval_ms;
    while target < duration_ms {
        let snapped = chapters.and_then(|chapters| {
            chapters
                .iter()
                .copied()
                .filter(|chapter| *chapter >= 30_000 && *chapter <= duration_ms - 1_000)
                .filter(|chapter| {
                    points
                        .last()
                        .is_none_or(|last| *chapter >= *last + min_spacing)
                })
                .min_by_key(|chapter| (*chapter - target).abs())
                .filter(|chapter| (*chapter - target).abs() <= tolerance)
        });
        let point = snapped.unwrap_or(target);
        if point > 0
            && point < duration_ms
            && points
                .last()
                .is_none_or(|last| point >= *last + min_spacing)
        {
            points.push(point);
        }
        target = point + interval_ms;
    }
    points
}

fn manual_break_offsets(duration_ms: i64, markers_seconds: &[i64]) -> Vec<i64> {
    let mut points: Vec<i64> = markers_seconds
        .iter()
        .map(|seconds| seconds * 1_000)
        .filter(|point| *point > 0 && *point < duration_ms)
        .collect();
    points.sort_unstable();
    points.dedup();
    points
}

async fn persist_timeline(
    pool: &SqlitePool,
    channel_id: &str,
    items: &[PlayoutPart],
    week_start: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM channel_items WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    for (position, item) in items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO channel_items (channel_id, position, media_id, title, start_ms, end_ms, in_ms, out_ms, kind) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(channel_id)
        .bind(position as i64)
        .bind(&item.media_id)
        .bind(&item.title)
        .bind(item.start_ms)
        .bind(item.end_ms)
        .bind(item.in_ms)
        .bind(item.out_ms)
        .bind(match item.kind {
            ItemKind::Program => "program",
            ItemKind::Filler => "filler",
        })
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE channels SET start_ms = ? WHERE id = ?")
        .bind(week_start)
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn mask_to_weekdays(mask: u32) -> Vec<Weekday> {
    let mut days = Vec::new();
    for (bit, weekday) in [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ]
    .into_iter()
    .enumerate()
    {
        if mask & (1 << bit) != 0 {
            days.push(weekday);
        }
    }
    days
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(id: &str, duration_ms: i64) -> db::Media {
        db::Media {
            id: id.into(),
            path: format!("/media/{id}.mkv"),
            title: format!("Media {id}"),
            kind: "episode".into(),
            duration_ms,
            width: None,
            height: None,
            video_codec: None,
            audio_codec: None,
            size_bytes: None,
            chapters: Vec::new(),
        }
    }

    #[test]
    fn mid_rolls_split_programs_and_insert_filler() {
        let program = media("program", 60 * 60_000);
        let filler = vec![media("f1", 15_000), media("f2", 15_000)];
        let mut index = 0;
        let parts = build_program_parts(
            &program,
            1_000_000,
            20 * 60_000,
            30_000,
            &filler,
            &mut index,
            None,
            &[],
        );
        let programs = parts
            .iter()
            .filter(|part| part.kind == ItemKind::Program)
            .count();
        let breaks = parts
            .iter()
            .filter(|part| part.kind == ItemKind::Filler)
            .count();
        assert_eq!(programs, 3, "60 minutes at a 20 minute interval");
        assert_eq!(breaks, 4, "two breaks of two 15 second fillers");
        assert!(
            parts
                .windows(2)
                .all(|pair| pair[0].end_ms == pair[1].start_ms)
        );
        assert_eq!(
            parts.last().unwrap().end_ms,
            1_000_000 + 60 * 60_000 + 2 * 30_000
        );
    }

    #[test]
    fn mid_roll_filler_is_trimmed_to_the_break_length() {
        let program = media("program", 30 * 60_000);
        let filler = vec![media("long", 5 * 60_000)];
        let mut index = 0;
        let parts = build_program_parts(
            &program,
            0,
            10 * 60_000,
            30_000,
            &filler,
            &mut index,
            None,
            &[],
        );
        let break_parts: Vec<&PlayoutPart> = parts
            .iter()
            .filter(|part| part.kind == ItemKind::Filler)
            .collect();
        assert_eq!(break_parts.len(), 2);
        for part in break_parts {
            assert_eq!(part.out_ms, 30_000);
            assert_eq!(part.end_ms - part.start_ms, 30_000);
        }
    }

    #[test]
    fn break_offsets_snap_to_chapter_starts() {
        let chapters = [0, 17 * 60_000, 34 * 60_000, 51 * 60_000];
        let points = break_offsets(70 * 60_000, 20 * 60_000, Some(&chapters));
        assert_eq!(
            points,
            vec![17 * 60_000, 34 * 60_000, 51 * 60_000],
            "breaks should land on chapter boundaries near each interval"
        );
    }

    #[test]
    fn break_offsets_fall_back_to_fixed_intervals() {
        let points = break_offsets(60 * 60_000, 20 * 60_000, None);
        assert_eq!(points, vec![20 * 60_000, 40 * 60_000]);
    }

    #[test]
    fn chapter_breaks_segment_the_program_at_chapters() {
        let program = media("program", 70 * 60_000);
        let filler = vec![media("f1", 15_000)];
        let chapters = [0, 17 * 60_000, 34 * 60_000, 51 * 60_000];
        let mut index = 0;
        let parts = build_program_parts(
            &program,
            0,
            20 * 60_000,
            30_000,
            &filler,
            &mut index,
            Some(&chapters),
            &[],
        );
        let program_parts: Vec<&PlayoutPart> = parts
            .iter()
            .filter(|part| part.kind == ItemKind::Program)
            .collect();
        assert_eq!(program_parts.len(), 4);
        assert_eq!(program_parts[0].out_ms, 17 * 60_000);
        assert_eq!(program_parts[1].in_ms, 17 * 60_000);
        assert_eq!(program_parts[1].out_ms, 34 * 60_000);
        assert_eq!(program_parts[3].out_ms, 70 * 60_000);
    }

    #[test]
    fn manual_break_markers_split_at_exact_points() {
        let program = media("program", 60 * 60_000);
        let filler = vec![media("f1", 15_000)];
        let mut index = 0;
        let parts = build_program_parts(
            &program,
            0,
            0,
            30_000,
            &filler,
            &mut index,
            None,
            &[1_200, 600, 600, 9_999],
        );
        let program_parts: Vec<&PlayoutPart> = parts
            .iter()
            .filter(|part| part.kind == ItemKind::Program)
            .collect();
        assert_eq!(program_parts.len(), 3);
        assert_eq!(program_parts[0].out_ms, 600_000);
        assert_eq!(program_parts[1].in_ms, 600_000);
        assert_eq!(program_parts[1].out_ms, 1_200_000);
        assert_eq!(program_parts[2].out_ms, 60 * 60_000);
    }

    #[test]
    fn no_filler_means_no_breaks() {
        let program = media("program", 60 * 60_000);
        let mut index = 0;
        let parts =
            build_program_parts(&program, 0, 20 * 60_000, 30_000, &[], &mut index, None, &[]);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].in_ms, 0);
        assert_eq!(parts[0].out_ms, 60 * 60_000);
    }
}
