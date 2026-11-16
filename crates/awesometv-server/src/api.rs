use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;

use crate::backup;
use crate::cloud::CloudManager;
use crate::config::Config;
use crate::db::{self, Channel, ChannelItem, Media};
use crate::engine::Engine;
use crate::external;
use crate::jellyfin;
use crate::programming;
use crate::scan;
use crate::scripted;

#[derive(Clone)]
pub struct Session {
    pub username: String,
    pub expires_ms: i64,
}

const LOGIN_WINDOW_MS: i64 = 15 * 60 * 1000;
const LOGIN_MAX_FAILURES: usize = 10;

/// In-memory failed login tracking. A single administrator account on a
/// self-hosted box does not need a persistent store, and losing the counters on
/// restart only ever makes the limiter more permissive for one window.
#[derive(Default)]
pub struct LoginThrottle {
    failures: HashMap<String, Vec<i64>>,
}

impl LoginThrottle {
    /// Seconds the caller should wait, or `None` when attempts are allowed.
    fn retry_after(&mut self, key: &str, now: i64) -> Option<u64> {
        let retry = match self.failures.get_mut(key) {
            Some(timestamps) => {
                timestamps.retain(|at| now.saturating_sub(*at) < LOGIN_WINDOW_MS);
                if timestamps.len() < LOGIN_MAX_FAILURES {
                    None
                } else {
                    timestamps.first().map(|oldest| {
                        let remaining_ms = LOGIN_WINDOW_MS - (now - oldest);
                        (remaining_ms.max(1_000) / 1_000) as u64
                    })
                }
            }
            None => None,
        };
        if retry.is_none()
            && self
                .failures
                .get(key)
                .is_some_and(|timestamps| timestamps.is_empty())
        {
            self.failures.remove(key);
        }
        retry
    }

    fn record_failure(&mut self, key: &str, now: i64) {
        if self.failures.len() > 4_096 {
            self.failures
                .retain(|_, timestamps| timestamps.iter().any(|at| now - at < LOGIN_WINDOW_MS));
        }
        let timestamps = self.failures.entry(key.to_string()).or_default();
        timestamps.push(now);
        if timestamps.len() > LOGIN_MAX_FAILURES * 2 {
            let excess = timestamps.len() - LOGIN_MAX_FAILURES * 2;
            timestamps.drain(0..excess);
        }
    }

    fn clear(&mut self, key: &str) {
        self.failures.remove(key);
    }
}

#[cfg(test)]
mod throttle_tests {
    use super::*;

    #[test]
    fn blocks_after_repeated_failures() {
        let mut throttle = LoginThrottle::default();
        let now = 1_000_000;
        for _ in 0..LOGIN_MAX_FAILURES {
            assert!(throttle.retry_after("1.2.3.4", now).is_none());
            throttle.record_failure("1.2.3.4", now);
        }
        assert!(throttle.retry_after("1.2.3.4", now).is_some());
        assert!(throttle.retry_after("5.6.7.8", now).is_none());
    }

    #[test]
    fn clears_after_successful_login() {
        let mut throttle = LoginThrottle::default();
        for _ in 0..LOGIN_MAX_FAILURES {
            throttle.record_failure("1.2.3.4", 1_000_000);
        }
        throttle.clear("1.2.3.4");
        assert!(throttle.retry_after("1.2.3.4", 1_000_000).is_none());
    }

    #[test]
    fn window_expiry_unblocks() {
        let mut throttle = LoginThrottle::default();
        for _ in 0..LOGIN_MAX_FAILURES {
            throttle.record_failure("1.2.3.4", 1_000_000);
        }
        assert!(throttle.retry_after("1.2.3.4", 1_000_000).is_some());
        assert!(
            throttle
                .retry_after("1.2.3.4", 1_000_000 + LOGIN_WINDOW_MS + 1)
                .is_none()
        );
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub pool: SqlitePool,
    pub engine: Arc<Engine>,
    pub cloud: Arc<CloudManager>,
    pub sessions: Arc<Mutex<HashMap<String, Session>>>,
    pub login_throttle: Arc<Mutex<LoginThrottle>>,
    pub precache: Arc<crate::precache::Precache>,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/api/v1/auth/me", get(auth_me))
        .route("/api/v1/config", get(public_config))
        .route("/api/v1/sources", get(sources))
        .route("/api/v1/sources/jellyfin/sync", post(jellyfin_sync))
        .route("/api/v1/sources/emby/sync", post(emby_sync))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/metrics", get(metrics))
        .route("/api/v1/precache", get(precache_status))
        .route("/api/v1/precache/{media_id}", delete(remove_precache))
        .route(
            "/api/v1/settings/language",
            get(language_settings).put(update_language_settings),
        )
        .route("/metrics", get(prometheus))
        .route("/api/v1/media", get(media))
        .route(
            "/api/v1/collections",
            get(collections).post(create_collection),
        )
        .route(
            "/api/v1/collections/{id}",
            get(collection).delete(delete_collection),
        )
        .route(
            "/api/v1/channels/{id}/blocks",
            get(blocks).put(update_blocks),
        )
        .route(
            "/api/v1/channels/{id}/classic",
            get(classic).put(update_classic),
        )
        .route(
            "/api/v1/channels/{id}/schedule.yaml",
            get(export_schedule_yaml).post(apply_schedule_yaml),
        )
        .route(
            "/api/v1/channels/{id}/playout.json",
            get(export_playout_json).post(apply_playout_json),
        )
        .route("/api/v1/channels/{id}/rebuild", post(rebuild))
        .route("/api/v1/backups", get(backups).post(create_backup))
        .route("/api/v1/backups/{name}", get(download_backup))
        .route("/api/v1/restore", post(restore))
        .route("/api/v1/scan", post(scan_handler))
        .route(
            "/api/v1/cloud/sources",
            get(cloud_sources).post(create_cloud_source),
        )
        .route("/api/v1/cloud/sources/{id}", delete(delete_cloud_source))
        .route(
            "/api/v1/cloud/sources/{id}/roots",
            get(cloud_roots).post(add_cloud_root),
        )
        .route("/api/v1/cloud/roots/{id}", delete(delete_cloud_root))
        .route("/api/v1/cloud/sources/{id}/browse", get(browse_cloud))
        .route("/api/v1/cloud/sources/{id}/drives", get(cloud_drives))
        .route("/api/v1/cloud/sources/{id}/scan", post(scan_cloud))
        .route("/api/v1/media/probe-chapters", post(probe_chapters_handler))
        .route(
            "/api/v1/templates",
            get(block_templates).post(create_block_template),
        )
        .route(
            "/api/v1/templates/{id}",
            get(block_template).delete(delete_block_template),
        )
        .route(
            "/api/v1/channels/{id}/apply-template",
            post(apply_block_template),
        )
        .route(
            "/api/v1/channels/{id}/overrides",
            get(schedule_overrides).post(create_schedule_override),
        )
        .route(
            "/api/v1/channels/{id}/overrides/{override_id}",
            delete(delete_schedule_override),
        )
        .route(
            "/api/v1/channels/{id}/graphics",
            get(channel_graphics).put(update_channel_graphics),
        )
        .route(
            "/api/v1/channels/{id}/stream",
            get(channel_stream).put(update_channel_stream),
        )
        .route("/api/v1/channels", get(channels).post(create_channel))
        .route("/api/v1/channels/{id}/catchup", post(start_catchup))
        .route(
            "/api/v1/channels/{id}/dvr",
            post(start_dvr).delete(stop_dvr_handler),
        )
        .route("/api/v1/channels/{id}/cast", post(start_cast))
        .route("/api/v1/channels/{id}/now", get(now_playing))
        .route("/api/v1/channels/{id}/guide", get(guide))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            basic_auth,
        ));

    let public = Router::new()
        .route("/", get(public_index))
        .route("/admin", get(admin_index))
        .route("/admin/", get(admin_index))
        .route("/admin/login", get(admin_index))
        .route("/cast-receiver", get(cast_receiver))
        .route("/api/v1/public/config", get(public_viewer_config))
        .route("/api/v1/public/channels", get(public_channels))
        .route("/api/v1/public/guide", get(public_guide))
        .route("/api/v1/public/channels/{number}/cast", post(public_cast))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/icons/{file}", get(icon))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/healthz", get(health))
        .route("/discover.json", get(discover))
        .route("/lineup.json", get(hdhr_lineup))
        .route("/lineup_status.json", get(lineup_status))
        .route("/device.xml", get(device_xml))
        .route("/auto/{file}", get(ts_stream))
        .route("/tuner/{tuner}/{file}", get(ts_stream))
        .route("/iptv/channels.m3u", get(m3u))
        .route("/iptv/xmltv.xml", get(xmltv))
        .route("/channel/{file}", get(hls_playlist))
        .route("/channel/{number}/{file}", get(hls_segment))
        .route("/catchup/{file}", get(catchup_playlist))
        .route("/catchup/{session}/{file}", get(catchup_segment))
        .route("/dvr/{file}", get(dvr_playlist))
        .route("/dvr/{number}/{file}", get(dvr_segment))
        .route("/cloud/{source}/{remote_id}", get(cloud_proxy))
        .route("/cast/{file}", get(cast_playlist))
        .route("/cast/{number}/{file}", get(cast_segment));

    protected.merge(public).with_state(state)
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// Requests reach the app over plain HTTP when a reverse proxy terminates TLS,
/// so trust the standard forwarding headers to decide whether the session
/// cookie must be marked `Secure`.
fn request_is_secure(headers: &HeaderMap) -> bool {
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        return proto
            .split(',')
            .any(|value| value.trim().eq_ignore_ascii_case("https"));
    }
    headers
        .get("forwarded")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').any(|part| {
                part.trim()
                    .strip_prefix("proto=")
                    .is_some_and(|proto| proto.trim_matches('"').eq_ignore_ascii_case("https"))
            })
        })
}

/// Prefer the header the reverse proxy sets itself. `X-Forwarded-For` is
/// append-only, so the last entry is the one added by the closest proxy; using
/// the first entry would let a client spoof its way around the limiter.
fn client_key(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.rsplit(',').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("unknown")
        .to_string()
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let key = client_key(&headers);
    let now = db::now_ms();
    if let Some(retry_after) = state.login_throttle.lock().await.retry_after(&key, now) {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "too many failed login attempts"})),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return Ok(response);
    }
    if request.username != state.config.admin_user
        || request.password != state.config.admin_password
    {
        state.login_throttle.lock().await.record_failure(&key, now);
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    state.login_throttle.lock().await.clear(&key);
    let token = uuid::Uuid::new_v4().to_string();
    state.sessions.lock().await.insert(
        token.clone(),
        Session {
            username: request.username,
            expires_ms: db::now_ms() + 7 * 86_400_000,
        },
    );
    let mut response = Json(serde_json::json!({"user": state.config.admin_user})).into_response();
    let secure = if request_is_secure(&headers) {
        "; Secure"
    } else {
        ""
    };
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "awesometv_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800{secure}"
        ))
        .map_err(|error| ApiError::internal(format!("cookie: {error}")))?,
    );
    Ok(response)
}

async fn logout(State(state): State<AppState>, request: Request) -> Response {
    if let Some(token) = session_token(request.headers()) {
        state.sessions.lock().await.remove(&token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let secure = if request_is_secure(request.headers()) {
        "; Secure"
    } else {
        ""
    };
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "awesometv_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"
        ))
        .expect("static cookie header is valid"),
    );
    response
}

async fn auth_me(State(state): State<AppState>, request: Request) -> Json<serde_json::Value> {
    let username =
        session_token(request.headers())
            .and_then(|token| {
                state.sessions.try_lock().ok().and_then(|sessions| {
                    sessions.get(&token).map(|session| session.username.clone())
                })
            })
            .unwrap_or_else(|| state.config.admin_user.clone());
    Json(serde_json::json!({ "user": username }))
}

async fn manifest() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../web/manifest.webmanifest"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../web/sw.js"),
    )
}

async fn icon(Path(file): Path<String>) -> Result<Response, ApiError> {
    let bytes: &'static [u8] = match file.as_str() {
        "icon-192.png" => include_bytes!("../web/icons/icon-192.png"),
        "icon-512.png" => include_bytes!("../web/icons/icon-512.png"),
        "icon-maskable-512.png" => include_bytes!("../web/icons/icon-maskable-512.png"),
        "apple-touch-icon.png" => include_bytes!("../web/icons/apple-touch-icon.png"),
        _ => return Err(ApiError::not_found("icon not found")),
    };
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        Body::from(bytes),
    )
        .into_response())
}

async fn public_index() -> Html<&'static str> {
    Html(include_str!("../web/public.html"))
}

async fn cast_receiver() -> Html<&'static str> {
    Html(include_str!("../web/cast-receiver.html"))
}

/// The admin console shell is only sent to authenticated callers. Anonymous
/// visitors get the login page instead, so the admin JavaScript and markup are
/// not handed out publicly.
async fn admin_index(State(state): State<AppState>, request: Request) -> Response {
    let authorized = request_is_authorized(&state, request.headers()).await;
    let mut response = if authorized {
        Html(include_str!("../web/index.html")).into_response()
    } else {
        Html(include_str!("../web/login.html")).into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("x-robots-tag", HeaderValue::from_static("noindex"));
    response
}

async fn health() -> &'static str {
    "ok\n"
}

async fn media(State(state): State<AppState>) -> Result<Json<Vec<Media>>, ApiError> {
    Ok(Json(db::list_media(&state.pool).await?))
}

#[derive(Serialize)]
struct PublicConfig {
    public_url: String,
    jellyfin_url: String,
    emby_url: String,
    emby_configured: bool,
    cast_base_url: String,
    cast_receiver_id: String,
}

async fn public_config(State(state): State<AppState>) -> Json<PublicConfig> {
    Json(PublicConfig {
        public_url: state.config.public_url.clone(),
        jellyfin_url: state.config.jellyfin_url.clone(),
        emby_url: state.config.emby_url.clone(),
        emby_configured: !state.config.emby_api_key.trim().is_empty(),
        cast_base_url: cast_base(&state.config),
        cast_receiver_id: state.config.cast_receiver_id.clone(),
    })
}

#[derive(Serialize)]
struct ViewerConfig {
    cast_receiver_id: String,
    cast_base_url: String,
}

async fn public_viewer_config(State(state): State<AppState>) -> Json<ViewerConfig> {
    Json(ViewerConfig {
        cast_receiver_id: state.config.cast_receiver_id.clone(),
        cast_base_url: cast_base(&state.config),
    })
}

#[derive(Serialize)]
struct PublicItem {
    title: String,
    start_ms: i64,
    end_ms: i64,
    kind: String,
}

impl From<ChannelItem> for PublicItem {
    fn from(item: ChannelItem) -> Self {
        Self {
            title: item.title,
            start_ms: item.start_ms,
            end_ms: item.end_ms,
            kind: item.kind,
        }
    }
}

#[derive(Serialize)]
struct PublicChannel {
    number: String,
    name: String,
    now: Option<PublicItem>,
    next: Option<PublicItem>,
}

#[derive(Serialize)]
struct PublicGuideChannel {
    number: String,
    name: String,
    items: Vec<PublicItem>,
}

/// Public projection of the channel lineup. Deliberately omits stream,
/// graphics, and media identifiers so the viewer cannot read admin config.
async fn public_channels(
    State(state): State<AppState>,
) -> Result<Json<Vec<PublicChannel>>, ApiError> {
    let views = db::list_channel_views(&state.pool).await?;
    Ok(Json(
        views
            .into_iter()
            .map(|view| PublicChannel {
                number: view.number,
                name: view.name,
                now: view.now.map(PublicItem::from),
                next: view.next.map(PublicItem::from),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PublicGuideQuery {
    #[serde(default = "default_guide_hours")]
    hours: i64,
}

fn default_guide_hours() -> i64 {
    6
}

async fn public_guide(
    State(state): State<AppState>,
    Query(query): Query<PublicGuideQuery>,
) -> Result<Json<Vec<PublicGuideChannel>>, ApiError> {
    let now = db::now_ms();
    let hours = query.hours.clamp(1, 24);
    let channels = db::list_channels(&state.pool).await?;
    let mut guide = Vec::with_capacity(channels.len());
    for channel in channels {
        let items = db::guide_items(
            &state.pool,
            &channel.id,
            // The viewer floors its guide window to the previous hour, so look
            // back two hours to avoid a gap at the left edge.
            now - 2 * 3_600_000,
            now + hours * 3_600_000,
        )
        .await?
        .into_iter()
        .map(PublicItem::from)
        .collect();
        guide.push(PublicGuideChannel {
            number: channel.number,
            name: channel.name,
            items,
        });
    }
    Ok(Json(guide))
}

#[derive(Serialize)]
struct SourceView {
    id: String,
    label: String,
    path: String,
    exists: bool,
    kind: String,
}

async fn sources(State(state): State<AppState>) -> Json<Vec<SourceView>> {
    let items = [
        ("movies", "Movies", &state.config.movies_root, "movie"),
        ("tv", "TV Shows", &state.config.tv_root, "episode"),
        ("music", "Music", &state.config.music_root, "music"),
        ("images", "Images", &state.config.images_root, "image"),
        (
            "commercials",
            "Commercials",
            &state.config.commercials_root,
            "other",
        ),
    ];
    Json(
        items
            .into_iter()
            .map(|(id, label, path, kind)| SourceView {
                id: id.into(),
                label: label.into(),
                path: path.display().to_string(),
                exists: path.exists(),
                kind: kind.into(),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct JellyfinSyncRequest {
    #[serde(default = "default_jellyfin_limit")]
    limit: usize,
}

fn default_jellyfin_limit() -> usize {
    20_000
}

async fn jellyfin_sync(
    State(state): State<AppState>,
    Json(request): Json<JellyfinSyncRequest>,
) -> Result<Json<jellyfin::SyncReport>, ApiError> {
    let api_key = std::env::var("JELLYFIN_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        return Err(ApiError::bad_request("JELLYFIN_API_KEY is not set"));
    }
    let base_url = state.config.jellyfin_url.clone();
    let movies_root = state.config.movies_root.clone();
    let tv_root = state.config.tv_root.clone();
    let music_root = state.config.music_root.clone();
    let limit = request.limit.clamp(1, 50_000);
    let (mut report, media) = tokio::task::spawn_blocking(move || {
        jellyfin::sync(
            &base_url,
            &api_key,
            &movies_root,
            &tv_root,
            &music_root,
            limit,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("jellyfin task failed: {error}"))??;
    for item in media {
        match db::upsert_media(&state.pool, &item).await {
            Ok(()) => {}
            Err(error) => {
                report.imported = report.imported.saturating_sub(1);
                report.errors += 1;
                tracing::warn!(%error, path = %item.path, "jellyfin upsert failed");
            }
        }
    }
    Ok(Json(report))
}

async fn language_settings(
    State(state): State<AppState>,
) -> Result<Json<db::LanguageSettings>, ApiError> {
    Ok(Json(db::language_settings(&state.pool).await?))
}

async fn update_language_settings(
    State(state): State<AppState>,
    Json(settings): Json<db::LanguageSettings>,
) -> Result<Json<db::LanguageSettings>, ApiError> {
    if !matches!(settings.subtitle_mode.as_str(), "off" | "burn") {
        return Err(ApiError::bad_request(
            "default subtitle mode must be off or burn",
        ));
    }
    for language in [&settings.audio_language, &settings.subtitle_language]
        .into_iter()
        .flatten()
    {
        if language.chars().count() > 16 {
            return Err(ApiError::bad_request(
                "language codes are limited to 16 characters",
            ));
        }
    }
    db::set_app_setting(
        &state.pool,
        "default_audio_language",
        settings.audio_language.as_deref().unwrap_or("").trim(),
    )
    .await?;
    db::set_app_setting(
        &state.pool,
        "default_subtitle_mode",
        &settings.subtitle_mode,
    )
    .await?;
    db::set_app_setting(
        &state.pool,
        "default_subtitle_language",
        settings.subtitle_language.as_deref().unwrap_or("").trim(),
    )
    .await?;
    for channel in db::list_channels(&state.pool).await? {
        state.engine.refresh(&channel.id).await;
    }
    Ok(Json(db::language_settings(&state.pool).await?))
}

#[derive(Serialize)]
struct MetricsResponse {
    uptime_ms: u128,
    channels: usize,
    media_counts: Vec<(String, i64)>,
    hls_bytes: u64,
    producers: Vec<crate::engine::ProducerSnapshot>,
}

async fn collect_metrics(state: &AppState) -> Result<MetricsResponse, ApiError> {
    let producers = state.engine.producer_snapshots().await;
    let hls_bytes = producers.iter().map(|producer| producer.bytes).sum();
    Ok(MetricsResponse {
        uptime_ms: state.engine.uptime_ms(),
        channels: db::list_channels(&state.pool).await?.len(),
        media_counts: db::counts_by_kind(&state.pool).await?,
        hls_bytes,
        producers,
    })
}

async fn metrics(State(state): State<AppState>) -> Result<Json<MetricsResponse>, ApiError> {
    Ok(Json(collect_metrics(&state).await?))
}

async fn prometheus(State(state): State<AppState>) -> Result<Response, ApiError> {
    let metrics = collect_metrics(&state).await?;
    let mut body = String::new();
    body.push_str("# HELP awesometv_uptime_seconds Process uptime in seconds.\n");
    body.push_str("# TYPE awesometv_uptime_seconds gauge\n");
    body.push_str(&format!(
        "awesometv_uptime_seconds {:.3}\n",
        metrics.uptime_ms as f64 / 1000.0
    ));
    body.push_str("# HELP awesometv_channels Configured channels.\n");
    body.push_str("# TYPE awesometv_channels gauge\n");
    body.push_str(&format!("awesometv_channels {}\n", metrics.channels));
    body.push_str("# HELP awesometv_media_items Catalog media by kind.\n");
    body.push_str("# TYPE awesometv_media_items gauge\n");
    for (kind, count) in &metrics.media_counts {
        body.push_str(&format!(
            "awesometv_media_items{{kind=\"{kind}\"}} {count}\n"
        ));
    }
    body.push_str("# HELP awesometv_producer_segments HLS segments on disk per channel.\n");
    body.push_str("# TYPE awesometv_producer_segments gauge\n");
    for producer in &metrics.producers {
        body.push_str(&format!(
            "awesometv_producer_segments{{channel=\"{}\"}} {}\n",
            producer.channel, producer.segments
        ));
    }
    body.push_str("# HELP awesometv_producer_bytes HLS bytes on disk per channel.\n");
    body.push_str("# TYPE awesometv_producer_bytes gauge\n");
    for producer in &metrics.producers {
        body.push_str(&format!(
            "awesometv_producer_bytes{{channel=\"{}\"}} {}\n",
            producer.channel, producer.bytes
        ));
    }
    body.push_str("# HELP awesometv_producer_startup_ms Producer startup latency.\n");
    body.push_str("# TYPE awesometv_producer_startup_ms gauge\n");
    for producer in &metrics.producers {
        body.push_str(&format!(
            "awesometv_producer_startup_ms{{channel=\"{}\"}} {}\n",
            producer.channel, producer.startup_ms
        ));
    }
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

#[derive(Serialize)]
struct Stats {
    media_counts: Vec<(String, i64)>,
    channels: usize,
    jellyfin_configured: bool,
    emby_configured: bool,
}

async fn precache_status(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let entries = db::list_precache(&state.pool).await?;
    let ready_bytes: i64 = entries
        .iter()
        .filter(|entry| entry.status == "ready")
        .map(|entry| entry.bytes)
        .sum();
    let running_bytes: i64 = entries
        .iter()
        .filter(|entry| entry.status == "running")
        .map(|entry| entry.bytes)
        .sum();
    let queued = entries
        .iter()
        .filter(|entry| entry.status == "queued")
        .count();
    Ok(Json(serde_json::json!({
        "enabled": state.config.precache_enabled,
        "max_bytes": state.config.precache_max_gb.max(0) * 1_000_000_000i64,
        "ready_bytes": ready_bytes,
        "running_bytes": running_bytes,
        "queued": queued,
        "entries": entries,
    })))
}

async fn remove_precache(
    State(state): State<AppState>,
    Path(media_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.precache.remove(&media_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stats(State(state): State<AppState>) -> Result<Json<Stats>, ApiError> {
    Ok(Json(Stats {
        media_counts: db::counts_by_kind(&state.pool).await?,
        channels: db::list_channels(&state.pool).await?.len(),
        jellyfin_configured: std::env::var("JELLYFIN_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false),
        emby_configured: !state.config.emby_api_key.trim().is_empty(),
    }))
}

async fn emby_sync(
    State(state): State<AppState>,
    Json(request): Json<JellyfinSyncRequest>,
) -> Result<Json<jellyfin::SyncReport>, ApiError> {
    if state.config.emby_api_key.trim().is_empty() {
        return Err(ApiError::bad_request("EMBY_API_KEY is not set"));
    }
    let base_url = state.config.emby_url.clone();
    let api_key = state.config.emby_api_key.clone();
    let movies_root = state.config.movies_root.clone();
    let tv_root = state.config.tv_root.clone();
    let music_root = state.config.music_root.clone();
    let limit = request.limit.clamp(1, 50_000);
    let (mut report, media) = tokio::task::spawn_blocking(move || {
        jellyfin::sync(
            &base_url,
            &api_key,
            &movies_root,
            &tv_root,
            &music_root,
            limit,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("emby task failed: {error}"))??;
    report.imported = media.len();
    for item in &media {
        if let Err(error) = db::upsert_media(&state.pool, item).await {
            tracing::warn!(%error, path = %item.path, "emby upsert failed");
        }
    }
    Ok(Json(report))
}

#[derive(Deserialize)]
struct ScanRequest {
    #[serde(default)]
    source: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    kind: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Serialize)]
struct ScanResponse {
    scanned: usize,
    path: String,
    media: Vec<Media>,
}

async fn scan_handler(
    State(state): State<AppState>,
    Json(request): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    let (path, kind) = if !request.path.trim().is_empty() {
        let kind = match request.kind.trim() {
            "" | "other" => "other",
            "movie" => "movie",
            "episode" => "episode",
            "music" => "music",
            "image" => "image",
            other => return Err(ApiError::bad_request(format!("unknown scan kind: {other}"))),
        };
        (
            std::path::PathBuf::from(request.path.trim()),
            kind.to_string(),
        )
    } else {
        match request.source.as_str() {
            "" | "movies" => (state.config.movies_root.clone(), "movie".to_string()),
            "tv" => (state.config.tv_root.clone(), "episode".to_string()),
            "music" => (state.config.music_root.clone(), "music".to_string()),
            "images" => (state.config.images_root.clone(), "image".to_string()),
            "commercials" => (state.config.commercials_root.clone(), "other".to_string()),
            other => return Err(ApiError::bad_request(format!("unknown source: {other}"))),
        }
    };
    let path_for_scan = path.clone();
    let kind_for_scan = kind.clone();
    let media = tokio::task::spawn_blocking(move || {
        scan::scan_path(&path_for_scan, request.limit, &kind_for_scan)
    })
    .await
    .map_err(|error| anyhow::anyhow!("scan task failed: {error}"))??;
    for item in &media {
        db::upsert_media(&state.pool, item).await?;
        db::replace_chapters(&state.pool, &item.id, &item.chapters).await?;
    }
    let path_label = path.display().to_string();
    Ok(Json(ScanResponse {
        scanned: media.len(),
        path: path_label,
        media,
    }))
}

#[derive(Deserialize, Default)]
struct ProbeChaptersRequest {
    #[serde(default)]
    collection_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ProbeChaptersResponse {
    probed: usize,
    chapters: usize,
    remaining: i64,
}

async fn probe_chapters_handler(
    State(state): State<AppState>,
    Json(request): Json<ProbeChaptersRequest>,
) -> Result<Json<ProbeChaptersResponse>, ApiError> {
    let limit = request.limit.unwrap_or(100).clamp(1, 1_000);
    let targets = match request.collection_id {
        Some(collection_id) => db::collection_media_paths(&state.pool, &collection_id).await?,
        None => db::media_missing_chapters(&state.pool, limit).await?,
    };
    let probed = tokio::task::spawn_blocking(move || {
        targets
            .into_iter()
            .filter_map(
                |(id, path)| match scan::probe_chapters(std::path::Path::new(&path)) {
                    Ok(chapters) => Some((id, chapters)),
                    Err(error) => {
                        tracing::warn!(%path, %error, "chapter probe failed");
                        None
                    }
                },
            )
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|error| anyhow::anyhow!("chapter probe task failed: {error}"))?;

    let probed_count = probed.len();
    let mut chapter_count = 0usize;
    for (media_id, chapters) in probed {
        chapter_count += chapters.len();
        db::replace_chapters(&state.pool, &media_id, &chapters).await?;
    }
    let remaining = db::count_missing_chapters(&state.pool).await?;
    Ok(Json(ProbeChaptersResponse {
        probed: probed_count,
        chapters: chapter_count,
        remaining,
    }))
}

async fn cloud_sources(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::CloudSource>>, ApiError> {
    Ok(Json(state.cloud.list_sources().await?))
}

async fn create_cloud_source(
    State(state): State<AppState>,
    Json(input): Json<db::CloudSourceInput>,
) -> Result<Json<db::CloudSource>, ApiError> {
    Ok(Json(state.cloud.create_source(&input).await?))
}

async fn delete_cloud_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.cloud.delete_source(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cloud_roots(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<db::CloudRoot>>, ApiError> {
    Ok(Json(db::list_cloud_roots(&state.pool, &id).await?))
}

async fn add_cloud_root(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(root): Json<db::CloudRootInput>,
) -> Result<Json<db::CloudRoot>, ApiError> {
    Ok(Json(state.cloud.add_root(&id, &root).await?))
}

async fn delete_cloud_root(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    db::delete_cloud_root(&state.pool, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default = "default_folder")]
    folder: String,
}

fn default_folder() -> String {
    "root".into()
}

async fn browse_cloud(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<Vec<crate::cloud::google::RemoteEntry>>, ApiError> {
    Ok(Json(state.cloud.browse(&id, &query.folder).await?))
}

async fn cloud_drives(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::cloud::google::DriveEntry>>, ApiError> {
    Ok(Json(state.cloud.shared_drives(&id).await?))
}

#[derive(Deserialize, Default)]
struct CloudScanRequest {
    limit: Option<usize>,
}

async fn scan_cloud(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<CloudScanRequest>,
) -> Result<Json<crate::cloud::CloudScanReport>, ApiError> {
    Ok(Json(
        state
            .cloud
            .scan_source(&id, request.limit.unwrap_or(1000))
            .await?,
    ))
}

#[derive(Deserialize)]
struct CloudProxyQuery {
    expires: i64,
    sig: String,
}

async fn cloud_proxy(
    State(state): State<AppState>,
    Path((source_id, remote_id)): Path<(String, String)>,
    Query(query): Query<CloudProxyQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !state
        .cloud
        .verify_proxy(&source_id, &remote_id, query.expires, &query.sig)
    {
        return Err(ApiError::unauthorized("invalid cloud proxy signature"));
    }
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let upstream = state
        .cloud
        .open_proxy(&source_id, &remote_id, range)
        .await
        .map_err(|error| ApiError::service_unavailable(format!("cloud proxy failed: {error:#}")))?;
    let status = StatusCode::from_u16(upstream.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .to_string();
    let content_length = upstream.header("Content-Length").map(str::to_string);
    let content_range = upstream.header("Content-Range").map(str::to_string);
    let mut reader = upstream.into_reader();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx
                        .blocking_send(Ok(axum::body::Bytes::copy_from_slice(&buffer[..read])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.blocking_send(Err(error));
                    break;
                }
            }
        }
    });
    let stream = ReceiverStream::new(rx);
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    if let Some(range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, range);
    }
    builder
        .body(Body::from_stream(stream))
        .map_err(|error| ApiError::internal(format!("build cloud proxy response: {error}")))
}

#[derive(Deserialize)]
struct CreateChannelRequest {
    number: String,
    name: String,
    media_ids: Vec<String>,
}

async fn create_channel(
    State(state): State<AppState>,
    Json(request): Json<CreateChannelRequest>,
) -> Result<Json<Channel>, ApiError> {
    if request.number.trim().is_empty() || request.name.trim().is_empty() {
        return Err(ApiError::bad_request("number and name are required"));
    }
    let channel = db::create_channel(
        &state.pool,
        request.number.trim(),
        request.name.trim(),
        &request.media_ids,
    )
    .await?;
    Ok(Json(channel))
}

async fn collections(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::CollectionSummary>>, ApiError> {
    Ok(Json(db::list_collections(&state.pool).await?))
}

#[derive(Deserialize)]
struct CreateCollectionRequest {
    name: String,
    media_ids: Vec<String>,
}

async fn create_collection(
    State(state): State<AppState>,
    Json(request): Json<CreateCollectionRequest>,
) -> Result<Json<db::CollectionSummary>, ApiError> {
    Ok(Json(
        db::create_collection(&state.pool, &request.name, &request.media_ids).await?,
    ))
}

async fn collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::CollectionDetail>, ApiError> {
    db::collection_detail(&state.pool, &id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("collection not found"))
}

async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    db::delete_collection(&state.pool, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn blocks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<db::ChannelBlock>>, ApiError> {
    Ok(Json(db::list_channel_blocks(&state.pool, &id).await?))
}

#[derive(Serialize)]
struct UpdateBlocksResponse {
    blocks: Vec<db::ChannelBlock>,
    rebuild: programming::RebuildReport,
}

async fn update_blocks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(blocks): Json<Vec<db::BlockInput>>,
) -> Result<Json<UpdateBlocksResponse>, ApiError> {
    let saved = db::set_channel_blocks(&state.pool, &id, &blocks).await?;
    db::set_schedule_mode(&state.pool, &id, "blocks").await?;
    let rebuild = programming::rebuild_channel(&state.pool, &id).await?;
    state.engine.refresh(&id).await;
    Ok(Json(UpdateBlocksResponse {
        blocks: saved,
        rebuild,
    }))
}

async fn classic(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<db::ClassicItem>>, ApiError> {
    Ok(Json(db::list_classic_items(&state.pool, &id).await?))
}

#[derive(Serialize)]
struct ClassicResponse {
    items: Vec<db::ClassicItem>,
    rebuild: programming::RebuildReport,
}

async fn update_classic(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(items): Json<Vec<db::ClassicItemInput>>,
) -> Result<Json<ClassicResponse>, ApiError> {
    let saved = db::set_classic_items(&state.pool, &id, &items).await?;
    let rebuild = programming::rebuild_channel(&state.pool, &id).await?;
    state.engine.refresh(&id).await;
    Ok(Json(ClassicResponse {
        items: saved,
        rebuild,
    }))
}

async fn schedule_overrides(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<db::ScheduleOverride>>, ApiError> {
    Ok(Json(db::list_schedule_overrides(&state.pool, &id).await?))
}

#[derive(Serialize)]
struct OverridesResponse {
    overrides: Vec<db::ScheduleOverride>,
    rebuild: programming::RebuildReport,
}

async fn create_schedule_override(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<db::ScheduleOverrideInput>,
) -> Result<Json<OverridesResponse>, ApiError> {
    scripted::parse_definition(&state.pool, &request.schedule_yaml)
        .await
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    db::create_schedule_override(&state.pool, &id, &request).await?;
    let rebuild = programming::rebuild_channel(&state.pool, &id).await?;
    state.engine.refresh(&id).await;
    Ok(Json(OverridesResponse {
        overrides: db::list_schedule_overrides(&state.pool, &id).await?,
        rebuild,
    }))
}

async fn delete_schedule_override(
    State(state): State<AppState>,
    Path((id, override_id)): Path<(String, String)>,
) -> Result<Json<OverridesResponse>, ApiError> {
    db::delete_schedule_override(&state.pool, &override_id).await?;
    let rebuild = programming::rebuild_channel(&state.pool, &id).await?;
    state.engine.refresh(&id).await;
    Ok(Json(OverridesResponse {
        overrides: db::list_schedule_overrides(&state.pool, &id).await?,
        rebuild,
    }))
}

async fn channel_graphics(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::GraphicsSettings>, ApiError> {
    Ok(Json(db::channel_graphics(&state.pool, &id).await?))
}

async fn update_channel_graphics(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(graphics): Json<db::GraphicsSettings>,
) -> Result<Json<db::GraphicsSettings>, ApiError> {
    validate_graphics(&state, &graphics).await?;
    db::set_channel_graphics(&state.pool, &id, &graphics).await?;
    state.engine.refresh(&id).await;
    Ok(Json(graphics))
}

async fn channel_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::StreamSettings>, ApiError> {
    Ok(Json(db::channel_stream(&state.pool, &id).await?))
}

async fn update_channel_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(stream): Json<db::StreamSettings>,
) -> Result<Json<db::StreamSettings>, ApiError> {
    validate_stream(&stream)?;
    db::set_channel_stream(&state.pool, &id, &stream).await?;
    state.engine.refresh(&id).await;
    if stream.prewarm
        && let Some(channel) = db::channel_by_id(&state.pool, &id).await?
    {
        state.engine.ensure(&channel).await?;
    }
    Ok(Json(stream))
}

async fn validate_graphics(
    state: &AppState,
    graphics: &db::GraphicsSettings,
) -> Result<(), ApiError> {
    if !matches!(
        graphics.position.as_str(),
        "top-left" | "top-right" | "bottom-left" | "bottom-right"
    ) {
        return Err(ApiError::bad_request(
            "graphics position must be top-left, top-right, bottom-left, or bottom-right",
        ));
    }
    if !(0..=100).contains(&graphics.opacity) {
        return Err(ApiError::bad_request("graphics opacity must be 0-100"));
    }
    if !(8..=96).contains(&graphics.size) {
        return Err(ApiError::bad_request("graphics size must be 8-96"));
    }
    if graphics.text.chars().count() > 80 {
        return Err(ApiError::bad_request(
            "graphics text is limited to 80 characters",
        ));
    }
    if !matches!(
        graphics.watermark_position.as_str(),
        "top-left" | "top-right" | "bottom-left" | "bottom-right"
    ) {
        return Err(ApiError::bad_request(
            "watermark position must be top-left, top-right, bottom-left, or bottom-right",
        ));
    }
    if !(0..=100).contains(&graphics.watermark_opacity) {
        return Err(ApiError::bad_request("watermark opacity must be 0-100"));
    }
    if !(3..=50).contains(&graphics.watermark_scale) {
        return Err(ApiError::bad_request("watermark scale must be 3-50"));
    }
    if !(8..=96).contains(&graphics.lower_third_size) {
        return Err(ApiError::bad_request("lower third size must be 8-96"));
    }
    if !(0..=100).contains(&graphics.lower_third_opacity) {
        return Err(ApiError::bad_request("lower third opacity must be 0-100"));
    }
    if graphics.lower_third_text.chars().count() > 120 {
        return Err(ApiError::bad_request(
            "lower third text is limited to 120 characters",
        ));
    }
    if let Some(media_id) = graphics
        .watermark_media_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        match db::media_kind(&state.pool, media_id).await? {
            Some(kind) if kind == "image" => {}
            Some(_) => {
                return Err(ApiError::bad_request(
                    "watermark media must be an image from the catalog",
                ));
            }
            None => return Err(ApiError::bad_request("watermark media not found")),
        }
    }
    Ok(())
}

fn validate_stream(stream: &db::StreamSettings) -> Result<(), ApiError> {
    if !(320..=3840).contains(&stream.width) || !(240..=2160).contains(&stream.height) {
        return Err(ApiError::bad_request(
            "stream width must be 320-3840 and height 240-2160",
        ));
    }
    if !matches!(
        stream.preset.as_str(),
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "medium" | "slow"
    ) {
        return Err(ApiError::bad_request("unknown x264 preset"));
    }
    if !(16..=32).contains(&stream.crf) {
        return Err(ApiError::bad_request("stream crf must be 16-32"));
    }
    if !(64..=512).contains(&stream.audio_kbps) {
        return Err(ApiError::bad_request("stream audio_kbps must be 64-512"));
    }
    if !matches!(stream.subtitle_mode.as_str(), "off" | "burn" | "inherit") {
        return Err(ApiError::bad_request(
            "subtitle mode must be off, burn, or inherit",
        ));
    }
    if !matches!(stream.hdr_mode.as_str(), "passthrough" | "tonemap") {
        return Err(ApiError::bad_request(
            "hdr mode must be passthrough or tonemap",
        ));
    }
    if !matches!(stream.encoder.as_str(), "software" | "vaapi") {
        return Err(ApiError::bad_request("encoder must be software or vaapi"));
    }
    if stream.encoder == "vaapi" && !std::path::Path::new("/dev/dri/renderD128").exists() {
        return Err(ApiError::bad_request(
            "VAAPI device /dev/dri/renderD128 is not available",
        ));
    }
    for language in [&stream.audio_language, &stream.subtitle_language]
        .into_iter()
        .flatten()
    {
        if language.chars().count() > 16 {
            return Err(ApiError::bad_request(
                "language codes are limited to 16 characters",
            ));
        }
    }
    Ok(())
}

async fn block_templates(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::BlockTemplateSummary>>, ApiError> {
    Ok(Json(db::list_block_templates(&state.pool).await?))
}

#[derive(Deserialize)]
struct CreateTemplateRequest {
    name: String,
    #[serde(default)]
    blocks: Vec<db::BlockInput>,
    #[serde(default)]
    graphics: db::GraphicsSettings,
}

async fn create_block_template(
    State(state): State<AppState>,
    Json(request): Json<CreateTemplateRequest>,
) -> Result<Json<db::BlockTemplate>, ApiError> {
    validate_graphics(&state, &request.graphics).await?;
    Ok(Json(
        db::create_block_template(
            &state.pool,
            &request.name,
            &request.blocks,
            &request.graphics,
        )
        .await?,
    ))
}

async fn block_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::BlockTemplate>, ApiError> {
    db::block_template(&state.pool, &id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("template not found"))
}

async fn delete_block_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    db::delete_block_template(&state.pool, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ApplyTemplateRequest {
    template_id: String,
}

#[derive(Serialize)]
struct ApplyTemplateResponse {
    blocks: Vec<db::ChannelBlock>,
    rebuild: programming::RebuildReport,
}

async fn apply_block_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ApplyTemplateRequest>,
) -> Result<Json<ApplyTemplateResponse>, ApiError> {
    let template = db::block_template(&state.pool, &request.template_id)
        .await?
        .ok_or_else(|| ApiError::not_found("template not found"))?;
    let blocks = db::set_channel_blocks(&state.pool, &id, &template.blocks).await?;
    db::set_schedule_mode(&state.pool, &id, "blocks").await?;
    db::set_channel_graphics(&state.pool, &id, &template.graphics).await?;
    state.engine.refresh(&id).await;
    let rebuild = programming::rebuild_channel(&state.pool, &id).await?;
    Ok(Json(ApplyTemplateResponse { blocks, rebuild }))
}

async fn export_schedule_yaml(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let yaml = scripted::export(&state.pool, &id).await?;
    Ok((
        [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
        yaml,
    )
        .into_response())
}

async fn export_playout_json(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let json = external::export(&state.pool, &id).await?;
    Ok((
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        json,
    )
        .into_response())
}

#[derive(Serialize)]
struct PlayoutJsonResponse {
    items: usize,
    start_ms: i64,
    end_ms: i64,
}

async fn apply_playout_json(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: String,
) -> Result<Json<PlayoutJsonResponse>, ApiError> {
    if body.trim().is_empty() {
        return Err(ApiError::bad_request("playout JSON is empty"));
    }
    let applied = external::apply(&state.pool, &id, &body)
        .await
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    state.engine.refresh(&id).await;
    Ok(Json(PlayoutJsonResponse {
        items: applied.items,
        start_ms: applied.start_ms,
        end_ms: applied.end_ms,
    }))
}

#[derive(Serialize)]
struct ScheduleYamlResponse {
    mode: String,
    rebuild: programming::RebuildReport,
}

async fn apply_schedule_yaml(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: String,
) -> Result<Json<ScheduleYamlResponse>, ApiError> {
    if body.trim().is_empty() {
        return Err(ApiError::bad_request("schedule YAML is empty"));
    }
    let applied = scripted::apply(&state.pool, &id, &body)
        .await
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    state.engine.refresh(&id).await;
    Ok(Json(ScheduleYamlResponse {
        mode: applied.mode,
        rebuild: applied.rebuild,
    }))
}

async fn rebuild(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<programming::RebuildReport>, ApiError> {
    let rebuild = programming::rebuild_channel(&state.pool, &id).await?;
    state.engine.refresh(&id).await;
    Ok(Json(rebuild))
}

async fn backups(State(state): State<AppState>) -> Result<Json<Vec<backup::BackupInfo>>, ApiError> {
    Ok(Json(backup::list(&state.config.data_dir).await?))
}

async fn create_backup(
    State(state): State<AppState>,
) -> Result<Json<backup::BackupInfo>, ApiError> {
    Ok(Json(
        backup::create(&state.pool, &state.config.data_dir).await?,
    ))
}

async fn download_backup(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let path = backup::backup_path(&state.config.data_dir, &name)
        .ok_or_else(|| ApiError::bad_request("invalid backup name"))?;
    let body = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("backup not found"))?;
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.sqlite3"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .map_err(|error| ApiError::internal(format!("header: {error}")))?,
    );
    Ok(response)
}

#[derive(Deserialize)]
struct RestoreRequest {
    name: String,
}

async fn restore(
    State(state): State<AppState>,
    Json(request): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    backup::stage_restore(&state.config.data_dir, &request.name).await?;
    Ok(Json(serde_json::json!({
        "staged": request.name,
        "restart_required": true
    })))
}

async fn channels(State(state): State<AppState>) -> Result<Json<Vec<db::ChannelView>>, ApiError> {
    Ok(Json(db::list_channel_views(&state.pool).await?))
}

async fn now_playing(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Option<db::ChannelItem>>, ApiError> {
    Ok(Json(
        db::current_item(&state.pool, &id, db::now_ms()).await?,
    ))
}

#[derive(Deserialize)]
struct GuideQuery {
    #[serde(default = "default_hours")]
    hours: i64,
}

fn default_hours() -> i64 {
    12
}

async fn guide(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<GuideQuery>,
) -> Result<Json<Vec<db::ChannelItem>>, ApiError> {
    let now = db::now_ms();
    let hours = query.hours.clamp(1, 72);
    Ok(Json(
        db::guide_items(&state.pool, &id, now - 3_600_000, now + hours * 3_600_000).await?,
    ))
}

async fn m3u(State(state): State<AppState>) -> Result<Response, ApiError> {
    let channels = db::list_channels(&state.pool).await?;
    let mut body = format!(
        "#EXTM3U x-tvg-url=\"{}/iptv/xmltv.xml\"\n",
        state.config.public_url.trim_end_matches('/')
    );
    for channel in channels {
        body.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"awesome.{number}\" tvg-chno=\"{number}\" tvg-name=\"{name}\",{name}\n{base}/channel/{number}.m3u8\n",
            number = channel.number,
            name = channel.name,
            base = state.config.public_url.trim_end_matches('/')
        ));
    }
    Ok(([(header::CONTENT_TYPE, "application/x-mpegURL")], body).into_response())
}

async fn xmltv(State(state): State<AppState>) -> Result<Response, ApiError> {
    let channels = db::list_channels(&state.pool).await?;
    let now = db::now_ms();
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv>\n");
    for channel in &channels {
        body.push_str(&format!(
            "  <channel id=\"awesome.{}\"><display-name>{}</display-name></channel>\n",
            channel.number,
            escape_xml(&channel.name)
        ));
    }
    for channel in &channels {
        let items = db::guide_items(
            &state.pool,
            &channel.id,
            now - 3_600_000,
            now + 24 * 3_600_000,
        )
        .await?;
        for item in items {
            body.push_str(&format!(
                "  <programme start=\"{}\" stop=\"{}\" channel=\"awesome.{}\"><title>{}</title></programme>\n",
                xmltv_time(item.start_ms),
                xmltv_time(item.end_ms),
                channel.number,
                escape_xml(&item.title)
            ));
        }
    }
    body.push_str("</tv>\n");
    Ok(([(header::CONTENT_TYPE, "application/xml")], body).into_response())
}

#[derive(Serialize)]
struct CastResponse {
    media_url: String,
    content_type: String,
    title: String,
    channel: String,
}

fn cast_response(state: &AppState, channel: &Channel) -> CastResponse {
    let now = db::now_ms() / 1000;
    let expires = now + 4 * 60 * 60;
    let token = state
        .cloud
        .sign(format!("cast:{}:{expires}", channel.number).as_bytes());
    let media_url = format!(
        "{}/cast/{}.m3u8?expires={expires}&token={token}",
        cast_base(&state.config),
        channel.number
    );
    CastResponse {
        media_url,
        content_type: "application/vnd.apple.mpegurl".into(),
        title: channel.name.clone(),
        channel: channel.number.clone(),
    }
}

async fn start_cast(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CastResponse>, ApiError> {
    let channel = db::channel_by_id(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    // Warm the producer before handing the URL to the receiver; a cold start
    // can otherwise exceed the Cast media load timeout.
    state.engine.ensure(&channel).await?;
    tracing::info!(channel = %channel.number, base = %cast_base(&state.config), "admin cast requested");
    Ok(Json(cast_response(&state, &channel)))
}

/// Public cast discovery. The returned URL is signed and short-lived, so this
/// is equivalent to handing out the public HLS stream for one channel.
async fn public_cast(
    State(state): State<AppState>,
    Path(number): Path<String>,
) -> Result<Json<CastResponse>, ApiError> {
    let channel = db::channel_by_number(&state.pool, &number)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    state.engine.ensure(&channel).await?;
    tracing::info!(channel = %channel.number, base = %cast_base(&state.config), "public cast requested");
    Ok(Json(cast_response(&state, &channel)))
}

#[derive(Deserialize)]
struct CastQuery {
    expires: i64,
    token: String,
}

fn cast_base(config: &Config) -> String {
    let value = if config.cast_base_url.trim().is_empty() {
        config.public_url.trim()
    } else {
        config.cast_base_url.trim()
    };
    value.trim_end_matches('/').to_string()
}

fn cast_token_valid(state: &AppState, number: &str, expires: i64, token: &str) -> bool {
    expires >= db::now_ms() / 1000
        && state
            .cloud
            .verify(format!("cast:{number}:{expires}").as_bytes(), token)
}

async fn cast_playlist(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(query): Query<CastQuery>,
) -> Result<Response, ApiError> {
    let number = file
        .strip_suffix(".m3u8")
        .ok_or_else(|| ApiError::not_found("expected a .m3u8 playlist"))?;
    if !cast_token_valid(&state, number, query.expires, &query.token) {
        return Err(ApiError::unauthorized("invalid cast token"));
    }
    let channel = db::channel_by_number(&state.pool, number)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    let playlist = playlist_for_channel(&state, &channel).await?;
    let body = tokio::fs::read_to_string(&playlist).await?;
    let base = cast_base(&state.config);
    let body = body
        .lines()
        .map(|line| {
            if line.ends_with(".ts") {
                format!(
                    "{base}/cast/{number}/{line}?expires={}&token={}",
                    query.expires, query.token
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

async fn cast_segment(
    State(state): State<AppState>,
    Path((number, file)): Path<(String, String)>,
    Query(query): Query<CastQuery>,
) -> Result<Response, ApiError> {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err(ApiError::bad_request("invalid segment name"));
    }
    if !cast_token_valid(&state, &number, query.expires, &query.token) {
        return Err(ApiError::unauthorized("invalid cast token"));
    }
    let path = state
        .config
        .data_dir
        .join("hls")
        .join(number.replace('.', "_"))
        .join(file);
    let body = tokio::fs::read(&path).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "max-age=30"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

#[derive(Deserialize, Default)]
struct DvrRequest {
    window_seconds: Option<i64>,
}

#[derive(Serialize)]
struct DvrResponse {
    playlist_url: String,
    start_ms: i64,
    window_seconds: i64,
}

async fn start_dvr(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<DvrRequest>,
) -> Result<Json<DvrResponse>, ApiError> {
    let channel = db::channel_by_id(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    let info = state
        .engine
        .start_dvr(&channel, request.window_seconds.unwrap_or(3600))
        .await?;
    Ok(Json(DvrResponse {
        playlist_url: info.playlist_url,
        start_ms: info.start_ms,
        window_seconds: info.window_seconds,
    }))
}

async fn stop_dvr_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if let Some(channel) = db::channel_by_id(&state.pool, &id).await? {
        state.engine.stop_dvr(&channel.number).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn dvr_playlist(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let number = file
        .strip_suffix(".m3u8")
        .ok_or_else(|| ApiError::not_found("expected a .m3u8 playlist"))?;
    let body = state.engine.dvr_playlist(number).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

async fn dvr_segment(
    State(state): State<AppState>,
    Path((number, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let path = state.engine.dvr_segment_path(&number, &file).await?;
    let body = tokio::fs::read(&path).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "max-age=30"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

#[derive(Serialize)]
struct CatchupResponse {
    session: String,
    title: String,
    start_ms: i64,
    end_ms: i64,
    playlist_url: String,
}

async fn start_catchup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CatchupResponse>, ApiError> {
    let channel = db::channel_by_id(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    let info = state.engine.start_catchup(&channel).await?;
    Ok(Json(CatchupResponse {
        playlist_url: format!("/catchup/{}.m3u8", info.session),
        session: info.session,
        title: info.title,
        start_ms: info.start_ms,
        end_ms: info.end_ms,
    }))
}

async fn catchup_playlist(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let session = file
        .strip_suffix(".m3u8")
        .ok_or_else(|| ApiError::not_found("expected a .m3u8 playlist"))?;
    let body = state.engine.catchup_playlist(session).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

async fn catchup_segment(
    State(state): State<AppState>,
    Path((session, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let path = state.engine.catchup_segment_path(&session, &file).await?;
    let body = tokio::fs::read(&path).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "max-age=30"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

fn playlist_is_fresh(path: &std::path::Path, max_age: Duration) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    if age > max_age {
        return false;
    }
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    body.lines().any(|line| line.ends_with(".ts"))
}

/// Return the current playlist immediately when it is live, and warm the next
/// producer in the background. This keeps a receiver's playlist requests from
/// blocking on a slow probe at an item boundary.
async fn playlist_for_channel(
    state: &AppState,
    channel: &Channel,
) -> Result<std::path::PathBuf, ApiError> {
    let path = state
        .config
        .data_dir
        .join("hls")
        .join(channel.number.replace('.', "_"))
        .join("index.m3u8");
    if playlist_is_fresh(&path, Duration::from_secs(15)) {
        let engine = state.engine.clone();
        let channel = channel.clone();
        let number = channel.number.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.ensure(&channel).await {
                tracing::warn!(%error, channel = %number, "background playlist ensure failed");
            }
        });
        return Ok(path);
    }
    state
        .engine
        .ensure(channel)
        .await
        .map_err(|error| ApiError::service_unavailable(format!("{error:#}")))
}

async fn hls_playlist(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let number = file
        .strip_suffix(".m3u8")
        .ok_or_else(|| ApiError::not_found("expected a .m3u8 playlist"))?;
    let channel = db::channel_by_number(&state.pool, number)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    let playlist = playlist_for_channel(&state, &channel).await?;
    let body = tokio::fs::read_to_string(&playlist).await?;
    // The producer writes relative segment names. Prefix them with the channel
    // number so they resolve under /channel/{number}/{file}.
    let body = body
        .lines()
        .map(|line| {
            if line.ends_with(".ts") {
                format!("{number}/{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

async fn hls_segment(
    State(state): State<AppState>,
    Path((number, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err(ApiError::bad_request("invalid segment name"));
    }
    let channel = db::channel_by_number(&state.pool, &number)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    let path = state
        .config
        .data_dir
        .join("hls")
        .join(channel.number.replace('.', "_"))
        .join(file);
    let body = tokio::fs::read(&path).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "max-age=30"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response())
}

async fn discover(State(state): State<AppState>) -> Response {
    let base = state.config.public_url.trim_end_matches('/');
    Json(serde_json::json!({
        "FriendlyName": "Awesome TV",
        "ModelNumber": "HDHR-AWESOME",
        "FirmwareName": "awesometv",
        "FirmwareVersion": "0.2.0",
        "DeviceID": "A11CE001",
        "DeviceAuth": "awesometv",
        "BaseURL": base,
        "LineupURL": format!("{base}/lineup.json"),
        "TunerCount": 4
    }))
    .into_response()
}

async fn lineup_status() -> Response {
    Json(serde_json::json!({
        "ScanInProgress": 0,
        "ScanPossible": 1,
        "Source": "Cable",
        "SourceList": ["Cable"]
    }))
    .into_response()
}

async fn hdhr_lineup(State(state): State<AppState>) -> Result<Response, ApiError> {
    let channels = db::list_channels(&state.pool).await?;
    let base = state.config.public_url.trim_end_matches('/');
    let items: Vec<_> = channels
        .into_iter()
        .map(|channel| {
            serde_json::json!({
                "GuideNumber": channel.number,
                "GuideName": channel.name,
                "URL": format!("{base}/auto/v{}.ts", channel.number)
            })
        })
        .collect();
    Ok(Json(items).into_response())
}

async fn device_xml(State(state): State<AppState>) -> Response {
    let base = state.config.public_url.trim_end_matches('/');
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <URLBase>{base}</URLBase>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>Awesome TV</friendlyName>
    <manufacturer>Silicondust</manufacturer>
    <modelName>HDHR-AWESOME</modelName>
    <modelNumber>HDHR-AWESOME</modelNumber>
    <tunerCount>4</tunerCount>
    <serialNumber>A11CE001</serialNumber>
    <UDN>uuid:A11CE001</UDN>
  </device>
</root>"#
    );
    ([(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

async fn ts_stream(
    State(state): State<AppState>,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let file = params
        .get("file")
        .ok_or_else(|| ApiError::bad_request("missing channel"))?;
    let base = file.strip_suffix(".ts").unwrap_or(file);
    let number = base.strip_prefix('v').unwrap_or(base);
    let channel = db::channel_by_number(&state.pool, number)
        .await?
        .ok_or_else(|| ApiError::not_found("channel not found"))?;
    state
        .engine
        .ensure(&channel)
        .await
        .map_err(|error| ApiError::service_unavailable(format!("{error:#}")))?;
    let hls_url = format!(
        "{}/channel/{}.m3u8",
        state.config.public_url.trim_end_matches('/'),
        channel.number
    );
    let mut child = tokio::process::Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-i")
        .arg(&hls_url)
        .arg("-c")
        .arg("copy")
        .arg("-f")
        .arg("mpegts")
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ApiError::service_unavailable(format!("start TS muxer: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::service_unavailable("TS muxer has no output"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(ReaderStream::new(stdout)),
    )
        .into_response())
}

async fn request_is_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let basic_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|credentials| {
            credentials
                == format!(
                    "{}:{}",
                    state.config.admin_user, state.config.admin_password
                )
        });

    let session_ok = if let Some(token) = session_token(headers) {
        let mut sessions = state.sessions.lock().await;
        match sessions.get(&token) {
            Some(session) if session.expires_ms > db::now_ms() => true,
            Some(_) => {
                sessions.remove(&token);
                false
            }
            None => false,
        }
    } else {
        false
    };

    basic_ok || session_ok
}

async fn basic_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request_is_authorized(&state, request.headers()).await {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "Authentication required\n").into_response()
    }
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                cookie
                    .trim()
                    .strip_prefix("awesometv_session=")
                    .map(str::to_string)
            })
        })
}

fn xmltv_time(ms: i64) -> String {
    let value =
        OffsetDateTime::from_unix_timestamp(ms / 1000).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02} +0000",
        value.year(),
        value.month() as u8,
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    )
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{:#}", error.into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
