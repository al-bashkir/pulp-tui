# Compatibility and capability matrix

## Tested versions

All live tests (`tests/run-live.sh`) passed on 2026-10-23 against the
`quay.io/pulp/pulp:latest` all-in-one image:

| Component | Version |
|---|---|
| pulpcore | 3.123.0 |
| pulp_rpm | 3.39.3 |
| pulp_file | 3.123.0 |
| Client OS | Fedora 44 (Linux 7.2), Rust 1.99 |

Other versions are not tested. pulp-tui detects capabilities at runtime and shows a
precise "not supported by this server: …" message when a screen or action needs
something the server does not have.

## Runtime detection

| Feature | Detected by | When missing |
|---|---|---|
| RPM screens | `status/` component `rpm` | screen shows "not supported: pulp_rpm plugin" |
| File screens | `status/` component `file` | same for pulp_file |
| RPM copy (Content Views, copy action) | OpenAPI path `rpm/copy/` | action fails with a precise error |
| Chunked upload | OpenAPI `uploads/` | — (large uploads need it) |
| Native replication | pulpcore ≥ 3.23 and OpenAPI `upstream-pulps/`, `{upstream_pulp_href}replicate/` | Replication screen points to the fallback |
| Replication deletion policy | OpenAPI field `UpstreamPulp.policy` | field hidden |
| Distribution filter for replication | field `q_select`, else `pulp_label_select` | matching field shown |
| RPM sync policies | enum `SyncPolicyEnum` | defaults to additive / mirror_complete / mirror_content_only |
| Repository form fields (checksum type, signing services, retain package versions, manifest) | OpenAPI schema fields | field hidden |
| Current user | `login/`, else (pulpcore without it, e.g. 3.63) `users/?username=` with the Basic-auth name | header shows "anonymous": no `login/` and the user cannot read `users/`, or bearer auth |
| Users/groups/roles | OpenAPI `users/`, `roles/` | screen shows not supported |

If the OpenAPI document cannot be read (for example a 403), checks are optimistic and the
server gives the final answer; the dashboard shows a warning.

## Verified behavior (live tests)

| Area | Status |
|---|---|
| RPM: create, upload (single + idempotent repeat), remote, sync (mirror_complete, additive), versions, diff, publish, distribute, served content (metadata + package digest), update, rollback | verified |
| File: create, upload (single request and chunked with progress), remove content (old versions unchanged), sync from manifest, publish, distribute, served manifest, update, rollback, mismatch detection | verified |
| Distribution base path conflicts | refused with 409 |
| Content Views: multi-repo snapshot, RPM/File filters, advisory filtering, numbered versions, Library auto-binding, promotion path rules, rollback, served-content verification, retention protection, partial failure + retry, resume with re-attach to a submitted task, version and view deletion | verified |
| Native replication: select by `q_select`, nodelete policy, rerun, upstream update | verified |
| Fallback replication: dry run, create, idempotent rerun, upstream update, conflict with native-replicated objects, loop guard, self-replication guard | verified |
| RBAC: 401 on wrong password, 403 without role, no self-escalation, model-level creator role, group object role, role removal | verified |
| Task cancellation (waiting and running), 409 on completed task, TLS errors, unreadable CA bundle | verified |
| Mock HTTP: pagination, foreign `next` links, 401/403/409, malformed JSON, read retries vs mutation non-retry, 429 Retry-After, timeouts, unreachable server, task polling, old-server capability negotiation, read-only blocking, size limits | verified |

## Known server behaviors that affect operators

* **Publication grace period.** pulpcore keeps serving files of a previously distributed
  publication for `DISTRIBUTED_PUBLICATION_RETENTION_PERIOD` (default 3 days). After a
  rollback, repository metadata (`repomd.xml`, `PULP_MANIFEST`) switches immediately, but
  direct URLs of files that exist only in the newer publication can still answer 200
  during this period. pulp-tui verifies rollbacks through metadata and says so in the
  rollback preview.
* **mirror_complete** RPM syncs create their own publication with upstream package paths.
  Served-content checks therefore try the layout path and the original `location_href`.
* **Native replication** pins distributions with `repository_version`, not a publication.
  pulp-tui shows these as pinned and re-points them correctly.
* **Task cancellation** is asynchronous. A task blocked in a network read can end as
  `failed` before the worker processes the cancel request; pulp-tui shows Pulp's final
  state. Cancelling a completed task returns 409 and is shown as such.
* `all_tasks_dispatched` of replication task groups stays false in 3.123; see
  `docs/ARCHITECTURE.md` for how completion is detected.
* `api_root` of an Upstream Pulp is relative and needs a trailing slash (`pulp/`).

## Unsupported or limited

* Plugins other than pulp_rpm and pulp_file (container, deb, python, …) are not managed.
* RPM publication layout `nested_by_digest`: the package probe of served-content checks is
  skipped (metadata is still checked).
* Signing: repository fields for signing services are editable, but pulp-tui does not
  create signing services (that requires server-side scripts).
* Content guards are only shown and set by href; they are not created or replicated.
* Composite Content Views and reusable filter sets are not implemented yet. The data model
  keeps filters per view repository, so a later "filter set" table can be referenced
  without changing published versions.
* The fallback replication does not delete destination objects and does not copy Pulp
  metadata; use native replication for full metadata replication.
* Pulp domains: the profile `domain` setting prefixes all API paths. It is implemented but
  was not tested against a domain-enabled server.
* Bearer tokens are sent as given; token refresh (OIDC) is not implemented.
* Windows builds are not tested.
