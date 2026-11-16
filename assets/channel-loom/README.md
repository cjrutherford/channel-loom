# ChannelLoom — Agent Rebrand Kit

## Objective
Rebrand the application currently called **Awesome TV** to **ChannelLoom** without changing the behavior of its two equally important experiences:
- **Watch:** tune in, channel surf, and enjoy continuous programming from a personal movie, TV, and music library.
- **Studio:** create channels, assemble their schedules/rotation, and control what plays.

**Brand character:** friendly, inviting, gently nostalgic, and comprehensible to non-technical people. Not a professional-broadcasting dashboard in consumer-facing copy.

**Suggested line:** “Your favorites are always on.”
**Alternate positioning:** “Your media. Your channels. Your way.”

## Asset precedence and status
The *original-approved-references* directory holds the user's previously selected visuals. They are included deliberately and must not be discarded casually.
The *brand-primary* and branded background / marketing images are the **latest generated ChannelLoom iteration**, not final technical vector masters. Treat them as the current implementation candidates, retain the originals, and flag any mismatch to review rather than silently redesigning them.

## What is where
| Asset | Path | Intended use |
|---|---|---|
| Branded logo/wordmark | `01-brand-primary/channelloom-wordmark-transparent.png` | Headers, about screen, installer and package branding; transparent raster PNG |
| Branded app icon | `01-brand-primary/channelloom-app-icon.png` | Primary icon source (square raster) |
| Icon derivatives | `03-platform-icons/icon-*.png` | Icons for PWA, web, desktop and mobile (select appropriate sizes) |
| Multi-resolution Windows icon | `03-platform-icons/channelloom.ico` | Windows executables or favicon |
| Branded cast background | `04-backgrounds/channelloom-cast-background-branded.png` | TV casting / idle screen with ample center space for overlay UI |
| 1920×1080 and 1280×720 cast exports | `04-backgrounds/cast-*.png` | Pre-scaled output for casting devices; check safe zones |
| Mobile splash | `04-backgrounds/channelloom-mobile-splash-branded.png` | Portrait launch / onboarding illustration |
| Portrait resized output | `04-backgrounds/splash-1080x1920.png` | Pre-scaled portrait illustration, not native OS launch configuration |
| Branded promotional poster | `05-marketing/channelloom-poster-branded.png` | Store marketing, app about, site sections |
| Original approved choices | `02-original-approved-references/` | Visual fidelity and fallback references |

## Agent implementation brief
1. Audit the repository for existing application name, window title, package identifiers, app manifests, meta tags, favicons, splash/cast screen references, README, install/packaging metadata, and UI copy. Make a map before editing.
2. Replace user-facing **Awesome TV** text with **ChannelLoom** consistently. Retain existing persistence/database identifiers, bundle IDs, APIs, URLs, and package namespaces unless the migration is deliberately scoped and backward-compatible; changing these can break upgrades.
3. Integrate primary assets into the app's existing public/static asset pipeline. Avoid unnecessary new asset-loader layers. Where a current system already supports responsive images or theme resources, reuse it.
4. Implement Watch and Studio navigation/copy with equal prominence, preserving existing roles and user flows. Do not invent functionality or change app routing just to fit the marketing concept.
5. Preserve proper image aspect ratios and add appropriate contrast scrims behind text over the cast background. Important playback metadata should be live UI text, not burned into a background image.
6. Use the raster icon exports at matching target sizes. These files do **not** constitute an editable SVG/vector master. Do not rename PNG files to `.svg`. For PWA maskable icons, Android adaptive icons, or monochrome tray icons, create purpose-built variants with safe-area checks rather than assuming this detailed square art is maskable.
7. Verify title/metadata changes, screen-reader labels, small icon legibility (16–64px), both dark/light interface treatments, 16:9 cast display at TV overscan boundaries, mobile splash aspect ratios, and clean production builds.
8. Provide evidence: changed-file inventory, screenshots (Watch, Studio, casting, installed icon), test results, regression risks, migration notes, and deferred items.

## Working palette (conceptual, not sampled precisely from the generated artwork)
- Midnight `#10162B`
- Teal `#22D3CF`
- Blue `#368BEE`
- Violet `#9252DF`
- Coral `#FF9972`

## Copy principles
- “Make a channel” rather than “Configure a playout stream.”
- “Plan what's on” rather than “Edit the playout schedule.”
- “You're tuned in!” rather than “Playback session initialized.”
- Keep user-facing controls plain-language; retain precise technical diagnostics in expert interfaces.

## Important cautions
- **Name not legally cleared.** Do trademark/domain/app-store clearance before public commercial release.
- Generated PNG assets may have soft glows and complex details that need hand cleanup. Check wordmark spelling visually and avoid claiming a true SVG source exists.
- Cast artwork includes a small static brand mark, so keep interactive UI separate and avoid overlapping this area.
- Resize exports are upscaled or downscaled from the generated raster; they are not additional higher-resolution source images.
- Follow the repository's existing architecture and design tokens where possible.
