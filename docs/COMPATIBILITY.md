# Compatibility and capability matrix

## Tested versions

All live tests (`tests/run-live.sh`) passed on 2026-10-25 against the
`quay.io/pulp/pulp:latest` all-in-one image:

| Component                             | Version                                                     |
| ------------------------------------- | ----------------------------------------------------------- |
| pulpcore                              | 3.123.0                                                     |
| pulp_rpm                              | 3.39.3                                                      |
| pulp_file                             | 3.123.0                                                     |
| pulp_deb                              | 3.11.1                                                      |
| PostgreSQL (optional shared database) | 16.15 (`docker.io/library/postgres:16`), through sqlx 0.8.6 |
| Client OS                             | Fedora 44 (Linux 7.2), Rust 1.99                            |

On 2026-10-30, all live scenarios, `real` included (Debian `main`, Rocky 9 BaseOS and
AppStream), passed on fresh containers of these all-in-one images:

| Image                       | pulpcore / pulp_file | pulp_rpm | pulp_deb | `real` |
| --------------------------- | -------------------- | -------- | -------- | ------ |
| `quay.io/pulp/pulp:3.63.36` | 3.63.36              | 3.27.12  | 3.5.4    | 807 s  |
| `quay.io/pulp/pulp:3.95.3`  | 3.95.3               | 3.34.0   | 3.8.1    | 804 s  |
| `quay.io/pulp/pulp:latest`  | 3.123.1              | 3.39.5   | 3.11.1   | 680 s  |

On 3.95.3, one `native_replication` run failed once with "connection closed before
message completed" on `POST upstream-pulps/`; the next two runs passed.

The nginx of the 3.63.36 image listens on IPv4 only. With rootless podman (pasta),
`localhost` on the host goes to `::1` and the connection is reset, so the test
environment uses `127.0.0.1`.

Other versions are not tested. pulp-tui detects capabilities at runtime and shows a
precise "not supported by this server: …" message when a screen or action needs
something the server does not have.

## Runtime detection

| Feature                                                                                     | Detected by                                                                                 | When missing                                                                                                                                                                                          |
| ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| RPM screens                                                                                 | `status/` component `rpm`                                                                   | screen shows "not supported: pulp_rpm plugin"                                                                                                                                                         |
| File screens                                                                                | `status/` component `file`                                                                  | same for pulp_file                                                                                                                                                                                    |
| Deb screens                                                                                 | `status/` component `deb`                                                                   | same for pulp_deb                                                                                                                                                                                     |
| Deb Content View signing (`signing_service`, `signing_service_release_overrides`)           | the field is in the JSON of the derived repository                                          | the field is not sent; a chosen signing service stops the publish with "not supported by this server"                                                                                                 |
| RPM copy (Content Views, copy action)                                                       | `status/` component `rpm` and OpenAPI path `rpm/copy/`                                      | the copy action shows "not supported by this server" before its form; a Content View publish gets the server's error                                                                                  |
| Deb copy (copy action, Add to repository, Content Views)                                    | `status/` component `deb` and OpenAPI path `deb/copy/`                                      | the copy action, Add to repository and Publish of a Content View with a Deb repository show "not supported by this server" before their form; Resume of such a version stops before it sends anything |
| Chunked upload                                                                              | OpenAPI `uploads/` and `{upload_href}`                                                      | an upload larger than one chunk (`upload_chunk_mb`) shows "not supported by this server" before it sends anything; smaller files do not need it                                                       |
| Native replication                                                                          | pulpcore ≥ 3.23 and OpenAPI `upstream-pulps/`, `{upstream_pulp_href}replicate/`             | Replication screen points to the fallback                                                                                                                                                             |
| Replication deletion policy                                                                 | OpenAPI field `UpstreamPulp.policy`                                                         | field hidden                                                                                                                                                                                          |
| Upstream Pulp download settings (replica download policy, concurrency, retries, timeouts)   | OpenAPI fields of `UpstreamPulp`; choices: the enum of the `remote_policy` `$ref`           | field hidden; choices default to immediate / on_demand / streamed                                                                                                                                     |
| Distribution filter for replication                                                         | field `q_select`, else `pulp_label_select`                                                  | matching field shown                                                                                                                                                                                  |
| Choices of the remote download policy and the replication deletion policy                   | the enum that the `$ref` of the `policy` property points to (not its generated name)        | defaults to immediate / on_demand / streamed and nodelete / labeled / all                                                                                                                             |
| Fallback replication of a type                                                              | `status/` component of the type on the source and on the destination                        | the dry run stops with "<profile> (source): not supported by this server: pulp_file plugin" before it lists or changes anything                                                                       |
| RPM sync policies                                                                           | the enum of the `$ref` of `RpmRepositorySyncURL.sync_policy` (not its generated name)       | defaults to additive / mirror_complete / mirror_content_only                                                                                                                                          |
| Sync options `optimize` (RPM, File, Deb), `skip_types` (RPM)                                | fields of `{Rpm,File,Apt}RepositorySyncURL`; the enum of the `skip_types` items `$ref`      | field hidden: the sync uses the server default (skip types without the enum: `srpm`, `treeinfo`)                                                                                                      |
| Repository form fields (checksum type, signing services, retain package versions, manifest) | OpenAPI schema fields                                                                       | field hidden                                                                                                                                                                                          |
| RPM repository: compression, layout, repo config, package signing                           | fields of `rpm.RpmRepository`                                                               | field hidden                                                                                                                                                                                          |
| Repository field `retain_checkpoints` (RPM, File, Deb)                                      | field of `rpm.RpmRepository`, `file.FileRepository`, `deb.AptRepository`                    | field hidden                                                                                                                                                                                          |
| Choices of the RPM checksum type, compression and layout                                    | the enum of the property's `$ref` (not its generated name, e.g. `LayoutD5aEnum`)            | only empty (the server default) and the current value                                                                                                                                                 |
| Remote timeouts, `max_retries`, `headers`; Deb remote `excluded_package_metadata_fields`    | fields of `rpm.RpmRemote`, `file.FileRemote`, `deb.AptRemote`                               | field hidden                                                                                                                                                                                          |
| Distribution `hidden`, `checkpoint`; RPM `generate_repo_config`                             | fields of `rpm.RpmDistribution`, `file.FileDistribution`, `deb.AptDistribution`             | field hidden                                                                                                                                                                                          |
| Publication options (Publications **c**)                                                    | fields of `rpm.RpmPublication`, `file.FilePublication`, `deb.AptPublication`                | field hidden; the choices of checksum, compression and layout come from the enum of the property's `$ref` (else only empty: the server default)                                                       |
| Current user                                                                                | `login/`, else (pulpcore without it, e.g. 3.63) `users/?username=` with the Basic-auth name | header shows "anonymous": no `login/` and the user cannot read `users/`, or bearer auth                                                                                                               |
| Users/groups/roles                                                                          | OpenAPI `users/`, `roles/`                                                                  | screen shows not supported                                                                                                                                                                            |
| Pulp exporters                                                                              | OpenAPI `exporters/core/pulp/`                                                              | screen shows not supported                                                                                                                                                                            |
| Filesystem exporters; their methods                                                         | OpenAPI `exporters/core/filesystem/`; the enum of the `$ref` of `FilesystemExporter.method` | screen shows not supported; the choices default to write / hardlink / symlink                                                                                                                         |
| Pulp importers and the import check                                                         | OpenAPI `importers/core/pulp/` and `importers/core/pulp/import-check/`                      | screen shows not supported                                                                                                                                                                            |
| Domain scope of a role assignment                                                           | `status/` `domain_enabled: true`                                                            | the Assign role form has no Domain field                                                                                                                                                              |

If the OpenAPI document cannot be read (for example a 403), checks are optimistic and the
server gives the final answer; the dashboard shows a warning.

## Verified behavior (live tests)

| Area                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | Status                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| RPM: create, upload (single + idempotent repeat), remote, sync (mirror_complete, additive), versions, diff, publish, distribute, served content (metadata + package digest), update, rollback                                                                                                                                                                                                                                                                                                                                                                                                                                                           | verified               |
| File: create, upload (single request and chunked with progress), remove content (old versions unchanged), sync from manifest, publish, distribute, served manifest, update, rollback, mismatch detection                                                                                                                                                                                                                                                                                                                                                                                                                                                | verified               |
| Distribution base path conflicts                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | refused with 409       |
| Distribution `base_path` PATCH (environment relabel): runs as a task (`ageneral_update`), moves the distribution in place (same href, same publication), the new path answers 200 and the old path 404 as soon as the task ends (no content-cache delay seen); a promotion after the relabel serves at the new label                                                                                                                                                                                                                                                                                                                                    | verified               |
| Content Views: multi-repo snapshot, RPM/File filters, advisory filtering, numbered versions, Library auto-binding, promotion path rules, rollback, served-content verification, retention protection, partial failure + retry, resume with re-attach to a submitted task, version and view deletion                                                                                                                                                                                                                                                                                                                                                     | verified               |
| Deb (`live_deb`): mirror sync of the fixture, filtered structured copy, publication, distribution, served `Release`, `Packages` index and package digest; a publication with the test signing service is reused only for that service and serves `Release.gpg` and `InRelease`; a Deb Content View with `exclude name=*-dbg`, signed with a service, unsigned (`none`), resumed from a checkpoint with the service, promoted to Dev, and used by a composite; a per-release key of the source signs a `same as source` publish and a `none` publish removes it                                                                                          | verified               |
| Composite Content Views (`live_cv::composite_content_view_rocky9`): one publication served by two distributions at once (the component's Library distribution and the composite's own), auto publish after the extras view publishes, deleting a composite version or a composite keeps the publications and repositories of its components                                                                                                                                                                                                                                                                                                             | verified               |
| Native replication: select by `q_select`, nodelete policy, rerun, upstream update                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | verified               |
| Fallback replication: dry run, create, idempotent rerun, upstream update, conflict with native-replicated objects, loop guard, self-replication guard                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | verified               |
| Repository and remote definitions (`export-repos` on pulp2, `import-repos` on pulp1): RPM and Deb remotes with their repositories and a File repository, no secret in the file, the repository's remote found by name, idempotent rerun                                                                                                                                                                                                                                                                                                                                                                                                                 | verified               |
| Pulp export (`live_export`): full export of the latest version of a File repository, the TOC and output files, the exporter's `last_export`, export record delete (204), exporter delete (task); incremental export: 400 without a previous export; after a full export and a new file it exports the latest version, and its import gives the copy the new file; an export of a chosen older version (`versions`) exports that version; an incremental export with `start_versions` exports the latest version, and `start_versions` without `full: false` answers 400                                                                                 | verified               |
| Filesystem export (`live_export`): a File publication found as the newest of the latest version and exported; a second export into the same directory fails (publication and version), an export after a PATCH of the directory works                                                                                                                                                                                                                                                                                                                                                                                                                   | verified               |
| Pulp import (`live_export`, on the server of the export): import check valid, and not valid for a missing TOC; import of the File export through an importer mapping into another repository: the task group ends (without `all_tasks_dispatched`) with one `import_repository_version` task, and the copy has the content count of the exported version; the imports list names the TOC; importer delete (204)                                                                                                                                                                                                                                         | verified               |
| Orphan cleanup (**O** on Repository Versions, TUI on pulp1, 1440 minutes): count, typed confirm, `orphan_cleanup` task, notice from its progress reports                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | verified once (manual) |
| RBAC: 401 on wrong password (any authenticated endpoint; pulpcore 3.63 has no `login/`), 403 without role, no self-escalation, model-level creator role, group object role, role removal                                                                                                                                                                                                                                                                                                                                                                                                                                                                | verified               |
| Task cancellation (waiting and running), 409 on completed task, TLS errors, unreadable CA bundle                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | verified               |
| Mock HTTP: pagination, foreign `next` links, 401/403/409, malformed JSON, read retries vs mutation non-retry, 429 Retry-After, timeouts, unreachable server, task polling, old-server capability negotiation, read-only blocking, size limits                                                                                                                                                                                                                                                                                                                                                                                                           | verified               |
| Shared database on PostgreSQL 16 (`live_shared_db`): explicit schema creation (`db migrate`), refusal of an uninitialized, older and newer schema, two operators on one database, lock conflict and release, 8 simultaneous acquisitions of a free lock (exactly one wins), every store method, composite views (pins, borrowed rows, the foreign keys that refuse deleting a component, `db copy` of a composite), `db copy` from a local file, a profile with its own database next to a local profile, the migration of schema 2 rows to schema 3 (labels `library`/`dev`/`dev` for `Library`/`Dev`/`dev`, a served path kept, a used label refused) | verified               |
| Shared database with a role that has only `SELECT/INSERT/UPDATE/DELETE` (manual): import, export, `db migrate` on a current schema                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | verified               |
| Shared database TLS: `ca_cert` against a server with a private CA, connected through a non-loopback address (`verify-full`), see "Database TLS"                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | verified once (manual) |

## Known server behaviors that affect operators

* **Publication grace period.** pulpcore keeps serving files of a previously distributed
  publication for `DISTRIBUTED_PUBLICATION_RETENTION_PERIOD` (default 3 days). After a
  rollback, repository metadata (`repomd.xml`, `PULP_MANIFEST`) switches immediately, but
  direct URLs of files that exist only in the newer publication can still answer 200
  during this period. pulp-tui verifies rollbacks through metadata and says so in the
  rollback preview.
* **Purged tasks.** `tasks/purge/` deletes finished tasks only; a GET of a purged task
  answers 404, as does a task that the user may not view (RBAC scopes the task list). So a
  task href that pulp-tui recorded can answer 404 although the task ran. The Task Center
  shows it as `gone`; a Content View *Resume* clears the checkpoint and sends that step
  again (unit-tested against a mock server, not in the live suite).
* **Restore as latest** (`modify/` with only `base_version`). pulpcore 3.63.36, 3.95.3 and
  3.123.1 have `base_version` in the modify request (`RepositoryAddRemoveContentSerializer`),
  and the modify views of pulp_rpm, pulp_file and pulp_deb pass it on (read in the three
  images), so pulp-tui does not check for it. The new
  version starts from the content of the latest version, then removes what the base version
  lacks and adds what it has (`Repository.new_version`); so it has every content type of the
  base version, also the Deb release records. Exception: pulp_deb 3.11.1 removes the upstream
  metadata units (`ReleaseFile`, `PackageIndex`, `InstallerFileIndex`) from every new version
  (`AptRepository.initialize_new_version`); only a sync adds them again. A restored Deb version
  therefore lacks them; an APT publication does not use them. The restore preview leaves them
  out of the new version and says so. A version without a change is deleted at once
  (`RepositoryVersion.__exit__`): the task completes with no created version. A new version
  runs the retention cleanup (`retain_repo_versions`) and autopublish (`on_new_version`), as
  a sync does. Verified in `live_repos` (File, RPM) and `live_deb` (3.123.1).
* **Deleting a served publication or repository version.** In pulpcore 3.123.0
  (`pulpcore/app/models/publication.py`, read in the test container),
  `Publication.repository_version` is `on_delete=CASCADE`: deleting a repository version
  deletes its publications. `Distribution.publication` is `on_delete=SET_NULL`: deleting a
  publication does not fail and does not warn, the distribution silently stops serving
  (it is not refused when a distribution serves it). Because of this, pulp-tui does not
  delete a served object. One publication may be served by several distributions: a
  composite Content View serves the publication of its component under its own path while
  the component serves it too. `delete_version` keeps a publication (and its repository
  version) that any binding of the server still serves, and `delete_view` refuses to delete
  the derived repository of a publication that another view still serves.
* **Protected repository versions.** pulpcore 3.123.0 refuses to delete a repository
  version with HTTP 400 ("currently being used to distribute content";
  `RepositoryVersionViewSet.destroy`, `Repository.protected_versions()`) when a
  distribution has `repository_version` = the version, or `publication` = one of its
  publications, or follows the repository (`repository`) and: with `checkpoint: true`, the
  version has a checkpoint publication; with `checkpoint: false`, the version is the
  highest-numbered version that has a complete publication (File, RPM and Deb distributions
  serve from publications). So the latest version without a publication can be deleted,
  and an older published version that nothing pins can be deleted too. The version delete
  preview applies the same rules (`ops::protecting`) and does not offer the delete for a
  protected version. `checkpoint` on publications and distributions is new in pulpcore
  (migration 0131); older servers lack it and pulp-tui reads it as `false`.
* **mirror_complete** RPM syncs create their own publication with upstream package paths.
  Served-content checks therefore try the layout path and the original `location_href`.
  The RPM publication response has no `distributions` field (the File one has), and the
  distribution list has no `publication` filter: pulp-tui lists the distributions of the
  type and matches the bindings in the client before it deletes a publication, a version
  or a repository (checked on 3.123.0 / pulp_rpm 3.39.3).
* **Sync options.** pulp_rpm 3.39.5 (`viewsets/repository.py:251-264`, HTTP 400) refuses a
  `mirror_complete` sync with `skip_types` or with an `autopublish` repository, and both
  mirror policies with `retain_package_versions` above 0; the sync form refuses these pairs
  before it sends anything (it reads the two settings from the repository row). `optimize`
  (RPM and Deb, default `true`) and
  `skip_types` (RPM, `srpm` and `treeinfo`) are in the sync request of every tested version
  (pulp_rpm 3.27.12 to 3.39.5, pulp_deb 3.5.4 to 3.11.1). pulp_file has `optimize` (default
  `true`) in `FileRepositorySyncURL` too (pulpcore 3.123.1, `pulp_file/app/viewsets.py`).
* **Native replication** pins distributions with `repository_version`, not a publication.
  pulpcore 3.95.3 (`app/replica.py`) does not pin: its distributions follow the repository
  (`repository`, `publication: null`).
  pulp-tui shows these as pinned and re-points them correctly. A distribution PATCH is
  merged with the stored object (pulpcore `serializers/publication.py`): a body with
  `publication` but without `repository_version: null` gets HTTP 400, and
  `{publication: null, repository: null}` answers 200 but keeps serving the version. Every
  binding change of pulp-tui therefore also sends `repository_version: null`.
* **A distribution that follows a repository** serves the newest complete publication of
  the repository (pulpcore `models/publication.py`: ordered by repository version, then
  creation), not the latest version. Verify compares with that publication. pulpcore
  reads the base `Publication` model (`get_repository_publication_and_version`), so for Deb
  a newer verbatim publication wins over the APT ones; Verify lists both types.
* **RPM publish settings** (`metadata_signing_service`, `checksum_type`, `repo_config`,
  `compression_type`, `layout`) come from the repository when the publication request does
  not set them (pulp_rpm `viewsets/repository.py`). A Content View publish copies them from
  the source repository to the snapshot repository first.
* **File publish takes no setting from the repository** (pulp_file of pulpcore 3.123.1,
  `serializers.py` `FilePublicationSerializer`, `models.py` `FileRepository.on_new_version`):
  a publication request without `manifest` gets `PULP_MANIFEST`; the repository `manifest`
  is used only by autopublish. `manifest: null` publishes no manifest. pulp-tui sends the
  repository's `manifest` with every File publication it creates (a Content View snapshot
  repository gets the `manifest` of its source first), and Verify fetches the
  manifest that the served publication names (with none, it reports that it compared no
  files).
* **File, RPM and Deb distributions serve only publications** (`SERVE_FROM_PUBLICATION =
  True` in pulp_file, pulp_rpm and pulp_deb; pulpcore 3.123.1
  `Distribution.get_repository_publication`): a distribution bound to a repository version
  serves the newest complete publication of that version, and with none every path is 404.
  Verify and the rollback preview then say that the distribution serves nothing.
* **RPM content types.** pulp_rpm 3.39.5 has 11 content types (OpenAPI
  `content/rpm/...`). A `rpm/comps/` upload adds package groups, categories, environments and
  langpacks; langpacks have no id (`unique_together` is the digest), so the compare keys
  them by `digest`. The version compare and the Content browser cover all 11 types. Compare
  also reads `content_summary.present` of both versions and warns when Pulp counts other
  units than it lists. A content type without its endpoint in the OpenAPI (an older plugin)
  is not read; the compare and the restore preview name it.
* **`sha256` of RPM packages and File content** comes from the stored file (pulpcore
  `ContentChecksumSerializer`; read in pulp_rpm 3.39.3 and pulp_file 3.123.0), so it is
  `null` for `on_demand` and `streamed` content until a client downloads it. Verify then
  takes the RPM digest from `pkgId` when `checksum_type` is `sha256` (else it fails), and
  compares a File digest of `PULP_MANIFEST` only where the API has one (the paths always).
* **`PULP_MANIFEST` lines are not quoted** (pulp_file 3.123.0 `manifest.py`): a line is
  `relative_path,sha256,size`, written with `",".join(...)` and read with
  `rsplit(",", maxsplit=2)`. A path can contain commas. Verify reads each line from the
  right too.
* **`rpm/copy/` with a `content` list adds related units** (pulp_rpm 3.39.3
  `tasks/copy.py`, `find_children_of_content`, also without dependency solving): the
  packages and modules that a copied advisory lists, the groups of copied comps categories
  and environments, and the newest package of each group member whose name the request
  does not have. It does not add the packages of a module. The check runs per request, so a
  list sent in batches gets other units than one request. A filtered RPM Content View
  snapshot therefore adds its units with `modify/`, which adds exactly the given units.
* **A content change that changes nothing creates no version** (pulpcore 3.123
  `RepositoryVersion.__exit__` deletes it, and its `CreatedResource` with it): the
  `modify/` task completes with an empty `created_resources`, for example when the units to
  remove are not in the latest version. After Remove or Add to repository, pulp-tui reads
  the new version from `created_resources` and says "no new version" when it is empty
  (checked on pulp1 with a File repository).
* **Task cancellation** is asynchronous. A task blocked in a network read can end as
  `failed` before the worker processes the cancel request; pulp-tui shows Pulp's final
  state. Cancelling a completed task returns 409 and is shown as such.
* **Skipped tasks.** `skipped` is a final task state in pulpcore (`TASK_FINAL_STATES`),
  but no code of pulpcore 3.123.1 or of the RPM, File and Deb plugins sets it (read in the
  source on pulp1). pulp-tui treats a skipped task as not run: the workflow step fails
  (Content View copy or publication, fallback replication sync) and nothing is published.
* `all_tasks_dispatched` of replication task groups stays false in 3.123; see
  `docs/ARCHITECTURE.md` for how completion is detected.
* **`task-groups/` has no filters** in 3.123 (only `limit`, `offset`, `fields`,
  `exclude_fields` in the live OpenAPI) and lists the newest first. The replication history
  pages through it and keeps the groups whose description is `Replication of <upstream>`:
  it stops at five matches or after 2000 groups.
* **Canceled replication tasks.** On 3.63 `replicate_distributions` sets
  `all_tasks_dispatched` itself, so a canceled sync leaves a group with `failed 0,
  canceled 1`. On 3.95 and 3.123 `finalize_replication` fails when another task of the
  group did not complete, but a canceled `replicate_distributions` dispatches no finalizer:
  the group ends with only `canceled 1`. pulp-tui reports canceled or skipped tasks as not
  complete (read in the pulpcore source of the three images).
* **Task groups in the Task Center.** The group counters, `all_tasks_dispatched` and
  `tasks` exist on 3.63 too. Every supported version ends a replication group by a rule of
  `TaskGroup::is_final` (3.63: `all_tasks_dispatched`; 3.95 and 3.123: the
  `finalize_replication` task, or a failed or canceled task), never only by the three-poll
  rule; an import group ends by its completed `pulp_import` task (see "Pulp import"). Cancel of a group (`PATCH {task_group_href}`, `task_groups_cancel`) exists on 3.95
  and 3.123, not on 3.63. pulp-tui reads the `patch` operation of `{task_group_href}` from
  the OpenAPI: without it, **X** on a replication row says "not supported by this server" and sends
  nothing; with it, the confirmation reads the group first (description, unfinished tasks).
  pulpcore `cancel_task_group` (`tasking/tasks.py`) cancels every waiting or running task of
  the group and sets `all_tasks_dispatched`. The row keeps the state that Pulp reports.
* `api_root` of an Upstream Pulp is relative and needs a trailing slash (`pulp/`).
* **Clearing remote fields.** pulpcore `RemoteNetworkConfigSerializer` (`ca_cert`,
  `client_cert`, `client_key`, `username`, `password`, `proxy_username`,
  `proxy_password`), pulp_rpm `sles_auth_token` and pulp_deb `gpgkey` have
  `allow_null=True`, and the repository `remote` is nullable (3.123.0 OpenAPI). A PATCH
  with `null` unsets the value (`hidden_fields` shows `is_set: false`; checked live for a
  File remote and repository). The remote and repository Edit forms send `null` for `-`
  and for a cleared Remote.
* **Readable remote secrets.** The write-only fields of a remote (`username`, `password`,
  `client_key`, `proxy_username`, `proxy_password`) are never in a GET answer;
  `hidden_fields` shows which are set. pulp_rpm 3.39.3 `sles_auth_token` is not write-only
  (`serializers/repository.py`): a GET returns it. `headers` can hold an `Authorization`
  header. pulpcore 3.123.0 refuses credentials in `url` ("The remote url contains username
  or password") and in `proxy_url` (`serializers/base.py`), but older servers can have them.
  `export-repos` leaves out all of these.
* **Remote URL scheme.** The remote `url` of pulpcore 3.123.1 is a `CharField`: Pulp saves
  any scheme (a slip like `host:8000/x` has the scheme `host`) and only the sync fails ("URL
  ... not supported"). The downloaders handle `http`, `https` and `file`; a `file://` path
  must be under `ALLOWED_IMPORT_PATHS` (`validate_url`). The remote form accepts only these
  three schemes, and the Upstream Pulp form only `http` and `https`. `proxy_url` is a
  `CharField` too ("Format: scheme://host:port", `serializers/base.py`): Pulp saves
  `proxy.corp:3128` (scheme `proxy.corp`). The remote form accepts a proxy only with `http`
  or `https`; an unchanged value on edit is not sent and not checked.
* **Orphan cleanup.** `POST orphans/cleanup/` (pulpcore 3.123.0) takes
  `orphan_protection_time` (minutes; without it the server setting
  `ORPHAN_PROTECTION_TIME` applies, which the API does not show) and returns a task
  (`orphan_cleanup`) with the progress reports `clean-up.content` and
  `clean-up.artifacts`. `content/?orphaned_for=N` counts the content that a cleanup with
  the same N deletes. Protection counts from the content's `timestamp_of_interest`, not from
  when it became an orphan: on pulp1, 142,224 units were orphans at `orphaned_for=0` and none
  at 1440, because all of them were created that day. The old `DELETE orphans/` still
  exists; pulp-tui does not use it.
* **Pulp export** (pulpcore 3.123.1, `serializers/exporter.py`, `tasks/export.py`). The
  `path` of an exporter must start (after `realpath`) with an entry of
  `ALLOWED_EXPORT_PATHS`, which is empty by default: then Pulp refuses every exporter with
  "Path '...' is not an allowed export path". The export task creates the directory. A full
  export without `versions` exports the latest version of each repository of the exporter
  and becomes its `last_export`. It writes `export-<id>-<YYYYMMDD_HHMM>.tar` (with
  `chunk_size`: `.tar.0000`, `.tar.0001`, ...) and `export-<id>-<YYYYMMDD_HHMM>-toc.json`
  (file names and CRC32 checksums). A PATCH or DELETE of an exporter answers 202 with a
  task; a DELETE of an export answers 204. Deleting an export, or an exporter with its
  exports, deletes only the database records, never the files. Only a successful export
  becomes `last_export`; deleting that export record sets `last_export` to null.
* **Incremental Pulp export** (`full: false`, same files). Without `start_versions`, Pulp
  starts at the versions of the exporter's `last_export` (its `exported_resources`). With
  neither, the POST answers 400 "Incremental export can only be requested when there is a
  previous export or start_versions= has been specified." (`PulpExportSerializer.validate`).
  When a repository was added to the exporter after the last export, the POST answers 202
  but the task fails with only the repository as the error (`<Repository ...: pk=...>`, a
  `KeyError` in `tasks/export.py` `_version_match`; verified live). The export holds only
  the artifacts that the start version lacks, but the content metadata of the whole
  version, so its import into the same server sets the full content (verified live).
* **Pulp export `versions` and `start_versions`** (same files,
  `_validate_versions_to_repos`). Each list must have exactly one version per repository of
  the exporter: a count that differs answers 400 "Number of versions (N) does not match the
  number of Repositories (M)" (pulpcore puts the two numbers in the wrong order), and a set
  of repositories that differs (a version of another repository, or two versions of one)
  answers 400 "Requested RepositoryVersions must belong to the Repositories named by the
  Exporter!". A version that does not exist answers 400 "Invalid hyperlink - Object does
  not exist." (all verified live). `start_versions` with `full` true (the default) answers
  400 "start_versions is only valid for incremental exports (full=False)"; with
  `full: false` it replaces the versions of `last_export`, so it also works without a
  previous export.
* **Filesystem export** (same files). The export takes exactly one of `publication` and
  `repository_version`, and the exporter path must be in `ALLOWED_EXPORT_PATHS` (checked
  again when the export is posted). The task makes the directory with `os.makedirs` without
  `exist_ok`, so the directory must not exist: a second export into it fails, for a
  publication with "Cannot export to directories that contain existing data", for a
  repository version with "[Errno 17] File exists" (verified live). `hardlink` falls back
  to `write` when the export directory is on another file system than the Pulp storage;
  `hardlink` and `symlink` need the file system storage.
* **Pulp import** (pulpcore 3.123.1, `serializers/importer.py`, `views/importer.py`,
  `tasks/importer.py`, `models/importer.py`). `toc` and `path` must start (after `realpath`)
  with an entry of `ALLOWED_IMPORT_PATHS` (empty by default). `import-check/` answers 200
  with `is_valid` and `messages` per value: for the TOC it checks the allowed path, that the
  file exists and can be read, and that Pulp can write to its directory; for
  `repo_mapping` (a JSON string) only that it is JSON. The import (`{importer}imports/`)
  answers 202 with a task group `Import of <toc>`: `pulp_import` checks the chunks, imports
  the artifacts and dispatches one `import_repository_version` task per repository of the
  export before it completes. The group never gets `all_tasks_dispatched` (only a cancel
  sets it; verified live), so `TaskGroup::is_final` also ends a group whose `pulp_import`
  task completed when no task waits or runs. Each repository task sets the content of a new
  version of the destination to exactly the exported content (`set_content`). The
  destination is the pair of `repo_mapping`, else the repository of the same name; a
  missing one is skipped with only a log line, so such an import completes without a
  repository task. The mapping: a create with an unknown destination is refused (400,
  "Failed to find repositories from repo_mapping"); only `create` handles it, so a PATCH
  with an unknown destination answers 500 and leaves `repo_mapping` empty (verified live).
  Deleting a destination repository removes its pair from the mapping (cascade).
  `create_repositories` is refused together with a mapping; pulp-tui does not send it. A
  DELETE of an importer answers 204 and deletes its import records.

## pulp_deb

Read in the test container (pulp_deb 3.11.1, `pulp_deb/app/`) and in its live OpenAPI:

* The package name field of `content/deb/packages/` is `package` (not `name`). The other
  fields pulp-tui reads are `version`, `architecture`, `relative_path` and `sha256`.
* `sha256` comes from the stored file (pulpcore `ContentArtifactChecksumField`), so it is
  `null` for an `on_demand` package until a client downloads it. Verify therefore takes the
  digest from the `SHA256:` line of the served `Packages` index (what apt checks), and fails
  when the API has a digest that is different.
* `deb/copy/` with `structured: true` also copies the records that a structured publication
  needs: the package-release-component (PRC) units of the copied packages, their release
  components, release architectures and releases (`tasks/copy.py`,
  `find_structured_publish_content`). pulp-tui always sends `structured: true`.
* A structured publication lists every PRC in the version and loads its package by id; it does
  not check that the package is in the version (`tasks/publishing.py`). The `modify/` endpoint
  of an APT repository runs `signed_add_and_remove` (`tasks/signing.py`). Its
  `_prepare_package_removals` adds the PRCs (and source PRCs) of each removed package in the
  base version to the removal. Seen live: removing a package alone took its PRC count from 1
  to 0. pulp-tui still sends the binary-package PRCs of a removed package itself (Content View
  snapshots and Remove content), so a pulp_deb version without that step (not checked) does not
  keep publishing the package. pulp-tui keeps releases, release components and release
  architectures, which all packages of a release share.
* `content/deb/source_packages/` answers HTTP 500 (`KeyError: 'artifacts'`) to a `fields`
  value without `artifacts`, even for an empty page: `DscFileSerializer.__init__` sets
  `self.fields["artifacts"].read_only` (`serializers/content_serializers.py`). The version
  compare asks for `source,version,artifacts`. Seen live in `live_deb`.
* pulp_deb 3.11.1 has 13 content types (OpenAPI `content/deb/...`). A sync makes one PRC per
  package, component and index architecture (`models/content/structure_content.py`
  `unique_together` has `index_architecture`), and the API does not show
  `index_architecture`. The index and Release file units (`package_indices`, `source_indices`,
  `installer_file_indices`, `release_files`) show `relative_path` and `artifacts`, not the
  `sha256` that makes them unique. The version compare therefore keys these units, and the
  PRCs, by their unit id. With these keys the compare lists as many units as
  `content_summary.present` counts (checked in `live_deb` on the fixture).
* `dependency_solving: true` is not implemented: the task fails with
  `FeatureNotImplementedError`. pulp-tui always sends `false`.
* `dest_base_version: 0` starts the new version from the empty version 0 of the destination
  (the viewset turns the number into the id of that version).
* A Release file is signed with the first service that exists in this order: the signing
  service of the publication, then the release override of the repository
  (`signing_service_release_overrides`), then the signing service of the repository
  (`publishing.py`, `AptRepository.release_signing_service`).
* A PATCH of `signing_service_release_overrides` merges
  (`serializers/repository_serializers.py`, `_update_signing_service_overrides`): only the
  distributions in the body change, a `null` value deletes that override, and `{}` changes
  nothing. So pulp-tui sends `null` for each per-release key that a Content View snapshot
  repository must lose (signing `none` or a service, or a key the source no longer has).
  The Deb repository edit form does the same for each release that the operator removes.
  `package_signing_fingerprint_release_overrides` merges the same way
  (`_update_package_signing_fingerprint_overrides`), but its values are not nullable: an
  empty string deletes the override. The edit form sends `""` for each removed release.
* Repository signing fields: `signing_service`, `signing_service_release_overrides` and
  `publish_upstream_release_fields` (model default `true`) are in pulp_deb 3.5.4, 3.8.1 and
  3.11.1. `package_signing_service`, `package_signing_fingerprint` and
  `package_signing_fingerprint_release_overrides` are only in 3.11.1 (migration
  `0035_package_signing`). `excluded_package_metadata_fields` (a list, model default `[]`;
  the default of a publication that does not set its own) is in 3.11.1 (migration
  `0042_add_excluded_package_metadata_fields`); an edit clears it with `[]`. `signing_service` accepts only an APT release signing service,
  `package_signing_service` only an APT package signing service. The package signing service
  signs packages on upload and `modify/` (`tasks/signing.py`), not on sync or `deb/copy/`.
* `AptPublication` defaults are `structured=true` and `simple=false`.
* `signing-services/` is the pulpcore endpoint for all plugins. It has no type filter (only
  `name`, `q`, `pulp_href__in` and similar), so a Deb signing service cannot be told from an
  RPM one by a query.
* A publication signed with the keys of its repository stores `signing_service: null`, the
  same as an unsigned one. A `deb/copy/` with the same content as the latest version makes
  no new version, so two Content View versions with other signing choices can have the same
  derived version. Seen live: after a signed publish, a `none` publish of the same content
  reused the signed publication. So pulp-tui never reuses publications for Deb Content Views:
  each version creates its own (the task href is checkpointed before the wait).
* **Request body limit.** Pulp refuses a request body above Django's
  `DATA_UPLOAD_MAX_MEMORY_SIZE` (2,621,440 bytes, the default in the test image) with HTTP 400
  and an HTML page `Bad Request (400)`, not 413. The log shows
  `django.core.exceptions.RequestDataTooBig`. nginx in the image allows 10 MB
  (`client_max_body_size 10m`). Measured with `deb/copy/` bodies of synthetic package hrefs
  (75 bytes each in that test; pulp-tui writes compact JSON, 74 bytes each):

  | hrefs  | body bytes | result                                           |
  | ------ | ---------- | ------------------------------------------------ |
  | 1      | 372        | 400 with a JSON validation error (body accepted) |
  | 34,900 | 2,617,797  | 400 with a JSON validation error (body accepted) |
  | 35,000 | 2,625,297  | 400 HTML `Bad Request (400)` (body refused)      |
  | 60,000 | 4,500,297  | 400 HTML `Bad Request (400)` (body refused)      |

  `rpm/copy/` and `modify/` have the same limit. So pulp-tui sends a list in batches of
  at most 2,000,000 bytes of hrefs (`ops::batches`, sized by the longest href), one task
  after the other. A filtered Content View snapshot changes the latest derived version only
  where it differs from the selection: first `modify/` removes the units that are no longer
  selected, then the copy adds the missing ones. Each batch makes a version of the derived
  repository; the last one is the snapshot, and an unchanged selection makes no version. A
  resumed publish does the same from the latest derived version. After the copy, pulp-tui
  deletes the derived versions that no Content View version uses (except version 0).
  pulpcore's `delete_version` merges a deleted version's changes into the next newer
  version, so the content of the other versions does not change, and it refuses a version
  that a distribution serves directly or through a publication (`protected_versions`).
* That merge (`RepositoryVersion._squash`) locks rows with nested `IN` subqueries for the
  units that the deleted version removed and the next version adds again. In a live test
  (pulpcore with PostgreSQL 16), PostgreSQL planned it from statistics older than the new
  versions and ran one delete for more than 8 hours at full CPU; with current statistics the
  same query is a hash join. A snapshot that starts from an empty version and is completed by
  later batches makes exactly that case (every unit leaves and comes back), so pulp-tui never
  builds a snapshot that way. A copy without filters is one request on `dest_base_version: 0`:
  inside one version, pulpcore undoes the removal of a unit that is added again.

## Shared database (PostgreSQL)

### Database TLS and `ca_cert`

pulp-tui uses sqlx 0.8.6 with `tls-rustls-aws-lc-rs`. For the database connection:

* **The trust anchors are the Mozilla roots that are compiled into the binary
  (`webpki-roots`). The operating system trust store is not read.** `ca_cert` (sqlx
  `sslrootcert`) **adds** its certificates to these roots; it does not replace them (source:
  `sqlx-core/src/net/tls/tls_rustls.rs` starts from `certs_from_webpki()` and calls
  `cert_store.add` for each certificate of the file). The addition was read in the source;
  it was not tested against a PostgreSQL server with a publicly signed certificate (none was
  available). This differs from the `ca_cert` of a Pulp profile (reqwest), which adds to the
  system roots. An internal CA that the host already trusts still needs `ca_cert` for the
  database.
* A `ca_cert` file may hold several certificates.
* `sslmode=verify-full` is set by pulp-tui for every host except a Unix socket or a
  loopback address, so the host name in the URL must be in the server certificate (an IP
  address needs an IP entry in the subject alternative names).

Checked on 2026-10-23 (by hand, not part of `tests/run-live.sh`): PostgreSQL 16.15 with
`ssl=on` and a server certificate from a private CA (OpenSSL, SAN `IP:<host-ip>`,
`DNS:<host>`, `DNS:localhost`, `IP:127.0.0.1`), published on the LAN address of the
test host. `pulp-tui db migrate` connected through that non-loopback address:

| `[database]`                                                               | Result                                                                                                                                                                                       |
| -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ca_cert` = the private CA                                                 | connects (`schema 0 -> 2`)                                                                                                                                                                   |
| `ca_cert` = a PEM file with an unrelated CA first, then the private CA     | connects                                                                                                                                                                                     |
| no `ca_cert`                                                               | `invalid peer certificate: UnknownIssuer`                                                                                                                                                    |
| no `ca_cert`, `SSL_CERT_FILE` = the private CA (OS store would trust it)   | `invalid peer certificate: UnknownIssuer` (the OS store is not read)                                                                                                                         |
| `ca_cert` = an unrelated CA                                                | `invalid peer certificate: UnknownIssuer`                                                                                                                                                    |
| `ca_cert` = the private CA, URL host `172.17.0.1` (not in the certificate) | `invalid peer certificate: certificate not valid for name "172.17.0.1"; certificate is only valid for IpAddress(<host-ip>), DnsName("<host>"), DnsName("localhost") or IpAddress(127.0.0.1)` |

The live tests connect to `127.0.0.1` without TLS, so they do not exercise this path.

### Other behaviors

* Loopback hosts keep the `sslmode` of the URL (sqlx default `prefer`).
* The URL parameter `options=-csearch_path%3D<schema>` works; the live tests use it to give
  each test its own schema.
* A role with only `SELECT`, `INSERT`, `UPDATE` and `DELETE` on the tables worked for
  `import-cv` and `export-cv` (they insert Content Views and environments, so identity
  columns need no sequence grant) and for `db migrate` on a current schema. An upgrade needs
  a role that can create tables. The other functions use the same kinds of statements.
* Identifiers are `BIGINT`; PostgreSQL has no `INTEGER` here because sqlx `Any` reads
  integers as `i64`.

## Unsupported or limited

* Plugins other than pulp_rpm, pulp_file and pulp_deb (container, python, …) are not managed.
* pulp_deb: no upload of `.deb` files, no replication (native or fallback), no verbatim
  publications, no dependency solving on copy. A verbatim publication made elsewhere is
  served when it is the newest (Verify and Rollback read it) and it still
  protects its version (pulpcore `protected_versions` reads every publication type), so the
  version delete impact lists `publications/deb/verbatim/` too.
* RPM publication layout `nested_by_digest`: the package probe of served-content checks is
  skipped (metadata is still checked).
* Signing: repository fields for signing services are editable, but pulp-tui does not
  create signing services (that requires server-side scripts).
* Content guards are only shown and set by href; they are not created or replicated.
* Reusable filter sets are not implemented yet. The data model keeps filters per view
  repository, so a later "filter set" table can be referenced without changing published
  versions. A composite Content View has no filters of its own and cannot be a component
  of another composite.
* The fallback replication does not delete destination objects and does not copy Pulp
  metadata; use native replication for full metadata replication.
* Pulp domains: the profile `domain` setting prefixes all API paths. It is implemented but
  was not tested against a domain-enabled server.
* Bearer tokens are sent as given; token refresh (OIDC) is not implemented.
* Windows builds are not tested.
