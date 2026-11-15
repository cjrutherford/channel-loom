//! Phase 0 HDHomeRun emulation spike.
//!
//! Serves the minimal endpoint set Plex/Jellyfin/Emby use to discover a tuner
//! and fetch a lineup, then streams a pre-built MPEG-TS file from /auto/v1.ts.
//! This is a protocol-shape test, not the production tuner implementation.

use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};

const DEVICE_ID: &str = "A11CE001";

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut port: u16 = 18409;
    let mut ts: Option<PathBuf> = None;
    let mut base_url: Option<String> = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().context("--port needs a value")?.parse()?,
            "--ts" => ts = Some(PathBuf::from(args.next().context("--ts needs a path")?)),
            "--base-url" => base_url = args.next(),
            "--help" | "-h" => {
                println!(
                    "usage: awesometv-hdhr-spike [--port 18409] [--ts channel.ts] [--base-url http://host:port]"
                );
                return Ok(());
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    let base = base_url.unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    let state = Arc::new(State { base, ts, port });

    let listener =
        TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("bind 0.0.0.0:{port}"))?;
    println!("HDHomeRun spike listening on http://0.0.0.0:{port}");
    println!("  discover: {}/discover.json", state.base);
    println!("  lineup:   {}/lineup.json", state.base);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(error) = handle(stream, &state) {
                        eprintln!("request error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("accept error: {error}"),
        }
    }
    Ok(())
}

struct State {
    base: String,
    ts: Option<PathBuf>,
    #[allow(dead_code)]
    port: u16,
}

fn handle(mut stream: TcpStream, state: &State) -> Result<()> {
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let path = path.split('?').next().unwrap_or(path);

    match path {
        "/" | "/healthz" => text(&mut stream, 200, "ok\n", "text/plain"),
        "/discover.json" => json(
            &mut stream,
            &serde_json::json!({
                "FriendlyName": "Awesome TV Spike",
                "ModelNumber": "HDHR-AWESOME",
                "FirmwareName": "awesometv_spike",
                "FirmwareVersion": "0.1.0",
                "DeviceID": DEVICE_ID,
                "DeviceAuth": "spike",
                "BaseURL": state.base,
                "LineupURL": format!("{}/lineup.json", state.base),
                "TunerCount": 4
            }),
        ),
        "/lineup.json" => {
            let stream_url = format!("{}/auto/v1.ts", state.base);
            let body = serde_json::json!([
                {
                    "GuideNumber": "1",
                    "GuideName": "Awesome TV Spike",
                    "URL": stream_url
                }
            ]);
            json(&mut stream, &body)
        }
        "/lineup_status.json" => json(
            &mut stream,
            &serde_json::json!({
                "ScanInProgress": 0,
                "ScanPossible": 1,
                "Source": "Cable",
                "SourceList": ["Cable"]
            }),
        ),
        "/device.xml" => {
            let body = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <URLBase>{base}</URLBase>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>Awesome TV Spike</friendlyName>
    <manufacturer>Silicondust</manufacturer>
    <modelName>HDHR-AWESOME</modelName>
    <modelNumber>HDHR-AWESOME</modelNumber>
    <tunerCount>4</tunerCount>
    <serialNumber>{DEVICE_ID}</serialNumber>
    <UDN>uuid:{DEVICE_ID}</UDN>
  </device>
</root>"#,
                base = state.base
            );
            text(&mut stream, 200, &body, "application/xml")
        }
        "/auto/v1.ts" | "/tuner0/v1.ts" => match &state.ts {
            Some(path) => stream_ts(&mut stream, path),
            None => text(
                &mut stream,
                503,
                "no TS fixture supplied (use --ts channel.ts)\n",
                "text/plain",
            ),
        },
        _ => text(&mut stream, 404, "not found\n", "text/plain"),
    }
}

fn json(stream: &mut TcpStream, body: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_string_pretty(body)?;
    text(stream, 200, &body, "application/json")
}

fn text(stream: &mut TcpStream, status: u16, body: &str, content_type: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )?;
    Ok(())
}

fn stream_ts(stream: &mut TcpStream, path: &PathBuf) -> Result<()> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    )?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read])?;
    }
    Ok(())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
