# ChannelLoom desktop wrapper

A Tauri v2 desktop shell that opens the ChannelLoom web app in a native window.

## Requirements

Debian/Ubuntu build dependencies:

```sh
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev patchelf
```

Rust and the Tauri CLI:

```sh
cargo install tauri-cli --version '^2'
```

## Run

```sh
cargo tauri dev
```

On first launch the window shows a setup page that asks for the server address
and saves it in the app's config directory (`server-url`). Change it later with
**File → Change Server…** (Ctrl/Cmd+Shift+L). `AWESOMETV_URL` overrides the saved
address:

```sh
AWESOMETV_URL=http://localhost:8409/ cargo tauri dev
```

## Build

```sh
cargo tauri build
```

The package lands under `src-tauri/target/release/bundle`.

## Notes

- The window loads the server's web app directly. The bundled `ui/index.html` is
  the setup page; only it can call the app's `connect` command, because remote
  pages get no IPC capability.
