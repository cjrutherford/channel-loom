# Native and installable apps

## Installable web app (PWA)

Awesome TV ships an installable app shell:

- manifest: `GET /manifest.webmanifest`
- service worker: `GET /sw.js`
- icons: `/icons/icon-192.png`, `/icons/icon-512.png`, `/icons/icon-maskable-512.png`
- shortcuts: Live TV, TV Mode, Guide

Installation requires a secure context: HTTPS or `localhost`. Plain HTTP on a
tailnet IP will serve the app but browsers will not offer installation.

### Tailscale HTTPS

The current host exposes the app over Tailscale Serve with a valid certificate:

```text
https://nirvana.tail5a4a0.ts.net:8447/
```

This is tailnet-only. To recreate the proxy:

```sh
tailscale serve --bg --https=8447 http://100.120.253.100:8409
```

To remove it:

```sh
tailscale serve --https=8447 off
```

### Install steps

- Desktop Chrome/Edge: open the HTTPS URL, use the install icon in the address
  bar or menu → **Install Awesome TV**.
- Android Chrome: menu → **Add to Home screen**.
- iOS Safari: Share → **Add to Home Screen**.
- TV browsers: open the HTTPS URL and use TV Mode.

The app starts in standalone mode with the Awesome TV icon. The service worker
caches only the app shell and icons; API, HLS, catch-up, and DVR requests are
never cached.

## TV mode

Open **TV Mode** in the sidebar or go to `#tv`.

- large 16:9 player, now-playing overlay, and clock
- horizontal channel strip with now/next/progress
- arrow keys change channels and auto-tune
- Enter re-tunes the selected channel

## Native player endpoints

Any compatible player can use:

```text
HDHomeRun: http://100.120.253.100:8409/discover.json
M3U:       http://100.120.253.100:8409/iptv/channels.m3u
XMLTV:     http://100.120.253.100:8409/iptv/xmltv.xml
MPEG-TS:   http://100.120.253.100:8409/auto/v<channel>.ts
```

## Future wrappers

The PWA is the first native-app milestone. Dedicated Tauri desktop and Android
TV wrappers can reuse the same web app and endpoints. QSV/NVENC profiles are
deferred because the current host exposes VAAPI but rejects QSV/NVENC device
initialization.


## Android TV wrapper

Source: `android-tv/`

The app is a full-screen WebView shell with a Leanback launcher entry, TV banner,
and native D-pad forwarding to the web app's TV Mode.

Build:

```sh
cd android-tv
echo "sdk.dir=$HOME/Android/Sdk" > local.properties
JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 ./gradlew assembleDebug
```

Output:

```text
android-tv/app/build/outputs/apk/debug/app-debug.apk
```

Override the server URL:

```sh
./gradlew assembleDebug -Pawesometv.url=http://100.120.253.100:8409/
```

The default is the Tailscale HTTPS URL. The TV needs tailnet access or a LAN URL.

## Tauri desktop wrapper

Source: `desktop/`

A Tauri v2 window that loads the remote app and accepts `AWESOMETV_URL`.

```sh
AWESOMETV_URL=https://nirvana.tail5a4a0.ts.net:8447/ cargo tauri dev
```

The wrapper is scaffolded but not compiled on this host because the WebKitGTK
development package is not installed. See `desktop/README.md` for the required
system packages and build steps.
