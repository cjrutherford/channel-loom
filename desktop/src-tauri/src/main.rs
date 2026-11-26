#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const SERVER_FILE: &str = "server-url";
const CHANGE_SERVER: &str = "change-server";

/// Accepts "host", "host:port" or a full http(s) URL.
fn normalize_server_url(input: &str) -> Result<url::Url, String> {
    let value = input.trim();
    if value.is_empty() {
        return Err("Enter your server's address.".into());
    }
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let parsed =
        url::Url::parse(&value).map_err(|error| format!("Not a valid address: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().unwrap_or("").is_empty() {
        return Err("Enter an http:// or https:// address.".into());
    }
    Ok(parsed)
}

fn server_file(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(SERVER_FILE))
}

fn saved_server(app: &AppHandle) -> Option<url::Url> {
    let text = std::fs::read_to_string(server_file(app)?).ok()?;
    normalize_server_url(&text).ok()
}

/// The bundled setup page, as a URL a remote page can be navigated back to.
fn setup_url() -> Option<url::Url> {
    let base = if cfg!(windows) {
        "http://tauri.localhost/"
    } else {
        "tauri://localhost/"
    };
    url::Url::parse(base).ok()?.join("index.html").ok()
}

/// Called by the setup page: validate, remember, and open the server.
#[tauri::command]
fn connect(app: AppHandle, window: WebviewWindow, url: String) -> Result<(), String> {
    let parsed = normalize_server_url(&url)?;
    let path = server_file(&app).ok_or("No configuration directory is available.")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("Cannot save the address: {error}"))?;
    }
    std::fs::write(&path, parsed.as_str())
        .map_err(|error| format!("Cannot save the address: {error}"))?;
    window
        .navigate(parsed)
        .map_err(|error| format!("Cannot open the server: {error}"))
}

/// Lets the setup page prefill the last address.
#[tauri::command]
fn current_server(app: AppHandle) -> Option<String> {
    saved_server(&app).map(|url| url.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![connect, current_server])
        .menu(|app| {
            let change = MenuItem::with_id(
                app,
                CHANGE_SERVER,
                "Change Server…",
                true,
                Some("CmdOrCtrl+Shift+L"),
            )?;
            let file = Submenu::with_items(
                app,
                "File",
                true,
                &[
                    &change,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?;
            Menu::with_items(app, &[&file])
        })
        .on_menu_event(|app, event| {
            if event.id() == CHANGE_SERVER {
                if let Some(window) = app.get_webview_window("main") {
                    if let Some(url) = setup_url() {
                        let _ = window.navigate(url);
                    }
                }
            }
        })
        .setup(|app| {
            // AWESOMETV_URL still overrides the saved server, for kiosks and testing.
            let env_url = std::env::var("AWESOMETV_URL")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let target = match env_url {
                Some(value) => Some(normalize_server_url(&value).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid AWESOMETV_URL: {error}"),
                    )
                })?),
                None => saved_server(app.handle()),
            };
            let url = match target {
                Some(server) => WebviewUrl::External(server),
                None => WebviewUrl::App("index.html".into()),
            };
            WebviewWindowBuilder::new(app, "main", url)
                .title("ChannelLoom")
                .inner_size(1440.0, 900.0)
                .min_inner_size(960.0, 600.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ChannelLoom");
}

#[cfg(test)]
mod tests {
    use super::normalize_server_url;

    #[test]
    fn accepts_bare_hosts_and_full_urls() {
        assert_eq!(
            normalize_server_url(" 192.168.1.20:8409 ")
                .unwrap()
                .as_str(),
            "http://192.168.1.20:8409/"
        );
        assert_eq!(
            normalize_server_url("https://tv.example.com")
                .unwrap()
                .as_str(),
            "https://tv.example.com/"
        );
        assert!(normalize_server_url("").is_err());
        assert!(normalize_server_url("ftp://tv.example.com").is_err());
    }
}
