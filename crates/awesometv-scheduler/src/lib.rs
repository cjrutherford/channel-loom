//! A pure, deterministic timeline builder for ChannelLoom.
//!
//! Phase 0 scope: weekly dayparts, ordered/shuffled sources, one/count/duration
//! fill modes, tail filler, explicit cursor state, and invariant tests.
//!
//! The production scheduler will add classic/block/sequential schedule compilers,
//! DST-aware IANA time zones, linked slots, mid-roll, and EPG grouping. The core
//! contract is intentionally small and I/O-free.

pub mod plan;
pub mod rng;
pub mod timeline;

pub use plan::{ContentItem, Daypart, FillMode, FillerItem, Order, Plan, Slot, Source};
pub use timeline::{
    BuildResult, CursorState, SchedulerState, TimelineItem, build_week, every_day, weekdays,
};
