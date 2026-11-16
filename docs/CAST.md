# Google Cast

Awesome TV casts live channels to Chromecast and Google TV devices through the
Cast sender SDK. The public watch page and the admin console both expose a Cast
button and switch into a casting view when a session starts.

## How it works

1. The sender requests a session with `CastContext.requestSession()`.
2. `POST /api/v1/public/channels/{number}/cast` warms the producer and returns a
   signed HLS URL on `AWESOMETV_CAST_BASE_URL`.
3. The receiver loads the signed playlist and segments from
   `/cast/{number}.m3u8` and `/cast/{number}/{segment}`.
4. The page pauses the local player, shows **Casting to &lt;device&gt;**, and
   routes play/pause, mute, volume, channel switching, and stop through the Cast
   `RemotePlayerController`.

The Cast base URL must be an address the **Chromecast** can reach. In this
deployment that is the LAN address:

```sh
AWESOMETV_CAST_BASE_URL=http://192.168.1.153:8409
```

Do not point it at the public TLS hostname unless the Chromecast can reach it.
The sender must be able to discover the Chromecast on the same network; the
receiver then fetches media from the cast base.

Cold casts wait for the producer to become ready before returning the media
URL, avoiding the receiver's media-load timeout. The app also returns
`Access-Control-Allow-Origin: *` on cast playlists and segments.

For a step-by-step walkthrough of the developer console, device registration,
and where to copy the App ID, see
[Getting a Cast receiver App ID](CAST-APP-ID.md).

## Branding

### Default Media Receiver

With no configuration the app uses the Default Media Receiver (`CC1AD845`) and
sends branded metadata: programme title, channel name, `Awesome TV` as the
series, and the app icon as artwork.

### Styled Media Receiver (recommended, no code)

1. Open the [Google Cast SDK Developer Console](https://cast.google.com/publish).
2. Add a **Styled Media Receiver**.
3. Set the title to `Awesome TV`, upload the logo and background from
   `deploy/cast/` (`awesome-tv-cast-logo.png`,
   `awesome-tv-cast-background.png`), and choose a dark theme.
4. Copy the generated **App ID**.
5. Set it and restart the app:

   ```sh
   AWESOMETV_CAST_RECEIVER_ID=XXXXXXXX
   docker compose up -d --force-recreate
   ```

This gives the TV a branded splash/background with the Awesome TV logo while
keeping Google's media playback UI. Unpublished receivers only run on devices
registered in the console; add the Chromecast serial under **Cast Receiver
Devices**.

### Custom receiver (full control)

The app serves a custom Cast receiver at:

```text
https://atv.christopherrutherford.net/cast-receiver
```

It uses the Cast Application Framework (CAF), renders a branded header, live
badge, and now-playing title over the video, and supports the standard media
namespace (play, pause, seek, volume).

1. In the Cast developer console, add a **Custom Receiver**.
2. Set the receiver URL to the URL above.
3. Register the device(s) you are testing with.
4. Copy the App ID and set `AWESOMETV_CAST_RECEIVER_ID` as above.

The receiver must be public HTTPS; the URL above already is. If the hostname
changes, update the registered URL.

## Sender controls

While a session is active:

| Control | Behaviour |
|---|---|
| Cast button | Starts or stops the session |
| Play / Pause | `RemotePlayerController.playOrPause()` |
| Mute / volume | `muteOrUnmute()` and `setVolumeLevel()` |
| Channel rail / guide | Loads the selected channel on the receiver |
| Stop casting | Ends the session and resumes local playback |

The local video is paused and its native controls are hidden while casting so
the page cannot appear to be playing two streams.

## Troubleshooting

- **"Cast is unavailable"**: use Chrome with Cast device discovery enabled, and
  open the page over HTTPS. Some browsers and private windows do not expose the
  Cast SDK.
- **The device picker appears but the TV stays idle**: check that the
  Chromecast can open `AWESOMETV_CAST_BASE_URL` on the LAN. Open the cast media
  URL from a phone on the same Wi-Fi as a quick check.
- **No artwork/title on the default receiver**: metadata images are served from
  the cast base; make sure `/icons/icon-512.png` is reachable from the device.
- **Custom receiver shows an error**: verify the App ID matches the registered
  receiver and that its URL is exactly
  `https://atv.christopherrutherford.net/cast-receiver`.
- **Playback starts then stalls**: check the producer log in the admin
  (`Sources` → precache/status) and the channel's FFmpeg log under
  `data/logs/channel-<number>.log`.
