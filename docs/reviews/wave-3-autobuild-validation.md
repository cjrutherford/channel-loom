# Wave 3: auto-build validation (slice 6)

Preview and create run the same planning and validation before creation. Explicit collection IDs must exist and must not repeat; every selected collection must have playable media. Channel names are checked case-insensitively against existing channels and the entire proposed selection. Proposed consecutive numbers must not collide with existing numbers or exceed the supported integer range. Errors identify the offending ID, name, or number where applicable. Explicit selections are not truncated by the automatic selection count.

## Evidence

- `timeout 180s cargo test -p awesometv-server auto_build_preview_create_validation_and_persistence -- --nocapture` — passed: 1 test, 0 failed (83 filtered out). The test sends HTTP requests to the actual auto-build handler backed by a temporary SQLite database. It checks matching preview/create errors for a missing ID, repeated ID, duplicate names, existing name, first and later number collisions, number overflow, and a collection containing only excluded ISO media. It snapshots relevant table contents and counts before and after each invalid request. It also checks matching valid plans, a mutation-free preview, and persisted channels and generated items after creating two selected channels.
- Before the implementation change, the same focused test failed because preview accepted a selection whose second collection contained no playable media (HTTP 200 instead of 400). An initial test-harness attempt failed on an invalid SQLite snapshot query; that query was corrected before establishing this behavioral failure.

## Limits

The test mounts the real handler on a small Axum router; it does not exercise the production router's authentication or middleware. Validation and creation are separate database operations, so concurrent changes after validation can still invalidate the plan. Creation can also partially succeed if a later persistence or rebuild operation fails. Atomic publication and concurrent allocation belong to slice 7; this slice does not claim either guarantee. The broader test suite and UI were not run.
