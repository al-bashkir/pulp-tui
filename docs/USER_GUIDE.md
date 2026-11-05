# User guide

## Layout

```
 pulp-tui | server: lab (https://pulp…) | user: admin  READ-ONLY  PRODUCTION | jobs 1 | tasks 2 active
┌ Screens ─────────┐┌ <screen title> [items, filter, sort, marks] ─┐┌ Details ───────────────┐
│▶ Dashboard       ││ ▶ row                                         ││ explanation + full JSON│
│  Servers         ││   row                                         ││                        │
│  …               ││                                               ││                        │
└──────────────────┘└───────────────────────────────────────────────┘└────────────────────────┘
 key hints
 [ok] last notification  (or the running operation and its progress)
```

* The header shows the active server. A read-only profile shows a yellow tag. A production
  profile makes the header red.
* A narrow terminal hides the screen list (use the palette) and the details pane. Below 60x15 the
  UI asks for a larger terminal.
* `▶` and reverse video show the selection, so it does not depend on color.

## Common rules

These rules apply to all screens. The screen sections do not repeat them.

* **Read-only profile.** The action menu marks with `*` each action that changes Pulp or a
  shared database. A read-only profile does not show these actions. See "What is shared" for
  the Content View rules.
* **Production profile.** Rollbacks, promotions, *Restore as latest* and Importer imports ask
  you to type the object name.
* **Destructive actions.** pulp-tui loads the impact first (what serves the object, what it
  deletes). Then you type the shown name to confirm.
* **Capabilities.** A form field shows only when the server has it, and is sent only when you set
  or change it. An action that needs a missing endpoint shows
  `not supported by this server: <feature>` and sends nothing.
* **Edit** sends only the changed fields. Clear a field to remove the setting, or to return to the
  server default.
* **Secret fields** (passwords, keys, tokens, headers) never show their value. On edit, empty keeps
  the server value and `-` clears it.
* **Tasks.** An action that starts a Pulp task waits for the task. The notice then gives the
  result, for example the new version number.
* **Signing service pickers** list the signing services of all plugins. Pulp refuses a service of
  the wrong type.
* **Content View objects** belong to pulp-tui: snapshot repositories (`pulp-tui-cv-<view>-<repo>`,
  label `pulp_tui_cv`) and Content View distributions. The Pulp screens do not edit, sync, upload
  to, copy into, reclaim or restore a snapshot repository, change its content, or delete it, its
  versions or its publications. Use the Content View actions.
* **Dialogs of background jobs** wait until the open dialog closes (the footer shows
  `N dialog(s) waiting`). A new dialog ignores keys for 0.3 s.

### Three kinds of cancel

1. **Esc on a loading list** cancels the HTTP request. Esc does not stop an operation (sync,
   publish, replication, upload).
2. **Quit** or **Connect** stops waiting for client operations, so both ask first. Pulp tasks
   continue, and the Task Center attaches them again at the next start. When a task cannot be read
   for about two minutes, the operation ends with `could not read task …, it may still be running`.
   This is not a failure.
3. **X on Tasks** asks Pulp to cancel a task or a task group. The Task Center then shows the state
   that Pulp reports, and reads the task again at once. When pulp-tui cannot read the state after
   the request, the cancel is still requested; a warning tells you that the state was not read.

## Keys

| Key                                   | Action                                                   |
| ------------------------------------- | -------------------------------------------------------- |
| `Tab`                                 | move focus between the screen list and the main area     |
| `:` / `Ctrl-P`                        | command palette (see below)                              |
| `?`                                   | help (`j k g G Ctrl-D Ctrl-U` scroll, `Esc` closes)      |
| `N`                                   | notification log                                         |
| `q` / `Ctrl-C`                        | quit (`q` asks first while client operations run)        |
| `↑ ↓ j k PgUp PgDn Ctrl-D Ctrl-U g G` | move                                                     |
| `h` / `l`                             | go to the screen list / open the selected screen         |
| `Enter`                               | action menu: shortcut of each action, `*` marks a change |
| `/`                                   | search (empty clears)                                    |
| `s` / `S`                             | sort the current page by the next column / reverse       |
| `n` / `p`                             | next / previous page (100 items)                         |
| `Space`                               | mark the item (compare, bulk remove, add or delete)      |
| `t`                                   | toggle the type or the sub-list of the screen            |
| `r`                                   | refresh                                                  |
| `Esc`                                 | cancel a loading request, clear the marks, or go back    |

* **Search** is server-side on Pulp lists, and local on pulp-tui screens such as Content Views. A
  list that the API cannot filter (Versions, Publications) says `this list has no search`.
* **Palette:** go to a screen, connect to a server, or dismiss finished tasks. It opens in filter
  mode (`-- FILTER --`): type to filter, `Ctrl-J/K` or `Ctrl-N/P` move. `Esc` goes to move mode
  (`j k g G`), `/` filters again, a second `Esc` closes.
* **Bulk delete:** on Repositories, Remotes and Distributions, **d** with marked items deletes all
  the marked items. One confirm lists the impact of each item. Type `delete N <what>`, for example
  `delete 3 repositories`. A mark goes away only when the delete task of its item completed. An
  item whose delete fails, or whose task fails or is canceled, stays marked.
* **Object roles (B):** on repositories, remotes, alternate sources, publications,
  distributions, content guards, groups, server tasks, task schedules, uploads and upstream
  Pulps. The form shows who holds which role on the object, and your permissions on it. Then
  **add** or **remove** one role (from the role list) for the users and groups that you pick.
  An owner or admin role, and a removal from your own account, ask first. Pulp refuses a role
  without a permission for the object, a role that a user already has, and the removal of a
  role that a user does not have. On a type without object roles (an x509 or RHSM content
  guard), pulp-tui says that the server does not support it. In read-only mode **B** shows the
  roles and your permissions only, with no add or remove.
* Lists keep 5 rows of margin above and below the selected row.

**Forms** open in move mode:

* `j k`, `Tab` or `↑ ↓` change the field. `h l`, `← →` or `Space` change a choice. `Space`
  toggles a checkbox.
* `i` or `Enter` edits a text field (`-- EDIT --`). `Enter` then goes to the next field, or
  submits on the last one. `Esc` stops editing.
* `Ctrl-S` submits from both modes, also from an open picker. `Esc` in move mode cancels.
* A form with one text, secret or number field opens in edit mode.
* Text editing (also in search): `← →`/`Ctrl-B Ctrl-F` one character, `Alt-B Alt-F`/`Ctrl-← Ctrl-→`
  one word, `Home End`/`Ctrl-A Ctrl-E` start or end. `Backspace`/`Delete` delete one character,
  `Ctrl-W` the word before the cursor, `Ctrl-U`/`Ctrl-K` all text before/after the cursor.

**Pickers** (a field shown as `[ value ▾ ]`) pick an existing object from the server:

* `Enter` or `Space` opens the list in move mode: `j k`/`↑ ↓` one row, `g G` first or last row,
  `Ctrl-D Ctrl-U`/`PgDn PgUp` 10 rows.
* A letter other than `j` or `k` on the closed field opens the list in filter mode with that
  letter. `/` also starts filter mode, where `Ctrl-J/K`, `Ctrl-N/P` and the arrows move. `Esc`
  returns to move mode, and `Esc` in move mode closes the list and keeps the old value.
* `Enter` selects and goes to the next field. It does not submit.
* In a multi-select list, `Space` marks items. `Enter` takes the marked items, or the selected
  item when none is marked.
* `Backspace` on the closed field clears it.
* When the list cannot load, the popup shows the error, and `Enter` takes the typed text.

**Confirmations:** `y`/`Enter` confirms, `n`/`Esc` cancels. `j k g G Ctrl-D Ctrl-U ↑ ↓ PgUp PgDn
Home End` scroll a long preview. When you must type a name, letters type, so only `Ctrl-D`/
`Ctrl-U`, `PgUp`/`PgDn`, `Home`/`End` and the arrows scroll.

## Screens

### Dashboard

Server health (database, Redis, workers, content apps, storage), the current user, plugins with
versions, capabilities, and counts of repositories, distributions, content, tasks, Content Views
and environments. It says if the Content Views are in the local file or in a shared database, and
shows the shared database scope (see "What is shared").

A count shows `...` until its request returns. `r` refreshes health, versions and counts. The
capability marks change only with a new Connect. The Dashboard also refreshes when a tracked task
ends.

### Servers

The profiles of the config file. See "Server profiles" and "Content View database".

* **o** connect. **T** test the connection (user, content origin, plugin versions).
* **c** / **e** create / edit. An edit of the connected profile applies **Read-only** and
  **Production** at once; the other fields apply at the next Connect.
* **d** removes the profile from the config. It does not change Pulp or the Content View
  database, and the dialog names the database that keeps the definitions. The profile of the
  session cannot be removed: connect to another profile first.
* **K** stores a secret in the system keyring. When the profile uses a database (its own or the
  top-level `[database]`), it asks if the secret is for the "Pulp server" or the "database".
* **E** / **I** export / import Content View definitions, as `export-cv` and `import-cv`. **I**
  writes to the database of the profile, so with a shared database a read-only profile cannot
  import. **E** reads the current repository names from the server of the selected profile. **I**
  imports into the connected server: it finds the repositories there by name.
* **X** exports the repository and remote definitions of the selected profile's server, as
  `export-repos`. A text view shows the `note:` lines.
* **J** imports such a file into the connected server, as `import-repos`. It first shows the plan
  (`create N, keep M existing`), then creates the objects after you confirm.

The form has three database fields: **Database**, **Database secret** and **Database CA**.
**Database** shows what the profile uses now. Type `local` (or the local file path) for the local
file, a path such as `/srv/cv.db` for SQLite (saved as `sqlite:///srv/cv.db`), or a `postgres://`
or `sqlite://` URL. Never put a password in the URL. An unchanged field adds no database block.

**Connect** opens the Content View database in the background. A database without a current
schema asks `Migrate database of <profile>?` (see "Migrate"). A save with a new database asks the
same. A changed database location asks `Database of <profile> changes` (see "Change the database
of a profile").

### Repositories (RPM, File, Deb)

Columns: name, latest version, default remote, retention, description. The Deb screen needs
pulp_deb. The list hides Content View snapshot repositories; `t` shows them (the title then shows
`snapshots shown`).

* **v** versions. **o** content of the latest version. **H** sync history (tasks that touched the
  repository). **i** full JSON.
* **f** sync. Choose the remote and the policy: RPM `additive`, `mirror_complete`,
  `mirror_content_only`; File and Deb `additive`, `mirror`. The Remote field shows the default
  remote; untouched or empty, the sync uses the default remote by its href, of any type (also a
  ULN or Git remote). Edit sends the remote only when you change it. A mirror policy
  shows a warning first. With **Retain versions** 1, Pulp deletes the current version after the
  sync, unless a distribution serves it: then the warning says so and you type the repository
  name (on a Content View sync, the names of those repositories). Options:
  * **Optimize** (default on): turn it off for a full re-sync.
  * **Skip srpm**, **Skip treeinfo** (RPM): do not download source packages or `.treeinfo`.
  * The form refuses what Pulp refuses: skip types with `mirror_complete`, `mirror_complete` with
    autopublish on, and a mirror policy with **retain package versions** above 0.
* **u** uploads a local file: an RPM package, File content with its relative path, or a `.deb`
  package. Large files go in chunks with progress. pulp-tui verifies the server digest and
  deletes a failed chunked upload. The message names the new content and the new version.
  * **Overwrite** (default on, as Pulp): the new content replaces other content of the repository
    with the same key (File: the relative path; RPM: name, version, release, architecture and
    location). Off: the task fails instead, and the repository does not change.
  * Deb: **Distribution** and **Component** name the release that lists the package (empty =
    `pulp` and `upload`, the pulp_deb defaults). Pulp adds the release records to the new
    version and sets the `pool/` path. A structured publication serves the package under
    `dists/<distribution>/<component>/`.
* **U** (RPM) uploads a comps.xml. Its package groups, categories, environments and langpacks go
  into a new version. **Replace** first removes the comps units of the repository.
* **A** (RPM) uploads one advisory: a JSON file with the fields of an updateinfo record. `id`,
  `issued` and `updated` are required, and each package needs `sum_type` (for example `sha256`).
  Example: `tests/fixtures/advisory.json`.
* **M** (RPM) makes module content in the repository (a new version). Pick the kind (`modulemd`,
  `modulemd_defaults` or `modulemd_obsoletes`; only those that the server takes), then fill its
  required fields. A field marked `(JSON)` takes JSON, for example `["x-0:1.0-1.noarch"]`; a field
  with `empty = ...` can stay empty and goes as that value (`null`, `[]` or `{}`). **snippet** is the YAML document that a publication
  writes into `modules.yaml` unchanged, on one line: `--- {document: modulemd, version: 2, ...}`. A
  modulemd made here lists no packages.
* **P** publishes the latest version. It always makes a new publication, with the repository
  settings of that moment.
* **D** distributes: pin the latest version (publication), or follow the repository latest. Pulp
  cannot pin a checkpoint distribution: **D** refuses it before any change (see Distributions).
* **P** and **D** on Deb ask for an optional signing service (empty = the keys of the repository).
  **P** also asks for the kind: `apt`, or `verbatim` (see Publications).
  **D** reuses a publication only when its signing service is the same. On RPM, **D** never
  reuses a publication when the repository has a metadata signing service: Pulp signs a new one.
* **C** copies content from another repository, with the filter rules of Content Views (see
  "Filters"). RPM can copy dependencies. With filter rules, RPM takes what a Content View filter
  takes: the chosen packages, each advisory whose packages are all chosen, and the comps, modules
  and other metadata as they are (added after the packages; the confirm counts them). Advisories
  of the dependencies are not added. A Deb copy is structured: it also copies the Release,
  component and architecture records, and does not solve dependencies. A large selection goes in
  batches, as for **x** and **a** on Content.
* **R** reclaims disk space. Pulp deletes the stored files of synced content that no other
  repository has. The content stays in every version. Pulp downloads a file again from the remote
  when a client asks, so the upstream must still serve it. Uploaded content keeps its files. A
  form asks which versions keep their files (`3,4`; empty = none). The confirm shows the default
  remote and its download policy. Type the repository name to start.
* **O** (RPM) prunes old packages of the marked repositories (else of the selected one). A package
  goes when the repository has a newer version of it (same name and architecture) and it came into
  the repository more than **Keep days** ago (default 14; 0 keeps only the newest). pulp-tui runs
  Pulp's dry run first; the confirm shows how many packages each repository loses (Pulp names them
  only in its debug log). Type the repository name (several: `prune 2 repositories`). Each
  repository gets a new latest version without them; older versions keep them. A dry run that
  finds nothing ends with a notice. Content View snapshot repositories are refused. Some pulp_rpm
  versions (for example 3.34.0 with pulpcore 3.95.3; fixed in 3.35.0, 3.32.15, 3.29.13 and
  3.27.14) make the new version but keep the packages in it; the result then says that the new
  version still has them. Upgrade pulp_rpm to fix it.
* **c** / **e** / **d** create / edit / delete. Delete names each distribution that serves the
  repository, a version or a publication of it.

All types have **Retain checkpoints**: the number of checkpoint publications that Pulp keeps (empty
= all).

RPM fields:

* **Checksum type** of the published metadata and packages (`sha256` or stronger).
* **Metadata compression**: `zstd`, `gz` or `none`.
* **Package layout**: `nested_alphabetically` (`Packages/<letter>/`), `flat` or `nested_by_digest`.
* **Repo config (JSON)** for the generated `.repo` file, for example `{"gpgcheck": 1}`.
* **OSV config (JSON)** (pulp_rpm 3.39): the ecosystems of a vulnerability scan (**V** on
  Versions), for example `[{"name": "Rocky Linux", "releases": ["9"]}]`. Empty removes it. Pulp
  keeps it in the label `osv.rpm.config`, which **Labels** does not show: a label edit keeps it.
* **Metadata signing service**: signs `repomd.xml` of each publication.
* **Package signing service**: signs the packages that an upload or a modify adds (not a sync or a
  copy). **Package signing fingerprint**: `v4:<hex fingerprint>` or `keyid:<16 hex>`.

A Content View snapshot copies these fields from its source, except the package signing fields.

Deb fields:

* **Signing service (Release)**: signs the `Release` file of a publication without its own signing
  service.
* **Per-release signing services**: `release=signing service href,...`, for example
  `bookworm=/pulp/api/v3/signing-services/<id>/`.
* **Package signing service**: signs the `.deb` packages that an upload or a modify adds (not in
  pulp_deb 3.8 and older). **Package signing fingerprint**: `v4:<hex>` or `keyid:<16 hex>`.
* **Per-release package fingerprints**: `release=fingerprint,...`.
* **Publish upstream Release fields** (default on): a publication uses the version, origin, label
  and description of the upstream `Release`.
* **Excluded package metadata fields**: `field,...`, for example `Phased-Update-Percentage`. A
  publication leaves them out of the `Packages` indices.

On edit, a release that you remove from a per-release list loses its override. A `Release` file is
signed by the service of the publication, else the per-release service, else the service of the
repository. A Deb distribution serves at `<content URL>/dists/<distribution>/...`.

### Remotes

`t` toggles RPM, RPM ULN, File, File Git, Deb. **c** / **e** / **d** create / edit / delete,
**i** full JSON. The form shows only the fields of the server's schema for the type. Delete
lists the repositories that use the remote. Pulp does not delete a remote that an alternate
content source (ACS) uses, so pulp-tui refuses it (also in a bulk delete) and names the ACS:
delete them or change their remote first. An ACS takes only RPM, File and Deb remotes, never a
ULN or Git remote.

The Remote field of a repository and of a sync lists the remotes of both types of the plugin;
the note `ULN` or `Git` marks the second type.

* **URL**: `http://`, `https://`, or `file://` under the server's `ALLOWED_IMPORT_PATHS`. File: the
  URL of `PULP_MANIFEST`. Deb: the archive base URL (the directory with `dists/`).
* Download policy, TLS validation, concurrency, rate limit, labels.
* Secret fields: CA, client certificate and key (local file paths; pulp-tui uploads the content),
  username, password, SLES token (RPM), **Headers** (`Name: value; Name: value`, for example
  `Authorization: Bearer <token>`).
* **Proxy**: `http://` or `https://` with host and port, for example `http://proxy.corp:3128`.
* **Total / connect / socket connect / socket read timeout** (whole seconds), **Max retries**
  after a download failure (server default 3).

RPM ULN fields (Oracle Unbreakable Linux Network; the screen says when the server has no
`remotes/rpm/uln/`):

* **URL**: `uln://` and the channel label, for example `uln://ol8_x86_64_baseos_latest`.
* **Username**, **Password** (required on create): the ULN account. Pulp never returns them;
  on edit, empty keeps them.
* **ULN server base URL**: empty = the server setting `DEFAULT_ULN_SERVER_BASE_URL`.

File Git fields (a File repository syncs the files of a Git commit; pulpcore 3.123 has it, 3.95
not):

* **URL**: the Git repository, `http://`, `https://`, `git://`, `ssh://` or `file://`. Pulp
  uses **Username** and **Password** only with `http://` and `https://`.
* **Git ref**: branch, tag or commit; empty = `HEAD`. No download policy (always immediate).

Deb fields:

* **Distributions** (required): the releases to mirror, for example `bookworm bookworm-updates`.
* **Components**, **Architectures**: space-separated; empty = all.
* **Sync source packages / udebs / installer files**: default off.
* **Ignore missing package indices**: the sync continues when an upstream `Packages` index is
  missing.
* **GPG public key**: a local file. Pulp checks the `Release` signature with it.
* **Excluded package metadata fields**: `field,...`. The sync drops these custom fields.

### Alternate Sources

`t` toggles RPM/File/Deb. An alternate content source (ACS) is another place where Pulp gets
content files, for example a near mirror. A sync of any repository then gets the files that the
ACS has from the ACS. Columns: name, paths, last refreshed, remote.

* **R** refresh: one task per path in a task group (one row in the Task Center). Refresh again
  when the source changes.
* **c** / **e** / **d** create / edit / delete, **i** full JSON.
* The remote must have the download policy `on_demand`. Create it in Remotes first.
* **Paths** (RPM and File): `path,...` that Pulp adds to the remote URL; empty = the remote URL.
  The Deb form has no paths, because pulp_deb ignores them.
* An edit that removes a path first lists the other sources that lose it: Pulp removes that path
  from every ACS that has it.
* Delete also removes the hidden repositories of the refreshes, except on Deb (see
  COMPATIBILITY.md).

### Repository Versions

From a repository (**v**), or for all repositories. Columns: number, created, present, added and
removed counts.

* **o** content. **i** full JSON.
* **P** publishes this version (always a new publication). **D** distributes it. The form offers
  the repository name as the distribution name, and as the base path with every character other
  than letters, digits, `-`, `_` and `.` changed to `-` (the same on Repositories and
  Publications). On Deb, both ask
  for the optional signing service; **P** also asks for the kind (`apt` or `verbatim`, see
  Publications).
* **b** points a distribution to this version (rollback, with a preview). It makes no version, so
  the distributions that follow the repository and the next sync still use the newest version. On
  Deb, the new publication keeps the signing service of the one the distribution serves now, and
  stays verbatim when that one is verbatim (**D** too, unless you pick a signing service). The
  preview says if it reuses a publication of the version or makes a new one. On RPM, a repository
  with a metadata signing service always gets a new publication, signed with that service: an
  older one can be unsigned. A checkpoint distribution is refused before any change (see
  Distributions). When the version is of another repository than the one the distribution
  serves now, the preview has a `WARNING:` line with both repository names (also for **b** on
  Distributions).
* **m** compares with the marked version: mark one with `Space`, then press `m` on the other. It
  lists the added and removed units (older → newer) of each content type, for example
  `+ advisory RHSA-2024:0001`. A unit with the same key but other content (a rebuilt package
  with the same NEVRA, an updated advisory) is a `~` line, counted as changed. A `warning:` line
  shows when Pulp counts other numbers. A
  `not compared (not on this server):` line names the content types that the server cannot list.
* **R** restores as latest: a new latest version with the content of this version. The preview
  shows what changes, as compare does (also `~` lines for the same key with other content), and
  what serves the new version (with autopublish, Pulp publishes it; else
  use **P**). Retention can then delete the oldest versions. On Deb, pulp_deb removes the Release
  files and indices from the new version; the next publish writes new ones. Refused for the latest
  version. When the content is the same, Pulp makes no version.
* **F** repairs: Pulp downloads missing or corrupt files again from a remote, and deletes a
  corrupt file that no remote has. **Verify checksums** (default on) reads every file, which is
  slow; off only checks that each file exists. Refused for a version without content, because
  Pulp would then repair the whole server. The notice gives the counts
  (`0 missing, 0 corrupt, 0 repaired`).
* **V** (RPM, pulp_rpm 3.39) scans the version for known vulnerabilities. Pulp asks osv.dev about
  each package, for the ecosystems in the repository's `osv_config`, for example
  `[{"name": "Rocky Linux", "releases": ["9"]}]`. Pulp refuses a repository without it: set
  **OSV config** in the repository form. The Pulp server needs access to `api.osv.dev`. The result shows the counts of the task and each package with vulnerabilities
  (`bash-5.1.8-6.el9.x86_64: RLSA-2024:1, CVE-2024-1`).
* **d** deletes the version. It names the distributions that serve the version or its
  publications, and you type the version id. Rollback never needs a delete.
* **O** prunes orphans on the whole server: content that no version has, and files that no content
  uses. Give the minimum orphan age in minutes (default 1440). The confirm counts with the same
  age, so it shows what Pulp deletes. Type the profile name. `0` also deletes the content of an
  upload that still runs. A sync downloads deleted content again.

### Content browser

Opened from a repository or a version. `t` cycles the content types of the repository type (RPM:
packages, advisories, modulemds, package groups, distribution trees and more; Deb: packages,
releases, components, indices and more). `/` searches package names or file paths.

* **i** shows all metadata of the unit (requires, provides, files, ...).
* **x** removes the marked (or selected) units: a new version. The screen then shows the new
  version. The marks stay when the task fails. Older versions keep the units, but with
  **Retain versions** 1 Pulp deletes the current version, unless a distribution serves it. Then
  the confirm says so and you type the repository name: the units stay only in served versions,
  else orphan cleanup can delete them.
* **a** adds them to another repository: a new version. On Deb, it is a structured copy, only on
  the packages list.
* **x** and **a** send a large selection (above about 27,000 units) in batches, one task after
  the other; each batch makes a new version. The Task Center shows the task of each batch. When
  a batch fails, the batches after it are not sent. The confirm of **x** says how many versions
  it makes (on Deb, the release records of the packages count too). With N versions, Pulp deletes
  the current version when **Retain versions** is N or less, unless a distribution serves it.

### Publications

`t` toggles RPM/File/Deb. **D** distributes exactly this publication. **i** full JSON.

* Deb lists the APT publications and the verbatim ones, newest first. The *kind* column shows
  `apt` or `verbatim`. A verbatim publication serves the files of its version as they are: the
  synced upstream `Release` files (with their signatures) and indices. pulp_deb drops these files
  from a version made by upload, copy or modify, so only a synced version gives a usable verbatim
  publication.
* **c** creates a publication (always a new one): a repository, an optional version number, and
  for Deb the **Kind** (`apt` or `verbatim`, when the server has verbatim publications) and an
  optional signing service. It also has the publication options of the server. Empty means the
  repository setting or the server default. A verbatim publication takes no signing service and
  no options. (**P** in Repositories and Versions asks only for the kind and the signing
  service.)
  * **Checksum type**, **Metadata compression**, **Repo config** (RPM): as the repository fields.
  * **Package layout** (RPM, Deb).
  * **Simple** / **Structured** (Deb): one component, or the upstream components (default). One of
    them must be `true`.
  * **Legacy Release files** (Deb): also the `Release` file of each component and architecture.
  * **Publish upstream Release fields**, **Excluded package metadata fields** (Deb).
  * **Checkpoint**: a checkpoint publication. Pulp then publishes the latest version, so the form
    refuses a version number.
* **d** deletes. It names the distributions that serve the publication (an APT or a verbatim one)
  and what each one serves after, and you type its id.

### Distributions

`t` toggles RPM/File/Deb. Details show **PINNED** (a publication or a version), **FOLLOWS
LATEST** (a repository) or **CHECKPOINTS** (a checkpoint distribution of a repository). The
*binding* column shows `pinned`, `latest` or `checkpoint`. **i** full JSON.

* **y** copies the content URL (OSC 52 terminal clipboard). In tmux, set
  `set -g allow-passthrough on` or `set -s set-clipboard on`.
* **V** verifies what the distribution serves. One that follows a repository serves its newest
  publication, which is not the latest version when that version has no publication. A
  checkpoint distribution serves the newest checkpoint publication of its repository only at
  `<base path>/<timestamp>/` (for example `20260102T030405Z`); Verify reads it there. Deb: it
  fetches `Release`, a `Packages` index and the smallest `.deb`, and compares the digest. A
  difference reads `MISMATCH: served content differs: …`.
* **b** re-points to a version, with a preview. It first reads that version: a number that the
  repository does not have is refused, and the message gives the latest number. On Deb, it keeps the signing service of the
  publication that it serves now, or a verbatim publication stays verbatim. On RPM, it makes a new signed publication when the repository
  has a metadata signing service. Pulp cannot pin a checkpoint distribution, so the preview
  refuses it before any change and says what it serves now. To pin it, edit the distribution,
  clear **Checkpoint**, then re-point it.
* **c** / **e** / **d** create / edit / delete. Fields: name, base path, what it serves, content
  guard, labels (Deb: the signing service of a pinned publication). Flags:
  * **Hidden**: the content app does not list it. Its URL still serves.
  * **Checkpoint**: serves the checkpoint publications at `<base path>/<timestamp>/`. Needs
    "follow repository latest".
  * **Serve config.repo** (RPM): Pulp serves a `.repo` file.
* A Content View distribution: you can edit its base path and its other labels. You cannot
  re-point, re-bind or rename it, or change its `pulp_tui_*` labels. Use promote or roll back.
  You cannot delete it, also not as a marked item. Unpromote the view from the environment.

### Content Guards

A content guard decides who may download from the distributions that use it. The list shows
every guard of the server: name, type (`core/rbac`, `core/header`, `certguard/x509`, …) and
the distributions that use it. **i** full JSON.

* **c** asks for the type (only the types that the server has), then for its fields:
  * `core/header`: header name, header value, optional jq filter. The value is a shared
    secret: pulp-tui never shows it (Inspect, Details and Logs show `•••`).
  * `core/envvar_header`: header name and the name of an environment variable on the Pulp
    server.
  * `core/composite`: the guards of the composite. One guard that allows is enough. A
    composite without guards allows everyone.
  * `certguard/x509`, `certguard/rhsm`: the path of a local PEM file with the CA certificate.
    pulp-tui sends its content.
  * `core/rbac`, `core/content_redirect`: name and description only.
* **e** edits a guard. The header value and the CA certificate are not shown: empty keeps
  them.
* **a** (RBAC guard): download access. The form shows the users and groups that may download
  now, then allows or removes the users and groups that you pick (the role
  `core.rbaccontentguard_downloader`). The user that created the guard is in the list too.
* **d** deletes after you type the name. The impact lists the distributions that use the
  guard: Pulp removes the guard from them, so they become **public**. It also lists the
  composite guards that contain it; a composite that loses its only guard allows everyone.

### Content Views

Managed by pulp-tui, not by Pulp (see [ARCHITECTURE.md](ARCHITECTURE.md)). Columns: composite,
repositories, latest version, environments with bound versions and states.

* **a** adds repositories: a multi-select picker of RPM, File and Deb repositories, and filter
  rules. The rules apply to each selected repository, so a mix of types needs empty rules.
* **F** edits the filter rules of one repository. **x** removes a repository; published versions
  do not change.
* **f** syncs the repositories of the view. Pick the repositories (all marked) and one policy (the
  RPM choices when the view has an RPM repository; File and Deb take any mirror policy as
  `mirror`). Each repository syncs from its default remote; one without is skipped. The result
  lists each repository: new version, no change, failed or skipped. Sync does not publish.
* **w** preview: the content that the draft selects, compared with the latest version.
* **P** publishes a new version: notes (and Deb signing) → preview → confirm. The version becomes
  active in Library. When the Library step fails, the version stays published:
  `Library step failed: …; promote v4 to Library`.
* **P** refuses a view that holds a snapshot repository. Remove it (**x**) and add its source. To
  use the content of another view, make a composite.
* **v** versions. **i** full JSON.
* **M** promotes a version to an environment, with a preview of every distribution.
* **b** rolls an environment back. After Library: only a version older than the active one, or the
  active one again (a retry). For a newer version, or an environment without a version, use
  Promote. In Library: any published version.
* **u** unpromotes: it deletes the distributions of the view in the environment (the dialog lists
  the paths; type the view name) and removes the binding, so you can delete the version. Versions
  and snapshots stay. Refused while a later environment on the path has the view: go back along
  the path (Prod, then Test, then Dev).
* **D** verifies all environments (drift check). For each environment with drift, it tells how to
  reconcile: promote the bound version again, or, when the prior environment no longer has that
  version active (Promote refuses it), roll back to it (**b**).
* **U** breaks the lock of the view. It shows the holder; type the view name. See "Locks".
* **c** creates. **e** edits the description (composite: also auto publish and the pins).
* **d** deletes the distributions and snapshot repositories of the view, never the source
  repositories. The dialog lists every served path. A composite loses only its distributions.
* For **d** and **u**, also type a served protected environment after the view name, for example
  `web Prod`.

**Served paths.** A promoted repository serves at
`https://<pulp>/pulp/content/<label>/<view>/<repo>/` (`<label>`: see "Lifecycle Environments"). A
promotion keeps the path of an existing distribution, so you can change a base path in
Distributions. A relabel moves all paths of the environment to `<label>/<view>/<repo>`, also the
changed ones.

#### Filters

Rules separated by `;`:

```
include name=kernel* arch=x86_64 evr>=5.14-1
exclude name=*-debuginfo
include name=nginx* arch=amd64 version>=1.22    # Deb repositories
exclude name=*-dbg version<1.0                  # Deb repositories
exclude path=isos/*            # File repositories
```

* A unit is selected when (no include rule exists or one matches) and no exclude rule matches.
  All criteria of one rule must match.
* RPM: `name=`, `arch=`, `evr`. Deb: `name=`, `arch=`, `version`. File: `path=`. A criterion of
  another type is refused.
* `evr` and `version` take `= < <= > >=` with RPM or Debian version comparison. `epoch:` is
  optional. Without a release (RPM) or revision (Deb), the rule compares only epoch and version.
* The value must start with a letter or digit, so a typo such as `evr=>5.14` is refused.

#### Deb repositories in Content Views

A view can hold Deb repositories, also with RPM and File repositories (then without filter rules).
Publish uses a structured copy (the server needs the Deb copy API), so apt finds the same
distributions and components. A filtered snapshot has only the selected binary packages, and the
publication makes new indices.

The Publish form has one **Deb signing** choice for all Deb repositories of the view:

* `same as source` (default): the snapshot gets the signing service and the per-release keys of
  its source.
* `none`: no keys. Clients get an unsigned `Release`.
* `service`: the snapshot gets the picked **Signing service** and no per-release keys.

pulp-tui changes only the snapshot repository, never the source. The version stores the choice,
so *Resume* signs the same way. When Pulp refuses the service, delete the version and publish again
with another choice. Each Deb repository gets a new publication in each version. A composite
publish does not ask.

**Large selections** (above about 35,000 packages) go in batches (see COMPATIBILITY.md). The
progress shows each step, for example `main: copying 25316 units (2/3)`. When a publish stops
between batches, *Resume* continues it.

#### Composite Content Views

A composite view joins other Content Views (its **components**). Example: `rocky9` has
`rocky9-os` pinned to a tested `v1`, and `rocky9-extras` on `latest`.

* **Create**: `c` with *Composite (fixed after create)*. A composite has no repositories or
  filters of its own, and cannot be a component (no nesting). `F`, `f` and the repository forms of
  `a` and `x` are not available.
* **Components**: `a` picks normal views and asks for the pin of each: `latest` or one published
  version (`v3`). `x` removes one. `e` changes the pins and **auto publish**. Changes take effect
  at the next publish.
* **Repository rule**: two components must not hold the same repository, and two repositories must
  not give the same path segment (`os/x` and `os x` both give `os-x`). pulp-tui checks this on add
  and at each publish.
* **Publish** uses the newest published version for `latest`, and copies nothing in Pulp: the
  composite version points at the snapshots and publications of the components. It is refused,
  before it writes anything, with no components, a `latest` component without a published
  version, a pinned version that is not published, or a repository clash. The preview lists each
  component version (`rocky9-extras: v1 -> v2`), then the content changes.
* **Served paths**: the composite has its own distributions at `<label>/<composite>/<repo>`.
  Promote, roll back and drift check work as for a normal view.
* **Auto publish** (default off): when a component on `latest` publishes, the composite publishes
  too, under its own lock. The notice gives the result. A failed composite gives a warning; the
  component stays published. Auto publish never promotes, and publishes nothing while Library is
  protected.
* **Delete**: you cannot delete a component view, or a component version that a composite version
  uses or a composite pins. The error names the users. A composite delete removes only its own
  records and distributions.

### Content View versions

* **M** promote. **b** roll back. **m** compare with the marked version (composites: component
  versions first, then the content changes).
* **R** resumes a failed or interrupted publish. It continues from the checkpoints and attaches to
  a submitted task, also for a publish that another operator started. The version becomes active
  in Library only when no newer version is published. For a published version with an
  `interrupted` Library step, it runs the Library step and auto publish again.
* **W** watches the task of the copy step or the publication step. Other steps show their tasks
  only in the session that runs the publish.
* **d** deletes. Refused while the version is bound or a composite uses it. The confirm lists each
  publication and snapshot version that it deletes, and each one that it keeps (still served by an
  environment) with the reason.
* **i** details: source versions, snapshot versions, publications, unit counts, component
  versions.

A `publishing` version, or a `promoting` binding, shows as `interrupted` when nobody holds a live
lock on the view (the client quit, crashed or ran Connect). Press **R** for a version. Promote or
roll back again for a binding.

### Lifecycle Environments

Library exists by default. With a shared database, all operators see the same environments. The
*bound views* column shows a binding that is not active, for example `web:v2 (partial)`.

* **c** creates an environment after an existing one. Paths can branch.
* **L** toggles protection. To change what a protected environment serves, you type its name
  (promote, roll back; for Library also publish and *Resume*). To remove protection, you also
  type its name (with a shared database, it applies to all operators). Adding it asks nothing.
* **d** deletes an unused environment. First it shows why it is refused (bound views, later
  environments), or the served rows that pulp-tui forgets (their distributions stay in Pulp).
  Then you type the environment name.
* **e** edits the **label**, the part of the served path that names the environment. A new
  environment gets its name in lower case (`Dev` gives `dev`). A label has 1-64 characters of
  `a-z 0-9 - _ .` and is unique on the server. A label must not be equal to, a parent of, or a
  child of the base path of another distribution.
* The relabel shows the paths that move (`/old -> /new`), and you type the environment name. Then
  it moves each distribution in place (one task each), and shows the result and the drift check of
  each bound view. It locks every view bound in the environment. A row that fails stays at its old
  path: run **e** again with the same label.

### Replication

Native Upstream Pulp objects (tech preview).

* **R** replicates now and shows the task group result.
* **H** shows the 5 newest replication task groups of this upstream.
* **c** / **e** / **d** create / edit / delete, **i** full JSON. Fields:
  * upstream base URL (as *this* Pulp server reaches it), API root (`pulp/`), domain,
    credentials, CA and client certificates, TLS validation;
  * distribution filter (`q_select`, for example `name__startswith="rhel9"`);
  * deletion policy: `nodelete` (default), `labeled` or `all`; every policy but `nodelete`
    may delete local objects and needs a typed confirmation, also on Replicate;
  * **Replica download policy** (`immediate`, `on_demand` or `streamed`), concurrency, max
    retries, timeouts. Pulp copies them to the replica remotes.
* **F** fallback: sync from another profile. Choose types (RPM and File, RPM, File, or Deb), a name
  filter, and additive or mirror mode. A dry run lists `CREATE` / `SYNC` / `SKIP` per distribution
  before anything changes. You can run it again safely.
  * Both servers need the plugin of each type.
  * A File or Deb distribution without a readable publication is a `SKIP` row with the reason;
    so is a File one without a manifest.
  * A Deb replica syncs the releases (`dists/`) that the upstream publication serves and publishes
    them in the same mode: structured, simple or verbatim. It is signed only when its repository
    has a signing service; the dry run row says when the upstream is signed and the replica will
    not be. Set the service before the first run: a rerun reuses the publication of an unchanged
    version.
  * It does not follow an upstream base path change: the row shows a 409 conflict. Delete or move
    the replica distribution by hand.

### Exporters

Disconnected export with the exporters of pulpcore. Pulp writes the files to a directory on its
own disk, which must be in the server's `ALLOWED_EXPORT_PATHS` (empty by default). `t` toggles
Pulp and filesystem exporters. **i** full JSON.

Pulp exporters:

* **c** / **e**: name, directory, and RPM, File and Deb repositories.
* **E** exports. The form shows each repository with its latest version and its version in the
  last export. The export is one file, or chunks of a maximum size (`1GB`). Default: a full export
  of the latest versions.
  * **Incremental**: only the content added since the last export. Import it on a server that has
    the earlier exports.
  * **Start versions** (`web=3,os=7`, incremental only): start from these versions. The form
    refuses an incremental export of a repository without a start version.
  * **Versions** (`web=3,os=7`): export these versions; the others get their latest version.
  * At the end, pulp-tui shows the TOC file (`<directory>/export-<id>-<time>-toc.json`) and the
    output files. Copy them to the disconnected server and import the TOC there.
  * There is no dry run: pulpcore 3.123.1 accepts `dry_run` but writes the export all the same.
* **o** lists the exports (time, repository versions, TOC). **d** on an export deletes its record.
* **d** on an exporter shows its number of exports; type its name. The export files stay on the
  server disk.

Filesystem exporters write the plain files of one publication or version:

* **c** / **e**: name, directory, method (`write`, `hardlink` or `symlink`).
* **E**: a repository, a version (empty = latest), and the publication (the newest of that
  version) or the version. **Start version number** (optional, same repository): Pulp writes
  only the files that this version does not have (a publication: and all its metadata files),
  for example the changes since the version of the previous export.
* The directory must not exist. Change it before the next export.

### Importers

Disconnected import of a Pulp export. Copy the export files to a directory in the server's
`ALLOWED_IMPORT_PATHS` (empty by default). Pulp must be able to write there.

* **c** / **e**: name and mapping `source=destination,...` (an exported repository name, then a
  repository of this server). Each destination must exist; pulp-tui checks it first. An unmapped
  repository goes to the repository of the same name, or Pulp skips it.
* **I** imports: type the path of the `-toc.json` file on the server, or of the `.tar` of an
  export without chunks (for example one that came without its TOC). pulp-tui first runs the
  import check and shows its result. When the check fails, nothing is imported. Each imported
  repository gets a new version with exactly the exported content. When Pulp finds no destination
  repository, pulp-tui gives a warning.
* **o** lists the imports (time, TOC, task). **i** full JSON.
* **d** shows the number of imports; type the importer name. The imported versions stay.

### Users, Groups, Roles

* **Users**: **R** roles, **c** / **e** / **d**, **w** set password (local Pulp users only),
  **A** enable/disable. Delete refuses your own user and warns for staff. When the server does not
  name the user of the session (bearer or certificate auth without `login/`), the delete confirm
  says that it cannot tell whether this is your own user. A new password for your
  own user asks you to type the user name: with Basic auth, the session then gets 401 until you
  update the profile's secret and Connect again.
* **Groups**: **o** members (**c** add a user, **d** remove), **R** roles, **c** create, **e**
  rename (members and roles stay), **d** delete.
* Delete reads the impact first, then you type the name: for a user, its groups and its role
  assignments; for a group, its members and its role assignments; for a role, the users and groups
  that have it (pulp-tui reads the roles of each user and group, because the lists have no role
  filter).
* **Roles** of a user or group: **c** adds a role, an optional object, or an optional domain (when
  the server has domains; not both, Pulp refuses that). **d** removes. Owner and admin roles ask
  to confirm.
* **Roles** screen: system roles are read-only. **c** / **e** / **d** a custom role (permissions as
  a comma list).

### Domains

Only on a server with `DOMAIN_ENABLED` (status `domain_enabled: true`); else the screen says
that the server does not support it. Each domain has its own repositories, content, artifacts
and storage. The list shows name, storage class and description. Pulp does not edit, delete or
migrate the `default` domain; pulp-tui says so before it sends anything.

* **c** creates a domain: name (it becomes part of the URLs: `/pulp/<name>/api/v3/`),
  description, storage class (the classes of the server) and storage settings, a JSON object
  such as `{"location": "/var/lib/pulp/media"}` (FileSystem) or
  `{"bucket_name": "b", "access_key": "…", "secret_key": "…"}` (S3). The settings field is
  never shown, because it can hold keys. Two switches: redirect downloads to the object storage,
  and hide the distributions that have a content guard.
* **e** changes the description and the two switches. The storage changes only with **M**; a
  domain is not renamed (every URL of the domain would move).
* **d** counts what you may see in the domain (repositories, remotes, distributions, content
  units, artifacts; `?` when you may not list them), then you type the name. Pulp refuses while a
  repository version holds content, or while a repository, remote, distribution or other object
  is in the domain; the task then fails with Pulp's reason, and nothing is deleted. Else Pulp
  deletes the orphan content and the artifacts of the domain (with their files) and its task
  records.
* **M** migrates the storage: the new class and its settings, then you type the name. Pulp copies
  every artifact to the new storage and then uses it. Until the task ends, the other tasks of the
  domain wait. Pulp does not delete the files in the old storage.

### Access Policies

The access policy of each API viewset: viewset name (`repositories/rpm/rpm`, `tasks`, …),
customized (an operator changed it) and the number of statements. `/` searches the viewset name.
**i** shows the statements, creation hooks and queryset scoping.

pulp-tui does not edit a policy: a wrong statement can lock users out, and a form line is no
place for a JSON policy. Use the Pulp CLI or the API for that.

* **R** resets a customized policy to the default of its plugin. The confirmation shows the
  statements that are lost; you type the viewset name. Users may get or lose access at once.
  On a policy that is not customized, pulp-tui says so and sends nothing.

### Tasks

The Task Center shows tracked tasks: label (the operation that started it), state, progress (only
what Pulp reports), start time and error. The top box lists the running client operations. A
native replication or an ACS refresh is one row for its task group. `t` cycles: tracked tasks →
server tasks → task groups → workers.

* **W** live view of the task. **i** details with all progress reports.
* **X** cancels on the server. On a task group, it cancels the whole group; the confirm counts its
  unfinished tasks.
* **z** dismisses finished tasks, also from the local file: they do not come back after a restart
  or a Connect. Pulp keeps its task records. Above 500 tracked tasks, the oldest finished one goes
  by itself.
* At start and at Connect, the Task Center attaches every unfinished task again, and shows the 50
  newest finished ones.
* A canceled or skipped task gives a warning; a failed task gives an error.
* A task that the server does not have (404) shows `gone`. When polls get 401 (the credentials of
  the session no longer work, for example after you changed your own password), the tasks show
  `unauthorized`, polling stops, and one error tells you. Fix the profile's secret and Connect
  again: the tasks are attached again.

Server tasks:

* **a** tracks the task. **W** tracks it and opens the live view.
* **F** filters on the server: state, name contains, resource href (the tasks that locked an
  object), started at or after (`2026-11-01` or `2026-11-01T08:00:00Z`). The title shows the
  filters.
* **d** deletes the record of a finished task (what the task made stays). Cancel an unfinished
  task first.
* **P** purges finished tasks of the whole server: *Finished before* (`2026-10-01`) and states
  (`completed`, `failed`, `canceled`, `skipped`; default `completed`). pulp-tui counts the tasks
  first, then you type the profile name.

**Task groups** (newest first, no search): counts of tasks by state. **i** shows the tasks, **X**
cancels the group. **Workers** (read-only): name, last heartbeat, current task.

### Task Schedules

The tasks that Pulp starts by itself, read-only: name, task, interval (`1 00:00:00` is one
day), next dispatch and last task. `/` searches the name. **W** tracks the last task in the
Task Center and opens its live view. **i** full JSON.

### Uploads

Chunked uploads that are not committed, oldest first: id, created, size (in bytes, as the
client declared it) and completed. Pulp deletes an upload when it is committed. pulp-tui
deletes its own failed upload; the uploads of other clients, or of a pulp-tui that stopped,
stay until you delete them. **i** shows the chunks.

* **d** deletes the selected upload, or the marked ones (`Space` marks). It asks for a plain
  confirmation, not a typed name: an upload holds only the chunks that a client sent, and no
  content, artifact or repository changes. A client that still sends that upload fails and
  must start again. An upload whose delete fails stays marked.

### Artifacts

The files that Pulp stores, newest first: created, size (bytes) and sha256. Pulp's default
policy lets only an administrator list them. `/` finds one artifact by its full sha256 (the API
has no partial match). **i** full JSON, with the other digests and the storage path.

There is no delete: Pulp marks the artifact delete as deprecated, and its default policy refuses
it. *Prune orphan content* (**O** on Repository Versions) deletes the artifacts that no content
uses.

**F** repairs the files of all content of the server (of the profile's domain), also with an
empty list. *Verify checksums* is off by default: Pulp then checks only that each file exists;
on, it reads every file and compares its digest (slow on a big server). pulp-tui counts the
content, then you type the profile name. Pulp downloads a missing or corrupt file again from a
remote that has it, and deletes a corrupt file that no remote can give again. The Task Center
shows the task; at the end pulp-tui reports the missing, corrupt and repaired counts.

### Logs

All events of this session, newest first, with UTC times. It works without a connection.

* `request`: each HTTP request (method, path, status or error, duration).
* `action`: each action that you started, also a cancelled one.
* `notice`: each footer message with its tag.

`Enter` opens a row (a request shows the full bodies). `/` filters. `t` shows or hides task polling
(hidden by default).

The log never records the `Authorization` header. It shows as `•••` the JSON values of
`password`, `token`, `secret`, `client_key`, `authorization`, `headers`, `header_value`, `access_key`,
`secret_key`, `account_key`, `cloudfront_key`, and of keys that end in `_password`, `_token` or `_secret`,
in any letter case. An upload shows only its size. A body is
cut at 64 KiB. The log keeps 1000 rows in memory only and is gone when you quit.

## Server profiles

The config file is `--config`, `$PULP_TUI_CONFIG`, or `$XDG_CONFIG_HOME/pulp-tui/config.toml`.
`pulp-tui config-example` prints a full example. The profile is the first of: `--profile`,
`$PULP_TUI_PROFILE`, `default_profile`, the first profile.

Secrets are references:

| Reference                | Use                                                     |
| ------------------------ | ------------------------------------------------------- |
| `env:PULP_PASSWORD`      | environment variable                                    |
| `file:/run/secrets/pulp` | secret file (the last newline is removed)               |
| `keyring`                | system keyring, service `pulp-tui`, user = profile name |

* `auth`: `basic` (username and secret), `bearer` (the secret is the token), `none` (anonymous, or
  the identity of the mTLS client certificate).
* `ca_cert` (PEM bundle added to the system roots), `client_cert` and `client_key` (mTLS).
* `proxy`, `timeout_secs`, `read_only`, `production`.
* Never put credentials in `url`. `proxy` must not hold a password; set an authenticated proxy in
  `HTTPS_PROXY`.

### System keyring

pulp-tui stores secrets in the keyring of the operating system: Secret Service over D-Bus on Linux
and BSD (GNOME Keyring, KDE Wallet, KeePassXC), Keychain on macOS, Credential Manager on Windows.

| Secret                        | Keyring user      |
| ----------------------------- | ----------------- |
| Pulp server of profile `prod` | `prod`            |
| `[profiles.prod.database]`    | `[database]:prod` |
| top-level `[database]`        | `[database]`      |

* **K** on Servers only writes the keyring. Also set `secret = "keyring"` in the config. **K**
  again replaces the secret.
* **K** on a profile that uses the top-level `[database]` stores its secret (user `[database]`).
* On Linux the entry goes to the `default` collection (it can ask to unlock).
* Builds without the `keyring` feature (the container image) give an error for `keyring`. Use
  `env:` or `file:`.
* KDE Plasma 6: KWalletManager does not show these entries, but they are there (folder
  `Secret Service`). Use `secret-tool`.

```sh
secret-tool search --all service pulp-tui          # list entries (prints the secrets)
secret-tool store --label='keyring:[database]@pulp-tui' service pulp-tui username '[database]'
secret-tool clear service pulp-tui username prod   # remove the secret of profile prod
```

## Content View database

Content Views, environments and their history are in a database. By default this is the local
SQLite file `<data-dir>/pulp-tui.db`, for one operator. Several operators can share one database.
Each pulp-tui connects to it directly; press `r` to see the changes of another operator.

```toml
[database]                       # default for all profiles
url = "postgres://pulp_tui@db.example.com:5432/pulp_tui"   # or "sqlite:///srv/pulp-tui/cv.db"
secret = "env:PULP_TUI_DB_PASSWORD"                         # env:VAR, file:/path or keyring
ca_cert = "/etc/pki/tls/certs/internal-ca.pem"              # optional, PostgreSQL only

[profiles.prod.database]         # this profile only
url = "postgres://pulp_tui@db.example.com:5432/pulp_tui_prod"
secret = "keyring"               # keyring user [database]:prod
```

The database of a profile is the first of these (TUI and command line): its
`[profiles.<name>.database]` (`url = "local"` there means the local file), the top-level
`[database]`, the local file.

* A URL with a password is refused. Error messages show the URL without the password.
* PostgreSQL traffic is always encrypted and verified (`verify-full` replaces any `sslmode` of the
  URL), except over a Unix socket or to `localhost`, `127.0.0.1` or `::1`. The server certificate
  must list the host name or IP address of the URL.
* `ca_cert` is added to the roots built into pulp-tui. The system trust store is **not** used for
  the database, so an internal CA always needs `ca_cert`.
* When the database cannot open, the Pulp screens still work, and the Content View screens show
  `shared database unavailable: <reason>`. pulp-tui does not use the local file instead. Fix the
  cause and Connect again.
* Profiles with the same URL share one open database.

### Migrate

The local file migrates when it opens. An external database changes only with
`pulp-tui db migrate`, or with the question on Connect. Without a current schema, the Content View
screens show `database not migrated (...)`, and the Content View commands stop.

* **Connect** asks `Migrate database of <profile>?` with the schema change (for example
  `empty -> 5`). Enter migrates; Esc does not. A save with a new database asks the same.
* The role of the connection must be able to create tables.
* After a migration, all operators need this pulp-tui version or newer. An older one stops with
  `database schema N is newer than this pulp-tui`.
* Only the migration creates a SQLite file.
* A read-only profile gets a warning instead: run `pulp-tui db migrate --profile <name>`.

### Change the database of a profile

When you edit the database URL of a profile, pulp-tui counts the Content View data of this server
in the old database. With no data, it saves at once. Else it asks `Database of <profile> changes`:

* **copy and switch**: copy the data as `db copy` does, then save the profile;
* **switch without copy**: save only; the old data stays in the old database;
* **Esc**: change nothing.

A read-only profile, or an old database that cannot be read, gets only **switch without copy**. A
change of only the secret or the CA saves at once. The change takes effect at the next Connect.

### Set up a shared database (administrator)

1. Create the PostgreSQL database and a role for the operators.
2. Run `pulp-tui db migrate --profile <name>` with a role that can create tables. Do this again
   after each pulp-tui upgrade.
3. Give the operator role only `SELECT`, `INSERT`, `UPDATE` and `DELETE`. Grant again after a
   migration that adds tables, or use `ALTER DEFAULT PRIVILEGES`.

   ```sql
   GRANT USAGE ON SCHEMA public TO pulp_tui_op;
   GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO pulp_tui_op;
   ```

4. Each operator puts the same `[database]` (or the same profile `database`) in the config.

### Copy the data (`db copy`)

`pulp-tui --profile lab db copy` copies the Content View data from the local file into the
database of `lab`. `--from <profile>` reads the database of another profile instead (default:
`local`).

* It copies views (with repositories, filters, components, pins, auto publish), versions and
  checkpoints, environments, bindings and the audit log. It does not copy task references,
  replication history or locks.
* The rows get new IDs. The copy is one transaction. The source does not change.
* It never merges: when the target has a view, or an environment other than Library, with the same
  name for this server, it stops and writes nothing.
* It refuses when the source and the target are the same database, and when a source view has a
  live lock (renewed in the last 2 minutes).
* A read-only profile cannot run it. Run it once for each server.
* After the copy, the objects in Pulp get the database ID of the target at their next publish or
  promotion. The old source then cannot change them.

### What is shared

| In the shared database                                        | Always local           |
| ------------------------------------------------------------- | ---------------------- |
| views, filters, versions, checkpoints, environments, bindings | Task Center references |
| audit log (with the operator), locks                          | replication history    |

* **Server key.** Shared rows belong to a server, not to a profile name. The key is the URL, API
  root and domain, for example `https://pulp.example.com/pulp/#default`. **All operators must use
  the same scheme, host name, port, `api_root` and domain.** A short host name, an IP address or
  `http://` gives another key (a default port does not). The Dashboard shows
  `shared database scope: <key>`; compare it between operators.
* **Operator.** Locks and audit rows name `username@host`, for example `alice@ws12`. In a
  container, use `--hostname` so that the name stays the same.
* **Read-only profiles.** With a shared database, a read-only profile also cannot change Content
  View definitions (create, edit, add or remove repositories, filters and components) or
  environments, cannot run `import-cv` or `db copy`, and gets no migration or "copy and switch".
  With the local file these actions stay. The label edit on Lifecycle Environments moves
  distributions, so it is always blocked.

### Locks

* Publish, *Resume*, promote, roll back, unpromote and delete (a view or a version) lock **one
  Content View**. A second operator, or a second pulp-tui on the same file, gets
  `Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC` before anything goes
  to Pulp.
* A composite publish locks only the composite. Auto publish locks each composite after the
  component lock is released.
* A relabel locks **all** views bound in the environment. When another operator holds one, it stops
  before any change.
* Preview, compare, drift check and sync do not lock. Definition edits do not lock: the last write
  wins.
* The holder renews the lock every 30 s. A lock not renewed for 2 minutes expires, and the next
  operator takes it. After a crash, the lock stays until it expires.
* A workflow that loses its lock stops before its next change: `lock on web lost: <reason>`. It
  also stops when its own lock was not renewed for 90 s (for example after sleep). A stopped
  publish continues with *Resume*. A stopped promotion is `partial` or `failed`: promote again.
* **Break lock** (**U** on Content Views) removes the lock that the dialog shows. Use it only when
  the session of the holder is dead. Its workflow stops within 30 s. The audit log records it.

### Database ID

Each Content View database has a random ID. Snapshot repositories and Content View distributions
carry it as the label `pulp_tui_db=<id>`.

* pulp-tui refuses to change an object with the ID of another database:
  `... is owned by another pulp-tui database (<id>)`.
* It also refuses an object without the labels of its view and environment:
  `... is not labeled pulp_tui_env=Dev`. Delete and unpromote keep such an object.
* Two views can derive the same snapshot repository name: view `a-b` with repository `c` and view
  `a` with repository `b-c` both give `pulp-tui-cv-a-b-c`. The second view is refused with
  `... belongs to content view a-b (label pulp_tui_cv=a-b): view a with repository b-c derives
  the same name`. Create the second view under another name.
* Distribution names can collide in the same way: view `web` in `Library` with repository `b-c`
  and view `web-Library` in environment `b` with repository `c` both give
  `pulp-tui-cv-web-Library-b-c`. Promote (and its preview) refuses with `... belongs to content
  view web-Library in environment b (labels ...): view web in environment Library with
  repository b-c derives the same name`. Rename the view, the environment or the repository.
* An object without the ID label gets it at its next change.

### SQLite as the shared database

`sqlite:///srv/pulp-tui/cv.db` is safe only when all operators use one host (for example a jump
server). **Do not put the file on a network share (NFS, SMB):** SQLite locks do not work there,
and the file can become damaged. For several hosts, use PostgreSQL.

Each operator must be able to write the file and its `-wal` and `-shm` files. Use a group
directory with the setgid bit (`chmod 2770`). After `db migrate`, make the file group-writable
(`chmod 660`).

## Protect Content View objects (Pulp RBAC)

Other Pulp clients (`pulp` CLI, scripts) do not know the pulp-tui rules. A user with write access
to a snapshot repository can break published versions:

| Change by another client                    | Result                                          |
| ------------------------------------------- | ----------------------------------------------- |
| add or remove content                       | no effect; the next publish replaces it         |
| set `retain_repo_versions`                  | old versions and rollback stop working          |
| delete the repository or a distribution     | the environments stop serving that repository   |
| point a distribution to another publication | wrong content; the drift check (**D**) shows it |

Only Pulp RBAC stops this:

1. **pulp-tui operators** get global roles (no object): `rpm.admin`, and for File
   `file.filerepository_owner`, `file.fileremote_owner`, `file.filepublication_owner`,
   `file.filedistribution_owner`. Operators of one shared database change the objects of each
   other, so object roles are not sufficient.
2. **Other users** get only viewer roles (`rpm.viewer`, `file.filerepository_viewer`,
   `file.fileremote_viewer`, `file.filepublication_viewer`, `file.filedistribution_viewer`) and
   creator roles (for example `rpm.rpmrepository_creator`, `rpm.rpmremote_creator`,
   `rpm.rpmpublication_creator`, `rpm.rpmdistribution_creator`, `file.file…_creator`). Pulp gives
   the creator the owner role of each new object only.
3. **Do not give** `rpm.admin` or a global `*_owner` role to other users.

Assign roles on **Users** or **Groups**: **R**, then **c** with the role and an empty object. A
group such as `pulp-tui-operators` is easier to keep correct. RBAC does not stop superusers; keep
their number small. RBAC does not control downloads of served content (use content guards).

## Command line

```
pulp-tui [--config FILE] [-p|--profile NAME] [--data-dir DIR]   start the TUI
pulp-tui check                       test a profile non-interactively
pulp-tui export-cv [-o FILE]         export Content View definitions (JSON, no secrets)
pulp-tui import-cv FILE              import them (existing names are kept)
pulp-tui export-repos [-o FILE]      export repository and remote definitions (JSON, no secrets)
pulp-tui import-repos FILE           create the ones that the server does not have
pulp-tui db migrate                  create or upgrade the schema of the profile's database
pulp-tui --profile NAME db copy [--from PROFILE|local]
                                     copy Content View data into the database of NAME
pulp-tui config-example              print an example config
```

The global options also come from `$PULP_TUI_CONFIG`, `$PULP_TUI_PROFILE` and `$PULP_TUI_DATA_DIR`.
Logs go to `<data-dir>/pulp-tui.log`. Set the level with `PULP_TUI_LOG` (for example
`PULP_TUI_LOG=pulp_tui=debug`). Secrets are never logged.

**`export-cv`, `import-cv`, `db migrate`, `db copy`** use the Content View database of the
selected profile. `export-cv` and `import-cv` also use the Pulp server of the profile. `db migrate`
refuses a profile that uses the local file.

The `export-cv` file has `"format": 2`. It holds composites, components (by name), auto publish,
and the label and protection of Library (used only when an import creates Library). A format 1
file still imports.

* Versions are not exported, so `import-cv` (and **I** on Servers) imports each pin as `latest`
  with a warning, for example
  `warning: rocky9: component os was pinned to v1; imported as latest`. Pin it again with **e**.
* A component that the import does not find, or that is a composite, is skipped with a warning.
* The file holds the repository hrefs of the server that exported it. For each view that it
  creates, `import-cv` finds each repository on the server of the profile by name and type, and
  stores the href of that server. So you can import into another server when it has repositories
  with the same names (copy them first with `export-repos` and `import-repos`). When one or more
  are missing, nothing is imported and the error names each one, for example
  `error: repositories not found on this server (by name and type): os (RPM, view web); nothing was imported`.
* `export-cv` writes the current Pulp name of each repository, also when you renamed it after you
  added it to the view. When a repository is gone from the server, nothing is exported and the
  error names each one, for example
  `error: repositories not found on this server: os (RPM, view web); nothing was exported`.

**`export-repos`, `import-repos`** use the Pulp server of the profile (in the TUI: **X** and **J**
on Servers). Use them to copy the repository setup to another server:

```
pulp-tui --profile old export-repos -o repos.json
pulp-tui --profile new import-repos repos.json
```

* The file holds the RPM, File and Deb remotes and repositories. A repository gets the remote of
  the same name on the target.
* The export leaves out (with a `note:` line on stderr):
  * secrets (credentials, client keys, `sles_auth_token`, `headers`). The import names them:
    `warning: RPM remote os: set username, password again by hand (not in the export)`. Set them
    with **e** on Remotes.
  * signing services. Set them again with **e** on Repositories.
  * objects of Content Views and replication (`pulp_tui_*` labels, `UpstreamPulp`).
  * distributions, publications and content.
* `import-repos` never changes an existing object. It keeps it and prints the differences, for
  example `warning: RPM repository os exists; kept (differs: description)`. A field that the
  target does not have is left out with a warning. You can run the import again after an error.
