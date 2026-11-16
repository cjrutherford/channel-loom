# Getting a Cast receiver App ID

The app currently casts to Google's **Default Media Receiver** (`CC1AD845`).
That receiver only shows a small title in the corner and cannot be branded.
Changing the TV experience means creating your own receiver in the Google Cast
SDK Developer Console and giving the app its **App ID**.

You need a Google account and a one-time Cast developer registration (currently
US$5). The console is at:

```text
https://cast.google.com/publish
```

## Option A — Styled Media Receiver (recommended, no code)

This is the quickest branded experience. Google hosts the receiver; you upload a
background and logo and choose a title. It does not depend on the app's public
hostname.

1. Sign in at <https://cast.google.com/publish> and complete the developer
   registration if prompted.
2. Choose **Add new application**.
3. Select **Styled Media Receiver**.
4. Fill in:
   - **Name**: `Awesome TV`
   - **Description**: `Live channels from Awesome TV`
   - **Theme**: dark / black
   - **Title**: `Awesome TV`
   - **Subtitle**: `Live channels`
5. Upload the prepared artwork from this repository:
   - Logo: `deploy/cast/awesome-tv-cast-logo.png`
   - Background: `deploy/cast/awesome-tv-cast-background.png`
6. Save the application.
7. Copy the **App ID** shown in the application list. It is a short alphanumeric
   string such as `4F8B2C1D`.

## Option B — Custom Receiver (full control)

This uses the branded receiver page already served by Awesome TV:

```text
https://atv.christopherrutherford.net/cast-receiver
```

The page uses the Cast Application Framework (CAF) and shows the Awesome TV
brand, live badge, and now-playing title.

1. Sign in at <https://cast.google.com/publish>.
2. Choose **Add new application**.
3. Select **Custom Receiver**.
4. Fill in:
   - **Name**: `Awesome TV`
   - **Receiver URL**:
     `https://atv.christopherrutherford.net/cast-receiver`
5. Save the application.
6. Copy the **App ID** from the application list.

The URL must be publicly reachable over HTTPS. While the home network or the
public hostname is down, the Chromecast cannot load this receiver. Use a Styled
Media Receiver if you want branding before the public endpoint is restored.

## Register your Chromecast device

Unpublished receiver applications only run on devices registered for testing.
In the Cast developer console:

1. Open **Cast Receiver Devices**.
2. Choose **Add device**.
3. Enter the Chromecast serial number. In the Google Home app, open the device →
   **Settings** → **Device information** → **Serial number**.
4. Save.

Skip this only if you publish the receiver for public use.

## Give the App ID to Awesome TV

Set the App ID in the app environment:

```sh
AWESOMETV_CAST_RECEIVER_ID=YOUR_APP_ID
```

Then recreate the application:

```sh
docker compose up -d --force-recreate
```

If the app is running as a temporary container outside compose, restart it with
the same environment variable, or restore the compose deployment first.

The sender passes the App ID to the Cast SDK; no rebuild is required.

## Test

1. Open the watch page on the LAN, for example
   `http://192.168.1.153:8409/`.
2. Click **Cast** and choose the Chromecast.
3. The TV should show the Styled or Custom receiver instead of the Default
   Media Receiver.
4. Use the page's cast overlay to play/pause, mute, change volume, switch
   channels, or stop casting.

## Troubleshooting

| Symptom | Check |
|---|---|
| Device picker lists the TV but nothing loads | The device serial is registered for the App ID, or the receiver is published |
| Custom receiver shows a blank/error page | `https://atv.christopherrutherford.net/cast-receiver` must return 200 from the public internet |
| Cast starts then stalls | The Chromecast must reach `AWESOMETV_CAST_BASE_URL` (usually the LAN IP and port 8409) |
| Still sees the old corner title | The app is still using `CC1AD845`; verify `AWESOMETV_CAST_RECEIVER_ID` and recreate the container |
| Styled receiver has no artwork | Re-upload `deploy/cast/awesome-tv-cast-logo.png` and `awesome-tv-cast-background.png` and save |

## Notes

- The App ID is not a secret; it is safe to keep in `.env`.
- The receiver ID applies to both the public watch page and the admin console.
- The media URL still uses the LAN cast base, so the Chromecast and the app must
  be on the same network for playback.
- For the Custom Receiver, the public hostname must be healthy before the TV can
  load the receiver page. The Styled Receiver is hosted by Google and avoids that
  dependency.
