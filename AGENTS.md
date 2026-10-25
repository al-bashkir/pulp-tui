# AGENTS.md

Guidance for contributors and coding agents working on pulp-tui.

## Architecture in one paragraph

`src/pulp/` is the REST layer (transport, capabilities, typed models, plugin operations).
`src/cv.rs`, `src/replication.rs`, `src/filters.rs` are services built only on that layer.
`src/store.rs` is the SQLite/PostgreSQL store for application-owned state. `src/session_log.rs` is the
in-memory request/action log for the Logs screen. `src/tui/` is the UI:
`app.rs` (state, jobs, keys, modals), `kinds.rs` (screens: source, columns, actions),
`actions*.rs` (what actions do), `view.rs` (rendering only), `form.rs`, `pick.rs` (picker
option loading), `tasks.rs`.
Read `docs/ARCHITECTURE.md` before changing workflows.

## Rules

* Talk to Pulp only through `PulpClient`. Never shell out, never touch Pulp's database.
* Do not retry mutations. Make workflows idempotent instead (find-or-create by name,
  reuse publications, store task hrefs in checkpoints before waiting).
* Never block the UI thread on I/O. Use `app.job(...)` (needs a connection) or
  `app.spawn(...)`; return an `Apply` closure. `view.rs` must not perform I/O.
* Check capabilities (`Capabilities::supports/has_field/has_endpoint`) before using
  version-dependent endpoints or fields; show "not supported by this server" otherwise.
* Destructive actions: load the impact first, then `confirm_danger` with the object name.
  Mutating actions must be classified in `Act::mutates` (read-only mode depends on it).
* Never log or display secrets. Secrets in config are references only.
* Pulp tasks are the source of truth. Do not mark something done because a POST returned
  202; wait for the task and, where it matters, verify served content.
* Content View objects in Pulp carry ownership labels (`pulp_tui_cv`, `pulp_tui_env`,
  `pulp_tui_replica`) and the database id (`pulp_tui_db`). Never modify objects without
  their ownership labels, or with the id of another database.
* Keep code small and direct; prefer existing helpers over new abstractions.
* Each action key must be unique per screen and must not collide with list keys (a unit
  test checks this).

## Build and test

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test                         # unit + mock HTTP
tests/run-live.sh                  # live: Pulp containers, fixtures, all scenarios
podman build -f Containerfile .
cargo audit
```

Live tests skip unless `PULP_TUI_LIVE=1`. They use unique object names, so they can rerun
against the same containers; `tests/env/pulp-env.sh down-all` resets everything.

## Database migrations

Append a new SQL batch to both `MIGRATIONS` (SQLite) and `PG_MIGRATIONS` (PostgreSQL) in
`src/store.rs`; never edit an existing one. The lists must stay the same length (a unit
test checks it). Store code is shared by both backends, so keep SQL portable. Add a unit
test, and a case in `tests/live_shared_db.rs` when PostgreSQL behaves differently. The
external database changes only with `pulp-tui db migrate`; the local file migrates on open.

## When Pulp behaves unexpectedly

Read the live OpenAPI (`/pulp/api/v3/docs/api.json`) and, in the test container, the
plugin source under `/usr/local/lib/python3.12/site-packages/`. Record new server
behaviors in `docs/COMPATIBILITY.md`.
