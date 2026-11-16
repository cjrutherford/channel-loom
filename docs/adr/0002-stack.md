# ADR 0002 — Stack and repository shape

Status: accepted
Date: 2026-11-14

## Context

The product is a long-running, engine-heavy appliance that must ship as a single binary, run on x86_64 and arm64, supervise FFmpeg processes, and expose a typed API to a modern web UI. The team chose Rust.

## Decision

- **Backend:** Rust (stable toolchain), one Cargo workspace, multiple crates.
- **HTTP/API:** `axum` + `tower` + `hyper`, `serde`, OpenAPI generated from the API types (`utoipa` or `aide`).
- **Async/runtime:** `tokio` for process supervision, timers, and I/O.
- **Database:** `sqlx`; SQLite (WAL, FTS5) default, PostgreSQL optional in v1. Migrations via `sqlx::migrate`.
- **Logging/metrics:** `tracing`, `metrics`/Prometheus.
- **Frontend:** React 19 + TypeScript + Vite; TanStack Router/Query, Tailwind + shadcn/ui, dnd-kit, HLS.js.
- **Media:** external `ffmpeg`/`ffprobe`; capability probe at startup; no libav bindings.
- **Packaging:** single binary with embedded UI; Docker multi-arch (amd64/arm64).
- **Testing:** `cargo test`, `cargo nextest`, `insta` snapshots, `proptest`, `wiremock`; Playwright for UI.

## Consequences

- Rust compile times and contributor pool are real risks; keep the crate graph small and CI incremental.
- The scheduler is a pure crate with no I/O; the engine is a separate crate with a narrow contract.
- FFmpeg stays replaceable and debuggable as an external process.
- UI and backend can be developed in parallel against a generated API client.
