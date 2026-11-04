# Compatibility and capability matrix

## Tested versions

All live tests (`tests/run-live.sh`, `real` included) passed on 2026-11-04 on fresh
containers of `quay.io/pulp/pulp:latest`:

| Component                             | Version                                             |
| ------------------------------------- | --------------------------------------------------- |
| pulpcore, pulp_file                   | 3.123.1                                             |
| pulp_rpm                              | 3.39.5                                              |
| pulp_deb                              | 3.11.1                                              |
| PostgreSQL (optional shared database) | 16.15 (`docker.io/library/postgres:16`), sqlx 0.8.6 |
| Client OS                             | Fedora 44 (Linux 7.2), Rust 1.99.1                  |

On 2026-11-04, all live scenarios, `real` included (Debian `main`, Rocky 9 BaseOS and
AppStream), passed on fresh containers of these images:

| Image                       | pulpcore / pulp_file | pulp_rpm | pulp_deb | `real` |
| --------------------------- | -------------------- | -------- | -------- | ------ |
| `quay.io/pulp/pulp:3.63.36` | 3.63.36              | 3.27.12  | 3.5.4    | 826 s  |
| `quay.io/pulp/pulp:3.95.3`  | 3.95.3               | 3.34.0   | 3.8.1    | 807 s  |
| `quay.io/pulp/pulp:latest`  | 3.123.1              | 3.39.5   | 3.11.1   | 644 s  |

* On 3.63.36, `content` first failed in `acs_lifecycle`, a test newer than the 2026-10-30
  run on this image: pulp_deb 3.5.4 has no `acs/deb/deb/`, and the RPM refresh sets
  `all_tasks_dispatched`. The test now skips an ACS type without the endpoint and checks the
  flag only from 3.95; `content` then passed on all three images.

* On 3.95.3, one `native_replication` run failed once with "connection closed before
  message completed" on `POST upstream-pulps/`. The next two runs passed.
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
| Chunked upload                            | `uploads/` and `{upload_href}`                                       | message for files above `upload_chunk_mb` |
| Native replication                        | pulpcore ≥ 3.23, `upstream-pulps/`, `{upstream_pulp_href}replicate/` | screen points to the fallback             |
| Fallback replication of a type            | `status/` component on source and destination                        | dry run stops (3)                         |
| Alternate content sources                 | the plugin, `acs/rpm/rpm/`, `acs/file/file/`, `acs/deb/deb/`         | endpoint message on the screen            |
| RPM comps, advisory upload (**U**, **A**) | `post` of `rpm/comps/`, `content/rpm/advisories/`                    | endpoint message                          |
| Version repair (**F**)                    | `{rpm_rpm_repository_version_href}repair/` (4)                       | endpoint message                          |
| Reclaim space (**R**)                     | `repositories/reclaim_space/`                                        | endpoint message                          |
| Current user                              | `login/`, else `users/?username=<Basic-auth name>`                   | header shows "anonymous" (5)              |
| Users, groups, roles                      | `users/`, `roles/`                                                   | screen not supported                      |
| Domain scope of a role assignment         | `status/` `domain_enabled: true`                                     | Assign role has no Domain field           |
| Pulp exporters                            | `exporters/core/pulp/`                                               | screen not supported                      |
| Filesystem exporters                      | `exporters/core/filesystem/`                                         | screen not supported                      |
| Pulp importers, import check              | `importers/core/pulp/`, `importers/core/pulp/import-check/`          | screen not supported                      |
| Delete a task; purge finished tasks       | `delete` on `{task_href}`; `tasks/purge/`                            | endpoint message                          |
| Workers, Task groups screens              | `workers/`, `task-groups/`                                           | screen not supported                      |
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

### Form fields

| Form                           | Optional fields                                                                          |
| ------------------------------ | ---------------------------------------------------------------------------------------- |
| Repository                     | checksum type, signing services, retain package versions, manifest, `retain_checkpoints` |
| RPM repository                 | compression, layout, repo config, package signing                                        |
| Remote                         | timeouts, `max_retries`, `headers`; Deb `excluded_package_metadata_fields`               |
| Distribution                   | `hidden`, `checkpoint`; RPM `generate_repo_config`                                       |
| Publication (**c**)            | options of `rpm.RpmPublication`, `file.FilePublication`, `deb.AptPublication`            |
| Sync                           | `optimize` (RPM, File, Deb), `skip_types` (RPM): `{Rpm,File,Apt}RepositorySyncURL`       |
| Upstream Pulp (`UpstreamPulp`) | `policy`, replica download policy, concurrency, retries, timeouts                        |

* A hidden sync option uses the server default.
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
  `additive`), versions, diff, publish, distribute, served metadata and package digest,
  update, rollback.
* **File**: create, upload (single and chunked with progress), remove content (old versions
  stay), sync from a manifest, publish, distribute, served manifest, update, rollback,
  mismatch detection.
* **Distributions**: base path conflict (409), `base_path` PATCH.
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
  and File, no secret in the file, remote found by name, idempotent rerun.
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
  importer delete (204).
* **Comps and advisories** (`live_content`): a comps.xml makes a version with its group; a
  second with `replace` keeps only its own group; an advisory JSON makes the next version.
* **Version repair** (`live_content`): RPM version synced `immediate`, with and without
  `verify_checksums`: 0 missing, 0 corrupt, 0 repaired. A damaged file is not tested (needs
  access to the server storage).
* **Reclaim space** (`live_content`): see the behavior below.
* **Alternate content sources** (`live_content`, 3.123.1): `immediate` remote refused (400);
  refresh of RPM (two paths), File and Deb ACS as task groups (also as one Task Center
  task); path edit; delete.
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
  (replication, import, ACS refresh; see below), never only the three-poll rule.
* **Group cancel** (`PATCH {task_group_href}`) exists from 3.95, not on 3.63. It cancels
  every waiting or running task and sets `all_tasks_dispatched` (pulpcore
  `tasking/tasks.py`). The confirmation first reads the group (description, unfinished
  tasks); the row keeps the state that Pulp reports.

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
  Verify reads the newest checkpoint publication at its timestamp. The rollback confirm reads
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

### Content and digests

* **RPM content types.** pulp_rpm 3.39.5 has 11; the compare and the Content browser cover
  all. Langpacks have no id, so the compare keys them by `digest`. It warns when
  `content_summary.present` counts other units than it lists, and names types without an
  endpoint (older plugins), as the restore preview does.
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

### Copy and Content View snapshots

* **`rpm/copy/` adds related units** (pulp_rpm `tasks/copy.py`, also without dependency
  solving): the packages and modules of an advisory, the groups of categories and
  environments, the newest package of each group member not in the request; not the
  packages of a module. The result changes with the batch size, so a filtered RPM snapshot uses `modify/`, which adds exactly the given units.
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

### Sync and remotes

* **Refused sync pairs** (pulp_rpm 3.39.5, HTTP 400): `mirror_complete` with `skip_types`
  or `autopublish`, and both mirror policies with `retain_package_versions` above 0. The
  sync form refuses them first (it reads both settings from the repository row).
* **Sync options** `optimize` (RPM, File, Deb; default `true`) and `skip_types` (RPM) exist
  in every tested version.
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

### Replication

* **Replica bindings.** Native replication pins distributions with `repository_version`;
  pulpcore 3.95.3 does not pin (`repository`, `publication: null`). pulp-tui shows both as
  pinned and re-points them.
* **Group end.** On 3.63, `replicate_distributions` sets `all_tasks_dispatched`; on 3.123 it
  stays false, and from 3.95 the group ends with `finalize_replication` or a failed or
  canceled task. See `docs/ARCHITECTURE.md`.
* **Canceled tasks.** On 3.63, a canceled sync leaves `failed 0, canceled 1`. From 3.95, a
  canceled `replicate_distributions` dispatches no finalizer, so the group has only
  `canceled 1`. pulp-tui reports canceled or skipped tasks as not complete.
* **History** pages through `task-groups/` for `Replication of <upstream>` and stops at
  five matches or 2000 groups.
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
* **Filesystem export** takes one of `publication` and `repository_version`; Pulp checks
  the allowed path again on the export POST. The directory
  must not exist (live): "Cannot export to directories that contain existing data"
  (publication) or "[Errno 17] File exists" (version). `hardlink` falls back to `write`
  across file systems; `hardlink` and `symlink` need file system storage.
* **Import check** answers 200 with `is_valid` and `messages`: TOC allowed, readable, its
  directory writable; `repo_mapping` is JSON.
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

pulp-tui uses sqlx 0.8.6 with `tls-rustls-aws-lc-rs`.

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
* pulp_deb: no `.deb` upload, no replication, no verbatim publications, no dependency
  solving. A verbatim publication made elsewhere is served when newest (Verify and Rollback
  read it) and protects its version, so the delete impact lists `publications/deb/verbatim/`.
* RPM layout `nested_by_digest`: served-content checks skip the package probe (metadata is
  still checked).
* Signing services can be set on repositories, not created (that needs server scripts).
* Content guards are only shown and set by href, not created or replicated.
* Reusable filter sets are not implemented; filters are per view repository, so a later
  table can be added without a change to published versions. A composite has no filters
  and cannot be a component of another composite.
* Fallback replication does not delete destination objects or copy Pulp metadata; use
  native replication for that.
* Pulp domains: the profile `domain` prefixes all API paths; not tested on a
  domain-enabled server.
* Bearer tokens are sent as given; no token refresh (OIDC).
* Windows builds are not tested.
