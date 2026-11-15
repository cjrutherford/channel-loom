# Clean-room policy

Purpose: build Awesome TV as an independent implementation of the same user-facing problem, without copying source code or protected expression from ErsatzTV, ErsatzTV Next, Tunarr, dizqueTV, or their forks.

This is an engineering policy, not legal advice. Have counsel review it before a public release.

## 1. What clean room means here

A **clean-room implementation** is produced from:

- public documentation;
- public protocol and format specifications (HLS, XMLTV, M3U, HDHomeRun HTTP API, Plex/Jellyfin/Emby public APIs);
- release notes, changelogs, issue trackers, and user-facing behaviour;
- black-box observation of a running system (inputs, outputs, network traffic, logs the user can see);
- our own product requirements, architecture, and tests.

It is **not** produced by copying, translating, or mechanically transforming another project's source code, internal schemas, assets, or non-public implementation details.

## 2. Allowed

- Reading public documentation, READMEs, changelogs, and release notes.
- Reading public API descriptions and JSON schemas that are explicitly published as integration contracts.
- Running ErsatzTV/Tunarr and observing user-facing behaviour: UI flows, HTTP endpoints, HLS/XMLTV/M3U output, HDHomeRun responses, logs shown in the UI.
- Writing independent tests that assert observable behaviour.
- Implementing public standards and protocols from their specifications.
- Importing user data through documented public APIs or user-exported files.
- Naming compatibility in documentation factually ("imports ErsatzTV channel plans").
- Using permissively licensed third-party libraries and tools (FFmpeg, SQLite, React, etc.) under their licenses.

## 3. Not allowed

- Copying source code, comments, tests, or internal identifiers from another project.
- Translating source code from one language to another (e.g. C# to Go) as a shortcut.
- Copying internal database schemas, private APIs, or undocumented file formats.
- Copying artwork, icons, fonts, UI text, screenshots, or branding.
- Decompiling or extracting non-public implementation details.
- Using another project's trademarks as our product name or in a way that implies endorsement.
- Copying an entire feature's UI layout pixel-for-pixel; we design our own interaction model.
- Adding code from a project with an incompatible license.

## 4. Provenance log

Keep `docs/clean-room/provenance.md` with entries:

```
Date:
Source (URL/document):
What was learned:
How it will be used:
Reviewed by:
```

Any engineer who studies a competitor's behaviour records the source and the takeaway. This protects the project and makes audits cheap.

## 5. Roles

For a strict clean-room process:

- **Researcher** studies public behaviour/docs and writes a behaviour specification.
- **Implementer** works only from the specification, standards, and our own architecture; does not study competitor source.
- **Reviewer** checks for similarity, license issues, and provenance.

For a small team, one person can hold multiple roles, but the written spec and provenance log still matter. Do not paste competitor code into an LLM and ask for a rewrite; that is still derivation.

## 6. Compatibility commitments

We may support:

- **Import**: ErsatzTV/Tunarr/dizqueTV channel plans, collections, and schedules through documented APIs or user exports.
- **Export**: M3U, XMLTV, HDHomeRun, HLS, MPEG-TS, and a documented JSON channel/timeline format.
- **External contracts**: optionally consume or emit the ErsatzTV Next playout schema, implemented independently from its public JSON Schema and documentation.

Compatibility is a feature, not a dependency. The core must run without ErsatzTV Next.

## 7. Licensing

- Our application code: choose MIT or Apache-2.0 (decision pending).
- FFmpeg: use an LGPL-compatible build where possible; if bundling a GPL build, comply with GPL distribution obligations and document it. Prefer an external/user-supplied FFmpeg for flexibility.
- Third-party dependencies: automated license scan in CI; no copyleft contamination in proprietary modules.
- Docs and design assets: original work; do not reuse competitor assets.

## 8. Naming and branding

- "Awesome TV" is a working name; run a trademark search before launch.
- Do not use "ErsatzTV", "Tunarr", "dizqueTV", or their logos in the product name, icon, or marketing in a way that suggests affiliation.
- Factual compatibility statements are fine: "Works with Plex, Jellyfin, and Emby."

## 9. If in doubt

1. Stop.
2. Write down what you are about to do and why.
3. Prefer the public specification or black-box test.
4. Ask for review.
5. If a shortcut would save a day but blur the line, do not take it.
