# Architecture

pulp-tui is one Rust binary. It talks to Pulp 3 only through the documented REST API.
It never reads or writes the Pulp database and it never runs `pulp`, `curl` or other
tools for normal operation.

## Layers

| Layer | Module | Responsibility |
|---|---|---|
| Transport | `src/pulp/client.rs` | Auth (basic, bearer, mTLS), TLS (always verified, custom CA bundles), proxies, timeouts, bounded concurrency (semaphore), retries for reads only, pagination, response size limit, error mapping, href confinement to the configured API root, recording of each request attempt in the in-memory session log |
| Capabilities | `src/pulp/caps.rs` | `status/` (component versions, health, content origin), `login/` or `users/?username=` (current user), live OpenAPI schema (endpoints, fields, enums) |
| Typed adapters | `src/pulp/models.rs`, `src/pulp/ops.rs`, `src/pulp/rpm.rs`, `src/pulp/file.rs`, `src/pulp/deb.rs` | RPM/File/Deb endpoints, task and task-group waiting, find-or-create helpers, publications, distributions, uploads, content listing, diffs, served-content verification |
| Services | `src/cv.rs`, `src/replication.rs`, `src/filters.rs` | Content Views (publish, promote, rollback, verify, cleanup), replication (native and fallback), filter rules, `rpmvercmp` and the dpkg version comparison |
| Storage | `src/store.rs` | sqlx (SQLite, optionally PostgreSQL): task references, Content View definitions, versions, checkpoints, environments, bindings, audit log with operator, locks, replication runs |
| TUI | `src/tui/*` | Event loop, state, screens, forms, confirmations, Task Center |

### Plugins

REST operations of one Pulp plugin go in `src/pulp/<plugin>.rs` (`rpm.rs`, `file.rs`, `deb.rs`).
`RepoType` in `models.rs` holds the per-type facts: component name, label, endpoints,
schema names and required `Feature`. Every dispatch on `RepoType` is an exhaustive
`match`, so a new type is a compile error in each place that must handle it. There is no
`== RepoType::X`, no `_ =>` arm and no `unwrap_or(RepoType::X)` on a type.

### Event model

The TUI loop (`src/tui/mod.rs`) waits on three sources: key events, messages from
background jobs, and a 250 ms tick. A job is a Tokio task. It returns a closure that
changes `App` state on the UI thread. Rendering only reads state, so drawing never waits
on network or disk. Each list load carries a generation number; a stale response is
dropped. Esc aborts the client-side job (the HTTP request); it never cancels a Pulp task.

### Requests and retries

* At most `max_concurrent_requests` HTTP requests run per server.
* GET requests are retried (network error, timeout, 429, 502, 503, 504) with exponential
  backoff and jitter. `Retry-After` is honored.
* POST, PATCH, PUT and DELETE are never retried automatically. Workflows make retries
  safe on their own level: find-or-create by name, publication reuse, and checkpoints that
  store submitted task hrefs.
* Responses larger than `max_response_mb` are rejected.
* The client records each attempt (method, URL, status, duration, redacted bodies) in the
  in-memory session log, so a retried read shows as several rows.
* Hrefs from the server (including `next` links) must stay on the same origin and below
  the configured API root.

### Task tracking

Pulp tasks are the source of truth. The Task Center stores task references in the local file and
polls each unfinished task on its own schedule: the interval starts at `poll_min_ms`,
grows by 1.5x while nothing changes, resets on progress, and has ±20 % jitter. At most
three polls run at the same time. A task that a workflow already waits on is not polled
twice. After a restart, unfinished references are polled again ("re-attached"). A task that
the server answers with 404 (purged) gets the final state `gone`. A poll answer for a task
that is not tracked (sent to the previous server before a Connect) is dropped.

Task groups (native replication) are final when no task is waiting or running and the
group says all tasks were dispatched, a task failed, or the replication finalizer
completed. pulpcore 3.123 leaves `all_tasks_dispatched` false after a replication, so an
idle group whose counts stay the same for three polls is also treated as final.

## Native Pulp concepts versus application-managed concepts

| Concept | Owner | Notes |
|---|---|---|
| Repository, Remote, Repository Version, Publication, Distribution, Task, Task Group, User, Group, Role, Upstream Pulp | **Pulp** | Used as documented. The Repository and the Remote are separate objects. |
| Content View, Content View Version, Lifecycle Environment, Promotion, Binding | **pulp-tui** | Not a Pulp API and not Katello. Stored in the local SQLite file, or in the external database of the profile (its own `database`, else `[database]`; see "Storage"), and built from the Pulp objects above. |

### Content View implementation

* A **Content View** is a named list of RPM, File and Deb repositories. Each repository has
  optional filter rules (`src/filters.rs`). A composite view lists other Content Views
  instead (see "Composite Content Views" below).
* **Publish** first records the latest version of every source repository, so the
  snapshot is consistent in time, together with the Deb signing choice of the publish
  (`cv_version_repos.signing`: `''` = same as source, `none`, or a signing service href).
  Then for each repository:
  1. find or create the derived repository `pulp-tui-cv-<view>-<repo>`, labeled
     `pulp_tui_cv=<view>`, with no retention limit;
  2. copy the selected content into it from an empty base: RPM uses `rpm/copy/` with
     `dest_base_version: 0` (all content, or an explicit list when filters exist); File
     uses `modify` with `remove_content_units: ["*"]`; Deb uses `deb/copy/` like RPM, with
     `structured: true` (the release, component and architecture records of the copied
     packages come with them) and `dependency_solving: false`;
  3. record the new repository version (or the latest one if the content did not change);
  4. give the derived repository the publish settings of the source (RPM) or the signing
     choice (Deb: `signing_service` and `signing_service_release_overrides`, only the fields
     that differ, and only fields the server has);
  5. RPM and File: reuse or create the publication of that version. Deb: always create one,
     because a publication signed with repository keys stores `signing_service: null` and
     cannot be told apart from an unsigned one.
  Every step writes a checkpoint. The href of a submitted copy task is stored before
  waiting, so an interrupted publish **re-attaches to the same task** and does not submit
  it again. A copy task that failed or was canceled is cleared from the checkpoint, so the
  next *Resume* submits the copy again (it starts from an empty base). The Deb publication
  task uses the same checkpoint column (`pending_task`): with no derived version recorded it
  is the copy task, with one it is the publication task (the copy step clears it in the same
  write that records the version).
* RPM filters select packages by name glob, architecture glob and EVR comparison
  (`rpmvercmp` port, tested with rpm's own cases). An advisory is kept only when every
  package it references that exists in the source is selected. Other metadata (comps,
  modules, distribution trees, repo metadata files) is copied unchanged. Module artifact
  packages are copied with their module even when a filter excludes them, because
  `rpm/copy/` adds module children.
* Deb filters select binary packages by name glob, architecture glob and dpkg version
  comparison. A filtered snapshot has only the selected packages and the release records
  that the structured copy adds; source packages and upstream index files are dropped (the
  publication makes new indices). One copy body holds every selected href, and Pulp refuses
  a body above 2.5 MiB (see COMPATIBILITY.md); batching is not implemented.
* A **Content View Version** is immutable. It refers to exact derived repository versions
  and publications. Version numbers are never reused.
* **Lifecycle environments** form paths from `Library`, for example
  `Library -> Dev -> Test -> Prod`. Publishing makes the new version active in Library.
* **Promote** binds a version to an environment. The version must be active in the prior
  environment. Each (view, environment, repository) has a stable distribution
  `pulp-tui-cv-<view>-<env>-<repo>` at `<env label>/<view>/<repo>` (`cv::base_path`). An
  existing distribution keeps its path (`CvService::served_path`): older servers serve at
  `cv/...`, and an operator may move a distribution by hand. The distribution name and the
  ownership label `pulp_tui_env` keep the environment **name**; only the served path uses
  the environment **label**. Promotion re-points the distribution to the version's
  publication. Repositories that are bound but not part of the version stop serving
  (`publication: null`); the distribution stays for a later rollback.
* **Relabel** (`CvService::relabel_env`, Edit on Lifecycle Environments) changes the label
  of an environment and moves its distributions with a `PATCH` of `base_path`
  (`ops::move_distribution`: Pulp moves the distribution in place, the task is awaited and
  the old path stops serving). Steps: load the impact (`relabel_impact`: served rows whose
  path changes); `confirm_danger` typed with the environment name; take the locks of every
  view with a binding or served rows in the environment, in `cv_id` order; store the new
  label (the checkpoint); then for each row check the ownership
  labels and the database ID, `PATCH`, wait, and save the row. A row that fails keeps its old
  path and is reported; a rerun with the same label continues with the rows that are left
  (a row whose `PATCH` finished before the crash is saved without a second `PATCH`). A row
  of a view bound after the locks were taken is reported as failed and not touched. The
  audit entry is `env.relabel`.
* **Rollback** is a promotion of any earlier published version. Newer versions are never
  deleted.
* **Verify / drift**: for every binding, check that the distribution serves the expected
  publication, that the publication belongs to the expected snapshot version, that the
  snapshot still exists, and that served content matches (`PULP_MANIFEST` equal to the
  snapshot for File; `repomd.xml` plus the smallest package with digest check for RPM;
  `dists/<distribution>/Release` plus the smallest package, found in a `Packages` index,
  with digest check for Deb).
  Retention set on a derived repository is reported. Drift is reconciled by promoting the
  bound version again (idempotent).
* **Ownership**: pulp-tui never modifies a derived repository or CV distribution that does
  not carry its ownership labels. Deleting a CV version keeps Pulp objects that another
  live version of the view uses, and every publication (and with it its repository
  version) that any binding row of the server scope still serves, also the binding of a
  composite (`Store::served_publications`). Pulp deletes the publications of a repository
  version with it, and deleting a publication silently stops the distributions that serve
  it (COMPATIBILITY.md), so a served object is never removed. Deleting a view
  (`CvService::delete_view`) is refused while a composite lists it as a component or a
  non-deleted composite version used one of its versions, and when another view still
  serves a publication of one of its derived repositories, because deleting the
  repository would delete that publication. Deleting a version is refused while a
  composite version uses it or a composite pins it. Before that check the version is set to
  `deleting` (`Store::set_version_state`, conditional on its old state), because a composite
  publish takes the composite's lock only: a composite publish that starts later refuses a
  version that is not `published`, and one that started earlier is found by the check (on
  PostgreSQL `Store::begin` holds the row `FOR SHARE`, so the mark waits for its commit). A
  refused delete restores the old state; a delete that fails later stays `deleting`, and
  *Delete* again finishes it (*Resume* refuses it).
* **Composite Content Views**: a composite (`content_views.composite`) has components
  (`cv_components`: the component view and a pinned version, or none for `latest`) instead
  of repositories and filters. No nesting: `set_component` refuses a composite as a
  component. Publish (`CvService::publish_composite`) resolves each component to its pin
  or its newest published version, refuses one without a published version, and checks the
  repositories of all resolved versions (`check_repo_clash`: one repository in two
  components, or two names with one `slug` segment) before any write. Nothing is copied or
  published in Pulp. `Store::begin_composite_version` takes the component versions
  (`cv_version_components`) and copies their `cv_version_repos` rows as **borrowed** rows
  (`borrowed = 1`, same derived repository, version and publication, without the task
  reference) into the new version. It refuses a component version that is no longer
  published. Then `resume` runs the normal path: `publish_repo` returns at once for a row
  that has a publication, and `promote` binds Library with the composite's own
  distributions `pulp-tui-cv-<composite>-<env>-<repo>`, labeled `pulp_tui_cv=<composite>`,
  at `<label>/<composite>/<repo>`. One publication can be served by the
  distribution of the component and the one of the composite (verified, COMPATIBILITY.md).
  Delete never removes a borrowed object: `delete_version` skips borrowed rows, and
  `delete_view` of a composite removes its distributions only. The composite takes its own
  lock only; it reads the component versions and never changes them. `auto_publish`
  (`CvService::auto_publish`) runs after a component publish or resume, outside the lock of
  the component: for every composite with the flag that lists the component as `latest`
  (`Store::auto_composites`) it takes the composite's lock and publishes, and returns one
  result per composite, so a locked or failing composite does not undo the component's
  publish. It never promotes beyond Library. The compare and preview of composites list
  the component versions first (`cv::component_lines`).
* **Database ID label**: derived repositories and CV distributions also carry
  `pulp_tui_db=<database id>` (`Store::db_id`). An object that carries the id of another
  database is refused: publish and promote stop with "... is owned by another pulp-tui
  database (<id>)", and deleting a Content View keeps such objects (it logs them). The check
  also runs on hrefs recorded earlier: Resume stops before copying into a derived repository
  that another database adopted, deleting a version keeps the publication and snapshot of
  such a repository, and a promotion does not unbind such a distribution. An object
  without the label, or with an id in this database's aliases (`meta` keys `alias:<id>`),
  is adopted: the label is set to this database's id when the workflow touches it.

### No cross-resource transactions

Pulp has no multi-object transaction, so pulp-tui does not promise one. A promotion that
fails for some repositories is stored as `partial` (all failed: `failed`) together with
per-repository binding records. The operator can retry (idempotent) or roll back. A
failed publish keeps its checkpoints; *Resume* continues from them.

## Replication

* **Native** (`upstream-pulps/`, pulpcore ≥ 3.23, tech preview): pulp-tui manages the
  Upstream Pulp object, starts `replicate/`, follows the task group and lists task results.
  The deletion policy belongs to Pulp. The form puts `nodelete` first and requires typed
  confirmation for `all`.
* **Fallback** (`src/replication.rs`): content synchronization only. For each selected
  upstream RPM/File distribution it creates or updates a remote (pointing at the published
  upstream URL), a repository and a pinned distribution with the same name and base path,
  labeled `pulp_tui_replica=<source profile>`. A dry run shows CREATE / SYNC / SKIP per
  item. Objects without this label are conflicts and are skipped. The run checks the label
  of an existing remote and repository again, so an object created after the dry run is
  refused, not changed. The fallback never deletes anything and does not copy content
  guards, labels or other Pulp metadata.
* Guards: source and destination must be different servers; upstream distributions whose
  content URL is on the destination's content origin are skipped as loops.

## Storage

`Store` (`src/store.rs`) uses sqlx with its `Any` driver, so one implementation of each
method serves SQLite and PostgreSQL. All store calls are `async`; the UI thread never
calls them (they run in jobs). pulp-tui holds the local file and one store for each
external database in use:

| Store | Opened | Holds | Rows scoped by |
|---|---|---|---|
| Local file `<data_dir>/pulp-tui.db` (SQLite, WAL) | always, migrated on open | task references, replication runs; also the Content View data of every profile without an external database | profile name |
| External database (PostgreSQL, or SQLite at a path): the `database` of a profile, else `[database]` | on Connect (a job), never migrated by an open | Content Views (also composites and their components), repositories and filters, versions and checkpoints, environments, bindings, audit log, locks | server key |

`Config::database_for(profile)` decides which database a profile uses: its own `database`,
else `[database]`, else the local file (`None`). A profile with `url = "local"` also gets
`None`, even when `[database]` is set. It returns the keyring user of the secret too:
`[database]`, or `[database]:<profile>` for a profile's own database (`validate_name`
rejects such a profile name). The store of a database is cached in `App.stores`, keyed by
the connect URL (which holds the password: the key is never shown or logged), so profiles
that use one database share one `Arc<Store>`. `App::open_store(db)` returns the cached
store, or opens and caches it; `None` gives the local store, and a failed open is not cached.

`App.store` is the local file. `App.cv_store` is the store of Content View data of the
connected profile. `App::connect` opens it in a job (`App::with_store`) and sets it when the
job ends; a result for a profile that is no longer the active one is dropped, and while the
job runs `cv_store` holds an `opening` error, never the store of the previous profile. For
a profile without an external database it is the local store. If the external database
cannot be opened, `cv_store` holds the error, Content View screens and actions report
`shared database unavailable: ...`, and nothing falls back to the local file (the state
would split). The next Connect opens it again.

`Store::open_external` fails with `store::NotMigrated` (`from` 0 for an empty database or a
SQLite file that does not exist, else the older version) instead of changing anything.
`with_store` turns it into the message `<display>: database not migrated (open Servers,
Connect again to migrate, or run pulp-tui db migrate --profile <name>)` for `cv_store`. It
also shows the confirm `Migrate database of <name>?`; after Enter it runs
`Store::migrate_external` in a job and opens the store. A read-only profile gets a warning
instead of the confirm, because migrate is a mutating action (`Act::MigrateDb`). The
profile form does the same check after a save that changed the database.

**Server key scope.** In the external database the scope value is the server key
`<origin><api_root>#<domain>` (`Profile::server_key`, applied by `Store::scope`), so two
operators with different profile names for one server share rows. In the local file the
scope stays the profile name, so existing data stays valid. The key comes from the URL as
the operator typed it (`url::Url` only makes the host lower case and removes a default
port). So every operator must use the same scheme, host, port, `api_root` and domain for a
server; a short host name or an IP address gives another key and other rows. `db copy`
writes under the key of the profile that runs it. The Dashboard shows the key
(`shared database scope: <key>`), so operators can compare it.

**Schema and upgrades.** `MIGRATIONS` (SQLite, `PRAGMA user_version`) and `PG_MIGRATIONS`
(PostgreSQL, a `schema_version` table) are lists of numbered SQL batches of the same length
(a unit test checks this). PostgreSQL uses `BIGINT` for every integer and identity
columns, because sqlx `Any` reads integers as `i64`. A batch and its version number are
applied in one transaction. The local file migrates on open. The external database changes
only with `pulp-tui db migrate` or the confirm in the TUI. Both call
`Store::migrate_external`, which also creates the schema and, on PostgreSQL, holds a
`pg_advisory_lock` so two administrators cannot migrate at the same time. A normal open
(`Store::open_external`) compares versions and refuses an uninitialized, older or newer
schema with a message. Operators need only
`SELECT/INSERT/UPDATE/DELETE`; the database ID is created with an `INSERT`.
Schema 2 added the `meta` table (database ID `db_id` and aliases `alias:<id>`),
`scope_settings` (the base path prefix per server; no longer read), `locks` and
`audit.actor`.
Schema 3 added `environments.label`, the path segment of an environment (unique per
scope; `validate_label`). The migration sets it to `lower(name)`, which is exactly the
segment that served paths used before (so `Dev` and `dev` share `dev` and nothing moves),
and seeds `scope_settings` with the first segment of `binding_repos.base_path` (usually
`cv`; no longer read). Export (format 2) and `db copy` carry the labels;
`db copy` refuses a label that another environment uses in the target scope. An external
database at schema 2 needs `db migrate` after the upgrade.
Schema 4 added composite Content Views: `content_views.composite` and `auto_publish`
(flags, `BIGINT` on PostgreSQL), `cv_version_repos.borrowed`, `cv_components` (composite,
component, optional pinned version; `component_id` and the pinned `version_id` have no
cascade, so the database itself refuses to delete a view or version that a composite still
uses) and `cv_version_components` (the component versions that a composite version used).
Export format 2 carries composites, components by view name, the auto publish flag and a
pin as a version number. Versions are not exported, so import turns every pin into `latest`
and returns a warning that names it (the CLI prints it as `warning: ...`, the TUI shows one
warning notice); a missing component, or a composite used as component, is skipped with a
warning. Import checks every name, label and filter rule before its first write, so a bad
entry leaves nothing behind. Import still reads format 1; an older pulp-tui refuses format 2 (its check is
`format != 1`). `db copy` copies composites, components with pins, the recorded component
versions and the borrowed rows, with every id remapped. The links between composites,
components and versions are written after all views and versions, so a composite that
sorts before its components (`rocky9` < `rocky9-os`) is copied correctly. An external
database at schema 3 needs `db migrate` after the upgrade.
Schema 5 added `cv_version_repos.signing`, the Deb signing choice of a publish (`''` = same
as source, `none`, or a signing service href; rows of earlier versions read `''`). It is a
checkpoint like the source version, so a resumed publish signs as asked; composite rows and
`db copy` carry it. The schema number also stops an older pulp-tui on a shared database,
which would otherwise fail on a `deb` repository row and drop its `version` filter rules.
An external database at schema 4 needs `db migrate` after the upgrade.

**Connection.** `Database::connect_url` (`src/config.rs`) builds the URL: it adds the
password from the secret reference and forces `sslmode=verify-full` for every PostgreSQL
host except a Unix socket or a loopback address. sqlx uses rustls with the root
certificates built into the binary; `ca_cert` (`sslrootcert`) is added to them and the
operating system store is not read (see COMPATIBILITY.md). A first plain connection attempt
has a 10 s limit, so an unreachable host fails with a clear error. Error text uses the URL
without the password (`Database::display_url`).

**Operator and audit.** The operator is `username@host` (`store::actor`). It goes into
the `audit.actor` column and into locks.

**Locks.** One row per Content View in `locks` (`owner`, a random `token`, `operation`,
`renewed_at` in client Unix seconds). `Store::try_lock` is one statement: `INSERT ... ON
CONFLICT(cv_id) DO UPDATE ... WHERE locks.renewed_at < now - 120`. It wins when there is
no row or the row has expired (TTL `LOCK_TTL_SECS` = 120 s), so the takeover is atomic on
both backends. `CvLock` (`src/cv.rs`) holds the lock for one workflow and renews it every
`LOCK_RENEW_SECS` = 30 s in a background task. `Store::renew_lock` updates only the row
with its own token: if it matches nothing (another operator took over after expiry, or
*Break lock* removed it), the database is unreachable, or the round takes longer than
30 s (`database unreachable: renewal timed out`), the task sets a flag and stops.
`CvService::alive()` calls `CvLock::check` before every Pulp mutation and before every
checkpoint or binding write (no database round trip per step) and stops the workflow with
`lock on <view> lost: <reason>`. `check` reads that flag, and it also fails with `not
renewed for <n> s` when the last successful renewal (or the acquire) is more than
`LOCK_TTL_SECS - LOCK_RENEW_SECS` = 90 s old in wall-clock time (`CvLock::ok_at`). Tokio
timers do not count the time a computer sleeps, so after a resume the renewal task may not
have run yet while the row has already expired and another operator holds it; the 90 s
check stops the workflow before its next step.
Three writes still happen after a lost lock, because they record work already sent to Pulp
or why the workflow stopped: the href of a copy task that was just submitted; the `failed`
state of a stopped publish; and the final state of a promotion (`partial`, or `failed` when
no repository was done). The last two are conditional when the lock is lost:
`Store::fail_publishing_version` changes the version only while it is `publishing`, and
`Store::finish_promoting_binding` changes the binding only while it is still this
version's `promoting` row. So they do not overwrite the result of an operator who took the
lock over and finished the work (the audit row is still written). A stopped publish keeps
its checkpoints, so *Resume* continues it; a promotion is retried. *Break lock*
(`Store::break_lock`) deletes the row only when its owner and `acquired_at` still match the
holder shown in the dialog, so it cannot remove a lock that another operator took meanwhile.
Dropping the `CvLock` (workflow end, Esc) releases the row
by token. A crash leaves the row until it expires. The same code runs for the local file, so
two pulp-tui processes on one host also exclude each other. Publish, resume, promote
(rollback, reconcile), delete view and delete version lock; preview, compare, verify and
edits do not. The exception is an environment relabel: it takes the locks of every view
bound in the environment, in `cv_id` order, and fails with the holder's name if one is taken,
before it changes anything. `CvLock::acquire` never waits, so two relabels cannot block each
other; the order only fixes which holder is named in the refusal. A composite publish takes
the lock of the composite only: it reads the component versions and does not lock the
components, which are protected by the delete rules instead (a version that a composite uses
cannot be deleted). Auto publish locks each composite separately, after the lock of the
component is released.

**Read-only mode.** When the store in use by the active profile is external (`App::shared()`;
like `scope`, it follows `cv_store`, not the edited config, so an Edit applies at Connect),
edits of Content Views and environments and `import-cv` change shared state, so
`Act::mutates(kind, shared)` counts them as mutating. Otherwise they are not (they touch
only the local file). `MigrateDb` and `CopyDb` always mutate, so a read-only profile cannot
migrate or copy.

**Moving data.** `pulp-tui db copy [--from <profile|local>]` and the "copy and switch"
choice of the TUI use `Store::copy_from(src, src_scope, dst_scope)`. The source can be any
store: the local file (`--from local`, the default) or the database of another profile. The
copy runs in one target transaction, with new ids and remapped references. It refuses name
clashes and never merges. It refuses `source and target are the same database` (equal
`db_id`, so two URLs of one database are caught) and a Content View whose lock was renewed
within `LOCK_TTL_SECS` (`Content View <name> is locked by <owner> (<operation>); copy after
it ends`); the lock of a crashed session has expired and does not block. The database ID of
the source becomes an alias in the target `meta`. The source is only read. When Edit
changes the database of a profile that has data, the TUI counts the data in the old
database and offers copy, switch or cancel; the profile is saved only after the copy worked.

**Limits.**

* The database coordinates operators through locks only. There is no daemon: workflows run
  only while a TUI runs. Pulp tasks continue without the client; the client-side
  orchestration (next steps of a publish or promotion) waits for the next *Resume* or
  promotion, which any operator can start.
* Lock times come from the clients' clocks. The 2-minute TTL covers normal clock skew, not
  hours of it.
* Other operators' changes show after a refresh. There is no push.
* A shared SQLite file works only when all operators use one host, and every operator's
  account must be able to write the file and its `-wal` and `-shm` files (a group-writable
  directory). SQLite WAL does not work on NFS or SMB.
* The operator name contains the host name. In a container it is a random ID unless the
  container is started with `--hostname`.
* Two *different* databases that manage the same Pulp server do not coordinate. The
  ownership labels (including `pulp_tui_db`) stop silent takeover but do not lock.
* A server that later needs unattended multi-stage workflows can run the existing
  `CvService` / `replication` code as a worker against the same PostgreSQL tables and resume
  checkpointed versions. That daemon does not exist.

Export/import (`pulp-tui export-cv`, `import-cv`, or the Servers screen) moves Content
View and environment definitions between databases (format 2, see "Schema and upgrades").
It contains no secrets and no Pulp state.

## Security model

* TLS is always verified. A custom CA bundle is added to the system roots. There is no
  option to disable verification or to fall back from HTTPS to HTTP.
* The database connection to a remote PostgreSQL always uses TLS with `verify-full`
  (chain and host name); only a Unix socket or a loopback host is exempt. The URL may not
  contain a password, and errors show the URL without it.
* Secrets are references in the config (`env:VAR`, `file:/path`, `keyring`). The config
  parser rejects unknown keys such as `password`, a profile `url` with user info, and a
  `proxy` with a password (use `HTTPS_PROXY` for an authenticated proxy). Secret files that are readable by group
  or others cause a warning in the log. The `[database]` secret and the secret of a
  profile's own `database` use the same references.
* The `Authorization` header is marked sensitive. The log file (never the terminal)
  records method, path and status only. Write-only fields (remote passwords, keys) are
  never pre-filled in forms and are displayed masked.
* The Logs screen (`src/session_log.rs`) keeps the last 1000 requests, actions and notices
  of the session in memory, with request and response bodies cut at 64 KiB. Secret JSON
  keys (any letter case, also a remote's `headers`) are redacted before a body is stored, the `Authorization` header is never recorded
  and upload bodies are replaced by their size. The log is never written to disk.
* Read-only profiles block every mutating request in the client before it is sent, and
  the UI hides mutating actions.
* RBAC is enforced by Pulp. pulp-tui adds warnings (granting owner/admin roles, setting
  `is_staff`, changing or removing one's own access) and refuses deleting the logged-in
  account. It never works around a 403.
* Destructive operations show the impact first. Deleting repositories, distributions,
  users, roles, Content Views and protected promotions requires typing the name.
  Production profiles show a red banner and add a warning line to these dialogs.
