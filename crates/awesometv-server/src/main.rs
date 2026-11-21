mod api;
mod backup;
mod cloud;
mod config;
mod db;
mod engine;
mod external;
mod jellyfin;
mod limits;
mod oidc;
mod precache;
mod programming;
mod remote_refresh;
mod renewal;
mod scan;
mod scripted;
mod tokens;

use std::sync::Arc;

use anyhow::Context;

use crate::api::{AppState, LoginThrottle};
use crate::config::Config;
use crate::engine::Engine;
use crate::limits::{MediaRateLimiter, RateLimits, StreamLimiter, TokenRateLimiter, parse_cidrs};
use crate::precache::Precache;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "awesometv_server=info,tower_http=info".into()),
        )
        .init();

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    anyhow::ensure!(
        args.is_empty() || args == ["--migrate-only"],
        "usage: awesometv-server [--migrate-only]"
    );
    let migrate_only = !args.is_empty();
    let config = Config::from_env();
    let pool = db::init(&config.data_dir).await?;
    db::ensure_app_setting(
        &pool,
        "default_audio_language",
        &config.default_audio_language,
    )
    .await?;
    db::ensure_app_setting(
        &pool,
        "default_subtitle_mode",
        &config.default_subtitle_mode,
    )
    .await?;
    db::ensure_app_setting(
        &pool,
        "default_subtitle_language",
        &config.default_subtitle_language,
    )
    .await?;
    if migrate_only {
        pool.close().await;
        tracing::info!("database migration and schema initialization complete");
        return Ok(());
    }
    let cloud = Arc::new(cloud::CloudManager::new(
        pool.clone(),
        config.data_dir.clone(),
        &config.bind,
        config.port,
    )?);
    let precache = Arc::new(Precache::new(&config, pool.clone()));
    let engine = Arc::new(
        Engine::new(
            pool.clone(),
            config.data_dir.clone(),
            cloud.clone(),
            config.stream_max_kbps,
        )
        .with_precache(precache.clone()),
    );
    if let Ok(rotated) = cloud.rotate_secrets().await
        && rotated > 0
    {
        tracing::info!(rotated, "re-encrypted cloud secrets with the primary key");
    }
    precache.start();
    let admin_nets = parse_cidrs(&config.admin_allow_cidrs)?;
    let oidc = crate::oidc::OidcClient::from_env()?;
    let state = AppState {
        oidc,
        oidc_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        config: config.clone(),
        admin_nets,
        pool,
        engine: engine.clone(),
        cloud: cloud.clone(),
        sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        login_throttle: Arc::new(tokio::sync::Mutex::new(LoginThrottle::default())),
        token_limits: Arc::new(tokio::sync::Mutex::new(TokenRateLimiter::new(
            config.token_rate_per_min,
        ))),
        media_limits: Arc::new(tokio::sync::Mutex::new(MediaRateLimiter::new(RateLimits {
            enabled: config.rate_limit_enabled,
            api_per_min: config.rate_limit_api_per_min,
            cast_per_min: config.rate_limit_cast_per_min,
            playlist_per_min: config.rate_limit_playlist_per_min,
            segment_per_min: config.rate_limit_segment_per_min,
            stream_per_min: config.rate_limit_stream_per_min,
        }))),
        streams: Arc::new(StreamLimiter::new(
            config.max_concurrent_streams as usize,
            config.max_streams_per_client as usize,
        )),
        precache: precache.clone(),
    };
    let prewarm = engine.clone();
    tokio::spawn(async move {
        prewarm.prewarm().await;
    });
    if config.producer_max_lag_seconds > 0 {
        let guard = engine.clone();
        let max_lag_ms = config.producer_max_lag_seconds * 1000;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let restarted = guard.resync_lagging(max_lag_ms).await;
                if restarted > 0 {
                    tracing::info!(restarted, "producer lag guard restarted channels");
                }
            }
        });
    }
    let (refresh_stop, refresh_task) =
        remote_refresh::start(state.pool.clone(), config.clone(), cloud.clone());
    let state_pool_for_renewal = state.pool.clone();
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind((config.bind.as_str(), config.port))
        .await
        .with_context(|| format!("bind {}:{}", config.bind, config.port))?;

    println!();
    println!("ChannelLoom");
    println!("  UI:       {}", config.public_url);
    println!("  M3U:      {}/iptv/channels.m3u", config.public_url);
    println!("  XMLTV:    {}/iptv/xmltv.xml", config.public_url);
    println!("  Jellyfin: {}", config.jellyfin_url);
    println!("  Studio:   {}", config.admin_user);
    if config.generated_password {
        println!(
            "  Password: {}  (generated; set AWESOMETV_ADMIN_PASSWORD to override)",
            config.admin_password
        );
    }
    println!();

    let (renewal_stop, renewal_task) = renewal::start(state_pool_for_renewal);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        let _ = refresh_stop.send(true);
        let _ = refresh_task.await;
        let _ = renewal_stop.send(true);
        if let Err(error) = renewal_task.await {
            tracing::error!(%error, "renewal worker stopped unexpectedly");
        }
        engine.stop_all().await;
    })
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
