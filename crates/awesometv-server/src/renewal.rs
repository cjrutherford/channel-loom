//! Background renewal of generated timelines. Classic and blocks channels
//! without persisted generator ownership are adopted first.
use crate::{
    db,
    programming::{self, RenewalOutcome},
};
use sqlx::SqlitePool;
use std::{collections::HashMap, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::sync::{Mutex, watch};

const DAY_MS: i64 = 86_400_000;
const PERIOD: Duration = Duration::from_secs(3600);
const SCAN_TIMEOUT: Duration = Duration::from_secs(300);
const RENEW_TIMEOUT: Duration = Duration::from_secs(120);
const RETRIES: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
];

#[derive(Default, Debug)]
struct Status {
    last_success_ms: Option<i64>,
    last_error: Option<String>,
    horizon_ms: i64,
    at_risk: bool,
}

struct Worker {
    pool: SqlitePool,
    statuses: Mutex<HashMap<String, Status>>,
    flight: Mutex<()>,
}
impl Worker {
    fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            statuses: Mutex::new(HashMap::new()),
            flight: Mutex::new(()),
        }
    }

    async fn tick(&self, now: OffsetDateTime) -> anyhow::Result<bool> {
        let _flight = self.flight.lock().await;
        let now_ms = i64::try_from(now.unix_timestamp_nanos() / 1_000_000)?;
        let channels = db::list_channels(&self.pool).await?;
        let mut failed = false;
        let mut seen = std::collections::HashSet::new();
        for channel in channels {
            if !matches!(channel.schedule_mode.as_str(), "classic" | "blocks") {
                continue;
            }
            let state = match db::generated_timeline_state(&self.pool, &channel.id).await? {
                Some(state) => state,
                None => {
                    match programming::adopt_unowned_channel(&self.pool, &channel.id, now_ms).await
                    {
                        Ok(true) => {
                            tracing::info!(channel_id = %channel.id, "renewal adopted channel without generator state");
                        }
                        Ok(false) => continue,
                        Err(e) => {
                            failed = true;
                            tracing::warn!(channel_id = %channel.id, error = %format!("{e:#}"), "renewal could not adopt channel");
                            continue;
                        }
                    }
                    let Some(state) = db::generated_timeline_state(&self.pool, &channel.id).await?
                    else {
                        continue;
                    };
                    state
                }
            };
            seen.insert(channel.id.clone());
            if state.boundary_ms >= now_ms.saturating_add(5 * DAY_MS) {
                let mut statuses = self.statuses.lock().await;
                let status = statuses.entry(channel.id.clone()).or_default();
                status.horizon_ms = state.boundary_ms;
                status.at_risk = false;
                status.last_error = None;
                tracing::info!(channel_id = %channel.id, last_success_ms = ?status.last_success_ms, last_error = ?status.last_error, horizon_ms = status.horizon_ms, at_risk = status.at_risk, "renewal status");
                continue;
            }
            let result = bounded_renewal(RENEW_TIMEOUT, self.renew(&channel.id, now)).await;
            let mut statuses = self.statuses.lock().await;
            let status = statuses.entry(channel.id.clone()).or_default();
            match result {
                Ok(RenewalOutcome::SkippedUnowned | RenewalOutcome::UnsupportedMode) => {
                    tracing::warn!(channel_id = %channel.id, "renewal skipped: ownership or mode no longer supported");
                    statuses.remove(&channel.id);
                    continue;
                }
                Ok(_) => {
                    status.last_success_ms = Some(now_ms);
                    status.last_error = None;
                }
                Err(e) => {
                    failed = true;
                    status.last_error = Some(format!("{e:#}"));
                }
            }
            status.horizon_ms = db::generated_timeline_state(&self.pool, &channel.id)
                .await?
                .map_or(state.boundary_ms, |s| s.boundary_ms);
            status.at_risk = status.horizon_ms < now_ms.saturating_add(5 * DAY_MS);
            tracing::info!(channel_id = %channel.id, last_success_ms = ?status.last_success_ms, last_error = ?status.last_error, horizon_ms = status.horizon_ms, at_risk = status.at_risk, "renewal status");
        }
        self.statuses.lock().await.retain(|id, _| seen.contains(id));
        Ok(failed)
    }
    async fn renew(&self, id: &str, now: OffsetDateTime) -> anyhow::Result<RenewalOutcome> {
        #[cfg(test)]
        if FAIL_NEXT.swap(false, std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("injected renewal failure");
        }
        programming::renew_channel_at(&self.pool, id, now).await
    }
}
async fn bounded_renewal<F>(duration: Duration, future: F) -> anyhow::Result<RenewalOutcome>
where
    F: std::future::Future<Output = anyhow::Result<RenewalOutcome>>,
{
    tokio::time::timeout(duration, future)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "renewal timed out after {}s",
                duration.as_secs()
            ))
        })
}

#[cfg(test)]
static FAIL_NEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn start(pool: SqlitePool) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop, mut stopped) = watch::channel(false);
    let worker = Arc::new(Worker::new(pool));
    let handle = tokio::spawn(async move {
        loop {
            let mut retry = 0;
            loop {
                let failed = match tokio::time::timeout(
                    SCAN_TIMEOUT,
                    worker.tick(OffsetDateTime::now_utc()),
                )
                .await
                {
                    Ok(Ok(failed)) => failed,
                    Ok(Err(error)) => {
                        tracing::error!(error = %format!("{error:#}"), "renewal scan failed");
                        true
                    }
                    Err(_) => {
                        tracing::error!(
                            timeout_seconds = SCAN_TIMEOUT.as_secs(),
                            "renewal scan timed out"
                        );
                        true
                    }
                };
                if !failed || retry == RETRIES.len() {
                    break;
                }
                let delay = RETRIES[retry];
                retry += 1;
                tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = stopped.changed() => return }
            }
            tokio::select! { _ = tokio::time::sleep(PERIOD) => {}, _ = stopped.changed() => return }
        }
    });
    (stop, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn stalled_renewal_is_cancelled() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let result = bounded_renewal(Duration::from_millis(10), async move {
            let _guard = guard;
            std::future::pending().await
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn failure_retry_restart_and_duplicate_ticks() {
        let dir = std::env::temp_dir().join(format!("renewal-worker-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES ('m','/m','media',3600000,0)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO collections (id,name,created_at) VALUES ('col','Collection',0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO collection_items (collection_id,media_id,position) VALUES ('col','m',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let channel = db::create_channel(&pool, "1", "Test", &["m".into()])
            .await
            .unwrap();
        db::set_classic_items(
            &pool,
            &channel.id,
            &[db::ClassicItemInput {
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
            }],
        )
        .await
        .unwrap();
        programming::rebuild_channel(&pool, &channel.id)
            .await
            .unwrap();
        let before = db::all_channel_items(&pool, &channel.id).await.unwrap();
        let boundary = before.last().unwrap().end_ms;
        let now = OffsetDateTime::from_unix_timestamp(boundary / 1000 - 4 * 86400).unwrap();
        let worker = Worker::new(pool.clone());
        FAIL_NEXT.store(true, Ordering::SeqCst);
        assert!(worker.tick(now).await.unwrap());
        let after_failure = db::all_channel_items(&pool, &channel.id).await.unwrap();
        let rows = |items: &[db::ChannelItem]| {
            items
                .iter()
                .map(|i| (i.media_id.clone(), i.start_ms, i.end_ms, i.in_ms, i.out_ms))
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(&before), rows(&after_failure));
        {
            let statuses = worker.statuses.lock().await;
            let status = &statuses[&channel.id];
            assert!(status.at_risk && status.last_error.is_some());
        }
        assert!(!worker.tick(now).await.unwrap());
        {
            let statuses = worker.statuses.lock().await;
            let status = &statuses[&channel.id];
            assert!(
                !status.at_risk && status.last_error.is_none() && status.last_success_ms.is_some()
            );
        }
        let after = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert!(after.len() > before.len());
        assert_eq!(rows(&before), rows(&after[..before.len()]));
        assert!(after.last().unwrap().end_ms >= now.unix_timestamp() * 1000 + 7 * DAY_MS);
        let (a, b) = tokio::join!(worker.tick(now), worker.tick(now));
        assert!(!a.unwrap() && !b.unwrap());
        assert_eq!(
            after.len(),
            db::all_channel_items(&pool, &channel.id)
                .await
                .unwrap()
                .len()
        );
        drop(worker);
        pool.close().await;
        let pool = db::init(&dir).await.unwrap();
        let restarted = Worker::new(pool.clone());
        let later = now + time::Duration::days(4);
        assert!(!restarted.tick(later).await.unwrap());
        let caught_up = db::all_channel_items(&pool, &channel.id).await.unwrap();
        assert!(caught_up.last().unwrap().end_ms >= later.unix_timestamp() * 1000 + 7 * DAY_MS);
        assert!(caught_up.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn skips_unowned_and_non_generated_modes() {
        let dir = std::env::temp_dir().join(format!("renewal-skips-{}", uuid::Uuid::new_v4()));
        let pool = db::init(&dir).await.unwrap();
        for (id, mode) in [
            ("legacy", "classic"),
            ("manual", "manual"),
            ("external", "external"),
        ] {
            sqlx::query(
                "INSERT INTO channels (id,number,name,start_ms,schedule_mode,created_at) VALUES (?,?,?,?,?,0)",
            )
            .bind(id)
            .bind(id)
            .bind(id)
            .bind(0_i64)
            .bind(mode)
            .execute(&pool)
            .await
            .unwrap();
        }
        // A legacy channel has no persisted generator state; modes alone do not confer ownership.
        sqlx::query("INSERT INTO generated_timeline_state (channel_id,definition_version,boundary_ms,continuation_json) VALUES ('manual','x',0,'{}'),('external','x',0,'{}')").execute(&pool).await.unwrap();
        let worker = Worker::new(pool.clone());
        assert!(!worker.tick(OffsetDateTime::now_utc()).await.unwrap());
        assert!(worker.statuses.lock().await.is_empty());
        for id in ["legacy", "manual", "external"] {
            assert!(db::all_channel_items(&pool, id).await.unwrap().is_empty());
            let state = db::generated_timeline_state(&pool, id).await.unwrap();
            assert_eq!(
                state.map(|s| s.boundary_ms),
                if id == "legacy" { None } else { Some(0) }
            );
        }
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn unowned_channel(dir: &std::path::Path) -> (SqlitePool, String) {
        let pool = db::init(dir).await.unwrap();
        sqlx::query("INSERT INTO media_items (id,path,title,duration_ms,created_at) VALUES ('m','/m','media',3600000,0)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO collections (id,name,created_at) VALUES ('col','Collection',0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO collection_items (collection_id,media_id,position) VALUES ('col','m',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let channel = db::create_channel(&pool, "1", "Test", &["m".into()])
            .await
            .unwrap();
        db::set_classic_items(
            &pool,
            &channel.id,
            &[db::ClassicItemInput {
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
            }],
        )
        .await
        .unwrap();
        programming::rebuild_channel(&pool, &channel.id)
            .await
            .unwrap();
        // Simulate a channel built before generator ownership was persisted.
        sqlx::query("DELETE FROM generated_timeline_state WHERE channel_id = ?")
            .bind(&channel.id)
            .execute(&pool)
            .await
            .unwrap();
        (pool, channel.id)
    }

    #[tokio::test]
    async fn adopts_live_unowned_channel_without_rewriting_it() {
        let dir = std::env::temp_dir().join(format!("renewal-adopt-{}", uuid::Uuid::new_v4()));
        let (pool, id) = unowned_channel(&dir).await;
        let before = db::all_channel_items(&pool, &id).await.unwrap();
        let boundary = before.last().unwrap().end_ms;
        let now = OffsetDateTime::from_unix_timestamp(boundary / 1000 - 2 * 86400).unwrap();
        let worker = Worker::new(pool.clone());
        assert!(!worker.tick(now).await.unwrap());
        let after = db::all_channel_items(&pool, &id).await.unwrap();
        let key = |i: &db::ChannelItem| (i.media_id.clone(), i.start_ms, i.end_ms);
        assert_eq!(
            before.iter().map(key).collect::<Vec<_>>(),
            after[..before.len()].iter().map(key).collect::<Vec<_>>()
        );
        assert!(after.last().unwrap().end_ms >= now.unix_timestamp() * 1000 + 7 * DAY_MS);
        assert!(after.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        assert!(!worker.statuses.lock().await[&id].at_risk);
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rebuilds_expired_unowned_channel() {
        let dir = std::env::temp_dir().join(format!("renewal-expired-{}", uuid::Uuid::new_v4()));
        let (pool, id) = unowned_channel(&dir).await;
        let first = db::all_channel_items(&pool, &id).await.unwrap()[0].start_ms;
        sqlx::query("DELETE FROM channel_items WHERE channel_id = ? AND start_ms > ?")
            .bind(&id)
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        let now_ms = now.unix_timestamp() * 1000;
        let worker = Worker::new(pool.clone());
        assert!(!worker.tick(now).await.unwrap());
        let after = db::all_channel_items(&pool, &id).await.unwrap();
        assert!(
            after
                .iter()
                .any(|i| i.start_ms <= now_ms && i.end_ms > now_ms)
        );
        assert!(after.last().unwrap().end_ms >= now_ms + 6 * DAY_MS);
        assert!(
            db::generated_timeline_state(&pool, &id)
                .await
                .unwrap()
                .is_some()
        );
        pool.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
