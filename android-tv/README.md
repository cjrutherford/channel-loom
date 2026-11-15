# Awesome TV Android TV wrapper

A small native Android TV shell that loads the Awesome TV web app in a full-screen
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

Or sideload it on the TV. The app requires network access to the Awesome TV
server.

## Server URL

The default URL is the Tailscale HTTPS address:

```text
https://nirvana.tail5a4a0.ts.net:8447/
```

Build with a different URL:

```sh
./gradlew assembleDebug -Pawesometv.url=http://100.120.253.100:8409/
```

Or launch with an explicit URL:

```sh
adb shell am start -n net.christopherrutherford.awesometv/.MainActivity \
  --es url http://100.120.253.100:8409/
```

On a TV, either join the tailnet with Tailscale or use a LAN HTTP URL. Cleartext
HTTP is permitted by the app's network security config for local playback.

## Controls

- D-pad arrows are forwarded to the web app's TV Mode.
- Enter/center re-tunes the selected channel.
- Back navigates WebView history, then exits.
- HTML5 fullscreen video is handled in a native full-screen view.

## Release signing

The debug APK is signed with the debug key. Create a release keystore and add a
signing config before distributing outside the local network.
