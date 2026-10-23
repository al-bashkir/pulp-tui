# Implementation status

Date: 2026-10-23. Tested against pulpcore 3.123.0, pulp_rpm 3.39.3, pulp_file 3.123.0
(`quay.io/pulp/pulp:latest`), see [COMPATIBILITY.md](COMPATIBILITY.md).

## Verified workflows

Automated against real Pulp containers (`tests/run-live.sh`):

| Scenario | Test |
|---|---|
| RPM: local repo, upload, remote, sync, inspect version, publish, distribute, fetch served metadata and package, update, publish, roll back, verify | `live_repos::rpm_lifecycle` |
| File: local repo, single and chunked upload, remove content, sync from manifest, publish, distribute, fetch served files, update, roll back, verify | `live_repos::file_lifecycle` |
| Content Views: RPM + File snapshot with filters, preview, numbered versions, Library, Dev, Prod path rule, rollback, served-content verification, retention protection, advisory filtering, partial failure and retry, resume of an interrupted publish (re-attach to the submitted task), cleanup | `live_cv::content_view_lifecycle` |
| Native replication pulp2 → pulp1, rerun, upstream update; fallback conflicts with native objects | `live_replication::native_replication` |
| Fallback replication: dry run, create, idempotent rerun, upstream update, loop and self guards | `live_replication::fallback_replication` |
| RBAC: 401, 403, no self-escalation, creator role, group object role, revoke | `live_rbac` |
| Server-side cancel of waiting and running tasks, 409 for completed tasks, TLS errors | `live_failures` |
| Shared database on PostgreSQL 16: `db migrate` creates the schema (and again changes nothing), an uninitialized, older or newer schema is refused with the right message, the database ID is created once | `live_shared_db::postgres_schema_lifecycle` |
| Two operators on one PostgreSQL database: B sees A's Content View and audit actor, lock conflict names A, B gets the lock after A releases, `break_lock` removes only the holder shown (a second call removes nothing), 8 simultaneous lock attempts give exactly one winner | `live_shared_db::two_operators_share_state_and_locks` |
| Every store method on PostgreSQL (definitions, versions, checkpoints, conditional writes after a lost lock, environments, bindings, audit, replication runs, tasks, prefix, export/import, delete cascade) and `db copy` from a local SQLite store | `live_shared_db::store_methods_on_postgres` |

Manually in the TUI (tmux, real servers): dashboard, repository create/search/upload,
remote create, mirror sync with warning, Task Center progress, distribute, versions,
compare, rollback with preview and verification, environments (protected Prod with typed
confirmation), Content View create/add repos/preview/publish/promote/drift check and
reconcile, restart with task re-attachment, fallback replication from another profile,
user creation and the self-delete guard.

By hand (not in `tests/run-live.sh`): `db migrate`, `import-cv` and `export-cv` with a
PostgreSQL role that has only `SELECT/INSERT/UPDATE/DELETE`; TLS to a PostgreSQL with a
private CA through a non-loopback address, with and without `ca_cert` (results in
[COMPATIBILITY.md](COMPATIBILITY.md)).

## Test results (last run)

* `cargo test --lib`: 117 passed
* `cargo test --test mock_http`: 17 passed
* Live (`tests/run-live.sh`): repos 2, cv 2, rbac 1, failures 2, replication 2,
  shared_db 3 (PostgreSQL 16) — all passed; cv and shared_db run again on 2026-10-24
  after the lock and config fixes — passed
* `cargo fmt --check`, `cargo clippy --all-targets -D warnings` (default and
  `--no-default-features`): clean
* `podman build -f Containerfile .`: built (29.5 MB image; no keyring; it contains the
  PostgreSQL driver and `pulp-tui db --help` runs)
* `cargo audit` (494 crates): no vulnerabilities; 3 "unmaintained" warnings, unchanged
  (RUSTSEC-2026-0370/0371/0372: `zvariant`, `zvariant_derive`, `zbus_names`), all
  reached only through the optional `keyring` feature (Secret Service over D-Bus). Builds
  with `--no-default-features` (the container image) do not contain them.
  `.cargo/audit.toml` ignores RUSTSEC-2023-0071 (`rsa`, Marvin attack): `rsa` is in
  `Cargo.lock` only because sqlx `any` names the optional `sqlx-mysql` driver; it is never
  compiled (`cargo tree -i rsa --target all` is empty).

## Limitations

* Content Views, environments and their history live in the local SQLite file of one
  operator, or, with `[database]`, in a database that several operators share (PostgreSQL,
  or SQLite on one host). There is no pulp-tui server: each pulp-tui connects directly.
  Other operators' changes show after a refresh.
* Client-orchestrated steps (publish/promote) continue only while pulp-tui runs; Pulp
  tasks always continue. Interrupted publishes resume with *Resume*, also by another
  operator when the database is shared. No daemon resumes them unattended.
* pulp-tui locks one Content View per workflow (expiry 2 minutes, renewal every 30 s, from
  the clients' clocks). Operations on the same repository from other tools, and from two
  databases that manage one Pulp server, are only serialized by Pulp's task resource locks
  (the later task waits; visible in Tasks).
* The `[database]` password cannot be stored in the keyring from the TUI (the **K** key
  stores profile secrets only); use `env:` or `file:`, or create the keyring entry
  (service `pulp-tui`, user `[database]`) with the system tool.
* The shared-database TLS path (private CA, non-loopback host) is checked by hand, not by
  `tests/run-live.sh`. `ca_cert` adds to the built-in Mozilla roots; the OS trust store is
  not used (COMPATIBILITY.md).
* Two live TUI sessions on a shared database were not run by hand; the lock logic is
  covered by the unit and live tests.
* Fallback replication synchronizes content only and never deletes.
* Composite Content Views and reusable filter sets are not implemented.
* Plugins other than RPM and File, content guard creation, signing service creation,
  OIDC token refresh: not supported.
* Not tested: Pulp domains, macOS at runtime (CI builds and runs unit tests), Windows.
* `nested_by_digest` RPM layout: served-content checks verify metadata only.

## Risks

* Native replication is a pulpcore technology preview; its API changed between versions
  (for example `policy`, `q_select`). pulp-tui reads the live schema, but older or newer
  servers may need adjustments.
* pulpcore serves files of superseded publications for
  `DISTRIBUTED_PUBLICATION_RETENTION_PERIOD` (default 3 days). After a rollback, clients
  that use repository metadata get the old content immediately; direct file URLs may
  still work during the grace period.
* RPM filters keep module artifact packages with their module, even if excluded.
* The derived repository and distribution names are derived from view, environment and
  repository names; renaming a source repository in Pulp does not rename them.
