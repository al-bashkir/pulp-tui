# Compatibility and capability matrix

## Tested versions

All live tests (`tests/run-live.sh`, `real` included) passed on 2026-11-08 on fresh
containers of `quay.io/pulp/pulp:latest`:

| Component                             | Version                                             |
| ------------------------------------- | --------------------------------------------------- |
| pulpcore, pulp_file                   | 3.123.1                                             |
| pulp_rpm                              | 3.39.5                                              |
| pulp_deb                              | 3.11.1                                              |
| PostgreSQL (optional shared database) | 16.15 (`docker.io/library/postgres:16`), sqlx 0.9.0 |
| Client OS                             | Fedora 44 (Linux 7.2), Rust 1.99.1                  |

On 2026-11-08, all live scenarios, `real` included (Debian `main`, Rocky 9 BaseOS and
AppStream), passed on fresh containers of these images: 649 unit, 50 mock HTTP, 145 live
and 3 `real` tests on each.

| Image                       | pulpcore / pulp_file | pulp_rpm | pulp_deb | `real` |
| --------------------------- | -------------------- | -------- | -------- | ------ |
| `quay.io/pulp/pulp:3.63.36` | 3.63.36              | 3.27.12  | 3.5.4    | 817 s  |
| `quay.io/pulp/pulp:3.95.3`  | 3.95.3               | 3.34.0   | 3.8.1    | 814 s  |
| `quay.io/pulp/pulp:latest`  | 3.123.1              | 3.39.5   | 3.11.1   | 626 s  |

* On 3.95.3 and 3.63.36, four TUI tests new in audit 3 first failed. Each one used a field
  or a guard type that only newer servers have: File sync `optimize` (pulp_file 3.95.3),
  distribution `checkpoint` (pulpcore 3.63.36), Deb repository
  `package_signing_fingerprint_release_overrides` (pulp_deb 3.8.1) and the
  `core/envvar_header` guard (pulpcore 3.95.3 answers 404). The TUI hides each one on a
  server without it. The tests now use only what the form shows; `deb` and `admin` then
  passed on 3.95.3 in a second run on fresh containers.
* On 2026-11-04, on 3.95.3, one `native_replication` run failed once with "connection closed
  before message completed" on `POST upstream-pulps/`. The next two runs passed.
* The nginx of the 3.63.36 image listens on IPv4 only. With rootless podman (pasta),
  `localhost` goes to `::1` and the connection is reset, so the tests use `127.0.0.1`.

Other versions are not tested. pulp-tui detects capabilities at runtime.

## Runtime detection

General rules:

* **Plugin**: the `status/` component (`rpm`, `file`, `deb`). Without it, the screens show
  "not supported: pulp_rpm plugin" (or the File or Deb one).
* **Endpoint**: the path and method in the OpenAPI. Without it, the action shows "not
  supported by this server: <feature> (<path>)" before its form or confirmation and sends
  nothing, for example "not supported by this server: comps.xml upload (rpm/comps/)".
* **Field**: the field in the OpenAPI schema of the type (for example `rpm.RpmRemote`).
  Without it, the form hides the field and does not send it.
* **Choices**: the enum that the `$ref` of the property points to (by reference, not by its
  generated name such as `LayoutD5aEnum`).
* **OpenAPI not readable** (for example 403): checks are optimistic, the server gives the
  final answer, and the dashboard shows a warning.

### Plugins and endpoints

| Feature                                   | Detected by                                                          | When missing                              |
| ----------------------------------------- | -------------------------------------------------------------------- | ----------------------------------------- |
| RPM, File, Deb screens                    | `status/` component `rpm`, `file`, `deb`                             | plugin message                            |
| RPM copy (copy action, Content Views)     | component `rpm` and `rpm/copy/`                                      | endpoint message (1)                      |
| Deb copy (copy, Add to repository, CVs)   | component `deb` and `deb/copy/`                                      | endpoint message (2)                      |
| Deb verbatim publications (kind, list)    | `publications/deb/verbatim/`                                         | APT publications only                     |
| Chunked upload                            | `uploads/` and `{upload_href}`                                       | message for files above `upload_chunk_mb` |
| Uploads screen                            | `uploads/` and `{upload_href}`                                       | screen not supported                      |
| Task Schedules screen                     | `task-schedules/`                                                    | screen not supported                      |
| Artifacts screen                          | `artifacts/`                                                         | screen not supported                      |
| Access Policies screen                    | `access_policies/`                                                   | screen not supported                      |
| Access policy reset (**R**)               | `{access_policy_href}reset/`                                         | endpoint message                          |
| Native replication                        | pulpcore ≥ 3.23, `upstream-pulps/`, `{upstream_pulp_href}replicate/` | screen points to the fallback             |
| Fallback replication of a type            | `status/` component on source and destination                        | dry run stops (3)                         |
| Alternate content sources                 | the plugin, `acs/rpm/rpm/`, `acs/file/file/`, `acs/deb/deb/`         | endpoint message on the screen            |
| RPM ULN remotes                           | the plugin, `remotes/rpm/uln/`                                       | endpoint message on the screen (6)        |
| File Git remotes                          | the plugin, `remotes/file/git/`                                      | endpoint message on the screen (7)        |
| RPM comps, advisory upload (**U**, **A**) | `post` of `rpm/comps/`, `content/rpm/advisories/`                    | endpoint message                          |
| RPM module content (**M**)                | `post` of `content/rpm/modulemds/` and the two others (8)            | kind not offered; none: endpoint message  |
| Version repair (**F**)                    | `{rpm_rpm_repository_version_href}repair/` (4)                       | endpoint message                          |
| Server repair (**F** on Artifacts)        | `repair/`                                                            | endpoint message                          |
| Reclaim space (**R**)                     | `repositories/reclaim_space/`                                        | endpoint message                          |
| RPM prune old packages (**O**)            | `post` of `rpm/prune/` (8)                                           | endpoint message                          |
| RPM version scan (**V**)                  | `post` of `{rpm_rpm_repository_version_href}scan/` (9)               | endpoint message                          |
| Current user                              | `login/`, else `users/?username=<Basic-auth name>`                   | header shows "anonymous" (5)              |
| Users, groups, roles                      | `users/`, `roles/`                                                   | screen not supported                      |
| Domain scope of a role assignment         | `status/` `domain_enabled: true`                                     | Assign role has no Domain field           |
| Domains screen                            | `status/` `domain_enabled: true` and `domains/`                      | screen not supported                      |
| Domain storage migration (**M**)          | `domains/migrate/`                                                   | endpoint message                          |
| Pulp exporters                            | `exporters/core/pulp/`                                               | screen not supported                      |
| Filesystem exporters                      | `exporters/core/filesystem/`                                         | screen not supported                      |
| Pulp importers, import check              | `importers/core/pulp/`, `importers/core/pulp/import-check/`          | screen not supported                      |
| Delete a task; purge finished tasks       | `delete` on `{task_href}`; `tasks/purge/`                            | endpoint message                          |
| Workers, Task groups screens              | `workers/`, `task-groups/`                                           | screen not supported                      |
| Content guards screen                     | `contentguards/` and `distributions/`                                | screen not supported                      |
| Content guard types (**c**)               | `contentguards/<type>/`, for example `contentguards/core/rbac/`      | type not offered                          |
| RBAC guard download access (**a**)        | `{r_b_a_c_content_guard_href}add_role/`                              | endpoint message                          |
| Object roles (**B**)                      | operation `<collection>_list_roles`, e.g. `repositories_rpm_rpm_…`   | endpoint message                          |
| Task group cancel                         | `patch` on `{task_group_href}`                                       | endpoint message                          |

1. For the copy action. A Content View publish gets the server's error.
2. Also for Add to repository and Content View Publish. Resume of such a version stops
   before it sends anything.
3. "<profile> (source): not supported by this server: pulp_file plugin", before it lists or
   changes anything.
4. File: `file_file`, Deb: `deb_apt`.
5. `users/` is for servers without `login/` (for example 3.63). "anonymous": the user
   cannot read `users/`, or the profile uses bearer auth. The own-user guards (delete,
   disable, password, role removal) then use the Basic-auth username; without one, the user
   delete confirm says that it cannot tell whether this is your own user.
6. pulp_rpm 3.27.12, 3.34.0 and 3.39.5 have it. The Remote picker of a repository and a sync
   lists it only when the server has the endpoint.
7. pulpcore 3.123.1 has it; 3.63.36 and 3.95.3 do not (read in their images). The picker is
   the same as in (6).
8. pulp_rpm 3.27.12, 3.34.0 and 3.39.5 have it (read in their images).
9. pulp_rpm 3.39.5 has it; 3.27.12 and 3.34.0 do not (read in their images).

### Form fields

| Form                           | Optional fields                                                                          |
| ------------------------------ | ---------------------------------------------------------------------------------------- |
| Repository                     | checksum type, signing services, retain package versions, manifest, `retain_checkpoints` |
| RPM repository                 | compression, layout, repo config, package signing, `osv_config`                          |
| Remote                         | timeouts, `max_retries`, `headers`; Deb `excluded_package_metadata_fields`               |
| Distribution                   | `hidden`, `checkpoint`; RPM `generate_repo_config`                                       |
| Publication (**c**, **P**)     | options of `rpm.RpmPublication`, `file.FilePublication`, `deb.AptPublication`            |
| Deb upload (**u**)             | `distribution`, `component` of `deb.Package`                                             |
| Upload (**u**)                 | `overwrite` of `rpm.Package`, `file.FileContent`, `deb.Package`                          |
| Filesystem export (**E**)      | `start_repository_version` of `FilesystemExport`                                         |
| Sync                           | `optimize` (RPM, File, Deb), `skip_types` (RPM): `{Rpm,File,Apt}RepositorySyncURL`       |
| Upstream Pulp (`UpstreamPulp`) | `policy`, replica download policy, concurrency, retries, timeouts                        |

* A hidden sync option uses the server default.
* **Remote**: the form shows only the fields of the type's schema (`rpm.RpmRemote`,
  `rpm.UlnRemote`, `file.FileRemote`, `file.FileGitRemote`, `deb.AptRemote`); Labels always.
  The four timeouts are floats (`FloatField(min_value=0.0, allow_null=True)`, also on
  `UpstreamPulp`): the forms take decimal seconds and send a JSON number with a fraction
  (`2.5`, also `5.0`); 2.5 and 0.1 come back exactly (live,
  `remote_timeouts_keep_decimal_seconds_through_the_tui`).
* **Replication filter**: the form shows `q_select`, else `pulp_label_select`.
* **Deb Content View signing** (`signing_service`, `signing_service_release_overrides`):
  detected in the JSON of the derived repository. Without them, pulp-tui does not send them,
  and a chosen signing service stops the publish with "not supported by this server".

### Choices

| Choice (property)                                      | Without the enum                                 |
| ------------------------------------------------------ | ------------------------------------------------ |
| Remote download policy, Upstream `remote_policy`       | immediate / on_demand / streamed                 |
| Replication deletion policy (`policy`)                 | nodelete / labeled / all                         |
| RPM sync policy (`RpmRepositorySyncURL.sync_policy`)   | additive / mirror_complete / mirror_content_only |
| RPM `skip_types` (the `$ref` of the items)             | `srpm`, `treeinfo`                               |
| Filesystem export method (`FilesystemExporter.method`) | write / hardlink / symlink                       |
| RPM checksum type, compression, layout: repo           | empty (the server default) and the current value |
| Same, on a publication                                 | only empty (the server default)                  |

## Verified behavior (live tests)

One bullet per scenario, with its main checks. The server behavior behind them is in the
next section.

* **RPM**: create, upload (and idempotent repeat), remote, sync (`mirror_complete`,
  `additive`; through the TUI also `mirror_content_only` with Optimize off and Skip srpm:
  the request, the new version, no publication), versions, diff, publish, distribute, served
  metadata and package digest, update, rollback.
* **File**: create, upload (single and chunked with progress), remove content (old versions
  stay), sync from a manifest, publish, distribute, served manifest, update, rollback,
  mismatch detection.
* **Distributions**: base path conflict (409), `base_path` PATCH.
* **RPM ULN remote** (`live_repos`, 3.123.1, through the TUI): create with `uln://`, an
  edit of `uln_server_base_url` keeps the account (`hidden_fields`), delete after the impact
  names the repository that uses it. A ULN sync needs a ULN account: mock tests only.
* **File Git remote** (`live_repos`, 3.123.1, through the TUI): create with `git_ref` v1 of
  the fixture `upstream/git/files.git`; a File repository with it as default remote (made
  outside the TUI) syncs with the untouched, pre-filled sync form (one file); after an edit
  of `git_ref` to `main`, the next sync adds the second file.
* **Content Views**: multi-repository snapshot, RPM, File and advisory filters, numbered
  versions, Library auto-binding, promotion path rules, rollback, served-content check,
  retention protection, partial failure and retry, Resume that re-attaches to a submitted
  task, version and view delete.
* **Composite Content Views** (`live_cv::composite_content_view_rocky9`): one publication
  served by two distributions (the component's Library one and the composite's own), auto
  publish after the extras view publishes, a delete of a composite version or composite
  keeps the publications and repositories of its components.
* **Deb** (`live_deb`): mirror sync, filtered structured copy, publication, distribution,
  served `Release`, `Packages` and package digest. A publication with the test signing
  service is reused only for that service and serves `Release.gpg` and `InRelease`. A Deb
  Content View (`exclude name=*-dbg`): signed, unsigned (`none`), resumed from a checkpoint,
  promoted to Dev, used by a composite. A per-release key of the source signs a
  `same as source` publish; a `none` publish removes it.
* **Native replication**: select by `q_select`, `nodelete` policy, rerun, upstream update.
* **Fallback replication**: dry run, create, idempotent rerun, upstream update, conflict
  with native-replicated objects, loop guard, self-replication guard.
* **Repository definitions** (`export-repos` on pulp2, `import-repos` on pulp1): RPM, Deb
  and File, ULN and Git remotes, no secret in the file, remote found by name, idempotent rerun.
  `rpm.UlnRemote` requires `username` and `password` (OpenAPI `required`), which GET never
  returns: the import creates a ULN remote only when the operator added them to the file.
* **Pulp export** (`live_export`, File): full export (TOC, files, `last_export`), export
  record delete (204), exporter delete (task); incremental export (400 without a previous
  export; after a new file, its import adds the file to the copy); `versions` with an older
  version; `start_versions` (400 without `full: false`).
* **Filesystem export** (`live_export`): newest File publication of the latest version; a
  second export into the same directory fails (publication and version); after a PATCH of
  the directory it works.
* **Pulp import** (`live_export`, same server): import check (valid; not valid for a
  missing TOC); import through a mapping into another repository (one
  `import_repository_version` task, same content count, imports list names the TOC);
  importer delete (204); check and import of the `.tar` as `path` (same content count); a
  snapshot as a mapped destination is refused (create, and a mapping set with the API), and an
  unmapped import names the local snapshot of the same name.
* **Comps and advisories** (`live_content`): a comps.xml makes a version with its group; a
  second with `replace` keeps only its own group; an advisory JSON makes the next version.
* **Version repair** (`live_content`): RPM version synced `immediate`, with and without
  `verify_checksums`: 0 missing, 0 corrupt, 0 repaired. A damaged file is not tested (needs
  access to the server storage).
* **Reclaim space** (`live_content`): see the behavior below.
* **Alternate content sources** (`live_content`, 3.123.1): `immediate` remote refused (400);
  refresh of RPM (two paths, no common package), File and Deb ACS as task groups (also as one
  Task Center task); path edit; delete.
* **Orphan cleanup** (once, by hand: **O** on Repository Versions, pulp1, 1440 minutes):
  count, typed confirmation, `orphan_cleanup` task, notice from its progress reports.
* **RBAC**: 401 for a wrong password (any authenticated endpoint; 3.63 has no `login/`), 403
  without a role, no self-escalation, model-level creator role, group object role, role
  removal.
* **Administration** (`live_admin`, 3.123.1): group rename (`PATCH name`); task filters
  (state, name, reserved repository href, start date; a bad date shows Pulp's error); task
  delete (then 404); purge with `finished_before` 2000-01-01 (count 0, typed profile name,
  notice); Task groups screen (the list leaves out `tasks`, Inspect reads them); Workers
  screen. Servers: **X** writes a File remote with password, username and headers (no
  secret in the file, `secrets` names password and username); **J** plans, then creates a
  renamed remote and repository linked to each other.
* **Tasks and TLS**: cancel of a waiting and a running task, 409 for a completed task, TLS
  errors, unreadable CA bundle.
* **Mock HTTP** (`cargo test`): pagination, foreign `next` links, 401/403/409, malformed
  JSON, read retries and no mutation retry, 429 Retry-After, timeouts, unreachable server,
  task polling, old-server capabilities, read-only blocking, size limits.
* **Shared database, PostgreSQL 16** (`live_shared_db`): `db migrate`; refusal of an
  uninitialized, older or newer schema; two operators; lock conflict and release; 8
  simultaneous acquisitions of a free lock (one wins); every store method; composite views
  (pins, borrowed rows, foreign keys that refuse a component delete, `db copy`); `db copy`
  from a local file; a profile with its own database; schema 2 to 3 migration (labels
  `library`/`dev`/`dev` for `Library`/`Dev`/`dev`, served path kept, used label refused).
* **Shared database, by hand**: a role with only `SELECT/INSERT/UPDATE/DELETE`; TLS with
  `ca_cert` (once). See "Shared database (PostgreSQL)".

## Known server behaviors

What Pulp does, then what pulp-tui does. Versions are those in which the source was read
(in the test containers) or the behavior was seen.

### Tasks

* **404 for a task that ran.** `tasks/purge/` deletes finished tasks, and RBAC hides tasks
  that the user may not view. The Task Center shows such a task as `gone`; Content View
  Resume clears the checkpoint and sends the step again (mock test only).
* **Task filters** (3.123: `state`, `name__contains`, `reserved_resources`,
  `started_at__gte`). Reserved resources are PRNs (`prn:file.filerepository:<id>`); the
  filter turns an href into its PRN, so the href of a deleted object matches nothing but
  its PRN does (pulpcore `custom_filters.py`). A bad date gets `400 Enter a valid
  date/time.`; an unknown parameter gets `400 Invalid Filter`.
* **Task purge** (pulpcore `purge.py`) deletes the tasks of the current domain with
  `finished_at < finished_before`, `state` in `states` (final states only: `"running" is
  not a valid choice`) and `core.delete_task` for the user. pulp-tui first counts with the
  same filter on `tasks/` (`finished_at__lt`, `state__in`), so only visible tasks. Result:
  progress reports `purge.tasks.key.core.Task` (deleted) and `purge.tasks.error` (kept by a
  foreign key). `DELETE` of one task answers 204.
* **Cancel is asynchronous.** A task blocked in a network read can end as `failed` first;
  pulp-tui shows Pulp's final state. A cancel of a completed task gets 409.
* **`skipped`** is a final state, but pulpcore 3.123.1 and the RPM, File and Deb plugins
  never set it. pulp-tui treats it as not run: the step (Content View copy or publication,
  fallback sync) fails and nothing is published.
* **Task groups.** Counters, `all_tasks_dispatched` and `tasks` exist from 3.63.
  `task-groups/` (3.123) has no filters, only `limit`, `offset`, `fields`,
  `exclude_fields`, and lists the newest first. `TaskGroup::is_final` has a rule per kind
  (replication, import, ACS refresh, RPM prune; see below), never only the three-poll rule.
* **Group cancel** (`PATCH {task_group_href}`) exists from 3.95, not on 3.63. It cancels
  every waiting or running task and sets `all_tasks_dispatched` (pulpcore
  `tasking/tasks.py`). The confirmation first reads the group (description, unfinished
  tasks); the row keeps the state that Pulp reports.
* **Task schedules** (3.123.1): `task-schedules/` has GET only (no create, edit or
  delete). `dispatch_interval` is a Django duration (`1 00:00:00` is one day); `last_task`
  is a task href or null. pulp1 has two: orphan cleanup and analytics, each once a day.

### Repository versions

* **No change, no version.** pulpcore deletes an unchanged new version and its
  `CreatedResource` (`RepositoryVersion.__exit__`), so the task has an empty
  `created_resources` (for example a remove of units not in the latest version). After
  Remove or Add to repository, pulp-tui says "no new version" (checked on pulp1, File).
* **Restore as latest** (`modify/` with only `base_version`). pulpcore 3.63.36, 3.95.3 and
  3.123.1 and all three plugins accept it, so pulp-tui does not check for it. The new version starts from the latest one and is
  made equal to the base version, all content types included; retention and autopublish
  run as after a sync. pulp_deb 3.11.1 removes `ReleaseFile`, `PackageIndex` and
  `InstallerFileIndex` from every new version (only a sync adds them; APT publications do
  not use them), and the restore preview says so. Verified in `live_repos` and `live_deb`.
* **Protected versions** (`Repository.protected_versions`, read in the source of 3.63.36,
  3.95.3 and 3.123.1). A version delete gets HTTP 400 ("currently being used to distribute
  content") when a distribution pins the version or one of its publications, or follows the
  repository and the version is the newest version with a complete publication
  (`checkpoint: false`) or, from 3.95.3, has a checkpoint publication (`checkpoint: true`).
  Every publication type counts. The delete preview applies the same rules
  (`ops::protecting`). `checkpoint` is new (migration 0131); pulp-tui reads it as `false` on
  older servers. The Content View cleanup of unused snapshot versions applies
  `ops::protecting` before it deletes, so it sends no delete that Pulp refuses and its log
  names the distribution (`live_cv`, on 3.63.36, 3.95.3 and 3.123.1).
* **Checkpoint distributions** (read in the source of 3.123.1, verified live on it in
  `live_admin`). `DistributionSerializer.validate` (`serializers/publication.py:337-366`) keeps
  the stored `checkpoint` on a PATCH and refuses it with a publication or version binding (HTTP
  400, "The 'checkpoint' attribute may only be used with the 'repository' attribute"). Rollback,
  pinned Distribute and `ops::ensure_distribution` (promote, replication) refuse such a
  distribution before any write. The content app (`content/handler.py:392-434`) serves only
  `<base path>/<timestamp>/`: the newest checkpoint publication of the repository created at or
  before that second (any publication type), with `CHECKPOINT_TS_FORMAT` `%Y%m%dT%H%M%SZ`
  (`constants.py:124`) of `pulp_created` in UTC (`TIME_ZONE = "UTC"`, `settings.py:215`).
  Another timestamp redirects to the one of the served publication, and the bare base path is a
  listing of the timestamps (`CheckpointListings`); a file directly under it gets HTTP 301.
  Verify reads the newest checkpoint publication at its timestamp, and fallback replication
  syncs the replica from there (all types; `live_replication`, File). The rollback confirm reads
  the distribution again, so a Checkpoint set after the preview is refused too. The publication
  delete impact names `<base path>/<timestamp>/` of a deleted checkpoint publication and the
  older checkpoint publication that it then serves (or nothing); a deleted publication that is
  not a checkpoint does not change what a checkpoint distribution serves. Servers without the
  field never send it, and pulp-tui reads it as `false`.
* **Version retention** (`Repository.cleanup_old_versions`, 3.123.1 `models/repository.py:479`,
  run after each new version, `:1536`). Pulp keeps the newest `retain_repo_versions` complete
  versions that are not protected (above), and deletes the other unprotected versions;
  `null` keeps all. So with 1, the next version deletes the current one, unless a
  distribution serves it. The remove content and mirror sync confirms check this
  (`latest_goes`): the content that leaves then stays only in served versions, or it is
  orphaned.
* **Publications and repositories are not protected** (read in the source of 3.123.1:
  `PublicationViewSet` is a plain `DestroyModelMixin`, `Distribution.publication` is
  `on_delete=SET_NULL`). A served publication is deleted without an error: a distribution
  pinned to it then serves nothing, and one that follows the repository serves an older
  publication. A repository delete takes all its versions and publications, also served
  ones. So the Content View version delete keeps the publication and the snapshot version
  that a distribution serves (`ops::protecting`; a distribution pinned to another
  publication of the version keeps only the version), and the view delete is refused while
  a distribution that the view does not delete serves its snapshot repository, one of its
  versions or one of its publications (`live_cv`, on 3.123.1). Deb verbatim publications
  of a snapshot repository are not checked. A list filter with the href of a deleted
  repository gets HTTP 400 (`URI … not found for repository.`); these checks then read
  the repository, and when it is gone (404), nothing serves it, so a rerun of an
  interrupted delete continues.
* **Version repair** (`{version}repair/`, `verify_checksums` default true; progress reports
  `repair.missing`, `repair.corrupted`, `repair.repaired`). Pulp downloads each missing or
  corrupt file from the remote artifacts, else deletes the stored file. A version without
  content repairs every artifact of the domain (pulpcore `tasks/repository.py`), so
  pulp-tui refuses it.
* **Server repair** (`repair/`, pulpcore `views/repair.py`): the same task code for every
  content unit of the request's domain, with the same progress reports. It reserves only
  `pdrn:<domain>:repair`, so a second repair waits and other tasks go on. pulp-tui sends
  `verify_checksums: false` unless the operator asks for it (reading every file is slow).
* **Reclaim space** (`repositories/reclaim_space/`: `repo_hrefs`, optional
  `repo_versions_keeplist`; progress report `reclaim-space.artifact`). It deletes the stored
  file of each unit that no other repository has, that has a remote artifact (not an
  upload; no `force` in the API) and whose type allows it. The units stay. On pulp1 (5
  packages synced `immediate`): 5 files deleted, `artifact: null`, the distribution still
  served a package with the right digest, which then had its file again.
* **Orphan cleanup** (`orphans/cleanup/`, task `orphan_cleanup`, progress reports
  `clean-up.content`, `clean-up.artifacts`). `orphan_protection_time` is in minutes; without
  it the hidden server setting `ORPHAN_PROTECTION_TIME` applies. `content/?orphaned_for=N`
  counts what the same N deletes. Protection counts from `timestamp_of_interest`, not from
  the orphan time: on pulp1, 142,224 units were orphans at 0 minutes and none at 1440,
  because all were created that day. The old `DELETE orphans/` is not used.
* **Prune old packages** (`rpm/prune/`: `repo_hrefs`, `keep_days` default 14, `dry_run`; read
  in pulp_rpm 3.39.5 `tasks/prune.py`). The answer is a task group: the `prune_packages` task
  dispatches one `prune_repo_packages` task per repository and ends. It calls
  `TaskGroup.finish()` in 3.27.12 but not in 3.34.0 and 3.39.5, so `all_tasks_dispatched` stays
  false; `TaskGroup::is_final` ends the group when `prune_packages` completed and nothing runs.
  A package goes when the latest version has a newer one of the same name and arch and its
  `RepositoryContent` was created more than `keep_days` ago. Each repository task has the
  progress report `rpm.package.prune.repository` (`message` `Pruning <name>`, `total` = packages
  that go, `done` = 0 on a dry run) and, unless dry, makes a new version (none when nothing
  goes). A dry run logs the packages at debug level only; the API gives the counts.
  pulp_rpm 3.34.0 on pulpcore 3.95.3 (seen on a fresh container) does not remove them: the new
  version has the package as removed (`content_summary.removed` 1,
  `repository_version_removed=<v>` lists it), but `repository_version=<v>` still lists it and
  `present` keeps the old count. `prune.py` gives `remove_content` a `RepositoryContent`
  queryset, and pulpcore 3.95.3 takes its ids away from the version's `content_ids`, which hold
  content ids. pulp_rpm fixed it in 3.35.0 (commit 4860ebf397) and in 3.32.15, 3.29.13 and
  3.27.14; 3.33.x and 3.34.0 have no fix. pulpcore 3.63.36 has no `content_ids`, so pulp_rpm
  3.27.12 there removes the packages. After a run pulp-tui counts the packages that the new
  version lists and has as removed (`repository_version=<v>&repository_version_removed=<v>`, 0
  on a correct server) and, when there are any, says "new version N still has K of the M
  package(s) that Pulp marked removed (pulp_rpm bug, ...)" instead of "removed M package(s)"
  (`rpm_prune_dry_run_then_prune_through_the_tui`).
* **Vulnerability scan** (`{version}scan/`, empty body; read in pulp_rpm 3.39.5
  `viewsets/repository.py` and pulpcore 3.123.1 `tasks/vulnerability_report.py`). The
  ecosystems come from the repository label `osv.rpm.config` (URL-encoded JSON), which the RPM
  repository field `osv_config` reads and writes; without it the POST gets 400. The task
  (`check_content`) sends one osv.dev query per package and release, from the server, and keeps
  one `vuln_report/` record per package (`vulns`: the OSV entries; `repo_versions`: the scanned
  versions). Progress reports `total_scanned`, `created`, `updated`. The filter
  `vuln_report/?repo_versions=<version href>` answers HTTP 500 on pulpcore 3.123.1: a version
  href has no pk, so `HyperlinkRelatedFilter` filters `RepositoryVersion` by `pulp_domain`,
  which that model lacks (`FieldError`, seen on pulp1). The version's PRN
  (`prn:core.repositoryversion:<id>`) resolves to a pk and works, so pulp-tui reads the `prn`
  of the version and lists with it, then names each package with vulnerabilities. On pulp1 the
  scan of the 3 fixture packages (Rocky Linux 9) reached osv.dev and found none.
  pulp_rpm refuses `pulp_labels` with the key `osv.rpm.config` ("Use the 'osv_config' field"),
  and `pulp_labels` without it replaces the label set, so the label goes. A request with both
  `pulp_labels` and `osv_config` merges the current labels into the new ones
  (`RpmRepositorySerializer.validate`), so a removed label stays. The repository form hides the
  label from Labels and has an OSV config field; an edit that changes the labels sends them alone,
  waits for the task, then sets `osv_config` again (the form's value, else the one the server has
  just before the edit). Checked on pulp1 (`rpm_osv_config_survives_a_label_edit_through_the_tui`).

### Content and digests

* **RPM content types.** pulp_rpm 3.39.5 has 11; the compare and the Content browser cover
  all. Langpacks have no id, so the compare keys them by `digest`. It warns when
  `content_summary.present` counts other units than it lists, and names types without an
  endpoint (older plugins), as the restore preview does.
* **Module content create** (`content/rpm/modulemds/`, `modulemd_defaults/`,
  `modulemd_obsoletes/`; the same fields in pulp_rpm 3.27.12, 3.34.0 and 3.39.5). The answer is a
  task (`general_create`); with `repository` it adds the unit to a new version. Required:
  modulemd `name`, `stream`, `version`, `context`, `arch`, `description`, `snippet`, and the
  JSON `artifacts`, `dependencies`, `profiles`; defaults `module`, `stream`, `profiles` (JSON),
  `snippet`; obsoletes `module_name`, `module_stream`, `modified`, `message`, `snippet` and five
  fields that may be null (`null=True` columns). The modulemd serializer allows null for
  `artifacts`, `dependencies` and `profiles`, but their columns are NOT NULL (`JSONField`,
  default list, list, dict): null fails the task with `null value in column "dependencies" ...
  violates not-null constraint` (seen on pulp1), so pulp-tui sends an empty one as `[]`, `[]`,
  `{}`. `packages` is optional in the schema, but the modulemd serializer reads it with no
  default, so pulp-tui sends `[]`. A publication writes
  each `snippet` into `modules.yaml` unchanged.
* **`sha256` is `null` until download** for `on_demand` and `streamed` RPM and File content
  (it comes from the stored file). Verify then uses `pkgId` when `checksum_type` is `sha256`
  (else it fails), and checks a File digest only where the API has one (paths always).
* **`PULP_MANIFEST` lines are not quoted**: `relative_path,sha256,size`, and a path can
  have commas (pulp_file `manifest.py`). Verify reads each line from the right, as Pulp
  does.
* **Comps and advisory uploads** (`rpm/comps/` with `file`, `repository`, `replace`;
  `content/rpm/advisories/` with `file`, `repository`) are tasks that make a new version.
  `replace` first removes the groups, categories, environments and langpacks. The advisory
  JSON needs `id`, `issued` and `updated`; a package without `sum_type` fails with
  `[RPM0018] "None" is not supported.`
* **Upload `overwrite`** (pulpcore 3.123.1 `NoArtifactContentUploadSerializer`; write-only,
  default true, used only with `repository`). False: before the new version,
  `Repository.check_content_overwrite` compares the `repo_key_fields` of the content type
  (File `relative_path`; RPM `name`, `version`, `release`, `arch`, `location_href`) with the
  latest version, and other content with the same key fails the task
  (`ContentOverwriteError`; the same content again is no conflict). pulp-tui sends it only when
  off. On pulp1 the second File upload to the same path failed and the version kept the first
  file (`file_upload_without_overwrite_keeps_the_file_of_the_same_path`).
* **Chunked uploads** (pulpcore 3.123.1): the commit task deletes the upload
  (`tasks/upload.py`), so `uploads/` lists only uploads that are not committed. The list has
  no date filter (`size` filters only); `ordering=pulp_created` lists the oldest first.
  `DELETE {upload_href}` answers 204 at once (no task).

### Copy and Content View snapshots

* **`rpm/copy/` adds related units** (pulp_rpm `tasks/copy.py`, also without dependency
  solving): the packages and modules of an advisory, the groups of categories and
  environments, the newest package of each group member not in the request; not the
  packages of a module. With `dependency_solving`, a module in the request also adds all its
  packages (`depsolving.py`, pulp_rpm 3.39.5). The result changes with the batch size, so a filtered RPM snapshot uses `modify/`, which adds exactly the given units.
  The copy action (**C**) with filter rules sends only the chosen packages to `rpm/copy/` (with
  dependency solving when asked), then adds the advisories and metadata of the selection with `modify/`.
* **An empty `content` list copies nothing** (pulp_rpm and pulp_deb `tasks/copy.py`: only
  `content_pks is None` means everything). The task completes with no new version. **C** sends
  nothing when its filter rules select no unit, and leaves out an empty package batch.
* **Request body limit.** A body above Django's `DATA_UPLOAD_MAX_MEMORY_SIZE` (2,621,440
  bytes, the test image default) gets HTTP 400 with an HTML page `Bad Request (400)`, not
  413 (log: `django.core.exceptions.RequestDataTooBig`); nginx allows 10 MB. Same for
  `deb/copy/`, `rpm/copy/` and `modify/`. pulp-tui sends batches of at most 2,000,000 bytes
  of hrefs (`ops::batches`, sized by the longest href), one task after the other. Measured with `deb/copy/` (75 bytes per href; pulp-tui
  writes 74):

  | hrefs  | body bytes | result                                 |
  | ------ | ---------- | -------------------------------------- |
  | 1      | 372        | JSON validation error (body accepted)  |
  | 34,900 | 2,617,797  | JSON validation error (body accepted)  |
  | 35,000 | 2,625,297  | 400 HTML `Bad Request (400)` (refused) |
  | 60,000 | 4,500,297  | 400 HTML `Bad Request (400)` (refused) |
* **Filtered snapshots change only the difference.** `modify/` removes the units that are no
  longer selected, then the copy adds the missing ones; each batch makes a version, and an
  unchanged selection makes none. Resume does the same. pulp-tui then deletes the derived
  versions that no Content View version uses (not version 0); pulpcore merges their changes
  into the next version, so the other versions keep their content.
* **Slow version merge.** The merge (`RepositoryVersion._squash`) of units that leave and
  come back ran one delete for more than 8 hours at full CPU on PostgreSQL 16 with old
  statistics (with current statistics it is a hash join). So pulp-tui never starts a
  snapshot from an empty version and fills it in batches. A copy without filters is one
  request on `dest_base_version: 0`; inside one version, a unit that is removed and added
  again stays.

### Publications and distributions

* **Grace period.** pulpcore serves the files of a previous publication for
  `DISTRIBUTED_PUBLICATION_RETENTION_PERIOD` (default 3 days). After a rollback the
  metadata changes at once, but files of the newer publication can still answer 200.
  pulp-tui verifies rollbacks by metadata and says so in the preview.
* **Silent cascades** (pulpcore `models/publication.py`). A version delete deletes its
  publications; a publication delete sets the distribution's `publication` to null, without
  an error. So pulp-tui never deletes a served object, and checks every binding of the
  server: a composite and its component can serve one publication. `delete_view` refuses to
  delete a derived repository that another view or any other distribution serves.
* **Only publications are served** (`SERVE_FROM_PUBLICATION` in all three plugins). A
  distribution on a version serves the newest complete publication of that version; with
  none, every path is 404, and Verify and the rollback preview say so.
* **A distribution that follows a repository** serves the newest complete publication (by
  version, then creation), not the latest version. Any publication type counts, so a newer
  Deb verbatim publication wins over APT ones. Verify compares with that publication and
  lists both Deb types.
* **Distribution PATCH merges** with the stored object. A body with `publication` but
  without `repository_version: null` gets HTTP 400; `{publication: null, repository: null}`
  answers 200 but keeps the version. pulp-tui always sends `repository_version: null`.
* **`base_path` PATCH** (environment relabel) is a task (`ageneral_update`) that keeps the
  href and publication. When it ends, the new path answers 200 and the old one 404 (no
  cache delay seen); a later promotion serves at the new label. A taken base path gets 409.
* **Overlapping base paths** (pulpcore 3.123.1, `DistributionSerializer.validate_base_path`): a
  base path that equals another, is a prefix of another (`a` and `a/b`), or is inside another
  gets HTTP 400 `Overlaps with an existing distribution.` (checked on POST and PATCH). The
  relabel therefore refuses a label when another distribution has the label or a path below it
  as its base path, before it stores anything. A new distribution (promote, replication) gets
  the same check before the POST, and the promote preview names the distribution
  (`ops::overlapping_distribution`). It uses the `distributions/` filters `base_path__in` (the
  path and each path above it) and `base_path__contains` (`<path>/`, then a prefix check);
  pulpcore 3.63.36 has both.
* **File mirror sync publishes** (pulp_file 3.123.1, `tasks/synchronizing.py`): a sync with
  `mirror` that makes a version also makes a pass-through publication of it with the upstream
  manifest (live). pulp_deb 3.11.1 mirror makes none. The mirror confirm says so for each File
  repository, as it does for RPM `mirror_complete`. Only a File remote gives that manifest
  (`FileFirstStage` fills `metadata_files`, `GitFirstStage` does not): a `mirror` sync from a
  Git remote makes the new version, then the task fails with `pop from empty list` and makes no
  publication (pulp_file of pulpcore 3.123.1, live). The mirror confirm says so for a File
  repository that syncs from a Git remote (the picked one, else its default remote).
* **RPM publications.** A `mirror_complete` sync makes its own publication with upstream
  paths, so served-content checks also try `location_href`. The RPM publication has no
  `distributions` field (the File one has) and `distributions/` has no `publication`
  filter, so before a delete pulp-tui matches the bindings in the client (pulp_rpm 3.39.3).
* **RPM publish settings** (`metadata_signing_service`, `checksum_type`, `repo_config`,
  `compression_type`, `layout`) default to the repository values. A Content View publish
  copies them to the snapshot repository first. `repodata/repomd.xml.asc` exists only in a
  publication made while the repository had a `metadata_signing_service`; Verify requires
  it when the repository has one.
* **RPM publications store no signing service** (pulp_rpm 3.39.5: no such field in
  `RpmPublication` or its serializer). A publication made before the repository got its
  `metadata_signing_service`, or with another service, looks the same as a signed one. So
  rollback and pinned Distribute never reuse a publication when the repository has a
  `metadata_signing_service`: they make a new one, which Pulp signs with the service of
  now. A server without the field counts as unsigned. Fallback replication still reuses
  (it runs often); its verify fails when the reused publication has no `repomd.xml.asc`.
* **Publication mode.** A publication stores how it publishes (RPM `checksum_type`,
  `compression_type`, `layout`, `repo_config`; File `manifest`; Deb `simple`,
  `structured`, `layout`, `signing_service`, `publish_*`, `excluded_package_metadata_fields`).
  Rollback and pinned Distribute copy these fields from the publication that the
  distribution serves now (same repository only) and reuse only a publication with the same
  values; otherwise clients can lose their paths or signatures.
* **File publish ignores the repository** (pulp_file 3.123.1). Without `manifest` the
  publication gets `PULP_MANIFEST` (the repository value is for autopublish only);
  `manifest: null` gives none. pulp-tui sends the repository `manifest` (snapshot
  repositories copy it from the source). Verify reads the manifest that the publication
  names; with none, it says that it compared no files.

### Content guards

Read in pulpcore 3.123.1 (models and serializers in `pulpcore/app/`) and live on pulp1.

* **A guard delete makes its distributions public.** `Distribution.content_guard` is
  `on_delete=SET_NULL`. The delete answers 204 at once (no task).
* **A composite without guards allows everyone** (`CompositeContentGuard.permit`). Its
  `guards` is a many-to-many field, so a deleted member just leaves the composite. Pulp does
  not refuse a composite that lists itself; the form never offers it.
* **RBAC guard `users` and `groups`** are the holders of `core.download_rbaccontentguard` on
  the guard. `add_role/` and `remove_role/` take `{role, users, groups}` (user and group
  names); the role is `core.rbaccontentguard_downloader`. The user that creates the guard
  gets that role at once, and `add_role/` refuses a role that the user already has (400).
* **`header_value` of a header guard is readable on GET.** pulp-tui masks it like a secret.
* **An env var header guard needs an allowed variable.** `env_var` must be in the server
  setting `ENVVAR_HEADER_CONTENT_GUARD_ALLOWED_VARS`, empty by default (`settings.py`): else
  400 "Environment variable '...' is not in ENVVAR_HEADER_CONTENT_GUARD_ALLOWED_VARS." (live).
  The other types (RBAC, redirect, composite, x509, RHSM with a self-signed CA) are created
  through the TUI in `live_admin`.
* `description` and `jq_filter` take `null`, not an empty string (`minLength` 1). The
  certguard types take the PEM text of the CA certificate in `ca_certificate`.
* `contentguards/` lists every type with name and description only; the type is in the
  href. `distributions/` has no `content_guard` filter, so the screen reads `name` and
  `content_guard` of all distributions.

### Artifacts

Read in pulpcore 3.123.1 (`ArtifactViewSet`, `ArtifactFilter`) and live on pulp1.

* The default policy allows `list`, `retrieve` and `create` to `admin` only, and has no
  `destroy` statement: a delete is refused unless an operator changed the policy. Pulp marks
  the delete as deprecated ("use orphan cleanup instead") and answers 409 for an artifact that
  content uses. pulp-tui offers no delete.
* `artifacts/` filters on the exact digests (`md5` … `sha512`), `repository_version` and
  `orphaned_for`; no partial match. `md5` and `sha1` are null unless the server allows them.

### Access policies

Read in pulpcore 3.123.1 (`AccessPolicyViewSet`, `AccessPolicySerializer`) and live on pulp1
(127 policies, none customized).

* `access_policies/` lists every viewset with its `statements`, `creation_hooks` and
  `queryset_scoping`; it filters on `viewset_name` and `customized`. There is no create or
  delete.
* Any PUT or PATCH sets `customized: true`, also when the statements do not change.
* `reset/` (POST, no body, 200) writes the plugin's `DEFAULT_ACCESS_POLICY` again and sets
  `customized: false`.

### Object roles

Read in pulpcore 3.123.1 (`RolesMixin` in `app/viewsets/base.py`, `NestedRoleSerializer`).

* `list_roles/`, `add_role/`, `remove_role/` and `my_permissions/` come together. The detail
  path in the OpenAPI is a template (`{rpm_rpm_repository_href}list_roles/`), so pulp-tui finds
  the type by the operation id: the collection path with `_` for `/` and `-`
  (`task_schedules_list_roles`). The test server has them on repositories, remotes, ACSs,
  publications, distributions, content guards (not x509 or RHSM), groups, tasks, task
  schedules, uploads and upstream Pulps; not on domains, exporters or importers.
* `list_roles/` answers `{roles: [{role, users, groups}]}` with user and group names.
  `my_permissions/` answers `{permissions}` (`get_all_permissions` of the object).
* `add_role/` and `remove_role/` take `{role, users, groups}` and answer 201. They refuse
  (400) a role without a permission for the object type, an assignment that exists, and the
  removal of one that does not.

### Domains

Read in pulpcore 3.123.1 (`app/viewsets/domain.py`, `app/models/domain.py`,
`app/serializers/domain.py`). Not live: pulp1 has `DOMAIN_ENABLED` off.

* **Without `DOMAIN_ENABLED`** `domains/` still answers, with the `default` domain only, and
  no URL serves another domain. pulp-tui shows the screen only with `domain_enabled: true`.
* **The `default` domain** is refused for update, delete and `migrate/` (400).
* **Update and delete are tasks** (202). The delete raises PLP0007 ("You cannot delete a
  domain that still contains repositories with content") while a repository version holds
  content. Else the `BEFORE_DELETE` hook deletes the orphan content and the artifacts, and
  Django refuses while a repository, remote, distribution, publication, exporter, importer,
  ACS, upload or upstream Pulp of the domain exists (`on_delete=PROTECT`). The delete is one
  transaction (django-lifecycle 1.3.0 `delete` is `atomic`) and artifact files go on commit,
  so a refused delete changes nothing. Tasks of the domain are deleted with it (`CASCADE`).
* **`domains/migrate/`** acts on the domain of the URL (`/pulp/<name>/api/v3/domains/migrate/`)
  and takes `{storage_class, storage_settings}`. It copies the artifacts, then switches the
  domain; the old files stay. The task locks the domain.
* **Storage secrets are write-only**: `access_key`, `secret_key`, `security_token`,
  `cloudfront_key` (S3), `account_name`, `account_key` (Azure). GET shows only their names in
  `hidden_fields`. Azure `connection_string`, `token_credential` and `sas_token` are **not**
  write-only (pulpcore 3.123.1): GET returns them. Pulp also takes the Django setting names
  in any case (`AWS_SECRET_ACCESS_KEY`, `AZURE_CONNECTION_STRING`). pulp-tui masks all of them
  in Inspect, Details and Logs.
* `default_content_guard` must be a guard of the same domain, and only on update.

### Sync and remotes

* **Refused sync pairs** (pulp_rpm 3.39.5, HTTP 400): `mirror_complete` with `skip_types`
  or `autopublish`, and both mirror policies with `retain_package_versions` above 0. The
  sync form refuses them first (it reads both settings from the repository row).
* **Sync without a remote** (pulpcore `RepositorySyncURLSerializer`, HTTP 400): "This field is
  required since a remote is not set on the repository." The sync form refuses it first.
* **`retain_package_versions`** (pulp_rpm `IntegerField(min_value=0)`, not nullable; model
  default 0 = keep all): an emptied field on edit sends 0 (live,
  `rpm_retain_package_versions_cleared_through_the_tui`).
* **Sync options** `optimize` (RPM, File, Deb; default `true`) and `skip_types` (RPM) exist
  in every tested version. pulp_rpm skips a sync whose upstream, remote and repository did
  not change since the last sync (progress report `sync.was_skipped`); with `optimize` false
  it syncs in full (live, `rpm_mirror_content_only_sync_with_options_through_the_tui`).
* **Clearing remote fields** (3.123.0). `null` unsets `ca_cert`, `client_cert`,
  `client_key`, `username`, `password`, `proxy_username`, `proxy_password`, pulp_rpm `sles_auth_token`, pulp_deb `gpgkey` and the repository `remote`
  (`hidden_fields` shows `is_set: false`; checked live). The Edit forms send `null` for `-`
  and a cleared Remote.
* **Remote secrets.** GET never returns `username`, `password`, `client_key`,
  `proxy_username`, `proxy_password`; but it returns pulp_rpm `sles_auth_token`, and
  `headers` can hold `Authorization`. pulpcore 3.123.0 refuses credentials in `url` ("The
  remote url contains username or password") and `proxy_url`; older servers allow them.
  `export-repos` leaves out all of these.
* **URL schemes.** Pulp 3.123.1 saves any `url` scheme (`host:8000/x` has scheme `host`);
  only the sync fails ("URL ... not supported"). Downloaders handle `http`, `https` and
  `file` (under `ALLOWED_IMPORT_PATHS`). `proxy_url` is the same (`proxy.corp:3128` is
  saved). The remote form accepts these three schemes and an `http`/`https` proxy (it does
  not check an unchanged value); the Upstream Pulp form accepts `http` and `https`.
* **ULN remotes** (pulp_rpm 3.39.5, `rpm.UlnRemote`): `url` must be `uln://<channel>`
  (`validate_url`); `username` and `password` are required and write-only; an RPM repository
  syncs from a ULN remote (`REMOTE_TYPES = [RpmRemote, UlnRemote]`), an RPM ACS does not take
  one (`REMOTE_TYPES = [RpmRemote]`). Remote names are unique across all remote types
  (`DomainUniqueValidator` on `Remote`), so pulp-tui finds a picked name in the first
  collection that has it.
* **Git remotes** (pulpcore 3.123.1, `file.FileGitRemote`): `policy` is a hidden field
  (always `immediate`, not in the schema); `git_ref` defaults to `HEAD` and is not nullable,
  so a cleared Git ref is sent as `HEAD`. The sync runs `git clone --bare --depth 1 --branch
  <ref>`, and after a failure a full bare clone (GitPython; the image has `git`). A shallow
  clone fails on the dumb HTTP protocol, the full clone works: the fixture is a bare
  repository with `git update-server-info` on the static fixture server. Username and
  password go into the clone URL for `http` and `https` only. A File ACS does not take a Git
  remote (`REMOTE_TYPES = [FileRemote]`).

### Replication

* **Replica bindings.** Native replication pins distributions with `repository_version`;
  pulpcore 3.95.3 does not pin (`repository`, `publication: null`). pulp-tui shows both as
  pinned and re-points them.
* **Upstream Pulp fields** (3.123.1): `username`, `password` and `client_key` are write-only
  (`hidden_fields`); `ca_cert` and `client_cert` are readable. Each accepts `null`, which
  removes the stored value. Replication labels what it creates `UpstreamPulp=<upstream pk>`;
  a delete of the Upstream Pulp keeps those objects.
* **Group end.** On 3.63, `replicate_distributions` sets `all_tasks_dispatched`; on 3.123 it
  stays false, and from 3.95 the group ends with `finalize_replication` or a failed or
  canceled task. See `docs/ARCHITECTURE.md`.
* **Canceled tasks.** On 3.63, a canceled sync leaves `failed 0, canceled 1`. From 3.95, a
  canceled `replicate_distributions` dispatches no finalizer, so the group has only
  `canceled 1`. pulp-tui reports canceled or skipped tasks as not complete.
* **History** lists the groups of the five newest `replicate_distributions` tasks that reserve
  the Upstream Pulp (`tasks/?reserved_resources=<upstream href>`; 3.123.1 dispatches it with
  `shared_resources=[server]`). The group description holds the name at run time, so it is
  no link after a rename. An unknown filter is a 400 in pulpcore, never a wider list.
  3.63.36 and 3.95.3 reserve only `pdrn:<domain>:servers` (no Upstream Pulp), so there no task
  links a run to its Upstream Pulp. When no run reserves the Upstream Pulp and the newest run of
  the server reserves none, History pages through `task-groups/` for `Replication of <name>`
  (five matches or 20 pages of 100) and says that it matched by name.
* **Deb replica (fallback).** A structured APT publication serves each release of its
  release components under `dists/<release>/`, and a flat `/` as an ordinary release under
  `dists/flat-repo/` (pulp_deb 3.11.1 `_ReleaseHelper`); a simple one serves `dists/default/`
  with component `all`. So the replica remote syncs those directories. Without
  `components`, `architectures` and `sync_sources`, it syncs every component and
  architecture of the served Release and no source packages (pulp_deb defaults). An APT
  publication without its own `signing_service` is signed with the repository's
  (`release_signing_service`, structured releases only; a simple release uses the
  repository's `signing_service`). A verbatim one serves the synced Release files and
  signatures as they are, at their upstream paths. Each `ReleaseFile` of the version has the
  path of its Release file (seen live): `dists/<d>/Release`, or `Release` for a flat repository
  at the root. pulp_deb 3.11.1 (`_handle_distribution`) syncs a remote distribution that ends in
  `/` as a flat repository in that directory (`/`: the root), stored as distribution `flat-repo`
  with component `flat-repo-component` and one `Packages` index. So the replica remote of a
  verbatim publication syncs `<d>`, or `/` for a flat one, and verify probes `Release` and
  `Packages` there (`live_replication`, flat). A verbatim publication of a version that no
  sync made serves no Release file: its row is `SKIP`.
* **Upstream `base_url`** is the upstream's content origin: in the test containers
  `http://host.containers.internal:8081`, which pulp1 reaches and the host does not resolve
  (seen live). So the fallback reads the upstream through its API only; the replica
  remote gets that URL as it is.
* **`api_root`** of an Upstream Pulp is relative and needs a trailing slash (`pulp/`).
* **Deletion policy.** In 3.123.1, `remove_missing` deletes for every policy but `nodelete`:
  `labeled` deletes the objects labeled with this Upstream Pulp that are gone upstream, `all`
  every one that is missing upstream. The default is `all`. pulp-tui asks for typed
  confirmation for every policy but `nodelete`, also for an empty or missing one.

### Alternate content sources

Source: pulpcore 3.123.1 `serializers/acs.py` and the `refresh` views of pulp_rpm 3.39.5,
pulp_file 3.123.1 and pulp_deb 3.11.1.

* **The remote must be `on_demand`**, else HTTP 400.
* **Create** answers with the object; edit and delete are tasks. **Refresh** answers with a
  task group, one `synchronize` per path (Deb: one for the URL; it ignores paths). On 3.95.3
  and 3.123.1 the group never gets `all_tasks_dispatched` (on 3.63.36 the RPM refresh sets
  it, live), but all its tasks exist at once, so `TaskGroup::is_final` ends it with its
  tasks either way. pulp_deb 3.5.4 (3.63.36) has no Deb ACS. Descriptions:
  `Refreshing <n> alternate content source path(s).` (RPM), `Refreshing <n> alternate
  content source paths.` (File), `Refreshing Alternate Content Source '<name>'.` (Deb).
  pulp_file does not set `last_refreshed`.
* **RPM paths with a common package can fail a refresh at random.** pulp_rpm dispatches the
  path syncs with shared locks only (`shared_resources=[acs.remote, acs]`, `viewsets/acs.py`),
  so with two workers they run at the same time with the same remote. pulpcore
  `RemoteArtifactSaver` reads the remote artifacts of a package, then calls `bulk_create`
  (`plugin/stages/artifact_stages.py`). When both paths hold the package, both insert it, and
  one sync fails with `duplicate key value violates unique constraint
  "core_remoteartifact_content_artifact_id_remo_f11e4ef2_uniq"`. The group ends with one
  failed task, so the Task Center row fails. Live: once in about 25 refreshes of `rpm/` and
  `rpm2/` (2026-11-05). A second refresh completes (source): the sync that completed made the
  rows, and the saver only updates their URL. The live test uses paths with no common package.
* **A path edit changes other sources.** Removing a path deletes it from every ACS with that
  path. The Edit form lists them and asks first.
* **Delete** removes the hidden path repositories, but not the pulp_deb `<name>--repository`.
* **An ACS keeps its remote.** `AlternateContentSource.remote` is `on_delete=PROTECT`
  (`models/acs.py:33`): the delete task of a remote that an ACS uses fails (ProtectedError).
  An ACS takes only a remote of its own plugin type (`REMOTE_TYPES`, `serializers/acs.py:70`),
  and the ACS list has no `remote` filter. So the remote delete reads the ACS list of the
  remote's type, when the server has that endpoint, and refuses before any request.

### Export and import

Source: pulpcore 3.123.1 `tasks/export.py`, `tasks/importer.py` and their serializers.
"Live": seen in `live_export`.

* **Allowed paths.** Exporter paths must be under `ALLOWED_EXPORT_PATHS`, import `toc` and
  `path` under `ALLOWED_IMPORT_PATHS` (after `realpath`). Both are empty by default: "Path '...' is not an
  allowed export path".
* **Pulp export.** The task creates the directory. A full export without `versions` takes
  the latest version of each repository. It writes `export-<id>-<YYYYMMDD_HHMM>.tar` (or `.tar.0000`, ... with
  `chunk_size`) and `...-toc.json` (names, CRC32). Only a successful export becomes
  `last_export`; deleting that record sets it to null. Exporter PATCH and DELETE are tasks;
  export DELETE answers 204. Deletes never remove files.
* **Incremental export** (`full: false`) starts at `last_export`, else 400 "Incremental
  export can only be requested when there is a previous export or start_versions= has been
  specified." For a repository added after the last export, the POST answers 202 but the
  task fails with only the repository as the error (`<Repository ...: pk=...>`, a
  `KeyError`; live). The export has only the new artifacts but the full content metadata,
  so an import into the same server sets the full content (live).
* **`versions` and `start_versions`** need one version per exporter repository (all live).
  Another count: "Number of versions (N) does not match the number of Repositories (M)"
  (the numbers are swapped). Another set of repositories: "Requested RepositoryVersions
  must belong to the Repositories named by the Exporter!". A missing version: "Invalid
  hyperlink - Object does not exist.". `start_versions` with `full` true: "start_versions
  is only valid for incremental exports (full=False)". All are 400. With `full: false`,
  `start_versions` works without a previous export.
* **`dry_run`** ("Generate report on what would be exported and disk-space required") is in
  `PulpExportSerializer`, but neither the viewset nor `pulp_export` reads it: the task writes
  the tar and TOC and sets `last_export` (live, `pulp_export_dry_run_still_writes_the_export`).
  The Export form does not offer it.
* **Filesystem export** takes one of `publication` and `repository_version`, and optionally
  `start_repository_version` (any version; the form sends one of the same repository): then
  the task writes only the artifacts that the start version lacks, plus all published metadata
  of a publication (`_export_publication_to_file_system`). Pulp checks
  the allowed path again on the export POST. The directory
  must not exist (live): "Cannot export to directories that contain existing data"
  (publication) or "[Errno 17] File exists" (version). `hardlink` falls back to `write`
  across file systems; `hardlink` and `symlink` need file system storage.
* **Import check** answers 200 with `is_valid` and `messages`: TOC allowed, readable, its
  directory writable; `repo_mapping` is JSON. It answers for each of `toc` and `path` that the
  request has. It does not list the repositories of the export, and the TOC has only the
  chunk names, their CRC32 and `meta` (`views/importer.py`, `tasks/export.py`): the
  repository list is in the tar, read by the `pulp_import` task. No API gives it before the import.
* **Import source.** A Pulp import takes exactly one of `toc` (the chunks it lists are checked
  and joined) and `path` (one `.tar`, an export without chunks); both or neither is 400.
  pulp-tui sends a file that ends in `.json` as `toc`, any other as `path` (live).
* **Import** answers 202 with a task group `Import of <toc>`: `pulp_import` checks the chunks,
  imports the artifacts and dispatches one `import_repository_version` per repository,
  which sets the exact exported content. Only a cancel sets `all_tasks_dispatched` (live), so
  `TaskGroup::is_final` ends it when `pulp_import` completed and no task waits or runs.
* **Import mapping.** The destination is the `repo_mapping` pair, else the same name; a
  missing one is skipped with only a log line (no repository task). An unknown destination gets 400 "Failed to find
  repositories from repo_mapping" on create, but 500 on PATCH, and `repo_mapping` is then
  empty (live). A repository delete removes its pair. pulp-tui never sends
  `create_repositories` (refused with a mapping). Importer DELETE answers 204.

## pulp_deb

Read in pulp_deb 3.11.1 (`pulp_deb/app/`) and its OpenAPI.

* **Package fields**: `package` (not `name`), `version`, `architecture`, `relative_path`,
  `sha256`.
* **Package upload** (`content/deb/packages/` with `file` or `upload`, `repository`,
  `distribution`, `component`; `SinglePackageUploadSerializer`). With a repository, pulp_deb
  lists the package in that release (defaults `pulp` and `upload`, each on its own): the new
  version also gets the release component, release architecture and PRC. The task's
  `created_resources` also names these records when they are new, so pulp-tui takes the one
  under `content/deb/packages/`. Pulp sets `relative_path` (`pool/...`) from the package and
  reuses an equal package (same `sha256`, `relative_path` and metadata).
* **Other uploads** take `file` or `upload` and `repository` too, but no release:
  `content/deb/installer_packages/` (a `.udeb`; pulp_deb refuses a package whose section is not
  `debian-installer`, and `packages/` refuses one whose section is) and
  `content/deb/generic_contents/` (`relative_path` required; an equal `sha256` and path is
  reused). pulp_deb 3.5.4, 3.8.1 and 3.11.1 have both endpoints (read in their source).
* **Source package upload** (`content/deb/source_packages/`, JSON, in 3.5.4, 3.8.1 and 3.11.1)
  takes no file: `artifact` is the `.dsc` as an artifact. The task parses it and refuses it
  unless an artifact with the `sha256` and size of each `Checksums-Sha256` file exists
  (`SourcePackageSerializer.deferred_validate`). pulp-tui looks each one up
  (`artifacts/?sha256=`) and uploads only the missing ones: `artifacts/` (multipart `file`, 201),
  or above one chunk `uploads/` and `{upload}commit/` with `sha256` (a task). A second POST of an
  existing artifact answers 400, so the lookup makes a rerun safe. pulp_deb reuses a source
  package with the same `source` and `version` (`SourcePackageSerializer.retrieve`), also when
  its `.dsc` differs, and adds that one; its `sha256` is the `.dsc` digest. Before any upload,
  pulp-tui looks up `content/deb/source_packages/?source=&version=` (the `Source` and `Version`
  of the local `.dsc`; `SourcePackageFilter` has both, 3.11.1) and refuses a package with
  another `sha256`; after the POST it compares the digest again. pulp_deb 3.11.1 answers HTTP
  500 to a read or list of source packages whose `fields` lack `artifacts` (`KeyError` in the
  serializer's `__init__`; live), so pulp-tui reads `fields=…,sha256,artifacts`. pulpcore allows `artifacts/`
  (list, create) only to admins by default.
* **Release record create.** `content/deb/releases/`, `release_components/` and
  `release_architectures/` (JSON with `repository`, in 3.5.4, 3.8.1 and 3.11.1) answer 202: the
  task reuses an equal record (`retrieve`) and adds it to a new version (none when the latest
  version has it). A release also makes the records of its `components` and `architectures`.
  `package_release_components/` is a plain create: 201 with the record, and a new record on each
  POST, also for the same package and release component (seen live). A second record of a pair
  breaks each later upload of that package into that release ("get() returned more than one
  PackageReleaseComponent", seen live) until orphan cleanup removes it. So pulp-tui looks the pair
  up first (`?package=&release_component=`) and adds the record with `modify/`.
* **`sha256` is `null` until download** for `on_demand` packages. Verify takes the digest
  from the served `Packages` index (what apt checks) and fails when the API differs.
* **Structured copy.** `deb/copy/` with `structured: true` also copies the
  package-release-component (PRC) units, release components, architectures and releases.
  pulp-tui always sends it.
* **PRCs of removed packages.** A structured publication publishes every PRC of the version,
  even if the package is gone. Deb `modify/` removes them with the package (seen live);
  pulp-tui also sends them itself, for versions that do not (not checked). Releases,
  components and architectures stay.
* **`content/deb/source_packages/` answers HTTP 500** (`KeyError: 'artifacts'`) when
  `fields` lacks `artifacts`. The compare asks for `source,version,artifacts`.
* **Compare keys.** 13 content types. PRCs (one per index architecture, which the API does
  not show) and index and Release file units (no `sha256`) are keyed by unit id; then the
  compare matches `content_summary.present`.
* **Copy options.** `dependency_solving: true` fails with `FeatureNotImplementedError`, so
  pulp-tui sends `false`. `dest_base_version: 0` starts from the empty version 0.
* **Release signing order**: the publication's service, then the repository's release
  override (`signing_service_release_overrides`), then the repository's service.
* **Override PATCH merges.** Only the distributions in the body change; `null` deletes one, `{}`
  changes nothing. pulp-tui sends `null` for each key that a snapshot repository or an edit
  must lose. `package_signing_fingerprint_release_overrides` is the same, but uses `""`.
* **Signing fields by version.** `signing_service`, `signing_service_release_overrides` and
  `publish_upstream_release_fields` (default `true`) are in 3.5.4, 3.8.1 and 3.11.1.
  `package_signing_service`, `package_signing_fingerprint` and its release overrides are
  only in 3.11.1 (migration `0035_package_signing`). Each signing field takes only its kind of APT signing service. Package
  signing runs on upload and `modify/`, not on sync or `deb/copy/`.
* **`excluded_package_metadata_fields`** (3.11.1, migration `0042`, default `[]`, also for publications that
  do not set it). An edit clears it with `[]`.
* **`AptPublication`** defaults: `structured=true`, `simple=false`. A structured
  publication serves `dists/<release>/<component>/`; a simple one serves
  `dists/default/Release` and `dists/default/all/binary-<arch>/Packages` (pulp_deb
  `tasks/publishing.py`). Verify probes the paths of each mode that the publication has. A
  Deb Content View snapshot takes the mode of the newest APT publication of its source.
* **Structured releases come from release components**, not from Release units: one
  `dists/<distribution>/` per distribution of the version's release components (`/` is
  `flat-repo`), with a Release file that pulp_deb makes when the version has no Release unit
  (removed with `modify/`, seen live). With `simple`, a structured `default` is dropped. A
  version without release components gets `dists/default/` (component `empty`) on 3.11.1,
  and no structured release on 3.5.4 and 3.8.1 (read in their source). Verify probes each.
* **Verbatim publications** (`publications/deb/verbatim/`, `VerbatimPublicationSerializer`):
  the request takes only `repository_version` or `repository`; the answer has no `checkpoint`
  and no mode field, so pulp-tui tells one by its href. It publishes every file of the version at
  its path, so the served `Release` is the synced upstream one. A version made by upload, copy or
  modify has no Release or index files (`NEW_VERSION_DROPS`), so its verbatim publication serves
  only the packages. A Deb distribution takes a verbatim publication, and a delete unbinds it, as
  an APT one. The Deb Publications screen reads both collections from the start of the list
  (DRF `LimitOffsetPagination` has no `max_limit`) and merges them by `pulp_created`.
* **`signing-services/` has no type filter**, so Deb and RPM services look the same.
* **No publication reuse for Deb Content Views.** A publication signed with the repository
  keys stores `signing_service: null`, as an unsigned one, and an unchanged copy makes no
  new version. So a `none` publish reused a signed publication (seen live). pulp-tui
  creates a publication for each Deb Content View version (task href checkpointed first).
* **Empty `package_signing_fingerprint`.** Older pulp_rpm returns `""` for an RPM repository
  without one; pulp_rpm 3.39.5 refuses `""` (nullable, `minLength` 1) with 400. `import-repos`
  leaves out every `""` field (the server default applies) and does not report it as "differs".

## Shared database (PostgreSQL)

### Database TLS and `ca_cert`

pulp-tui uses sqlx 0.9.0 with `tls-rustls-aws-lc-rs` (it still takes `webpki-roots`).

* **The trust anchors are the Mozilla roots in the binary (`webpki-roots`), not the OS
  store.** `ca_cert` (`sslrootcert`) adds to them (sqlx `tls_rustls.rs`; read in the source,
  not tested with a public certificate). A Pulp profile `ca_cert` (reqwest) adds to the
  system roots instead, so a CA that the host trusts still needs `ca_cert` here.
* A `ca_cert` file may hold several certificates.
* pulp-tui sets `sslmode=verify-full` except for a Unix socket or loopback, so the URL host
  must be in the certificate (an IP needs an IP SAN). Loopback keeps the URL `sslmode`
  (default `prefer`).

Checked by hand on 2026-10-23 (the live tests use `127.0.0.1` without TLS): PostgreSQL
16.15, `ssl=on`, a private-CA certificate (SAN `IP:<host-ip>`, `DNS:<host>`,
`DNS:localhost`, `IP:127.0.0.1`), `pulp-tui db migrate` through the LAN address:

| `[database]`                                     | Result                                                        |
| ------------------------------------------------ | ------------------------------------------------------------- |
| `ca_cert` = the private CA                       | connects (`schema 0 -> 2`)                                    |
| `ca_cert` = an unrelated CA, then the private CA | connects                                                      |
| no `ca_cert`                                     | `invalid peer certificate: UnknownIssuer`                     |
| no `ca_cert`, `SSL_CERT_FILE` = the private CA   | `invalid peer certificate: UnknownIssuer` (OS store not read) |
| `ca_cert` = an unrelated CA                      | `invalid peer certificate: UnknownIssuer`                     |
| `ca_cert` = the private CA, host `172.17.0.1`    | not valid for name (below)                                    |

The last case: `invalid peer certificate: certificate not valid for name "172.17.0.1";
certificate is only valid for IpAddress(<host-ip>), DnsName("<host>"), DnsName("localhost")
or IpAddress(127.0.0.1)`.

### Other behaviors

* `options=-csearch_path%3D<schema>` in the URL works; the live tests use it for one schema
  per test.
* A role with only `SELECT`, `INSERT`, `UPDATE` and `DELETE` works for `import-cv`,
  `export-cv` (identity columns need no sequence grant) and `db migrate` on a current
  schema (checked by hand). An upgrade needs a role that can create tables.
* Identifiers are `BIGINT`, because sqlx `Any` reads integers as `i64`.

## Unsupported or limited

* Plugins other than pulp_rpm, pulp_file and pulp_deb (container, python, …).
* pulp_deb: no dependency solving. Fallback replication copies binary packages only (no
  source packages, udebs or installer files).
* RPM layout `nested_by_digest`: served-content checks skip the package probe (metadata is
  still checked).
* ULN: a sync from a ULN remote is tested with mock HTTP only (it needs a ULN account).
* Signing services can be set on repositories, not created (that needs server scripts).
* Content guards are not replicated (fallback replication). The object role form lists all
  roles, not only the ones for the object type (Pulp refuses the others).
* Reusable filter sets are not implemented; filters are per view repository, so a later
  table can be added without a change to published versions. A composite has no filters
  and cannot be a component of another composite.
* Fallback replication does not delete destination objects or copy Pulp metadata; use
  native replication for that.
* Pulp domains: the profile `domain` prefixes all API paths; not tested on a
  domain-enabled server. The Domains screen does not rename a domain, set its labels or its
  default content guard (the guard picker lists the guards of the profile's domain only).
* Bearer tokens are sent as given; no token refresh (OIDC).
* Windows builds are not tested.
