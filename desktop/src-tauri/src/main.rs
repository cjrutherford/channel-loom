#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{WebviewUrl, WebviewWindowBuilder};

fn main() {
    let url = std::env::var("AWESOMETV_URL")
        .unwrap_or_else(|_| "https://nirvana.tail5a4a0.ts.net:8447/".to_string());
    tauri::Builder::default()
        .setup(move |app| {
            let parsed = url
                .parse()
                .map_err(|error| format!("invalid AWESOMETV_URL: {error}"))?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
                .title("Awesome TV")
                .inner_size(1440.0, 900.0)
                .min_inner_size(960.0, 600.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Awesome TV");
}
