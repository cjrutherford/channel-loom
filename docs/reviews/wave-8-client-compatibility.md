# Wave 8: client compatibility evidence

**Disposition: blocked at the slice 14 client-availability stop.** No client-specific acceptance is claimed. The Wave 7 local prerequisite for slices 13 and 15 closed as recorded in [its checkpoint](wave-7-checkpoint.md); hosted CI, actual providers, provisioning and live midnight playback remain outside this evidence.

On 2026-11-18, local executable and desktop-entry discovery found VLC 3.0.23 (Ubuntu build, Linux x86-64) and FFmpeg/ffplay, but no Kodi, Jellyfin Media Player, Plex Media Server, Emby, or dedicated IPTV client. `DISPLAY` and `WAYLAND_DISPLAY` were unset. T3 `device_list` returned no devices: local device support failed during installation, iOS requires macOS/Xcode, and Android SDK command-line tools are missing. This is a local availability result, not an exhaustive inventory of other machines. No device was opened or controlled. No real credentials or external accounts were used.

## Reproduction harness

From the repository root, with the current server binary already built and FFmpeg installed:

```sh
python3 tests/client-compatibility/serve_fixture.py --seconds 600 --evidence-dir /tmp/awesome-client-evidence-NEW
```

Choose a **new** evidence directory. The harness binds only to loopback, generates two four-second red/blue H.264/AAC fixtures, creates a temporary database and an auto-generated sequential channel using a random temporary administrator password, and prints M3U and XMLTV URLs. It writes public feed snapshots and a server log to the evidence directory, terminates its server on expiry or Ctrl-C, and removes its media and database. The local client must run on the same machine: `127.0.0.1` cannot be reached from a separate device. Do not expose the fixture or use real credentials. A port race fails startup; listener ownership is checked before API writes. A pre-existing binary may not reflect current sources; build it from the intended revision and record its identity before a certification run.

In the actual client, record client version, OS, configuration, timestamp and timezone; import the M3U and XMLTV URLs, compare the playlist `tvg-id` with the XMLTV channel ID, and compare displayed program titles and times with XMLTV UTC start/stop converted to the client's timezone. Tune the channel, observe red/blue changes across a scheduled boundary, disconnect/reconnect or retune, and record video/audio, timing and failures. Capture client screenshots or logs and preserve the feed snapshots. Four-second items stress transitions; they do not establish behavior for longer programming, and guide refresh/caching may require client-specific configuration. A successful feed fetch alone is not a client pass. Record results in the matrix by client and version.

## Harness smoke, not client acceptance

`timeout 90s python3 tests/client-compatibility/serve_fixture.py --seconds 10 --evidence-dir /tmp/awesome-wave8-client-harness-20261118` exited 0 on the Linux host. The disposable server published a playlist and XMLTV; snapshots and log are in that directory. Playlist `tvg-id="awesome.1"` matched XMLTV channel `awesome.1`; its first two programmes were `fixture b` 2026-11-19 00:35:16–00:35:20 UTC and `fixture a` 00:35:20–00:35:24 UTC. The XML contained 22,500 short programme entries. A second smoke after adding termination handling also exited 0, with artifacts in `/tmp/awesome-wave8-client-harness-final-20261118`. This checks fixture setup and feed structure only; no tune, client guide rendering, transition, or reconnect was observed. The binary was already present and its source revision was not established for this smoke.

To obtain the missing proof, make an actual IPTV client available on this machine with a graphical session, or provide an authorized device/client setup reachable through the supported device tools and arrange a safe reachable fixture address. Then run the reproduction, capture its client behavior, and record the versioned results. An external client/device requires a deliberate networking adaptation: the loopback-only harness does not serve it as written.

For a future remote client, prefer an authorized SSH local forward bound to the client machine's loopback, using the same port printed by the harness (for example, `ssh -N -L 127.0.0.1:PORT:127.0.0.1:PORT SERVER`). The playlist, XMLTV and stream URLs all contain that loopback address and port, so the client must reach them through the forward on its own machine. A separate device that cannot use that forward needs a deliberately adapted fixture bind and public URL, restricted to an authorized private network with firewall controls; verify every advertised URL and remove the access after the run. Do not expose the disposable admin service publicly or use real credentials.
