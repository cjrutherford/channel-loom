mod api;
mod backup;
mod cloud;
mod config;
mod db;
mod engine;
mod external;
mod jellyfin;
mod programming;
mod scan;
mod scripted;

use std::sync::Arc;

use anyhow::Context;

use crate::api::{AppState, LoginThrottle};
use crate::config::Config;
use crate::engine::Engine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "awesometv_server=info,tower_http=info".into()),
        )
        .init();

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
    let cloud = Arc::new(cloud::CloudManager::new(
        pool.clone(),
        config.data_dir.clone(),
        &config.bind,
        config.port,
    )?);
    let engine = Arc::new(Engine::new(
        pool.clone(),
        config.data_dir.clone(),
        cloud.clone(),
    ));
    let state = AppState {
        config: config.clone(),
        pool,
        engine: engine.clone(),
        cloud: cloud.clone(),
        sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        login_throttle: Arc::new(tokio::sync::Mutex::new(LoginThrottle::default())),
    };
    let prewarm = engine.clone();
    tokio::spawn(async move {
        prewarm.prewarm().await;
    });
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind((config.bind.as_str(), config.port))
        .await
        .with_context(|| format!("bind {}:{}", config.bind, config.port))?;

    println!();
    println!("Awesome TV");
    println!("  UI:       {}", config.public_url);
    println!("  M3U:      {}/iptv/channels.m3u", config.public_url);
    println!("  XMLTV:    {}/iptv/xmltv.xml", config.public_url);
    println!("  Jellyfin: {}", config.jellyfin_url);
    println!("  Admin:    {}", config.admin_user);
    if config.generated_password {
        println!(
            "  Password: {}  (generated; set AWESOMETV_ADMIN_PASSWORD to override)",
            config.admin_password
        );
    }
    println!();

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
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
