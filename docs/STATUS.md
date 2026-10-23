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

Manually in the TUI (tmux, real servers): dashboard, repository create/search/upload,
remote create, mirror sync with warning, Task Center progress, distribute, versions,
compare, rollback with preview and verification, environments (protected Prod with typed
confirmation), Content View create/add repos/preview/publish/promote/drift check and
reconcile, restart with task re-attachment, fallback replication from another profile,
user creation and the self-delete guard.

## Test results (last run)

* `cargo test --lib`: 32 passed
* `cargo test --test mock_http`: 12 passed
* Live: repos 2, cv 1, rbac 1, failures 2, replication 2 — all passed
* `cargo fmt --check`, `cargo clippy --all-targets -D warnings` (default and
  `--no-default-features`): clean
* `cargo audit` (443 crates): no vulnerabilities; 3 "unmaintained" warnings
  (RUSTSEC-2026-0370/0371/0372: `zvariant`, `zvariant_derive`, `zbus_names`), all
  reached only through the optional `keyring` feature (Secret Service over D-Bus). Builds
  with `--no-default-features` (the container image) do not contain them.

## Limitations

* Content Views, environments and their history live in the local SQLite database of one
  operator. Several operators need the PostgreSQL service described in ARCHITECTURE.md.
* Client-orchestrated steps (publish/promote) continue only while pulp-tui runs; Pulp
  tasks always continue. Interrupted publishes resume with *Resume*.
* pulp-tui does not add its own locks: conflicting operations on the same repository are
  serialized by Pulp's task resource locks (the later task waits; visible in Tasks).
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
