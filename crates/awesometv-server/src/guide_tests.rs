use super::*;
use crate::programming;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

async fn compare(pool: &sqlx::SqlitePool, id: &str, now: i64) {
    let rows = db::all_channel_items(pool, id).await.unwrap();
    assert!(rows.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
    let normalized: Vec<_> = rows
        .iter()
        .map(|r| (r.start_ms.div_euclid(1000), r.end_ms.div_euclid(1000)))
        .collect();
    assert!(normalized.windows(2).all(|w| w[0].1 == w[1].0));
    assert!(normalized.iter().all(|(start, end)| start < end));
    let public = serde_json::to_value(public_guide_at(pool, 24, now).await.ok().unwrap()).unwrap();
    let channel = public
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["number"] == "42")
        .unwrap();
    let expected: Vec<_> = rows
        .iter()
        .filter(|r| r.end_ms > now - 7_200_000 && r.start_ms < now + 86_400_000)
        .map(|r| (r.title.clone(), r.start_ms, r.end_ms))
        .collect();
    let actual: Vec<_> = channel["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["title"].as_str().unwrap().to_owned(),
                r["start_ms"].as_i64().unwrap(),
                r["end_ms"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(actual, expected);
    let xml = xmltv_at(pool, now).await.ok().unwrap();
    assert!(xml.contains("<channel id=\"awesome.42\">"));
    let expected_xml: Vec<_> = rows.iter().filter(|r| r.end_ms > now - 3_600_000 && r.start_ms < now + 86_400_000).map(|r| {
        let fmt = |ms: i64| OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).unwrap().format(&time::macros::format_description!("[year][month][day][hour][minute][second] +0000")).unwrap();
        format!("  <programme start=\"{}\" stop=\"{}\" channel=\"awesome.42\"><title>{}</title></programme>", fmt(r.start_ms), fmt(r.end_ms), r.title)
    }).collect();
    let actual_xml: Vec<_> = xml
        .lines()
        .filter(|line| line.contains("<programme "))
        .map(str::to_owned)
        .collect();
    assert_eq!(actual_xml, expected_xml);
    assert!(
        rows.iter().any(|r| r.start_ms <= now && now < r.end_ms)
            || rows.last().unwrap().end_ms == now
    );
}

async fn exercise_boundary(count: i64) {
    let dir = std::env::temp_dir().join(format!("guide-fixed-{}", uuid::Uuid::new_v4()));
    let pool = db::init(&dir).await.unwrap();
    sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES ('a','/a','Alpha',3600000,0),('b','/b','Beta',3600000,0)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO collections (id,name,created_at) VALUES ('col','Collection',0)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO collection_items (collection_id,media_id,position) VALUES ('col','a',0),('col','b',1)").execute(&pool).await.unwrap();
    let channel = db::create_channel(&pool, "42", "Fixed", &["a".into(), "b".into()])
        .await
        .unwrap();
    db::set_classic_items(
        &pool,
        &channel.id,
        &[db::ClassicItemInput {
            collection_id: "col".into(),
            mode: "multiple".into(),
            count,
            duration_minutes: 60,
            filler_collection_id: None,
            mid_roll_interval_minutes: 0,
            mid_roll_duration_seconds: 0,
            chapter_breaks: false,
            break_markers: vec![],
            order: "sequential".into(),
        }],
    )
    .await
    .unwrap();
    programming::rebuild_channel(&pool, &channel.id)
        .await
        .unwrap();
    let old = db::all_channel_items(&pool, &channel.id).await.unwrap();
    let boundary = 1_735_776_000_123i64;
    let shift = old.last().unwrap().end_ms - boundary;
    sqlx::query("UPDATE channel_items SET start_ms=start_ms-?,end_ms=end_ms-? WHERE channel_id=?")
        .bind(shift)
        .bind(shift)
        .bind(&channel.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE generated_timeline_state SET boundary_ms=boundary_ms-? WHERE channel_id=?")
        .bind(shift)
        .bind(&channel.id)
        .execute(&pool)
        .await
        .unwrap();
    let before = db::all_channel_items(&pool, &channel.id).await.unwrap();
    for now in [boundary - 1, boundary] {
        compare(&pool, &channel.id, now).await;
    }
    let fixed = OffsetDateTime::from_unix_timestamp_nanos(i128::from(boundary) * 1_000_000)
        .unwrap()
        .to_offset(UtcOffset::from_hms(5, 30, 0).unwrap());
    assert_eq!(
        OffsetDateTime::parse(&fixed.format(&Rfc3339).unwrap(), &Rfc3339)
            .unwrap()
            .unix_timestamp_nanos(),
        i128::from(boundary) * 1_000_000
    );
    programming::renew_channel_at(&pool, &channel.id, fixed)
        .await
        .unwrap();
    let after = db::all_channel_items(&pool, &channel.id).await.unwrap();
    assert!(after.len() > before.len());
    assert_eq!(
        serde_json::to_value(&after[..before.len()]).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    assert_eq!(after[before.len()].start_ms, boundary);
    for now in [boundary - 1, boundary, boundary + 1, boundary + 3_600_000] {
        compare(&pool, &channel.id, now).await;
    }
    assert_eq!(xmltv_time(-123), "19691231235959 +0000");
    // Whole-second XMLTV necessarily collapses some positive subsecond intervals.
    assert_eq!(xmltv_time(123), xmltv_time(999));
    pool.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn fixed_clock_boundary_and_renewal_agree() {
    exercise_boundary(4).await;
}

#[tokio::test]
async fn adjacent_repeated_broadcasts_remain_distinct() {
    exercise_boundary(5).await;
}
