# pulp-tui — functional coverage and async consistency audit

Date: 2026-11-09 (audit 7). Revision: `a0aaca9` (`main`, `v0.5.0-181-ga0aaca9`, clean tree except this untracked file).
Audit only: no source, test, doc or config change. This file is the only output.

This is a re-audit of audit 6 (2026-11-09, also of `a0aaca9`). No commit was made after audit 6
(`git diff a0aaca9` is empty), so none of its findings can be fixed; section 6.1 confirms that each one is still
there. The work of this audit went into what audit 6 and the audits before it missed. New findings continue the
numbering: F76–F97, G19–G20 and T06, so an ID never means two things. The findings of the audits before (F01–F65,
E01–E03, G01–G18, T01–T04) stay as audit 6 recorded them: fixed, with G03–G18 open by the user's decision.

## 1. Executive summary

The audit-6 findings F66–F75 and T05 are all still open (same revision). The full test suite passed again on fresh
containers at `a0aaca9` (685 unit, 53 mock HTTP, 162 live and 3 `real` tests on pulpcore 3.123.1; 2796 s; section 8).

Five reviewers with no earlier context searched one area each (repositories and remotes; versions, content,
publications and distributions; the app shell and Task Center; pulpcore administration; Content Views and the store).
Three other reviewers then tried to disprove each candidate against the code and the Pulp source in the container.
22 findings survived. Three are P2, and all three were reproduced through the real binary in this audit:

* **F76 (P2)**: **Reclaim** says "Pulp deletes the stored files of its synced content that no other repository
  has". pulpcore (3.63.36, 3.95.3 and 3.123.1 have the same code) keeps back only shared *published metadata*; it
  deletes the files of every other unit that the repository shares with other repositories, Content View snapshots
  included. Runtime [R29]: after a typed Reclaim of `a7-src`, the three units of repository `a7-copy` had
  `artifact: null`. They come back only on demand, and only while the upstream still serves them.
* **F77 (P2)**: **ACS Edit** of an ACS with no paths (Pulp stores `[""]`) skips the "Remove ACS paths" check, and
  pulpcore then deletes the `""` path row of *every* ACS on the server. Runtime [R30]: no dialog; the other ACS went
  from `[""]` to `[]`; its next Refresh made an empty task group ("Refreshing 0 alternate content source paths")
  that the Task Center shows as `gone`, "its tasks were purged". That ACS never indexes again.
* **F78 (P2)**: **Delete of a Content View version** can delete version 0 of a snapshot repository. Every later
  unfiltered RPM or Deb publish of that view then fails, and Resume cannot repair it. Runtime [R31]: v1 of `a7cv`
  recorded `…/versions/0/` (first publish of an empty source); after v2 and the typed delete of v1, Publish v3 failed
  with Pulp's 400 "Version 0 does not exist for repository 'pulp-tui-cv-a7cv-a7-os'".

19 findings are P3. Three of them were reproduced at runtime: **F79** Distribute in follow mode reports
"[ok] … serves URL" for a repository with no publication, and the URL answers 404 [R32]; **F80** the palette entry
"Tasks: dismiss finished" does not remove the stored references, so the 10 dismissed rows came back after a restart
[R33]; **F81** the RPM Checksum type choice offers `sha224`, which Pulp refuses with 400 [R34]. The others are static
findings, each checked against the plugin source: a false RPM `nested_by_digest` verify (F82), a File mirror sync
that the confirm accepts and Pulp refuses (F83), an `import-repos` that refuses every RPM repository with an OSV
config (F84), the Restore wording (F85), the false "Staff (bypasses RBAC)" texts (F86), self-lockout through a group
(F87), and smaller gaps in export, guard, domain, upstream, pagination, Task Center, config and Content View paths
(F88–F97).

Two new Pulp → client gaps: content labels (G19) and a self-service password change (G20).

The task model is still sound: only `completed` is success, dependent steps wait for their tasks, no mutation is
retried, and a `202` is never shown as done. The new async findings are about what a final state *says*: F77 calls an
empty task group "purged", F79 says "serves" for a 404, F80 brings dismissed rows back.

### Coverage summary (from the matrix in section 10)

Cells: ✅ / ❌ / ❓ / — (not applicable). `—` is not in any coverage ratio, and `❓` is never counted as a success.

| Plugin / area | Rows | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent |
|---|---:|---|---|---|---|---|
| `pulp-tui` | 40 | 0 / 0 / 0 / 40 | 40 / 0 / 0 / 0 | 40 / 0 / 0 / 0 | 39 / 1 / 0 / 0 | 20 / 0 / 1 / 19 |
| `pulp_deb` | 33 | 32 / 1 / 0 / 0 | 32 / 0 / 0 / 1 | 32 / 0 / 0 / 1 | 31 / 1 / 0 / 1 | 27 / 0 / 0 / 6 |
| `pulp_file` | 47 | 47 / 0 / 0 / 0 | 47 / 0 / 0 / 0 | 47 / 0 / 0 / 0 | 44 / 2 / 1 / 0 | 36 / 1 / 0 / 10 |
| `pulp_rpm` | 44 | 44 / 0 / 0 / 0 | 43 / 1 / 0 / 0 | 43 / 1 / 0 / 0 | 40 / 2 / 1 / 1 | 35 / 1 / 1 / 7 |
| `pulpcore` | 72 | 69 / 2 / 0 / 1 | 54 / 16 / 0 / 2 | 54 / 16 / 0 / 2 | 50 / 1 / 3 / 18 | 39 / 0 / 2 / 31 |
| **All** | 236 | 192 / 3 / 0 / 41 | 216 / 17 / 0 / 3 | 216 / 17 / 0 / 3 | 204 / 7 / 5 / 20 | 157 / 2 / 4 / 73 |

Reading the numbers (counted from the rows of section 10 with a script):

* **Rows:** 236 (audit 6: 234). New rows: G19 and G20 (both `pulpcore`).
* **API client and TUI:** the 17 ❌ rows are the open gaps G03–G18 (decision) and the new G19, G20.
* **Works end-to-end:** 204 of 216 applicable rows are proven through the TUI against real Pulp (audit 6: 210 of 216). The 7 ❌ rows are F66 (still open), F76 (Reclaim: File [R29], RPM and Deb static), F77 (ACS Edit: File [R30], RPM static) and F78 (Content View version delete [R31]). The 5 ❓ rows are unchanged: domains, ULN sync, Copy URL, the Artifacts screen, Task Center dismiss (now with F80 for its palette path).
* **Async state consistent:** 157 proven, 2 inconsistent, 4 not proven (audit 6: 159 / 0 / 4). The 2 ❌ rows are the File and RPM ACS refresh rows (F77: an empty task group shown as `gone`, "its tasks were purged" [R30]).
* P3 findings are named in their rows but do not set ❌ (the rule of the audits before).

## 2. Scope and environment

### Repository and tools

* pulp-tui `v0.5.0-181-ga0aaca9`, one Rust binary, about 43,000 lines in `src/` (unit tests in the same files).
  Rust 1.99.1, Fedora 44, Linux 7.2.
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
| Tested in this audit (`quay.io/pulp/pulp:latest`, image of 2026-11-01, fresh containers) | 3.123.1 | 3.39.5 | 3.11.1 | certguard 3.123.1; `domain_enabled: false`; read from `status/` in this session |
| Source read in this audit (`podman run --rm` of the `3.95.3` and `3.63.36` images) | 3.95.3 / 3.63.36 | 3.34.0 / 3.27.12 | 3.8.1 / 3.5.4 | used to scope F76, F77, F78, F81, F83, F84, G20 by version; no suite run on them in this audit |
| Audit 6 (same revision `a0aaca9`) | 3.123.1 and 3.95.3 | | | every scenario and `real` passed on both (section 8 of audit 6) |

### Test hosts

`http://127.0.0.1:8080` (pulp1) and `http://127.0.0.1:8081` (pulp2), local podman containers from
`tests/env/pulp-env.sh`. No container ran before this audit; `down-all` ran at the start of the suite and at the end
of the audit. User `admin` with the test-only password of the script. PostgreSQL 16 on `127.0.0.1:55432`. Fixture
HTTP server on port 8000, and a second one on port 8011 (scratch folder) with three random files for F76 and F77, so
that the Reclaim could not touch content that the suite shares. The runtime runs used their own config file and data
directory in a scratch folder with one `pulp1` profile (secret `env:PULP_PASSWORD`). No secret appears in this
report.

### Assumptions and limits

* "Works end-to-end ✅" needs a TUI-driven proof against a real server: a live test that calls `run(&mut app, Act::…)`
  (`tests/common/mod.rs`), or a runtime run in a PTY. Service-level live tests and mock tests give ❓. A ❌ needs a
  runtime proof, except where the row says "static".
* Content Views, environments, fallback replication and repository definitions are pulp-tui concepts, not Pulp
  APIs: `pulp-tui` rows with `—` in the Pulp API column.
* Not verified: domains (disabled on the server), ULN sync (needs an account), a TLS-terminating outer proxy (F92),
  macOS/Windows, two TUI sessions on one shared database.
* The runtime runs ran while the live suite ran on the same pulp1, with other object names.

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

Registry check: `src/tui/kinds.rs` is unchanged since audit 5; every action in `Kind::actions` has a handler arm; no
TODO, FIXME, `todo!` or `unimplemented!` in `src`. `Act::mutates` treats an action as mutating unless it is listed
as read-only, so every new impact job is blocked in read-only mode too.

**OpenAPI cross-check (this session).** The live `docs/api.json` of pulp1 (3.123.1) has 598 operations for pulpcore,
certguard, rpm, file and deb. Every operation maps to a matrix row, to a G gap, or to an `ObjectRoles` screen
(`list_roles`/`add_role`/`remove_role`/`my_permissions` also on tasks, uploads and task schedules), with two new gaps:
the `set_label`/`unset_label` operations of every content type (G19) and `PATCH login/` (G20). `DELETE login/`
(logout) has no use with Basic auth. Pulp's newer `content/file/files/upload/` and `content/rpm/packages/upload/`
endpoints are not used; the TUI posts to the content collection, which is equivalent.

## 4. Missing Pulp → client features

G03–G18 are unchanged since audit 3 and open by the user's decision (known gaps, built on demand only). G19 and G20
are new.

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

Pulp unavailable (not a pulp-tui defect): signing-service create (no API), task-schedule create/edit (GET only),
Deb copy with dependency solving (pulp_deb raises `FeatureNotImplementedError`), Pulp export `dry_run` (accepted but
ignored by pulpcore 3.123.1), the repository list of an export before the import (F28 limit), the
`architecture_variant` of a Deb package (a model field and part of the duplicate key in pulp_deb 3.11.1
`models/repository.py:283`, but not in `deb.PackageResponse` of the live OpenAPI).

## 5. Client → TUI exposure gaps

The client layer has no orphan operation (two dead helpers, section 2). No new exposure gap.

## 6. Broken or incomplete TUI workflows

Evidence: **runtime** = PTY run of the real binary against pulp1 in this session (section 8); **static** = code
reading, with the Pulp behavior checked in the plugin source of the container where named.

### 6.1 Status of the audit-6 findings

`git diff a0aaca9` is empty and `HEAD` is `a0aaca9`, so no fix exists. The code of each finding was read again.

| ID | Status | Evidence in this audit |
|---|---|---|
| F66 (P2) | not fixed | the Delete impact `actions_admin.rs:505-531` still checks only direct composite membership (`members.contains(&href)`); `guard_reach` is not called |
| F67 | not fixed | `distribute` → `pinned_publication` before `ensure_distribution`, unchanged |
| F68 | not fixed | `confirm_distribute` refuses a checkpoint distribution only for a pin, unchanged |
| F69 | not fixed | `bind_target` `?` on `named_distribution`, unchanged |
| F70 | not fixed | `resume_library_step`, unchanged |
| F71 | not fixed | `verify_repo` compares only what is served, unchanged |
| F72 | not fixed | the Publish preview, unchanged |
| F73 | not fixed | `add_to_repo`, `copy_preview` without `latest_goes`, unchanged |
| F74 | not fixed | `Tasks::poll_failed` / `stop` (`tasks.rs:125-142`) have no `is_final` check; `finish_task` (`store.rs:677-686`) has no `WHERE finished_at IS NULL` |
| F75 | not fixed | `role_form` (`actions_admin.rs:344-369`) PATCHes at once |
| T05 | not done | no new test |
| Polish of audit 6 | not done | unchanged |

The audit-6 texts follow unchanged, so that this file stays the only reference (line numbers are still valid: the code
did not change).

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

### 6.2 New findings

Root causes, so that a fix closes a class and not one case:

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

Candidate parts rejected after verification: the Edit form URL display (documented design, above); "export
incremental stops silently" (the Export form explains it: kept as F88 with the smaller scope); the guard Edit and the
domain default guard (no effect); "Restore does not name deleted versions" (it has a retention line: kept as F85);
`save_binding_repo` as the cause of F97 (the name comes from the version row); "every TLS deployment breaks" (only an
outer proxy: F92).

## 7. Asynchronous state consistency findings

### What holds (static and runtime)

* Only `completed` is success; groups need every task completed. Canceled and skipped tasks are warnings, failed
  tasks errors. No POST, PATCH, PUT or DELETE is retried (`client.rs:425-433`), redirects are off.
* Dependent steps wait for their tasks; Content View checkpoints store the task href before the wait. A failure moves
  the version to `failed` and keeps the served version ([R31]: v3 `failed`, Library still `v2`).
* Every runtime step in this audit ended with a final Task Center row, and the visible list reloaded: `a7cv` showed
  `Library:v1`, then `Library:v2`; the Versions list showed `deleted` and `failed` at once; the File list after the
  Reclaim; the ACS list after the Edit.
* Connect drops messages and jobs of the previous server; stale loads are dropped by generation.

### Async findings of this audit

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
| Reset + full suite (`latest`) | scratch script: `tests/env/pulp-env.sh down-all`, then each scenario in its own `CARGO_BUILD_JOBS=2 systemd-run --user --scope -p MemoryHigh=6G -p MemoryMax=10G -p CPUQuota=200% --quiet nice -n 19 tests/run-live.sh <scenario>` call | every call exit 0, 2796 s |
| Unit + mock HTTP | part of each call: `cargo test --lib --test mock_http` | 685 passed; 53 passed (in each of the 11 calls) |
| Live scenarios | repos / cv / rbac / failures / replication / shared_db / deb / export / content / admin | 44 / 27 / 2 / 3 / 12 / 10 / 16 / 8 / 11 / 29 passed (162) |
| Real upstreams | `real` (Debian `main`, Rocky 9) | 3 passed, 630 s (call 651 s) |
| Format, Markdown, advisories | `cargo fmt --check`; `dprint check` (tracked Markdown); `cargo audit` | exit 0; exit 0; exit 0 (RUSTSEC-2026-0459 ignored with a reason in `.cargo/audit.toml`; the same 4 allowed warnings) |
| Status and schema | `GET status/`, `GET docs/api.json` of pulp1 | versions in section 2; 598 in-scope operations (section 3) |
| Plugin source | `podman exec pulp1` reads (pulpcore `tasks/reclaim_space.py`, `serializers/acs.py`, `models/repository.py`, `access_policy.py`, `viewsets/user.py`, `serializers/user.py`, `models/exporter.py`, `models/domain.py`, `settings.py`; pulp_rpm `constants.py`, `viewsets/repository.py`, `tasks/publishing.py`, `serializers/repository.py`; pulp_deb `viewsets/repository.py`; pulp_file `viewsets.py`); `podman run --rm` of the 3.95.3 and 3.63.36 images for the same lines | used for F76–F92, G19, G20 |
| Proxy behavior | `curl -H 'X-Forwarded-Proto: https' -H 'Host: pulp.example.com' …/tasks/?limit=1` | `next` = `http://pulp.example.com/…` (F92) |
| Runtime TUI | `pulp-tui v0.5.0 (v0.5.0-181-ga0aaca9)` in tmux 200×50, own `--config` and `--data-dir`, profile `pulp1`, poll 500–3000 ms | R29–R34 below |
| Test count | a script over `tests/live_*.rs` and `tests/common/mod.rs` | 99 of 162 live tests drive the TUI (165 with `real`) |

Not run: clippy (audit 6 ran it at this revision; no code changed), `podman build -f Containerfile .`, mutation tests,
the suite on 3.95.3 and 3.63.36 (audit 6 ran 3.95.3 at this revision; the version facts of the new findings were read
from the old images' source).

### Runtime runs

R0–R28 are the runs of audits 3–6; the matrix still cites them where the code path did not change (the revision is
the same as audit 6). R29–R34 are the runs of this audit (`a7-*` objects, view `a7cv`; the scratch fixture server on
port 8011; pulpcore 3.123.1). They went with the containers at the final `down-all`.

| Run | Steps | Result |
|---|---|---|
| R29 F76 | API: remote `a7-rem` (immediate), `a7-src` synced (3 units), `a7-copy` gets the 3 units by `modify/`. File Repositories → `a7-src` → `R` → `Ctrl-S` → typed name | confirm "…that no other repository has."; "[ok] … deleted 3 file(s)"; `a7-copy` units `artifact: null` |
| R30 F77 | API: File ACS `a7-acs1`, `a7-acs2` with no paths (`[""]`). Alt. Sources → `t` → `a7-acs2` → `e` → Paths `sub/` → `Ctrl-S`; then `R` on `a7-acs1` | no dialog; `a7-acs1` `[]`; refresh group "Refreshing 0 alternate content source paths", Task Center `gone` "its tasks were purged" |
| R31 F78 | `a7-os` (RPM, empty). `a7cv` + `a7-os`, `P` v1 (`versions/0/`); sync; `P` v2; Versions → v1 → `d` → `v1`; `P` v3 | the delete lists `…/versions/0/`; Pulp keeps only version 1; v3 "rejected [400]: Version 0 does not exist …", `failed`; Library `v2` |
| R32 F79 | `a7-src` → `D` → Binding "follow repository latest" → `Ctrl-S` | "[ok] distribution a7-src serves …/a7-src/"; 404; 0 publications |
| R33 F80 | `:` "Tasks: dismiss finished"; `q`; start again; `:` Tasks | "dismissed 10 finished task(s)", 0 items; after restart 10 items |
| R34 F81 | RPM Repositories → `a7-os` → `e` → Checksum type `sha224` → `Ctrl-S` | choices `sha224 sha256 sha384 sha512`; "[error] … rejected [400]: … sha256, sha384, or sha512." |

### Verification of the candidates

* 5 search reviewers proposed 24 candidates and 3 polish items; 3 disprove reviewers checked each against code and
  Pulp source. 22 became F76–F97 (17 confirmed as proposed, 5 partly confirmed and rewritten to the confirmed part:
  F85, F88, F89, F92, F97), 1 became G20, 1 was rejected as documented design (the Edit form URL display).
* Checked again at runtime by this audit: F76, F77, F78, F79, F80, F81 (all reproduced).
* Checked again in the code by this audit: F78 (`cv.rs:1131-1134`, `:1946-1954`), F85 (`restore_lines`), F77
  (`acs_paths`, `acs_edit`), F80 (palette entry), F92 (`url_for`), and the audit-6 findings of section 6.1.
* Taken from the verifiers' reading, with the lines quoted in this file: F82–F84, F86–F91, F93–F97, the polish items.

### Blockers

* Domains: `domain_enabled: false`; F89, F90 and the domain rows stay static / ❓.
* ULN sync needs an Oracle ULN account.
* OSC 52 clipboard (Copy URL) cannot be observed in tmux capture.
* No TLS-terminating outer proxy in the test environment (F92).
* F91 needs a 3.95 or 3.63 server with a running replication; F95–F97 need multi-profile or rename set-ups; not run.

## 9. Prioritized remediation backlog

| Pri | ID | Action | Plugins / features | Likely fix location | Acceptance criteria | Suggested test |
|---|---|---|---|---|---|---|
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
| — | G03–G20 | Optional Pulp features; G19, G20 new | various | — | built on demand (user decision for G03–G18) | — |

## 10. Final comprehensive functionality matrix

Evidence tags: **[Rn]** = runtime run of the real binary in a PTY (tmux) against pulp1 (section 8; R0–R6 of audit 3 at
`3f59b9b`, R7–R11 of audit 4 at `f530b17`, R12–R19 of audit 5 at `5812b99`, R20–R28 of audit 6 and R29–R34 of this audit, both at `a0aaca9`);
**[LT:x]** = live test that drives the TUI action (`run(&mut app, Act::…)`), passed in this audit's run at `a0aaca9`;
**[LS:x]** = live test at service level (calls `ops`/`CvService`/`Store` directly; the TUI path is not exercised),
passed in this run; **[U]** = unit or mock-HTTP test only; **[S]** = static analysis. *Works end-to-end* is ✅ only
with [Rn] or [LT]. App-level rows (`pulp-tui`) are not Pulp APIs: their *Pulp API* cell is `—`.
`*_x_through_the_tui` names a test that runs for File, RPM and Deb (or the types it names). P3 findings are named in
their rows but do not set ❌.

Totals of this matrix (✅ / ❌ / ❓ / — per column; `—` is outside every ratio, `❓` is not a success):

| Plugin / area | Rows | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent |
|---|---:|---|---|---|---|---|
| `pulp-tui` | 40 | 0 / 0 / 0 / 40 | 40 / 0 / 0 / 0 | 40 / 0 / 0 / 0 | 39 / 1 / 0 / 0 | 20 / 0 / 1 / 19 |
| `pulp_deb` | 33 | 32 / 1 / 0 / 0 | 32 / 0 / 0 / 1 | 32 / 0 / 0 / 1 | 31 / 1 / 0 / 1 | 27 / 0 / 0 / 6 |
| `pulp_file` | 47 | 47 / 0 / 0 / 0 | 47 / 0 / 0 / 0 | 47 / 0 / 0 / 0 | 44 / 2 / 1 / 0 | 36 / 1 / 0 / 10 |
| `pulp_rpm` | 44 | 44 / 0 / 0 / 0 | 43 / 1 / 0 / 0 | 43 / 1 / 0 / 0 | 40 / 2 / 1 / 1 | 35 / 1 / 1 / 7 |
| `pulpcore` | 72 | 69 / 2 / 0 / 1 | 54 / 16 / 0 / 2 | 54 / 16 / 0 / 2 | 50 / 1 / 3 / 18 | 39 / 0 / 2 / 31 |
| **All** | 236 | 192 / 3 / 0 / 41 | 216 / 17 / 0 / 3 | 216 / 17 / 0 / 3 | 204 / 7 / 5 / 20 | 157 / 2 / 4 / 73 |

### `pulp-tui` (application-level features built on the Pulp API)

| Plugin / area | Resource / operation | Pulp API | API client | Available in TUI | Works end-to-end | Async state consistent | Explanation / evidence |
|---|---|:---:|:---:|:---:|:---:|:---:|---|
| `pulp-tui` | Content View: add repositories (+ filter rules) | — | ✅ | ✅ | ✅ | — | [R23] multi-select picker, `audit6-a`, `audit6-b`, then `audit6-d`; store only, no task |
| `pulp-tui` | Content View: auto publish of composites | — | ✅ | ✅ | ✅ | ✅ | [LT:composite_content_view_through_the_tui] a Publish of the extras auto-publishes the composite; [LT:resume_publishes_a_lagging_composite_through_the_tui] |
| `pulp-tui` | Content View: break lock | — | ✅ | ✅ | ✅ | — | [LT:resume_watch_and_break_lock_through_the_tui] |
| `pulp-tui` | Content View: compare versions | — | ✅ | ✅ | ✅ | — | [LT:content_view_workflows_through_the_tui] (Compare on Versions) |
| `pulp-tui` | Content View: composite create, add/pin/remove components, publish | — | ✅ | ✅ | ✅ | ✅ | [LT:composite_content_view_through_the_tui] Library serves the composite's paths |
| `pulp-tui` | Content View: create | — | ✅ | ✅ | ✅ | — | [R23]; store only |
| `pulp-tui` | Content View: delete view (impact, distributions, snapshots) | — | ✅ | ✅ | ✅ | ✅ | [R16]; [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View: drift check of all environments | — | ✅ | ✅ | ✅ | ✅ | [R4]; [LT] `CvDrift`; F60 fixed [LS:a_kept_library_distribution_bound_to_a_repository_is_reported_and_unbound]; F71 ownership loss not reported, wrong hint |
| `pulp-tui` | Content View: edit (description, pins, auto publish) | — | ✅ | ✅ | ✅ | — | [LT:environments_and_view_definition_through_the_tui, composite_content_view_through_the_tui] |
| `pulp-tui` | Content View: edit filter rules | — | ✅ | ✅ | ✅ | — | [LT:content_view_workflows_through_the_tui] Filters then a second Publish |
| `pulp-tui` | Content View: export / import definitions (E / I on Servers) | — | ✅ | ✅ | ✅ | ✅ | [LT:environment_relabel_and_definitions_export_import_through_the_tui] |
| `pulp-tui` | Content View: left-out repository on publish / promote (unbind; forget a gone or taken-over distribution) | — | ✅ | ✅ | ✅ | ✅ | F56 fixed: [R28] a row with a publication on a taken-over distribution is forgotten as the preview says, audit-trail line; [LT:a_left_out_distribution_deleted_outside_pulp_tui_is_forgotten_through_the_tui, a_kept_library_distribution_taken_over_outside_pulp_tui_is_left_alone_through_the_tui]; F59 fixed [LS:a_404_of_a_hidden_distribution_keeps_a_content_view_row_for_a_user] |
| `pulp-tui` | Content View: preview draft vs latest version | — | ✅ | ✅ | ✅ | — | [R23] "In Library:" refusal lines with the way out (F55, F57 fixed); F69 a name collision fails the whole preview; F72 two `publish` refusals not previewed |
| `pulp-tui` | Content View: promote to environment — failure path | — | ✅ | ✅ | ✅ | ✅ | [LT:a_promotion_to_an_overlapping_base_path_is_named_and_creates_nothing_through_the_tui, a_partial_promotion_and_what_the_screens_show_through_the_tui]; [R15] |
| `pulp-tui` | Content View: promote to environment — success path | — | ✅ | ✅ | ✅ | ✅ | [R15]; [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View: publish new version (Deb, signing choice) | — | ✅ | ✅ | ✅ | ✅ | [LT:deb_content_view_publish_signing_through_the_tui] signed / none / same as source |
| `pulp-tui` | Content View: publish new version (RPM + File) | — | ✅ | ✅ | ✅ | ✅ | [R23] v1 `Library:v1`, v2 partial exactly for the previewed repositories (`Library:v1 (v2 partial)`); [R28] v3 `Library:v3`; F95 two local profiles of one server share the derived objects (static) |
| `pulp-tui` | Content View: remove repository | — | ✅ | ✅ | ✅ | — | [R28]; [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Content View: resume interrupted publish | — | ✅ | ✅ | ✅ | ✅ | [LT:resume_watch_and_break_lock_through_the_tui]; F58 fixed (Resume confirm has the "In Library:" lines, [U]); F70 deleted version; F96 a gone source version gives a raw error on every Resume (static) |
| `pulp-tui` | Content View: roll back environment | — | ✅ | ✅ | ✅ | ✅ | [LT:content_view_workflows_through_the_tui]; F97 after a source rename and re-add, the other derived distribution stays unrecorded (static) |
| `pulp-tui` | Content View: sync source repositories (f) | — | ✅ | ✅ | ✅ | ✅ | [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View: unpromote | — | ✅ | ✅ | ✅ | ✅ | [LT:content_view_workflows_through_the_tui] |
| `pulp-tui` | Content View database: migrate / copy (offered on Connect, save) | — | ✅ | ✅ | ✅ | — | [LT:database_migrate_and_copy_through_the_tui]; [LS:postgres_migration_9_restores_the_served_version_of_an_old_failed_row] |
| `pulp-tui` | Content View version: delete | — | ✅ | ✅ | ❌ | ✅ | **F78 (P2)**: [R31] a version that records `…/versions/0/` of a snapshot repository deletes it; every later unfiltered RPM or Deb publish fails with 400; [R15]; [LT:content_view_workflows_through_the_tui, a_version_that_dev_serves_in_part_is_not_deleted_through_the_tui] |
| `pulp-tui` | Content View version: list | — | ✅ | ✅ | ✅ | ✅ | [R15], [R23] states and environments |
| `pulp-tui` | Content View version: watch pending task | — | ✅ | ✅ | ✅ | ✅ | [LT:resume_watch_and_break_lock_through_the_tui] (Watch W) |
| `pulp-tui` | Environment: create | — | ✅ | ✅ | ✅ | — | [R23] `A6Dev` after Library; [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Environment: delete | — | ✅ | ✅ | ✅ | — | [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Environment: relabel (moves distributions) | — | ✅ | ✅ | ✅ | ✅ | [LT:environment_relabel_and_definitions_export_import_through_the_tui]; refusal [LT:relabel_to_an_overlapping_label_is_refused_through_the_tui] |
| `pulp-tui` | Environment: toggle protected | — | ✅ | ✅ | ✅ | — | [LT:environments_and_view_definition_through_the_tui] |
| `pulp-tui` | Fallback replication: dry run and run (RPM, File, Deb) | — | ✅ | ✅ | ✅ | ✅ | [LT:fallback_replication_through_the_tui, deb_fallback_replication_through_the_tui]; F57 fixed [LS:the_fallback_plan_names_a_base_path_that_overlaps_a_local_distribution] |
| `pulp-tui` | Form: keys typed ahead after Enter are held, with a footer marker | — | ✅ | ✅ | ✅ | — | [R27] `-- WAIT --`; [R19] |
| `pulp-tui` | Logs screen (session requests, actions, notices) | — | ✅ | ✅ | ✅ | — | [LT:upstream_base_url_user_info_never_shows_through_the_tui] every Logs row and detail masked (F64 fixed); 64 KiB cut (polish) |
| `pulp-tui` | Read-only profile blocks mutations | — | ✅ | ✅ | ✅ | — | [LT:server_profiles_create_edit_and_delete_through_the_tui] a mutation is refused at once |
| `pulp-tui` | Repository definitions: export (X) | — | ✅ | ✅ | ✅ | — | [LT:repository_definitions_export_and_import] |
| `pulp-tui` | Repository definitions: import (J, plan then create) | — | ✅ | ✅ | ✅ | ✅ | [LT:repository_definitions_export_and_import]; headline count (polish); F84 an RPM repository with an OSV config is refused with 400 (static) |
| `pulp-tui` | Server profile: connect / switch host | — | ✅ | ✅ | ✅ | ❓ | [R0], [LT:server_profiles_create_edit_and_delete_through_the_tui]; dropping stale task messages on a switch [U] only; `last_repo` kept (polish) |
| `pulp-tui` | Server profile: create / edit / delete | — | ✅ | ✅ | ✅ | — | [LT:server_profiles_create_edit_and_delete_through_the_tui] keeps the file comment; F94 the config is written on the UI thread (static) |
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
| `pulp_deb` | Content: remove units (with release records) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui] the screen moves to the new version |
| `pulp_deb` | Content: upload `.deb` (distribution, component, overwrite) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:uploads_of_each_type_through_the_tui] |
| `pulp_deb` | Content: upload `.udeb` / source package / generic content | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:the_tui_uploads_a_udeb_a_source_package_and_generic_content_as_their_types] |
| `pulp_deb` | Distribution: create / edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui] base path, binding, guard and checkpoint edits after the typed name |
| `pulp_deb` | Distribution: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui] (single); marked [U] |
| `pulp_deb` | Distribution: list and binding details | ✅ | ✅ | ✅ | ✅ | — | [LT:deb_publications_and_distributions_through_the_tui] |
| `pulp_deb` | Distribution: re-point / rollback (keeps signing and verbatim mode) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui]; [LS:deb_rollback_keeps_a_simple_publication] |
| `pulp_deb` | Distribution: verify served content | ✅ | ✅ | ✅ | ✅ | — | [LT:deb_publications_and_distributions_through_the_tui] "OK: " |
| `pulp_deb` | Publication: create APT (structured / simple / signing) | ✅ | ✅ | ✅ | ✅ | ✅ | [R3]; [LT:deb_publications_and_distributions_through_the_tui] |
| `pulp_deb` | Publication: create verbatim | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_verbatim_publication_create_through_the_tui] |
| `pulp_deb` | Publication: delete (APT, verbatim; served-after impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:deb_publications_and_distributions_through_the_tui] names the distribution and what it serves after |
| `pulp_deb` | Publication: list APT + verbatim (merged, newest first) | ✅ | ✅ | ✅ | ✅ | — | [LT:deb_publications_and_distributions_through_the_tui] |
| `pulp_deb` | Remote: create (distributions, components, architectures, udebs, sources, GPG key) | ✅ | ✅ | ✅ | ✅ | ✅ | [R3]; [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_deb` | Remote: delete (impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] names the repository; Pulp unsets its remote |
| `pulp_deb` | Remote: edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] (a task) |
| `pulp_deb` | Repository: copy content from another repository (`deb/copy/`, filters) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:copy_content_through_the_tui] name rule; F73 |
| `pulp_deb` | Repository: copy with dependency solving | ❌ | — | — | — | — | Pulp unavailable: pulp_deb 3.11.1 raises `FeatureNotImplementedError` |
| `pulp_deb` | Repository: create | ✅ | ✅ | ✅ | ✅ | ✅ | [R3] with the remote picker |
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
| `pulp_file` | Content: browse, search paths | ✅ | ✅ | ✅ | ✅ | — | [LT:content_add_and_remove_through_the_tui] |
| `pulp_file` | Content: remove units | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui]; a snapshot of another database refused [LT:snapshot_versions_publications_and_content_of_another_database_are_not_changed_through_the_tui] |
| `pulp_file` | Content: upload chunked (progress, digest check) | ✅ | ✅ | ✅ | ✅ | ✅ | [R1] 12 MB file; `latest` 1 → 2; no upload record left |
| `pulp_file` | Content: upload single file | ✅ | ✅ | ✅ | ✅ | ✅ | [R1]; [LT:uploads_of_each_type_through_the_tui]; F73 overwrite has no retention check |
| `pulp_file` | Content: upload without overwrite | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_upload_without_overwrite_through_the_tui] |
| `pulp_file` | Distribution: checkpoint (verify, rollback refused) | ✅ | ✅ | ✅ | ✅ | — | [LT:checkpoint_distribution_verifies_and_refuses_rollback]; field from pulpcore 3.95 |
| `pulp_file` | Distribution: copy content URL (OSC 52) | ✅ | ✅ | ✅ | ❓ | — | clipboard not observable in tmux; [U] |
| `pulp_file` | Distribution: create / edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] base path and binding edits after the typed name, old URL 404 |
| `pulp_file` | Distribution: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui]; marked [U] |
| `pulp_file` | Distribution: edit of content guard or checkpoint (all types) | ✅ | ✅ | ✅ | ✅ | ✅ | [R9]; [LT:*_publications_and_distributions_through_the_tui] guard none → A → B → none with guard names, checkpoint on/off |
| `pulp_file` | Distribution: list and binding details | ✅ | ✅ | ✅ | ✅ | — | [R1]; [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Distribution: re-point an existing distribution with Distribute (D) from Repositories, Versions or Publications (all types) | ✅ | ✅ | ✅ | ✅ | ✅ | F54 fixed: [R21] preview and typed name, Esc changes nothing; [LT:file_distribute_re_points_after_the_typed_name_through_the_tui, deb_distribute_re_points_after_the_typed_name_through_the_tui]; F68 [R25] follow onto a checkpoint distribution; repository id in "now serves" (polish) |
| `pulp_file` | Distribution: re-point to a version (b) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] the job re-points and verifies |
| `pulp_file` | Distribution: verify served content | ✅ | ✅ | ✅ | ✅ | — | [LT:checkpoint_distribution_verifies_and_refuses_rollback, file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Publication: checkpoint publication that clears older checkpoints (impact, typed name; all types) | ✅ | ✅ | ✅ | ✅ | ✅ | [R22] the confirm names the checkpoint that loses the flag; after the typed name Pulp and the list agree; mock `a_checkpoint_publication_that_clears_older_checkpoints_needs_the_typed_name`; no live test (T05); pulpcore 3.123 (`retain_checkpoints`) |
| `pulp_file` | Publication: create (manifest name, checkpoint) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Publication: delete (served-after impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Publication: list | ✅ | ✅ | ✅ | ✅ | — | [R22]; [LT:file_publications_and_distributions_through_the_tui] |
| `pulp_file` | Remote (Git): create | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_git_remote_syncs_a_repository_with_the_prefilled_form]; pulpcore 3.123 only |
| `pulp_file` | Remote (Git): delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] (File Git) |
| `pulp_file` | Remote (Git): edit `git_ref` | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:file_git_remote_syncs_a_repository_with_the_prefilled_form] |
| `pulp_file` | Remote: create (timeouts, headers, TLS) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remote_timeouts_keep_decimal_seconds_through_the_tui] |
| `pulp_file` | Remote: delete (impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] |
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
| `pulp_rpm` | Content: remove units | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_add_and_remove_through_the_tui] |
| `pulp_rpm` | Distribution: create / edit (`generate_repo_config`, hidden, guard) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui] guard and checkpoint edits after the typed name |
| `pulp_rpm` | Distribution: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui]; marked [U] |
| `pulp_rpm` | Distribution: list and binding details | ✅ | ✅ | ✅ | ✅ | — | [R2] `pinned` binding shown |
| `pulp_rpm` | Distribution: re-point (signing repository makes a new publication) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui]; signing case [LS:rpm_rollback_of_a_signing_repository_serves_signed_metadata] |
| `pulp_rpm` | Distribution: verify served content | ✅ | ✅ | ✅ | ✅ | — | [R2] `… served with matching digest (5 packages)`; F82 a `nested_by_digest` publication gives a false MISMATCH (static) |
| `pulp_rpm` | Publication: create (checksum, compression, layout, repo config, checkpoint) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui]; F81 `sha224` offered (static) |
| `pulp_rpm` | Publication: delete (served-after impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_publications_and_distributions_through_the_tui] |
| `pulp_rpm` | Publication: list | ✅ | ✅ | ✅ | ✅ | — | [LT:rpm_publications_and_distributions_through_the_tui] |
| `pulp_rpm` | Remote (ULN): create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:uln_remote_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Remote: create (SLES token, headers, timeouts) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2]; [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Remote: delete (impact, ACS refusal) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Remote: edit | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:remotes_create_edit_and_delete_through_the_tui] |
| `pulp_rpm` | Repository: copy content, filtered (+ advisories, metadata, deps) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_filtered_copy_through_the_tui_takes_the_advisory_of_the_chosen_packages]; F73 |
| `pulp_rpm` | Repository: advanced copy (several config entries, `dest_base_version`, `dependency_upgrade`) | ✅ | ❌ | ❌ | — | — | G03 (open by decision) |
| `pulp_rpm` | Repository: copy content, unfiltered (`rpm/copy/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:copy_content_through_the_tui] RPM without rules |
| `pulp_rpm` | Repository: create (remote picker, RPM fields) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2]; [LT:rpm_osv_config_survives_a_label_edit_through_the_tui] |
| `pulp_rpm` | Repository: delete (single, marked) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:repository_delete_single_and_marked_through_the_tui] |
| `pulp_rpm` | Repository: distribute (follow latest) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_serve_from_versions_through_the_tui]; F79 (static for RPM) |
| `pulp_rpm` | Repository: distribute (pin latest) | ✅ | ✅ | ✅ | ✅ | ✅ | [R2] `repodata/repomd.xml` 200; re-point after the typed name (F54, `pulp_file` row); F67 with a signing service makes a new publication on each try |
| `pulp_rpm` | Repository: edit (labels + OSV config, retain package versions) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rpm_osv_config_survives_a_label_edit_through_the_tui, rpm_retain_package_versions_cleared_through_the_tui]; Retain versions: F53 fixed; F81 [R34] `sha224` offered, Pulp 400 |
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
| `pulpcore` | Artifact: list, find by sha256 | ✅ | ✅ | ✅ | ❓ | — | [LS:artifacts_screen_lists_and_finds_a_sha256] (screen load, no action) |
| `pulpcore` | Artifact: upload / delete | ✅ | ❌ | ❌ | — | — | G16; delete is deprecated and refused by the default policy |
| `pulpcore` | Content guard: create `core/header` | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_guard_create_edit_and_delete_name_its_distribution] |
| `pulpcore` | Content guard: create other types (rbac, envvar header, redirect, composite, x509, RHSM) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_guards_of_the_other_types_are_created_through_the_tui] (`core/envvar_header` from pulpcore 3.95) |
| `pulpcore` | Content guard: delete (impact: distributions, composites) | ✅ | ✅ | ✅ | ❌ | ✅ | **F66 (P2)**: [R26] a guard inside two composite levels: "No distribution uses it", and the distribution behind the outer composite is public after the delete; direct use [LT:content_guard_create_edit_and_delete_name_its_distribution]; F89 a domain default guard is not named (static, domains only) |
| `pulpcore` | Content guard: edit (impact through composites at any depth) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:content_guard_create_edit_and_delete_name_its_distribution] confirm, Esc, description-only edit at once; unit two composite levels |
| `pulpcore` | Content guard: RBAC download access (add/remove role) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:rbac_guard_access_allows_and_removes_a_user] |
| `pulpcore` | Content: server-wide search (`content/`, generic lists) | ✅ | ❌ | ❌ | — | — | G14 |
| `pulpcore` | Content: set / unset labels (`{content}set_label/`) | ✅ | ❌ | ❌ | — | — | G19 (new): no client method; in the 3.123.1 OpenAPI for every RPM, File and Deb content type |
| `pulpcore` | Current user and capabilities (status, login, OpenAPI) | ✅ | ✅ | ✅ | ✅ | — | [R0], [R12]; every [LT]; F92 behind an outer TLS proxy a multi-page `list_all` fails (static; Pulp side checked with curl) |
| `pulpcore` | Dashboard: health, versions, counts | ✅ | ✅ | ✅ | ✅ | ❓ | [R12]; this audit's start screen; reload on task end [U] |
| `pulpcore` | Data repair `datarepair/7272/`, `livez/` | ✅ | ❌ | ❌ | — | — | G13 |
| `pulpcore` | Distribution: bind directly to a repository version (no publication pin) | ✅ | ❌ | ❌ | — | — | G15 |
| `pulpcore` | Domain: create / edit / delete / migrate storage | ✅ | ✅ | ✅ | ❓ | ❓ | Pulp feature off on the test server (`domain_enabled: false`); gating [LS:domains_screen_follows_domain_enabled]; operations [U]; F90 deleting the profile's own domain has no warning (static) |
| `pulpcore` | Domain: labels, default content guard, rename | ✅ | ❌ | ❌ | — | — | G08 |
| `pulpcore` | Exporter (filesystem): create / edit / delete / export | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:filesystem_exporter_through_the_tui] |
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
| `pulpcore` | Object roles: list, my permissions, add, remove | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:object_roles_add_and_remove_a_viewer_on_a_repository] |
| `pulpcore` | OpenPGP keyrings, keys, OpenPGP distributions | ✅ | ❌ | ❌ | — | — | G05 |
| `pulpcore` | Orphan cleanup (age; whole server) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:prune_orphans_through_the_tui] Pulp's count, typed profile name, result; G12 scope not offered |
| `pulpcore` | Repository: `modify/` with a base version, or add and remove in one call | ✅ | ❌ | ❌ | — | — | G04 |
| `pulpcore` | Repository: reclaim space of several repositories in one request | ✅ | ❌ | ❌ | — | — | G18 |
| `pulpcore` | Role: create / edit / delete custom role | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:custom_role_create_edit_and_delete_through_the_tui] (Create with no row selected); F75 Edit changes every holder with no impact |
| `pulpcore` | Role: list | ✅ | ✅ | ✅ | ✅ | — | [LT:custom_role_create_edit_and_delete_through_the_tui] |
| `pulpcore` | Server repair (`repair/`) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:server_repair_checks_that_the_files_exist] |
| `pulpcore` | Signing service: create | ❌ | — | — | — | — | Pulp unavailable: no API; list is used in pickers |
| `pulpcore` | Task: cancel (single) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:track_and_cancel_server_tasks_through_the_tui] the Task Center row ends canceled |
| `pulpcore` | Task: delete record | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_delete_and_purge]; typed task id [U] |
| `pulpcore` | Task: filters beyond state/name/resource/start, `profile_artifacts` | ✅ | ❌ | ❌ | — | — | G11 |
| `pulpcore` | Task: purge finished tasks | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_delete_and_purge]; a group of purged tasks ends `gone` [U] |
| `pulpcore` | Task: server task list, filter, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:server_tasks_filter] |
| `pulpcore` | Task: track a server task (a) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:track_and_cancel_server_tasks_through_the_tui]; F61 fixed: Track of a stale row keeps the final state and label [U] |
| `pulpcore` | Task Center: dismiss finished tasks | — | ✅ | ✅ | ❓ | — | local store [U]; the palette path F80 [R33]: the dismissed rows come back after a restart (the `z` path not run) |
| `pulpcore` | Task Center: re-attach after restart / Connect | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_center_reattaches_a_task_of_a_previous_session]; F93 references keyed by profile name: a profile Edit to another server stores them `gone` (static) |
| `pulpcore` | Task Center: track submitted task until final, reload visible list | ✅ | ✅ | ✅ | ✅ | ✅ | [R20]–[R23], [R28] every task row ended final and the visible list reloaded; F74 a failed late poll can change a final row (race) |
| `pulpcore` | Task Center: task-group row (replication, import, ACS refresh, prune) | ✅ | ✅ | ✅ | ✅ | ✅ | [R13]; [LT:native_replication_through_the_tui, canceled_native_replication_is_not_complete] |
| `pulpcore` | Task Center: watch task live (W) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_schedules_screen, resume_watch_and_break_lock_through_the_tui] |
| `pulpcore` | Task group: cancel | ✅ | ✅ | ✅ | ✅ | ✅ | pulpcore ≥ 3.95; [LT:task_group_cancel] |
| `pulpcore` | Task group: list, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:task_groups_screen] |
| `pulpcore` | Task group: track / watch from the Task groups screen | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_groups_screen_tracks_and_watches_a_group] |
| `pulpcore` | Task schedule: create / edit | ❌ | — | — | — | — | Pulp unavailable: `task-schedules/` is GET only |
| `pulpcore` | Task schedule: list, watch last task | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:task_schedules_screen] |
| `pulpcore` | Upload (chunked): list, delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:an_unfinished_upload_is_listed_and_deleted] |
| `pulpcore` | Upstream Pulp: create / edit / delete | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:upstream_pulp_create_edit_and_delete_through_the_tui] impact and typed name on delete; F91 on 3.95 and 3.63 a running replication is not named (static) |
| `pulpcore` | Upstream Pulp: replicate now (task group) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:native_replication_through_the_tui] row ends completed, pulp1 serves; canceled run says CANCELED [LT:canceled_native_replication_is_not_complete]; failed run with masked user info [LT:upstream_base_url_user_info_never_shows_through_the_tui], [R13] |
| `pulpcore` | Upstream Pulp: replicate with per-request `q_select` | ✅ | ❌ | ❌ | — | — | G10 |
| `pulpcore` | Upstream Pulp: replication history | ✅ | ✅ | ✅ | ✅ | — | [LT:native_replication_through_the_tui, canceled_native_replication_is_not_complete]; by reserved upstream, by name on 3.63 / 3.95 |
| `pulpcore` | User: create / edit / set password / enable-disable | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:user_create_edit_password_disable_and_delete_through_the_tui] the passwords sign in; F86 "Staff (bypasses RBAC)" is false; F87 self-disable needs only `y` (static) |
| `pulpcore` | User: delete (self guard, impact) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:user_create_edit_password_disable_and_delete_through_the_tui] |
| `pulpcore` | User: list | ✅ | ✅ | ✅ | ✅ | — | [LT:user_create_edit_password_disable_and_delete_through_the_tui] |
| `pulpcore` | User: roles add / remove (object, domain scope) | ✅ | ✅ | ✅ | ✅ | ✅ | [LT:group_create_roles_and_delete_through_the_tui] (a user's Roles screen; the user then sees the repository) |
| `pulpcore` | Versions: list per repository / all repositories | ✅ | ✅ | ✅ | ✅ | ✅ | [R20], [R21]; [LT:*_version_actions_through_the_tui] list reloads |
| `pulpcore` | Vulnerability reports: list / delete as a screen | ✅ | ❌ | ❌ | — | — | G17 |
| `pulpcore` | Worker: list, inspect | ✅ | ✅ | ✅ | ✅ | — | [LT:workers_screen] |
