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
        }
    }
}
