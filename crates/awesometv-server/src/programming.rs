use anyhow::{Context, Result, bail};
use awesometv_scheduler::rng::Rng;
use awesometv_scheduler::timeline::{ItemKind, SchedulerState, TimelineItem, build_range};
use awesometv_scheduler::{
    ContentItem, Daypart, FillMode, FillerItem, Order, Plan, Slot, Source, build_week,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Time, Weekday};

use crate::db;

#[derive(Debug, Serialize)]
pub struct RebuildReport {
    pub items: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub(crate) struct StagedClassic {
    pub items: Vec<db::TimelineWrite>,
    pub state: db::GeneratedTimelineState,
    pub start_ms: i64,
}

pub(crate) async fn stage_classic(
    pool: &SqlitePool,
    input: db::ClassicItemInput,
) -> Result<StagedClassic> {
    let definition = classic_definition_from_inputs(pool, vec![input]).await?;
    let start_ms = PrimitiveDateTime::new(OffsetDateTime::now_utc().date(), Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp()
        * 1000;
    let mut cursor = ClassicCursor::default();
    let parts = generate_classic(
        pool,
        &definition,
        &mut cursor,
        start_ms,
        start_ms + 7 * 86_400_000,
    )
    .await?;
    let state = db::GeneratedTimelineState {
        definition_version: definition.fingerprint,
        boundary_ms: parts.last().context("empty classic timeline")?.end_ms,
        continuation_json: serde_json::to_string(&cursor)?,
    };
    let items = parts
        .into_iter()
        .map(|p| db::TimelineWrite {
            media_id: p.media_id,
            title: p.title,
            start_ms: p.start_ms,
            end_ms: p.end_ms,
            in_ms: p.in_ms,
            out_ms: p.out_ms,
            kind: match p.kind {
                ItemKind::Program => "program",
                ItemKind::Filler => "filler",
            }
            .into(),
        })
        .collect();
    Ok(StagedClassic {
        items,
        state,
        start_ms,
    })
}

pub async fn rebuild_channel(pool: &SqlitePool, channel_id: &str) -> Result<RebuildReport> {
    let revision = db::rebuild_revision(pool).await?;
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
    let overrides = db::list_schedule_overrides(pool, channel_id).await?;
    if matches!(channel.schedule_mode.as_str(), "classic" | "blocks") && !overrides.is_empty() {
        let mut state = OverlayState::default();
        let parts = generate_overlay(
            pool,
            &channel,
            &overrides,
            &mut state,
            week_start,
            week_start + 7 * 86_400_000,
        )
        .await?;
        let saved = db::GeneratedTimelineState {
            definition_version: "overlay-v1".into(),
            boundary_ms: week_start + 7 * 86_400_000,
            continuation_json: serde_json::to_string(&state)?,
        };
        persist_timeline(pool, channel_id, &parts, week_start, Some(&saved), revision).await?;
        return Ok(RebuildReport {
            items: parts.len(),
            warnings: 0,
        });
    }
    if channel.schedule_mode == "classic"
        && db::list_schedule_overrides(pool, channel_id)
            .await?
            .is_empty()
    {
        let definition = classic_definition(pool, channel_id).await?;
        let mut cursor = ClassicCursor::default();
        let parts = generate_classic(
            pool,
            &definition,
            &mut cursor,
            week_start,
            week_start + 7 * 86_400_000,
        )
        .await?;
        let state = db::GeneratedTimelineState {
            definition_version: definition.fingerprint.clone(),
            boundary_ms: parts.last().context("empty classic timeline")?.end_ms,
            continuation_json: serde_json::to_string(&cursor)?,
        };
        persist_timeline(pool, channel_id, &parts, week_start, Some(&state), revision).await?;
        return Ok(RebuildReport {
            items: parts.len(),
            warnings: 0,
        });
    }
    if channel.schedule_mode == "blocks"
        && db::list_schedule_overrides(pool, channel_id)
            .await?
            .is_empty()
    {
        let (plan, fingerprint) = block_definition(pool, &channel).await?;
        let result = build_range(
            &plan,
            &SchedulerState::default(),
            week_start,
            week_start + 7 * 86_400_000,
        );
        let parts = block_result_parts(&result, week_start, week_start + 7 * 86_400_000)?;
        let state = db::GeneratedTimelineState {
            definition_version: fingerprint,
            boundary_ms: week_start + 7 * 86_400_000,
            continuation_json: serde_json::to_string(&result.state)?,
        };
        persist_timeline(pool, channel_id, &parts, week_start, Some(&state), revision).await?;
        return Ok(RebuildReport {
            items: parts.len(),
            warnings: result.warnings.len(),
        });
    }
    let (base_parts, warnings) = build_base_parts(pool, &channel, start_date).await?;
    let parts = apply_overrides(pool, &channel, base_parts, start_date, week_start).await?;
    if parts.is_empty() {
        bail!("the schedule produced no playout items");
    }
    persist_timeline(pool, &channel.id, &parts, week_start, None, revision).await?;
    Ok(RebuildReport {
        items: parts.len(),
        warnings,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ClassicCursor {
    item: usize,
    cycle: u64,
    index: usize,
    placed: usize,
    duration_end: Option<i64>,
    filler_index: usize,
}

struct ClassicDefinition {
    inputs: Vec<db::ClassicItemInput>,
    collections: Vec<(Vec<db::Media>, Vec<db::Media>)>,
    fingerprint: String,
    chapters: std::collections::BTreeMap<String, Vec<i64>>,
}

async fn classic_definition(pool: &SqlitePool, channel_id: &str) -> Result<ClassicDefinition> {
    let inputs: Vec<_> = db::list_classic_items(pool, channel_id)
        .await?
        .iter()
        .map(classic_input)
        .collect();
    classic_definition_from_inputs(pool, inputs).await
}

async fn classic_definition_from_inputs(
    pool: &SqlitePool,
    inputs: Vec<db::ClassicItemInput>,
) -> Result<ClassicDefinition> {
    if inputs.is_empty() {
        bail!("classic sequence is empty");
    }
    let mut collections = Vec::new();
    for item in &inputs {
        let media = db::collection_detail(pool, &item.collection_id)
            .await?
            .context("classic collection not found")?
            .items;
        if !media.iter().any(|m| m.duration_ms > 0) {
            bail!("classic collection has no playable media");
        }
        let filler = match &item.filler_collection_id {
            Some(id) if !id.is_empty() => db::collection_detail(pool, id)
                .await?
                .map(|c| c.items)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        collections.push((media, filler));
    }
    let chapters = db::chapter_starts(pool).await?;
    let ids: std::collections::HashSet<_> = collections
        .iter()
        .flat_map(|(media, _)| media.iter().map(|m| m.id.as_str()))
        .collect();
    let chapters: std::collections::BTreeMap<_, _> = chapters
        .into_iter()
        .filter(|(id, _)| ids.contains(id.as_str()))
        .collect();
    let bytes = serde_json::to_vec(&(&inputs, &collections, &chapters))?;
    let hash = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let fingerprint = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    Ok(ClassicDefinition {
        inputs,
        collections,
        fingerprint,
        chapters,
    })
}

async fn generate_classic(
    pool: &SqlitePool,
    def: &ClassicDefinition,
    state: &mut ClassicCursor,
    start: i64,
    target: i64,
) -> Result<Vec<PlayoutPart>> {
    let _ = pool;
    let chapters = &def.chapters;
    if state.item >= def.inputs.len() {
        bail!("invalid classic continuation");
    }
    let mut out = Vec::new();
    let mut at = start;
    let mut idle = 0usize;
    while at < target {
        let item = &def.inputs[state.item];
        let (media, filler) = &def.collections[state.item];
        let deadline = *state.duration_end.get_or_insert_with(|| {
            at.saturating_add(item.duration_minutes.max(1).saturating_mul(60_000))
        });
        let done = (item.mode == "one" && state.placed >= 1)
            || (item.mode == "multiple" && state.placed >= item.count.max(1) as usize)
            || (item.mode == "duration" && at >= deadline);
        if done {
            state.item += 1;
            if state.item == def.inputs.len() {
                state.item = 0;
                state.cycle = state.cycle.wrapping_add(1);
            }
            state.index = 0;
            state.placed = 0;
            state.duration_end = None;
            state.filler_index = 0;
            continue;
        }
        let mut candidates = media.clone();
        if item.order == "shuffle" {
            Rng::new(
                0xC1A551C_u64
                    .wrapping_add(state.cycle)
                    .wrapping_add(state.item as u64 * 0x100000),
            )
            .shuffle(&mut candidates);
        }
        let candidate = &candidates[state.index % candidates.len()];
        state.index += 1;
        if candidate.duration_ms <= 0 {
            idle += 1;
            if idle > media.len() * 2 {
                bail!("classic sequence cannot advance");
            }
            continue;
        }
        let mut filler_index = state.filler_index;
        let parts = build_program_parts(
            candidate,
            at,
            item.mid_roll_interval_minutes.max(0) * 60_000,
            item.mid_roll_duration_seconds.max(0) * 1_000,
            filler,
            &mut filler_index,
            if item.chapter_breaks {
                chapters.get(&candidate.id).map(Vec::as_slice)
            } else {
                None
            },
            &item.break_markers,
        );
        let end = parts
            .last()
            .context("classic media produced no parts")?
            .end_ms;
        if item.mode == "duration" && end > deadline {
            // The existing duration mode skips media that does not fit.
            state.item += 1;
            if state.item == def.inputs.len() {
                state.item = 0;
                state.cycle = state.cycle.wrapping_add(1);
            }
            state.index = 0;
            state.placed = 0;
            state.duration_end = None;
            state.filler_index = 0;
            idle += 1;
            if idle > def.inputs.len() {
                bail!("classic sequence cannot advance");
            }
            continue;
        }
        idle = 0;
        state.filler_index = filler_index;
        state.placed += 1;
        at = end;
        out.extend(parts);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewalOutcome {
    SkippedUnowned,
    UpToDate,
    Appended,
    UnsupportedMode,
}

pub async fn renew_channel_at(
    pool: &SqlitePool,
    channel_id: &str,
    now: OffsetDateTime,
) -> Result<RenewalOutcome> {
    let revision = db::renewal_input_revision(pool).await?;
    let channel = db::channel_by_id(pool, channel_id)
        .await?
        .context("channel not found")?;
    if !matches!(channel.schedule_mode.as_str(), "classic" | "blocks") {
        return Ok(RenewalOutcome::UnsupportedMode);
    }
    let target = i64::try_from(now.unix_timestamp_nanos() / 1_000_000)?
        .checked_add(7 * 86_400_000)
        .context("renewal horizon overflow")?;
    let overrides = db::list_schedule_overrides(pool, channel_id).await?;
    let existing = db::generated_timeline_state(pool, channel_id).await?;
    if !overrides.is_empty()
        || existing
            .as_ref()
            .is_some_and(|s| s.definition_version == "overlay-v1")
    {
        return renew_overlay(pool, &channel, target, revision).await;
    }
    if channel.schedule_mode == "blocks" {
        return renew_blocks(pool, &channel, target, revision).await;
    }
    loop {
        let Some(old) = db::generated_timeline_state(pool, channel_id).await? else {
            return Ok(RenewalOutcome::SkippedUnowned);
        };
        let definition = classic_definition(pool, channel_id).await?;
        let mut cursor: ClassicCursor = match serde_json::from_str(&old.continuation_json) {
            Ok(c) => c,
            Err(_) => return Ok(RenewalOutcome::SkippedUnowned),
        };
        if old.boundary_ms >= target {
            return Ok(RenewalOutcome::UpToDate);
        }
        if old.definition_version != definition.fingerprint {
            cursor = ClassicCursor::default();
        }
        let parts =
            generate_classic(pool, &definition, &mut cursor, old.boundary_ms, target).await?;
        let next = db::GeneratedTimelineState {
            definition_version: definition.fingerprint,
            boundary_ms: parts.last().context("empty classic append")?.end_ms,
            continuation_json: serde_json::to_string(&cursor)?,
        };
        let writes: Vec<_> = parts
            .iter()
            .map(|p| db::TimelineWrite {
                media_id: p.media_id.clone(),
                title: p.title.clone(),
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                in_ms: p.in_ms,
                out_ms: p.out_ms,
                kind: match p.kind {
                    ItemKind::Program => "program",
                    ItemKind::Filler => "filler",
                }
                .into(),
            })
            .collect();
        match db::compare_append_generated_timeline_guarded(
            pool,
            channel_id,
            &old,
            &writes,
            &next,
            None,
            Some(revision),
        )
        .await
        {
            Ok(()) => return Ok(RenewalOutcome::Appended),
            Err(error) => {
                if db::generated_timeline_state(pool, channel_id)
                    .await?
                    .as_ref()
                    != Some(&old)
                {
                    continue;
                }
                return Err(error);
            }
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct OverlayState {
    generators: std::collections::BTreeMap<String, OverlayCursor>,
}

#[derive(Serialize, Deserialize)]
struct OverlayCursor {
    fingerprint: String,
    classic: Option<ClassicCursor>,
    block: Option<SchedulerState>,
    pending: Vec<PlayoutPart>,
}

enum OverlayDefinition {
    Classic(ClassicDefinition),
    Blocks(Plan, String),
}

impl OverlayDefinition {
    fn fingerprint(&self) -> &str {
        match self {
            Self::Classic(d) => &d.fingerprint,
            Self::Blocks(_, f) => f,
        }
    }
}

async fn overlay_parts(
    pool: &SqlitePool,
    def: &OverlayDefinition,
    states: &mut OverlayState,
    key: &str,
    start: i64,
    end: i64,
) -> Result<Vec<PlayoutPart>> {
    let state = states
        .generators
        .entry(key.to_string())
        .or_insert_with(|| OverlayCursor {
            fingerprint: String::new(),
            classic: None,
            block: None,
            pending: Vec::new(),
        });
    if state.fingerprint != def.fingerprint() {
        *state = OverlayCursor {
            fingerprint: def.fingerprint().into(),
            classic: None,
            block: None,
            pending: Vec::new(),
        };
    }
    match def {
        OverlayDefinition::Blocks(plan, _) => {
            let result = build_range(
                plan,
                state.block.get_or_insert_with(SchedulerState::default),
                start,
                end,
            );
            let parts = block_result_parts(&result, start, end)?;
            state.block = Some(result.state);
            Ok(parts)
        }
        OverlayDefinition::Classic(def) => {
            let mut parts = std::mem::take(&mut state.pending);
            let at = parts.last().map_or(start, |p| p.end_ms);
            if at < end {
                parts.extend(
                    generate_classic(
                        pool,
                        def,
                        state.classic.get_or_insert_with(ClassicCursor::default),
                        at,
                        end,
                    )
                    .await?,
                );
            }
            let mut out = Vec::new();
            for mut part in parts {
                if part.start_ms >= end {
                    state.pending.push(part);
                    continue;
                }
                if part.end_ms > end {
                    let mut tail = part.clone();
                    tail.in_ms += end - part.start_ms;
                    tail.start_ms = end;
                    part.out_ms = tail.in_ms;
                    part.end_ms = end;
                    state.pending.push(tail);
                }
                out.push(part);
            }
            Ok(out)
        }
    }
}

async fn generate_overlay(
    pool: &SqlitePool,
    channel: &db::Channel,
    overrides: &[db::ScheduleOverride],
    state: &mut OverlayState,
    start: i64,
    end: i64,
) -> Result<Vec<PlayoutPart>> {
    let base = if channel.schedule_mode == "classic" {
        OverlayDefinition::Classic(classic_definition(pool, &channel.id).await?)
    } else {
        let (p, f) = block_definition(pool, channel).await?;
        OverlayDefinition::Blocks(p, f)
    };
    let start_date = OffsetDateTime::from_unix_timestamp_nanos(start as i128 * 1_000_000)?.date();
    let mut defs = Vec::new();
    for row in overrides {
        let first = db::parse_date(&row.start_date)?;
        let last = db::parse_date(&row.end_date)?;
        if last < start_date {
            continue;
        }
        let parsed = crate::scripted::parse_definition(pool, &row.schedule_yaml)
            .await
            .with_context(|| format!("override '{}'", row.name))?;
        let def = match parsed.definition {
            db::ScheduleDefinition::Classic(items) => {
                OverlayDefinition::Classic(classic_definition_from_inputs(pool, items).await?)
            }
            db::ScheduleDefinition::Blocks(blocks) => {
                let plan = make_block_plan(pool, channel, &blocks).await?;
                let hash = ring::digest::digest(
                    &ring::digest::SHA256,
                    &serde_json::to_vec(&(&blocks, &plan))?,
                );
                OverlayDefinition::Blocks(
                    plan,
                    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect(),
                )
            }
        };
        defs.push((row, first, last, def));
    }
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        let next = ((at.div_euclid(86_400_000) + 1) * 86_400_000).min(end);
        let date = OffsetDateTime::from_unix_timestamp_nanos(at as i128 * 1_000_000)?.date();
        let mut selected = overlay_parts(pool, &base, state, "base", at, next).await?;
        let winner = defs
            .iter()
            .filter(|(_, first, last, _)| *first <= date && date <= *last)
            .max_by(|a, b| {
                a.0.priority
                    .cmp(&b.0.priority)
                    .then_with(|| b.0.id.cmp(&a.0.id))
            })
            .map(|x| x.0.id.as_str());
        for (row, _, _, def) in &defs {
            let parts =
                overlay_parts(pool, def, state, &format!("override:{}", row.id), at, next).await?;
            if winner == Some(row.id.as_str()) {
                selected = parts;
            }
        }
        let mut cursor = at;
        for p in &selected {
            if p.start_ms != cursor || p.end_ms <= p.start_ms {
                bail!("override overlay has a gap or overlap at {cursor}");
            }
            cursor = p.end_ms;
        }
        if cursor != next {
            bail!("override overlay has a gap at {cursor}");
        }
        out.extend(selected);
        at = next;
    }
    Ok(out)
}

async fn renew_overlay(
    pool: &SqlitePool,
    channel: &db::Channel,
    target: i64,
    revision: i64,
) -> Result<RenewalOutcome> {
    loop {
        let Some(old) = db::generated_timeline_state(pool, &channel.id).await? else {
            return Ok(RenewalOutcome::SkippedUnowned);
        };
        if old.boundary_ms >= target {
            return Ok(RenewalOutcome::UpToDate);
        }
        let mut state = if old.definition_version == "overlay-v1" {
            match serde_json::from_str(&old.continuation_json) {
                Ok(s) => s,
                Err(_) => return Ok(RenewalOutcome::SkippedUnowned),
            }
        } else {
            let mut s = OverlayState::default();
            let cursor = if channel.schedule_mode == "classic" {
                let Ok(c) = serde_json::from_str::<ClassicCursor>(&old.continuation_json) else {
                    return Ok(RenewalOutcome::SkippedUnowned);
                };
                OverlayCursor {
                    fingerprint: old.definition_version.clone(),
                    classic: Some(c),
                    block: None,
                    pending: Vec::new(),
                }
            } else {
                let Ok(c) = serde_json::from_str::<SchedulerState>(&old.continuation_json) else {
                    return Ok(RenewalOutcome::SkippedUnowned);
                };
                OverlayCursor {
                    fingerprint: old.definition_version.clone(),
                    classic: None,
                    block: Some(c),
                    pending: Vec::new(),
                }
            };
            s.generators.insert("base".into(), cursor);
            s
        };
        let end = target
            .div_euclid(86_400_000)
            .checked_add(i64::from(target.rem_euclid(86_400_000) != 0))
            .and_then(|d| d.checked_mul(86_400_000))
            .context("renewal horizon overflow")?;
        let overrides = db::list_schedule_overrides(pool, &channel.id).await?;
        let parts =
            generate_overlay(pool, channel, &overrides, &mut state, old.boundary_ms, end).await?;
        let writes: Vec<_> = parts
            .iter()
            .map(|p| db::TimelineWrite {
                media_id: p.media_id.clone(),
                title: p.title.clone(),
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                in_ms: p.in_ms,
                out_ms: p.out_ms,
                kind: match p.kind {
                    ItemKind::Program => "program",
                    ItemKind::Filler => "filler",
                }
                .into(),
            })
            .collect();
        let next = db::GeneratedTimelineState {
            definition_version: "overlay-v1".into(),
            boundary_ms: end,
            continuation_json: serde_json::to_string(&state)?,
        };
        match db::compare_append_generated_timeline_guarded(
            pool,
            &channel.id,
            &old,
            &writes,
            &next,
            Some(&overrides),
            Some(revision),
        )
        .await
        {
            Ok(()) => return Ok(RenewalOutcome::Appended),
            Err(e) => {
                if db::generated_timeline_state(pool, &channel.id)
                    .await?
                    .as_ref()
                    == Some(&old)
                {
                    return Err(e);
                }
            }
        }
    }
}

async fn build_base_parts(
    pool: &SqlitePool,
    channel: &db::Channel,
    start_date: Date,
) -> Result<(Vec<PlayoutPart>, usize)> {
    let classic = db::list_classic_items(pool, &channel.id).await?;
    if channel.schedule_mode == "classic" && !classic.is_empty() {
        let inputs: Vec<db::ClassicItemInput> = classic.iter().map(classic_input).collect();
        return Ok((
            build_classic_parts(pool, channel, &inputs, start_date).await?,
            0,
        ));
    }
    let blocks = db::list_channel_blocks(pool, &channel.id).await?;
    if blocks.is_empty() {
        bail!("add at least one programming block first");
    }
    let inputs: Vec<db::BlockInput> = blocks.iter().map(block_input).collect();
    build_block_parts(pool, channel, &inputs, start_date).await
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
    start_date: Date,
) -> Result<(Vec<PlayoutPart>, usize)> {
    let plan = make_block_plan(pool, channel, blocks).await?;
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

async fn make_block_plan(
    pool: &SqlitePool,
    channel: &db::Channel,
    blocks: &[db::BlockInput],
) -> Result<Plan> {
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

    Ok(Plan {
        name: channel.name.clone(),
        seed: 0xA11CE,
        utc_offset_minutes: 0,
        dayparts,
        fallback,
    })
}

async fn block_definition(pool: &SqlitePool, channel: &db::Channel) -> Result<(Plan, String)> {
    let blocks: Vec<_> = db::list_channel_blocks(pool, &channel.id)
        .await?
        .iter()
        .map(block_input)
        .collect();
    if blocks.is_empty() {
        bail!("add at least one programming block first");
    }
    let plan = make_block_plan(pool, channel, &blocks).await?;
    let bytes = serde_json::to_vec(&(&blocks, &plan))?;
    let hash = ring::digest::digest(&ring::digest::SHA256, &bytes);
    Ok((
        plan,
        hash.as_ref().iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

fn block_result_parts(
    result: &awesometv_scheduler::BuildResult,
    start: i64,
    end: i64,
) -> Result<Vec<PlayoutPart>> {
    let parts: Vec<_> = result
        .items
        .iter()
        .map(PlayoutPart::from_timeline)
        .collect();
    let mut boundary = start;
    for part in &parts {
        if part.start_ms != boundary || part.end_ms <= part.start_ms {
            bail!("block schedule has a gap or overlap at {boundary}");
        }
        boundary = part.end_ms;
    }
    if boundary != end {
        bail!("block schedule has a gap at {boundary}");
    }
    Ok(parts)
}

async fn renew_blocks(
    pool: &SqlitePool,
    channel: &db::Channel,
    target: i64,
    revision: i64,
) -> Result<RenewalOutcome> {
    loop {
        let Some(old) = db::generated_timeline_state(pool, &channel.id).await? else {
            return Ok(RenewalOutcome::SkippedUnowned);
        };
        let (plan, fingerprint) = block_definition(pool, channel).await?;
        let mut state: SchedulerState = match serde_json::from_str(&old.continuation_json) {
            Ok(state) => state,
            Err(_) => return Ok(RenewalOutcome::SkippedUnowned),
        };
        if old.boundary_ms >= target {
            return Ok(RenewalOutcome::UpToDate);
        }
        if old.definition_version != fingerprint {
            state = SchedulerState::default();
        }
        let end = target
            .div_euclid(86_400_000)
            .checked_add(i64::from(target.rem_euclid(86_400_000) != 0))
            .and_then(|d| d.checked_mul(86_400_000))
            .context("renewal horizon overflow")?;
        let result = build_range(&plan, &state, old.boundary_ms, end);
        let parts = block_result_parts(&result, old.boundary_ms, end)?;
        let next = db::GeneratedTimelineState {
            definition_version: fingerprint,
            boundary_ms: end,
            continuation_json: serde_json::to_string(&result.state)?,
        };
        let writes: Vec<_> = parts
            .iter()
            .map(|p| db::TimelineWrite {
                media_id: p.media_id.clone(),
                title: p.title.clone(),
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                in_ms: p.in_ms,
                out_ms: p.out_ms,
                kind: match p.kind {
                    ItemKind::Program => "program",
                    ItemKind::Filler => "filler",
                }
                .into(),
            })
            .collect();
        match db::compare_append_generated_timeline_guarded(
            pool,
            &channel.id,
            &old,
            &writes,
            &next,
            None,
            Some(revision),
        )
        .await
        {
            Ok(()) => return Ok(RenewalOutcome::Appended),
            Err(error) => {
                if let Some(current) = db::generated_timeline_state(pool, &channel.id).await? {
                    if current.boundary_ms != old.boundary_ms
                        || current.definition_version != old.definition_version
                        || current.continuation_json != old.continuation_json
                    {
                        continue;
                    }
                }
                return Err(error);
            }
        }
    }
}

async fn build_classic_parts(
    pool: &SqlitePool,
    _channel: &db::Channel,
    items: &[db::ClassicItemInput],
    start_date: Date,
) -> Result<Vec<PlayoutPart>> {
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
    id: String,
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
                build_classic_parts(pool, channel, &items, start_date).await?
            }
            db::ScheduleDefinition::Blocks(blocks) => {
                build_block_parts(pool, channel, &blocks, start_date)
                    .await?
                    .0
            }
        };
        built.push(OverrideParts {
            start,
            end,
            priority: row.priority,
            id: row.id,
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
        let chosen = select_override(&built, date);
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

fn select_override(built: &[OverrideParts], date: Date) -> Option<&OverrideParts> {
    built
        .iter()
        .filter(|item| item.start <= date && date <= item.end)
        .max_by(|a, b| a.priority.cmp(&b.priority).then_with(|| b.id.cmp(&a.id)))
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
    state: Option<&db::GeneratedTimelineState>,
    revision: (i64, i64),
) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Acquire the write lock before checking the snapshot. All timeline and
    // definition mutations advance this revision in their own transaction.
    sqlx::query("UPDATE renewal_input_revision SET revision = revision WHERE id = 1")
        .execute(&mut *tx)
        .await?;
    let actual: (i64, i64) =
        sqlx::query_as("SELECT (SELECT revision FROM renewal_input_revision WHERE id = 1), revision FROM rebuild_timeline_revision WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    if actual != revision {
        bail!("rebuild inputs or timeline changed");
    }
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
    // Each successful rebuild establishes a new definition epoch. Cursors are
    // deliberately unavailable until the renewal generator can serialize them.
    let boundary = items
        .iter()
        .map(|item| item.end_ms)
        .max()
        .context("empty timeline")?;
    let version = uuid::Uuid::new_v4().to_string();
    let (version, boundary, continuation) = state
        .map(|s| {
            (
                s.definition_version.as_str(),
                s.boundary_ms,
                s.continuation_json.as_str(),
            )
        })
        .unwrap_or((&version, boundary, r#"{"schema":1,"cursors":null}"#));
    sqlx::query("INSERT INTO generated_timeline_state (channel_id, definition_version, boundary_ms, continuation_json) VALUES (?, ?, ?, ?) ON CONFLICT(channel_id) DO UPDATE SET definition_version=excluded.definition_version, boundary_ms=excluded.boundary_ms, continuation_json=excluded.continuation_json")
        .bind(channel_id).bind(version).bind(boundary).bind(continuation)
        .execute(&mut *tx).await?;
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

    #[tokio::test]
    async fn stale_rebuild_rejects_replacement_and_newer_rebuild() {
        let dir = std::env::temp_dir().join(format!("rebuild-race-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES ('m','/m','media',3600000,0)").execute(&pool).await.unwrap();
        let channel = db::create_channel(&pool, "1", "test", &["m".into()])
            .await
            .unwrap();
        let part = PlayoutPart {
            title: "generated".into(),
            start_ms: 0,
            end_ms: 100,
            media_id: "m".into(),
            in_ms: 0,
            out_ms: 100,
            kind: ItemKind::Program,
        };
        let stale = db::rebuild_revision(&pool).await.unwrap();
        let external = db::TimelineWrite {
            title: "external".into(),
            start_ms: 0,
            end_ms: 200,
            media_id: "m".into(),
            in_ms: 0,
            out_ms: 200,
            kind: "program".into(),
        };
        db::replace_channel_timeline(&pool, &channel.id, &[external], 0)
            .await
            .unwrap();
        assert!(
            persist_timeline(&pool, &channel.id, &[part.clone()], 0, None, stale)
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT title FROM channel_items WHERE channel_id = ?")
                .bind(&channel.id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            "external"
        );
        assert!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap()
                .is_none()
        );
        let snapshot = db::rebuild_revision(&pool).await.unwrap();
        persist_timeline(&pool, &channel.id, &[part.clone()], 0, None, snapshot)
            .await
            .unwrap();
        let saved = db::generated_timeline_state(&pool, &channel.id)
            .await
            .unwrap();
        assert!(
            persist_timeline(&pool, &channel.id, &[part.clone()], 0, None, snapshot)
                .await
                .is_err()
        );
        assert_eq!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap(),
            saved
        );
        let before_renewal = db::rebuild_revision(&pool).await.unwrap();
        let old = saved.unwrap();
        let next = db::GeneratedTimelineState {
            boundary_ms: 200,
            ..old.clone()
        };
        let appended = db::TimelineWrite {
            title: "appended".into(),
            start_ms: 100,
            end_ms: 200,
            media_id: "m".into(),
            in_ms: 0,
            out_ms: 100,
            kind: "program".into(),
        };
        db::compare_append_generated_timeline(&pool, &channel.id, &old, &[appended], &next)
            .await
            .unwrap();
        assert!(
            persist_timeline(&pool, &channel.id, &[part], 0, None, before_renewal)
                .await
                .is_err()
        );
        assert_eq!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap(),
            Some(next)
        );
        assert_eq!(
            db::all_channel_items(&pool, &channel.id)
                .await
                .unwrap()
                .len(),
            2
        );
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn override_renewal_dates_priority_restart_rebase_and_concurrency() {
        for base_mode in ["classic", "blocks"] {
            let dir = std::env::temp_dir().join(format!("overlay-{}", uuid::Uuid::new_v4()));
            let pool = db::init(&dir).await.unwrap();
            for id in ["base", "a", "b", "c"] {
                sqlx::query("INSERT INTO collections (id,name,created_at) VALUES (?,?,0)")
                    .bind(id)
                    .bind(id)
                    .execute(&pool)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES (?,?,?,?,0)").bind(id).bind(format!("/{id}")).bind(id).bind(if id == "base" || id == "a" { 3_600_000_i64 } else { 25_200_000 }).execute(&pool).await.unwrap();
                sqlx::query(
                    "INSERT INTO collection_items (collection_id,media_id,position) VALUES (?,?,0)",
                )
                .bind(id)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            }
            let channel = db::create_channel(&pool, "1", "test", &["base".into()])
                .await
                .unwrap();
            if base_mode == "classic" {
                let item = db::ClassicItemInput {
                    collection_id: "base".into(),
                    mode: "one".into(),
                    count: 1,
                    duration_minutes: 60,
                    filler_collection_id: None,
                    mid_roll_interval_minutes: 0,
                    mid_roll_duration_seconds: 0,
                    chapter_breaks: false,
                    break_markers: vec![],
                    order: "sequential".into(),
                };
                db::set_classic_items(&pool, &channel.id, &[item])
                    .await
                    .unwrap();
            } else {
                let block = db::BlockInput {
                    days_mask: 127,
                    start_minute: 0,
                    end_minute: 1440,
                    collection_id: "base".into(),
                    filler_collection_id: None,
                    order: "sequential".into(),
                };
                db::set_channel_blocks(&pool, &channel.id, &[block])
                    .await
                    .unwrap();
            }
            let channel = db::channel_by_id(&pool, &channel.id)
                .await
                .unwrap()
                .unwrap();
            let classic = |id: &str| format!("mode: classic\nitems:\n  - collection: {id}\n");
            let blocks = |id: &str| {
                format!(
                    "mode: blocks\nblocks:\n  - start: '00:00'\n    end: '24:00'\n    collection: {id}\n"
                )
            };
            // UTC dates span the US spring DST transition; days remain exactly 24 hours.
            for (id, start, end, priority, yaml) in [
                ("expired", "2027-03-01", "2027-03-12", 99, classic("c")),
                ("low", "2027-03-14", "2027-03-14", 4, classic("c")),
                ("z", "2027-03-14", "2027-03-14", 5, classic("b")),
                ("a", "2027-03-14", "2027-03-14", 5, blocks("a")),
                ("future", "2027-03-15", "2027-03-16", 9, classic("b")),
            ] {
                sqlx::query("INSERT INTO schedule_overrides (id,channel_id,name,start_date,end_date,priority,schedule_yaml,created_at) VALUES (?,?,?,?,?,?,?,0)").bind(id).bind(&channel.id).bind(id).bind(start).bind(end).bind(priority).bind(yaml).execute(&pool).await.unwrap();
            }
            rebuild_channel(&pool, &channel.id).await.unwrap();
            let rebuilt = db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(rebuilt.definition_version, "overlay-v1");
            let start = PrimitiveDateTime::new(
                Date::from_calendar_date(2027, time::Month::March, 13).unwrap(),
                Time::MIDNIGHT,
            )
            .assume_utc()
            .unix_timestamp()
                * 1000;
            let mut state = OverlayState::default();
            let overrides = db::list_schedule_overrides(&pool, &channel.id)
                .await
                .unwrap();
            let parts = generate_overlay(
                &pool,
                &channel,
                &overrides,
                &mut state,
                start,
                start + 86_400_000,
            )
            .await
            .unwrap();
            let saved = db::GeneratedTimelineState {
                definition_version: "overlay-v1".into(),
                boundary_ms: start + 86_400_000,
                continuation_json: serde_json::to_string(&state).unwrap(),
            };
            persist_timeline(
                &pool,
                &channel.id,
                &parts,
                start,
                Some(&saved),
                db::rebuild_revision(&pool).await.unwrap(),
            )
            .await
            .unwrap();
            let original = db::all_channel_items(&pool, &channel.id).await.unwrap();
            // A concurrent override edit invalidates the snapshot under the append lock.
            sqlx::query("UPDATE schedule_overrides SET priority=10 WHERE id='future'")
                .execute(&pool)
                .await
                .unwrap();
            let stale_item = db::TimelineWrite {
                media_id: "base".into(),
                title: "base".into(),
                start_ms: saved.boundary_ms,
                end_ms: saved.boundary_ms + 1,
                in_ms: 0,
                out_ms: 1,
                kind: "program".into(),
            };
            let stale_next = db::GeneratedTimelineState {
                definition_version: saved.definition_version.clone(),
                boundary_ms: saved.boundary_ms + 1,
                continuation_json: saved.continuation_json.clone(),
            };
            assert!(
                db::compare_append_generated_timeline_with_overrides(
                    &pool,
                    &channel.id,
                    &saved,
                    &[stale_item],
                    &stale_next,
                    Some(&overrides)
                )
                .await
                .is_err()
            );
            assert_eq!(
                db::generated_timeline_state(&pool, &channel.id)
                    .await
                    .unwrap(),
                Some(saved)
            );
            pool.close().await;
            let pool = db::init(&dir).await.unwrap();
            let now = OffsetDateTime::from_unix_timestamp(start / 1000 - 4 * 86400).unwrap();
            let (a, b) = tokio::join!(
                renew_channel_at(&pool, &channel.id, now),
                renew_channel_at(&pool, &channel.id, now)
            );
            let outcomes = [a.unwrap(), b.unwrap()];
            assert!(outcomes.contains(&RenewalOutcome::Appended));
            assert!(outcomes.contains(&RenewalOutcome::UpToDate));
            let rows = db::all_channel_items(&pool, &channel.id).await.unwrap();
            let key =
                |r: &db::ChannelItem| (r.media_id.clone(), r.start_ms, r.end_ms, r.in_ms, r.out_ms);
            assert_eq!(
                original.iter().map(key).collect::<Vec<_>>(),
                rows[..original.len()].iter().map(key).collect::<Vec<_>>()
            );
            assert!(rows.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
            for row in &rows {
                let day = (row.start_ms - start).div_euclid(86_400_000);
                assert_eq!(
                    row.media_id,
                    match day {
                        1 => "a",
                        2 | 3 => "b",
                        _ => "base",
                    }
                );
            }
            assert!(rows.iter().any(|r| r.in_ms > 0));
            assert_eq!(
                renew_channel_at(&pool, &channel.id, now).await.unwrap(),
                RenewalOutcome::UpToDate
            );
            // Change the still-active definition and extend its date: rebase exactly at the published end.
            sqlx::query("UPDATE schedule_overrides SET end_date='2027-03-20', schedule_yaml=? WHERE id='future'").bind(classic("c")).execute(&pool).await.unwrap();
            let before = rows.len();
            renew_channel_at(&pool, &channel.id, now + Duration::days(1))
                .await
                .unwrap();
            let after = db::all_channel_items(&pool, &channel.id).await.unwrap();
            assert_eq!(
                rows.iter().map(key).collect::<Vec<_>>(),
                after[..before].iter().map(key).collect::<Vec<_>>()
            );
            assert_eq!(after[before].start_ms, rows.last().unwrap().end_ms);
            assert_eq!(after[before].media_id, "c");
            pool.close().await;
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn block_renewal_restart_shuffle_and_rebase() {
        let dir =
            std::env::temp_dir().join(format!("awesometv-block-renewal-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        sqlx::query("INSERT INTO collections (id,name,created_at) VALUES ('col','Collection',0)")
            .execute(&pool)
            .await
            .unwrap();
        for (pos, id) in ["a", "b", "c"].iter().enumerate() {
            sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES (?,?,?,3600000,0)").bind(id).bind(format!("/{id}")).bind(id).execute(&pool).await.unwrap();
            sqlx::query(
                "INSERT INTO collection_items (collection_id,media_id,position) VALUES ('col',?,?)",
            )
            .bind(id)
            .bind(pos as i64)
            .execute(&pool)
            .await
            .unwrap();
        }
        let channel = db::create_channel(&pool, "9", "Blocks", &["a".into()])
            .await
            .unwrap();
        let mut block = db::BlockInput {
            days_mask: 127,
            start_minute: 0,
            end_minute: 1440,
            collection_id: "col".into(),
            filler_collection_id: None,
            order: "shuffle".into(),
        };
        db::set_channel_blocks(&pool, &channel.id, &[block.clone()])
            .await
            .unwrap();
        rebuild_channel(&pool, &channel.id).await.unwrap();
        let original = db::all_channel_items(&pool, &channel.id).await.unwrap();
        let saved = db::generated_timeline_state(&pool, &channel.id)
            .await
            .unwrap()
            .unwrap();
        let (plan, _) = block_definition(&pool, &channel).await.unwrap();
        let state: SchedulerState = serde_json::from_str(&saved.continuation_json).unwrap();
        assert!(state.rng_state.is_some());
        let partial_start = saved.boundary_ms + 3_600_000;
        let partial_end = saved.boundary_ms + 86_400_000;
        let partial = build_range(&plan, &state, partial_start, partial_end);
        assert!(block_result_parts(&partial, partial_start, partial_end).is_ok());
        let expected = build_range(
            &plan,
            &state,
            saved.boundary_ms,
            saved.boundary_ms + 2 * 86_400_000,
        );
        let now =
            OffsetDateTime::from_unix_timestamp(saved.boundary_ms / 1000 - 5 * 86400).unwrap();
        pool.close().await;
        let pool = db::init(&dir).await.unwrap();
        assert_eq!(
            renew_channel_at(&pool, &channel.id, now).await.unwrap(),
            RenewalOutcome::Appended
        );
        let rows = db::all_channel_items(&pool, &channel.id).await.unwrap();
        let key =
            |r: &db::ChannelItem| (r.media_id.clone(), r.start_ms, r.end_ms, r.in_ms, r.out_ms);
        assert_eq!(
            original.iter().map(key).collect::<Vec<_>>(),
            rows[..original.len()].iter().map(key).collect::<Vec<_>>()
        );
        assert_eq!(
            rows[original.len()..]
                .iter()
                .map(|r| (&r.media_id, r.start_ms, r.end_ms))
                .collect::<Vec<_>>(),
            expected
                .items
                .iter()
                .map(|r| (&r.source_id, r.start_ms, r.end_ms))
                .collect::<Vec<_>>()
        );
        assert!(rows.last().unwrap().end_ms >= now.unix_timestamp() * 1000 + 7 * 86_400_000);
        assert!(rows.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        block.order = "sequential".into();
        db::set_channel_blocks(&pool, &channel.id, &[block])
            .await
            .unwrap();
        let (changed, _) = block_definition(&pool, &channel).await.unwrap();
        let boundary = rows.last().unwrap().end_ms;
        let expected_rebase = build_range(
            &changed,
            &SchedulerState::default(),
            boundary,
            boundary + 86_400_000,
        );
        assert_eq!(
            renew_channel_at(&pool, &channel.id, now + Duration::days(1))
                .await
                .unwrap(),
            RenewalOutcome::Appended
        );
        let after = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert_eq!(
            rows.iter().map(key).collect::<Vec<_>>(),
            after[..rows.len()].iter().map(key).collect::<Vec<_>>()
        );
        assert_eq!(
            after[rows.len()..]
                .iter()
                .map(|r| (&r.media_id, r.start_ms, r.end_ms))
                .collect::<Vec<_>>(),
            expected_rebase
                .items
                .iter()
                .map(|r| (&r.source_id, r.start_ms, r.end_ms))
                .collect::<Vec<_>>()
        );
        assert!(after.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn classic_renewal_horizon_restart_rebase_and_concurrency() {
        let dir = std::env::temp_dir().join(format!("classic-renew-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        for (id, pos) in [("a", 0), ("b", 1), ("c", 2)] {
            sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES (?,?,?,25200000,0)").bind(id).bind(format!("/{id}")).bind(id).execute(&pool).await.unwrap();
            if pos == 0 {
                sqlx::query(
                    "INSERT INTO collections (id,name,created_at) VALUES ('col','Collection',0)",
                )
                .execute(&pool)
                .await
                .unwrap();
            }
            sqlx::query(
                "INSERT INTO collection_items (collection_id,media_id,position) VALUES ('col',?,?)",
            )
            .bind(id)
            .bind(pos)
            .execute(&pool)
            .await
            .unwrap();
        }
        let channel = db::create_channel(&pool, "9", "Classic", &["a".into()])
            .await
            .unwrap();
        let item = db::ClassicItemInput {
            collection_id: "col".into(),
            mode: "multiple".into(),
            count: 5,
            duration_minutes: 60,
            filler_collection_id: None,
            mid_roll_interval_minutes: 0,
            mid_roll_duration_seconds: 0,
            chapter_breaks: false,
            break_markers: vec![],
            order: "sequential".into(),
        };
        db::set_classic_items(&pool, &channel.id, &[item.clone()])
            .await
            .unwrap();
        rebuild_channel(&pool, &channel.id).await.unwrap();
        let original = db::all_channel_items(&pool, &channel.id).await.unwrap();
        let boundary = original.last().unwrap().end_ms;
        let now = OffsetDateTime::from_unix_timestamp(boundary / 1000 - 6 * 86400).unwrap();
        assert_eq!(
            renew_channel_at(&pool, &channel.id, now).await.unwrap(),
            RenewalOutcome::Appended
        );
        let rows = db::all_channel_items(&pool, &channel.id).await.unwrap();
        let key =
            |r: &db::ChannelItem| (r.media_id.clone(), r.start_ms, r.end_ms, r.in_ms, r.out_ms);
        assert_eq!(
            original.iter().map(key).collect::<Vec<_>>(),
            rows[..original.len()].iter().map(key).collect::<Vec<_>>()
        );
        assert!(rows.last().unwrap().end_ms >= now.unix_timestamp() * 1000 + 7 * 86400000);
        assert!(rows.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        assert!(
            rows.iter()
                .enumerate()
                .all(|(i, r)| r.media_id == ["a", "b", "c", "a", "b"][i % 5])
        );
        pool.close().await;
        let pool = db::init(&dir).await.unwrap();
        assert_eq!(
            renew_channel_at(&pool, &channel.id, now).await.unwrap(),
            RenewalOutcome::UpToDate
        );
        let now2 = now + Duration::days(1);
        let (left, right) = tokio::join!(
            renew_channel_at(&pool, &channel.id, now2),
            renew_channel_at(&pool, &channel.id, now2)
        );
        let outcomes = [left.unwrap(), right.unwrap()];
        assert!(outcomes.contains(&RenewalOutcome::Appended));
        assert!(outcomes.contains(&RenewalOutcome::UpToDate));
        let before = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert!(before.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        assert!(
            before
                .iter()
                .enumerate()
                .all(|(i, r)| r.media_id == ["a", "b", "c", "a", "b"][i % 5])
        );
        let mut changed = item;
        changed.count = 1;
        db::set_classic_items(&pool, &channel.id, &[changed])
            .await
            .unwrap();
        let now3 = now2 + Duration::days(1);
        assert_eq!(
            renew_channel_at(&pool, &channel.id, now3).await.unwrap(),
            RenewalOutcome::Appended
        );
        let after = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert_eq!(
            before.iter().map(key).collect::<Vec<_>>(),
            after[..before.len()].iter().map(key).collect::<Vec<_>>()
        );
        assert!(after[before.len()..].iter().all(|r| r.media_id == "a"));
        assert_eq!(after[before.len()].start_ms, before.last().unwrap().end_ms);
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn generated_ownership_and_atomic_append() {
        let dir = std::env::temp_dir().join(format!("awesometv-renewal-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES ('m','/m','M',3600000,0)").execute(&pool).await.unwrap();
        let channel = db::create_channel(&pool, "1", "Manual", &["m".into()])
            .await
            .unwrap();
        assert_eq!(channel.schedule_mode, "blocks");
        assert!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap()
                .is_none()
        );
        let unowned = db::GeneratedTimelineState {
            definition_version: "unknown".into(),
            boundary_ms: 0,
            continuation_json: "{}".into(),
        };
        let unowned_next = db::GeneratedTimelineState {
            boundary_ms: 1,
            ..unowned.clone()
        };
        let unowned_item = db::TimelineWrite {
            media_id: "m".into(),
            title: "M".into(),
            start_ms: 0,
            end_ms: 1,
            in_ms: 0,
            out_ms: 1,
            kind: "program".into(),
        };
        assert!(
            db::compare_append_generated_timeline(
                &pool,
                &channel.id,
                &unowned,
                &[unowned_item],
                &unowned_next
            )
            .await
            .is_err()
        );
        sqlx::query("INSERT INTO collections (id,name,created_at) VALUES ('c','C',0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO collection_items (collection_id,media_id,position) VALUES ('c','m',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO channel_blocks (id,channel_id,days_mask,start_minute,end_minute,collection_id,play_order,position) VALUES ('b',?,127,0,1440,'c','sequential',0)").bind(&channel.id).execute(&pool).await.unwrap();
        rebuild_channel(&pool, &channel.id).await.unwrap();
        let state = db::generated_timeline_state(&pool, &channel.id)
            .await
            .unwrap()
            .unwrap();
        let rows = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert_eq!(
            state.boundary_ms,
            rows.iter().map(|r| r.end_ms).max().unwrap()
        );
        assert!(serde_json::from_str::<SchedulerState>(&state.continuation_json).is_ok());
        sqlx::query("CREATE TRIGGER fail_rebuild BEFORE UPDATE ON generated_timeline_state BEGIN SELECT RAISE(ABORT, 'test rebuild failure'); END").execute(&pool).await.unwrap();
        assert!(rebuild_channel(&pool, &channel.id).await.is_err());
        assert_eq!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap(),
            Some(state.clone())
        );
        let after_failed_rebuild = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert_eq!(after_failed_rebuild.len(), rows.len());
        assert_eq!(
            after_failed_rebuild
                .iter()
                .map(|r| (r.start_ms, r.end_ms, &r.media_id))
                .collect::<Vec<_>>(),
            rows.iter()
                .map(|r| (r.start_ms, r.end_ms, &r.media_id))
                .collect::<Vec<_>>()
        );
        sqlx::query("DROP TRIGGER fail_rebuild")
            .execute(&pool)
            .await
            .unwrap();
        let item = db::TimelineWrite {
            media_id: "m".into(),
            title: "M".into(),
            start_ms: state.boundary_ms,
            end_ms: state.boundary_ms + 1000,
            in_ms: 0,
            out_ms: 1000,
            kind: "program".into(),
        };
        let next = db::GeneratedTimelineState {
            definition_version: state.definition_version.clone(),
            boundary_ms: item.end_ms,
            continuation_json: r#"{"cursor":1}"#.into(),
        };
        // Edits to either a base definition or its collection during generation
        // must invalidate the append, even if the edit is later reverted.
        for (change, revert) in [
            (
                "UPDATE channel_blocks SET play_order='shuffle' WHERE id='b'",
                "UPDATE channel_blocks SET play_order='sequential' WHERE id='b'",
            ),
            (
                "DELETE FROM collection_items WHERE collection_id='c'",
                "INSERT INTO collection_items (collection_id,media_id,position) VALUES ('c','m',0)",
            ),
        ] {
            let revision = db::renewal_input_revision(&pool).await.unwrap();
            sqlx::query(change).execute(&pool).await.unwrap();
            sqlx::query(revert).execute(&pool).await.unwrap();
            let error = db::compare_append_generated_timeline_guarded(
                &pool,
                &channel.id,
                &state,
                &[item.clone()],
                &next,
                None,
                Some(revision),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("renewal inputs changed"));
            assert_eq!(
                db::generated_timeline_state(&pool, &channel.id)
                    .await
                    .unwrap(),
                Some(state.clone())
            );
            assert_eq!(
                db::all_channel_items(&pool, &channel.id)
                    .await
                    .unwrap()
                    .len(),
                rows.len()
            );
        }
        sqlx::query("CREATE TRIGGER fail_state BEFORE UPDATE ON generated_timeline_state WHEN NEW.boundary_ms != OLD.boundary_ms BEGIN SELECT RAISE(ABORT, 'test failure'); END").execute(&pool).await.unwrap();
        assert!(
            db::compare_append_generated_timeline(
                &pool,
                &channel.id,
                &state,
                &[item.clone()],
                &next
            )
            .await
            .is_err()
        );
        assert_eq!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap(),
            Some(state.clone())
        );
        assert_eq!(
            db::all_channel_items(&pool, &channel.id)
                .await
                .unwrap()
                .len(),
            rows.len()
        );
        sqlx::query("DROP TRIGGER fail_state")
            .execute(&pool)
            .await
            .unwrap();
        db::compare_append_generated_timeline(&pool, &channel.id, &state, &[item.clone()], &next)
            .await
            .unwrap();
        assert!(
            db::compare_append_generated_timeline(&pool, &channel.id, &state, &[item], &next)
                .await
                .is_err()
        );
        assert_eq!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap(),
            Some(next)
        );
        assert_eq!(
            db::all_channel_items(&pool, &channel.id)
                .await
                .unwrap()
                .len(),
            rows.len() + 1
        );
        db::replace_channel_timeline(&pool, &channel.id, &[], 0)
            .await
            .unwrap();
        assert!(
            db::generated_timeline_state(&pool, &channel.id)
                .await
                .unwrap()
                .is_none()
        );
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_unowned_timeline_survives_migration_and_renewal() {
        let dir = std::env::temp_dir().join(format!("awesometv-legacy-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES ('m','/m','M',3600000,0)").execute(&pool).await.unwrap();
        let channel = db::create_channel(&pool, "1", "Legacy", &["m".into()])
            .await
            .unwrap();
        let before = db::all_channel_items(&pool, &channel.id).await.unwrap();
        sqlx::query("DROP TABLE generated_timeline_state")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = db::init(&dir).await.unwrap();
        assert_eq!(
            renew_channel_at(&pool, &channel.id, OffsetDateTime::now_utc())
                .await
                .unwrap(),
            RenewalOutcome::SkippedUnowned
        );
        let after = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert_eq!(
            before
                .iter()
                .map(|r| (&r.media_id, r.start_ms, r.end_ms))
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|r| (&r.media_id, r.start_ms, r.end_ms))
                .collect::<Vec<_>>()
        );
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overrides_choose_priority_then_stable_id_independent_of_order() {
        let date = Date::from_calendar_date(2026, time::Month::November, 18).unwrap();
        let make = |id: &str, priority| OverrideParts {
            start: date,
            end: date,
            priority,
            id: id.into(),
            parts: Vec::new(),
        };
        let mut choices = vec![make("z", 2), make("a", 2), make("low", 1)];
        assert_eq!(select_override(&choices, date).unwrap().id, "a");
        choices.reverse();
        assert_eq!(select_override(&choices, date).unwrap().id, "a");
        assert!(select_override(&choices, date + Duration::days(1)).is_none());
    }

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
