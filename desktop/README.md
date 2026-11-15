# Awesome TV desktop wrapper

A Tauri v2 desktop shell that opens the Awesome TV web app in a native window.

## Requirements

Debian/Ubuntu build dependencies:

```sh
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev patchelf
```

Rust 1.80+ and the Tauri CLI:

```sh
cargo install tauri-cli --version '^2'
```

## Run

```sh
AWESOMETV_URL=https://nirvana.tail5a4a0.ts.net:8447/ cargo tauri dev
```

The URL defaults to the Tailscale HTTPS app. Override it with `AWESOMETV_URL` to
use a LAN or localhost address.

## Build

```sh
cargo tauri build
```

The package lands under `src-tauri/target/release/bundle`.

## Notes

- The window loads the remote app directly; the local `ui/index.html` is only a
  loading/fallback document for the Tauri bundle.
- This host currently lacks the WebKitGTK development package, so the desktop
  wrapper is scaffolded but not compiled here. The Android TV wrapper is built.
