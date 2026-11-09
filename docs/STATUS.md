# Implementation status

Date: 2026-11-09. Tested against pulpcore 3.123.1, pulp_rpm 3.39.5, pulp_file 3.123.1,
pulp_deb 3.11.1 (`quay.io/pulp/pulp:latest`) and PostgreSQL 16. pulpcore 3.95.3 and 3.63.36
are also tested. See [COMPATIBILITY.md](COMPATIBILITY.md) for their plugin versions.

## Verified workflows

Automated against real Pulp containers (`tests/run-live.sh`). Each item gives the scenario,
then the test. Tests with `through_the_tui` in the name, and all of `live_admin`, drive the
real `App` (keys, forms, confirmations, jobs, Task Center), not only the services. Braces in
a test path stand for several tests, for example `{file,rpm}_x` for `file_x` and `rpm_x`.

### Repositories (RPM, File)

* RPM: local repository, upload, remote, sync, inspect and compare versions (advisory and
  packages), publish, distribute, served metadata and package, update, rollback, verify,
  restore v1 as latest. `live_repos::rpm_lifecycle`
* RPM: a rollback of a repository that has a metadata signing service serves signed
  metadata. `live_repos::rpm_rollback_of_a_signing_repository_serves_signed_metadata`
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
* File: an upload with Overwrite off fails on other content at the same path and keeps the
  old file; with Overwrite on it replaces the file in a new version.
  `live_repos::file_upload_without_overwrite_keeps_the_file_of_the_same_path`
* Sync history lists the syncs of a repository, newest first, with the version of each.
  `live_repos::sync_history_lists_the_syncs_of_a_repository`

### Repositories, remotes and distributions through the TUI

* Publications and Distributions screens (File, RPM, Deb): create a publication and pinned
  and bound distributions; edit the content guard, checkpoint, base path and binding after
  the typed name; rollback with a preview that names the publication; delete with an impact
  that names the distribution.
  `live_repos::{file,rpm,deb}_publications_and_distributions_through_the_tui`
* Publish, Distribute and Rollback from the Versions and Repositories screens; "follow
  repository latest"; the Logs screen lists the actions and requests.
  `live_repos::{file,rpm}_serve_from_versions_through_the_tui`
* Re-point refuses an unknown version and warns about another repository; Distribute from
  all versions offers the repository name.
  `live_repos::re_point_checks_the_version_and_warns_about_another_repository`,
  `live_repos::distribute_from_all_versions_offers_the_repository_name`
* Distribute (D) to an existing distribution of the same name (File, Deb): the dialog says
  what it serves now and what it will serve; Esc changes nothing; the typed name re-points it.
  `live_repos::{file,deb}_distribute_re_points_after_the_typed_name_through_the_tui`
* Versions screen (File, RPM, Deb): compare, restore, repair, delete, reclaim space.
  `live_repos::{file,rpm,deb}_version_actions_through_the_tui`
* Mirror sync (File, RPM, Deb): the confirmation says that the new version matches the
  upstream; the next version has the upstream content again.
  `live_repos::{rpm,file,deb}_mirror_sync_through_the_tui`
* RPM sync with `mirror_content_only`, Optimize off and Skip srpm: the request has the
  policy and both options, the new version has the upstream content again, and no
  publication is made. `live_repos::rpm_mirror_content_only_sync_with_options_through_the_tui`
* Uploads of a File, an RPM package and a `.deb` into empty repositories; Copy content
  (path rule, no rules, name rule); Prune orphans.
  `live_repos::uploads_of_each_type_through_the_tui`,
  `live_repos::copy_content_through_the_tui`, `live_repos::prune_orphans_through_the_tui`
* File upload with Overwrite off keeps the file of the same path.
  `live_repos::file_upload_without_overwrite_through_the_tui`
* Repository delete, single (the impact names the distribution and the Content View) and
  marked (one typed confirmation).
  `live_repos::repository_delete_single_and_marked_through_the_tui`
* A repository, version or publication that belongs to another database is not deleted,
  and its content is not removed.
  `live_repos::snapshot_repository_of_another_database_is_not_deleted_through_the_tui`,
  `live_repos::snapshot_versions_publications_and_content_of_another_database_are_not_changed_through_the_tui`
* Repository edit: a File repository saves and clears its fields; an emptied *Retain package
  versions* is saved as 0; a label edit keeps the RPM OSV config.
  `live_repos::file_repository_edit_through_the_tui`,
  `live_repos::rpm_retain_package_versions_cleared_through_the_tui`,
  `live_repos::rpm_osv_config_survives_a_label_edit_through_the_tui`
* A lower *Retain versions*: the dialog lists the versions that Pulp deletes; Esc keeps
  them; after the typed name Pulp deletes exactly those.
  `live_repos::a_lower_retain_versions_deletes_versions_only_after_the_typed_name_through_the_tui`
* RPM: Prune with a dry run, then a real prune; Scan; a filtered copy takes the advisory
  only of the chosen packages. `live_repos::rpm_prune_dry_run_then_prune_through_the_tui`,
  `live_repos::rpm_version_scan_through_the_tui`,
  `live_repos::rpm_filtered_copy_through_the_tui_takes_the_advisory_of_the_chosen_packages`
* Remotes screens (File, RPM, Deb, File Git): create, edit, delete with the impact of the
  repository that uses the remote; timeouts keep decimal seconds; a ULN remote keeps its
  write-only account on edit; a File Git remote syncs from the prefilled form.
  `live_repos::remotes_create_edit_and_delete_through_the_tui`,
  `live_repos::remote_timeouts_keep_decimal_seconds_through_the_tui`,
  `live_repos::uln_remote_create_edit_and_delete_through_the_tui`,
  `live_repos::file_git_remote_syncs_a_repository_with_the_prefilled_form`
* A File mirror sync from a Git remote typed by name (the picker has no options): the
  confirmation says that the sync task fails.
  `live_repos::file_mirror_sync_from_a_typed_git_remote_name_warns_that_the_task_fails_through_the_tui`

### Deb

* Mirror sync of the fixture, structured copy with `exclude name=*-dbg`, publish,
  distribute, served `Packages` without the excluded package, package digest; a signed
  publication is reused only for its signing service. `live_deb::deb_mirror_copy_publish_verify`
* A package removed with its release records is no longer served; a restore serves it
  again. `live_deb::a_removed_deb_package_is_no_longer_served`
* A version without Release units is served and verified: pulp_deb makes `dists/` and the
  Release file. `live_deb::a_deb_version_without_release_units_is_served_and_verified`
* A distribution that serves a verbatim publication protects its version, as Pulp says on
  delete. `live_deb::a_served_verbatim_publication_protects_its_version`
* A verbatim publication is made, distributed, verified and deleted; the delete leaves the
  distribution with nothing to serve, as the impact says. The Publications Create form
  makes one through the TUI.
  `live_deb::a_verbatim_publication_is_made_distributed_verified_and_deleted`,
  `live_deb::deb_verbatim_publication_create_through_the_tui`
* A repository with its own signing service signs a publication without a service; after
  `signing_service: null` the next one is unsigned.
  `live_deb::deb_repository_signs_with_its_own_service`
* A rollback of a distribution with a simple publication makes a simple publication.
  `live_deb::deb_rollback_keeps_a_simple_publication`
* Repository delete names the distribution of a verbatim publication; both publications
  go. `live_deb::deb_repository_delete_impact_names_the_distribution_of_a_verbatim_publication`
* Repository edit saves the signing service, the per-release service and fingerprint and the
  excluded metadata fields; a second edit clears them.
  `live_deb::deb_repository_edit_of_the_signing_fields_through_the_tui`
* Uploads: a `.deb` goes into a release and makes the package and its release records. The
  TUI uploads a `.udeb`, a source package (`.dsc` with its files) and generic content, each
  as its own type. `live_deb::an_uploaded_deb_package_is_published_and_served`,
  `live_deb::the_tui_uploads_a_udeb_a_source_package_and_generic_content_as_their_types`
* Release records: the TUI creates a release, a release component, a release architecture
  and a package release component; a structured publication serves the package under the
  component. `live_deb::the_tui_creates_release_records_that_a_publication_serves`
* A package release component created on the Content screen of an older version goes into
  the latest version; the confirm names the package and the release component that come
  back, and a release architecture that the latest version lacks. `live_deb::a_package_release_component_of_an_older_version_confirms_what_comes_back_through_the_tui`
* Deb Content View: filter, preview, publish signed, promote to Dev, publish unsigned
  (`none`), Resume of a signed version, a composite with this view.
  `live_deb::deb_content_view`
* Publish of a Deb Content View through the TUI with each signing choice: the signing
  service (signed Release), none, and same as source (no key, unsigned). Each version stores
  its choice. `live_deb::deb_content_view_publish_signing_through_the_tui`

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
* A promotion that fails for every repository keeps the served version bound; it cannot be
  deleted and a rollback to it works. `live_cv::a_failed_promotion_keeps_the_served_version`
* Two databases share one Pulp: B refuses A's objects, adopts them after it knows A's id
  as an alias; A's delete then keeps them. `live_cv::content_view_owned_by_another_database`
* Composite `rocky9` (File view pinned at v1, RPM view on latest): publish without a copy,
  own Library paths, auto publish, compare, delete refusals, deletes keep borrowed
  objects. `live_cv::composite_content_view_rocky9`
* The cleanup of a publish keeps a served version that no view version records. A delete
  keeps what a plain distribution serves, and the view delete is refused until that
  distribution is gone.
  `live_cv::publish_cleanup_keeps_a_served_version_that_no_view_version_records`,
  `live_cv::deletes_keep_what_a_plain_distribution_serves`
* A promotion to a base path inside or above a foreign distribution: the preview says that
  it fails, and pulp-tui refuses it before Pulp does; after a clean preview it completes.
  `live_cv::a_promotion_to_a_base_path_that_overlaps_another_fails_as_the_preview_says`
* A distribution that left the view and was deleted or taken over outside pulp-tui does
  not block the environment: the promotion or the Publish forgets its row and completes.
  `live_cv::a_left_out_distribution_deleted_outside_pulp_tui_does_not_block_the_environment`,
  `live_cv::publish_leaves_alone_a_kept_library_distribution_taken_over_outside_pulp_tui`
* The same three cases through the TUI: the Promote preview names the overlapping base path
  and nothing is created; the Promote preview says that pulp-tui forgets the deleted
  distribution, the promotion completes and the old version can be deleted; the Publish
  preview names the taken-over distribution and Publish leaves it alone.
  `live_cv::a_promotion_to_an_overlapping_base_path_is_named_and_creates_nothing_through_the_tui`,
  `live_cv::a_left_out_distribution_deleted_outside_pulp_tui_is_forgotten_through_the_tui`,
  `live_cv::a_kept_library_distribution_taken_over_outside_pulp_tui_is_left_alone_through_the_tui`
* A kept Library distribution bound outside pulp-tui to a repository: Verify reports it, and
  the next Library step points it at nothing again.
  `live_cv::a_kept_library_distribution_bound_to_a_repository_is_reported_and_unbound`
* `export-cv` on pulp2 and `import-cv` on pulp1 map repositories by name and type; one
  missing repository refuses the whole import.
  `live_cv::import_cv_maps_repositories_by_name_to_the_target_server`

Through the TUI:

* Environments (create, protect, unprotect, delete) and the view definition (edit, remove
  repository). `live_cv::environments_and_view_definition_through_the_tui`
* The whole workflow on a File and an RPM repository: sync, Publish, Verify, Promote,
  Filters and a second Publish, Rollback, Compare, Unpromote, delete of a version and of
  the view. `live_cv::content_view_workflows_through_the_tui`
* Composite view: create, add components with pins, auto publish on, Publish, auto publish
  after a component publish, remove a component.
  `live_cv::composite_content_view_through_the_tui`
* Resume of a publish that stopped after its checkpoint: Watch shows the pending task,
  Resume is refused while another operator holds the lock, Break lock removes it, then
  Resume publishes. `live_cv::resume_watch_and_break_lock_through_the_tui`
* Resume publishes a composite that lags behind its component; a second Resume does
  nothing. `live_cv::resume_publishes_a_lagging_composite_through_the_tui`
* Environment relabel (Edit form, impact list, typed name) and the definitions export and
  import into an empty database.
  `live_cv::environment_relabel_and_definitions_export_import_through_the_tui`
* A relabel to a label with a foreign distribution below it is refused.
  `live_cv::relabel_to_an_overlapping_label_is_refused_through_the_tui`
* A partial promotion (a foreign distribution has the Dev path of one repository): the
  preview warns, and the Content Views, Versions, Environments and Verify screens show the
  attempt. A version that Dev serves in part is not deleted.
  `live_cv::a_partial_promotion_and_what_the_screens_show_through_the_tui`,
  `live_cv::a_version_that_dev_serves_in_part_is_not_deleted_through_the_tui`

### Replication

* Native replication pulp2 → pulp1, rerun, upstream update; fallback conflicts with
  native objects. `live_replication::native_replication`
* A canceled native replication ends and is not reported as completed.
  `live_replication::canceled_native_replication_is_not_complete`
* Fallback: dry run, create, idempotent rerun, upstream update, loop and self guards.
  `live_replication::fallback_replication`
* Fallback of Deb: a structured and a simple publication on pulp2; each replica syncs the
  served dists, publishes in the same mode, serves only the paths of that mode, and reruns.
  `live_replication::deb_fallback_replication`
* Fallback of a checkpoint File distribution (syncs from the newest checkpoint) and of a
  flat Deb repository served by a verbatim publication.
  `live_replication::checkpoint_fallback_replication`,
  `live_replication::deb_flat_verbatim_fallback_replication`
* Repository and remote definitions pulp2 → pulp1: secrets are not in the file, the
  repositories get the remote of the same name, a rerun keeps all objects.
  `live_replication::repo_definitions_move_between_servers`
* Through the TUI: Replicate (R) with its Task Center row, result view and History (H);
  fallback replication (F) of RPM and File, and of Deb, with the dry run and the result view.
  `live_replication::native_replication_through_the_tui`,
  `live_replication::fallback_replication_through_the_tui`,
  `live_replication::deb_fallback_replication_through_the_tui`
* The fallback dry run names a base path that overlaps a local distribution as a conflict.
  `live_replication::the_fallback_plan_names_a_base_path_that_overlaps_a_local_distribution`
* An Upstream Pulp whose base URL has user info: the list, Details, the Replicate
  confirmation, the failure text, the Task Center and the Logs show `•••@`, never the user
  or the password. `live_replication::upstream_base_url_user_info_never_shows_through_the_tui`

### Export and import

* Pulp export of a File repository (full, incremental, chosen versions, start versions),
  import check, import through an importer mapping into another repository (also the
  incremental export), list and delete of exports, importers and the exporter.
  `live_export::pulp_export_and_import_of_a_file_repository`
* Pulp writes the export even with `dry_run`, so the Export form does not offer it. An
  import takes the `.tar` of an export by its path.
  `live_export::pulp_export_dry_run_still_writes_the_export`,
  `live_export::pulp_import_of_the_export_tar_by_its_path`
* A Content View snapshot can be exported, and an import does not write into a snapshot
  unasked. `live_export::a_content_view_snapshot_can_be_exported`,
  `live_export::an_import_does_not_write_into_a_content_view_snapshot_unasked`
* Filesystem export of a File publication and of a version; a second export into the same
  directory fails until the path changes. `live_export::filesystem_export_of_a_file_publication`
* Through the TUI: Pulp Exporters and Importers (create, edit, export, import with the
  check and the typed name, delete) and Filesystem Exporters.
  `live_export::pulp_exporter_and_importer_through_the_tui`,
  `live_export::filesystem_exporter_through_the_tui`

### Content

* RPM comps and advisory uploads make new versions; `replace` removes the old groups.
  `live_content::rpm_comps_and_advisory_uploads_make_new_versions`
* An advisory with a newer `updated` replaces the older one; compare shows it as changed.
  `live_content::compare_shows_an_updated_advisory_as_changed`
* ACS for RPM, File and Deb: an `immediate` remote is refused, refresh is a task group that
  ends with its tasks (also as one Task Center task), delete. `live_content::acs_lifecycle`
* An ACS path edit removes the path from another ACS too (pulpcore behavior).
  `live_content::acs_path_edit_removes_the_path_from_other_sources`
* Version repair reports zero missing, corrupted and repaired files.
  `live_content::version_repair_reports_its_counts`
* Reclaim space removes stored files; the content stays in the version and is served on
  demand with the right digest. `live_content::reclaim_space_keeps_the_content_served_on_demand`
* Through the TUI: ACS create, edit, delete and refresh for RPM, File and Deb; Content
  screen Add and Remove for File, RPM and Deb; RPM uploads of comps.xml (also with
  Replace), advisory, modulemd, modulemd defaults and obsoletes.
  `live_content::acs_create_edit_and_delete_through_the_tui`,
  `live_content::acs_refresh_through_the_tui_ends_with_its_task_group`,
  `live_content::content_add_and_remove_through_the_tui`,
  `live_content::rpm_metadata_uploads_and_module_content_through_the_tui`,
  `live_content::a_modulemd_made_through_the_tui_is_in_the_new_version`

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
* Task groups: Track and Watch follow a group to its end. A new session re-attaches the task
  of a previous session once. `live_admin::task_groups_screen_tracks_and_watches_a_group`,
  `live_admin::task_center_reattaches_a_task_of_a_previous_session`
* Users: create with a password, edit, change the password, disable, delete.
  `live_admin::user_create_edit_password_disable_and_delete_through_the_tui`
* Groups and roles: group create and delete, member add and remove (the user stays), owner
  and viewer roles assigned and removed, custom role create, edit and delete.
  `live_admin::group_member_add_and_remove_keeps_the_user`,
  `live_admin::group_create_roles_and_delete_through_the_tui`,
  `live_admin::custom_role_create_edit_and_delete_through_the_tui`,
  `live_admin::object_roles_add_and_remove_a_viewer_on_a_repository`
* Content guards: a header guard used by a distribution (list, an edit of its value with the
  typed name, delete impact), an RBAC
  guard with download access, and the other guard types of the server.
  `live_admin::content_guard_create_edit_and_delete_name_its_distribution`,
  `live_admin::rbac_guard_access_allows_and_removes_a_user`,
  `live_admin::content_guards_of_the_other_types_are_created_through_the_tui`
* Server screens: Uploads (an unfinished upload is listed and deleted), Task Schedules,
  Domains (follows `DOMAIN_ENABLED`), Artifacts (find a sha256), Server repair, Access
  Policies (reset). `live_admin::an_unfinished_upload_is_listed_and_deleted`,
  `live_admin::task_schedules_screen`, `live_admin::domains_screen_follows_domain_enabled`,
  `live_admin::artifacts_screen_lists_and_finds_a_sha256`,
  `live_admin::server_repair_checks_that_the_files_exist`,
  `live_admin::access_policy_reset_restores_the_default`
* A checkpoint distribution verifies and refuses rollback; a remote that an ACS uses is not
  deleted. `live_admin::checkpoint_distribution_verifies_and_refuses_rollback`,
  `live_admin::remote_delete_refused_while_an_acs_uses_it`
* Upstream Pulps: create, edit (policy `labeled`), delete.
  `live_admin::upstream_pulp_create_edit_and_delete_through_the_tui`
* Server profiles: create, connect, edit to read-only, delete (the profile of the session is
  refused), comments kept in the file; Test connection; Store secret (K) in the system
  keyring (skips without one).
  `live_admin::server_profiles_create_edit_and_delete_through_the_tui`,
  `live_admin::server_profile_test_connection`,
  `live_admin::store_secret_in_the_keyring_through_the_tui`

### RBAC and failures

* 401, 403, no self-escalation, creator role, group object role, revoke.
  `live_rbac::rbac_is_enforced_and_not_bypassed`
* A user who cannot see a distribution gets 404 for it: a left-out Content View row of that
  distribution stays, with its protection; only an administrator's 404 forgets the row.
  `live_rbac::a_404_of_a_hidden_distribution_keeps_a_content_view_row_for_a_user`
* Server-side cancel of waiting and running tasks, 409 for completed tasks.
  `live_failures::cancel_waiting_and_running_tasks`
* TLS errors are reported and never downgraded.
  `live_failures::tls_errors_are_reported_and_not_downgraded`
* Server Tasks through the TUI: Track a running task, Cancel a waiting and a running one.
  `live_failures::track_and_cancel_server_tasks_through_the_tui`

### Shared database (PostgreSQL 16)

* `db migrate` creates the schema once; an uninitialized, older or newer schema is
  refused; the database id is created once. `live_shared_db::postgres_schema_lifecycle`
* Schema 2 → 3 keeps served paths (`Library`, `Dev`, `dev` get `library`, `dev`, `dev`).
  `live_shared_db::postgres_migration_3_keeps_served_paths`
* Migration 6 drops served rows of a missing environment, and a served row needs its
  environment. Migration 7 keeps an active binding served and turns any other into the
  attempt only. `live_shared_db::postgres_served_rows_need_their_environment`,
  `live_shared_db::postgres_migration_7_moves_unfinished_bindings_to_promotions`
* Migration 9 gives an environment back the served version of an old failed attempt when
  all its rows serve that version; a mixed set gets no served version.
  `live_shared_db::postgres_migration_9_restores_the_served_version_of_an_old_failed_row`
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
* Through the TUI (Servers screen): Edit of a profile to a new database offers the copy and
  the migrate; Connect to a database that is not migrated offers the migrate, and the
  Content View screens then work. `live_shared_db::database_migrate_and_copy_through_the_tui`

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

Run of 2026-11-09 at 6d64485 on fresh containers (`tests/env/pulp-env.sh down-all`, then
`tests/run-live.sh <scenario>` for each scenario and for `real`). The counts below are for
`quay.io/pulp/pulp:latest` (pulpcore 3.123.1; the whole run took 2814 s). The images 3.95.3
and 3.63.36 ran the same number of tests and passed (see COMPATIBILITY.md).

| Check                                         | Result           |
| --------------------------------------------- | ---------------- |
| `cargo test --lib`                            | 685 passed       |
| `cargo test --test mock_http`                 | 53 passed        |
| live `repos` / `cv` / `rbac` / `failures`     | 44 / 27 / 2 / 3  |
| live `replication` / `shared_db` / `deb`      | 12 / 10 / 16     |
| live `export` / `content` / `admin`           | 8 / 11 / 29      |
| live total, all scenarios except `real`       | 162 passed       |
| live `real`                                   | 3 passed (644 s) |
| `cargo fmt --check`, `dprint check`           | clean            |
| `cargo clippy -D warnings`, both feature sets | clean            |

Not run again on 2026-11-09 (the results below are older):

* `podman build -f Containerfile .` (2026-10-27): built (31.0 MB image, no keyring, with
  the PostgreSQL driver; `pulp-tui --version` runs).
* `podman build --build-arg CARGO_FLAGS= -f Containerfile .` (2026-10-27, the release
  build of the Linux binary): a static-pie x86_64 binary with keyring; it reads the host's
  Secret Service over D-Bus. Only the release workflow builds the aarch64 binary.

`cargo audit` (2026-11-09): 0 vulnerabilities that are not ignored, and 4 allowed warnings,
all only through the optional `keyring` feature (`--no-default-features` builds, such as the
container image, do not contain them): `zvariant`, `zvariant_derive` and `zbus_names` are unmaintained
(RUSTSEC-2026-0370/0371/0372), and `async-executor` 1.14.0 is yanked. `.cargo/audit.toml`
ignores RUSTSEC-2023-0071 (`rsa`): it is in `Cargo.lock` only because sqlx `any` names the
optional `sqlx-mysql` driver, and it is never compiled (`cargo tree -i rsa --target all` is
empty). It also ignores RUSTSEC-2026-0459 (bundled SQLite before 3.54.0: a crafted database
file and its `-wal` file can make queries return stale process memory). No fix can be
installed yet: the fix is in libsqlite3-sys 0.39, and sqlx 0.9.0 requires a version below
0.38. pulp-tui is exposed only when it opens a SQLite file that a less trusted party can
write (see `.cargo/audit.toml`).

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

* Plugins other than RPM, File and Deb; creation of signing services; OIDC token refresh.
* Deb: dependency solving on copy; fallback replication of source packages. Deb Content
  View publications are never reused, and a Content View never makes a verbatim
  publication.
* The signing service pickers list the services of all plugins (pulpcore has no type
  filter); Pulp refuses a wrong one.
* The **K** key cannot store the password of the top-level `[database]`. Use `env:` or
  `file:`, or create the keyring entry (service `pulp-tui`, user `[database]`) with the
  system tool. A profile's own `database` works (user `[database]:<profile>`).
* `nested_by_digest` RPM layout: served-content checks verify metadata only.

### Not tested

* Pulp domains (the Domains screen is tested only on a server with domains off), macOS at
  runtime (CI builds and runs unit tests), Windows.
* The shared-database TLS path (private CA, non-loopback host) only by hand. `ca_cert`
  adds to the built-in Mozilla roots; the OS store is not used.
* Two TUI sessions on one shared database. No test or manual run drives two sessions at
  once. The service tests (`two_operators_share_state_and_locks`) cover the lock and the
  shared state.

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
