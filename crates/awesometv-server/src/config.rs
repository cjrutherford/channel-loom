use std::env;
use std::path::PathBuf;

use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub public_url: String,
    pub movies_root: PathBuf,
    pub tv_root: PathBuf,
    pub music_root: PathBuf,
    pub images_root: PathBuf,
    pub commercials_root: PathBuf,
    pub admin_user: String,
    pub admin_password: String,
    pub generated_password: bool,
    pub jellyfin_url: String,
    pub emby_url: String,
    pub emby_api_key: String,
    pub default_audio_language: String,
    pub default_subtitle_mode: String,
    pub default_subtitle_language: String,
    pub cast_base_url: String,
    pub cast_receiver_id: String,
    pub stream_max_kbps: i64,
    pub precache_enabled: bool,
    pub precache_dir: PathBuf,
    pub precache_max_gb: i64,
    pub precache_ahead_hours: i64,
    pub precache_proxy_kbps: i64,
    pub precache_proxy_height: i64,
    pub precache_copy_mbps: i64,
    pub producer_max_lag_seconds: i64,
    pub rate_limit_enabled: bool,
    pub rate_limit_api_per_min: u32,
    pub rate_limit_cast_per_min: u32,
    pub rate_limit_playlist_per_min: u32,
    pub rate_limit_segment_per_min: u32,
    pub rate_limit_stream_per_min: u32,
    pub max_concurrent_streams: u32,
    pub max_streams_per_client: u32,
    pub admin_allow_cidrs: Vec<String>,
    pub token_rate_per_min: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let bind = env::var("AWESOMETV_BIND").unwrap_or_else(|_| "100.120.253.100".into());
        let port = env::var("AWESOMETV_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8409);
        let data_dir = env::var("AWESOMETV_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data"));
        let public_url =
            env::var("AWESOMETV_PUBLIC_URL").unwrap_or_else(|_| format!("http://{bind}:{port}"));
        let media_root = env::var("AWESOMETV_MEDIA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/mnt/valhalla/media"));
        let movies_root = env::var("AWESOMETV_MOVIES_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| media_root.join("Movies"));
        let tv_root = env::var("AWESOMETV_TV_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| media_root.join("Tv"));
        let music_root = env::var("AWESOMETV_MUSIC_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| media_root.join("music"));
        let images_root = env::var("AWESOMETV_IMAGES_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| media_root.join("images"));
        let commercials_root = env::var("AWESOMETV_COMMERCIALS_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| media_root.join("commercials"));
        let admin_user = env::var("AWESOMETV_ADMIN_USER").unwrap_or_else(|_| "admin".into());
        let (admin_password, generated_password) = match env::var("AWESOMETV_ADMIN_PASSWORD") {
            Ok(value) if !value.is_empty() => (value, false),
            _ => (Uuid::new_v4().simple().to_string()[..16].to_string(), true),
        };
        let jellyfin_url = env::var("JELLYFIN_URL")
            .unwrap_or_else(|_| "https://jelly.christopherrutherford.net".into());
        let emby_url = env::var("EMBY_URL").unwrap_or_else(|_| "http://localhost:8096".into());
        let emby_api_key = env::var("EMBY_API_KEY").unwrap_or_default();
        let default_audio_language =
            env::var("AWESOMETV_AUDIO_LANGUAGE").unwrap_or_else(|_| "eng".into());
        let default_subtitle_mode =
            env::var("AWESOMETV_SUBTITLE_MODE").unwrap_or_else(|_| "off".into());
        let default_subtitle_language = env::var("AWESOMETV_SUBTITLE_LANGUAGE").unwrap_or_default();
        let cast_base_url = env::var("AWESOMETV_CAST_BASE_URL").unwrap_or_default();
        let cast_receiver_id = env::var("AWESOMETV_CAST_RECEIVER_ID")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "CC1AD845".into());
        let stream_max_kbps = env::var("AWESOMETV_STREAM_MAX_KBPS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let precache_enabled = env::var("AWESOMETV_PRECACHE_ENABLED")
            .map(|value| !matches!(value.trim(), "0" | "false" | "no"))
            .unwrap_or(true);
        let precache_dir = env::var("AWESOMETV_PRECACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("precache"));
        let precache_max_gb = env::var("AWESOMETV_PRECACHE_MAX_GB")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(100);
        let precache_ahead_hours = env::var("AWESOMETV_PRECACHE_AHEAD_HOURS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(24);
        let precache_proxy_kbps = env::var("AWESOMETV_PRECACHE_PROXY_KBPS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4000);
        let precache_proxy_height = env::var("AWESOMETV_PRECACHE_PROXY_HEIGHT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(720);
        let precache_copy_mbps = env::var("AWESOMETV_PRECACHE_COPY_MBPS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(7);
        let producer_max_lag_seconds = env::var("AWESOMETV_PRODUCER_MAX_LAG_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let rate_limit_enabled = env::var("AWESOMETV_RATE_LIMIT_ENABLED")
            .map(|value| !matches!(value.trim(), "0" | "false" | "no"))
            .unwrap_or(true);
        let rate_limit_api_per_min = env_u32("AWESOMETV_RATE_LIMIT_API_PER_MIN", 120);
        let rate_limit_cast_per_min = env_u32("AWESOMETV_RATE_LIMIT_CAST_PER_MIN", 20);
        let rate_limit_playlist_per_min = env_u32("AWESOMETV_RATE_LIMIT_PLAYLIST_PER_MIN", 120);
        let rate_limit_segment_per_min = env_u32("AWESOMETV_RATE_LIMIT_SEGMENT_PER_MIN", 1_800);
        let rate_limit_stream_per_min = env_u32("AWESOMETV_RATE_LIMIT_STREAM_PER_MIN", 12);
        let max_concurrent_streams = env_u32("AWESOMETV_MAX_CONCURRENT_STREAMS", 4).max(1);
        let max_streams_per_client = env_u32("AWESOMETV_MAX_STREAMS_PER_CLIENT", 2);
        let token_rate_per_min = env_u32("AWESOMETV_TOKEN_RATE_PER_MIN", 600);
        let admin_allow_cidrs = env::var("AWESOMETV_ADMIN_ALLOW_CIDRS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();

        Self {
            bind,
            port,
            data_dir,
            public_url,
            movies_root,
            tv_root,
            music_root,
            images_root,
            commercials_root,
            admin_user,
            admin_password,
            generated_password,
            jellyfin_url,
            emby_url,
            emby_api_key,
            default_audio_language,
            default_subtitle_mode,
            default_subtitle_language,
            cast_base_url,
            cast_receiver_id,
            stream_max_kbps,
            precache_enabled,
            precache_dir,
            precache_max_gb,
            precache_ahead_hours,
            precache_proxy_kbps,
            precache_proxy_height,
            precache_copy_mbps,
            producer_max_lag_seconds,
            rate_limit_enabled,
            rate_limit_api_per_min,
            rate_limit_cast_per_min,
            rate_limit_playlist_per_min,
            rate_limit_segment_per_min,
            rate_limit_stream_per_min,
            max_concurrent_streams,
            max_streams_per_client,
            admin_allow_cidrs,
            token_rate_per_min,
        }
    }
}

fn env_u32(name: &str, fallback: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(fallback)
}
