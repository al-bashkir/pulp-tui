# Implementation status

Date: 2026-11-02. Tested against pulpcore 3.123.1, pulp_rpm 3.39.5, pulp_file 3.123.1,
pulp_deb 3.11.1 (`quay.io/pulp/pulp:latest`) and PostgreSQL 16. See
[COMPATIBILITY.md](COMPATIBILITY.md) for other tested images.

## Verified workflows

Automated against real Pulp containers (`tests/run-live.sh`). Each item gives the scenario,
then the test.

### Repositories (RPM, File)

* RPM: local repository, upload, remote, sync, inspect and compare versions (advisory and
  packages), publish, distribute, served metadata and package, update, rollback, verify,
  restore v1 as latest. `live_repos::rpm_lifecycle`
* RPM: a comps-only version; compare lists group, category, environment and langpacks,
  with Pulp's counts. `live_repos::rpm_compare_lists_comps_units`
* File: single and chunked upload, remove content, sync from a manifest, publish,
  distribute, served files, rollback, verify, a custom manifest name.
  `live_repos::file_lifecycle`
* File: a path with a comma (`reports/q1,2024.txt`) is published, served and verified.
  `live_repos::file_path_with_a_comma_is_served_and_verified`
* File: versions that a distribution serves are reported as protected, and Pulp refuses
  their delete (400); an unserved version deletes.
  `live_repos::served_versions_are_protected_from_delete`
* File: restore as latest makes a new version with the old content; a following
  distribution serves it after publish; a second restore makes no version.
  `live_repos::restored_version_is_served_after_publish`

### Deb

* Mirror sync of the fixture, structured copy with `exclude name=*-dbg`, publish,
  distribute, served `Packages` without the excluded package, package digest; a signed
  publication is reused only for its signing service. `live_deb::deb_mirror_copy_publish_verify`
* A package removed with its release records is no longer served; a restore serves it
  again. `live_deb::a_removed_deb_package_is_no_longer_served`
* A distribution that serves a verbatim publication protects its version, as Pulp says on
  delete. `live_deb::a_served_verbatim_publication_protects_its_version`
* A repository with its own signing service signs a publication without a service; after
  `signing_service: null` the next one is unsigned.
  `live_deb::deb_repository_signs_with_its_own_service`
* Deb Content View: filter, preview, publish signed, promote to Dev, publish unsigned
  (`none`), Resume of a signed version, a composite with this view.
  `live_deb::deb_content_view`

### Content Views

* RPM + File snapshot with filters, preview, numbered versions, Library, Dev, Prod path
  rule, rollback, served-content verification, retention protection, advisory filtering,
  partial failure and retry, Resume that re-attaches to the submitted task, cleanup.
  `live_cv::content_view_lifecycle`
* Environment relabel moves the distribution in place: the new URL answers 200, the old
  one 404, the drift check follows. `live_cv::environment_relabel_moves_served_paths`
* A File view serves the custom manifest name of its source, not `PULP_MANIFEST`.
  `live_cv::a_file_view_serves_the_manifest_name_of_its_source`
* Resume of an older failed version publishes it; Library keeps the newer version.
  `live_cv::resume_of_an_older_version_keeps_library_on_the_newer_one`
* Unpromote goes back along the path (Test, Dev, Library); unpromote from Dev is refused
  while Test has the view. `live_cv::unpromote_goes_back_along_the_path`
* Two databases share one Pulp: B refuses A's objects, adopts them after it knows A's id
  as an alias; A's delete then keeps them. `live_cv::content_view_owned_by_another_database`
* Composite `rocky9` (File view pinned at v1, RPM view on latest): publish without a copy,
  own Library paths, auto publish, compare, delete refusals, deletes keep borrowed
  objects. `live_cv::composite_content_view_rocky9`

### Replication

* Native replication pulp2 → pulp1, rerun, upstream update; fallback conflicts with
  native objects. `live_replication::native_replication`
* A canceled native replication ends and is not reported as completed.
  `live_replication::canceled_native_replication_is_not_complete`
* Fallback: dry run, create, idempotent rerun, upstream update, loop and self guards.
  `live_replication::fallback_replication`
* Repository and remote definitions pulp2 → pulp1: secrets are not in the file, the
  repositories get the remote of the same name, a rerun keeps all objects.
  `live_replication::repo_definitions_move_between_servers`

### Export and import

* Pulp export of a File repository (full, incremental, chosen versions, start versions),
  import check, import through an importer mapping into another repository (also the
  incremental export), list and delete of exports, importers and the exporter.
  `live_export::pulp_export_and_import_of_a_file_repository`
* Filesystem export of a File publication and of a version; a second export into the same
  directory fails until the path changes. `live_export::filesystem_export_of_a_file_publication`

### Content

* RPM comps and advisory uploads make new versions; `replace` removes the old groups.
  `live_content::rpm_comps_and_advisory_uploads_make_new_versions`
* ACS for RPM, File and Deb: an `immediate` remote is refused, refresh is a task group that
  ends with its tasks (also as one Task Center task), delete. `live_content::acs_lifecycle`
* An ACS path edit removes the path from another ACS too (pulpcore behavior).
  `live_content::acs_path_edit_removes_the_path_from_other_sources`
* Version repair reports zero missing, corrupted and repaired files.
  `live_content::version_repair_reports_its_counts`
* Reclaim space removes stored files; the content stays in the version and is served on
  demand with the right digest. `live_content::reclaim_space_keeps_the_content_served_on_demand`

### Administration (through the TUI actions)

* Group rename through the Edit form. `live_admin::group_rename`
* Server tasks filter (state, name, reserved resource, start date); Pulp refuses a bad
  date. `live_admin::server_tasks_filter`
* Task delete, and a purge that matches no task (count, typed confirmation, result).
  `live_admin::task_delete_and_purge`
* Workers screen: name, heartbeat, Inspect. `live_admin::workers_screen`
* Repository definitions export from the Servers screen without secrets, then plan and
  import. `live_admin::repository_definitions_export_and_import`
* Task groups screen: counts, Inspect reads the whole group. `live_admin::task_groups_screen`
* Cancel of a running task group (a stalled File ACS refresh); the Task Center row ends
  `canceled` with a warning. `live_admin::task_group_cancel`

### RBAC and failures

* 401, 403, no self-escalation, creator role, group object role, revoke.
  `live_rbac::rbac_is_enforced_and_not_bypassed`
* Server-side cancel of waiting and running tasks, 409 for completed tasks.
  `live_failures::cancel_waiting_and_running_tasks`
* TLS errors are reported and never downgraded.
  `live_failures::tls_errors_are_reported_and_not_downgraded`

### Shared database (PostgreSQL 16)

* `db migrate` creates the schema once; an uninitialized, older or newer schema is
  refused; the database id is created once. `live_shared_db::postgres_schema_lifecycle`
* Schema 2 → 3 keeps served paths (`Library`, `Dev`, `dev` get `library`, `dev`, `dev`).
  `live_shared_db::postgres_migration_3_keeps_served_paths`
* Two operators: shared views and audit actor, lock conflict names the holder, `break_lock`
  removes only the holder shown, 8 parallel attempts give one winner.
  `live_shared_db::two_operators_share_state_and_locks`
* Every store method, conditional writes after a lost lock, `db copy` from SQLite.
  `live_shared_db::store_methods_on_postgres`
* Composite store methods: component refusals, pins, auto publish, borrowed rows, foreign
  keys, `db copy` of a composite. `live_shared_db::composite_store_methods_on_postgres`
* A database per profile next to a local profile: keyring user, `NotMigrated`, cached
  store, `copy_from` with the source id as alias.
  `live_shared_db::profile_database_next_to_a_local_profile`

### Real upstream repositories

`tests/run-live.sh real` needs internet. Syncs are on demand, so only metadata and the
probed packages download.

* Debian `trixie` `main` `amd64` (68,825 packages): a Content View with `hello` and
  `nginx*` publishes and verifies; v2 with `hello` only has no `nginx`; a removed package
  is not served. `live_real::debian_main_real`
* Rocky 9 BaseOS (3,336 packages): a `kernel*` view publishes and verifies; after a
  `checksum_type: sha512` PATCH, a new publication uses `sha512`. `live_real::rocky9_baseos_real`
* Rocky 9 AppStream (10,743 packages): cost of one page (500 and 5000 rows) and of the full
  listing. `live_real::rocky9_appstream_page_cost`

### By hand

* In the TUI (tmux, real servers): dashboard, repository create/search/upload, remote
  create, mirror sync with warning, Task Center progress, distribute, versions, compare,
  rollback with preview and verification, protected Prod with typed confirmation, Content
  View create/add repos/preview/publish/promote/drift check and reconcile, restart with
  task re-attachment, fallback replication from another profile, user creation and the
  self-delete guard.
* Not in `tests/run-live.sh`: `db migrate`, `import-cv` and `export-cv` with a PostgreSQL
  role that has only `SELECT/INSERT/UPDATE/DELETE`; TLS to a PostgreSQL with a private CA
  on a non-loopback address, with and without `ca_cert` (see COMPATIBILITY.md).

## Test results (last run)

Run of 2026-11-02 on fresh containers (`tests/env/pulp-env.sh down-all`, then
`tests/run-live.sh` and `tests/run-live.sh real`):

| Check                                         | Result                    |
| --------------------------------------------- | ------------------------- |
| `cargo test --lib`                            | 442 passed                |
| `cargo test --test mock_http`                 | 34 passed                 |
| live `repos` / `cv` / `rbac` / `failures`     | 6 / 7 / 1 / 2, all passed |
| live `replication` / `shared_db` / `deb`      | 4 / 6 / 5, all passed     |
| live `export` / `content` / `admin`           | 2 / 5 / 7, all passed     |
| live `real`                                   | 3 passed (624 s)          |
| `cargo fmt --check`, `dprint check`           | clean                     |
| `cargo clippy -D warnings`, both feature sets | clean                     |

Not run again on 2026-11-02 (the results below are older):

* `podman build -f Containerfile .` (2026-10-27): built (31.0 MB image, no keyring, with
  the PostgreSQL driver; `pulp-tui --version` runs).
* `podman build --build-arg CARGO_FLAGS= -f Containerfile .` (2026-10-27, the release
  build of the Linux binary): a static-pie x86_64 binary with keyring; it reads the host's
  Secret Service over D-Bus. Only the release workflow builds the aarch64 binary.
* `cargo audit` (2026-10-26, 494 crates): no vulnerabilities. The dependencies in
  `Cargo.lock` have not changed since (only the pulp-tui version). 3 "unmaintained"
  warnings (RUSTSEC-2026-0370/0371/0372: `zvariant`, `zvariant_derive`, `zbus_names`), all
  only through the optional `keyring` feature; `--no-default-features` builds (the
  container image) do not contain them. `.cargo/audit.toml` ignores RUSTSEC-2023-0071
  (`rsa`): it is in `Cargo.lock` only because sqlx `any` names the optional `sqlx-mysql`
  driver, and it is never compiled (`cargo tree -i rsa --target all` is empty).

## Limitations

### Design

* Content View data lives in the local SQLite file of one operator, or in a shared
  database (`[database]` or a profile's `database`: PostgreSQL, or SQLite on one host).
  There is no pulp-tui server. Other operators' changes show after a refresh.
* Client steps of publish and promote run only while pulp-tui runs; Pulp tasks always
  continue. *Resume* continues an interrupted publish, also by another operator on a
  shared database. No daemon resumes them.
* pulp-tui locks one Content View per workflow (expiry 2 minutes, renewal every 30 s,
  client clocks). Other tools, and two databases on one Pulp server, are serialized only by
  Pulp's task resource locks.
* Fallback replication synchronizes content only and never deletes.
* A composite has no filters of its own and cannot be a component. Reusable filter sets
  are not implemented.

### Not supported

* Plugins other than RPM, File and Deb; creation of content guards and signing services;
  OIDC token refresh.
* Deb: replication (a Deb distribution is skipped with a note),
  creation of verbatim publications (pulp-tui reads existing ones for protection, verify
  and rollback), dependency solving on copy. Deb Content View publications are never
  reused.
* The signing service pickers list the services of all plugins (pulpcore has no type
  filter); Pulp refuses a wrong one.
* The **K** key cannot store the password of the top-level `[database]`. Use `env:` or
  `file:`, or create the keyring entry (service `pulp-tui`, user `[database]`) with the
  system tool. A profile's own `database` works (user `[database]:<profile>`).
* `nested_by_digest` RPM layout: served-content checks verify metadata only.

### Not tested

* Pulp domains, macOS at runtime (CI builds and runs unit tests), Windows.
* The shared-database TLS path (private CA, non-loopback host) only by hand. `ca_cert`
  adds to the built-in Mozilla roots; the OS store is not used.
* Not run by hand in a terminal; covered by unit tests with mock servers and SQLite
  stores, and by the live service tests:
  * two TUI sessions on one shared database;
  * the database per profile (open on Connect, migrate confirm, copy and switch);
  * environment relabel (Edit form, impact list, result);
  * composite Content Views (create, components and pins, auto publish, deletes);
  * Deb publish with a signing service and the Deb signing choice of a publish.

## Risks

* Native replication is a pulpcore technology preview. Its API changed between versions
  (for example `policy`, `q_select`). pulp-tui reads the live schema, but older or newer
  servers can need changes.
* pulpcore serves files of superseded publications for
  `DISTRIBUTED_PUBLICATION_RETENTION_PERIOD` (default 3 days). After a rollback, clients
  that use repository metadata get the old content at once; direct file URLs can still
  work during that period.
* An RPM filter that excludes a package of a module or a comps group excludes it. The
  module or group then lists a package that the snapshot does not have.
* pulpcore 3.123 deletes a path that an ACS edit removes from every ACS with that path.
  The Edit form warns about it.
* Derived repository and distribution names come from the view, environment and repository
  names. A rename of a source repository in Pulp does not rename them.
