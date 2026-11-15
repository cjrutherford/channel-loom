use serde::{Deserialize, Serialize};
use time::Weekday;

/// A weekly repeating plan, expressed in a fixed offset for Phase 0.
/// Production uses an IANA time zone; this spike deliberately keeps DST out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub name: String,
    pub seed: u64,
    /// Minutes east of UTC. Phase 0 only; production stores an IANA zone.
    pub utc_offset_minutes: i32,
    pub dayparts: Vec<Daypart>,
    pub fallback: Vec<FillerItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Daypart {
    pub name: String,
    pub days: Vec<Weekday>,
    /// Minutes from local midnight, inclusive.
    pub start_minute: u16,
    /// Minutes from local midnight, exclusive. May be 1440.
    pub end_minute: u16,
    pub priority: i32,
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub name: String,
    pub source: Source,
    pub order: Order,
    pub mode: FillMode,
    pub filler: Vec<FillerItem>,
    pub epg_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub items: Vec<ContentItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Sequential,
    Shuffle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FillMode {
    One,
    Count(u32),
    Duration { duration_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentItem {
    pub id: String,
    pub title: String,
    pub duration_ms: u64,
    pub series: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillerItem {
    pub id: String,
    pub title: String,
    pub duration_ms: u64,
    /// Relative selection weight. 0 means never select.
    pub weight: u32,
    /// Minimum time between two plays of this item.
    pub cooldown_ms: u64,
}

impl FillerItem {
    pub fn content(&self) -> ContentItem {
        ContentItem {
            id: self.id.clone(),
            title: self.title.clone(),
            duration_ms: self.duration_ms,
            series: None,
        }
    }
}
