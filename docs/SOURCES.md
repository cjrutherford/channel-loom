# Canonical media sources

Confirmed 2026-11-14.

## Source of truth

| Source | Host path | Container path | Kind |
|---|---|---|---|
| Movies | `/mnt/valhalla/media/Movies` | `/media/Movies` | `movie` |
| TV | `/mnt/valhalla/media/Tv` | `/media/Tv` | `episode` |
| Music | `/mnt/valhalla/media/music` | `/media/music` | `music` |
| Images | `/mnt/valhalla/media/images` | `/media/images` | `image` |
| Commercials (later) | `/mnt/valhalla/media/commercials` | `/media/commercials` | `other` |

The mount is CIFS: `//192.168.1.100/media` → `/mnt/valhalla/media`.

## Excluded

- `download/complete`, `download/completed`, `download/incomplete`: download staging, not the organized library.
- `live-tv/`: captured `.ts` recordings exist, but they are not part of the selected canonical video library. Add them later as a separate source if desired.
- `videoad/`, `configs/`: empty and unrelated.
- `transcode_cache/`: empty; ChannelLoom uses its own cache under its data directory.
- `tanuki-assets/`: unrelated project assets.
- `#recycle/`: recycle bin.
- Any directory or file component ending in `.1` (duplicate music folders).
- `@eaDir`, `._*`, and hidden metadata.

## Jellyfin

`jelly.christopherrutherford.net` is backed by the same media share. Jellyfin API paths map as follows:

- `/data/movies` → `/media/Movies`
- `/data/tvshows` → `/media/Tv`
- `/data/music` → `/media/music`
- `/config/data/livetv/recordings` → skipped (live TV recordings are outside the selected library)

Full API sync uses `JELLYFIN_API_KEY`. The current sync imported 10,322 items (68 movies, 10,254 episodes) and skipped 95 live-TV recordings.

## Scanning behavior

- Movies: `*.mkv`, `*.mp4`, `*.m4v`, `*.avi`, `*.mov`, `*.ts`, `*.mpg`, `*.mpeg`, `*.webm`, `*.wmv`, `*.flv`
- Music: `*.flac`, `*.mp3`, `*.m4a`, `*.aac`, `*.ogg`, `*.opus`, `*.wav`
- Images: `*.jpg`, `*.jpeg`, `*.png`, `*.webp`, `*.gif`, `*.bmp`, `*.tif`, `*.tiff`
- Images default to 15 seconds in a timeline.
- Titles come from NFO metadata first, then folder/filename fallback.
