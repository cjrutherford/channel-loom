# ChannelLoom brand guide

ChannelLoom is the product name. It is one word, with a capital **C** and a
capital **L** — never “Channel Loom”, “Channelloom”, or “ChannelLoom TV”.

**Tagline:** Your favorites are always on.

**Alternate positioning:** Your media. Your channels. Your way.

## What the two places are called

| Place | Route | What it is |
|---|---|---|
| **Watch** | `/` | Tune in, surf channels, and enjoy continuous programming. |
| **Studio** | `/admin` | Create channels, arrange schedules, and control what plays. |

Watch and Studio have equal prominence. Public copy should stay friendly and
plain-language: “what’s on”, “build a channel”, “your library”, not
“broadcast control”, “playout”, or “transcode profile” unless the reader is in
Studio.

## Palette

| Token | Hex | Use |
|---|---|---|
| Midnight | `#10162B` | App background, theme color |
| Deep navy | `#0B1120` | Inputs, rails, deep surfaces |
| Panel | `#151D36` | Cards and panels |
| Panel raised | `#1B2444` | Hover and raised surfaces |
| Line | `#2B3557` | Borders |
| Paper | `#F5F7FF` | Primary text |
| Muted | `#A8B3D4` | Secondary text |
| Dim | `#7581A5` | Tertiary text and metadata |
| Teal | `#22D3CF` | Primary accent, live state, primary actions |
| Blue | `#368BEE` | Secondary accent |
| Violet | `#9252DF` | Ambient glow, secondary emphasis |
| Coral | `#FF9972` | Channel numbers, filler blocks |
| Live red | `#FF6B7A` | Live dots and on-air state |

Gradients should stay restrained: a teal glow at the top-left and a violet glow
at the top-right on deep navy. Avoid saturated full-page gradients.

## Typography

| Role | Family | Notes |
|---|---|---|
| Display | **Baloo 2** | Friendly, rounded. Headlines, channel names, wordmark lockups. |
| Body | **Nunito Sans** | Readable body copy and controls. |
| Telemetry | **IBM Plex Mono** | Times, counts, status labels, technical metadata. |

Display text is sentence case. Uppercase is reserved for small mono labels and
eyebrows with generous letter spacing.

## Assets

Approved kit assets are checked in under [`assets/channel-loom/`](../assets/channel-loom/).
The original approved references live in
`assets/channel-loom/02-original-approved-references/` and must be retained.

| Asset | Repository path | Used by |
|---|---|---|
| App icon | `crates/awesometv-server/web/icons/icon-512.png` | PWA, web, Cast metadata |
| Maskable icon | `crates/awesometv-server/web/icons/icon-maskable-512.png` | Android/PWA |
| Wordmark | `crates/awesometv-server/web/icons/channelloom-wordmark.png` | Watch/Studio headers, login, Cast, desktop splash |
| ICO | `crates/awesometv-server/web/icons/channelloom.ico` | Windows/Tauri |
| Cast background | `deploy/cast/channelloom-cast-background.png` | Styled receiver upload |
| Cast logo | `deploy/cast/channelloom-cast-logo.png` | Styled receiver upload |
| Android TV banner | `android-tv/app/src/main/res/drawable/banner.png` | Android TV launcher |
| Tauri icons | `desktop/src-tauri/icons/` | Desktop bundle |

The kit is raster-only. Do not rename PNGs to `.svg` or present a generated
raster as a vector master. If a true vector master is produced later, add it as
a new asset and keep the approved rasters.

The name has not been legally cleared. Complete a trademark search before a
public launch.

## Surfaces

- **Watch:** icon + “ChannelLoom” lockup in Baloo 2, the tagline as the small
  subtitle, and a coral channel number. The player and guide are the focus.
- **Studio:** the same lockup with the subtitle “Studio”. Studio navigation uses
  the teal active pill.
- **Cast receiver:** interactive Cast controls stay separate from the baked-in
  brand marks. The receiver page uses the app icon, the ChannelLoom name, and
  the tagline; the Styled receiver artwork is uploaded through the Cast console.
- **Native:** Android TV label and banner, Tauri `productName`, window title, and
  desktop splash all say ChannelLoom.

## Internal identifiers stay unchanged

The rebrand is user-facing only. Keep the existing identifiers unless a
deliberate migration is planned:

- `AWESOMETV_*` environment variables
- `awesometv.db`, `awesometv` crate, container, and service names
- Android package `net.christopherrutherford.awesometv`
- Tauri identifier `net.christopherrutherford.awesometv.desktop`
- Hostnames, routes, APIs, and the `/admin` URL

## Voice

- Warm and direct: “Your favorites are always on.”
- Prefer “channel”, “show”, “movie”, “what’s on”, “library”.
- Explain technical states in human terms: “Tuning in…”, “Getting things
  ready…”, “We could not load your channels.”
- No broadcast-operations jargon in Watch. Studio may be more precise, but keep
  the same friendly tone.
