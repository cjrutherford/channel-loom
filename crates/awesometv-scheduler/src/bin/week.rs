use std::env;
use std::fs;

use awesometv_scheduler::{
    ContentItem, Daypart, FillMode, FillerItem, Order, Plan, Slot, Source, build_week, every_day,
};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

fn main() {
    let mut plan_path: Option<String> = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plan" => plan_path = args.next(),
            "--help" | "-h" => {
                println!("usage: week [--plan plan.json]");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let plan = match plan_path {
        Some(path) => {
            let raw = fs::read_to_string(&path).expect("read plan");
            serde_json::from_str::<Plan>(&raw).expect("parse plan")
        }
        None => sample_plan(),
    };

    let start = Date::from_calendar_date(2026, Month::November, 16).expect("date");
    let offset =
        UtcOffset::from_whole_seconds(plan.utc_offset_minutes * 60).unwrap_or(UtcOffset::UTC);
    let week_start_ms = PrimitiveDateTime::new(start, Time::MIDNIGHT)
        .assume_offset(offset)
        .unix_timestamp()
        * 1000;
    let result = build_week(&plan, &Default::default(), start);

    println!("plan: {}", plan.name);
    println!(
        "week: 2026-11-16 .. 2026-11-23 (UTC{:+})",
        plan.utc_offset_minutes / 60
    );
    println!("items: {}", result.items.len());
    println!(
        "program coverage: {:.1}%",
        result.coverage_fraction() * 100.0
    );
    println!("warnings: {}", result.warnings.len());
    for warning in result.warnings.iter().take(10) {
        println!(
            "  warning: {} @ {}: {}",
            warning.daypart,
            format_clock(week_start_ms, warning.at_ms),
            warning.message
        );
    }
    println!();
    println!("first 20 items:");
    for item in result.items.iter().take(20) {
        println!(
            "  {}  {:>5} min  {:<9}  {}",
            format_clock(week_start_ms, item.start_ms),
            item.duration_ms() / 60_000,
            match item.kind {
                awesometv_scheduler::timeline::ItemKind::Program => "program",
                awesometv_scheduler::timeline::ItemKind::Filler => "filler",
            },
            item.title
        );
    }
}

fn sample_plan() -> Plan {
    Plan {
        name: "Phase 0 sample week".into(),
        seed: 0xA11CE,
        utc_offset_minutes: -300,
        dayparts: vec![
            Daypart {
                name: "morning cartoons".into(),
                days: every_day(),
                start_minute: 6 * 60,
                end_minute: 9 * 60,
                priority: 10,
                slots: vec![Slot {
                    name: "morning".into(),
                    source: Source {
                        items: (0..10)
                            .map(|i| ContentItem {
                                id: format!("cartoon-{i}"),
                                title: format!("Cartoon {i}"),
                                duration_ms: 22 * 60_000,
                                series: Some("cartoons".into()),
                            })
                            .collect(),
                    },
                    order: Order::Shuffle,
                    mode: FillMode::Duration {
                        duration_ms: 3 * 60 * 60_000,
                    },
                    filler: vec![
                        FillerItem {
                            id: "bumper-a".into(),
                            title: "Station bumper A".into(),
                            duration_ms: 30_000,
                            weight: 50,
                            cooldown_ms: 5 * 60_000,
                        },
                        FillerItem {
                            id: "bumper-b".into(),
                            title: "Station bumper B".into(),
                            duration_ms: 45_000,
                            weight: 25,
                            cooldown_ms: 5 * 60_000,
                        },
                        FillerItem {
                            id: "bumper-c".into(),
                            title: "Station bumper C".into(),
                            duration_ms: 15_000,
                            weight: 50,
                            cooldown_ms: 0,
                        },
                    ],
                    epg_title: Some("Morning Cartoons".into()),
                }],
            },
            Daypart {
                name: "primetime".into(),
                days: every_day(),
                start_minute: 18 * 60,
                end_minute: 22 * 60,
                priority: 20,
                slots: vec![Slot {
                    name: "primetime".into(),
                    source: Source {
                        items: (0..8)
                            .map(|i| ContentItem {
                                id: format!("sitcom-{i}"),
                                title: format!("Sitcom {i}"),
                                duration_ms: 30 * 60_000,
                                series: Some("sitcoms".into()),
                            })
                            .collect(),
                    },
                    order: Order::Sequential,
                    mode: FillMode::Duration {
                        duration_ms: 4 * 60 * 60_000,
                    },
                    filler: vec![FillerItem {
                        id: "ad-break".into(),
                        title: "Commercial break".into(),
                        duration_ms: 60_000,
                        weight: 100,
                        cooldown_ms: 0,
                    }],
                    epg_title: None,
                }],
            },
        ],
        fallback: vec![FillerItem {
            id: "overnight".into(),
            title: "Overnight filler".into(),
            duration_ms: 5 * 60_000,
            weight: 10,
            cooldown_ms: 0,
        }],
    }
}

fn format_clock(week_start_ms: i64, at_ms: i64) -> String {
    let delta = (at_ms - week_start_ms).max(0);
    let day = delta / 86_400_000;
    let minute = (delta % 86_400_000) / 60_000;
    format!("D{} {:02}:{:02}", day + 1, minute / 60, minute % 60)
}
