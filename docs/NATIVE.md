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
caches the app shell and icons, public channel/guide API responses, and a bounded
cache of HLS segments. Cast, catch-up, DVR, and cloud routes are not cached.

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


## Google Cast

Cast is first-class on the web/PWA surface.

- Cast buttons in the public viewer and admin console.
- The sender preloads the Cast SDK after configuration so the device picker
  retains the click's user gesture.
- The backend creates a short-lived signed Cast HLS playlist at
  `/api/v1/channels/{id}/cast` and
  `/api/v1/public/channels/{number}/cast`.
- Public playlist/segment routes:
  - `/cast/{channel}.m3u8`
  - `/cast/{channel}/{segment}.ts`
- The playlist contains absolute signed segment URLs, CORS headers, and a
  continuous handoff across programme boundaries.
- The page enters a cast-mode overlay (local playback pauses) with play/pause,
  mute, volume, stop, and channel switching.
- Receiver is configurable with `AWESOMETV_CAST_RECEIVER_ID`; the default is the
  Default Media Receiver (`CC1AD845`). A custom receiver page is served at
  `/cast-receiver`.
- See [`CAST.md`](CAST.md) and [`CAST-APP-ID.md`](CAST-APP-ID.md) for branding.

The Cast device must be able to reach the media base URL. Set:

```sh
AWESOMETV_CAST_BASE_URL=http://192.168.1.50:8409
```

to a LAN-reachable address. Tailscale addresses are often not reachable by a
Chromecast. Settings shows the effective Cast base URL.
