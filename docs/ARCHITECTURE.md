# Architecture

pulp-tui is one Rust binary. It talks to Pulp 3 only through the documented REST API.
It never reads or writes the Pulp database and it never runs `pulp`, `curl` or other
tools for normal operation.

## Layers

| Layer | Module | Responsibility |
|---|---|---|
| Transport | `src/pulp/client.rs` | Auth (basic, bearer, mTLS), TLS (always verified, custom CA bundles), proxies, timeouts, bounded concurrency (semaphore), retries for reads only, pagination, response size limit, error mapping, href confinement to the configured API root |
| Capabilities | `src/pulp/caps.rs` | `status/` (component versions, health, content origin), `login/` or `users/?username=` (current user), live OpenAPI schema (endpoints, fields, enums) |
| Typed adapters | `src/pulp/models.rs`, `src/pulp/ops.rs` | RPM/File endpoints, task and task-group waiting, find-or-create helpers, publications, distributions, uploads, content listing, diffs, served-content verification |
| Services | `src/cv.rs`, `src/replication.rs`, `src/filters.rs` | Content Views (publish, promote, rollback, verify, cleanup), replication (native and fallback), filter rules and `rpmvercmp` |
| Storage | `src/store.rs` | SQLite: task references, Content View definitions, versions, checkpoints, environments, bindings, audit log, replication runs |
| TUI | `src/tui/*` | Event loop, state, screens, forms, confirmations, Task Center |

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
* Hrefs from the server (including `next` links) must stay on the same origin and below
  the configured API root.

### Task tracking

Pulp tasks are the source of truth. The Task Center stores task references in SQLite and
polls each unfinished task on its own schedule: the interval starts at `poll_min_ms`,
grows by 1.5x while nothing changes, resets on progress, and has ±20 % jitter. At most
three polls run at the same time. A task that a workflow already waits on is not polled
twice. After a restart, unfinished references are polled again ("re-attached").

Task groups (native replication) are final when no task is waiting or running and the
group says all tasks were dispatched, a task failed, or the replication finalizer
completed. pulpcore 3.123 leaves `all_tasks_dispatched` false after a replication, so an
idle group whose counts stay the same for three polls is also treated as final.

## Native Pulp concepts versus application-managed concepts

| Concept | Owner | Notes |
|---|---|---|
| Repository, Remote, Repository Version, Publication, Distribution, Task, Task Group, User, Group, Role, Upstream Pulp | **Pulp** | Used as documented. The Repository and the Remote are separate objects. |
| Content View, Content View Version, Lifecycle Environment, Promotion, Binding | **pulp-tui** | Not a Pulp API and not Katello. Stored in the local SQLite database and built from the Pulp objects above. |

### Content View implementation

* A **Content View** is a named list of RPM and File repositories. Each repository has
  optional filter rules (`src/filters.rs`).
* **Publish** first records the latest version of every source repository, so the
  snapshot is consistent in time. Then for each repository:
  1. find or create the derived repository `pulp-tui-cv-<view>-<repo>`, labeled
     `pulp_tui_cv=<view>`, with no retention limit;
  2. copy the selected content into it from an empty base: RPM uses `rpm/copy/` with
     `dest_base_version: 0` (all content, or an explicit list when filters exist); File
     uses `modify` with `remove_content_units: ["*"]`;
  3. record the new repository version (or the latest one if the content did not change);
  4. reuse or create the publication of that version.
  Every step writes a checkpoint. The href of a submitted copy task is stored before
  waiting, so an interrupted publish **re-attaches to the same task** and does not submit
  it again.
* RPM filters select packages by name glob, architecture glob and EVR comparison
  (`rpmvercmp` port, tested with rpm's own cases). An advisory is kept only when every
  package it references that exists in the source is selected. Other metadata (comps,
  modules, distribution trees, repo metadata files) is copied unchanged. Module artifact
  packages are copied with their module even when a filter excludes them, because
  `rpm/copy/` adds module children.
* A **Content View Version** is immutable. It refers to exact derived repository versions
  and publications. Version numbers are never reused.
* **Lifecycle environments** form paths from `Library`, for example
  `Library -> Dev -> Test -> Prod`. Publishing makes the new version active in Library.
* **Promote** binds a version to an environment. The version must be active in the prior
  environment. Each (view, environment, repository) has a stable distribution
  `pulp-tui-cv-<view>-<env>-<repo>` at `<prefix>/<env>/<view>/<repo>`. Promotion re-points
  it to the version's publication. Repositories that are bound but not part of the version
  stop serving (`publication: null`); the distribution stays for a later rollback.
* **Rollback** is a promotion of any earlier published version. Newer versions are never
  deleted.
* **Verify / drift**: for every binding, check that the distribution serves the expected
  publication, that the publication belongs to the expected snapshot version, that the
  snapshot still exists, and that served content matches (`PULP_MANIFEST` equal to the
  snapshot for File; `repomd.xml` plus the smallest package with digest check for RPM).
  Retention set on a derived repository is reported. Drift is reconciled by promoting the
  bound version again (idempotent).
* **Ownership**: pulp-tui never modifies a derived repository or CV distribution that does
  not carry its ownership labels. Deleting a CV version keeps Pulp objects that another
  live version or a binding still uses.

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
  item. Objects without this label are conflicts and are skipped. The fallback never
  deletes anything and does not copy content guards, labels or other Pulp metadata.
* Guards: source and destination must be different servers; upstream distributions whose
  content URL is on the destination's content origin are skipped as loops.

## Storage

SQLite (`<data_dir>/pulp-tui.db`, WAL mode). Migrations are numbered SQL batches applied
in a transaction and tracked with `PRAGMA user_version`. A database from a newer version
is refused. All rows are scoped by profile name.

**Limits of local-only orchestration**

* One operator per database file. Two operators with two databases can create Content
  Views with the same name on one Pulp server; ownership labels prevent silent takeover,
  but they do not coordinate work.
* Workflows run only while the TUI runs. Pulp tasks continue without the client; the
  client-side orchestration (next steps of a publish or promotion) waits for the next
  *Resume* or promotion.

**Migration path to a shared service.** The schema uses only portable SQL (integer keys,
text, no SQLite-specific types). To support several operators or unattended multi-stage
workflows, move `Store` behind a small service with PostgreSQL, keep the same tables, and
run the existing `CvService` / `replication` code there as a worker that resumes
checkpointed versions. The TUI would call that service instead of opening SQLite. No such
daemon is needed for single-operator use.

Export/import (`pulp-tui export-cv`, `import-cv`, or the Servers screen) moves Content
View and environment definitions between databases. It contains no secrets and no Pulp
state.

## Security model

* TLS is always verified. A custom CA bundle is added to the system roots. There is no
  option to disable verification or to fall back from HTTPS to HTTP.
* Secrets are references in the config (`env:VAR`, `file:/path`, `keyring`). The config
  parser rejects unknown keys such as `password`. Secret files that are readable by group
  or others cause a warning in the log.
* The `Authorization` header is marked sensitive. Logs (file only, never the terminal)
  record method, path and status only. Write-only fields (remote passwords, keys) are
  never pre-filled in forms and are displayed masked.
* Read-only profiles block every mutating request in the client before it is sent, and
  the UI hides mutating actions.
* RBAC is enforced by Pulp. pulp-tui adds warnings (granting owner/admin roles, setting
  `is_staff`, changing or removing one's own access) and refuses deleting the logged-in
  account. It never works around a 403.
* Destructive operations show the impact first. Deleting repositories, distributions,
  users, roles, Content Views and protected promotions requires typing the name.
  Production profiles show a red banner and add a warning line to these dialogs.
