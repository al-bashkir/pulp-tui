# pulp-tui — functional coverage and async consistency audit

Date: 2026-11-10 (audit 8). Revision: `v0.6.0` (`git describe`), `f980f36` (`main`, clean tree).
Audit only: no source, test, doc or config change. This file is the only output.

This is a re-audit of audit 7 (2026-11-09, of `a0aaca9`). The three commits after it (this file, the 0.6.0 version
number, one test helper) fix no finding, so F66–F97, G19, G20, T05 and T06 are all still open; section 6.1 gives the
status of each one. The work of this audit went into what the seven audits before it missed, with other methods than
theirs (below). New findings continue the numbering: F98–F132, G21, G22 and T07, so an ID never means two things. The
findings of the older audits (F01–F65, E01–E03, G01–G18, T01–T04) stay as recorded: fixed, with G03–G18 open by the
user's decision.

## 1. Executive summary

**Earlier findings.** None is fixed (no code change). Two that were static are now reproduced on a server with
domains: F89 [R36] and F90 [R37]. F67 has a second entry point, and F94 two more lines (section 6.1).

**Suite.** The full test suite passed on fresh containers at `f980f36`: 685 unit, 53 mock HTTP, 162 live and 3 `real`
tests on pulpcore 3.123.1, every call exit 0, 2757 s (section 8).

**What this audit did that the earlier ones did not.** It ran the TUI against three kinds of server that no earlier
audit and no test uses: a server with domains, a server with the default `CONTENT_ORIGIN` (not set), and a 3.63.36
server for the form paths (the suite on old images creates most objects through the API). It swept every form
field against the serializers of the three supported images, compared the OpenAPI paths with the client by script,
and reviewed selection identity and Connect in the shell. 43 candidates came out; the lead reproduced 18 at runtime,
and three reviewers with no earlier context tried to disprove the others.

**Result: 35 new findings (11 P2, 24 P3), 2 new gaps. No P0, no P1.** 17 findings are runtime verified. The P2
findings:

* **F98 [R48]** — After **Connect** from a sub-list, the list of the previous server stays while "connecting...". A
  form opened in that window is sent to the new server: the role of "user a8-x1" went to another user with the same
  number on the other server, and the result said "assign … to user a8-x1: done".
* **F107 [R60]** — A **filtered RPM Content View publish** after the source replaced a modulemd: the preview said
  "+0 added, -0 removed"; the version that became active in Library had none of the selected packages. Pulp removes
  the packages of a removed modulemd between pulp-tui's remove request and its add request.
* **F99 [R44], [R45]** — On a server with the default **`CONTENT_ORIGIN`** (not set; so in pulpcore 3.95.3 and 3.123.1, not in 3.63.36), `base_url` is
  relative: every Verify is MISMATCH, every environment is DRIFT with the hint "repair the snapshot", the Rollback
  check and fallback replication fail. **F105** (static): the same for object storage, where the content app answers 302.
* **F100 [R46]** — **Remote Delete** says "Repository content and versions are not affected". Pulp deletes the
  remote artifacts with the remote: on-demand content that was not downloaded yet answers 404 from then on.
* **F101 [R52]** — **Native replication** with policy `nodelete` ("never delete local objects", confirm `y`) took
  over a local repository and distribution that had the name of an upstream distribution: content replaced, labels
  replaced, distribution re-pointed.
* **F102 [R49]**, **F103 [R50]**, **F104 [R58]** — on a **3.63.36** server: every Deb repository Create from the
  form fails (`autopublish` is sent with no capability check); every RPM repository Edit erases `repo_config` (a
  pulp_rpm 3.27 quirk that the TUI does not know); Deb fallback replication from 3.11 into an older pulp_deb fails
  after the sync, on every rerun.
* **F106** (static) — an upload into a repository with a **package signing service** ends in "digest mismatch",
  though Pulp signed and added the package.
* **F109 [R62]** — for a user that is **not a superuser**, the Delete and Edit impacts say "No distribution uses
  it" from lists that Pulp scopes to the user. A guard owner deleted a guard behind that text, and the distribution
  that used it went from 403 to public.

P3 findings reproduced at runtime: an empty role description is refused (F110); a config save removes what another
process added to the file (F111); File Content search is 400 on the older images (F112); Verify of a guarded
distribution says MISMATCH (F113); the Task Center filter hides nothing (F114); the header shows the edited URL while
requests go to the old server (F115); the delete of an import task is a bare 500 (F116); Remove of a modulemd takes
its packages with it and counts one unit (F117). 15 more P3 findings are from code reading (F108, F118–F132).

**New gaps.** G21 [R43]: an uploaded Deb source package is in no `Sources` index of a structured publication,
because the TUI cannot create a source package release component. G22: content create from a URL (`file_url`).

**Rows that are verified now.** Domains (create, edit, migrate storage, delete), Copy URL (OSC 52), the Artifacts
screen, the `z` dismiss, the Dashboard reload on a task end, and a host switch while a task runs were ❓ until
audit 7. They are proven now ([R35]–[R42]). One ❓ is left: a sync from a ULN remote (needs an account).

**Task model.** It is still sound: only `completed` is success, dependent steps wait for their tasks, no mutation is
retried, a `202` is never shown as done, and a task that ends on another screen or across a host switch is shown
with its final state. The new async findings are of three kinds: state of the previous server that survives a
Connect (F98, F115); a check after a completed task that reports a wrong result (F99, F105, F106, F113); and two
`modify/` tasks between which Pulp changes the version (F107).

### Coverage summary (from the matrix in section 10)

Cells: ✅ / ❌ / ❓ / — (not applicable). `—` is not in any coverage ratio, and `❓` is never counted as a success.

| Plugin / area | Rows | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent |
|---|---:|---|---|---|---|---|
| `pulp-tui` | 42 | 0 / 0 / 0 / 42 | 42 / 0 / 0 / 0 | 42 / 0 / 0 / 0 | 38 / 4 / 0 / 0 | 22 / 1 / 0 / 19 |
| `pulp_deb` | 36 | 35 / 1 / 0 / 0 | 34 / 1 / 0 / 1 | 34 / 1 / 0 / 1 | 29 / 5 / 0 / 2 | 27 / 1 / 0 / 8 |
| `pulp_file` | 48 | 47 / 1 / 0 / 0 | 47 / 1 / 0 / 0 | 48 / 0 / 0 / 0 | 43 / 5 / 0 / 0 | 36 / 1 / 0 / 11 |
| `pulp_rpm` | 46 | 46 / 0 / 0 / 0 | 45 / 1 / 0 / 0 | 45 / 1 / 0 / 0 | 38 / 6 / 1 / 1 | 36 / 2 / 1 / 7 |
| `pulpcore` | 75 | 72 / 2 / 0 / 1 | 56 / 17 / 0 / 2 | 56 / 17 / 0 / 2 | 52 / 4 / 0 / 19 | 41 / 0 / 0 / 34 |
| **All** | 247 | 200 / 4 / 0 / 43 | 224 / 20 / 0 / 3 | 225 / 19 / 0 / 3 | 200 / 24 / 1 / 22 | 162 / 5 / 1 / 79 |

Reading the numbers (counted from the rows of section 10 with a script):

* **Rows:** 247 (audit 7: 236). The 11 new rows: G21, G22; one row for each case where a supported server version
  or set-up behaves in another way (F102, F103, F104, F112, F106 for RPM and for Deb, F105, F109); F107.
* **API client:** the 20 ❌ rows are the gaps G03–G18 (decision), G19, G20, the new G21 and G22, and the File search
  filter that the older servers do not have (F112).
* **Works end-to-end:** 200 of 225 applicable rows are proven through the TUI against real Pulp. 24 ❌ rows (audit 7:
  7): the 7 of before (F66, F76 ×3, F77 ×2, F78) and 17 new ones: F99 (Verify ×3, drift), F100 (Remote delete ×3),
  F101, F102, F103, F104, F105, F106 (×2), F107, F109, F112. 10 of the 17 have a runtime proof; 7 say "static" in
  their row (F99 for RPM and Deb, F100 for RPM and Deb, F105, F106 ×2 — the last three with no runtime proof for
  any type). 1 ❓ row (audit 7: 5): the ULN sync.
* **Async state consistent:** 162 proven, 5 inconsistent, 1 not proven (audit 7: 157 / 2 / 4). The ❌ rows: the two
  ACS refresh rows (F77), Connect (F98), and the two signing-upload rows (F106, static).
* The rules for ❌ are at the top of section 10. The 24 ❌ do not mean that 24 features fail on the tested `latest`
  server with its test settings: there, the suite and the earlier runs still pass. They are failures on another
  supported version, on the default content origin, or behind a text that says the opposite of what Pulp does.

## 2. Scope and environment

### Repository and tools

* pulp-tui `v0.6.0` (`f980f36`), one Rust binary, about 43,000 lines in `src/` (unit tests in the same files).
  Fedora 44, Linux 7.3. Against `a0aaca9` (the revision of audits 6 and 7) the source differs only by
  `session_log::clear()` (`src/session_log.rs:115-119`, a helper for one live test), the version number and the CI
  timeout: `git diff a0aaca9..HEAD -- src tests Cargo.toml .github`. Later lines of `src/session_log.rs` moved down
  by 5; every other line number of the earlier findings is still valid.
* Tests: `cargo test` (unit + `tests/mock_http.rs`), live tests `tests/live_*.rs` through `tests/run-live.sh`.

### Supported plugins (from code and UX)

| Plugin | Status | Evidence |
|---|---|---|
| pulp_rpm | active | `RepoType::Rpm` (`src/pulp/models.rs`), `src/pulp/rpm.rs`, screens in `src/tui/kinds.rs` |
| pulp_file | active | `RepoType::File`, `src/pulp/file.rs` (File and Git remotes) |
| pulp_deb | active | `RepoType::Deb`, `src/pulp/deb.rs` (also installer, source, generic content and release records) |
| pulp_certguard | active (content guard types only) | `GUARD_TYPES` in `src/tui/actions_admin.rs` |
| pulpcore management | active | `src/tui/actions_admin.rs`, `src/tui/actions.rs` (tasks, versions, orphans, repair) |
| container, python, ansible, gem, maven, npm, hugging_face, ostree | out of scope | installed on the test server, never used by pulp-tui; not in the coverage numbers |

No unused generated client or scaffolding: the REST layer is hand-written. Two helpers have no caller
(`all_content_keys`, `content_keys_except`); they are dead code, not a missing TUI path.

### Pulp versions

| Source | pulpcore / pulp_file | pulp_rpm | pulp_deb | Notes |
|---|---|---|---|---|
| Suite of this audit (`quay.io/pulp/pulp:latest`, fresh containers pulp1, pulp2) | 3.123.1 | 3.39.5 | 3.11.1 | certguard 3.123.1; `domain_enabled: false`; read from `status/` in this session |
| Runtime runs of this audit, server `dom` (own container of the `latest` image, `DOMAIN_ENABLED = True`) | 3.123.1 | 3.39.5 | 3.11.1 | used for the domain rows, F89, F90 and most new findings; restarted once without `CONTENT_ORIGIN` (F99) |
| Runtime runs of this audit, server `old` (own container of the `3.63.36` image) | 3.63.36 | 3.27.12 | 3.5.4 | used for F102, F103, F104, F112 and F113: each claim about this version was run on it |
| Source read in this audit (copied out of the three images to a scratch folder) | 3.123.1 / 3.95.3 / 3.63.36 | 3.39.5 / 3.34.0 / 3.27.12 | 3.11.1 / 3.8.1 / 3.5.4 | every Pulp line number in section 6.2 is from these copies; no suite and no runtime run on 3.95.3 in this audit |
| Audits 5 and 6 (code equal to this revision but for the test helper) | 3.123.1, 3.95.3, 3.63.36 | | | every scenario and `real` passed on all three |

### Test hosts

`http://127.0.0.1:8080` (pulp1) and `http://127.0.0.1:8081` (pulp2), local podman containers from
`tests/env/pulp-env.sh`. No container ran before this audit; `down-all` ran at the start of the suite and at the end
of the audit. User `admin` with the test-only password of the script. PostgreSQL 16 on `127.0.0.1:55432`. Fixture
HTTP server on port 8000. Two more containers were made for the runtime runs, with their own names, ports, volumes
and settings files in the scratch folder, and removed at the end: `dom` (`127.0.0.1:8090`, `latest`, domains on) and
`old` (`127.0.0.1:8091`, `3.63.36`). The runtime runs used their own config file and data directory in the scratch
folder (profiles for pulp1, pulp2, `dom` in two domains, and `old`; secret `env:PULP_PASSWORD`). The suite ran on
pulp1 and pulp2 at the same time; the only audit object on them was one user on pulp2 (made and deleted for F98).
No secret appears in this report.

### Assumptions and limits

* "Works end-to-end ✅" needs a TUI-driven proof against a real server: a live test that calls `run(&mut app, Act::…)`
  (`tests/common/mod.rs`), or a runtime run in a PTY. Service-level live tests and mock tests give ❓. A ❌ needs a
  runtime proof, except where the row says "static".
* Content Views, environments, fallback replication and repository definitions are pulp-tui concepts, not Pulp
  APIs: `pulp-tui` rows with `—` in the Pulp API column.
* Not verified: ULN sync (needs an account), a TLS-terminating outer proxy (F92), object storage (S3, Azure, GCS:
  F105 is static), a package signing service (F106 is static), macOS/Windows, two TUI sessions on one shared
  database. A user that is not a superuser was used for one run only (F109, [R62]); F121 is static.
* Domains are verified in this audit for the first time (server `dom`). The domain rows of the matrix were ❓ in
  audits 2 to 7.
* A finding that names an older server version was run on that version (`old`, 3.63.36) or is marked static. No run
  was made on 3.95.3: where a finding names 3.95.3, the evidence is the plugin source of that image.

## 3. Architecture and feature inventory

### Path from key to HTTP request

`src/tui/mod.rs` (event loop: keys, job messages, 250 ms tick) → `App::handle_key` (`src/tui/app.rs`) → action menu
`App::available_actions` (filters read-only, item needs and unsupported screens) → `actions::run`
(`src/tui/actions.rs:427`, dispatch on `(Kind, Act)`) → handler (`repo_action`, `remote_action`, `acs_action`,
`version_action`, `content_action`, `publication_action`, `distribution_action`, `task_action`, `profile_action`;
`actions_cv::run`; `actions_admin::run`) → form / impact job / confirm → `App::job` (a Tokio task) → `PulpClient`
(`src/pulp/client.rs`: auth, TLS, retries for GET only, redirects off, size limits, same-origin hrefs) → Pulp. The job
returns an `Apply` closure that changes `App` on the UI thread.

### Task handling

* `submit` / `submitted` (`actions.rs:26-51`): a task or task group goes to the Task Center; a synchronous answer
  shows "done" and reloads the list. `follow` (`actions.rs:54`) runs a callback only on `completed`.
* Inline waits: `ops::wait_task` (only `completed` is success), `ops::settle` (a group needs every task completed),
  `ops::wait_task_group`. Each driven update goes to the Task Center through `Ctx::on_task`.
* Task Center (`src/tui/tasks.rs`): adaptive polling (`poll_min_ms` ×1.5 up to `poll_max_ms`, ±20 % jitter, at most
  three polls at a time); references stored in the local file by profile name (F93) and re-attached at start and
  Connect; 404 → `gone`, 401 → `unauthorized`. A task group with no tasks is final as `gone` (`TaskGroup::purged`,
  `models.rs:505`), which F77 shows to be wrong for an empty refresh.
* Refresh: when a tracked task ends, `task_finished` reloads the visible list or the Dashboard; `open`, `push`,
  `replace` and `back` always reload; a stale load is dropped by generation; a failed operation job reloads.

### Screens, actions and code locations

| Area | Screens (`Kind`) | Actions | Handler | Client |
|---|---|---|---|---|
| Repositories RPM/File/Deb | `Repos(t, snapshots)` | Versions, Content, Sync, SyncHistory, Upload, Publish, Distribute, CopyContent, Reclaim, Create, Edit, Delete, ObjectRoles, Inspect; RPM also UploadComps, UploadAdvisory, CreateModule, PrunePackages | `repo_action` (Edit `repo_edit`, Distribute `distribute_form` `actions.rs:2257`, Reclaim `reclaim` `:1995`) | `ops.rs`, `rpm.rs`, `deb.rs`, `file.rs` |
| Remotes (RPM, RPM ULN, File, File Git, Deb) | `Remotes(t, alt)` | Create, Edit, Delete, ObjectRoles, Inspect | `remote_action` | generic POST/PATCH/DELETE |
| Alternate content sources | `Acs(t)` | Refresh, Create, Edit, Delete, ObjectRoles, Inspect | `acs_action`, `acs_edit` (`actions.rs:2700-2760`) | generic |
| Versions | `Versions(repo)` | Content, Publish, Distribute, Rollback, Restore, Compare, Repair, Scan (RPM), Delete, PruneOrphans, Inspect | `version_action` | `ops::restore_version`, `repair_version`, `rpm::scan` |
| Content | `Content{version, ctype}` (RPM 11, File 1, Deb 13 types) | Inspect, RemoveContent, AddToRepo; Deb release records also Create | `content_action` | `ops::remove_content`, `add_content`, `deb::copy` |
| Publications | `Publications(t)` (Deb: APT + verbatim merged) | Create, Distribute, Delete, ObjectRoles, Inspect | `publication_action` | `ops::create_publication` |
| Distributions | `Distributions(t)` | CopyUrl, Verify, Rollback, Create, Edit, Delete, ObjectRoles, Inspect | `distribution_action` | `ops::ensure_distribution`, `distribution_target`, `verify_served`, `rpm::verify` |
| Content Views, versions, environments | `ContentViews`, `CvVersions`, `Environments` | 18 / 7 / 4 actions | `actions_cv.rs` | `src/cv.rs`, `src/store.rs` (9 migrations), `src/filters.rs` |
| Replication | `UpstreamPulps` | Replicate, ReplicationHistory, FallbackReplicate, Create, Edit, Delete, ObjectRoles, Inspect | `upstream_action` (`actions_admin.rs`) | `src/replication.rs` |
| Export / import | `PulpExporters`, `FsExporters`, `Exports`, `Importers`, `Imports` | Export, Exports, Import (check first), Imports, CRUD | `actions_admin.rs` | `ops::import_check`, `pulp_import` |
| Users, groups, roles | `Users`, `UserRoles`, `Groups`, `GroupUsers`, `GroupRoles`, `Roles` | CRUD, Password, ToggleActive, Members, Roles | `actions_admin.rs` | generic |
| Object roles | Remotes, ACS, Repos, Publications, Distributions, Guards, Upstreams, Groups, Server tasks, Uploads, Task schedules | ObjectRoles | `actions_admin.rs` | `list_roles/`, `add_role/`, `remove_role/`, `my_permissions/` |
| Content guards | `ContentGuards` | GuardAccess (RBAC), Create (7 types), Edit, Delete | `actions_admin.rs` | generic |
| Domains, access policies, uploads, schedules, artifacts, workers | own screens | MigrateStorage, ResetPolicy, Delete, WatchTask, RepairServer, Inspect | `actions_admin.rs`, `actions.rs` | generic |
| Tasks | `Tasks`, `ServerTasks(filters)`, `TaskGroups`, `Workers` | WatchTask, Inspect, CancelTask, DismissTasks, TrackTask, Filter, Delete, PurgeTasks | `task_action` | `ops::get_task`, `wait_task` |
| Servers | `Profiles` | Connect, TestProfile, CRUD, StoreSecret, ExportCv, ImportCv, ExportRepos, ImportRepos | `profile_action` | `src/config.rs`, `src/repo_export.rs` |

Registry check (this session): `src/tui/kinds.rs` is unchanged since audit 5. A reviewer read each `(Kind, Act)` pair
of `Kind::actions` (`kinds.rs:911-998`) against the dispatch in `actions::run` (`actions.rs:427-462`): every action
has an arm that does real work, and no arm is unreachable except `MigrateDb` / `CopyDb` (offered on Connect, on
purpose). `Act::mutates` treats an action as mutating unless it is listed as read-only, and the client itself refuses
POST, PATCH, PUT and DELETE for a read-only profile (`client.rs:287-342`), so a form or confirm closure cannot send a
mutation either. One write is outside that guard: the Library row of the shared database (F128).

**OpenAPI cross-check (this session).** The live `docs/api.json` of pulp1 (3.123.1) has 1200 operations; 596 are for
pulpcore, certguard, rpm, file and deb (a script over the paths; audit 7 counted 598 with another filter). The 101
collection paths were compared with the path strings in `src/` by a script; the 111 per-object paths (without the
role and label sub-paths) were compared by hand with the matrix rows. Every operation maps to a
matrix row, to a G gap, or to an `ObjectRoles` screen (`list_roles` / `add_role` / `remove_role` / `my_permissions`
also on tasks, uploads and task schedules). Collections that `src/` never names: the OpenPGP ones (G05),
`distributions/core/artifacts/` (G06), `datarepair/7272/` and `livez/` (G13), `docs/api.yaml`, and Pulp's
`content/file/files/upload/` and `content/rpm/packages/upload/` (the TUI posts to the content collection, which is
equivalent). The check found one gap that the earlier audits missed: `POST content/deb/source_release_components/`
(G21: the record Create of the TUI has four of the five release record types, `deb::RECORDS`, `src/pulp/deb.rs:355-356`).
A reviewer added `file_url` on content create (G22). `DELETE login/` (logout) has no use with Basic auth. Deb content
collections with POST that the TUI does not post to on purpose: `package_indices`, `source_indices`,
`installer_file_indices`, `release_files` (index files of a synced repository; a verbatim publication serves them).

## 4. Missing Pulp → client features

G03–G18 are unchanged since audit 3 and open by the user's decision (known gaps, built on demand only). G19 and G20
are from audit 7 and still open. G21 and G22 are new.

| ID | Plugin | Pulp operation | Notes |
|---|---|---|---|
| G03 | pulp_rpm | Advanced copy: several `config` entries in one task, `dest_base_version`, `dependency_upgrade` | The TUI sends one entry; Content Views use `dest_base_version` internally |
| G04 | pulpcore / all | `modify/` with `base_version` plus add/remove in one call | Only Restore uses `base_version` |
| G05 | pulpcore | OpenPGP keyrings and content (`repositories/core/openpgp_keyring/`, `content/core/openpgp_*`, `distributions/core/openpgp/`) | No reference |
| G06 | pulpcore | Artifact distributions (`distributions/core/artifacts/`) | No reference |
| G07 | pulpcore | Access policy edit (statements, creation hooks, queryset scoping) | Left out on purpose (USER_GUIDE "Access Policies") |
| G08 | pulpcore | Domain labels (`set_label/`), `default_content_guard`, rename | Domain edit sends description and two flags only |
| G09 | pulpcore | Delete of an import record; `create_repositories` on Pulp import | Imports screen is Inspect only |
| G10 | pulpcore | Per-request `q_select` on `{upstream}replicate/` | The TUI posts `{}` |
| G11 | pulpcore | Task filters `state__in`, worker, `task_group`, `parent_task`, `created_resources`, finished/created ranges; `{task}profile_artifacts/` | Filter form has state, name, reserved resource, start time |
| G12 | pulpcore | Orphan cleanup scoped by `content_hrefs` | Only the age is sent |
| G13 | pulpcore | `datarepair/7272/`, `livez/` | `status/` is used for health |
| G14 | pulpcore | Server-wide content search (`content/`, generic `publications/`, `distributions/`, `repositories/` lists) | `content/` is read for counts only; `distributions/` is read for the overlap check |
| G15 | pulpcore / all | Distribution bound to a `repository_version` | Forms pin a publication or follow the repository; Verify and the left-out step now read the field (F60) |
| G16 | pulpcore | Artifact upload and delete | Delete is deprecated and refused by the default policy |
| G17 | pulpcore | `vuln_report/` list and delete as its own screen | Read only inside RPM Scan |
| G18 | pulpcore | `reclaim_space` of several repositories in one task | `ops::reclaim_space` takes one repository |
| G19 | pulpcore / all content types | `{content}set_label/`, `{content}unset_label/` (content labels) | In the 3.123.1 OpenAPI for every RPM, File and Deb content type. The Content screens show no labels and cannot set them. Low demand; capability-gate it if built |
| G20 | pulpcore | `PATCH login/` with `old_password` / `new_password` (`LoginUpdateSerializer`, `viewsets/user.py:454-461`; allowed to any authenticated user) | The client only reads `GET login/` (`src/pulp/caps.rs:116`). The Users screen needs the default admin-only policy, so a normal user cannot change their own password in the TUI. Not in pulpcore 3.95.3 (no `patch` on `LoginViewSet`); gate on `has_operation` |
| G21 | pulp_deb | `POST content/deb/source_release_components/` (`source_package`, `release_component`; `SourcePackageReleaseComponentSerializer`, pulp_deb 3.11.1 `app/serializers/content_serializers.py:1394-1414`; in the 3.123.1 OpenAPI) | New, **runtime verified** [R43]. The TUI uploads a `.dsc` (`deb::upload_source`) but its record Create knows only `releases`, `release_components`, `release_architectures` and `package_release_components` (`deb::RECORDS`, `src/pulp/deb.rs:355-356`). A structured APT publication (the default mode) lists only source packages that have this record (`pulp_deb/app/tasks/publishing.py:310-350`); simple mode lists all (`:181-184`). Run: `.deb` uploaded into `stable` / `main`, then `fixture-src_1.0-1.dsc` ("new version 2"), Publish with Simple and Structured, Distribute: `dists/stable/main/source/Sources` is 200 with 0 bytes; `dists/default/all/source/Sources` lists `fixture-src`. The upload result and `docs/USER_GUIDE.md:222-229` do not say it. Smallest fix: add the fifth type to `deb::RECORDS` with a form like the package release component one |
| G22 | pulpcore / all content types | Content create from a URL: `file_url` on `POST content/<type>/` (pulpcore `plugin/serializers/content.py:42`; 3.95.3 `:43`; 3.63.36 `:41`; in `file.FileContent`, `rpm.Package`, `deb.Package` of the OpenAPI), with `downloader_config` on 3.123.1 only (`:48`) | New (static). The upload forms take a local path only. Low demand; `pulp_labels` on create belongs to G19 |

Pulp unavailable (not a pulp-tui defect): signing-service create (no API), task-schedule create/edit (GET only),
Deb copy with dependency solving (pulp_deb raises `FeatureNotImplementedError`), Pulp export `dry_run` (accepted but
ignored by pulpcore 3.123.1), the repository list of an export before the import (F28 limit), the
`architecture_variant` of a Deb package (a model field and part of the duplicate key in pulp_deb 3.11.1
`models/repository.py:283`, but not in `deb.PackageResponse` of the live OpenAPI).

## 5. Client → TUI exposure gaps

The client layer has no orphan operation (two dead helpers, section 2): no client method waits for a TUI path. The
reverse case exists once: `ops::sees_everything` (`src/pulp/ops.rs:478-484`) tells whether the user sees all objects,
but only the Content View left-out step uses it; the Delete impacts of the Pulp screens do not (F109). One screen
state is reachable that no screen list offers: a parameterised sub-list of the previous server after a Connect (F98).

## 6. Broken or incomplete TUI workflows

Evidence: **runtime** = PTY run of the real binary against a real Pulp server (section 8: [R35]–[R62] are of this
session; lower numbers are of the earlier audits at the same code); **static** = code reading, with the Pulp behavior
checked in the plugin source of the image where named.

### 6.1 Status of the findings of the previous audit

The previous audit (audit 7) left F66–F75 and T05 (from audit 6) and F76–F97, G19, G20 and T06 (its own) open. No
commit after it changes the code of a finding: `git diff a0aaca9..HEAD -- src` has only `session_log::clear()`
(section 2). So **no finding is fixed**. This audit did not only repeat that: it ran two findings that were static
until now (F89, F90: both reproduced on a server with domains), and it found a second entry point for F67.

| ID | Pri | Status | Evidence in this audit |
|---|---|---|---|
| F66 | P2 | not fixed | `actions_admin.rs:505-531` unchanged; the new F109 is another cause of the same false "No distribution uses it." |
| F67 | P3 | not fixed, wider | `distribute` unchanged. New: the New / Edit distribution form has the same order (`save_distribution` `actions.rs:4002-4011` → `binding_fields` `:3986-3999` → `pinned_publication` `:3994` before the POST or PATCH; no `ops::overlapping_distribution` first). Fix both entry points together |
| F68–F73 | P3 | not fixed | files unchanged |
| F74 | P3 | not fixed | `tasks.rs:125-142`, `store.rs:677-686` unchanged |
| F75 | P3 | not fixed | `role_form` unchanged; the new F110 is in the same function |
| F76 | P2 | not fixed | `reclaim` text `actions.rs:2018` unchanged |
| F77 | P2 | not fixed | `acs_paths`, `acs_edit` unchanged |
| F78 | P2 | not fixed | `row_objects` `cv.rs:2055-2084` unchanged |
| F79–F88 | P3 | not fixed | files unchanged |
| F89 | P3 | not fixed, **now runtime verified** | [R36] on a server with domains: guard `a8-g` is the `default_content_guard` of domain `a8x`; Delete says "Delete content guard a8-g (core/header). No distribution uses it."; after the typed name the domain has `default_content_guard: null` |
| F90 | P3 | not fixed, **now runtime verified** | [R37] the profile's domain is `a8x`; Domains → `a8x` → `d`: the impact has no line about the profile; after the typed name "task completed: delete domain a8x", then the Domains title says "error: not found [404]" over the old rows and every other screen says "Error: not found [404]" |
| F91–F93 | P3 | not fixed | files unchanged |
| F94 | P3 | not fixed, wider | also `path.is_file()` in two form submit closures (`actions.rs:1695`, `:1840`): a `stat` on the UI thread |
| F95–F97 | P3 | not fixed | files unchanged |
| G19, G20 | — | open | no client method |
| T05, T06 | P3 | not done | no new test |
| Polish of audits 6 and 7 | P3 | not done | unchanged; the Logs cut is now at `session_log.rs:233-234` |

The texts of the open findings follow unchanged, so that this file stays the only reference. Their line numbers are
still valid.

#### Open findings of audit 6

#### F66 — P2 — Content guard Delete does not name distributions behind nested composites (runtime)

* Where: the Delete impact (`actions_admin.rs:505-531`) lists the distributions that use the guard directly, and for
  each composite that has the guard as a direct member: "Composite guard X loses its only guard: a composite without
  guards allows everyone", plus the distributions of X. It does not follow X into a composite Y that contains X.
* pulpcore 3.123.1: `CompositeContentGuard.permit` allows when it has no guards; a member can be another composite
  (the serializer takes any guard type). `docs/COMPATIBILITY.md:577-582` says a guard change "changes access at the
  distributions of each composite that contains it, at any depth". The Edit follows that chain (`guard_reach`
  `actions_admin.rs:671-684`); the Delete does not use it.
* Runtime [R26]: header guard `audit6-h` → composite `audit6-x` [h] → composite `audit6-y` [x] → distribution
  `audit6-g`. Without the header, `audit6-g` answered 403. Content Guards → `audit6-h` → `d`: "No distribution uses
  it. Composite guard audit6-x loses its only guard: a composite without guards allows everyone" — `audit6-g` not
  named. After the typed name: "[ok] delete content guard audit6-h: done"; `audit6-g/` and `audit6-g/PULP_MANIFEST`
  answered 200 without the header.
* The same Pulp behavior on pulpcore 3.95.3 (API only, fresh container of section 8): header guard in composite in
  composite on a distribution; `PULP_MANIFEST` 403 before, 200 after the DELETE of the header guard.
* Why P2: a destructive change with an impact that says the opposite of what happens ("No distribution uses it"),
  and the result is content open to anyone. The typed confirm and the "allows everyone" line are there, so an
  operator who reads the composite line can find out; nested composites are rare.

#### F67 — P3 — Distribute to a new distribution at an overlapping base path makes a publication first (runtime)

* Where: `distribute` (`actions.rs:2354-2362`) → `pinned_publication` (`:2371-2381`) → `ops::ensure_publication`
  (POST and task wait) before `ops::ensure_distribution` → `distribution_target` → `refuse_overlap`
  (`ops.rs:448-457`). `confirm_distribute` (`:2312-2314`) runs the job at once for a new name and does not call
  `distribution_target`, which only reads.
* Runtime [R24]: distribution `audit6-outside2` at `audit6-d/sub`; `D` on `audit6-d` (no publication) with the
  defaults: "[error] distribute audit6-d: conflict [409]: base path audit6-d overlaps base path audit6-d/sub of
  distribution audit6-outside2". Publications of `audit6-d`: 0 before, 1 after; no distribution made.
* The error is clear and nothing is served. The publication stays; for RPM with a metadata signing service each try
  makes a new signed publication (`ops.rs` publication reuse does not match a signed one).

#### F68 — P3 — Distribute in follow mode onto a checkpoint distribution says what it does not do (runtime)

* Where: `confirm_distribute` refuses a checkpoint distribution only for a pin (`actions.rs:2319-2321`);
  `Binding::line` says "the newest publication of repository X (follows it)" (`:2322`); `follow_repository` PATCHes
  `{"repository": X, "publication": null}` (`:2410`) and leaves `checkpoint: true`.
* Runtime [R25]: `audit6-c` was a checkpoint distribution of repository `audit6-c`. `D` on `audit6-b`, name and base
  path `audit6-c`, Binding "follow repository latest": the confirm said "now serves: 01a1c0d1@v3 (checkpoint
  distribution …) / will serve: the newest publication of repository audit6-b (follows it)"; after the typed name
  "[ok] distribution audit6-c serves http://…/audit6-c/". Pulp: `checkpoint: true`, `repository` = `audit6-b`; the
  base path is an empty checkpoint index (200), `PULP_MANIFEST` 301; `audit6-b` has no publication at all.
* Fix direction: refuse a checkpoint distribution in follow mode too (as for a pin), or say "checkpoint distribution:
  serves the checkpoint publications of X at <base path>/<timestamp>/".

#### F69 — P3 — A derived-name collision with another view makes the whole Publish and Resume preview fail (static)

* Where: `bind_target` calls `self.named_distribution(cv, env, repo).await?` (`cv.rs:1466`); `named_distribution`
  bails for a distribution with the labels of another view or environment (`cv.rs:1447-1457`). `refused_lines`
  (`actions_cv.rs:889-897`), `library_lines` (`:860-868`) and the Publish preview pass the error up, so the preview
  job ends in an error and no confirm opens. The run would publish and fail only for that repository
  (`library_step`).
* Scenario (the code comment at `cv.rs:1445-1446` names it): view `web` with repository `b-c`, and view
  `web-Library` promoted to environment `b` with repository `c`, both derive `pulp-tui-cv-web-Library-b-c`. Publish
  `web` from the TUI is then impossible. Before 588ba95 the Publish preview did not call `named_distribution`.
* Fix direction: turn that error into the refusal text, as the 409 of `distribution_target` (`cv.rs:1473`).

#### F70 — P3 — The Resume confirm of a deleted version lists "In Library:" changes, then Resume refuses (static)

* Where: `resume_library_step` (`actions_cv.rs:883-886`) is true for every state except `published`; `resume`
  refuses a `deleted` or `deleting` version (`cv.rs:794-799`). Resume is on every Versions row.
* Effect: one confirm with lines of a step that never runs, then the error "version N was deleted". Nothing changes.

#### F71 — P3 — Verify does not report a distribution whose ownership labels were removed (static)

* Where: `verify_repo` (`cv.rs:1733-1748`) compares only what the distribution serves. Fix `Bind` → `bind_hint`
  says "promote vN again (M) to reconcile (idempotent)" (`actions_cv.rs:754-759`), but `bind_repo` →
  `ensure_distribution` → `owner_check` refuses an unlabeled distribution with 409 (`ops.rs:500-515`).
* Scenario: Dev serves v1; an operator removes the labels of `pulp-tui-cv-web-Dev-os` and points it at something
  else. Verify: DRIFT with "promote again"; the promotion ends partial for `os` (the Promote preview names the
  refusal since F55). With only the labels removed, Verify says OK.

#### F72 — P3 — The Publish preview misses two refusals of `publish` (static)

* `publish` refuses a source repository with no versions (`cv.rs:655`) and a source repository that is a Content
  View snapshot (`cv.rs:652-654`). The preview only shows "== X has no versions" (`cv.rs:1849-1851`) and does not
  check `pulp_tui_cv`. The confirm opens; the run then refuses before `begin_snapshot_version` (`cv.rs:669`), so
  nothing changes. Same kind as the fixed "view with no repositories" polish item.

#### F73 — P3 — Add, Copy and an overwriting upload replace same-key units with no word and no retention check (static)

* Where: `add_to_repo` / `add_run` (`actions.rs:3275-3322`) have no dialog; `copy_preview` (`:1916`) has an untyped
  confirm without a replacement line; the upload with Overwrite is documented only (`docs/USER_GUIDE.md`). None calls
  `latest_goes` (`actions.rs:366`). Remove content (`:3223`), Replace comps (`:1860`), the File mirror sync (`:317`)
  and the prune (`:2106`) do.
* Pulp: a new version keeps one unit per repository key and drops the other (pulp_file `relative_path`; pulp_rpm
  `name, epoch, version, release, arch, location_href`; pulp_deb `handle_duplicate_packages`). With
  `retain_repo_versions` 1 and an unserved current version, Pulp then deletes the current version, so the replaced
  unit is in no version and orphan cleanup can delete it.
* Scenario (File): `docs` has `a.txt` (sha X), Retain versions 1, no distribution serves its latest version. On
  another repository's Content screen, mark `a.txt` (sha Y) → `a` → `docs`: no dialog; the new version has Y, the old
  version is gone.

#### F74 — P3 — A late failed poll still changes a final Task Center row (static, the rest of F61)

* Where: `App` routes a poll error to `task_gone` (404), `task_unauthorized` (401) or `poll_failed`
  (`app.rs:584-588`); `Tasks::stop` and `Tasks::poll_failed` (`tasks.rs:125-142`) do not check `is_final`, and
  `finish_task` in the store has no `WHERE finished_at IS NULL` (`store.rs:677-686`).
* Scenario: a workflow reports `completed` while a Task Center poll of the same task is in flight, and that poll then
  fails: a 5xx or network error puts its text on the completed row; a 404 (purge) turns it into `gone` with "task not
  found (purged?)" and overwrites the stored `completed`; a 401 turns it into `unauthorized`.
* `docs/ARCHITECTURE.md:72-73` says a late poll answer does not change a final row. Low chance: the Task Center
  polls a driven task only after `2 × poll_max_ms` without a driven update.

#### F75 — P3 — A custom role Edit changes every holder's permissions with no impact and no confirm (static)

* Where: `role_form` (`actions_admin.rs:344-369`) submits the PATCH at once. The role Delete names the users and
  groups that have it (unit `role_delete_names_the_users_and_groups_that_have_it`); the content guard Edit now asks
  first for the same kind of access change (9676b8f).
* Effect: a removed permission breaks the work of every holder at once; an added one grants it to every holder.
  Pulp roles are not destructive data, so P3.

#### Audit-6 polish (still open)

* F54 confirm: "now serves" names the repository by its id prefix (`01a1c0ce@v3`), while "will serve" uses the name
  (runtime [R21]); the Publications list has the same `01a1c0ce@v3` column.
* F62 note: pulp_deb keys duplicates on `architecture_variant`, which its API does not return, so a build of another
  variant is reported as replaced (over-warning only).
* ImportRepos: the headline "N not created" counts the repository that waits for a skipped remote, but not the remote
  that was skipped for a missing required field (a "warning:" line). Audit 5 named the wrong one.
* Connect keeps `App::last_repo` (repository names by href) of the previous server (`app.rs:783-807`); it only
  matters for servers cloned from one database.
* Logs: a non-JSON body is cut at 64 KiB before the URL mask (`session_log.rs:228-229`), so user info that crosses
  the cut is not masked.
* Esc in a typed confirm drops the whole form (for example the description of the F53 edit, [R20]); the form must be
  filled in again.
* The footer cuts long refusal texts at the screen width; the notification list (`N`) has them all (kept from
  audit 5).

#### Open findings of audit 7

Root causes of the audit-7 findings, so that a fix closes a class and not one case:

* **RC1 — A confirm text states a Pulp rule that Pulp does not follow** (F76, F85, F86, F79): the text was written
  from the intent of the Pulp feature, not from its code. Each such text needs a test against the live server.
* **RC2 — An impact check works on a normalised form value instead of what Pulp stores** (F77: `""` dropped; F78:
  version 0 not filtered like `drop_unused_versions` does).
* **RC3 — Choices and refusals copied by hand from one plugin rule** (F81, F83, F84): the form offers what Pulp
  refuses, or a refusal covers RPM but not File.

#### F76 — P2 — Reclaim deletes the files of content that other repositories share; the confirm says the opposite (runtime)

* Where: `reclaim` (`src/tui/actions.rs:2018`): "Reclaim disk space of {name}: Pulp deletes the stored files of its
  synced content that no other repository has." The same claim is in `docs/COMPATIBILITY.md:367-369` and
  `docs/USER_GUIDE.md:273-274`.
* Endpoint: `POST repositories/reclaim_space/` (`repo_hrefs`, `repo_versions_keeplist`).
* Pulp (pulpcore 3.123.1, and the same lines in 3.95.3 and 3.63.36): `tasks/reclaim_space.py:39-43`
  `c_reclaim_qs.exclude(pk__in=c_keep_qs, pulp_type=PublishedMetadata.get_pulp_type())`. Two keyword arguments of one
  `exclude()` give `NOT (pk IN keep AND pulp_type = publishedmetadata)`; a verifier printed this SQL in pulp1. So only
  shared published metadata is kept. RPM packages and File units have `PROTECTED_FROM_RECLAIM = False`. The artifact
  belongs to the shared `ContentArtifact`, so *every* repository that has the unit loses the file. Uploads keep their
  files (`remoteartifact__isnull=False` without `force`).
* Runtime [R29]: File remote `a7-rem` (policy immediate, port 8011), repository `a7-src` synced (3 units),
  `a7-copy` given the same 3 units by `modify/`. File Repositories → `a7-src` → `R` → empty Keep versions → confirm
  "Pulp deletes the stored files of its synced content that no other repository has." → typed `a7-src` → "[ok] reclaim
  space of a7-src: deleted 3 file(s)". `content/file/files/?repository_version=<a7-copy v1>`: all three
  `"artifact": null`.
* Effect: a reclaim of a source repository strips the files from every promoted Content View snapshot (they share the
  units). Distributions still serve them on demand while the upstream has them; a package that left the upstream is
  lost for every repository.
* Smallest reproduction: the run above (any repository type; the task is pulpcore's).

#### F77 — P2 — ACS Edit of an ACS with no paths removes the path of every other ACS with no paths (runtime)

* Where: `acs_edit` (`src/tui/actions.rs:2711-2733`): `old` comes from `acs_paths(&x.initial)`, and `acs_paths`
  (`:2652-2654`) drops empty strings. Pulp returns `paths: [""]` for an ACS with no paths, so `removed` is empty and
  the PATCH goes out at once (`if removed.is_empty() { go(app) }`). The "Remove ACS paths" check that names other ACSs
  (`:2734-2760`, documented in `docs/COMPATIBILITY.md:781-782`) never runs for `""`.
* Endpoint: `PATCH acs/{file/file|rpm/rpm}/<id>/` with `{"paths": ["sub/"]}`.
* Pulp (3.123.1 `serializers/acs.py:97-104`, same in 3.95.3 and 3.63.36):
  `AlternateContentSourcePath.objects.filter(path__in=to_remove).delete()` has no ACS and no domain filter. The `""`
  row of every ACS with no paths is deleted. The File and RPM refresh loops over path rows (pulp_file
  `viewsets.py:741-778`, pulp_rpm `viewsets/acs.py:130-177`) and then dispatches nothing.
* Runtime [R30]: File ACS `a7-acs1` and `a7-acs2`, both `paths: [""]`. Alt. Sources → `t` (File) → `a7-acs2` → `e` →
  Paths `sub/` → `Ctrl-S`: no dialog, "[ok] task completed: update ACS a7-acs2". Pulp: `a7-acs2` `["sub/"]`,
  `a7-acs1` `[]`. `R` on `a7-acs1`: Pulp made the task group "Refreshing 0 alternate content source paths" with no
  task; the Task Center row is `gone`, "ERROR: its tasks were purged".
* Effect: every other RPM and File ACS with no paths stops being indexed, silently and for good; the TUI cannot add
  the `""` row back except by adding and removing a path. The Task Center message is false (nothing was purged).
* Deb is not affected (its form sends no paths, its refresh ignores them).

#### F78 — P2 — Deleting a Content View version can delete version 0 of a snapshot repository; every later unfiltered RPM or Deb publish fails (runtime)

* Where: `CvService::version_objects` → `row_objects` (`src/cv.rs:1959-1975`, `:2055-2084`) list the recorded
  `derived_version` whatever its number; `delete_objects` (`:1946-1954`) deletes it. `drop_unused_versions`
  (`:1133`) filters `v.number > 0` on purpose, and `docs/COMPATIBILITY.md:493` says "(not version 0)".
* How a version records `versions/0/`: an unfiltered snapshot copies onto base version 0 (`rpm::copy(.., Some(0), ..)`
  `cv.rs:994`, `deb::copy(.., empty.then_some(0), ..)` `:997`). When the copy changes nothing (empty source, or a
  filter that selects nothing), pulpcore's `RepositoryVersion.__exit__` deletes the new version, and `snapshot`
  records `latest_version_href` (`:1003-1012`), which is `…/versions/0/`.
* Pulp: no rule protects version 0 (only served versions and the last complete version). pulp_rpm
  `viewsets/repository.py:796-805` and pulp_deb `viewsets/repository.py:362-370` answer 400 "Version 0 does not exist
  for repository …" for `dest_base_version: 0` (same text in 3.95.3 and 3.63.36).
* Runtime [R31]: RPM repository `a7-os` (not synced). Content Views → `c` `a7cv` → `a` `a7-os` → `P`: preview "from
  …/versions/0/: 0 selected" → v1; the snapshot `pulp-tui-cv-a7cv-a7-os` stays at `versions/0/`. Sync `a7-os`; `P` → v2
  (3 packages, Library). Versions → v1 → `d`: "a7-os: delete …/pulp-tui-cv-a7cv-a7-os/versions/0/", typed `v1` →
  "[ok] version deleted"; the snapshot repository has only version 1. `P` → v3: "repository a7-os: rejected [400]:
  Version 0 does not exist for repository 'pulp-tui-cv-a7cv-a7-os'", v3 `failed`.
* Effect: the view cannot publish again for that repository; Resume repeats the same 400. The way out (add a filter
  rule, which uses `modify/`; or delete the view) is not said. When version 0 is the only complete version, Pulp
  refuses the delete task and the version stays `deleting`.
* File and filtered copies are not affected (they use `add_content`).

#### F79 — P3 — Distribute in follow mode says "serves" for a repository with no publication (runtime)

* Where: `follow_repository` (`actions.rs:2393-2417`) binds `repository` with no publication check; `distribute_form`
  (`:2287-2291`) then says "distribution {dist} serves {url}". pulpcore `models/publication.py:789-806`: no complete
  publication → 404.
* Runtime [R32]: `a7-src` (0 publications) → `D` → Binding "follow repository latest" → `Ctrl-S`: "[ok] distribution
  a7-src serves http://127.0.0.1:8080/pulp/content/a7-src/"; the base path and `r1.bin` answer 404.
* Fix direction: with no publication, say "no publication yet: publish (P) to serve" (or offer to publish).

#### F80 — P3 — The palette entry "Tasks: dismiss finished" does not forget the stored references (runtime)

* Where: the palette (`app.rs:1609-1615`) calls `a.tasks.dismiss_finished()` only. `App::dismiss_finished_tasks`
  (`:650-658`, used by `z` in `actions.rs:4021`) also calls `store.forget_finished_tasks` and the `z` path reloads.
  `reattach_tasks` reads the 50 newest rows, finished ones too.
* Runtime [R33]: `:` "Tasks: dismiss finished" → "[ok] dismissed 10 finished task(s)", Task Center 0 items; quit,
  start again → Task Center 10 items, the same rows. Contradicts `docs/ARCHITECTURE.md:78-79` and
  `docs/USER_GUIDE.md:967-968`.

#### F81 — P3 — The RPM Checksum type choice offers `sha224`, which Pulp refuses (runtime)

* Where: `enum_choices` (`actions.rs:966-970`) removes only `unknown|md5|sha1` from `PackageChecksumTypeEnum`; it
  feeds the repository form (`:986-991`) and the publication form (`:2185-2193`). pulp_rpm `constants.py:56`
  `ALLOWED_PUBLISH_CHECKSUMS = {sha256, sha384, sha512}` (same set in 3.95.3 and 3.63.36).
* Runtime [R34]: RPM Repositories → `a7-os` → `e` → Checksum type cycles `sha224, sha256, sha384, sha512` → `sha224` →
  "[error] update repository a7-os: rejected [400]: checksum_type: Checksum must be one of the allowed checksum types:
  sha256, sha384, or sha512." The unit test of `enum_choices` passes only because its fixture enum has no `sha224`.

#### F82 — P3 — RPM `nested_by_digest`: Verify, Rollback, drift and fallback replication report a false MISMATCH (static)

* Where: `rpm::verify` (`src/pulp/rpm.rs:248-256`) probes `package_path(layout, loc)` and then `location_href`;
  `package_path` (`:196-204`) returns `None` for `nested_by_digest`, so only `location_href` is probed.
  `docs/COMPATIBILITY.md:998-999` and the comment at `rpm.rs:197` say the probe is skipped for that layout.
* Pulp: pulp_rpm 3.39.5 `tasks/publishing.py:299-311, 392-393` serves the package only at
  `Packages/<letter>/by-digest/<sha[:6]>-<file>`; the OpenAPI `LayoutD5aEnum` offers `nested_by_digest`, and the
  repository and publication forms offer it.
* Effect: Distributions Verify "MISMATCH", Rollback "verification failed", Content View drift DRIFT with a Repair hint,
  and a fallback replication item fails, for content that is served correctly.

#### F83 — P3 — File Sync "mirror" on an autopublish repository passes the confirm, then Pulp refuses it (static)

* Where: `sync_refusal` (`actions.rs:1370-1385`) refuses autopublish only for RPM `mirror_complete`; File policy
  `mirror` goes through `confirm_mirror` (a typed confirm when the retention is 1 or less).
* Pulp: pulp_file 3.123.1 `viewsets.py:269-272` "Cannot use mirror mode with autopublished repository." → 400. (Not in
  the pulp_file of 3.95.3 and 3.63.36.) The Content View "Sync repositories" action fails the same rows.

#### F84 — P3 — `import-repos` refuses every RPM repository with an OSV config (static)

* Where: `repo_export::entry` / `field` (`src/repo_export.rs:47-79`) export `pulp_labels` as Pulp returns them,
  including `osv.rpm.config`, and `create_body` (`:93-125`) sends them back.
* Pulp: pulp_rpm 3.39.5 `serializers/repository.py:334-337` refuses any `pulp_labels` with that key ("Use the
  'osv_config' field …"). The repository form already removes the label (`actions.rs:1095-1097`).
* Effect: export-repos then import-repos (CLI `J` / `import-repos`) ends "N not created" with 400 for each such
  repository, on every rerun.

#### F85 — P3 — The Restore confirm says "newer versions are kept"; the retention can delete them after a plain `y` (static)

* Where: `restore_lines` (`actions.rs:3115`) says "…; newer versions are kept.", and a later line says
  "retain_repo_versions = n: Pulp then deletes the versions older than the newest n." `restore_preview` uses
  `confirm_prod` (typed name only on production profiles) and not `latest_goes` / `goes_lines` (`:366-385`), which
  Remove content, Replace comps and the mirror sync use.
* Pulp: `RepositoryVersion.__exit__` → `cleanup_old_versions` (`models/repository.py:479-497`).
* Scenario: Retain versions 1, versions 1–5, no distribution serves 5; Restore version 2 → Pulp deletes 1–5 after an
  untyped `y` on a non-production profile; content only in 3–5 is then orphan.

#### F86 — P3 — "Staff (bypasses RBAC)" is false: `is_staff` grants nothing in Pulp (static)

* Where: the user form label (`actions_admin.rs:71`), the "PRIVILEGE ESCALATION … bypass RBAC" confirm (`:93-95`),
  the self-lockout warning when Staff is cleared (`:96-98`), the delete warning "staff (administrator) account"
  (`:171-172`), the Dashboard "(staff/admin)" (`actions.rs:4261`, seen in this session's Dashboard), and
  `docs/ARCHITECTURE.md:576`, `docs/USER_GUIDE.md:902`.
* Pulp: the fallback policy is `principal: "admin"` (`access_policy.py:11`); drf-access-policy maps `admin` to
  `is_superuser` and `staff` to `is_staff` (`rest_access_policy/access_policy.py:171,173`); no pulpcore or plugin
  policy names `staff`; the superuser bypass in `role_util.py` reads `is_superuser`. `UserSerializer` does not expose
  `is_superuser`. The project's own `docs/COMPATIBILITY.md:225` says "`is_staff` does not bypass RBAC".
* Effect: an operator who sets Staff to make an admin gets a user with no rights; one who clears Staff gets a false
  lockout warning; the delete of a real superuser (with `is_staff` false) has no admin warning.

#### F87 — P3 — No self-lockout warning when the current user loses access through a group; self-disable needs only `y` (static)

* Where: `role_assignment` (`actions_admin.rs:193`, `is_me = !group && …`); the group Delete (`:270-288`) reads the
  members but does not compare them with `me()`; `object_roles` (`:452`) checks only picked users. ToggleActive and
  Edit with active off on one's own account use `confirm` (`:96-153`), while a self password change uses
  `confirm_danger` (`:131`).
* Effect: a non-superuser removes the role of a group they are in (or deletes the group) and loses the permission with
  no "YOUR account" line; one `y` disables one's own account (401 on every request after).

#### F88 — P3 — Deleting the newest export record does not say that the next incremental export needs start versions (static)

* Where: `export_action` (`actions_admin.rs:1640-1654`) says only that the files stay.
* Pulp: `Exporter.last_export` is `on_delete=SET_NULL` (`models/exporter.py:149-150`); an incremental export without
  `last_export` or `start_versions` is refused (`serializers/exporter.py:216-224`). The Export form then says "No
  previous export…" and offers Start versions, so it is not silent; the confirm should say it first.

#### F89 — P3 — Content guard Delete does not name a domain whose default content guard it is (static; domains only)

* Where: the Delete impact (`actions_admin.rs:505-537`, `guard_use` `:656-663`) reads distributions and composites
  only. Pulp 3.123.1 `Domain.default_content_guard` (`models/domain.py:51-53`, `SET_NULL`) is set on each new
  distribution of the domain (`models/publication.py:911-922`). After the delete, new distributions there are public.
  Applies only with domains enabled. (The Edit is not affected: a default guard serves nothing by itself.)

#### F90 — P3 — Deleting the domain of the current profile has no warning (static; domains only)

* Where: `domain_action` (`actions_admin.rs:816-888`) refuses only `default`. A delete of the (empty) domain of the
  profile (`config.rs:193`) makes every later request of the session 404.

#### F91 — P3 — On pulpcore 3.95 and 3.63 the Upstream Delete impact never names a running replication (static)

* Where: `upstream_delete` (`actions_admin.rs:1265-1276`) counts busy tasks with `reserved_resources=<upstream href>`;
  the code (`:1191-1192`) and `docs/COMPATIBILITY.md:726-727` say those servers do not reserve the upstream; History
  has a fallback (`runs_reserve_no_upstream`, `:1229`), Delete does not. Not run on an old image in this audit.

#### F92 — P3 — Behind an outer TLS proxy, every `list_all` of more than one page fails (static; Pulp side runtime)

* Where: `PulpClient::url_for` (`src/pulp/client.rs:240-247`) refuses a `next` link whose origin differs from the
  profile; `list_all` (`:260-275`) follows `next` as Pulp sends it (500 per page).
* Pulp: DRF `LimitOffsetPagination` builds `next` from the request; pulpcore sets no `SECURE_PROXY_SSL_HEADER`.
  In this session `curl -H 'X-Forwarded-Proto: https'` to pulp1 still gave `next = http://…`. The image's own nginx
  is fine (gunicorn trusts `X-Forwarded-Proto` from localhost), so only TLS that ends at an outer proxy with a plain
  http inner hop breaks: page 2 of content keys (Content View publish, compare), pickers over 500 repositories, the
  overlap checks.
* Fix direction: for `next`, require the same host and port and a path under the API root, and send path and query to
  the configured base.

#### F93 — P3 — Task references are keyed by profile name; after a profile Edit to another server they end `gone` for good (static)

* Where: `track_task` / `finish_task` use `self.profile`; the table key is `(profile, href)` (`store.rs:659-686`);
  `reattach_tasks` (`app.rs:855-873`) polls the references on whatever server the profile now names; `task_gone`
  (`:674-683`) stores `gone`, and `track_task` cannot undo it (`WHERE finished_at IS NULL`). The Content View store
  already uses `Profile::server_key()`.

#### F94 — P3 — Config save is synchronous file I/O on the UI thread (static)

* Where: `Config::save` (`config.rs:497`) → `write_atomic` (`:457-469`: `create_dir_all`, write, `set_permissions`,
  `rename`), called from key handlers: `toggle_hidden` (`app.rs:1246`) and the profile Create, Edit and Delete
  submits (`actions.rs:630`, `:780`, `:833`). A breach of the `AGENTS.md` rule "Never block the UI thread on I/O";
  the keyring path uses `spawn_blocking` (`actions.rs:857`).

#### F95 — P3 — Two local profiles of one server share the Pulp objects of Content Views but not their records (static)

* Where: the local store scope is the profile name (`App::scope`, `app.rs:356-371`); Pulp names and ownership labels
  carry only the view and environment (`cv.rs:257-272`), and both profiles have the same `pulp_tui_db` (one file).
* Effect: view `web` of profile `p2` publishes into the snapshot repositories of `web` of `p1`, re-points its Library
  distribution, and a Delete of `p2`'s `web` deletes them. `used_versions` (`store.rs:1108-1116`) is the only check
  across scopes. Not documented (`docs/USER_GUIDE.md:1133` covers only an external database).

#### F96 — P3 — Resume cannot finish a publish whose recorded source version is gone, and does not say so (static)

* Where: `publish` records each source `latest_version_href` (`cv.rs:648-667`); `snapshot` (`:955-959`) and the copy
  use it; nothing checks that it still exists; `resume` (`:783-827`) stores the raw 400/404. A source with Retain
  versions 1 that syncs during the publish prunes it. Fix direction: name the gone version and say "delete vN and
  publish again".

#### F97 — P3 — A source repository renamed and added again leaves an unrecorded distribution; Rollback then leaves the newer version served (static)

* Where: `bind_repo` picks the distribution by the name that the version row records (`cv.rs:1523-1546`);
  `promote_target` (`:1485-1505`) returns early when a distribution with the derived name exists, so the Rollback
  preview does not name the other one; drift reads only recorded rows. The Promote preview after the rename does say
  that the old distribution "keeps its publication, and pulp-tui no longer records it" (with the wrong reason "renamed
  outside pulp-tui"). `docs/STATUS.md:496-497` documents only the rename itself.

#### Other P3 polish (static unless named)

* ULN remote Edit: "-" in Username or Password sends `null`; `rpm.UlnRemote` refuses null (400). The code comment at
  `actions.rs:2544` ("the server accepts null for all of them") is wrong for ULN.
* ACS Delete says "its 1 path(s)" for an ACS whose only path is `""` (`actions.rs:2665-2667`).
* Watch or Track of a finished Server tasks row shows no progress reports: the list excludes `progress_reports`
  (`kinds.rs:639-641`) and a final row is never polled.
* An empty task group is shown as "its tasks were purged" (`TaskGroup::purged`); F77 shows the case.
* Audit 4's F47 premise "Pulp refuses user info in a remote `url`" is wrong for RPM: pulp_rpm `RpmRemoteSerializer`
  and `UlnRemoteSerializer.validate_url` do not call the pulpcore check. The Edit forms show the URL as Pulp has it,
  as `docs/ARCHITECTURE.md:561-565` documents; the RPM remote `url` and the upstream `base_url` can hold a password.

Candidate parts rejected by audit 7 after verification: the Edit form URL display (documented design, above); "export
incremental stops silently" (the Export form explains it: kept as F88 with the smaller scope); the guard Edit and the
domain default guard (no effect); "Restore does not name deleted versions" (it has a retention line: kept as F85);
`save_binding_repo` as the cause of F97 (the name comes from the version row); "every TLS deployment breaks" (only an
outer proxy: F92).

### 6.2 New findings (audit 8)

IDs continue the numbering: F98–F132, G21, G22 and T07. Paths are relative to the repository; Pulp paths are relative
to the `site-packages` folder of the named image (`latest` = pulpcore 3.123.1, pulp_rpm 3.39.5, pulp_deb 3.11.1).
`dom` is the audit's server with domains (3.123.1); `old` is the audit's 3.63.36 server (section 2).

Root causes of this audit, so that a fix closes a class and not one case:

* **RC4 — State of the previous server survives a Connect** (F98, F115; F93 and F95 of audit 7 are the same class):
  a parameterised sub-list, an open form and the profile text are not tied to the connection that made them.
* **RC5 — A served-content check accepts only "200 from an absolute URL"** (F99, F105, F113): a relative `base_url`,
  a redirect to object storage and a content guard are all reported as MISMATCH or DRIFT, with the hint "repair".
* **RC6 — A field or filter is sent without a capability check, or a server quirk of one version is not known**
  (F102, F103, F104, F112, F129, F130): each one works on `latest`, where the suite runs its form tests, and fails
  on an older supported image. The live tests create Deb and File repositories through the API, not the form.
* **RC1 again — A confirm or help text states a Pulp rule that Pulp does not follow** (F100, F101, F117, F118).
* **RC7 — An impact preview is built from a list that Pulp scopes to the user** (F109, F121).
* **RC8 — Two `modify/` requests where Pulp reacts to the first one** (F107): Pulp's module rule changes the
  version between the remove step and the add step, and the add list was computed before.

#### F98 — P2 — After Connect from a sub-list, a form opened while "connecting..." writes to the new server with the object of the old server (runtime)

* Where: `App::connect_now` (`src/tui/app.rs:783-844`) sets `conn = None` and then keeps the screen: `if let
  Screen::List(k) = self.screen.clone() && !matches!(k, Kind::Profiles | Kind::Logs { .. }) { self.replace(k); }`
  (`:803-807`). `k` keeps its parameters (`Kind::UserRoles { user }`, `GroupRoles { group }`, `GroupUsers { group }`,
  `CvVersions { cv, .. }`, `Versions(Some)`, `Content { version }`). `App::unsupported` (`app.rs:1410-1412`) is `None`
  without a connection, so `available_actions` (`:1414-1427`) still offers each action that needs no row. A form opens
  with no I/O. The connect result (`app.rs:825-838`) opens the Dashboard (`:837`) but does not close the form, and
  `open(Dashboard)` keeps `app.list`. The submit then runs with the client of the new server.
* Endpoints: `POST {user_href}roles/`, `POST {group_href}roles/` (`assign_role`, `src/tui/actions_admin.rs:211-232`,
  URL at `:230`), `POST {group_href}users/` (`member_action`, `:317-326`). User and group hrefs hold an integer key
  (`/pulp/api/v3/users/2/`), and Pulp takes the target only from the URL (pulpcore `app/serializers/user.py:369`,
  `:439`; `app/viewsets/user.py:195-199`; the same in 3.95.3 and 3.63.36).
* Runtime [R48]: profile `old`: Users → `a8-x1` (`users/2/`) → `R` ("Roles of user a8-x1") → `:` "Connect: pulp2" →
  Enter. The header says "server: pulp2 | connecting...", the list is still "Roles of user a8-x1". `c` within 0.2 s
  opens "Assign role to user a8-x1". The connect ends, the form stays. Role `file.fileremote_viewer`, Object (none;
  the picker lists objects of pulp2), `Ctrl-S` → "[ok] assign file.fileremote_viewer to user a8-x1: done". Pulp: on
  pulp2, `users/2/` is `a8-carol` and now has the role; `a8-x1` on `old` has none.
* More effects of the same code (static): a member is added to the group with the same number on the new server;
  each form opened in that window is built with `caps = None`, so no version gate applies; `CvVersions` needs no
  connection (`kinds.rs:503-508`) and its load has no scope check (`kinds.rs:1172-1178`), so with two profiles in one
  local file the versions of the other profile's view stay on screen, and Delete marks a version deleted while its
  Pulp objects stay on the old server (a 404 counts as gone, `cv.rs:565-575`); Resume can create a snapshot
  repository on the new server (`cv.rs:909-945`).
* Why P2: a role or a membership goes to another account on another server, and the result text names the account of
  the old server. It needs one key in the connect window (some seconds: status, login and the OpenAPI download).
* Smallest reproduction: [R48]. No test covers it: `a_failed_connect_drops_the_rows_of_the_previous_server`
  (`app.rs:3073`) checks rows and marks only.

#### F99 — P2 — With `CONTENT_ORIGIN` not set (the default of pulpcore 3.95.3 and 3.123.1), Verify, the drift check, the Rollback check and fallback replication fail for served content that is correct (runtime)

* Where: `PulpClient::fetch_content` (`src/pulp/client.rs:346-349`): `url::Url::parse(url).map_err(|_|
  PulpError::BadHref(url.into()))?` accepts only an absolute URL. The callers pass `base_url` as Pulp returns it:
  `distribution_verify` (`src/tui/actions.rs:3686-3708`, "MISMATCH: {e}" at `:3704`), Rollback (`:3064-3068`), Content
  View `verify_publication` (`src/cv.rs:1768-1774`), fallback replication (`src/replication.rs:320`; the plan also
  uses the upstream `base_url` as the remote URL, `:126-164`), Copy URL (`actions.rs:3655`, `:3665-3670`).
* Endpoint: `GET distributions/<type>/` field `base_url`, then the content app.
* Pulp: `CONTENT_ORIGIN = None` is the default (`pulpcore/app/settings.py:288`; 3.95.3 `:289`), and
  `BaseURLField.to_representation` then returns `/pulp/content/<base_path>/` (`app/serializers/fields.py:331-342`;
  3.95.3 `:316-320`). pulpcore's own replication joins it with the server URL (`app/replica.py:121-122`). 3.63.36
  needs an absolute value (`settings.py:276`), so it is not affected. The test environment always sets the value
  (`tests/env/pulp-env.sh:66`), so no test sees a relative `base_url`.
* Runtime [R44] (`dom` restarted without `CONTENT_ORIGIN`; `status/` says `content_origin: null`): Distributions →
  `a8-big` → `V`: "URL: /pulp/content/default/a8-big/ … MISMATCH: unsafe href rejected:
  /pulp/content/default/a8-big/PULP_MANIFEST". `y` copies "/pulp/content/default/a8-big/". The Dashboard line
  "content:" is empty.
* Runtime [R45]: Content View `a8cv` with File repository `a8-f`: `P` → "[ok] a8cv version 1 published and active in
  Library" (the publish does not read served content). `D` → "Library: v1 DRIFT / a8-f: unsafe href rejected:
  /pulp/content/default/library/a8cv/a8-f/PULP_MANIFEST / Library: check the content app, then repair the snapshot
  version (Repositories, v, F): the served files are not those of the snapshot."
* Effect: on such a server every Verify is MISMATCH, every promoted environment is DRIFT with a wrong repair hint,
  Rollback says "re-pointed, but verification failed", each fallback replication item fails after it made its objects
  (static), and Copy URL gives a path with no host. `Fix::Origin` ("distribution has no base_url: set CONTENT_ORIGIN",
  `cv.rs:1768-1769`, `docs/USER_GUIDE.md:658`) waits for a null `base_url`, which no supported pulpcore returns.
* Fix direction: join a `base_url` that starts with `/` with the profile URL in one place (`fetch_content` callers,
  Copy URL, the fallback plan with the source profile URL).

#### F100 — P2 — Remote Delete says "Repository content and versions are not affected"; Pulp deletes the remote artifacts, and on-demand content can no longer be downloaded (runtime)

* Where: `remote_delete_impact` (`src/tui/actions.rs:1477-1492`), text at `:1481`; used by the Delete of one remote
  (`remote_action`, `:2636-2644`) and of marked remotes (`marked_impact`, `:1563-1567`). It lists only the
  repositories whose default remote it is; a remote used through the Sync form is not named.
* Endpoint: `DELETE remotes/<plugin>/<type>/<id>/`.
* Pulp: `RemoteArtifact.remote` is `on_delete=CASCADE` (`pulpcore/app/models/content.py:754`; 3.95.3 `:748`; 3.63.36
  `:727`). The content app streams a unit without a stored file only from its remote artifacts and else answers 404
  (`pulpcore/content/handler.py:1076-1096`).
* Runtime [R46]: File remote `a8-od` (policy `on_demand`), repository synced (40 units, `artifact: null`),
  published and distributed; `f1.bin` downloaded once. Remotes → `a8-od` → `d`: "Delete remote a8-od. Repository
  content and versions are not affected. / 1 repository(ies) use it as default remote: a8-od / No alternate content
  source uses it." → typed name → "task completed: delete remote a8-od". After: `PULP_MANIFEST` 200, `f1.bin` 200,
  `f3.bin` **404**; the version still lists 40 units.
* Effect: every unit of an `on_demand` or `streamed` remote that Pulp did not store yet (also files that a Reclaim
  removed, F76) stays in all versions, publications and Content View snapshots, and each download is 404. Repair does
  not bring it back; a new remote with the same URL and a new sync does.
* Fix direction: read the remote's `policy`; for a policy other than `immediate` say that content not yet downloaded
  cannot be served after the delete, and name the repositories.

#### F101 — P2 — Native replication with policy `nodelete` takes over local objects that have the name of an upstream distribution; the form says "never delete local objects" and the confirm needs only `y` (runtime)

* Where: the Upstream Pulp form note (`src/tui/actions_admin.rs:1060`): "nodelete: never delete local objects. …";
  `replicate` (`:1138-1169`): the line "Pulp creates/updates repositories, remotes and distributions with the
  upstream names." (`:1153`) and a plain `app.confirm` when `deletion_warning` (`:925-935`) gives `None`. No impact
  is read.
* Endpoint: `POST {upstream_pulp_href}replicate/`.
* Pulp (3.123.1): `pulpcore/app/replica.py:86-91` `_is_managed` is false only for policy `labeled`. For `nodelete`
  and `all`, a local remote, repository and distribution with the upstream distribution's name are updated
  (`:113-246`): the repository is synced with a mirror policy, `pulp_labels` are replaced by `{"UpstreamPulp": id}`
  (`:102-105`), and the distribution is set to the new `repository_version` (`tasks/replica.py:183-200`). On 3.123 a
  distribution at the same base path with another name is renamed too (`replica.py:227-228`). 3.95.3 matches by name
  (`replica.py:81`, `:125`, `:154`, `:188`); 3.63.36 has no policy and matches by name.
* Runtime [R52] (`dom`; the upstream is its own domain `a8t`): local File repository `a8-take` (version 1 with
  `local-only.txt`) and distribution `a8-take` (labels `owner=team-a`, follows the repository). Upstream Pulp
  `a8-self`, policy `nodelete`, filter `name="a8-take"`. Replication → `R`: "Replicate from … into this server … /
  Distribution filter: name="a8-take" / Deletion policy: nodelete / Pulp creates/updates repositories, remotes and
  distributions with the upstream names." → `y`. After: repository `a8-take` version 2 (+40, −1: `local-only.txt` is
  not in the latest version), its labels are `{"UpstreamPulp": …}`; the distribution lost `owner=team-a`, has
  `repository: null` and `repository_version` = version 2; a remote `a8-take` exists.
* Effect: a local repository that has the name of an upstream distribution loses its own content in the latest
  version and its labels. With pulp-tui on both servers and the same view name, the downstream distribution
  `pulp-tui-cv-<view>-<env>-<repo>` loses `pulp_tui_cv`, `pulp_tui_env` and `pulp_tui_db` and serves the replica
  (then F71 applies). The stricter policy `labeled` does not touch unlabeled objects but asks for the typed name;
  `nodelete` does touch them and asks for `y`.
* Fix direction: before the confirm, list the local repositories, remotes and distributions that have no
  `UpstreamPulp` label of this upstream (all of them when the filter cannot be evaluated locally: say "each local
  object with the name of an upstream distribution"), say "content replaced by a mirror sync, labels replaced,
  distribution re-pointed", and ask for the typed name. Correct the note at `:1060`.

#### F102 — P2 — On pulp_deb 3.5.4 (image 3.63.36) every Deb repository Create from the form fails: `autopublish` is sent with no capability check (runtime)

* Where: `Field::new("autopublish", "Autopublish", FieldKind::Bool)` (`src/tui/actions.rs:1106`) is in every
  repository form; the type fields above it are behind `has(...)`. `repo_body` (`:1121-1160`) sends a Bool always
  (`:1149`), so Create posts `"autopublish": false` (`:1211`).
* Endpoint: `POST repositories/deb/apt/`.
* Pulp: pulp_deb 3.5.4 has no `autopublish` (`pulp_deb/app/serializers/repository_serializers.py:78-84`); pulpcore
  refuses an unknown field (`pulpcore/app/serializers/base.py:302-321`). pulp_deb 3.8.1 and 3.11.1 have the field.
* Runtime [R49] (`old`): Deb Repositories → `c` → Name `a8-olddeb` → `Ctrl-S`: "[error] create repository a8-olddeb:
  rejected [400]: autopublish: Unexpected field"; no repository. An Edit that changes the box gives the same 400.
* Why no test saw it: `tests/live_deb.rs` creates repositories with `create(&c, t.repositories(), json!({"name": …}))`;
  only `tests/live_repos.rs:712` (RPM) runs `Act::Create` on `Kind::Repos`.

#### F103 — P2 — On pulp_rpm 3.27.12 (image 3.63.36) every RPM repository Edit that does not change "Repo config" erases `repo_config` (runtime)

* Where: `repo_body` sends only changed fields on Edit (`src/tui/actions.rs:1124-1126`); PATCH at `:1224`.
* Endpoint: `PATCH repositories/rpm/rpm/<id>/`.
* Pulp: pulp_rpm 3.27.12 `pulp_rpm/app/serializers/repository.py:235-263` (`update`): `instance.repo_config =
  gpgcheck_options if gpgcheck_options else validated_data.get("repo_config", {})`. A partial update without
  `repo_config` stores `{}`. pulp_rpm 3.34.0 and 3.39.5 have no such override.
* Runtime [R50] (`old`): repository `a8-oldrpm` with `repo_config {"gpgcheck": 1, "repo_gpgcheck": 1}`. RPM
  Repositories → `e` → Description "audit 8" → `Ctrl-S`. The Logs screen shows the body `{"description": "audit 8"}`;
  "task completed: update repository a8-oldrpm"; Pulp: `repo_config: {}`.
* Effect: after any edit (description, remote, retention, labels) the next publication serves a `config.repo`
  without the GPG check settings. Nothing tells the operator. Not in `docs/COMPATIBILITY.md`.
* Fix direction: on a server with that shim (the repository schema has the deprecated `gpgcheck` field), add the
  current `repo_config` to each RPM repository PATCH (also the settings PATCH of Content View snapshots,
  `cv.rs:1024-1058`); record it in `docs/COMPATIBILITY.md`.

#### F104 — P2 — Deb fallback replication from pulp_deb 3.11 into a 3.8 or 3.5 destination fails on every item after the sync (runtime)

* Where: `ops::mode_fields` (`src/pulp/ops.rs:278-286`) lists `layout`, `publish_legacy_release_files` and
  `excluded_package_metadata_fields`; `publication_mode` (`:292-297`) copies each one the upstream publication has
  (`replication::deb_source`, `src/replication.rs:257-260`); `publication_body` (`ops.rs:346-348`) posts them
  (`replication.rs:313-316`). No `has_field` check against the destination (`AGENTS.md` capability rule).
* Endpoint: `POST publications/deb/apt/` on the destination.
* Pulp: pulp_deb 3.11.1 `app/serializers/publication_serializers.py:82-91` has the three fields; 3.8.1 (`:59-65`)
  and 3.5.4 (`:57-62`) do not; pulpcore answers 400 "Unexpected field".
* Runtime [R58]: source profile `dom` (3.11.1), destination `old` (3.5.4). Replication → `F` → Types `deb`, name
  `a8-deb`. Dry run: "CREATE Deb  a8-deb -> /a8-deb  new remote + repository + distribution". Run: "failed    Deb
  a8-deb: rejected [400]: excluded_package_metadata_fields: Unexpected field; layout: Unexpected field;
  publish_legacy_release_files: Unexpected field". Left on `old`: remote `a8-deb`, repository `a8-deb` at version 1
  (synced), no publication, no distribution. A rerun fails the same way.
* Fix direction: filter the mode by the destination schema in the TUI job after `replication::plan`
  (`actions_admin.rs:1328`; `ctx.caps` is the destination), and let `find_publication` ignore fields that the
  destination does not have.

#### F105 — P2 — On object storage (S3, Azure, GCS) every served-content check reads Pulp's 302 as a mismatch (static)

* Where: the only HTTP client has `.redirect(reqwest::redirect::Policy::none())` (`src/pulp/client.rs:144`), and
  `fetch_content` (`:346-361`) returns the 302. Each probe accepts only 200: `src/pulp/rpm.rs:220-224` and `:250-251`,
  `src/pulp/file.rs:22-25`, `src/pulp/deb.rs:79-82`, `:157-158`, `:165-166`.
* Pulp: `pulpcore/content/handler.py:1239-1250` (3.95.3 `:1133-1141`, 3.63.36 `:1038-1049`): with S3, Azure or GCS
  storage and `redirect_to_object_storage` (default true, `app/models/domain.py:48`, `settings.py:80`) the content app
  answers `HTTPFound`, also for published metadata (`handler.py:886`, `:1254`).
* Effect: Verify "MISMATCH: … returned HTTP 302", Rollback "verification failed", Content View DRIFT with the repair
  hint, each fallback replication item `failed` after the replica is complete. The Domains form of pulp-tui offers
  these storage classes and the redirect switch (`actions_admin.rs:811`). Object storage is not in "Unsupported or
  limited" (`docs/COMPATIBILITY.md:993-1020`).
* Not run: the test servers use file system storage. Two reviewers found it independently and a third one could not
  disprove it.
* Fix direction: `fetch_content` follows a small number of 3xx by hand, sends no `Authorization` to another origin
  and masks the signed query in the session log.

#### F106 — P2 — An upload into a repository with a package signing service ends in "digest mismatch", although Pulp completed it (static)

* Where: `ops::uploaded` (`src/pulp/ops.rs:935-957`) calls `check_digest` (`:960-967`) with the sha256 of the local
  file; callers `upload_fields` (`ops.rs:875`, used by `ops::upload` and `deb::upload`, `src/pulp/deb.rs:224`). In the
  TUI, `upload_file` (`src/tui/actions.rs:1795-1818`) then skips the "[ok]" and the list reload. The repository form
  offers the service (`actions.rs:1009-1015` RPM, `:1051-1056` Deb), and `docs/USER_GUIDE.md:313`, `:324` say that it
  signs uploads.
* Endpoints: `POST content/rpm/packages/`, `POST content/deb/packages/` with `repository`.
* Pulp: pulp_rpm 3.39.5 `app/viewsets/package.py:116-152` dispatches `sign_and_create`; `app/tasks/signing.py:156-199`
  makes the package from the signed file (a new artifact, `:178-181`). The same in 3.34.0 (`viewsets/package.py:114-143`)
  and 3.27.12 (`:84-113`). pulp_deb 3.11.1: `app/viewsets/content.py:280-330`, `app/tasks/signing.py:212-245`.
* Effect: the task completes and the signed package is in a new version; the TUI shows "[error] upload <path>: digest
  mismatch for …" and the Task Center row says completed. A retry signs again and makes one more unit and version.
* Not run: the test environment has no package signing service (`grep package_signing tests/` finds only the Deb form
  test). Scenario: RPM repository → `e` → Package signing service and fingerprint → `u` → an unsigned `.rpm`.
* Fix direction: when the repository has `package_signing_service`, report the server digest and say that Pulp
  signed the package.

#### F107 — P2 — A filtered RPM Content View publish drops selected packages when a modulemd of the source was replaced; the preview says "+0 added, -0 removed" (runtime)

* Where: `CvService::snapshot` (`src/cv.rs:972-979`) computes the steps once: `let (remove, add) = self.changes(t,
  derived, all).await?;` then `[batches(true, &remove), batches(false, &add)].concat()` (`:978`). `changes`
  (`:1098-1118`) has `add = all - present` (`:1117`), with `present` read before any step. The steps are two requests
  (`ops::remove_content`, `cv.rs:993`; `ops::add_content`, `:996`). `rpm_metadata` (`cv.rs:249-253`) selects every
  modulemd of the source. Nothing compares the final snapshot with the selection (`:1010-1015` records only a count).
* Endpoint: `POST {snapshot repository}modify/` two times.
* Pulp: each new RPM version runs `resolve_module_packages` (`pulp_rpm/app/models/repository.py:469-471`;
  `pulp_rpm/app/modulemd.py:59-83`): the packages of a removed modulemd that no module of that version lists are
  removed too. The add side never adds packages (`:86-89`). The same in 3.34.0 (`repository.py:405-407`) and 3.27.12
  (`:398-400`).
* Runtime [R60] (`dom`): RPM repository `a8-rpm` (packages `fixture-alpha`, `fixture-beta`, `fixture-beta-debuginfo`,
  one advisory, modulemd `a8mod:1:1` that lists alpha and beta). View `a8mod`, rule `exclude name=*-debuginfo`; `P` →
  v1, snapshot: 2 packages, 1 modulemd. Then the source modulemd is replaced by `a8mod:1:2`, which lists beta; alpha
  and beta are still in the source. `P`: preview "2 selected … published -> draft: +0 added, -0 removed" → `y` →
  "Library:v2". Pulp: the new snapshot version has **no package** (`removed: rpm.modulemd 1, rpm.package 2`), one
  modulemd that lists `fixture-beta`, one advisory. The next `P` previews "+2 added" and brings both back.
* Effect: one published version, active in Library (and promoted further), without packages that the filter selects
  and the preview counts; a module lists packages that the repository does not have. It needs a filtered RPM view and
  a source that drops or replaces a modulemd while a package of it stays (mirror sync of a modular repository, or a
  removed module). Unfiltered views (`rpm/copy/`), File and Deb are not affected.
* Fix direction: `cv.rs:974-978`: send the modulemd removals in the same `modify/` as the adds, or run `changes`
  again after the loop and add what is missing (do not put the add before the remove: advisories with the same id
  conflict). Check the final snapshot keys against the selection before the version is recorded.

#### F108 — P3 — An RPM Content View publish reuses a publication that has other publish settings (static)

* Where: `CvService::publication` (`src/cv.rs:1065-1068`): `RepoType::Rpm | RepoType::File if !changed &&
  vr.pending_task.is_none() => ops::find_publication(c, t, dv, &json!({}))`. With an empty mode, `find_publication`
  (`src/pulp/ops.rs:326-334`) returns the newest publication of the version for RPM. `changed` is only "this run sent
  a PATCH" (`copy_settings`, `cv.rs:1024-1058`). `ops::reusable_publication` (`ops.rs:318-323`), which Rollback and
  Distribute use for a signing repository, is not used here. `docs/ARCHITECTURE.md:177-179` states the assumption.
* Scenario: v1 is published without signing. Set Metadata signing service (or checksum, layout, repo config) on the
  source. `P` → v2: the snapshot repository is PATCHed, then the publication task fails (or the client stops); v2 is
  `failed`. `P` again (not Resume) → v3: equal content, equal settings now, so no PATCH; the old unsigned publication
  is reused and v3 is "published and active in Library". The same after Delete of v2. Resume of v2 is safe. File
  compares `manifest`; Deb never reuses.
* The drift check then reports the missing `repomd.xml.asc`, so it is found when someone runs it.
* Fix direction: compare the `mode_fields` of the publication with the snapshot repository, and do not reuse for a
  repository that signs metadata.

#### F109 — P2 — For a user that is not a superuser, the Delete and Edit impacts say "No distribution uses it" from a list that Pulp scopes to the user (runtime for the guard Delete; the other paths static)

* Where: content guard Delete (`src/tui/actions_admin.rs:505-518`, "No distribution uses it." at `:517`); content
  guard Edit (`confirm_guard_edit`, `:635-641`: saves with no confirm when the list is empty); `guard_use`
  (`:656-662`) → `ops::guarded_distributions` (`src/pulp/ops.rs:187-197`); the "distributions" column
  (`src/tui/kinds.rs:1083-1088`). Repository Delete (`src/tui/actions.rs:1456-1466`, "No distribution serves it." at
  `:1463`). Publication Delete (`:3589-3595`): the WARNING line is missing. Version Delete (`:2859-2871`, `:2869`): a
  false line only, because Pulp refuses a protected version itself. `ops::sees_everything` (`ops.rs:478-484`), made for
  F59, is used only at `cv.rs:1394`.
* Pulp: lists are scoped to what the user may view (`pulpcore/app/viewsets/base.py:359-380`; the generic
  `distributions/` list, `app/viewsets/publication.py:574-584`; the same in 3.95.3 and 3.63.36). Delete needs only the
  object permission, which the creator gets by the owner role. `Distribution.content_guard`, `.repository` and
  `.publication` are `SET_NULL` (`app/models/publication.py:741-747`). `docs/USER_GUIDE.md:581-583` promises "The
  impact lists the distributions that use the guard".
* Runtime [R62] (`dom`): user `a8-u1` has only the role `core.headercontentguard_creator`. As `a8-u1`: Content Guards
  → `c` → header guard `a8-ug` (the creator gets the owner role). The administrator puts `a8-ug` on File distribution
  `a8-big`; without the header its `PULP_MANIFEST` answers 403. `a8-u1` sees no File distribution
  (`distributions/file/file/` count 0). As `a8-u1`: `d` → "Delete content guard a8-ug (core/header). No distribution
  uses it." → typed name → "[ok] delete content guard a8-ug: done". After: `a8-big` has `content_guard: null`, and
  `PULP_MANIFEST` answers 200 without the header. The "distributions" column of the list was empty too.
* The repository Delete, the publication Delete and the guard Edit were not run for such a user (same lists, same
  scoping). Same class as F66 and F89: a false "No distribution uses it".
* Fix direction: one helper next to `sees_everything`; when it is false, replace each "No distribution …" line with
  "none that this Pulp user may view (not an administrator: others can exist)".

#### P3 findings reproduced at runtime

* **F110 — Role Create and Edit send `"description": ""`; Pulp answers 400.** `role_form`
  (`actions_admin.rs:362`). pulpcore `RoleSerializer.description` is `allow_null` without `allow_blank`
  (`app/serializers/user.py:218-220`; 3.95.3 `:219-220`; 3.63.36 `:200-201`). [R47]: Roles → `c` → name, default
  permission, empty Description → "[error] create role a8-role: rejected [400]: description: This field may not be
  blank." A role with a null description (made with another tool) cannot be edited until a description is typed.
  The mock test `role_create_copies_the_permissions_but_not_the_name` asserts this body. Fix: send null for empty.
* **F111 — A config save removes what another process added to the file since the TUI started.** `Config::save`
  (`src/config.rs:497-529`) reads the file as `old` (`:503`) and removes each key that the in-memory config lacks
  (`:427-438`); the callers build the new config from `app.cfg.clone()` (`app.rs:1246`, `actions.rs:630`, `:780`,
  `:833`), and the file is loaded only at start. [R53]: `[profiles.outside]` appended to the file while the TUI ran;
  `x` in the Screens pane ("Replication hidden") → the profile is gone from the file. `docs/USER_GUIDE.md:1058-1060`
  says a save "changes only the keys whose values changed". Fix: apply the change to a fresh `Config::load`.
* **F112 — File Content search (`/`) is HTTP 400 on pulp_file 3.95.3 and 3.63.36.** `Kind::source`
  (`src/tui/kinds.rs:598`) sends `relative_path__icontains` with no capability check (`:1078-1080`). The old filter
  sets have only exact `relative_path` and `sha256` (`pulp_file/app/viewsets.py:57-66` in 3.95.3, `:53-62` in
  3.63.36). [R51] (`old`): File Repositories → `o` → `/` `a` Enter → "Error: rejected [400]: errors: Invalid Filter:
  'relative_path__icontains'". 3.95.3: source only.
* **F113 — Verify of a distribution with a content guard that the API user does not pass says MISMATCH.**
  `fetch_content` sends only the API authorization; `file::verify` (`src/pulp/file.rs:22-25`) and the RPM and Deb
  probes turn each non-200 into a mismatch; the Content View drift maps it to `Fix::Repair` (`cv.rs:1773-1774`, hint
  `actions_cv.rs:740-743`). [R57] (`old`, header guard on a File distribution): `V` → "MISMATCH: served content
  differs: PULP_MANIFEST returned HTTP 403". Fix: report 401 and 403 as "content guard refuses this client: not
  compared".
* **F114 — Task Center: `/` sets a filter that hides nothing.** `Kind::Tasks` is searchable (`kinds.rs:572-574`);
  `reload` and the poll refresh set all rows (`app.rs:950-956`, `:591-595`, `task_items` `:605-607`); the title shows
  the filter (`view.rs:156-158`). [R54]: `/` `zzzz` Enter → "[8 items, filter: zzzz]" with all 8 rows.
* **F115 — After an Edit of the connected profile, the header shows the new URL while every request goes to the old
  server.** `draw_header` (`src/tui/view.rs:48-54`) reads `app.cfg`; `put_profile` (`actions.rs:628-640`) does not
  reconnect; `dst_origin` (`actions_admin.rs:965-967`) gives the edited origin to the replication confirm and to
  `guard_self`. [R55]: connected to `dom` (port 8090); Edit → Server URL port 8091 → the header says "server: dom
  (http://127.0.0.1:8091)"; File Repositories still lists the objects of port 8090. `docs/USER_GUIDE.md:17` says "The
  header shows the active server". Fix: read the URL from `conn.client` while connected and mark "edited: reconnect".
* **F116 — Delete of the task that ran a Pulp import gives a bare "server error [500]".** `server_task_delete`
  (`actions.rs:4060-4073`). `Import.task` is `PROTECT` (`pulpcore/app/models/importer.py:26`), and
  `TaskViewSet.destroy` (`app/viewsets/task.py:239-243`) does not catch it. [R56] (API on `dom`): `DELETE` of the
  completed `pulp_import` task → HTTP 500 with an HTML body. The TUI side is static. Fix: turn the 500 of this
  request into "an import record keeps this task".
* **F117 — Remove (`x`) of an RPM modulemd also removes its packages; the confirm and the result count one unit.**
  `remove_lines` (`actions.rs:3234-3238`), `content_removed` (`:127-133`); `remove_units` (`:106-112`) adds related
  units only for Deb. Pulp: `resolve_module_packages` (F107). [R61]: Content → modulemds → `x` → "Create a new
  version of a8-rpm without 1 unit(s). / The current and older versions are immutable and keep the content." → "[ok]
  removed 1 unit(s): now showing version 5"; Pulp removed 1 modulemd and 1 package. An Add of a modulemd adds its
  packages the same way. Fix: name the package count from the modulemd's `packages` field.

#### P3 findings from code reading (each one passed a disprove review; none was run)

* **F118 — The replication deletion warning says "gone upstream"; a narrower filter deletes replicas that still
  exist upstream.** `deletion_warning` (`actions_admin.rs:925-935`); pulpcore builds the keep list from `q_select`
  (`app/tasks/replica.py:113-155`) and deletes each (labeled) object not in it (`app/replica.py:284-339`).
  `docs/COMPATIBILITY.md:752-754` has the same wording. The typed confirm does show the filter. Fix: "… that the
  distribution filter no longer selects or that are gone upstream".
* **F119 — A failed filesystem export leaves an export record that the list shows like a good one.** `Kind::Exports`
  columns (`kinds.rs:815-819`) have no task state; pulpcore creates the record before the checks
  (`app/tasks/export.py:320-337`). A second export into the same directory always fails
  (`docs/USER_GUIDE.md:873`), so the row is common. Fix: a state column from the export's `task`.
* **F120 — RPM fallback replication: the dry run says CREATE for an upstream distribution that serves nothing.**
  `replication.rs:144-146` skips the publication read for a non-checkpoint RPM distribution, so a distribution that
  follows a repository with no publication is a CREATE row (`:196-198`); the run makes the remote (`:306`) and the
  repository (`:307`), then the sync fails with 404 (`:308`). File and Deb get SKIP (`:149-152`);
  `docs/USER_GUIDE.md:822-823` promises that only for them.
* **F121 — Object roles (`B`) fails for a user without `manage_roles`; a group owner cannot delete the group.**
  `object_roles` (`actions_admin.rs:414-415`) reads `list_roles/` and `my_permissions/` together, and `list_roles/`
  needs `manage_roles_*`; `docs/USER_GUIDE.md:104-106` promises "your permissions on it". The group Delete impact
  reads `{group}roles/` (`:274`), which is administrator only (`pulpcore/app/viewsets/user.py:401-419`), while Pulp
  allows the delete to the owner (`:107-110`). Fix: treat a 403 of these two reads as "not readable for this user".
* **F122 — Plans check names per type, but Pulp names are unique across types.** Fallback replication
  (`replication.rs:188-189`, `conflict` `:220-240`, `ops::distribution_target` `ops.rs:434`) and `import-repos`
  (`repo_export.rs:283-285`) say CREATE or "create" when a repository, remote or distribution of another type has the
  name; the run then gets 400 after it made the remote, or the remote, repository, version and publication
  (`replication.rs:306`, `:369`, `:317`). Pulp side checked by API on `dom`: `{"name": ["This field must be
  unique."]}` for a File repository with the name of a Deb repository. Fix: look the name up on the generic
  `repositories/`, `remotes/` and `distributions/` lists in the plan.
* **F123 — The ownership label of a fallback replica is the source profile name, not the server.**
  `replication.rs:106`, `:305`, `owned` `:77-79`. A profile `up` that now names another server (an Edit, or the config
  of a second operator) gives "SYNC re-sync existing replica" (`:193-195`), and the run PATCHes the replica's remote
  URL (`:339-341`). Same class as F93. Fix: show "remote URL changes: old -> new" in the plan row, or label with
  `Profile::server_key()`.
* **F124 — `db copy` has two gaps.** (a) `--from PROFILE` does not check that both profiles are one server
  (`src/main.rs:124-139`; `Store::copy_from` refuses only the same database, `store.rs:1675-1678`), so the views,
  versions and bindings of server A can be written under server B. (b) A second copy loses the aliases of the first
  database id: `copy_from` writes only `alias:<src.db_id()>` (`store.rs:1855-1859`) and not `src.aliases()`
  (`:628-632`); `ops::label_owner` (`ops.rs:548-554`) then refuses the Pulp objects that still have the first id
  ("owned by another pulp-tui database"), and `add_alias` has no caller in the product.
* **F125 — Unpromote does not delete a distribution that a stopped promotion created but did not record.**
  `bind_repo` creates the distribution (`cv.rs:1532`), checks the lock (`:1534`), then records it (`:1535-1546`);
  `unpromote` reads only `binding_repos` rows (`:1567`, `:1575`); the preview too (`actions_cv.rs:933-957`). After a
  stop in between, `u` says "is not in Dev any more" and the path still serves the version. A later Promote adopts it
  by name.
* **F126 — Server tasks: with the cursor on the first row, a reload moves it to the newest task; the Cancel confirm
  shows only an href.** `set_items` (`app.rs:1034-1038`) keeps the row only for `selected > 0` or the Task Center;
  `Kind::ServerTasks` is ordered `-pulp_created` (`kinds.rs:639-641`); `cancel_confirm` (`actions.rs:4092-4099`) is a
  plain `y` with "Ask Pulp to cancel task {href}." Fix: follow the row on Server tasks and Task groups too; name and
  state in the confirm.
* **F127 — Content screen: Remove or Add picked from the action menu acts on the row selected at that moment, not on
  the row of the menu.** `actions::run` drops the item (`actions.rs:455`); `content_action` (`:3194-3207`) reads
  `app.list.current()`; `show_new_version` (`:137-146`) replaces the list (cursor on row 0, marks gone) when an
  earlier Remove ends, also under an open menu; the confirm names no unit (`:3234-3238`).
* **F128 — Opening Lifecycle Environments writes to the shared database for a read-only profile.** The list loader
  calls `store.ensure_library(scope)` (`kinds.rs:1205-1207`; `store.rs:1120-1122`, `:1935-1950`); also
  `actions_cv.rs:296`, `:863`. A database role without INSERT gets an error instead of the list.
  `docs/USER_GUIDE.md:1208-1212` says such a profile cannot change environments.
* **F129 — RPM "Package signing fingerprint" on pulp_rpm 3.34.0 and 3.27.12.** The help text
  (`actions.rs:1015`, `docs/USER_GUIDE.md:314`) names `v4:<hex fingerprint>`, which those versions refuse
  (`max_length=40`: `pulp_rpm/app/serializers/repository.py:90-99` in 3.34.0, `:74-83` in 3.27.12); clearing the
  field sends null (`repo_body`, `actions.rs:1157`), which 3.34.0 refuses (no `allow_null`).
* **F130 — Deb Verify probes `dists/default/` on pulp_deb 3.5.4 and 3.8.1, which do not publish it.** `deb::layout`
  (`src/pulp/deb.rs:105-107`) adds the pair with no version check; only 3.11.1 publishes an empty structured release
  (`pulp_deb/app/tasks/publishing.py:210`). A structured publication of a version without release components gives
  "MISMATCH: dists/default/Release returned HTTP 404" on the old servers. `docs/COMPATIBILITY.md:934-935` says the
  opposite of what Verify does.
* **F131 — Profile Delete leaves the keyring secret; a new profile with the same name uses it.** `actions.rs:813-839`
  only changes the config; `config.rs:355-369` has no keyring delete; the confirm (`:822-825`) and
  `docs/USER_GUIDE.md:170-172` do not name the keyring. The old secret is then sent to the new URL (`config.rs:350`).
  The entry `[database]:<name>` stays too.
* **F132 — A Content View can be named `.` or `..`.** `validate_name` (`store.rs:2225-2230`) accepts them
  (`validate_label` refuses them, `:2240`); the base path is then `library/../<repo>` (`cv.rs:270-272`). pulpcore
  3.123.1 refuses it (`app/serializers/fields.py:19-28`), so the Library step fails on every publish; 3.95.3 and
  3.63.36 accept it and serve a path that HTTP clients normalise away.

#### Other P3 polish of this audit (static)

* Each upload request has the total timeout `timeout_secs` (default 30 s, `client.rs:143`; chunk 8 MiB): below about
  2.2 Mbit/s every chunk times out and the upload is deleted. `docs/USER_GUIDE.md:1073` names the setting with no
  word about uploads.
* `fs_export_source` (`actions_admin.rs:1633-1634`) reads only APT publications: a Deb version with only a verbatim
  publication gets "has no publication". Use `ops::publications_where`.
* `upstream_body` trims the secret `password` (`actions_admin.rs:1083`); Pulp keeps leading and trailing spaces.
* `patch_repo` (`actions.rs:1215-1230`): a label edit of an RPM repository with an OSV config is two PATCHes; when
  the second one is refused or does not run, the label edit is saved and the OSV config is gone, with no word about it.
* A derived repository deleted outside pulp-tui gives a raw 404 on each Resume (`cv.rs:472`, `:904-909`); same kind
  as F96.
* `delete_view` checks `composite_refs` once (`cv.rs:2022`); a component add by another operator between the Pulp
  deletes and `delete_cv` leaves a view with records and no Pulp objects (raw foreign-key error).
* Filtered Deb snapshots never remove Release, ReleaseComponent and ReleaseArchitecture records (`cv.rs:1113`; on
  purpose, `:1104-1105`); the user documents do not say it.
* The Publish form note says "A preview of the exact content" (`actions_cv.rs:972`), but the run reads the source
  versions again (`cv.rs:650-656`): a sync that ends while the confirm is open is published without a word.
* The "Target repository" picker of Add to repository lists Content View snapshot repositories (`pick.rs:48`) that
  `add_run` then refuses (`actions.rs:3296-3298`).
* The Deb upload comment "Any file, served at `relative_path` of a publication" (`deb.rs:182`) is true only for a
  verbatim publication: an APT publication serves no `.udeb` and no generic content. The upload result does not say
  it. See G21 for source packages.
* `replicate` builds its confirm from the list row (`actions_admin.rs:1145-1152`): a policy changed by another
  operator after the list load is not seen.

Candidate parts rejected by this audit after verification: "the RPM fallback replica does not get the upstream
publish settings" (documented: `docs/ARCHITECTURE.md:359`, `docs/COMPATIBILITY.md:1008`); "an Edit of `q_select` asks
nothing" (the delete runs at Replicate, which asks); the Distribute form and the plain Create forms for cross-type
names (they pass Pulp's 400 through and leave nothing); "Deb same-as-source ignores a publication-level signing
service" (documented, `docs/USER_GUIDE.md:702-703`); the tracing file writer on the UI thread (one small append per
notice); "Production applies at once after a profile Edit" (documented, `docs/USER_GUIDE.md:169`).

## 7. Asynchronous state consistency findings

### What holds (static and runtime)

* Only `completed` is success; groups need every task completed. Canceled and skipped tasks are warnings, failed
  tasks errors. No POST, PATCH, PUT or DELETE is retried (`client.rs:425-433`), redirects are off.
* Dependent steps wait for their tasks; Content View checkpoints store the task href before the wait. A failure moves
  the version to `failed` and keeps the served version ([R31]: v3 `failed`, Library still `v2`).
* Every runtime step of audits 7 and 8 ended with a final Task Center row, and the visible list reloaded (this
  audit: domain create, edit, migrate and delete; upload; sync; publish; replicate; remote delete; Content View
  publish).
* A task that ends while the user is on another screen: [R42] the Dashboard changed "Tasks running 1" to 0 and the
  File count from 0 to 40 with no key pressed, 41 s after the sync started.
* A host switch while a task runs: [R41] the Task Center of the other servers did not show the task, and after the
  switch back the row was `completed`. Connect drops messages and jobs of the previous server; stale loads are
  dropped by generation. What a Connect does **not** drop is the screen and an open form (F98).
* A restart: [R40] rows dismissed with `z` stay gone; [LT:task_center_reattaches_a_task_of_a_previous_session]
  passed in this run.
* Waits: each request has a timeout; `wait_task` and `wait_task_group` give up after 120 s of read errors
  (`POLL_OUTAGE`, `src/pulp/ops.rs:21`); a task
  that stays `waiting` is waited on without limit, as documented (Cancel ends it). A confirm or a form closes before
  its closure runs, so a repeated key does not send a mutation twice.

### Async findings of this audit (audit 8)

| ID | Expected state | Actual state | Screens | Task handling |
|---|---|---|---|---|
| F98 | after Connect, no screen, form or action of the previous server | the sub-list of the previous server stays while "connecting..."; a form opened then is submitted to the new server; "[ok] assign … to user a8-x1: done" names the old object [R48] | every parameterised sub-list (user and group roles, group members, Content View versions) | no task: the POST is synchronous; the client of the new connection is used |
| F107 | the published version has what the preview counted | "Library:v2", preview "+0 added, -0 removed"; the snapshot has no package [R60] | Content Views, Versions, every environment that gets the version | both `modify/` tasks completed; Pulp changed the version between them; nothing compares the result with the selection |
| F106 | "uploaded …, Pulp signed it", list reloaded | "[error] … digest mismatch"; Task Center row `completed`; no reload (static) | Repositories, Content, Task Center | the task and its created resources are right; the check after it is wrong |
| F99 / F105 / F113 | OK, or "not compared: <reason>" | MISMATCH / DRIFT with a repair hint; fallback replication item `failed` after its tasks completed [R44], [R45], [R57] | Distributions, Content Views, Replication | tasks completed; the check after them fails |
| F101 | the confirm names the local objects that change | repository, remote and distribution of the same name changed; the row says `completed`, "tasks 5/5" [R52] | Replication, Repositories, Distributions | the task group is tracked correctly |
| F104 | dry run and run agree | dry run CREATE; run `failed` at the publication, with a synced repository left [R58] | Replication | sync task completed; the next POST is refused |
| F114 | the filter hides rows, or `/` is not offered | title "filter: zzzz", all rows shown [R54] | Task Center | local list |
| F115 | the header names the server that gets the requests | the edited URL [R55] | header, replication confirm | none |
| F116 | a clear refusal | "server error [500]" (static for the TUI; [R56] for Pulp) | Server tasks | the DELETE is synchronous |
| F119 | a failed export is told apart | the Exports list shows one more row like a good one (static) | Exports | the task is shown as failed in the Task Center |
| F126 | Cancel acts on the task the operator looked at | on the first row a reload moves the cursor to a newer task; the confirm shows only an href (static) | Server tasks | — |
| F127 | Remove acts on the row of the menu | the row selected when the menu entry runs, after the list replaced itself (static) | Content | the earlier Remove task ended and replaced the list |

### Async findings of the earlier audits (still open)

| ID | Expected state | Actual state | Screens | Task handling |
|---|---|---|---|---|
| F77 | Task Center: the refresh of an ACS made no task (or the TUI refuses an ACS with no path rows) | `gone`, "its tasks were purged" [R30] | Task Center, Alt. Sources | the empty group is final by `TaskGroup::purged` |
| F79 | "no publication yet" | "[ok] … serves URL"; URL 404 [R32] | footer, notifications | no task; the PATCH/POST is synchronous |
| F80 | dismissed rows stay gone after restart | rows return [R33] | Task Center | local store only |
| F93 | references of another server are not polled | stored as `gone` | Task Center | `reattach_tasks` |
| F78 | the next Publish works | `failed` with 400 every time [R31] | Content Views, Versions | the delete task completed; the next copy task is refused before it starts |
| F74 (audit 6) | a final row keeps its state | a failed late poll changes it | Task Center | still open |

### Inherent limits (by design, documented)

As in audit 6: Inspect, Verify, Compare and previews are snapshots; `follow` callbacks are lost on Connect or restart;
Content View client steps run only while a TUI runs; lock liveness uses the client clock; a superseded publication is
served for a grace period.

## 8. Verification evidence and blockers

### Commands

| What | Command (summary) | Result |
|---|---|---|
| Reset + full suite (`latest`) | scratch script: `tests/env/pulp-env.sh down-all`, then each scenario in its own `CARGO_BUILD_JOBS=2 systemd-run --user --scope -p MemoryHigh=6G -p MemoryMax=10G -p CPUQuota=200% --quiet nice -n 19 tests/run-live.sh <scenario>` call; the exit code of each call was recorded | every call exit 0, 2757 s |
| Unit + mock HTTP | part of each call: `cargo test --lib --test mock_http` | 685 passed; 53 passed (in each of the 11 calls) |
| Live scenarios | repos / cv / rbac / failures / replication / shared_db / deb / export / content / admin | 44 / 27 / 2 / 3 / 12 / 10 / 16 / 8 / 11 / 29 passed (162), 0 failed |
| Real upstreams | `real` (Debian `main`, Rocky 9) | 3 passed (call 681 s) |
| Format, advisories | `cargo fmt --check`; `cargo audit` | exit 0; exit 0 (RUSTSEC-2026-0459 ignored with a reason in `.cargo/audit.toml`; the same 4 allowed warnings) |
| Markdown | `dprint check` | exit 20: "Found 1 not formatted file", this report. Every other Markdown file passes. The report is tracked since `f5f6aca`, and the committed audit-7 text fails the same way (`dprint fmt --stdin` changes 792 lines of it): dprint wants padded table columns, which would double the size of this file (196 KB → 429 KB). The tables were left compact, as in the committed version. CI does not run dprint (`.github/workflows/` has no dprint step), `AGENTS.md` lists it. Decide: exclude `docs/audits/` in `dprint.json`, or format the file |
| Status and schema | `GET status/` and `GET docs/api.json` of pulp1; `status/` of `dom` and `old` | versions in section 2; 1200 operations, 596 in scope (section 3) |
| Plugin source | `podman create` + `podman cp` of `pulpcore`, `pulp_rpm`, `pulp_deb`, `pulp_file`, `pulp_certguard`, `rest_access_policy` out of the `latest`, `3.95.3` and `3.63.36` images into the scratch folder | every Pulp `file:line` of section 6.2 |
| Runtime TUI | `pulp-tui v0.6.0` (debug build of the suite) in tmux 200×50 on its own socket, `set-clipboard on`, own `--config` and `--data-dir`, poll 500–3000 ms | R35–R62 below |
| Test count | audit 7's script over `tests/live_*.rs` (the tests differ from audit 7 by one `session_log::clear()` call) | 99 of 162 live tests drive the TUI (165 with `real`) |

Not run: clippy (no code change since audit 6 ran it, but for a five-line test helper), `podman build -f
Containerfile .`, mutation tests, the suite on 3.95.3 and 3.63.36 (audits 5 and 6 ran both at this code). The
findings of this audit that name 3.63.36 were run on a 3.63.36 server; nothing was run on 3.95.3.

### Runtime runs

R0–R28 are the runs of audits 3–6 and R29–R34 those of audit 7; the matrix still cites them where the code path did
not change. R35–R62 are the runs of this audit. Servers: `dom` = own container of the `latest` image with
`DOMAIN_ENABLED = True` (port 8090; restarted once without `CONTENT_ORIGIN` for R44, R45 and once with a
container-reachable origin for R58); `old` = own container of the `3.63.36` image (port 8091); pulp2 of the suite
only as the Connect target of R48. Objects are named `a8-*`; they went with the two containers at the end.

| Run | Steps | Result |
|---|---|---|
| R35 domains (`dom`) | Domains → `c` (name `a8x`, FileSystem, settings JSON) → `Ctrl-S`; `e` (description, "Hide guarded distributions") → `Ctrl-S`; `M` (new location) → `Ctrl-S` → typed `a8x` | "[ok] create domain a8x: done"; "task completed: update domain a8x"; `POST /pulp/a8x/api/v3/domains/migrate/` → "task completed: migrate the storage of domain a8x"; Pulp and the list agree after each step |
| R36 F89 (`dom`) | API: header guard `a8-g` is the `default_content_guard` of domain `a8x`. Profile in domain `a8x`: Content Guards → `d` → typed name | "Delete content guard a8-g (core/header). No distribution uses it."; after: `default_content_guard: null` |
| R37 F90 (`dom`) | same profile: Domains → `a8x` → `d` → typed `a8x` | no line about the profile's own domain; "task completed: delete domain a8x"; then "error: not found [404]" on every screen |
| R38 Copy URL (`dom`) | Distributions (File) → `a8-f` → `y`; `tmux show-buffer` | "[ok] copied http://127.0.0.1:8090/pulp/content/default/a8-f/"; the buffer holds that URL |
| R39 Artifacts (`dom`) | upload `one.txt` through the TUI; Artifacts; `/` + another sha256; `/` + its sha256; `i` | 1 artifact listed; 0 items; 1 item; Inspect shows the digests |
| R40 dismiss (`dom`) | Tasks → `z`; restart; Tasks | "dismissed 3 finished task(s) from the list (Pulp keeps its records)"; 0 rows; 0 rows after the restart |
| R41 host switch (`dom`) | profile in domain `a8s`: sync of `a8-slow` (40 files, `rate_limit` 1, 41 s); Servers → Connect `pulp1`; Connect `dom`; Connect back | Task Center of `pulp1` and `dom`: 0 items; after the switch back: `completed`, "sync a8-slow (additive)" |
| R42 Dashboard (`dom`) | profile in domain `a8t`: sync started, then Dashboard, no key | "Tasks running 1", "File content 0" → "Tasks running 0", "File content 40", "[ok] task completed: sync a8-slow (additive)" |
| R43 G21 (`dom`) | Deb repository `a8-deb`: `u` a `.deb` (Distribution `stable`, Component `main`), `u` `fixture-src_1.0-1.dsc`; `P` (Simple true, Structured default); `D` | "new version 2"; `dists/stable/main/source/Sources` 200, 0 bytes; `dists/default/all/source/Sources` lists `fixture-src` |
| R44 F99 (`dom` without `CONTENT_ORIGIN`) | Distributions (File) → `a8-big` → `V`; `y`; Dashboard | "MISMATCH: unsafe href rejected: /pulp/content/default/a8-big/PULP_MANIFEST"; copied "/pulp/content/default/a8-big/"; "content:" empty |
| R45 F99 (same server) | Content Views → `c` `a8cv` → `a` `a8-f` → `P` → `y`; `D` | "[ok] a8cv version 1 published and active in Library"; "Library: v1 DRIFT / a8-f: unsafe href rejected: … / … repair the snapshot version …" |
| R46 F100 (`dom`) | domain `a8u`: File remote `a8-od` (`on_demand`), synced, autopublish, distribution; `f1.bin` fetched. Remotes → `d` → typed name | confirm "Repository content and versions are not affected."; after: `f1.bin` 200, `f3.bin` 404, the version has 40 units |
| R47 F110 (`dom`) | Roles → `c` → name, empty Description → `Ctrl-S` | "[error] create role a8-role: rejected [400]: description: This field may not be blank." |
| R48 F98 (`old` → pulp2) | Users → `a8-x1` (`users/2/`) → `R`; `:` "Connect: pulp2"; `c` after 0.2 s; role `file.fileremote_viewer`; `Ctrl-S` | form "Assign role to user a8-x1" stays over the Dashboard of pulp2; "[ok] assign file.fileremote_viewer to user a8-x1: done"; on pulp2 `users/2/` (`a8-carol`) has the role; `a8-x1` has none |
| R49 F102 (`old`) | Deb Repositories → `c` → name → `Ctrl-S` | "[error] create repository a8-olddeb: rejected [400]: autopublish: Unexpected field" |
| R50 F103 (`old`) | API: RPM repository with `repo_config {"gpgcheck":1,"repo_gpgcheck":1}`. `e` → Description → `Ctrl-S`; Logs | body `{"description": "audit 8"}`; "task completed"; Pulp `repo_config: {}` |
| R51 F112 (`old`) | File Repositories → `o` → `/` `a` Enter | "Error: rejected [400]: errors: Invalid Filter: 'relative_path__icontains'" |
| R52 F101 (`dom`) | local File repository and distribution `a8-take` (own file, label `owner=team-a`); Upstream Pulp `a8-self` (its own domain `a8t`, policy `nodelete`, filter `name="a8-take"`); Replication → `R` → `y` | repository version 2: +40, −1 (the local file is not in it), labels `{"UpstreamPulp": …}`; distribution: label gone, `repository: null`, `repository_version` set; remote `a8-take` made; Task Center `completed`, "tasks 5/5" |
| R53 F111 (`dom`) | append `[profiles.outside]` to the config file; Screens pane → `x` | "[ok] Replication hidden"; the profile block is gone from the file |
| R54 F114 (`dom`) | Tasks → `/` `zzzz` Enter | "[8 items, filter: zzzz]", 8 rows |
| R55 F115 (`dom`) | Servers → `dom` → `e` → Server URL port 8091 → `Ctrl-S`; File Repositories | header "server: dom (http://127.0.0.1:8091)"; the list is the one of port 8090 |
| R56 F116 (`dom`, API) | exporter, export, importer, import of `a8-f`; `DELETE` of the `pulp_import` task | HTTP 500, HTML "Server Error (500)" |
| R57 F113 (`old`) | File distribution with a header content guard; Distributions → `V` | "MISMATCH: served content differs: PULP_MANIFEST returned HTTP 403" |
| R58 F104 (`dom` → `old`) | connected to `old`: Replication → `F` → Source `dom`, Types `deb`, name `a8-deb` → `y` | dry run "CREATE Deb  a8-deb -> /a8-deb  new remote + repository + distribution"; "failed    Deb a8-deb: rejected [400]: excluded_package_metadata_fields: Unexpected field; layout: Unexpected field; publish_legacy_release_files: Unexpected field"; left: remote, repository at version 1 |
| R59 (`dom`, API) | `modify/` that removes modulemd `a8mod:1:1` from `a8-rpm` | the new version: removed `rpm.modulemd 1, rpm.package 2` |
| R60 F107 (`dom`) | view `a8mod` on `a8-rpm`, rule `exclude name=*-debuginfo`; `P` v1; modulemd replaced by `a8mod:1:2` (lists beta), packages still in the source; `P` v2; `P` v3 | v1 snapshot: 2 packages; v2 preview "+0 added, -0 removed", "Library:v2", snapshot: 0 packages, 1 modulemd, 1 advisory; v3 preview "+2 added", snapshot: 2 packages |
| R61 F117 (`dom`) | `a8-rpm` → `o` → modulemds → `x` → `y` | "Create a new version of a8-rpm without 1 unit(s)."; "[ok] removed 1 unit(s): now showing version 5"; Pulp: removed `rpm.modulemd 1, rpm.package 1` |
| R62 F109 (`dom`, profile of user `a8-u1` with only `core.headercontentguard_creator`) | as `a8-u1`: Content Guards → `c` → header guard `a8-ug`; API as administrator: `a8-ug` on File distribution `a8-big`; as `a8-u1`: `d` → typed name | "Delete content guard a8-ug (core/header). No distribution uses it."; "[ok] delete content guard a8-ug: done"; `a8-big`: `content_guard: null`, `PULP_MANIFEST` 403 before and 200 after, without the header |

Runs of audit 7 (kept for the open findings):

| Run | Steps | Result |
|---|---|---|
| R29 F76 | API: remote `a7-rem` (immediate), `a7-src` synced (3 units), `a7-copy` gets the 3 units by `modify/`. File Repositories → `a7-src` → `R` → `Ctrl-S` → typed name | confirm "…that no other repository has."; "[ok] … deleted 3 file(s)"; `a7-copy` units `artifact: null` |
| R30 F77 | API: File ACS `a7-acs1`, `a7-acs2` with no paths (`[""]`). Alt. Sources → `t` → `a7-acs2` → `e` → Paths `sub/` → `Ctrl-S`; then `R` on `a7-acs1` | no dialog; `a7-acs1` `[]`; refresh group "Refreshing 0 alternate content source paths", Task Center `gone` "its tasks were purged" |
| R31 F78 | `a7-os` (RPM, empty). `a7cv` + `a7-os`, `P` v1 (`versions/0/`); sync; `P` v2; Versions → v1 → `d` → `v1`; `P` v3 | the delete lists `…/versions/0/`; Pulp keeps only version 1; v3 "rejected [400]: Version 0 does not exist …", `failed`; Library `v2` |
| R32 F79 | `a7-src` → `D` → Binding "follow repository latest" → `Ctrl-S` | "[ok] distribution a7-src serves …/a7-src/"; 404; 0 publications |
| R33 F80 | `:` "Tasks: dismiss finished"; `q`; start again; `:` Tasks | "dismissed 10 finished task(s)", 0 items; after restart 10 items |
| R34 F81 | RPM Repositories → `a7-os` → `e` → Checksum type `sha224` → `Ctrl-S` | choices `sha224 sha256 sha384 sha512`; "[error] … rejected [400]: … sha256, sha384, or sha512." |

### Verification of the candidates

* This audit: 6 search reviewers with no earlier context, each with another cut than audit 7 (a field-by-field
  contract sweep of repositories, remotes and uploads against the serializers of the three images; impact previews
  against Pulp's cascades and Verify per plugin; pulpcore administration with a non-admin lens; Content View
  algorithms, crash points and the store; replication, definitions, CLI, config and the HTTP client; the app shell
  with selection identity, Connect and the async checklist). They proposed 43 candidates. Two reviewers found F99 and
  F105 independently.
* The lead reproduced 18 of the candidates at runtime (R43–R61; F109 later, R62). 3 disprove reviewers with no earlier context
  attacked every candidate that was not reproduced yet, and the polish items, and listed every action that F98
  reaches. Most were confirmed, some with corrected line numbers. Six were cut back to the part that holds: F109 (the
  version Delete part is a false line only), F115 (the Production part is documented), F118 (not the Edit), F120 (the
  publish-settings part is documented), F122 (not the Distribute and Create forms), the F94 extension (not the log
  writer). Two were rejected: the preview and the run read the source versions separately (an inherent limit, kept as
  a polish line for its wording), and Deb "same as source" signing (documented).
* Final: 35 findings F98–F132 (11 P2, 24 P3), 2 gaps (G21, G22), 11 polish items. Evidence: 17 findings and G21
  runtime verified (F116 on the Pulp side only, by API; F109 for the guard Delete, run after its disprove review);
  18 findings and G22 static, each with pulp-tui and Pulp lines that a second reader confirmed.
* Line numbers: the lead read again the cited pulp-tui lines of F98–F103, F106–F108, F110–F112, F117, F118, F120,
  F121, F124 (b), F126, F128–F130, F132 and G21. The pulp-tui lines of the other findings (F104, F105, F109,
  F113–F116, F119, F122, F123, F124 (a), F125, F127, F131) and every Pulp line number are from a reviewer and were
  confirmed by a disprove reviewer; they are not read a third time.
* Audit 7: 5 search reviewers, 24 candidates, 22 findings F76–F97, G20; F76–F81 reproduced at runtime then.

### Blockers

* ULN sync needs an Oracle ULN account.
* No object storage (F105), no package signing service (F106), no TLS-terminating outer proxy (F92) in the test
  environment.
* A user that is not a superuser was set up for one run (F109, [R62]); F121 and the other paths of F109 are static.
* F91 needs a 3.95 or 3.63 server with a running replication; F95–F97 need multi-profile or rename set-ups; not run.
* Two requests of this audit were refused by the session's permission layer. (1) A second fixture HTTP server on
  all interfaces: the slow-sync runs used content that the `dom` server served itself, so nothing was lost. (2) The
  removal of 10 anonymous podman volumes that the containers of this session left (pulp1, pulp2, `dom`, `old` and
  three containers made only to copy the plugin source): they are still there. All containers and all named volumes
  are gone (`down-all` and the removal of the two audit containers). `tests/env/pulp-env.sh down` leaves such
  anonymous volumes after every run (158 volumes of earlier days are on this machine); `podman volume prune` removes them.
* Resolved in this audit (were blockers until audit 7): domains (a server with `DOMAIN_ENABLED`), the OSC 52
  clipboard (tmux `set-clipboard on` and `show-buffer`), claims about 3.63.36 (a server of that image).

## 9. Prioritized remediation backlog

No P0 and no P1. The rows of this audit come first, then the open rows of audits 7 and 6. Inside a priority the order
is the suggested order of work.

| Pri | ID | Action | Plugins / features | Likely fix location | Acceptance criteria | Suggested test |
|---|---|---|---|---|---|---|
| P2 | F98 (RC4) | On Connect, leave every parameterised sub-list (open the Dashboard or the root list at once) and close an open form, menu or confirm; offer no action on a kind that needs a connection while `conn` is `None`; check the scope of the view in the `CvVersions` load | all servers; users, groups, Content View versions | `connect_now` `app.rs:803-807`, connect result `:825-838`, `available_actions` `:1414-1427`, `kinds.rs:1172-1178` | from "Roles of user X", Connect then `c` opens no form; a form open before the connect result is closed; the new server gets no `POST users/<n>/roles/` | unit: `connect_now` on `Kind::UserRoles`, then `c`; mock: two servers, assert no POST on B; live: R48 |
| P2 | F107 (RC8) | Make the filtered RPM snapshot equal to the selection: send the modulemd removals with the adds in one `modify/`, or run `changes` again after the steps and add what is missing; compare the final keys with the selection before the version is recorded | RPM Content Views with filters | `CvService::snapshot` `cv.rs:972-1015`, `changes` `:1098-1118` | R60: v2 has both packages; the preview and the snapshot agree | live: a fixture with a modulemd (`m:1:1` {a, b} replaced by `m:1:2` {b}), filtered view, two publishes, assert snapshot keys = selected keys; mock of the request order |
| P2 | F99, F105, F113 (RC5) | One served-content fetch for all checks: join a `base_url` that starts with `/` with the profile URL (the source profile URL in the fallback plan); follow a few redirects by hand with no `Authorization` to another origin and a masked query in the log; report 401 / 403 as "content guard refuses this client: not compared", not as MISMATCH; Copy URL and the Dashboard use the joined URL | all types: Verify, Rollback check, Content View drift, fallback replication, Copy URL | `fetch_content` `client.rs:346-361`; `rpm.rs:220-251`, `file.rs:22-25`, `deb.rs:79-166`; `cv.rs:1768-1774`; `replication.rs:126-164`, `:320`; `actions.rs:3655-3670` | R44, R45: OK for a correct distribution without `CONTENT_ORIGIN`; a mock 302 is followed; a mock 403 gives "not compared" | mock HTTP: relative `base_url`; 302 to a second mock server (assert no auth header there); 403; live: one server of the test environment without `CONTENT_ORIGIN` |
| P2 | F100 (RC1) | Read the remote's `policy`; for a policy other than `immediate` replace "not affected" by "content of this remote that Pulp has not downloaded yet cannot be served any more"; name every repository that has such content where it can be read cheaply, else say so | all remote types | `remote_delete_impact` `actions.rs:1477-1492`; `docs/USER_GUIDE.md:338-342` | the confirm of an `on_demand` remote has that line and no "not affected" | unit of the lines; live: R46 (404 after the delete, and the confirm said so) |
| P2 | F101 (RC1) | Before the confirm say what Pulp does for each policy: "a local repository, remote or distribution with the name (3.123: or the base path) of an upstream distribution is taken over: content replaced by a mirror sync, labels replaced, distribution re-pointed"; list the local distributions without this upstream's label when the filter is empty or simple; typed name for every policy; correct the form note | native replication | `replicate` `actions_admin.rs:1138-1169`, note `:1060`, `deletion_warning` `:925-935`; `docs/USER_GUIDE.md:801-817`, `docs/COMPATIBILITY.md:707-755` | R52: the confirm names `a8-take` and asks for the typed name | unit of the confirm lines; live: local repository + distribution with the upstream name |
| P2 | F102, F112, F129, F130 (RC6) | Gate by the server schema: `autopublish` in the repository form (`has("autopublish")`); the File path search (`relative_path__icontains` only when the filter exists, else exact `relative_path` or a note); the fingerprint help and `""` for a non-nullable field; `dists/default/` only for pulp_deb with the empty-release branch. Add a live test that creates a repository of **each** type through the form | Deb 3.5.4; File 3.95 / 3.63; RPM 3.34 / 3.27 | `actions.rs:1106`, `:1015`, `:1157`; `kinds.rs:598`; `deb.rs:105-107` | R49 creates the repository; R51 searches or says why not | unit: `repo_form(Deb, caps without autopublish)` has no such field; live on the 3.63.36 image: `Act::Create` for File and Deb repositories, File Content search |
| P2 | F103 (RC6) | On pulp_rpm with the `repo_config` shim (schema has the deprecated `gpgcheck`), send the current `repo_config` in every RPM repository PATCH, also from `copy_settings`; record it in `docs/COMPATIBILITY.md` | RPM 3.27.12 | `repo_body` / `patch_repo` `actions.rs:1121-1230`; `cv.rs:1024-1058` | R50: `repo_config` unchanged after a description edit | live on 3.63.36: set `repo_config`, edit the description through the TUI, read it back |
| P2 | F104 (RC6) | Filter the publication mode by the destination schema; `find_publication` ignores keys the destination lacks | Deb fallback replication across versions | `actions_admin.rs:1328`, `ops.rs:278-348`, `replication.rs:257-260`, `:313-316` | R58 ends with a served distribution | unit: destination caps with `simple, structured` only → the body has no `layout` |
| P2 | F106 | When the repository has `package_signing_service`, do not compare with the local digest: report the server digest and "Pulp signed the package", and reload | RPM; Deb 3.11 | `ops::uploaded` / `check_digest` `ops.rs:935-967`, `upload_file` `actions.rs:1795-1818` | an upload into a signing repository ends "[ok]" and the list reloads | mock: repository with the service, content with another sha256; live when the environment gets a package signing service |
| P2 | F109, F121 (RC7) | One helper next to `ops::sees_everything`: when the user does not see everything, replace each "No distribution uses / serves it" by "none that this Pulp user may view (others can exist)" and ask for the typed name; treat a 403 of `list_roles/` and `{group}roles/` as "not readable for this user" | guards, repositories, publications, versions; object roles; groups | `actions_admin.rs:505-518`, `:635-641`, `:414-415`, `:274`; `actions.rs:1456-1466`, `:2859-2871`, `:3589-3595`; `ops.rs:478-484` | a scoped user never reads "No distribution uses it"; `B` shows "my permissions" | mock: `access_policies/` 403 + empty lists; live in `live_rbac`: guard owner without distribution view |
| P3 | F108 | Do not reuse an RPM publication whose mode fields differ from the snapshot repository, and never for a repository that signs metadata | RPM Content Views | `CvService::publication` `cv.rs:1065-1068`, `ops::reusable_publication` `ops.rs:318-323` | after a failed publish that set a signing service, the next publish POSTs a publication | mock next to `an_rpm_publication_is_reused_or_recorded_before_the_wait` |
| P3 | F110 | Send null (or leave out) an empty role description | roles | `role_form` `actions_admin.rs:362` | R47 creates the role | mock (change the body assert); live Create with no description |
| P3 | F111 | Apply each config change to a fresh `Config::load` of the file, not to the config of the TUI start | config | `config.rs:497-529`; `app.rs:1246`, `actions.rs:630`, `:780`, `:833` | R53: the outside profile stays | unit: outside add, then save |
| P3 | F114 | Filter `task_items` with the same `keep`, or make Tasks not searchable | Task Center | `app.rs:605-607`, `kinds.rs:572-574` | R54 shows only matching rows | unit |
| P3 | F115 (RC4) | Header and `dst_origin` read the URL of `conn.client`; mark an edited profile "reconnect" | shell, replication | `view.rs:48-54`, `actions_admin.rs:965-967` | R55: the header keeps port 8090 until Connect | unit of the header text |
| P3 | F116 | Turn the 500 of a task Delete into "an import record keeps this task: delete the importer first" (or read `imports/` first) | server tasks | `server_task_delete` `actions.rs:4060-4073` | clear text | mock 500 |
| P3 | F117 | For a modulemd in a Remove or Add, add "and its N packages" from the `packages` field; count them in the result | RPM content | `remove_lines` `actions.rs:3234-3250`, `content_removed` `:127-133`, `add_to_repo` | R61 names the package | unit of the lines |
| P3 | F118 | Wording: "… that the distribution filter no longer selects or that are gone upstream"; same in the docs | native replication | `deletion_warning` `actions_admin.rs:925-935`, `docs/COMPATIBILITY.md:752-754` | text | unit |
| P3 | F119 | A state column in Exports from the export's `task` | exporters | `kinds.rs:815-819` | a failed export row says failed | unit of the columns |
| P3 | F120, F122 | Fallback plan: read the publication for RPM distributions too (SKIP "serves nothing"); look names up on the generic `repositories/`, `remotes/`, `distributions/` lists and report another type as a conflict; the same lookup in the `import-repos` plan | fallback replication, import-repos | `replication.rs:144-146`, `:188-240`; `repo_export.rs:283-289` | the dry run says SKIP / conflict; the run makes nothing | mock plan tests |
| P3 | F123, F124 | Fallback: show "remote URL changes: old -> new" for an owned replica (or label with `server_key()`). `db copy`: refuse `--from` a profile of another server key (with an override flag); copy `src.aliases()` too | replication; CLI, store | `replication.rs:77-106`, `:190-196`; `main.rs:124-139`; `store.rs:1855-1859` | plan row names the URL change; A → B → C keeps alias A | unit: two-hop copy next to `store.rs:3274`; mock plan |
| P3 | F125 | Unpromote and its preview also look up the derived distribution names of the attempted version and delete the owned ones | Content Views | `cv.rs:1567-1581`, `actions_cv.rs:933-957` | an unrecorded labeled distribution is named and deleted | mock |
| P3 | F126, F127 | Server tasks and Task groups follow the selected row on reload; the Cancel confirm names task and state. Pass the menu item into `content_action`; name the first units in the Remove confirm | shell | `app.rs:1034-1038`, `actions.rs:4092-4099`; `actions.rs:455`, `:3194-3250` | the target is the row the operator saw | unit: `set_items` on `Kind::ServerTasks`; menu on row 2, `replace`, dispatch |
| P3 | F128 | The Environments loaders only read (show a default Library row when none exists); only writes call `ensure_library` | shared database | `kinds.rs:1205-1207`, `actions_cv.rs:296`, `:863` | no INSERT from a read-only profile | unit with an empty store |
| P3 | F131 | Add a keyring delete and call it (in `spawn_blocking`) after a profile Delete, for both users; at least a confirm line | config | `actions.rs:813-839`, `config.rs:355-369` | no entry left; the confirm names it | unit of the confirm lines |
| P3 | F132 | Refuse `.` and `..` in `validate_name` | Content Views | `store.rs:2225-2230` | refused | unit |
| P3 | G21 | Add `content/deb/source_release_components/` to the Deb record Create (form: source package, release component); say in the upload result of a `.dsc`, `.udeb` or generic file which publication serves it | Deb | `deb::RECORDS` `deb.rs:355-356`, the record forms `actions.rs:3329-3506` | R43: `dists/stable/main/source/Sources` lists the package after the record is made | live next to `the_tui_creates_release_records_that_a_publication_serves` |
| P3 | T07 | Live tests through the TUI for F98–F104, F107, F110–F117 as listed above; a form Create test for every repository type; one scenario on a server without `CONTENT_ORIGIN`; run the form tests on the old images | various | `tests/live_*.rs`, `tests/env/pulp-env.sh` | each test fails before its fix | — |
| P3 | polish (audit 8) | The 11 items of "Other P3 polish of this audit" in section 6.2; add `actions.rs:1695`, `:1840` to F94; fix F67 in `save_distribution` too | various | section 6.2 | — | unit |
| P2 | F76 | Say what Pulp does: the files of every synced unit of the repository go, also of units that other repositories (Content View snapshots) share; name those repositories (or at least the snapshot repositories) in the impact; fix `COMPATIBILITY.md:367-369`, `USER_GUIDE.md:273-274` | all types | `reclaim` `actions.rs:1995-2040` | the confirm of a repository that shares units names the other repositories; no "no other repository" text | live: R29 (two File repositories, check that the confirm names `a7-copy`); unit of the lines |
| P2 | F77 | Build `old` from the raw `paths` (keep `""`, shown as "(the remote URL itself)"), so the existing scan names every other ACS with `""` and asks first | RPM, File ACS | `acs_edit` `actions.rs:2711-2760`, `acs_paths` `:2652` | an edit of `[""]` → `["sub/"]` lists the other ACSs with no paths and sends nothing before the confirm | mock: item `{"paths": [""]}`, ACS list with another `[""]`; live: R30 |
| P2 | F78 | Never delete `…/versions/0/` in `row_objects` (keep it with "base of every unfiltered copy", the rule of `drop_unused_versions`) | RPM, Deb Content Views | `cv.rs:2055-2084` | v1 that records `versions/0/` deletes only its publication; v3 publishes | mock (`delete_version_impact` keeps v0); live TUI: R31 |
| — | RC1 | Each confirm text that states a Pulp rule (reclaim, restore, staff, follow) gets a live test that checks the rule on the server | all | tests | — | — |
| P3 | F79 | With no publication, say "no publication yet: publish (P) to serve" | all types | `follow_repository` `actions.rs:2393-2417`, `distribute_form` `:2287-2291` | R32 gives that text | mock |
| P3 | F80 | Call `a.dismiss_finished_tasks()` and `a.reload()` in the palette entry | Task Center | `app.rs:1609-1615` | R33: no row after restart | unit with an in-memory store |
| P3 | F81 | Keep only `sha256`, `sha384`, `sha512` (with empty) for RPM checksum choices | RPM | `enum_choices` `actions.rs:966-970` | no `sha224` choice | unit with the real enum |
| P3 | F82 | Skip the package probe for `nested_by_digest` (or compute `Packages/<l>/by-digest/<sha[:6]>-<file>` when the checksum is sha256) | RPM | `rpm.rs:196-256` | Verify of such a publication says OK | mock HTTP |
| P3 | F83 | Refuse File `mirror` on an autopublish repository before the confirm (pulp_file 3.123) | File | `sync_refusal` `actions.rs:1370-1385` | refusal text, no request | unit table case |
| P3 | F84 | Drop `osv.rpm.config` from `pulp_labels` in `entry` and `create_body` when the schema has `osv_config` | RPM | `repo_export.rs:47-125` | import of an exported OSV repository creates it | unit |
| P3 | F85 | Remove "newer versions are kept"; use `latest_goes` / the retention impact and the typed name when versions go | all types | `restore_lines` `actions.rs:3113-3125`, `restore_preview` | Retain 1 + unserved latest → typed confirm naming the versions | mock |
| P3 | F86 | Label "Staff (Django flag; no Pulp permissions)"; drop the escalation and lockout texts for staff; fix the docs | pulpcore users | `actions_admin.rs:71-172`, `actions.rs:4261`, `ARCHITECTURE.md:576`, `USER_GUIDE.md:902` | texts match pulpcore | unit; live: a staff user still gets 403 |
| P3 | F87 | Compare group members with `me()` in group role removal, group delete and object roles; typed confirm for self-disable | pulpcore users/groups | `actions_admin.rs:96-153`, `:193`, `:270-288`, `:452` | "YOUR account" lines; typed self-disable | unit |
| P3 | F88 | Read the exporter; when `last_export` is this record, add the start-versions line | pulpcore export | `export_action` `actions_admin.rs:1640-1654` | the line only for the last export | mock |
| P3 | F89 | With `Domain.default_content_guard`, list the domains whose default it is | pulpcore guards (domains) | guard Delete `actions_admin.rs:505-537` | line names the domain | mock |
| P3 | F90 | Warn (or refuse) when the domain is the profile's | pulpcore domains | `domain_action` `actions_admin.rs:860-888` | WARNING line | unit |
| P3 | F91 | Use the `runs_reserve_no_upstream` fallback in the Delete impact | replication (3.63, 3.95) | `upstream_delete` `actions_admin.rs:1255-1283` | a running run is named on 3.95 | mock with a 3.95 reservation |
| P3 | F92 | Accept a `next` link with the same host and port and an API-root path whatever the scheme; request it on the configured base | all lists | `client.rs:240-275` | mock: `next` with `https` on an http mock is followed on the mock | mock HTTP (keep the foreign-host case) |
| P3 | F93 | Store `server_key()` with each reference; re-attach only the current key's | Task Center | `store.rs:659-708`, `app.rs:855-873` | no `gone` after a profile Edit | unit |
| P3 | F94 | Save the config in `spawn_blocking`, apply in the closure | config | `save_config` `actions.rs:605`, `app.rs:1246` | no file I/O in key handlers | unit |
| P3 | F95 | Refuse a view name that another local scope of the same server key uses (or use one scope per server key) | Content Views | `App::scope` `app.rs:356-371`, `store.rs` | the second profile cannot publish into the first's objects | store unit |
| P3 | F96 | Check the recorded source version on Resume; name it and say "delete vN and publish again" | Content Views | `cv.rs:783-827`, `:955-959` | clear refusal, no copy | mock |
| P3 | F97 | In Rollback/Promote of a renamed row, name (or unbind) the other derived-name distribution; drift lists unrecorded derived distributions | Content Views | `promote_target` `cv.rs:1485-1505`, `bind_repo` `:1523-1546` | preview names it | mock |
| P3 | T06 | Live tests for F76–F81 as listed; the T05 tests | various | `tests/live_*.rs` | each fails before its fix | — |
| P3 | polish | ULN "-" help; ACS "1 path(s)"; Watch of a finished row reads the task; an empty group is not "purged" | various | section 6.2 | — | unit |
| P2 | F66 | Build the "becomes public" lines of the Delete with `guard_reach`: every distribution behind a composite that has no guards left, at any depth; say "No distribution uses it" only when that list is empty | pulpcore content guards | Delete impact `actions_admin.rs:505-531`, `guard_reach` `:671-684` | Delete of a guard that is the only member of X, with X in Y on distribution D, names D as PUBLIC | unit (two composite levels); live TUI next to `content_guard_create_edit_and_delete_name_its_distribution` |
| P3 | F67 | For a new distribution, run `ops::distribution_target` (read only) in `confirm_distribute` before the job, or call it in `distribute` before `pinned_publication` | all repository types | `confirm_distribute` `actions.rs:2312-2314`, `distribute` `:2354-2362` | `D` onto an overlapping path refuses with no POST | mock (no POST of a publication before the 409) |
| P3 | F68 | Refuse follow mode onto a checkpoint distribution (as for a pin), or give a checkpoint "will serve" line | all repository types | `confirm_distribute` `actions.rs:2319-2322`, `Binding::line` | The confirm says what Pulp then serves | unit |
| P3 | F69 | Turn the `named_distribution` collision into a refusal line in `bind_target` | Content Views | `cv.rs:1465-1477` | A collision gives a line, the confirm opens, the run publishes | unit |
| P3 | F70 | No Library lines (or refuse at once) for a `deleted` / `deleting` version | Content Views | `resume_library_step` `actions_cv.rs:883-886` | Resume of a deleted version refuses with no confirm | unit |
| P3 | F71 | Report the ownership loss in `verify_repo` with its own hint (restore the labels or delete the distribution) | Content Views | `cv.rs:1733-1748`, `bind_hint` `actions_cv.rs:754-760`, `owner_check` `ops.rs:500-515` | An unlabeled served distribution is a drift with the right hint | unit |
| P3 | F72 | Show the `publish` refusals for a source without versions and for a snapshot source in the preview | Content Views | `preview` `cv.rs:1849-1851` | The preview ends in the refusal | unit |
| — | F69–F72 root cause | Previews still have their own checks for the publish step (as F55–F58 had for the bind step): extend "previews ask the run" to `publish` and `resume` | Content Views | `cv.rs` | — | — |
| P3 | F73 | Name the replaced units (same repository key) and run `confirm_if_latest_goes` in Add, Copy and an overwriting upload | all repository types | `add_to_repo` `actions.rs:3275`, `copy_preview` `:1916`, upload | Add of a same-path File unit names the replaced unit; with the retention, the typed confirm | mock |
| P3 | F74 | Return early from `poll_failed` and `stop` for a final row; add `WHERE finished_at IS NULL` to `finish_task` | Task Center | `tasks.rs:125-142`, `store.rs:677-686` | A failed late poll keeps `completed` | unit |
| P3 | F75 | Load the holders (as the Delete does) and ask for the role name when the permissions change | pulpcore roles | `role_form` `actions_admin.rs:344-369` | A permission edit of a held role names the holders; a description edit saves at once | unit |
| P3 | T05 | Tests: live F53 on Deb and for Retain checkpoints; live checkpoint-publication confirm; live TUI test of F56 (a row with a publication); live TUI test for F59/F60 (now service level); tests for F66–F75 when fixed | various | `tests/live_repos.rs`, `tests/live_deb.rs`, `tests/live_cv.rs`, `tests/live_admin.rs` | Each test fails on the code before its fix | the tests named here |
| P3 | polish | Repository name in the "now serves" line; `architecture_variant` wording ("may remove"); ImportRepos headline counts the skipped remote; clear `last_repo` on Connect; mask before the 64 KiB cut; keep the form after Esc in a confirm | various | section 6 | — | unit |
| — | G03–G20, G22 | Optional Pulp features (G21 is in the P3 rows above) | various | — | built on demand (user decision for G03–G18) | — |

## 10. Final comprehensive functionality matrix

Evidence tags: **[Rn]** = runtime run of the real binary in a PTY (tmux) against a real Pulp server (section 8; R0–R6
of audit 3 at `3f59b9b`, R7–R11 of audit 4 at `f530b17`, R12–R19 of audit 5 at `5812b99`, R20–R34 of audits 6 and 7
at `a0aaca9`, R35–R62 of this audit at `f980f36`: the code of the cited paths is the same in the last three);
**[LT:x]** = live test that drives the TUI action (`run(&mut app, Act::…)`), passed in this audit's run at `f980f36`;
**[LS:x]** = live test at service level (calls `ops`/`CvService`/`Store` directly; the TUI path is not exercised),
passed in this run; **[U]** = unit or mock-HTTP test only; **[S]** = static analysis. *Works end-to-end* is ✅ only
with [Rn] or [LT]. App-level rows (`pulp-tui`) are not Pulp APIs: their *Pulp API* cell is `—`.
`*_x_through_the_tui` names a test that runs for File, RPM and Deb (or the types it names).

Rules for ❌: a P2 finding sets ❌ in its row; where it has no runtime proof, the row says "static". A P3 finding is
named in its row and does not set ❌, with one exception: a row that exists only for an older server version
(F112) and whose operation fails there. Where an operation works on one supported server version or configuration
and fails on another, the matrix has one row for each (the rule "one row for each subtype when the behavior
differs"): the rows that name pulp_deb 3.5.4, pulp_rpm 3.27.12, pulp_file 3.95.3 / 3.63.36, a signing service,
object storage and a user that is not a superuser. The Verify rows and the drift row are ❌ because they fail on a
server with the default `CONTENT_ORIGIN` (F99); they work on every server that sets it, and all live tests do.

Totals of this matrix (✅ / ❌ / ❓ / — per column; `—` is outside every ratio, `❓` is not a success; counted from
the rows with a script):

| Plugin / area | Rows | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent |
|---|---:|---|---|---|---|---|
| `pulp-tui` | 42 | 0 / 0 / 0 / 42 | 42 / 0 / 0 / 0 | 42 / 0 / 0 / 0 | 38 / 4 / 0 / 0 | 22 / 1 / 0 / 19 |
| `pulp_deb` | 36 | 35 / 1 / 0 / 0 | 34 / 1 / 0 / 1 | 34 / 1 / 0 / 1 | 29 / 5 / 0 / 2 | 27 / 1 / 0 / 8 |
| `pulp_file` | 48 | 47 / 1 / 0 / 0 | 47 / 1 / 0 / 0 | 48 / 0 / 0 / 0 | 43 / 5 / 0 / 0 | 36 / 1 / 0 / 11 |
| `pulp_rpm` | 46 | 46 / 0 / 0 / 0 | 45 / 1 / 0 / 0 | 45 / 1 / 0 / 0 | 38 / 6 / 1 / 1 | 36 / 2 / 1 / 7 |
| `pulpcore` | 75 | 72 / 2 / 0 / 1 | 56 / 17 / 0 / 2 | 56 / 17 / 0 / 2 | 52 / 4 / 0 / 19 | 41 / 0 / 0 / 34 |
| **All** | 247 | 200 / 4 / 0 / 43 | 224 / 20 / 0 / 3 | 225 / 19 / 0 / 3 | 200 / 24 / 1 / 22 | 162 / 5 / 1 / 79 |

### `pulp-tui` (application-level features built on the Pulp API)

| Plugin / area | Resource / operation | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent | Explanation / evidence |
|---|---|:---:|:---:|:---:|:---:|:---:|---|
| `pulp-tui` | Content View: add repositories (+ filter rules) | — | ✅ | ✅ | ✅ | — | [R23] multi-select picker, `audit6-a`, `audit6-b`, then `audit6-d`; store only, no task |
| `pulp-tui` | Content View: auto publish of composites | — | ✅ | ✅ | ✅ | ✅ | [LT:composite_content_view_through_the_tui] a Publish of the extras auto-publishes the composite; [LT:resume_publishes_a_lagging_composite_through_the_tui] |
| `pulp-tui` | Content View: break lock | — | ✅ | ✅ | ✅ | — | [LT:resume_watch_and_break_lock_through_the_tui] |
| `pulp-tui` | Content View: compare versions | — | ✅ | ✅ | ✅ | — | [LT:content_view_workflows_through_the_tui] (Compare on Versions) |
| `pulp-tui` | Content View: composite create, add/pin/remove components, publish | — | ✅ | ✅ | ✅ | ✅ | [LT:composite_content_view_through_the_tui] Library serves the composite's paths |
| `pulp-tui` | Content View: create | — | ✅ | ✅ | ✅ | — | [R23], [R45]; store only; F132 the names `.` and `..` are accepted (static) |
| `pulp-tui` | Content View: delete view (impact, distributions, snapshots) | — | ✅ | ✅ | ✅ | ✅ | [R16]; [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View: drift check of all environments | — | ✅ | ✅ | ❌ | ✅ | **F99 (P2)**: [R45] on a server without `CONTENT_ORIGIN` (the default of pulpcore 3.95.3 and 3.123.1) a correct Library is "DRIFT … unsafe href rejected", with the hint "repair the snapshot version"; works where `base_url` is absolute: [R4]; [LT] `CvDrift`; F105 a 302 of object storage and F113 a 403 of a content guard are DRIFT too (static); F60 fixed [LS:a_kept_library_distribution_bound_to_a_repository_is_reported_and_unbound]; F71 ownership loss not reported, wrong hint |
| `pulp-tui` | Content View: edit (description, pins, auto publish) | — | ✅ | ✅ | ✅ | — | [LT:environments_and_view_definition_through_the_tui, composite_content_view_through_the_tui] |
| `pulp-tui` | Content View: edit filter rules | — | ✅ | ✅ | ✅ | — | [LT:content_view_workflows_through_the_tui] Filters then a second Publish |
| `pulp-tui` | Content View: export / import definitions (E / I on Servers) | — | ✅ | ✅ | ✅ | ✅ | [LT:environment_relabel_and_definitions_export_import_through_the_tui] |
| `pulp-tui` | Content View: left-out repository on publish / promote (unbind; forget a gone or taken-over distribution) | — | ✅ | ✅ | ✅ | ✅ | F56 fixed: [R28] a row with a publication on a taken-over distribution is forgotten as the preview says, audit-trail line; [LT:a_left_out_distribution_deleted_outside_pulp_tui_is_forgotten_through_the_tui, a_kept_library_distribution_taken_over_outside_pulp_tui_is_left_alone_through_the_tui]; F59 fixed [LS:a_404_of_a_hidden_distribution_keeps_a_content_view_row_for_a_user] |
| `pulp-tui` | Content View: preview draft vs latest version | — | ✅ | ✅ | ✅ | — | [R23] "In Library:" refusal lines with the way out (F55, F57 fixed); F69 a name collision fails the whole preview; F72 two `publish` refusals not previewed |
| `pulp-tui` | Content View: promote to environment — failure path | — | ✅ | ✅ | ✅ | ✅ | [LT:a_promotion_to_an_overlapping_base_path_is_named_and_creates_nothing_through_the_tui, a_partial_promotion_and_what_the_screens_show_through_the_tui]; [R15] |
| `pulp-tui` | Content View: promote to environment — success path | — | ✅ | ✅ | ✅ | ✅ | [R15]; [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View: publish new version (Deb, signing choice) | — | ✅ | ✅ | ✅ | ✅ | [LT:deb_content_view_publish_signing_through_the_tui] signed / none / same as source |
| `pulp-tui` | Content View: publish new version (RPM + File) | — | ✅ | ✅ | ✅ | ✅ | [R23] v1 `Library:v1`, v2 partial exactly for the previewed repositories (`Library:v1 (v2 partial)`); [R28] v3 `Library:v3`; [R45] File; F95 two local profiles of one server share the derived objects (static); F108 an RPM publish can reuse a publication with other publish settings (static) |
| `pulp-tui` | Content View: publish new version, filtered RPM view after a modulemd of the source was replaced or removed | — | ✅ | ✅ | ❌ | ✅ | **F107 (P2)**: [R60] the preview says "+0 added, -0 removed"; the new Library version has none of the 2 selected packages (Pulp removes the packages of the removed modulemd between the remove step and the add step); the next publish adds them again |
| `pulp-tui` | Content View: remove repository | — | ✅ | ✅ | ✅ | — | [R28]; [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Content View: resume interrupted publish | — | ✅ | ✅ | ✅ | ✅ | [LT:resume_watch_and_break_lock_through_the_tui]; F58 fixed (Resume confirm has the "In Library:" lines, [U]); F70 deleted version; F96 a gone source version gives a raw error on every Resume (static) |
| `pulp-tui` | Content View: roll back environment | — | ✅ | ✅ | ✅ | ✅ | [LT:content_view_workflows_through_the_tui]; F97 after a source rename and re-add, the other derived distribution stays unrecorded (static) |
| `pulp-tui` | Content View: sync source repositories (f) | — | ✅ | ✅ | ✅ | ✅ | [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View: unpromote | — | ✅ | ✅ | ✅ | ✅ | [LT:content_view_workflows_through_the_tui]; F125 a distribution that a stopped promotion made but did not record stays (static) |
| `pulp-tui` | Content View database: migrate / copy (offered on Connect, save) | — | ✅ | ✅ | ✅ | — | [LT:database_migrate_and_copy_through_the_tui]; [LS:postgres_migration_9_restores_the_served_version_of_an_old_failed_row]; F124 `db copy --from` has no same-server check, and a second copy loses the aliases of the first database id (static) |
| `pulp-tui` | Content View version: delete | — | ✅ | ✅ | ❌ | ✅ | **F78 (P2)**: [R31] a version that records `…/versions/0/` of a snapshot repository deletes it; every later unfiltered RPM or Deb publish fails with 400; [R15]; [LT:content_view_workflows_through_the_tui, a_version_that_dev_serves_in_part_is_not_deleted_through_the_tui] |
| `pulp-tui` | Content View version: list | — | ✅ | ✅ | ✅ | ✅ | [R15], [R23] states and environments |
| `pulp-tui` | Content View version: watch pending task | — | ✅ | ✅ | ✅ | ✅ | [LT:resume_watch_and_break_lock_through_the_tui] (Watch W) |
| `pulp-tui` | Environment: create | — | ✅ | ✅ | ✅ | — | [R23] `A6Dev` after Library; [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Environment: delete | — | ✅ | ✅ | ✅ | — | [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Environment: relabel (moves distributions) | — | ✅ | ✅ | ✅ | ✅ | [LT:environment_relabel_and_definitions_export_import_through_the_tui]; refusal [LT:relabel_to_an_overlapping_label_is_refused_through_the_tui] |
| `pulp-tui` | Environment: toggle protected | — | ✅ | ✅ | ✅ | — | [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Fallback replication: Deb, source pulp_deb 3.11 into a destination with pulp_deb 3.8 or 3.5 | — | ✅ | ✅ | ❌ | ✅ | **F104 (P2)**: [R58] 3.11.1 → 3.5.4: dry run "CREATE"; run "failed … rejected [400]: … layout: Unexpected field …" after the remote, the repository and the sync; the same on each rerun; 3.8.1 by its source |
| `pulp-tui` | Fallback replication: dry run and run (RPM, File, Deb; both servers of one version) | — | ✅ | ✅ | ✅ | ✅ | [LT:fallback_replication_through_the_tui, deb_fallback_replication_through_the_tui]; F57 fixed [LS:the_fallback_plan_names_a_base_path_that_overlaps_a_local_distribution]; static: F99 and F105 each item fails at its last check on a server without `CONTENT_ORIGIN` or with object storage; F120 RPM dry run CREATE for a distribution that serves nothing; F122 names of another type; F123 the label is the profile name |
| `pulp-tui` | Form: keys typed ahead after Enter are held, with a footer marker | — | ✅ | ✅ | ✅ | — | [R27] `-- WAIT --`; [R19] |
| `pulp-tui` | Logs screen (session requests, actions, notices) | — | ✅ | ✅ | ✅ | — | [LT:upstream_base_url_user_info_never_shows_through_the_tui] every Logs row and detail masked (F64 fixed); 64 KiB cut (polish) |
| `pulp-tui` | Read-only profile blocks mutations | — | ✅ | ✅ | ✅ | — | [LT:server_profiles_create_edit_and_delete_through_the_tui] a mutation is refused at once; F128 opening Environments still inserts the Library row into a shared database (static) |
| `pulp-tui` | Repository definitions: export (X) | — | ✅ | ✅ | ✅ | — | [LT:repository_definitions_export_and_import] |
| `pulp-tui` | Repository definitions: import (J, plan then create) | — | ✅ | ✅ | ✅ | ✅ | [LT:repository_definitions_export_and_import]; headline count (polish); F84 an RPM repository with an OSV config is refused with 400 (static) |
| `pulp-tui` | Server profile: connect / switch host | — | ✅ | ✅ | ✅ | ❌ | **F98 (P2)**: [R48] a sub-list of the previous server ("Roles of user a8-x1") stays during "connecting..."; a form opened then posts to the new server: the role went to another user there, and the result named the old one. What holds: [R41] a switch while a task runs: the Task Center of the other server does not show it, and after the switch back the row is `completed`; [R0], [LT:server_profiles_create_edit_and_delete_through_the_tui]; `last_repo` kept (polish) |
| `pulp-tui` | Server profile: create / edit / delete | — | ✅ | ✅ | ✅ | — | [LT:server_profiles_create_edit_and_delete_through_the_tui] keeps the file comment; F94 the config is written on the UI thread (static); F111 [R53] a save removes a profile that another process added to the file; F115 [R55] the header shows the edited URL while requests go to the old server; F131 Delete leaves the keyring secret (static) |
| `pulp-tui` | Server profile: store secret in system keyring | — | ✅ | ✅ | ✅ | — | [LT:store_secret_in_the_keyring_through_the_tui] |
| `pulp-tui` | Server profile: test connection | — | ✅ | ✅ | ✅ | — | [LT:server_profile_test_connection] |

### `pulp_deb` 3.11.1

| Plugin / area | Resource / operation | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent | Explanation / evidence |
|---|---|:---:|:---:|:---:|:---:|:---:|---|
| `pulp_deb` | ACS: create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:acs_create_edit_and_delete_through_the_tui]; not on pulp_deb 3.5.4 (COMPATIBILITY) |
| `pulp_deb` | ACS: refresh (task group) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:acs_refresh_through_the_tui_ends_with_its_task_group] the tracked task group is final |
| `pulp_deb` | Content: add to another repository (structured `deb/copy/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui]; F73 no replacement line, no retention check |
| `pulp_deb` | Content: browse 13 types, search packages | ✅ | ✅ | ✅ | ✅ | — | [LT:content_add_and_remove_through_the_tui, the_tui_creates_release_records_that_a_publication_serves] |
| `pulp_deb` | Content: create release / component / architecture / package release component | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:the_tui_creates_release_records_that_a_publication_serves]; F62 fixed: [LT:a_package_release_component_of_an_older_version_confirms_what_comes_back_through_the_tui]; `architecture_variant` over-warning (polish) |
| `pulp_deb` | Content: create source package release component (`content/deb/source_release_components/`) | ✅ | ❌ | ❌ | — | — | G21 (new): [R43] an uploaded source package is in no `Sources` index of a structured publication (0 bytes); the record Create has the four other types only |
| `pulp_deb` | Content: remove units (with release records) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui] the screen moves to the new version |
| `pulp_deb` | Content: upload `.deb` (distribution, component, overwrite) | ✅ | ✅ | ✅ | ✅ | ✅ | [R43]; [LT:uploads_of_each_type_through_the_tui] |
| `pulp_deb` | Content: upload `.deb` into a repository with a package signing service (pulp_deb 3.11) | ✅ | ✅ | ✅ | ❌ | ❌ | **F106 (P2)**, static: Pulp signs and completes; the TUI ends with "digest mismatch" and does not reload; no signing service of this kind in the test environment |
| `pulp_deb` | Content: upload `.udeb` / source package / generic content | ✅ | ✅ | ✅ | ✅ | ✅ | [R43] `.dsc`; [LT:the_tui_uploads_a_udeb_a_source_package_and_generic_content_as_their_types]; an APT publication serves no `.udeb` and no generic content, and a source package only in simple mode (G21, polish) |
| `pulp_deb` | Distribution: create / edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui] base path, binding, guard and checkpoint edits after the typed name |
| `pulp_deb` | Distribution: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui] (single); marked [U] |
| `pulp_deb` | Distribution: list and binding details | ✅ | ✅ | ✅ | ✅ | — | [LT:deb_publications_and_distributions_through_the_tui] |
| `pulp_deb` | Distribution: re-point / rollback (keeps signing and verbatim mode) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui]; [LS:deb_rollback_keeps_a_simple_publication] |
| `pulp_deb` | Distribution: verify served content | ✅ | ✅ | ✅ | ❌ | — | **F99 (P2)**, static for Deb: the same `fetch_content` as [R44]: MISMATCH for every distribution on a server without `CONTENT_ORIGIN`; works where `base_url` is absolute: [LT:deb_publications_and_distributions_through_the_tui] "OK: "; F105 object storage, F113 content guard, F130 `dists/default/` on pulp_deb 3.5.4 / 3.8.1 (static) |
| `pulp_deb` | Publication: create APT (structured / simple / signing) | ✅ | ✅ | ✅ | ✅ | ✅ | [R3]; [LT:deb_publications_and_distributions_through_the_tui] |
| `pulp_deb` | Publication: create verbatim | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_verbatim_publication_create_through_the_tui] |
| `pulp_deb` | Publication: delete (APT, verbatim; served-after impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui] names the distribution and what it serves after |
| `pulp_deb` | Publication: list APT + verbatim (merged, newest first) | ✅ | ✅ | ✅ | ✅ | — | [LT:deb_publications_and_distributions_through_the_tui] |
| `pulp_deb` | Remote: create (distributions, components, architectures, udebs, sources, GPG key) | ✅ | ✅ | ✅ | ✅ | ✅ | [R3]; [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_deb` | Remote: delete (impact) | ✅ | ✅ | ✅ | ❌ | ✅ | **F100 (P2)**, static for Deb: the same text and pulpcore cascade as [R46]; [LT:remotes_create_edit_and_delete_through_the_tui] names the repository; Pulp unsets its remote |
| `pulp_deb` | Remote: edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] (a task) |
| `pulp_deb` | Repository: copy content from another repository (`deb/copy/`, filters) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:copy_content_through_the_tui] name rule; F73 |
| `pulp_deb` | Repository: copy with dependency solving | ❌ | — | — | — | — | Pulp unavailable: pulp_deb 3.11.1 raises `FeatureNotImplementedError` |
| `pulp_deb` | Repository: create | ✅ | ✅ | ✅ | ✅ | ✅ | [R3] with the remote picker (pulp_deb 3.11.1) |
| `pulp_deb` | Repository: create on pulp_deb 3.5.4 (image 3.63.36) | ✅ | ✅ | ✅ | ❌ | — | **F102 (P2)**: [R49] every Create from the form: "rejected [400]: autopublish: Unexpected field"; the live tests create Deb repositories through the API |
| `pulp_deb` | Repository: delete (impact, typed confirm) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_repository_delete_impact_names_the_distribution_of_a_verbatim_publication] |
| `pulp_deb` | Repository: distribute (pin, optional signing service; re-point after the typed name) | ✅ | ✅ | ✅ | ✅ | ✅ | [R3]; F54 fixed [LT:deb_distribute_re_points_after_the_typed_name_through_the_tui]; F67 overlapping new path |
| `pulp_deb` | Repository: edit (signing services, overrides, excluded fields) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_repository_edit_of_the_signing_fields_through_the_tui]; Retain versions: F53 fixed (see the `pulp_file` row), no Deb live case (T05) |
| `pulp_deb` | Repository: reclaim space | ✅ | ✅ | ✅ | ❌ | ✅ | **F76 (P2)**, static for Deb: the same `reclaim` code and pulpcore task as [R29]; [LT:deb_version_actions_through_the_tui] impact, typed name, task |
| `pulp_deb` | Repository: sync additive | ✅ | ✅ | ✅ | ✅ | ✅ | [R3] Task Center completed; `latest` 0 → 1 |
| `pulp_deb` | Repository: sync mirror | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_mirror_sync_through_the_tui] |
| `pulp_deb` | Version: compare | ✅ | ✅ | ✅ | ✅ | — | [LT:deb_version_actions_through_the_tui] |
| `pulp_deb` | Version: delete (protection incl. verbatim) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_version_actions_through_the_tui]; [LS:a_served_verbatim_publication_protects_its_version] |
| `pulp_deb` | Version: repair | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_version_actions_through_the_tui] counts |
| `pulp_deb` | Version: restore as latest | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_version_actions_through_the_tui] version 3, list reloads; F85 |

### `pulp_file` 3.123.1

| Plugin / area | Resource / operation | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent | Explanation / evidence |
|---|---|:---:|:---:|:---:|:---:|:---:|---|
| `pulp_file` | ACS: create / edit (path warning) / delete | ✅ | ✅ | ✅ | ❌ | ✅ | **F77 (P2)**: [R30] Edit of an ACS with no paths sends the PATCH with no check, and every other ACS with no paths loses its path row; ACS Delete "its 1 path(s)" (polish); [LT:acs_create_edit_and_delete_through_the_tui] |
| `pulp_file` | ACS: refresh (task group) | ✅ | ✅ | ✅ | ✅ | ❌ | [LT:acs_refresh_through_the_tui_ends_with_its_task_group]; F77 [R30] the refresh of an ACS that lost its path row makes an empty group, shown `gone` "its tasks were purged" |
| `pulp_file` | Content: add to another repository | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui]; F73 a same-path unit is replaced with no dialog and no retention check |
| `pulp_file` | Content: browse, search paths | ✅ | ✅ | ✅ | ✅ | — | [LT:content_add_and_remove_through_the_tui] (pulp_file 3.123.1) |
| `pulp_file` | Content: search paths (`/`) on pulp_file 3.95.3 and 3.63.36 | ❌ | ❌ | ✅ | ❌ | — | F112: [R51] on 3.63.36 "Error: rejected [400]: errors: Invalid Filter: 'relative_path__icontains'"; those versions filter only by exact `relative_path` and `sha256` (3.95.3 by its source); no capability check |
| `pulp_file` | Content: remove units | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui]; a snapshot of another database refused [LT:snapshot_versions_publications_and_content_of_another_database_are_not_changed_through_the_tui]; F127 (all types) Remove picked from the action menu acts on the row selected at that moment, and the confirm names no unit (static) |
| `pulp_file` | Content: upload chunked (progress, digest check) | ✅ | ✅ | ✅ | ✅ | ✅ | [R1] 12 MB file; `latest` 1 → 2; no upload record left |
| `pulp_file` | Content: upload single file | ✅ | ✅ | ✅ | ✅ | ✅ | [R1]; [LT:uploads_of_each_type_through_the_tui]; F73 overwrite has no retention check |
| `pulp_file` | Content: upload without overwrite | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_upload_without_overwrite_through_the_tui] |
| `pulp_file` | Distribution: checkpoint (verify, rollback refused) | ✅ | ✅ | ✅ | ✅ | — | [LT:checkpoint_distribution_verifies_and_refuses_rollback]; field from pulpcore 3.95 |
| `pulp_file` | Distribution: copy content URL (OSC 52) | ✅ | ✅ | ✅ | ✅ | — | [R38] `y` → "[ok] copied http://127.0.0.1:8090/pulp/content/default/a8-f/", and the tmux buffer (`set-clipboard on`) holds that URL, with the domain; F99 [R44] a relative path without host on a server without `CONTENT_ORIGIN` |
| `pulp_file` | Distribution: create / edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] base path and binding edits after the typed name, old URL 404 |
| `pulp_file` | Distribution: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui]; marked [U] |
| `pulp_file` | Distribution: edit of content guard or checkpoint (all types) | ✅ | ✅ | ✅ | ✅ | ✅ | [R9]; [LT:*_publications_and_distributions_through_the_tui] guard none → A → B → none with guard names, checkpoint on/off |
| `pulp_file` | Distribution: list and binding details | ✅ | ✅ | ✅ | ✅ | — | [R1]; [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Distribution: re-point an existing distribution with Distribute (D) from Repositories, Versions or Publications (all types) | ✅ | ✅ | ✅ | ✅ | ✅ | F54 fixed: [R21] preview and typed name, Esc changes nothing; [LT:file_distribute_re_points_after_the_typed_name_through_the_tui, deb_distribute_re_points_after_the_typed_name_through_the_tui]; F68 [R25] follow onto a checkpoint distribution; repository id in "now serves" (polish) |
| `pulp_file` | Distribution: re-point to a version (b) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] the job re-points and verifies |
| `pulp_file` | Distribution: verify served content | ✅ | ✅ | ✅ | ❌ | — | **F99 (P2)**: [R44] without `CONTENT_ORIGIN`: "MISMATCH: unsafe href rejected: /pulp/content/default/a8-big/PULP_MANIFEST" for a correct distribution; works where `base_url` is absolute: [LT:checkpoint_distribution_verifies_and_refuses_rollback, file_publications_and_distributions_through_the_tui]; F113 [R57] a guarded distribution is "MISMATCH … HTTP 403"; F105 object storage (static) |
| `pulp_file` | Publication: checkpoint publication that clears older checkpoints (impact, typed name; all types) | ✅ | ✅ | ✅ | ✅ | ✅ | [R22] the confirm names the checkpoint that loses the flag; after the typed name Pulp and the list agree; mock `a_checkpoint_publication_that_clears_older_checkpoints_needs_the_typed_name`; no live test (T05); pulpcore 3.123 (`retain_checkpoints`) |
| `pulp_file` | Publication: create (manifest name, checkpoint) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Publication: delete (served-after impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Publication: list | ✅ | ✅ | ✅ | ✅ | — | [R22]; [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Remote (Git): create | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_git_remote_syncs_a_repository_with_the_prefilled_form]; pulpcore 3.123 only |
| `pulp_file` | Remote (Git): delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] (File Git) |
| `pulp_file` | Remote (Git): edit `git_ref` | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_git_remote_syncs_a_repository_with_the_prefilled_form] |
| `pulp_file` | Remote: create (timeouts, headers, TLS) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remote_timeouts_keep_decimal_seconds_through_the_tui] |
| `pulp_file` | Remote: delete (impact) | ✅ | ✅ | ✅ | ❌ | ✅ | **F100 (P2)**: [R46] the confirm says "Repository content and versions are not affected"; after the delete of an `on_demand` remote the files that Pulp had not stored answer 404; [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_file` | Remote: delete refused while an ACS uses it | ✅ | ✅ | ✅ | ✅ | — | [LT:remote_delete_refused_while_an_acs_uses_it] |
| `pulp_file` | Remote: edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remote_timeouts_keep_decimal_seconds_through_the_tui, remotes_create_edit_and_delete_through_the_tui] |
| `pulp_file` | Repository: copy content (filters, `modify/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:copy_content_through_the_tui] path rule; F73 |
| `pulp_file` | Repository: create | ✅ | ✅ | ✅ | ✅ | ✅ | [R1] row shown at once |
| `pulp_file` | Repository: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:repository_delete_single_and_marked_through_the_tui] one typed confirm for two marked, marks go |
| `pulp_file` | Repository: delete of a Content View snapshot repository (all types) | ✅ | ✅ | ✅ | ✅ | — | [R6]; [LT:snapshot_repository_of_another_database_is_not_deleted_through_the_tui] |
| `pulp_file` | Repository: distribute (follow latest) | ✅ | ✅ | ✅ | ✅ | ✅ | [R1]; [LT:file_serve_from_versions_through_the_tui]; F79 [R32] "[ok] … serves" for a repository with no publication, the URL 404 |
| `pulp_file` | Repository: distribute (pin latest) | ✅ | ✅ | ✅ | ✅ | ✅ | [R21] a new distribution goes on at once; F67 [R24] an overlapping new path leaves a publication |
| `pulp_file` | Repository: edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_repository_edit_through_the_tui]; [R20] description and retention in one PATCH after the confirm |
| `pulp_file` | Repository: edit of Retain versions / Retain checkpoints (all types) | ✅ | ✅ | ✅ | ✅ | ✅ | F53 fixed: [R20] impact "2, 0", typed name, Esc sends nothing, Pulp `[3,1]`; [LT:a_lower_retain_versions_deletes_versions_only_after_the_typed_name_through_the_tui]; no Deb or Retain-checkpoints live case (T05) |
| `pulp_file` | Repository: publish latest (manifest option) | ✅ | ✅ | ✅ | ✅ | ✅ | [R1] |
| `pulp_file` | Repository: reclaim space | ✅ | ✅ | ✅ | ❌ | ✅ | **F76 (P2)**: [R29] the confirm says "no other repository has"; the units shared with `a7-copy` lost their files; [LT:file_version_actions_through_the_tui] the files it reports are gone; "policy )" fixed (F63) |
| `pulp_file` | Repository: sync from Git remote | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_git_remote_syncs_a_repository_with_the_prefilled_form] |
| `pulp_file` | Repository: sync from manifest (additive / mirror) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_mirror_sync_through_the_tui]; F63 fixed [LT:file_mirror_sync_from_a_typed_git_remote_name_warns_that_the_task_fails_through_the_tui]; F83 mirror on an autopublish repository: confirm, then 400 (static) |
| `pulp_file` | Version: compare | ✅ | ✅ | ✅ | ✅ | — | [LT:file_version_actions_through_the_tui] |
| `pulp_file` | Version: delete (protected versions) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_version_actions_through_the_tui] the mark goes |
| `pulp_file` | Version / publication / content: change of a Content View snapshot of another database (all types) | ✅ | ✅ | ✅ | ✅ | — | [R14]; [LT:snapshot_versions_publications_and_content_of_another_database_are_not_changed_through_the_tui] checks the F51 ending |
| `pulp_file` | Version: publish / distribute from Versions | ✅ | ✅ | ✅ | ✅ | ✅ | [R21]; [LT:file_serve_from_versions_through_the_tui] |
| `pulp_file` | Version: repair | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_version_actions_through_the_tui] |
| `pulp_file` | Version: restore as latest | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_version_actions_through_the_tui]; F85 "newer versions are kept" while the retention deletes them after an untyped confirm |
| `pulp_file` | Version: roll back a distribution (b) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_serve_from_versions_through_the_tui] the preview names the publication |

### `pulp_rpm` 3.39.5

| Plugin / area | Resource / operation | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent | Explanation / evidence |
|---|---|:---:|:---:|:---:|:---:|:---:|---|
| `pulp_rpm` | ACS: create / edit / delete | ✅ | ✅ | ✅ | ❌ | ✅ | **F77 (P2)**, static for RPM: the same `acs_edit` code and pulpcore serializer as [R30]; [LT:acs_create_edit_and_delete_through_the_tui] (RPM also adds a path) |
| `pulp_rpm` | ACS: refresh (task group) | ✅ | ✅ | ✅ | ✅ | ❌ | [LT:acs_refresh_through_the_tui_ends_with_its_task_group]; F77, static for RPM: the same empty group and `gone` text as [R30] |
| `pulp_rpm` | Content: add to another repository (`modify/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui]; no related units, by design; F73 |
| `pulp_rpm` | Content: browse 11 types, search packages | ✅ | ✅ | ✅ | ✅ | — | [LT:content_add_and_remove_through_the_tui, a_modulemd_made_through_the_tui_is_in_the_new_version] |
| `pulp_rpm` | Content: comps.xml upload (replace) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_metadata_uploads_and_module_content_through_the_tui] twice, the second with Replace |
| `pulp_rpm` | Content: advisory upload (JSON) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_metadata_uploads_and_module_content_through_the_tui] |
| `pulp_rpm` | Content: modulemd create | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:a_modulemd_made_through_the_tui_is_in_the_new_version] |
| `pulp_rpm` | Content: modulemd defaults / obsoletes create | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_metadata_uploads_and_module_content_through_the_tui] |
| `pulp_rpm` | Content: package upload (single, chunked, overwrite) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:uploads_of_each_type_through_the_tui]; chunked path verified on File [R1] |
| `pulp_rpm` | Content: package upload into a repository with a package signing service | ✅ | ✅ | ✅ | ❌ | ❌ | **F106 (P2)**, static: Pulp signs and completes (a new version); the TUI ends with "digest mismatch" and does not reload; a retry makes one more unit; no signing service of this kind in the test environment |
| `pulp_rpm` | Content: remove units | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui]; F117 [R61] Remove of a modulemd says "without 1 unit(s)" and Pulp also removes its packages |
| `pulp_rpm` | Distribution: create / edit (`generate_repo_config`, hidden, guard) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui] guard and checkpoint edits after the typed name |
| `pulp_rpm` | Distribution: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui]; marked [U] |
| `pulp_rpm` | Distribution: list and binding details | ✅ | ✅ | ✅ | ✅ | — | [R2] `pinned` binding shown |
| `pulp_rpm` | Distribution: re-point (signing repository makes a new publication) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui]; signing case [LS:rpm_rollback_of_a_signing_repository_serves_signed_metadata] |
| `pulp_rpm` | Distribution: verify served content | ✅ | ✅ | ✅ | ❌ | — | **F99 (P2)**, static for RPM: the same `fetch_content` as [R44]: MISMATCH for every distribution on a server without `CONTENT_ORIGIN`; works where `base_url` is absolute: [R2] `… served with matching digest (5 packages)`; F82 a `nested_by_digest` publication gives a false MISMATCH; F105 object storage; F113 content guard (static) |
| `pulp_rpm` | Publication: create (checksum, compression, layout, repo config, checkpoint) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui]; F81 `sha224` offered (static) |
| `pulp_rpm` | Publication: delete (served-after impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui] |
| `pulp_rpm` | Publication: list | ✅ | ✅ | ✅ | ✅ | — | [LT:rpm_publications_and_distributions_through_the_tui] |
| `pulp_rpm` | Remote (ULN): create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:uln_remote_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Remote: create (SLES token, headers, timeouts) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2]; [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Remote: delete (impact, ACS refusal) | ✅ | ✅ | ✅ | ❌ | ✅ | **F100 (P2)**, static for RPM: the same text and pulpcore cascade as [R46]; [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Remote: edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Repository: copy content, filtered (+ advisories, metadata, deps) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_filtered_copy_through_the_tui_takes_the_advisory_of_the_chosen_packages]; F73 |
| `pulp_rpm` | Repository: advanced copy (several config entries, `dest_base_version`, `dependency_upgrade`) | ✅ | ❌ | ❌ | — | — | G03 (open by decision) |
| `pulp_rpm` | Repository: copy content, unfiltered (`rpm/copy/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:copy_content_through_the_tui] RPM without rules |
| `pulp_rpm` | Repository: create (remote picker, RPM fields) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2]; [LT:rpm_osv_config_survives_a_label_edit_through_the_tui] |
| `pulp_rpm` | Repository: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:repository_delete_single_and_marked_through_the_tui] |
| `pulp_rpm` | Repository: distribute (follow latest) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_serve_from_versions_through_the_tui]; F79 (static for RPM) |
| `pulp_rpm` | Repository: distribute (pin latest) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2] `repodata/repomd.xml` 200; re-point after the typed name (F54, `pulp_file` row); F67 with a signing service makes a new publication on each try |
| `pulp_rpm` | Repository: edit (labels + OSV config, retain package versions) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_osv_config_survives_a_label_edit_through_the_tui, rpm_retain_package_versions_cleared_through_the_tui]; Retain versions: F53 fixed; F81 [R34] `sha224` offered, Pulp 400; F129 the fingerprint help and clear on pulp_rpm 3.34.0 / 3.27.12 (static) |
| `pulp_rpm` | Repository: edit on pulp_rpm 3.27.12 (image 3.63.36) | ✅ | ✅ | ✅ | ❌ | ✅ | **F103 (P2)**: [R50] an Edit of the description sends `{"description": …}`; the task completes and Pulp stores `repo_config: {}` (was `gpgcheck` and `repo_gpgcheck` 1); nothing says so |
| `pulp_rpm` | Repository: prune old packages (dry run, run; task group) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_prune_dry_run_then_prune_through_the_tui] |
| `pulp_rpm` | Repository: publish latest (publication options) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2] |
| `pulp_rpm` | Repository: reclaim space | ✅ | ✅ | ✅ | ❌ | ✅ | **F76 (P2)**, static for RPM: the same `reclaim` code and pulpcore task as [R29]; [LT:rpm_version_actions_through_the_tui] |
| `pulp_rpm` | Repository: sync additive | ✅ | ✅ | ✅ | ✅ | ✅ | [R2] Task Center row completed; `latest` 0 → 1 |
| `pulp_rpm` | Repository: sync from a ULN remote | ✅ | ✅ | ✅ | ❓ | ❓ | needs a ULN account; mock only |
| `pulp_rpm` | Repository: sync history (H) | ✅ | ✅ | ✅ | ✅ | — | [LT:sync_history_lists_the_syncs_of_a_repository] |
| `pulp_rpm` | Repository: sync mirror_complete / mirror_content_only, optimize, skip types | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_mirror_sync_through_the_tui, rpm_mirror_content_only_sync_with_options_through_the_tui] |
| `pulp_rpm` | Version: compare (packages, advisories, comps) | ✅ | ✅ | ✅ | ✅ | — | [LT:compare_shows_an_updated_advisory_as_changed, rpm_version_actions_through_the_tui] |
| `pulp_rpm` | Version: delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_version_actions_through_the_tui] |
| `pulp_rpm` | Version: publish / distribute from Versions | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_serve_from_versions_through_the_tui] |
| `pulp_rpm` | Version: repair | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_version_actions_through_the_tui] |
| `pulp_rpm` | Version: restore as latest | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_version_actions_through_the_tui]; F85 |
| `pulp_rpm` | Version: roll back a distribution (b) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_serve_from_versions_through_the_tui] |
| `pulp_rpm` | Version: vulnerability scan (osv.dev) | ✅ | ✅ | ✅ | ✅ | ✅ | pulp_rpm ≥ 3.39; [LT:rpm_version_scan_through_the_tui] |

### `pulpcore` 3.123.1 (shared management)

| Plugin / area | Resource / operation | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent | Explanation / evidence |
|---|---|:---:|:---:|:---:|:---:|:---:|---|
| `pulpcore` | Access policy: edit statements | ✅ | ❌ | ❌ | — | — | G07: left out on purpose |
| `pulpcore` | Access policy: list, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:access_policy_reset_restores_the_default] |
| `pulpcore` | Access policy: reset to plugin default | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:access_policy_reset_restores_the_default] |
| `pulpcore` | Artifact distribution (`distributions/core/artifacts/`) | ✅ | ❌ | ❌ | — | — | G06 |
| `pulpcore` | Artifact: list, find by sha256 | ✅ | ✅ | ✅ | ✅ | — | [R39] the screen lists the artifact of a TUI upload; `/` with another sha256 gives 0 items, with its sha256 1 item; Inspect shows it; [LS:artifacts_screen_lists_and_finds_a_sha256] |
| `pulpcore` | Artifact: upload / delete | ✅ | ❌ | ❌ | — | — | G16; delete is deprecated and refused by the default policy |
| `pulpcore` | Content guard: create `core/header` | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_guard_create_edit_and_delete_name_its_distribution] |
| `pulpcore` | Content guard: create other types (rbac, envvar header, redirect, composite, x509, RHSM) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_guards_of_the_other_types_are_created_through_the_tui] (`core/envvar_header` from pulpcore 3.95) |
| `pulpcore` | Content guard: delete (impact: distributions, composites) | ✅ | ✅ | ✅ | ❌ | ✅ | **F66 (P2)**: [R26] a guard inside two composite levels: "No distribution uses it", and the distribution behind the outer composite is public after the delete; direct use [LT:content_guard_create_edit_and_delete_name_its_distribution]; F89 [R36] the default guard of a domain: "No distribution uses it", and the domain then has no default guard; F109 [R62] the same false line for a guard owner who is not a superuser |
| `pulpcore` | Content guard: edit (impact through composites at any depth) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_guard_create_edit_and_delete_name_its_distribution] confirm, Esc, description-only edit at once; unit two composite levels |
| `pulpcore` | Content guard: RBAC download access (add/remove role) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rbac_guard_access_allows_and_removes_a_user] |
| `pulpcore` | Content: create from a URL (`file_url`, `downloader_config`) | ✅ | ❌ | ❌ | — | — | G22 (new): no client method; `file_url` in all three images, `downloader_config` in 3.123.1 |
| `pulpcore` | Content: server-wide search (`content/`, generic lists) | ✅ | ❌ | ❌ | — | — | G14 |
| `pulpcore` | Content: set / unset labels (`{content}set_label/`) | ✅ | ❌ | ❌ | — | — | G19 (new): no client method; in the 3.123.1 OpenAPI for every RPM, File and Deb content type |
| `pulpcore` | Current user and capabilities (status, login, OpenAPI) | ✅ | ✅ | ✅ | ✅ | — | [R0], [R12]; every [LT]; F92 behind an outer TLS proxy a multi-page `list_all` fails (static; Pulp side checked with curl) |
| `pulpcore` | Dashboard: health, versions, counts | ✅ | ✅ | ✅ | ✅ | ✅ | [R42] with no key pressed, the end of a 41 s sync changed "Tasks running 1" to 0 and "File content 0" to 40; [R12]; F99 [R44] the "content:" line is empty without `CONTENT_ORIGIN` |
| `pulpcore` | Data repair `datarepair/7272/`, `livez/` | ✅ | ❌ | ❌ | — | — | G13 |
| `pulpcore` | Delete and Edit impacts for a user that is not a superuser (content guard Delete and Edit, repository Delete, publication Delete; all types) | ✅ | ✅ | ✅ | ❌ | — | **F109 (P2)**: [R62] a guard owner who may not view File distributions: "No distribution uses it."; after the typed name the distribution that used the guard is public (403 → 200); Pulp scopes the lists to what the user may view; repository and publication Delete and guard Edit static |
| `pulpcore` | Distribution: served-content checks on object storage (S3, Azure, GCS; Verify, Rollback check, drift, fallback replication; all types) | ✅ | ✅ | ✅ | ❌ | — | **F105 (P2)**, static: the content app answers 302 and the client follows no redirect, so each check is MISMATCH or DRIFT; not run (no object storage in the test environment) |
| `pulpcore` | Distribution: bind directly to a repository version (no publication pin) | ✅ | ❌ | ❌ | — | — | G15 |
| `pulpcore` | Domain: create / edit / delete / migrate storage | ✅ | ✅ | ✅ | ✅ | ✅ | first runtime proof, on a server with `DOMAIN_ENABLED`: [R35] Create "[ok] create domain a8x: done"; Edit (description, flag) and Migrate storage (typed name, `POST /pulp/a8x/api/v3/domains/migrate/`) each end "task completed", and Pulp and the list agree; [R37] Delete after the typed name; gating [LS:domains_screen_follows_domain_enabled]; F90 [R37] deleting the profile's own domain has no warning, then every screen is 404; uploads, sync, Task Center, Dashboard, Content Views and replication also ran inside domains ([R38]–[R42], [R45], [R52]) |
| `pulpcore` | Domain: labels, default content guard, rename | ✅ | ❌ | ❌ | — | — | G08 |
| `pulpcore` | Exporter (filesystem): create / edit / delete / export | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:filesystem_exporter_through_the_tui]; F119 a failed export leaves a record that the Exports list shows like a good one (static) |
| `pulpcore` | Exporter (Pulp): create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:pulp_exporter_and_importer_through_the_tui]; snapshots pickable [LT:a_content_view_snapshot_can_be_exported]; Create opens empty [R10] |
| `pulpcore` | Exporter (Pulp): export (full, incremental, versions, start versions) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:pulp_exporter_and_importer_through_the_tui] (full); other modes [LS:pulp_export_and_import_of_a_file_repository] |
| `pulpcore` | Exporter (Pulp): exports list / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:pulp_exporter_and_importer_through_the_tui]; F88 the delete of the last export does not name its effect on incremental exports (static) |
| `pulpcore` | Group: create / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:group_create_roles_and_delete_through_the_tui]; F87 (static) |
| `pulpcore` | Group: list, members list | ✅ | ✅ | ✅ | ✅ | — | [R6] |
| `pulpcore` | Group: member add | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:group_member_add_and_remove_keeps_the_user] |
| `pulpcore` | Group: member remove | ✅ | ✅ | ✅ | ✅ | ✅ | [R6] the user stays; [LT:group_member_add_and_remove_keeps_the_user] |
| `pulpcore` | Group: rename | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:group_rename] |
| `pulpcore` | Group: roles add / remove | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:group_create_roles_and_delete_through_the_tui] (a powerful role asks first); F87 no "YOUR account" line when the current user is a member (static) |
| `pulpcore` | Import record delete; `create_repositories` | ✅ | ❌ | ❌ | — | — | G09 |
| `pulpcore` | Importer: create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:pulp_exporter_and_importer_through_the_tui]; a mapped snapshot destination refused [U] |
| `pulpcore` | Importer: import check, then import (task group) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:pulp_exporter_and_importer_through_the_tui] check, confirm (Retain versions wording fixed, 6f66ad3), task group, the copy gets the file |
| `pulpcore` | Importer: import into a Content View snapshot repository | ✅ | ✅ | ✅ | ✅ | — | [R10]; [LT:an_import_does_not_write_into_a_content_view_snapshot_unasked] (F28 limit) |
| `pulpcore` | Importer: imports list | ✅ | ✅ | ✅ | ✅ | — | [LT:pulp_exporter_and_importer_through_the_tui] (Imports) |
| `pulpcore` | Login: change own password (`PATCH login/`) | ✅ | ❌ | ❌ | — | — | G20 (new): the client only reads `GET login/`; pulpcore 3.123.1 (not 3.95.3) |
| `pulpcore` | Object roles: list, my permissions, add, remove | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:object_roles_add_and_remove_a_viewer_on_a_repository] (as an administrator); F121 for a user without `manage_roles` the whole view is 403, also "my permissions" (static) |
| `pulpcore` | OpenPGP keyrings, keys, OpenPGP distributions | ✅ | ❌ | ❌ | — | — | G05 |
| `pulpcore` | Orphan cleanup (age; whole server) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:prune_orphans_through_the_tui] Pulp's count, typed profile name, result; G12 scope not offered |
| `pulpcore` | Repository: `modify/` with a base version, or add and remove in one call | ✅ | ❌ | ❌ | — | — | G04 |
| `pulpcore` | Repository: reclaim space of several repositories in one request | ✅ | ❌ | ❌ | — | — | G18 |
| `pulpcore` | Role: create / edit / delete custom role | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:custom_role_create_edit_and_delete_through_the_tui] (Create with no row selected); F75 Edit changes every holder with no impact; F110 [R47] an empty Description is sent as `""` and Pulp answers 400 |
| `pulpcore` | Role: list | ✅ | ✅ | ✅ | ✅ | — | [LT:custom_role_create_edit_and_delete_through_the_tui] |
| `pulpcore` | Server repair (`repair/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:server_repair_checks_that_the_files_exist] |
| `pulpcore` | Signing service: create | ❌ | — | — | — | — | Pulp unavailable: no API; list is used in pickers |
| `pulpcore` | Task: cancel (single) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:track_and_cancel_server_tasks_through_the_tui] the Task Center row ends canceled |
| `pulpcore` | Task: delete record | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_delete_and_purge]; typed task id [U]; F116 the task of a Pulp import answers 500 ([R56], API), shown as a bare server error |
| `pulpcore` | Task: filters beyond state/name/resource/start, `profile_artifacts` | ✅ | ❌ | ❌ | — | — | G11 |
| `pulpcore` | Task: purge finished tasks | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_delete_and_purge]; a group of purged tasks ends `gone` [U] |
| `pulpcore` | Task: server task list, filter, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:server_tasks_filter]; F126 on the first row a reload moves the cursor to the newest task, and the Cancel confirm shows only an href (static) |
| `pulpcore` | Task: track a server task (a) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:track_and_cancel_server_tasks_through_the_tui]; F61 fixed: Track of a stale row keeps the final state and label [U] |
| `pulpcore` | Task Center: dismiss finished tasks | — | ✅ | ✅ | ✅ | — | [R40] `z`: "dismissed 3 finished task(s) from the list (Pulp keeps its records)", 0 rows, and 0 rows after a restart; the palette path F80 [R33]: the dismissed rows come back after a restart |
| `pulpcore` | Task Center: re-attach after restart / Connect | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_center_reattaches_a_task_of_a_previous_session]; F93 references keyed by profile name: a profile Edit to another server stores them `gone` (static) |
| `pulpcore` | Task Center: track submitted task until final, reload visible list | ✅ | ✅ | ✅ | ✅ | ✅ | [R20]–[R23], [R28] every task row ended final and the visible list reloaded; F74 a failed late poll can change a final row (race); [R42] a task that ends while the Dashboard is open; F114 [R54] `/` shows "filter: zzzz" and hides no row |
| `pulpcore` | Task Center: task-group row (replication, import, ACS refresh, prune) | ✅ | ✅ | ✅ | ✅ | ✅ | [R13]; [LT:native_replication_through_the_tui, canceled_native_replication_is_not_complete] |
| `pulpcore` | Task Center: watch task live (W) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_schedules_screen, resume_watch_and_break_lock_through_the_tui] |
| `pulpcore` | Task group: cancel | ✅ | ✅ | ✅ | ✅ | ✅ | pulpcore ≥ 3.95; [LT:task_group_cancel] |
| `pulpcore` | Task group: list, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:task_groups_screen] |
| `pulpcore` | Task group: track / watch from the Task groups screen | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_groups_screen_tracks_and_watches_a_group] |
| `pulpcore` | Task schedule: create / edit | ❌ | — | — | — | — | Pulp unavailable: `task-schedules/` is GET only |
| `pulpcore` | Task schedule: list, watch last task | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_schedules_screen] |
| `pulpcore` | Upload (chunked): list, delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:an_unfinished_upload_is_listed_and_deleted] |
| `pulpcore` | Upstream Pulp: create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:upstream_pulp_create_edit_and_delete_through_the_tui] impact and typed name on delete; F91 on 3.95 and 3.63 a running replication is not named (static) |
| `pulpcore` | Upstream Pulp: replicate now (task group) | ✅ | ✅ | ✅ | ❌ | ✅ | **F101 (P2)**: [R52] policy `nodelete`, confirm with `y`: a local repository and distribution with the name of the upstream distribution were taken over (latest version −1 local file, +40; labels replaced; distribution re-pointed), while the form says "nodelete: never delete local objects"; the Task Center row ended `completed`, "tasks 5/5"; F118 the deletion warning says "gone upstream" also for a narrower filter (static); [LT:native_replication_through_the_tui] row ends completed, pulp1 serves; canceled run says CANCELED [LT:canceled_native_replication_is_not_complete]; failed run with masked user info [LT:upstream_base_url_user_info_never_shows_through_the_tui], [R13] |
| `pulpcore` | Upstream Pulp: replicate with per-request `q_select` | ✅ | ❌ | ❌ | — | — | G10 |
| `pulpcore` | Upstream Pulp: replication history | ✅ | ✅ | ✅ | ✅ | — | [LT:native_replication_through_the_tui, canceled_native_replication_is_not_complete]; by reserved upstream, by name on 3.63 / 3.95 |
| `pulpcore` | User: create / edit / set password / enable-disable | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:user_create_edit_password_disable_and_delete_through_the_tui] the passwords sign in; F86 "Staff (bypasses RBAC)" is false; F87 self-disable needs only `y` (static) |
| `pulpcore` | User: delete (self guard, impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:user_create_edit_password_disable_and_delete_through_the_tui] |
| `pulpcore` | User: list | ✅ | ✅ | ✅ | ✅ | — | [LT:user_create_edit_password_disable_and_delete_through_the_tui] |
| `pulpcore` | User: roles add / remove (object, domain scope) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:group_create_roles_and_delete_through_the_tui] (a user's Roles screen; the user then sees the repository); F98 [R48] after a Connect from this screen, the form assigns the role to the user with the same number on the new server |
| `pulpcore` | Versions: list per repository / all repositories | ✅ | ✅ | ✅ | ✅ | ✅ | [R20], [R21]; [LT:*_version_actions_through_the_tui] list reloads |
| `pulpcore` | Vulnerability reports: list / delete as a screen | ✅ | ❌ | ❌ | — | — | G17 |
| `pulpcore` | Worker: list, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:workers_screen] |
