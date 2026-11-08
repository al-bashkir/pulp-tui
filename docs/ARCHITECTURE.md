# Architecture

pulp-tui is one Rust binary. It talks to Pulp 3 only through the documented REST API. It
never reads or writes the Pulp database, and it never runs `pulp`, `curl` or other tools
for normal operation.

## Layers

| Layer          | Modules                                                       |
| -------------- | ------------------------------------------------------------- |
| Transport      | `src/pulp/client.rs`                                          |
| Capabilities   | `src/pulp/caps.rs`                                            |
| Typed adapters | `src/pulp/models.rs`, `ops.rs`, `rpm.rs`, `file.rs`, `deb.rs` |
| Services       | `src/cv.rs`, `replication.rs`, `filters.rs`, `repo_export.rs` |
| Storage        | `src/store.rs`                                                |
| TUI            | `src/tui/` (`view.rs` renders only)                           |

* **Transport**: auth (basic, bearer, mTLS), TLS (always verified, custom CA bundles),
  proxies, timeouts, a concurrency semaphore, retries for reads only, pagination, a
  response size limit, error mapping. It keeps every href below the API root and records
  each attempt in the session log.
* **Capabilities**: `status/` (versions, health, content origin), the current user, and
  the live OpenAPI schema (endpoints, methods, operation ids, fields, enums).
* **Typed adapters**: plugin endpoints, task and task-group waits, find-or-create,
  publications, distributions, uploads, content lists, diffs, served-content checks.
* **Services** use only the layers above: Content Views, replication, filter rules
  (`rpmvercmp` port, dpkg version comparison), repository and remote definitions.

Plugins: the REST operations of one plugin go in `src/pulp/<plugin>.rs`. `RepoType`
(`models.rs`) holds the per-type facts (component, label, endpoints, schema names, required
`Feature`). Dispatch on it with an exhaustive `match`, with no `_ =>` arm and no
`unwrap_or(RepoType::X)`, so a new type is a compile error at each place that must handle
it.

## Runtime

### Event model

* The loop (`src/tui/mod.rs`) waits on keys, job messages and a 250 ms tick.
* A job is a Tokio task. It returns a closure that changes `App` on the UI thread.
  Rendering only reads state.
* Each list load has a generation number; a stale response is dropped. A list reloads when
  a tracked task ends, and when an operation job fails (to show the state it left).
* Esc aborts only list loads (`App::abort_loads`). An operation job waits until it ends;
  Connect (`App::abort_all`) or quit stops the wait. Pulp tasks always continue: only
  *Cancel task* asks Pulp to cancel one. After that request, the Task Center polls the task
  at the next tick, also when a workflow polls it. A failed read after an accepted cancel is
  a warning, not a failed cancel.

### Requests and retries

* At most `max_concurrent_requests` requests run per server.
* GET is retried on network errors, timeouts, 429, 502, 503 and 504, with exponential
  backoff, jitter and `Retry-After`.
* POST, PATCH, PUT and DELETE are never retried. Workflows make a rerun safe instead:
  find-or-create by name, publication reuse, checkpoints with submitted task hrefs.
* Responses above `max_response_mb` are rejected.
* Every href (server, import file, database, `next` links) must stay on the same origin
  and below the configured API root.

### Tasks

Pulp tasks are the source of truth.

* The Task Center stores task references in the local file only (never in a shared
  `[database]`). The store work goes through one queue (`App::task_store`): the writes of a
  task apply in the order they were made, and the re-attach reads after them. The queue is
  not a job, so Connect (`abort_all`) does not stop a write; quit waits up to 2 s for it.
* It polls each unfinished task: the interval starts at `poll_min_ms`, grows 1.5x while
  nothing changes, resets on a change, has ±20 % jitter. At most three polls run at a time.
  A task that a workflow waits on is not polled twice.
* A workflow takes only `completed` as done (`ops::wait_task`). `failed`, `canceled` and
  `skipped` stop the step with the state as the error. `ops::settle` does the same for a
  task group: it is done only when every task completed.
* After a restart or a Connect, every unfinished reference is polled again (re-attached);
  the 50 newest finished ones show, not polled. Dismiss deletes the finished references
  from the file. A task that answers 404 (purged) gets the final state `gone`. A poll that
  gets 401 gives the row the state `unauthorized` (final in memory only: the stored
  reference stays unfinished, so the next Connect re-attaches it) and one error notice. An
  answer for a task that is not tracked (previous server) is dropped.

Task groups (native replication, import, ACS refresh) are one Task Center row each. The
href goes into the task references; `ops::get_task` reads a group as one task
(`TaskGroup::as_task`). A group is final (`TaskGroup::is_final`) when no task is waiting or
running, at least one is done, and one of these is true:

1. all tasks are dispatched;
2. a task failed or was canceled;
3. the replication finalizer completed;
4. the `pulp_import` task completed;
5. the description is an ACS refresh (it dispatches all tasks before it answers);
6. the pulp_rpm `prune_packages` task completed (it dispatches every repository task);
7. its newest task ended 10 minutes ago or more (`finished_at`, client clock): the last
   fallback for a group of an unknown kind, so its Task Center row stops polling.

A group with no task and no count is final too (`completed`): a purge deleted its tasks
(only final ones; `Task.task_group` is `SET_NULL`). Pulp's answer gives a group no time, so
this rule does not wait 10 minutes.

Its state is `failed`, `canceled` or `skipped` when a task ended so, else `completed`.
pulpcore 3.123 leaves `all_tasks_dispatched` false after a replication or an import, and
pulp_rpm 3.39 after a prune (rules 3, 4 and 6 cover them). A workflow does not wait for
rule 7: `ops::wait_task_group` also ends an idle group whose counts stay the same for three
polls. Replication, import and prune jobs report each group snapshot to the Task Center
(`actions::group_progress`), so a restart re-attaches it.

## Pulp objects and pulp-tui objects

| Owner    | Objects                                                                       |
| -------- | ----------------------------------------------------------------------------- |
| Pulp     | Repository, Remote, Repository Version, Publication, Distribution             |
|          | Content Guard, Upload, Task Schedule, Domain, Artifact, Access Policy         |
|          | Task, Task Group, User, Group, Role, Upstream Pulp                            |
|          | Exporter, Export, Importer, Import                                            |
| pulp-tui | Content View, Content View Version, Lifecycle Environment, Promotion, Binding |

pulp-tui uses Pulp objects as documented. Its own objects are not a Pulp API and not
Katello: they live in the store of the profile and are built from Pulp objects.

## Content Views

### Model and names

* A **Content View** lists RPM, File and Deb repositories, each with optional filter
  rules. A **composite** lists other views instead.
* A **version** is immutable. It refers to exact derived repository versions and
  publications. Numbers are never reused.
* **Lifecycle environments** form paths from `Library` (`Library -> Dev -> Test -> Prod`).

| Object             | Name                              | Labels                       |
| ------------------ | --------------------------------- | ---------------------------- |
| Derived repository | `pulp-tui-cv-<view>-<repo>`       | `pulp_tui_cv`, `pulp_tui_db` |
| CV distribution    | `pulp-tui-cv-<view>-<env>-<repo>` | also `pulp_tui_env`          |

* A new distribution serves at `<env label>/<view>/<repo>` (`cv::base_path`). An existing
  one keeps its path (`CvService::served_path`; older servers use `cv/...`).
* Names and `pulp_tui_env` use the environment **name**; only the path uses its **label**.

### Ownership

* Never modify a derived repository or CV distribution without its labels
  (`pulp_tui_cv=<view>`, and `pulp_tui_env=<env>` on a distribution).
* `pulp_tui_db=<database id>` (`Store::db_id`) of another database is refused: publish and
  promote stop, delete keeps the object and logs it.
* The checks also run on hrefs recorded earlier (`CvService::not_ours`): Resume, version
  delete and unbind skip or refuse such objects.
* An object without `pulp_tui_db`, or with an id in the aliases (`meta` key `alias:<id>`),
  is adopted: the workflow sets this database's id when it touches the object.

### Publish

`CvService::publish` records the latest version of every source repository (one point in
time) and the Deb signing choice (`cv_version_repos.signing`: `''` = as source, `none`, or
a signing service href). Then for each repository (`publish_repo`):

1. Find or create the derived repository (labeled, no retention limit). A repository of
   that name without the view's label is refused.
2. Fill it (`snapshot`):
   * No filters (RPM, Deb): one `rpm/copy/` or `deb/copy/` onto `dest_base_version: 0`.
   * Filters (File: always): change the latest derived version only where it differs.
     `modify/` removes; RPM and File add with `modify/`, Deb with `deb/copy/`
     (`structured: true`, `dependency_solving: false`). Lists go in batches below Pulp's
     2.5 MiB body limit.
3. Record the new version (or the latest one when nothing changed). Delete derived versions
   that no Content View version uses (`drop_unused_versions`, best effort).
4. Copy the publish settings of the source (RPM; File `manifest`) or the signing choice
   (Deb). PATCH only fields that differ and that the server has.
5. Publication: RPM and File reuse one of that version, except after a PATCH in step 4 or
   on Resume of a recorded version (a publication keeps the old settings). Deb always
   creates one (a signed and an unsigned one cannot be told apart). RPM keeps the reuse
   also with a metadata signing service: each change of the service is a PATCH in step 4,
   so a new publication follows it, and Pulp lists the newest publication first.

Then the version is stored `published` and the Library step runs.

### Checkpoints and Resume

Every step writes a checkpoint in `cv_version_repos`; *Resume* continues from them.

* A submitted task href goes into `pending_task` **before** the wait (`tracked_task`), so
  Resume re-attaches and never submits it twice. It is the copy task until the version is
  recorded, then the publication task: the write that records the version also clears it.
  *Watch task* (**W**) opens it.
* A failed, canceled or skipped task is cleared, so Resume sends that step again.
* An unfiltered copy that made no version, or whose version is gone, is sent again (a newer
  publish can change or delete versions).
* A recorded task that answers 404 (purged, or of another user) is cleared and the step is
  sent again (`CvService::reattach`). Under the view's lock this is safe.
* A filtered snapshot sends only the batches that the latest version lacks.

### Filters

* **RPM**: name glob, architecture glob, EVR comparison (`rpmvercmp`). An advisory stays
  only when all its packages that exist in the source are selected. Other metadata is
  copied unchanged. An excluded package stays out, also when a module or comps group
  lists it (see COMPATIBILITY.md).
* **Deb**: binary packages by name glob, architecture glob, dpkg version. Source packages
  and upstream indices are dropped; the publication makes new indices.
* **File**: relative path glob.

### Library step

* Publish makes the version active in Library (`bind_library`). Resume does so only for
  the newest published version; Library keeps a newer one.
* A failed Library step is a warning; the version stays published and auto publish runs.
* A stop in this step leaves the promotion `(version, promoting)`; under the view's lock such
  a row is an interrupted one (`CvService::library_interrupted`). Resume of that version
  runs the step again; Resume of another published version refuses.
* Not covered: a stop between `published` and the `promoting` row. Library then serves
  the older version, like after a rollback.

### Promote, rollback, unpromote

* **Promote** binds a version to an environment. The version must be active in the prior
  environment. Each distribution is re-pointed to the version's publication. Bound
  repositories that the version lacks get `publication: null`; the distribution stays.
* **Rollback** promotes an earlier version without the prior-environment check. After
  Library, `promote` accepts only a lower number than the highest bound one (served or
  attempted, `CvService::bound_number`), or that one (a retry or drift repair);
  `cv::rollback_refusal` refuses the rest before any request. In Library any published
  version is accepted. Newer versions are never deleted.
* **Binding** (`store::Binding`): two tables, so the version that Pulp serves is never lost.
  `bindings` holds the served version: only a promotion that completed writes it
  (`Store::finish_promotion` with `active`). `promotions` holds a promotion that has not
  completed: `promoting`, `partial` (some repositories failed) or `failed` (all); a `CHECK`
  refuses any other state. `Store::start_promotion` writes only `promotions`. Pulp has no
  multi-object transaction, so the per-repository rows (`binding_repos`) record what each
  distribution serves. Retry is idempotent.
* A version is in use (`Store::version_in_use`, delete refused) while it is served or
  attempted. The screens show `Dev:v1 (v2 failed)`; verify checks the served version and
  reports the attempt. A rollback to the served version clears the attempt.
* It is also in use while a served row names one of its publications that neither the served
  nor the attempted version has (`Store::partly_served`, `Dev (served in part)`): a `partial`
  promotion that a later attempt replaced, so some repositories still serve it.
* **Unpromote** deletes the view's own distributions in one environment, then the served
  rows, then the binding (last, so a rerun finishes). It is refused while a later
  environment on the path has the view.

### Relabel an environment

`CvService::relabel_env` moves the distributions with a PATCH of `base_path`
(`ops::move_distribution`):

1. Load the impact (`relabel_impact`), then `confirm_danger` with the environment name. The
   impact refuses a label when a distribution that is not one of the environment's rows has
   the label or a path below it as its base path (`refuse_label_overlap`; Pulp refuses
   overlapping base paths, COMPATIBILITY.md).
2. Lock every view with rows in the environment, in `cv_id` order.
3. Load the impact again (the same refusal), then store the new label (the checkpoint).
4. Per row: check the labels and database id, PATCH, wait, save the row.

A failed row keeps its old path; a rerun with the same label moves the rest (a row whose
PATCH finished before a crash is saved without a second PATCH). A row of a view bound after
the locks is reported, not touched. A promotion that read the old label stores `partial`; a
rerun of the relabel moves its row.

### Verify and drift

Verify checks for every binding that the distribution serves the expected publication, the
publication belongs to the snapshot, the snapshot exists, and a served sample matches (not every
file; the OK line of the drift check says so):

| Type | Served check                                                                                  |
| ---- | --------------------------------------------------------------------------------------------- |
| File | the manifest that the publication names (default `PULP_MANIFEST`)                             |
| RPM  | `repomd.xml`, the smallest package with digest                                                |
| Deb  | `dists/<dist>/Release` (verbatim flat: `Release`), the smallest package of `Packages`, digest |

It also reports retention on a derived repository. `CvService::drift` gives each finding the
kind of fix (`cv::Fix`), and the drift check names one action per kind: a new promotion of
the bound version re-points distributions; a gone or wrong snapshot or publication needs a
new version; served files that differ need a repair; a missing `base_url` needs
`CONTENT_ORIGIN`; retention must be cleared outside pulp-tui.

* A promotion re-points also a repository that the version lacks when Pulp shows that its
  distribution serves a publication (`unbind_removed` reads the distribution of a row that
  records none), so the "promote again" hint fixes that drift too.
* While a workflow holds a live lock on the view, the hint names that workflow: the lock
  refuses Promote and Rollback.
* `Fix::Unchecked` is not drift: a File publication without a manifest gives nothing to
  compare. The drift check and the relabel result say `OK` with that note.

### Delete

Never remove a served object: deleting a repository version deletes its publications, and
deleting a publication silently stops its distributions (COMPATIBILITY.md).

* **Version**: keeps objects that another live version uses and every publication that a
  binding row of the scope serves (`Store::served_publications`). Refused while a composite
  version uses it or a composite pins it.
* Before that check the version becomes `deleting` (`Store::set_version_state`,
  conditional). A later composite publish refuses it; an earlier one is found by the check
  (PostgreSQL: `FOR SHARE` in `Store::begin`). A refused delete restores the state; a
  failed one stays `deleting` until *Delete* again.
* **View** (`CvService::delete_view`): refused while a composite uses it, or another view
  serves a publication of one of its derived repositories.

### Composites

* Components (`cv_components`): a view and a pinned version, or `latest`. No nesting.
* Publish (`CvService::publish_composite`):
  1. Resolve each component to its pin or newest published version; refuse one without.
  2. Refuse a repository clash (`check_repo_clash`) before any write.
  3. `Store::begin_composite_version` records the component versions and copies their rows
     as **borrowed** rows (same Pulp objects, no task reference). It refuses a component
     version that is no longer published.
  4. `resume` runs the normal path. It skips rows with a publication and binds Library
     with the composite's own distributions.
* Nothing is copied or published in Pulp. Delete never removes a borrowed object.
* The composite takes only its own lock and never changes its components.
* **Auto publish** (`CvService::auto_publish`) runs after a component publish (or Resume
  of its newest version), outside the component's lock. It locks and publishes each
  composite that follows the component as `latest` and lags it: the newest published
  composite version does not use the component's newest published version
  (`CvService::lagging_composites`). One result per composite. It never promotes beyond
  Library and does nothing while Library is protected.
* Nothing records a pending auto publish: the lag is the record. When the client stops
  between the component publish and the auto publish, Resume of the component's newest
  version (also when it is published) publishes the composites that lag it.

## Replication

* **Native** (`upstream-pulps/`, pulpcore ≥ 3.23, tech preview): pulp-tui manages the
  Upstream Pulp, starts `replicate/` and follows the task group. Only an all-completed group
  is a success; the result title names any other outcome (`FAILED`, `CANCELED`, `SKIPPED`).
  Pulp owns the deletion policy; the form puts
  `nodelete` first. Every other policy may delete local objects, so setting it (create, or
  an edit that changes the policy) or replicating with it needs typed confirmation.
* **Fallback** (`src/replication.rs`): content sync only, RPM, File and Deb. Per upstream
  distribution it creates or updates a remote (the served URL), a repository and a
  pinned distribution with the same name and path, labeled
  `pulp_tui_replica=<source profile>`.
  * A dry run shows CREATE / SYNC / SKIP. The run checks the label again.
  * Objects without the label are conflicts and are skipped.
  * A File item without a readable served manifest is skipped (no URL guess).
  * A checkpoint distribution: the remote URL is its newest checkpoint publication at
    `<base>/<timestamp>/` (`ops::served_url`); the bare base path only lists the timestamps.
  * Deb: the remote's `distributions` are the `dists/` directories of the served
    publication (`deb::layout`, as verify probes them); for a verbatim one, the synced Release
    files (`/` for a flat repository at the root). The replica publishes in the mode of
    that publication (`ops::publication_mode`, verbatim too), without its signing service:
    that is an href of the upstream server. The dry run notes an upstream APT publication
    that is signed (its service, else its repository's service or release overrides) when
    the replica repository has no signing service. It reads only the API: the upstream
    `base_url` is for the destination server and may not resolve on the client.
  * It never deletes and does not copy content guards, labels or metadata.
  * It reuses a publication of the synced version (`ops::find_publication`), also for an
    RPM repository with a metadata signing service (a new one on every run would pile up).
    If the reused one is unsigned, the verify after it fails on `repomd.xml.asc`.
* Guards: source and destination differ; a distribution served from the destination's
  content origin is a loop and is skipped.

## Export and import

* **Pulp export and import**: Pulp writes and reads the files on its own disk. Export
  waits for the task and shows the TOC. Before an importer create or edit, the job finds
  every mapped destination and sends nothing when one is missing or is a Content View
  snapshot (`pulp_tui_cv`). *Import* reads the importer's mapping again, refuses a snapshot
  destination, and runs `ops::import_check` with it, then `ops::pulp_import` after
  `confirm_prod`, and follows the
  task group like replication. Deletes of exporters and importers load the impact and need
  the typed name.
* **Limit**: Pulp imports an unmapped repository into the local repository of the same name,
  and tells which repositories an export has only in the import task (`import-check` and the
  TOC do not). So pulp-tui cannot refuse a same-name snapshot. Instead the confirmation names
  every local snapshot whose name is not a mapping source, and then needs the typed importer
  name (`confirm_danger`).
* **Content View definitions** (`export-cv`, `import-cv`; format 2, reads format 1; an
  older pulp-tui refuses format 2): views, environments and labels, composites, pins as
  numbers. No secrets, no Pulp state. Versions are not exported, so import turns pins into
  `latest` with a warning. Import checks every entry before its first write, then writes in
  one transaction (`Store::import`); a view or environment that exists keeps its definition
  and gives a warning, so a rerun changes nothing. An export holds
  the repository hrefs of its own server, so `cv::import` first finds each repository of a view
  that it creates on the target server by name and type and stores that href. When one is
  missing, it writes nothing and names every missing repository in one error. `cv::export`
  writes the current Pulp name of each repository href (a view records the name when the
  repository is added; the store keeps it, because derived object names come from it). A
  repository that is gone (404) fails the export, again with every one named.
* **Repository and remote definitions** (`src/repo_export.rs`): the export keeps the
  fields of the live request schema. It leaves out readable secrets (`sles_auth_token`,
  `headers`, credentials in URLs), hrefs and objects with ownership labels. ULN and Git
  remotes (`RepoType::alt_remote`) go in `alt_remotes`, which an older pulp-tui ignores. The
  import creates what is missing and never changes an existing object, so a rerun is a no-op.
  A create that Pulp refuses does not stop the others (a repository of a refused remote
  waits); the error then names every object not created, and a rerun creates only those.
  It does not create an object that lacks a field of the schema's `required` list (a ULN
  remote's `username` and `password`); a repository of such a remote waits too.
  `repo_export::plan` is the dry run: it lists such a waiting repository as a note.

## Storage

`Store` (`src/store.rs`) uses the sqlx `Any` driver: one implementation for SQLite and
PostgreSQL, so SQL must be portable. Store calls are `async` and run in jobs.

| Store                                 | Opened               | Scope        |
| ------------------------------------- | -------------------- | ------------ |
| Local `<data_dir>/pulp-tui.db`        | always; migrates     | profile name |
| External (PostgreSQL, or SQLite path) | on Connect, in a job | server key   |

* The local file holds task references and replication runs, and the Content View data
  of profiles without an external database.
* The external database holds Content View data: views, filters, versions, checkpoints,
  environments, bindings, audit log, locks.
* `Config::database_for(profile)`: the profile's `database`, else `[database]`, else the
  local file. A database with `url = "local"` (the marker) is the local file, also when
  `[database]` is set. The keyring user is
  `[database]` or `[database]:<profile>`.
* `App.stores` caches one store per connect URL (the key holds the password: never show
  it). `App.cv_store` is the CV store of the connected profile, opened by `App::with_store`.
  While it opens, `cv_store` holds an `opening` error, never the previous profile's store;
  a result for a profile that is no longer active is dropped.
* A failed external open puts the error in `cv_store`. Content View screens then report
  it; nothing falls back to the local file (the state would split).
* The server key is `<origin><api_root>#<domain>` (`Profile::server_key`), from the URL as
  typed. All operators must type the same scheme, host, port, API root and domain. The
  Dashboard shows the key.

### Schema and migrations

* `MIGRATIONS` (SQLite, `PRAGMA user_version`) and `PG_MIGRATIONS` (PostgreSQL,
  `schema_version`) have the same length (a unit test checks it). Append to both; never
  edit a batch. A batch and its number apply in one transaction.
* PostgreSQL uses `BIGINT` for every integer, because sqlx `Any` reads `i64`.
* The local file migrates on open. The external database changes only with `pulp-tui db
  migrate` or the TUI confirm (`Store::migrate_external`, with `pg_advisory_lock` on
  PostgreSQL). `Store::open_external` changes nothing: it refuses an older schema
  (`store::NotMigrated`) or a newer one.
* Operators need only `SELECT/INSERT/UPDATE/DELETE`.

| Schema | Adds                                                                  |
| ------ | --------------------------------------------------------------------- |
| 2      | `meta` (`db_id`, aliases), `locks`, `audit.actor`, `scope_settings`   |
| 3      | `environments.label` (set to `lower(name)`, so no served path moves)  |
| 4      | composites, `cv_components`, `cv_version_components`, `borrowed` rows |
| 5      | `cv_version_repos.signing` (also stops an older pulp-tui on Deb rows) |
| 6      | `binding_repos.env_id` foreign key (drops orphan rows)                |
| 7      | `promotions`; `bindings` keeps only served (active) rows, no `state`  |
| 8      | `cv_versions.token` (lock token of the publish that runs it)          |
| 9      | served version of a row that 7 left with an attempt only (see below)  |

Migration 9: an environment with an attempt and no served version gets back the one
published version, other than the attempt, whose publications are exactly those that its
served rows (`binding_repos`) record. A mixed set (a partial promotion) gives none; the
versions it serves in part stay in use (`Store::partly_served`).

`cv_components` has no cascade, so the database refuses to delete a view or version that a
composite uses. `bindings.env_id`, `promotions.env_id` and `binding_repos.env_id` have no cascade either: the
database refuses to delete an environment that still has them. SQLite cannot add a constraint,
so migration 6 rebuilds `binding_repos`. Every SQLite connection sets `PRAGMA foreign_keys=ON`.

### Connection

* `Database::connect_url` (`src/config.rs`) adds the password from the secret reference
  and forces `sslmode=verify-full`, except for a Unix socket or loopback host.
* rustls with built-in roots; `ca_cert` adds to them. The OS store is not read.
* The first connection has a 10 s limit. Errors use `Database::display_url` (no password).

### Locks

The operator is `username@host` (`store::actor`), stored in `audit.actor` and in locks.
`locks` has one row per Content View (`owner`, `token`, `operation`, `renewed_at` from the
client clock).

* `Store::try_lock` is one atomic statement: insert, or take over a row older than
  `LOCK_TTL_SECS` (120 s). `CvLock::acquire` never waits.
* `CvLock` renews every `LOCK_RENEW_SECS` (30 s), by token. A renewal that matches no row,
  fails, or takes over 30 s marks the lock lost.
* `CvService::alive()` runs `CvLock::check` before every Pulp mutation and store write,
  with no database round trip. It fails when the lock is lost, or when the last renewal is
  older than 90 s (TTL minus renew; Tokio timers stop while a computer sleeps).
* A workflow writes its lock token (`CvLock::token`) on the rows it starts: the version that
  it publishes (`Store::start_publishing`, `cv_versions.token`) and its promotion
  (`Store::start_promotion`, `promotions.token`). An operator who takes over and resumes or
  promotes the same version writes their own token.
* After a lost lock, only three writes still happen, each only while the row still has the
  workflow's token: the href of a just-submitted task (`Store::save_own_version_repo`, also
  only while `publishing`), the `failed` state of the publish
  (`Store::fail_publishing_version`, only while `publishing`), and the promotion result
  (`Store::finish_promotion`). So they never change a row of an operator who took over.
* *Break lock* (`Store::break_lock`) deletes the row only if the holder is still the one
  shown. Drop releases by token; a crash leaves the row until it expires.
* Locked: publish, resume, promote, rollback, unpromote, delete. Not locked: preview,
  compare, verify, edits. A relabel locks all views of the environment. A composite publish
  locks only the composite.
* `Store::delete_env` locks the environment row first (SQLite: the write lock; PostgreSQL:
  `FOR UPDATE`), then checks for bindings and later environments, and deletes in the same
  transaction. A promotion into it or a new successor waits for that commit.

### Read-only mode and `db copy`

* With an external store (`App::shared()`, which follows `cv_store`), Content View and
  environment edits and `import-cv` count as mutating (`Act::mutates(kind, shared)`).
  `MigrateDb` and `CopyDb` always mutate.
* `pulp-tui db copy` and "copy and switch" use `Store::copy_from`: one target transaction,
  new ids, remapped references, all CV data, under the key of the running profile. It
  refuses name and label clashes, the same database (equal `db_id`), and a view with a live
  lock. The source id becomes an alias in the target. The source is only read.
* When Edit changes the database of a profile with data, the TUI offers copy, switch or
  cancel, and saves the profile only after the copy worked.

### Limits

* No daemon: workflows run only while a TUI runs. Pulp tasks continue; the next client
  steps wait for a *Resume* or promotion by any operator.
* Lock times use client clocks; the TTL covers normal skew only.
* Other operators' changes show after a refresh (no push).
* A shared SQLite file needs one host and a group-writable directory; WAL does not work on
  NFS or SMB.
* In a container the host part of the operator is random unless `--hostname` is set.
* Two different databases on one Pulp server do not coordinate; labels stop a silent
  takeover but do not lock.

## Security model

* TLS is always verified; a custom CA adds to the roots. No option disables it or falls
  back to HTTP. Remote PostgreSQL always uses `verify-full`; its URL must not contain a
  password.
* Secrets are references (`env:VAR`, `file:/path`, `keyring`). The parser rejects
  unknown keys such as `password`, a `url` with user info, and a `proxy` with a password.
  A secret file that group or others can read gives a warning.
* `Authorization` is marked sensitive and never recorded. The log file holds method, path
  and status only.
* Forms never pre-fill write-only fields and mask them. Inspect, Details and Logs mask the
  same keys (a remote's `headers` and tokens, and a header guard's `header_value`, are
  readable on GET). A domain's storage settings are one masked field in forms; Inspect,
  Details and Logs mask the S3 and Azure credentials in them (Azure `connection_string`,
  `token_credential` and `sas_token` are readable on GET). The key list is
  `session_log::SECRET_KEYS`; the repository export uses it too.
* The Logs screen (`src/session_log.rs`) keeps the last 1000 entries in memory, bodies cut
  at 64 KiB, secret keys redacted (any case), upload bodies replaced by their size. It is
  never written to disk.
* Read-only profiles block mutating requests in the client; the UI hides the actions.
* Pulp enforces RBAC. pulp-tui warns on owner/admin grants (also object roles), `is_staff`
  and changes to one's own access, refuses to delete the logged-in account, and never works around a 403.
* Destructive operations show the impact first. Deletes of repositories, distributions,
  users, roles, content guards, domains, Content Views, ACSs and protected promotions, reclaim
  space, a domain storage migration, a server repair, an access policy reset and a
  distribution edit that changes its binding or base path need the typed name. Production
  profiles show a red banner and a warning line.
