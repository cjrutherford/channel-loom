use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::{Date, Duration, PrimitiveDateTime, Time, UtcOffset, Weekday};

use crate::plan::{ContentItem, Daypart, FillMode, FillerItem, Order, Plan, Slot};
use crate::rng::Rng;

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchedulerState {
    pub cursors: BTreeMap<String, CursorState>,
    pub last_played_ms: BTreeMap<String, i64>,
    #[serde(default)]
    pub rng_state: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CursorState {
    pub position: usize,
    pub bag: Vec<usize>,
    pub bag_pos: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Program,
    Filler,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineItem {
    pub id: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub source_id: String,
    pub kind: ItemKind,
    pub epg_title: Option<String>,
}

impl TimelineItem {
    pub fn duration_ms(&self) -> u64 {
        (self.end_ms - self.start_ms).max(0) as u64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub at_ms: i64,
    pub daypart: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub items: Vec<TimelineItem>,
    pub state: SchedulerState,
    pub warnings: Vec<Warning>,
    pub coverage_ms: u64,
    pub scheduled_ms: u64,
}

impl BuildResult {
    pub fn coverage_fraction(&self) -> f64 {
        if self.scheduled_ms == 0 {
            0.0
        } else {
            self.coverage_ms as f64 / self.scheduled_ms as f64
        }
    }
}

/// Build one week starting at `start_date` in the plan's fixed offset.
///
/// This is deliberately pure: callers inject the start date and state.
pub fn build_week(plan: &Plan, state: &SchedulerState, start_date: Date) -> BuildResult {
    let offset =
        UtcOffset::from_whole_seconds(plan.utc_offset_minutes * 60).unwrap_or(UtcOffset::UTC);
    let start = local_midnight_ms(start_date, offset);
    build_range(plan, state, start, start + 7 * DAY_MS)
}

/// Generate within an explicit half-open interval using the supplied continuation.
pub fn build_range(plan: &Plan, state: &SchedulerState, start_ms: i64, end_ms: i64) -> BuildResult {
    let mut result = BuildResult {
        items: Vec::new(),
        state: state.clone(),
        warnings: Vec::new(),
        coverage_ms: 0,
        scheduled_ms: 0,
    };
    let mut rng = state
        .rng_state
        .map(Rng::from_state)
        .unwrap_or_else(|| Rng::new(plan.seed));
    let offset =
        UtcOffset::from_whole_seconds(plan.utc_offset_minutes * 60).unwrap_or(UtcOffset::UTC);

    let week_start = start_ms;
    let week_end = end_ms;
    let start_date = time::OffsetDateTime::from_unix_timestamp_nanos(start_ms as i128 * 1_000_000)
        .expect("scheduler start timestamp in range")
        .to_offset(offset)
        .date();
    let mut occupied: Vec<(i64, i64)> = Vec::new();

    let mut date = start_date;
    while local_midnight_ms(date, offset) < week_end {
        let weekday = date.weekday();
        let day_start = local_midnight_ms(date, offset);

        let mut active: Vec<&Daypart> = plan
            .dayparts
            .iter()
            .filter(|d| d.days.contains(&weekday))
            .collect();
        active.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.start_minute.cmp(&b.start_minute))
                .then_with(|| a.name.cmp(&b.name))
        });

        for daypart in active {
            let (start, mut end) = daypart_window(daypart, day_start);
            let start = start.max(week_start);
            if end <= start {
                continue;
            }
            if end > week_end {
                end = week_end;
            }
            if start >= week_end {
                continue;
            }

            let mut free = subtract_intervals(start, end, &occupied);
            if free.is_empty() {
                result.warnings.push(Warning {
                    at_ms: start,
                    daypart: daypart.name.clone(),
                    message: "daypart fully overlapped by a higher-priority daypart".into(),
                });
                continue;
            }

            for (free_start, free_end) in free.drain(..) {
                build_daypart_window(daypart, free_start, free_end, &mut result, &mut rng);
            }
        }

        // Reserve all program/filler items built so far for this day.
        for item in &result.items {
            if item.start_ms < day_start + DAY_MS && item.end_ms > day_start {
                occupied.push((item.start_ms, item.end_ms));
            }
        }
        date += Duration::days(1);
    }

    // Fill the remaining week with fallback content, if configured.
    if !plan.fallback.is_empty() {
        let gaps = subtract_intervals(week_start, week_end, &occupied);
        for (gap_start, gap_end) in gaps {
            fill_gap(
                &plan.fallback,
                gap_start,
                gap_end,
                &mut result,
                &mut rng,
                "fallback",
            );
        }
    } else {
        let gaps = subtract_intervals(week_start, week_end, &occupied);
        for (start, end) in gaps {
            result.warnings.push(Warning {
                at_ms: start,
                daypart: "fallback".into(),
                message: format!("unscheduled gap of {} ms", end - start),
            });
        }
    }

    result.items.sort_by_key(|i| (i.start_ms, i.end_ms));
    result.scheduled_ms = result
        .items
        .iter()
        .map(TimelineItem::duration_ms)
        .sum::<u64>();
    result.coverage_ms = result
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Program)
        .map(TimelineItem::duration_ms)
        .sum::<u64>();
    result.state.rng_state = Some(rng.state());
    result
}

fn build_daypart_window(
    daypart: &Daypart,
    start: i64,
    end: i64,
    result: &mut BuildResult,
    rng: &mut Rng,
) {
    let mut cursor = start;
    let mut tail_filler: Vec<FillerItem> = Vec::new();

    for slot in &daypart.slots {
        if cursor >= end {
            break;
        }
        if !slot.filler.is_empty() {
            tail_filler = slot.filler.clone();
        }
        match slot.mode {
            FillMode::One => {
                if let Some(item) = next_item(slot, &mut result.state, rng) {
                    let placed = place_item(slot, item, cursor, end, result, rng);
                    cursor = advance_cursor(result, cursor);
                    if !placed {
                        break;
                    }
                } else {
                    result.warnings.push(Warning {
                        at_ms: cursor,
                        daypart: daypart.name.clone(),
                        message: format!("slot '{}' has no content", slot.name),
                    });
                }
            }
            FillMode::Count(count) => {
                for _ in 0..count {
                    if cursor >= end {
                        break;
                    }
                    let Some(item) = next_item(slot, &mut result.state, rng) else {
                        result.warnings.push(Warning {
                            at_ms: cursor,
                            daypart: daypart.name.clone(),
                            message: format!("slot '{}' has no content", slot.name),
                        });
                        break;
                    };
                    let placed = place_item(slot, item, cursor, end, result, rng);
                    cursor = advance_cursor(result, cursor);
                    if !placed {
                        break;
                    }
                }
            }
            FillMode::Duration { duration_ms } => {
                let target = (cursor + duration_ms as i64).min(end);
                while cursor < target {
                    let item = match next_item(slot, &mut result.state, rng) {
                        Some(item) => item,
                        None => {
                            result.warnings.push(Warning {
                                at_ms: cursor,
                                daypart: daypart.name.clone(),
                                message: format!("slot '{}' has no content", slot.name),
                            });
                            break;
                        }
                    };
                    let placed = place_item(slot, item, cursor, target, result, rng);
                    cursor = advance_cursor(result, cursor);
                    if !placed {
                        break;
                    }
                }
            }
        }
    }

    if cursor < end {
        fill_gap(&tail_filler, cursor, end, result, rng, &daypart.name);
    }
}

/// Try to place a program item at `cursor` within `limit`.
/// Returns true if placed; false if it did not fit and the caller should stop.
fn place_item(
    slot: &Slot,
    item: ContentItem,
    cursor: i64,
    limit: i64,
    result: &mut BuildResult,
    rng: &mut Rng,
) -> bool {
    if item.duration_ms == 0 {
        result.warnings.push(Warning {
            at_ms: cursor,
            daypart: slot.name.clone(),
            message: format!("item '{}' has zero duration", item.id),
        });
        return true;
    }
    let end = cursor + item.duration_ms as i64;
    if end > limit {
        // Try filler that fits instead of overrunning the window.
        let remaining = (limit - cursor).max(0) as u64;
        if !fill_gap(&slot.filler, cursor, limit, result, rng, &slot.name) {
            result.warnings.push(Warning {
                at_ms: cursor,
                daypart: slot.name.clone(),
                message: format!(
                    "item '{}' ({} ms) does not fit in {} ms and no filler fits",
                    item.id, item.duration_ms, remaining
                ),
            });
        }
        return false;
    }
    result.items.push(TimelineItem {
        id: format!("{}:{}-{}", slot.name, item.id, cursor),
        title: item.title,
        start_ms: cursor,
        end_ms: end,
        source_id: item.id,
        kind: ItemKind::Program,
        epg_title: slot.epg_title.clone(),
    });
    true
}

fn advance_cursor(result: &BuildResult, cursor: i64) -> i64 {
    result
        .items
        .last()
        .map(|item| item.end_ms)
        .unwrap_or(cursor)
        .max(cursor)
}

fn fill_gap(
    filler: &[FillerItem],
    start: i64,
    end: i64,
    result: &mut BuildResult,
    rng: &mut Rng,
    scope: &str,
) -> bool {
    if filler.is_empty() || end <= start {
        return false;
    }
    let mut cursor = start;
    let mut placed_any = false;
    while cursor < end {
        let remaining = (end - cursor) as u64;
        let now = cursor;
        let candidates: Vec<&FillerItem> = filler
            .iter()
            .filter(|f| f.weight > 0 && f.duration_ms > 0 && f.duration_ms <= remaining)
            .filter(|f| {
                result
                    .state
                    .last_played_ms
                    .get(&f.id)
                    .is_none_or(|last| now - *last >= f.cooldown_ms as i64)
            })
            .collect();
        if candidates.is_empty() {
            break;
        }
        let total: u64 = candidates.iter().map(|f| f.weight as u64).sum();
        let mut pick = rng.next_u64() % total;
        let mut chosen = candidates[0];
        for candidate in &candidates {
            if pick < candidate.weight as u64 {
                chosen = candidate;
                break;
            }
            pick -= candidate.weight as u64;
        }
        let item_end = cursor + chosen.duration_ms as i64;
        result.items.push(TimelineItem {
            id: format!("{}:filler-{}-{}", scope, chosen.id, cursor),
            title: chosen.title.clone(),
            start_ms: cursor,
            end_ms: item_end,
            source_id: chosen.id.clone(),
            kind: ItemKind::Filler,
            epg_title: None,
        });
        result
            .state
            .last_played_ms
            .insert(chosen.id.clone(), cursor);
        cursor = item_end;
        placed_any = true;
    }
    if cursor < end {
        result.warnings.push(Warning {
            at_ms: cursor,
            daypart: scope.into(),
            message: format!("{} ms of filler gap could not be filled", end - cursor),
        });
    }
    placed_any
}

fn next_item(slot: &Slot, state: &mut SchedulerState, rng: &mut Rng) -> Option<ContentItem> {
    let len = slot.source.items.len();
    if len == 0 {
        return None;
    }
    let cursor = state.cursors.entry(slot.name.clone()).or_default();
    let index = match slot.order {
        Order::Sequential => {
            let index = cursor.position % len;
            cursor.position = (cursor.position + 1) % len;
            index
        }
        Order::Shuffle => {
            if cursor.bag.is_empty() || cursor.bag_pos >= cursor.bag.len() {
                cursor.bag = (0..len).collect();
                rng.shuffle(&mut cursor.bag);
                cursor.bag_pos = 0;
            }
            let index = cursor.bag[cursor.bag_pos];
            cursor.bag_pos += 1;
            cursor.position += 1;
            index
        }
    };
    slot.source.items.get(index).cloned()
}

fn daypart_window(daypart: &Daypart, day_start_ms: i64) -> (i64, i64) {
    let start = day_start_ms + daypart.start_minute as i64 * MINUTE_MS;
    let mut end = day_start_ms + daypart.end_minute as i64 * MINUTE_MS;
    if end <= start {
        end += DAY_MS;
    }
    (start, end)
}

fn local_midnight_ms(date: Date, offset: UtcOffset) -> i64 {
    PrimitiveDateTime::new(date, Time::MIDNIGHT)
        .assume_offset(offset)
        .unix_timestamp()
        * 1000
}

/// Subtract `occupied` from `[start, end)` and return sorted free intervals.
fn subtract_intervals(start: i64, end: i64, occupied: &[(i64, i64)]) -> Vec<(i64, i64)> {
    if end <= start {
        return Vec::new();
    }
    let mut clipped: Vec<(i64, i64)> = occupied
        .iter()
        .filter_map(|(a, b)| {
            let a = (*a).max(start);
            let b = (*b).min(end);
            (b > a).then_some((a, b))
        })
        .collect();
    clipped.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (a, b) in clipped {
        if let Some(last) = merged.last_mut()
            && a <= last.1
        {
            last.1 = last.1.max(b);
            continue;
        }
        merged.push((a, b));
    }
    let mut free = Vec::new();
    let mut cursor = start;
    for (a, b) in merged {
        if a > cursor {
            free.push((cursor, a));
        }
        cursor = cursor.max(b);
    }
    if cursor < end {
        free.push((cursor, end));
    }
    free
}

/// Convenience: all weekdays.
pub fn every_day() -> Vec<Weekday> {
    vec![
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ]
}

/// Convenience: weekdays only.
pub fn weekdays() -> Vec<Weekday> {
    vec![
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ContentItem, Daypart, FillMode, FillerItem, Order, Plan, Slot, Source};
    use std::collections::BTreeSet;
    use time::{Date, Month};

    fn item(id: &str, minutes: u64) -> ContentItem {
        ContentItem {
            id: id.into(),
            title: format!("Item {id}"),
            duration_ms: minutes * MINUTE_MS as u64,
            series: None,
        }
    }

    fn filler(id: &str, minutes: u64, weight: u32, cooldown_min: u64) -> FillerItem {
        FillerItem {
            id: id.into(),
            title: format!("Filler {id}"),
            duration_ms: minutes * MINUTE_MS as u64,
            weight,
            cooldown_ms: cooldown_min * MINUTE_MS as u64,
        }
    }

    fn start_date() -> Date {
        Date::from_calendar_date(2026, Month::November, 16).unwrap()
    }

    fn simple_plan() -> Plan {
        Plan {
            name: "test".into(),
            seed: 42,
            utc_offset_minutes: 0,
            dayparts: vec![Daypart {
                name: "morning".into(),
                days: every_day(),
                start_minute: 360,
                end_minute: 540,
                priority: 0,
                slots: vec![Slot {
                    name: "cartoons".into(),
                    source: Source {
                        items: (0..6).map(|i| item(&format!("c{i}"), 30)).collect(),
                    },
                    order: Order::Sequential,
                    mode: FillMode::Duration {
                        duration_ms: 3 * 60 * MINUTE_MS as u64,
                    },
                    filler: vec![],
                    epg_title: None,
                }],
            }],
            fallback: vec![],
        }
    }

    #[test]
    fn builds_seven_days() {
        let result = build_week(&simple_plan(), &SchedulerState::default(), start_date());
        assert_eq!(result.items.len(), 42, "6 items * 7 days");
        assert!(
            result.warnings.iter().all(|w| w.daypart == "fallback"),
            "unexpected scheduling warnings: {:?}",
            result.warnings
        );
        assert_eq!(result.coverage_ms, 7 * 3 * 60 * MINUTE_MS as u64);
    }

    #[test]
    fn items_are_monotonic_and_do_not_overlap() {
        let result = build_week(&simple_plan(), &SchedulerState::default(), start_date());
        for pair in result.items.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms, "{pair:?}");
            assert!(pair[0].start_ms < pair[0].end_ms);
        }
    }

    #[test]
    fn deterministic_for_same_seed_and_state() {
        let a = build_week(&simple_plan(), &SchedulerState::default(), start_date());
        let b = build_week(&simple_plan(), &SchedulerState::default(), start_date());
        assert_eq!(
            serde_json::to_string(&a.items).unwrap(),
            serde_json::to_string(&b.items).unwrap()
        );
    }

    #[test]
    fn shuffled_week_persists_random_state_for_restart() {
        let mut plan = simple_plan();
        plan.dayparts[0].slots[0].order = Order::Shuffle;
        let first = build_week(&plan, &SchedulerState::default(), start_date());
        let json = serde_json::to_string(&first.state).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["rng_state"].is_number());
        let restored: SchedulerState = serde_json::from_str(&json).unwrap();
        let date = start_date() + Duration::days(7);
        let a = build_week(&plan, &first.state, date);
        let b = build_week(&plan, &restored, date);
        assert_eq!(
            serde_json::to_string(&a.items).unwrap(),
            serde_json::to_string(&b.items).unwrap()
        );
        assert_ne!(
            serde_json::to_value(&first.state).unwrap()["rng_state"],
            serde_json::to_value(&a.state).unwrap()["rng_state"]
        );
    }

    #[test]
    fn shuffle_does_not_repeat_until_bag_is_exhausted() {
        let mut plan = simple_plan();
        plan.dayparts[0].slots[0].source.items =
            (0..4).map(|i| item(&format!("s{i}"), 5)).collect();
        plan.dayparts[0].slots[0].mode = FillMode::Count(8);
        plan.dayparts[0].slots[0].order = Order::Shuffle;
        let result = build_week(&plan, &SchedulerState::default(), start_date());
        let ids: Vec<&str> = result
            .items
            .iter()
            .take(4)
            .map(|i| i.source_id.as_str())
            .collect();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 4, "first bag should be unique: {ids:?}");
    }

    #[test]
    fn filler_fills_the_tail_of_a_duration_slot() {
        let mut plan = simple_plan();
        plan.dayparts[0].slots[0].source.items = vec![item("a", 22), item("b", 22), item("c", 22)];
        plan.dayparts[0].slots[0].filler = vec![filler("f1", 8, 50, 0), filler("f2", 8, 50, 0)];
        plan.dayparts[0].slots[0].mode = FillMode::Duration {
            duration_ms: 60 * MINUTE_MS as u64,
        };
        let result = build_week(&plan, &SchedulerState::default(), start_date());
        let filler_count = result
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Filler)
            .count();
        assert!(filler_count > 0, "expected filler in the 60 minute slot");
        assert!(
            result.warnings.iter().all(|w| w.daypart == "fallback"),
            "unexpected scheduling warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn fallback_covers_the_unprogrammed_day() {
        let mut plan = simple_plan();
        plan.fallback = vec![filler("bumper", 5, 10, 0)];
        let result = build_week(&plan, &SchedulerState::default(), start_date());
        assert!(result.items.iter().any(|i| i.kind == ItemKind::Filler));
        assert!(result.coverage_fraction() > 0.0);
        for pair in result.items.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms);
        }
    }

    #[test]
    fn randomized_invariants_hold() {
        for seed in 0..50 {
            let mut plan = simple_plan();
            plan.seed = seed;
            plan.dayparts = vec![
                Daypart {
                    name: "morning".into(),
                    days: every_day(),
                    start_minute: 360,
                    end_minute: 720,
                    priority: 10,
                    slots: vec![Slot {
                        name: "morning".into(),
                        source: Source {
                            items: (0..7).map(|i| item(&format!("m{i}"), 25)).collect(),
                        },
                        order: Order::Shuffle,
                        mode: FillMode::Duration {
                            duration_ms: 3 * 60 * MINUTE_MS as u64,
                        },
                        filler: vec![filler("m-f", 5, 10, 0)],
                        epg_title: None,
                    }],
                },
                Daypart {
                    name: "evening".into(),
                    days: every_day(),
                    start_minute: 1080,
                    end_minute: 1440,
                    priority: 0,
                    slots: vec![Slot {
                        name: "evening".into(),
                        source: Source {
                            items: (0..5).map(|i| item(&format!("e{i}"), 40)).collect(),
                        },
                        order: Order::Sequential,
                        mode: FillMode::Count(3),
                        filler: vec![filler("e-f", 5, 10, 0)],
                        epg_title: None,
                    }],
                },
            ];
            plan.fallback = vec![filler("fallback", 5, 10, 0)];
            let result = build_week(&plan, &SchedulerState::default(), start_date());
            for pair in result.items.windows(2) {
                assert!(pair[0].end_ms <= pair[1].start_ms, "seed {seed}");
            }
            assert!(result.items.iter().all(|i| i.start_ms < i.end_ms));
            assert!(result.coverage_fraction() <= 1.0);
        }
    }

    #[test]
    fn overnight_daypart_crosses_midnight() {
        let plan = Plan {
            name: "overnight".into(),
            seed: 1,
            utc_offset_minutes: 0,
            dayparts: vec![Daypart {
                name: "late".into(),
                days: every_day(),
                start_minute: 1320,
                end_minute: 120,
                priority: 0,
                slots: vec![Slot {
                    name: "movies".into(),
                    source: Source {
                        items: vec![item("movie", 120)],
                    },
                    order: Order::Sequential,
                    mode: FillMode::One,
                    filler: vec![],
                    epg_title: None,
                }],
            }],
            fallback: vec![],
        };
        let result = build_week(&plan, &SchedulerState::default(), start_date());
        assert!(
            result
                .items
                .iter()
                .any(|i| i.duration_ms() == 120 * MINUTE_MS as u64)
        );
        for pair in result.items.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms);
        }
    }
}
