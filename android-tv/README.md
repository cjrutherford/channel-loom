# ChannelLoom Android TV wrapper

A small native Android TV shell that loads the ChannelLoom web app in a full-screen
WebView with a Leanback launcher entry, banner, and D-pad/keyboard forwarding.

## Build

Requirements:

- Android SDK with platform 36 and build-tools 36
- JDK 17

```sh
cd android-tv
echo "sdk.dir=$HOME/Android/Sdk" > local.properties
./gradlew assembleDebug
```

The APK is written to:

```text
app/build/outputs/apk/debug/app-debug.apk
```

## Install

```sh
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Or sideload it on the TV. The app requires network access to the ChannelLoom
server.

## Server URL

On first launch the app asks for the server address and remembers it. Press
**Menu** on the remote, or choose **Change server** on the error screen, to
change it.

Preset an address at build time:

```sh
./gradlew assembleDebug -Pawesometv.url=http://192.168.1.20:8409/
```

Or launch with an explicit URL:

```sh
adb shell am start -n net.christopherrutherford.channelloom/.MainActivity \
  --es url http://192.168.1.20:8409/
```

Cleartext HTTP is permitted by the app's network security config for local
playback.

## Controls

- D-pad arrows are forwarded to the web app's TV Mode.
- Enter/center re-tunes the selected channel.
- Back navigates WebView history, then exits.
- HTML5 fullscreen video is handled in a native full-screen view.

## Release signing

`./gradlew assembleRelease` signs the APK when `CHANNELLOOM_KEYSTORE`,
`CHANNELLOOM_KEYSTORE_PASSWORD`, `CHANNELLOOM_KEY_ALIAS` and
`CHANNELLOOM_KEY_PASSWORD` are set, and builds an unsigned APK otherwise. Tagged
releases are signed in CI; see [docs/RELEASE.md](../docs/RELEASE.md).
