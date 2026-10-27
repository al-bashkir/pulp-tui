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

* The header always shows the active server. A **read-only** profile shows a yellow tag.
  A **production** profile turns the header red.
* Narrow terminals hide the screen list (use the palette) and the details pane. Below
  60x15 the UI asks for a larger terminal.
* Selection is shown with `▶` and reverse video, so it does not depend on color.

## Keys

| Key | Action |
|---|---|
| `Tab` | switch focus between the screen list and the main area |
| `:` or `Ctrl-P` | command palette: go to a screen, connect to a server, dismiss finished tasks. It opens in filter mode (`-- FILTER --`): type to filter, `Ctrl-J/K` or `Ctrl-N/P` move. `Esc` goes to move mode (`j k g G`), `/` filters again, a second `Esc` closes |
| `?` | help (a text view: `j k g G Ctrl-D Ctrl-U` scroll, `Esc` closes) |
| `N` | notification log |
| `q` / `Ctrl-C` | quit (q asks first when client operations still run) |
| `↑ ↓ j k PgUp PgDn Ctrl-D Ctrl-U g G` | move (in the screen list: `j k g G` choose a screen) |
| `h` / `l` | from the list to the screen list / from the screen list open the screen |
| `Enter` | action menu of the selected item (shows the shortcut letter of each action; `*` marks actions that change the server) |
| `/` | search: server-side, or in the list for Content Views and other pulp-tui screens (empty clears; lists the API cannot filter, such as Versions and Publications, say `this list has no search`) |
| `s` / `S` | sort by next column / reverse (current page) |
| `n` / `p` | next / previous page (100 items per page) |
| `Space` | mark item (compare, bulk remove or add content) |
| `t` | toggle RPM/File/Deb, content type, tracked/server tasks, task polling in Logs, or Content View snapshots in Repositories |
| `r` | refresh |
| `Esc` | cancel a loading request, clear marks, or go back |

In forms (they open in move mode): `j k`/`Tab`/`↑↓` change the field, `i` or `Enter` edits
a text field (the box shows `-- EDIT --`), `Enter` then goes to the next field (submits on
the last), and `Esc` stops editing. `h l`/`←→`/`Space` change a choice, `Space` toggles a
checkbox. `Ctrl-S` submits from either mode (also from an open picker, after it takes the
selection), and `Esc` in move mode cancels. A form with a single text, secret or number
field opens in edit mode.

While editing a text field (forms and the search input), the cursor moves inside the text:
`← →`/`Ctrl-B Ctrl-F` one character, `Alt-B Alt-F`/`Ctrl-← Ctrl-→` one word, `Home End`/`Ctrl-A Ctrl-E`
to the start or end. `Backspace`/`Delete` delete one character, `Ctrl-W` the word before the
cursor, `Ctrl-U`/`Ctrl-K` everything before/after the cursor.

Lists scroll with 5 rows of margin above and below the highlighted row (as vim `scrolloff=5`). In confirmations:
`y`/`Enter` confirms, `n`/`Esc` cancels; destructive operations require typing the shown
name, and so do rollbacks and promotions on a production profile. A dialog that a finished
background job opens never replaces the open one: it waits until that one closes (the footer
shows `N dialog(s) waiting`), and it ignores keys for 0.3 s after it appears, so keys typed
for another dialog never answer it. `j k g G Ctrl-D Ctrl-U ↑↓ PgUp PgDn Home End` scroll long previews. With a typed name
letters type, so only `Ctrl-D`/`Ctrl-U`, `PgUp`/`PgDn`, `Home`/`End` and the arrows scroll
(`Ctrl-J`/`Ctrl-K` do not).

**Pickers.** A field shown as `[ value ▾ ]` picks an existing object (repository, remote,
distribution, role, user, ...) from the server. `Enter` or `Space` opens the list in move
mode: `j k` or `↑↓` move one row, `g G` go to the first or last row, `Ctrl-D`/`Ctrl-U` or
`PgDn PgUp` move 10 rows. Typing a letter other than `j` or `k` on the closed field opens
the list in filter mode with that letter as the filter (`j` and `k` move to another field).
`/` enters filter mode (the popup shows `-- FILTER --`):
type to filter, `Ctrl-J/K` or `Ctrl-N/P` (and the arrow keys) move, and `Esc` keeps the
filter and returns to move mode. A second `Esc` closes the list and keeps the old value;
in move mode a single `Esc` closes it.
`Enter` selects and moves to the next field; it does not submit (use `Ctrl-S`). In a
multi-select list `Space` marks items and `Enter` takes all marked items (or the
highlighted one if none is marked). `Backspace` on the closed field clears it. If the
list cannot be loaded, the popup is in filter mode, shows the error, and `Enter` takes
the typed text as the value (with no text, it closes the list and keeps the old value).

### Three kinds of "cancel"

1. **Esc on a loading list** cancels the HTTP request in the client.
2. **Quitting** stops waiting for client operations. Pulp tasks continue on the server and
   are re-attached in the Task Center at the next start. **Connect** to a profile also stops
   waiting for them, so it asks first while operations run. When a task cannot be read
   (network or server error for more than about two minutes), the operation ends with
   "could not read task …, it may still be running": it is not reported as failed.
3. **Tasks → X** asks Pulp to cancel a server task. The Task Center then shows the state
   that Pulp reports (`canceled`, or `failed`/`completed` if the task ended first).

## Screens

### Dashboard
Server health (database, Redis, workers, content apps, storage), the current user,
installed plugins with versions, detected capabilities (RPM, File, Deb, ...), counts of
repositories, distributions, content and tasks (a Deb line when the server has pulp_deb),
and the number of Content Views and environments (it says if they are in the local file
or in the shared database). `r` refreshes.

### Servers
Named profiles from the config file. Actions: **o** connect, **T** test connection (shows
user, content origin, plugin versions), **c**/**e** create/edit, **d** remove from the
config (after typing the name; Pulp is not changed, and the Content View definitions of the
server stay where they are: the dialog names the database of the profile, or says the local
file), **K** store a password/token in the system keyring (when the profile has its own
database, it first asks if the secret is for the "Pulp server" or the "database"),
**E**/**I** export/import Content View definitions (with an external database, import
writes to it, so a read-only profile cannot import). See "Server profiles" and "Shared
database" below.

The create/edit form has three database fields: **Database**, **Database secret** and
**Database CA**. **Database** shows the database that the profile uses now: the path of the
local file, or the `[database]` that it follows. Type `local` (or the local file path) for
the local file, a file path such as `/srv/cv.db` for SQLite (it is saved as
`sqlite:///srv/cv.db`), or a `postgres://` or `sqlite://` URL. Never type a password in the
URL. If you do not change the field, the form adds no database block to the profile (a
block that the profile already has is kept).

**Connect** opens the Content View database of the profile in the background. If the
schema is not there yet, pulp-tui asks `Migrate database of <profile>?`. Saving a profile
with a new database asks the same. When you change the database location of a profile
that has Content View data, pulp-tui asks `Database of <profile> changes`: copy and
switch, switch without copy, or Esc. See "Shared database" for both.

### RPM Repositories / File Repositories / Deb Repositories
Columns: name, latest version, default remote, retention, description. Details explain
retention and mark Content View snapshot repositories. The Deb screen needs the pulp_deb
plugin on the server.

The list does not show Content View snapshot repositories (`pulp-tui-cv-<view>-<repo>`, label
`pulp_tui_cv`). Press **t** to show or hide them; the title shows `snapshots shown` when they
are visible.

| Key | Action |
|---|---|
| v | versions of the repository |
| o | browse content of the latest version |
| f | sync: choose remote (pre-filled with the default remote; empty = default) and policy (RPM: additive, mirror_complete, mirror_content_only; File and Deb: additive, mirror). Mirror policies show a warning first. |
| H | sync history (tasks that touched this repository) |
| u | upload a local file (RPM package or File content with relative path); large files use chunked upload with progress; the server digest is verified; failed or cancelled (Esc) chunked uploads are deleted. Not offered for Deb repositories (upload of `.deb` files is not supported). |
| P | publish the latest version: always creates a new publication, with the repository settings at that moment. Deb asks for an optional signing service (empty = the repository's own keys). The field is not asked when the server's Deb publications have no `signing_service`. |
| D | distribute: pin the latest version (publication) or follow the repository latest. Deb asks for the same optional signing service as **P**, for the pinned publication; a publication of the version is reused only when its signing service is the same. |
| C | copy content from another repository (filter rules; RPM can copy dependencies; a Deb copy is structured: it also copies the Release, component and architecture records of the packages, and never solves dependencies) |
| c / e / d | create / edit / delete (delete names every distribution that serves the repository, a version or a publication of it; refused while it holds Content View snapshots) |
| i | full JSON |

Edit sends only changed fields. Remote shows the default remote by name; clear it to remove
the default remote. Edit, sync, upload, copy and content add/remove are refused
on Content View snapshot repositories (a change of retention would prune their snapshots).

Deb repositories use the same keys. Sync a Deb repository from a Deb remote, publish it
(**P**) and distribute it (**D**) as for the other types. A Deb distribution is served
at `<content URL>/dists/<distribution>/...`. Distribution verification (**V** in
Distributions) fetches the `Release` file, finds the smallest package in a `Packages`
index and compares the digest of the downloaded `.deb`. Deb repositories can be used in
Content Views (see "Deb repositories in Content Views"). The fallback replication handles RPM
and File only.

### Remotes
`t` switches RPM/File/Deb. Create/edit: URL (File: URL of `PULP_MANIFEST`; Deb: the Debian
archive base URL, the directory that holds `dists/`), download policy, TLS validation,
CA/client certificate and key (file paths; content is uploaded), username and password,
proxy, concurrency, rate limit, labels, SLES token (RPM). The edit form cannot show the
certificates, key, credentials and token: empty keeps the server value, `-` clears it.
Delete lists repositories that use the remote.

Deb remote fields (each is shown only when the server has it):

| Field | Meaning |
|---|---|
| Distributions | required; space-separated releases to mirror, for example `bookworm bookworm-updates` |
| Components | space-separated, for example `main contrib`; empty = all |
| Architectures | space-separated, for example `amd64 arm64`; empty = all |
| Sync source packages / udebs / installer files | on or off (default off) |
| Ignore missing package indices | on or off; keeps a sync going when a `Packages` index is missing upstream |
| GPG public key | path of a local file; its content is uploaded and Pulp uses it to check the `Release` signature. On edit, empty keeps the server value and `-` clears it |

### Repository Versions
From a repository (**v**) or for all repositories. Columns: number, created, present /
added / removed counts. Actions: **o** content, **P** publish this version (always a new publication), **D**
distribute it (Deb: both ask for the optional signing service, as in Repositories), **b**
point a distribution to it (rollback, with preview; Deb: the new publication keeps the
signing service of the publication that the distribution serves now), **m** compare
with the marked version (`Space` on one, `m` on the other; shows added/removed units of every
content type that the Content browser lists, each line starts with its type, such as
`+ advisory RHSA-2024:0001`; older → newer), **d** delete (names the distributions that serve the version or its
publications, and you type the version id; refused for Content View snapshots; never needed
for rollback).

### Content browser
Opened from a repository or version. `t` cycles the content types of the repository type.
RPM: packages, advisories, modulemds, package groups, distribution trees, repo metadata
files. Deb: packages (package, version, architecture, sha256), releases (distribution,
codename, suite, version), release components, source packages. `/`
searches package names (on the RPM and Deb packages lists) or file paths. **i** shows all metadata of a unit (requires,
provides, files, advisory package lists, …). **x** removes the marked (or selected)
units from the repository; **a** adds them to another repository (Deb: a structured copy,
which also copies the Release, component and architecture records, so apt finds the packages;
only on the Deb packages list, because other Deb units would arrive without their records). Both create a new
repository version; old versions are unchanged.

### Publications
`t` switches RPM/File/Deb. **c** create from repository name and optional version number
(Deb: and the optional signing service, as in Repositories): always a new publication, with the
repository settings at that moment. **D** distribute: serves exactly the selected publication.
**d** delete (names the distributions that serve it, and you type the
publication id; refused for publications of Content View snapshots).

### Distributions
`t` switches RPM/File/Deb. Details say whether a distribution is **PINNED** (publication or
repository version) or **FOLLOWS LATEST** (repository). **y** copies the content URL
(OSC 52 terminal clipboard), **V** verifies what is served (for a distribution that follows
a repository: its newest publication, which is not the latest version when that version has
no publication), **b** re-points to a version (preview first; Deb: with the signing service
of the publication it serves now), **c**/**e** create/edit
(name, base path, binding, content guard, labels; Deb: the optional signing service of a
pinned publication), **d** delete. A distribution of a
Content View (label `pulp_tui_cv`) cannot be re-pointed or re-bound here (use promote or
roll back) or renamed, and its `pulp_tui_*` labels stay (promote finds it by name and
labels); its base path and other labels can be edited, and delete warns.

### Content Views
Managed by pulp-tui (see `docs/ARCHITECTURE.md`). Columns: composite (`yes` for a
composite view), repositories, latest version, environments with bound versions and
states. A composite view has components instead of repositories; see "Composite Content
Views" below.

| Key | Action |
|---|---|
| c / e / d | create / edit description / delete (removes the view's distributions and snapshot repositories, never source repositories; the dialog lists every served path, and a served protected environment must be typed after the view name, for example `web Prod`). On a composite, Edit also sets auto publish and the pin of each component, and Delete removes only its distributions. |
| a | add repositories: a multi-select picker of RPM, File and Deb repositories (those already in the view and Content View snapshot repositories are not listed) and filter rules; the rules apply to each selected repository, so a selection of more than one type needs empty rules. On a composite: add components (see below). |
| F | edit the filter rules of one repository (not offered on a composite) |
| x | remove a repository from the definition (published versions are unchanged). On a composite: remove a component. |
| f | sync repositories: a multi-select picker of the view's repositories (all marked) and one sync policy (RPM values when the view has an RPM repository, else the File and Deb values additive and mirror; File and Deb repositories treat any mirror policy as mirror). Each repository syncs from its own default remote; one without is skipped. Mirror policies show a warning first. The tasks show in Tasks; the result lists each repository: new version, no change, failed or skipped. Sync does not publish. Not offered on a composite. |
| w | preview: exact content the draft selects compared with the latest published version |
| P | publish a new version: notes (and the Deb signing, when the view has a Deb repository) → preview → confirm; the version becomes active in Library |
| v | versions of the view |
| M | promote a version to an environment (preview of every distribution; protected environments require typing their name) |
| b | roll back an environment to an earlier version |
| D | verify all environments (drift check) |
| U | break the lock of the selected view (shows who holds it; you type the view name). See "Shared database". |

Publish, promote, roll back, delete (the view or a version) and *Resume* take a lock on
the view while they run; a second operator (or a second pulp-tui on this host) gets
`Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC` before
anything is sent to Pulp. Preview, compare and the drift check do not lock. Sync
repositories does not lock the view; it writes one audit entry (`cv.sync`) when the syncs are
submitted, with how many were submitted, failed or skipped (the per-repository result of the
finished tasks is shown in the result view, not in the entry).

A snapshot repository of a view is never a source of a view. Publish refuses a view that has
one (for example, added before this check existed):
`pulp-tui-cv-web-os is a snapshot repository of content view web; remove it from outer and add its source`.
Remove it (**x**) and add the source repository of the other view. To use the content of
another view, make a composite view.

Each promoted repository is served at
`https://<pulp>/pulp/content/<lc label>/<view>/<repo>/`, where `<lc label>` is
the label of the lifecycle environment (see "Lifecycle Environments"). A promotion keeps the
path of a distribution that exists: distributions of an earlier pulp-tui stay at `cv/...`.
For a custom path, change the `base_path` of the distribution in Pulp (Distributions,
Edit); later promotions keep it. A relabel moves all paths of the environment to
`<lc label>/<view>/<repo>`, also the paths that you changed.

Filter syntax (rules separated by `;`):

```
include name=kernel* arch=x86_64 evr>=5.14-1
exclude name=*-debuginfo
include name=nginx* arch=amd64 version>=1.22    # Deb repositories
exclude name=*-dbg version<1.0                  # Deb repositories
exclude path=isos/*            # File repositories
```

A unit is selected when (no include rule exists or one matches) and no exclude rule
matches. All criteria of one rule must match. `evr` supports `= < <= > >=` with rpm
version comparison; `epoch:` is optional (a number); without a release only epoch and
version are compared. The value must start with a letter or digit, so a typo such as
`evr=>5.14` is refused instead of meaning `= >5.14`.

Each repository type accepts its own criteria, and a rule with a criterion of another type
is refused with a message that names the types that have it: RPM takes `name=`, `arch=` and
`evr`; Deb takes `name=`, `arch=` and `version`; File takes `path=`. `version` supports the
same operators as `evr` with the Debian rules for version comparison (`~` sorts before
everything, `epoch:` is optional, and a rule without a revision matches every revision of
that upstream version). The same rules filter the copy action (**C**) of a repository.

#### Deb repositories in Content Views

A Content View can hold Deb repositories, also together with RPM and File repositories (then
without filter rules). Publish copies the selected packages with a structured copy
(`deb/copy/`): it also copies the Release, component and architecture records of these
packages, so apt finds the same distributions and components. Without filter rules, all
content is copied. A filtered snapshot has only the selected binary packages: source packages
and the upstream index files are not copied, and the publication makes new indices.

**Deb signing.** The Publish form shows this choice when the view has a Deb repository. One
choice applies to every Deb repository of the publish:

* `same as source` (the default): the snapshot repository gets the `signing_service` and the
  per-release keys (`signing_service_release_overrides`) of its source repository.
* `none`: the snapshot repository has no keys (no signing service and no per-release keys).
  Clients get an unsigned `Release` (no `Release.gpg` and no `InRelease`).
* `service`: pick a signing service in **Signing service**. The snapshot repository gets it,
  and no per-release keys. The picker lists the signing services of all plugins; Pulp
  refuses one that is not a Deb (APT) signing service, and Publish stops with `signing
  service <href> was refused by the server; delete this Content View version and publish
  again with another choice` (*Resume* would send the same choice). Publish stops with `pick a signing
  service` when you pick none, and with `not supported by this server: signing_service on
  Deb repositories` when the server's Deb repositories have no signing field.

Pulp changes only the per-release keys that a request names, so pulp-tui removes each
per-release key that the snapshot repository must not keep (also one that the source no
longer has). pulp-tui changes only the snapshot repository, never the source. The choice is saved with
the version, so *Resume* signs as you asked. Each Deb repository of a version gets its own
publication: a publication that repository keys signed does not record the keys, so
pulp-tui never reuses one for a Deb repository (also when the content is the same as in the
previous version). A composite uses the publications of its component versions, so its
Publish does not ask.

**Large selections.** Pulp refuses a request body above 2.5 MiB (Django
`DATA_UPLOAD_MAX_MEMORY_SIZE`), which is about 35,000 package hrefs. A filtered snapshot starts
from the latest version of the snapshot repository: pulp-tui removes what the rules no longer
select, then copies what is missing, each list in batches, one after the other. The job's
progress shows each step, for example `main: copying 25316 units (2/3)`. An unchanged selection
sends nothing and uses the latest version again. Each batch makes one version of the snapshot
repository, and the last one is the snapshot. After the copy, pulp-tui deletes the versions of
the snapshot repository that no Content View version uses (Pulp merges each one into the next
version, so the snapshot does not change); version 0 stays, and a version that fails to delete
is logged and tried again by the next publish. When a publish stops between batches, Resume
continues from the latest version of the snapshot repository. Copy
content (`C` on a repository) also sends batches; Tasks shows the last one. A repository
without filter rules sends no list of hrefs.

#### Composite Content Views

A composite view joins other Content Views (its **components**), as a Katello composite
does. Example: the view `rocky9-os` holds BaseOS and AppStream, and you keep it at a
version that you tested. The view `rocky9-extras` holds docker, postgres and other
repositories that you want to follow. The composite `rocky9` has both as components:
`rocky9-os` pinned to a version, `rocky9-extras` on `latest`. When you publish
`rocky9-extras` and then `rocky9`, clients of `rocky9` keep the OS packages and get the new
extras.

* **Create.** `c` has the flag *Composite (fixed after create)*. A view is composite or
  normal for its whole life. A composite has no repositories and no filters of its own, and
  a composite cannot be a component of another composite (no nesting).
* **Components.** `a` opens a multi-select picker of the normal Content Views that are not
  yet components, then asks for the pin of each: `latest` (the newest published version at
  the time the composite publishes) or one published version (`v3`). `x` removes a
  component. `e` sets the description, **auto publish** and the pin of each component.
  Changes take effect at the next publish. The repository actions `a` (add repository), `F`,
  `x` (remove repository) and `f` are not offered on a composite.
* **Repository rule.** Two components must not contain the same repository, and two
  repositories must not have names with the same path segment (`os/x` and `os x` both
  become `os-x`). pulp-tui checks this when you add a component and again at every
  publish, over the versions that the publish would use, and names both views or both
  repositories. Nothing is written when it refuses.
* **Publish** (`P`) resolves each `latest` to the newest published version of the
  component. It is refused, before anything is written, for a composite with no
  components, a `latest` component that has no published version, a pinned version that
  is no longer published, and a repository clash (see above). It copies nothing in Pulp:
  the composite version only records which component versions it uses and points at their
  snapshot repositories and publications. Like every publish, it becomes active in
  Library. The preview lists every component version (`rocky9-os: v1 (unchanged)`,
  `rocky9-extras: v1 -> v2`), then the content that changes in each repository.
* **Served paths.** A composite has its own distributions
  `pulp-tui-cv-<composite>-<env>-<repo>` at
  `<lc label>/<composite>/<repo>`. Promote, roll back and the drift check work as
  for a normal view. One publication can be served by the distribution of its component and
  by the distribution of the composite at the same time.
* **Auto publish.** Off by default, set per composite with `e`. When a component publishes
  (Publish, or *Resume* that finishes), every composite with auto publish that lists this
  component on `latest` publishes too, each under its own lock. A component that is pinned
  does not trigger it. The result is part of the notice, for example `rocky9-extras version
  2 published and active in Library; auto publish: rocky9 v2 published`. If a composite
  fails (for example, it is locked), the component stays published and the notice is a
  warning that names the composite and the reason. Auto publish never promotes: the new
  composite version is active in Library only; promote it yourself.
* **Delete.** The delete actions never remove an object that a component owns.
  * A component version that a composite version uses (or a composite pins) cannot be
    deleted: `version 1 is used by rocky9 v1, rocky9 (pinned)`.
  * A Content View that is a component, or that a composite version uses, cannot be
    deleted: `content view rocky9-os is used by rocky9 (component), rocky9 v1; remove it
    from the composite and delete those composite versions first`.
  * Deleting a composite version, or a composite, removes only the composite's own
    records and distributions. The component snapshots and publications stay.
  * Deleting a version keeps every publication (with its repository version) that any
    environment of the server still serves. Deleting a view stops with `... is still
    served by <view> in <environment>` when another view serves one of its publications.
    Pulp deletes the publications of a repository version with it, and a deleted
    publication silently stops its distributions (COMPATIBILITY.md). These rules also apply
    to normal views.

### Content View versions
**M** promote, **b** roll back, **m** compare with the marked version, **R** resume a
publish that failed or was interrupted (continues from checkpoints and re-attaches to a
submitted task; with a shared database it can also continue a publish that another
operator started), **W** watch the pending copy task of a publishing version (see Tasks;
only the copy step, and the publication of a Deb repository, store their task, so during
other steps W says that no task is pending),
**d** delete (refused while bound, and while a composite uses it),
**i** details (source versions, snapshot versions, publications, unit counts; for a
composite version, the component versions). A `publishing` version shows as `interrupted`
when nobody holds a live lock on the view: the client that ran it ended (quit, crash), so
nothing continues it until you press **R**. Compare of two composite versions shows the
component versions first (`rocky9-extras: v1 -> v2`), then the added and removed content of
each repository.

### Lifecycle Environments
Library exists automatically. **c** creates an environment after an existing one
(paths can branch). **L** toggles protection. **d** deletes an unused environment. With a
shared database, environments belong to the server and every operator sees them.

Each environment has a **label**, the part of the served path that names it (column
*label*). A new environment gets its name in lower case as the label (`Dev` gives `dev`).
**e** edits the label: 1-64 characters of `a-z 0-9 - _ .`, unique for the server. pulp-tui
loads the served paths that change and shows them as `/old -> /new`, with the line
`Clients using the old paths stop getting content.` When served paths move, you type the
environment name to confirm (when none move, there is no confirmation). Then it moves each
distribution to the new path in place (one Pulp task each), and a result view lists what
moved, and the drift check of each bound view. The distribution names do not change. The label is stored first. If a row fails, it stays at its old path
and is named in the result; run Edit again with the same label to move the rest. A relabel
locks every view that is bound in the environment (see "Locks"), and stops with the
holder's name if another operator holds one of them. A view bound in the environment during
the relabel is not moved; run Edit again. A label must not overlap the base path of another distribution (for example a repository distributed at
`dev`, or at `dev/tools`): Pulp refuses a path that is equal to, a parent of, or a child of
another path, and the rows that fail stay as described above.

### Replication
Native Upstream Pulp objects (tech preview). **c**/**e** create/edit: upstream base URL
as reachable from *this* Pulp server, relative API root (`pulp/`), domain, credentials,
CA/client certificates, TLS validation, distribution filter (`q_select`, e.g.
`name__startswith="rhel9"`), deletion policy (`nodelete` default; `all` needs typed
confirmation). **R** replicates and shows the task group result. **H** shows recent
replication task groups. **F** starts the fallback: choose another configured profile as
source, types, a name filter and additive or mirror mode; a dry run lists
CREATE / SYNC / SKIP per distribution before anything changes; reruns are safe. The
fallback does not follow an upstream label change: a distribution that the source moved to
another base path is reported as a 409 conflict, and the replica stays at its old path.
Delete the replica distribution, or move it by hand, to make it follow.

### Users, Groups, Roles
Users: **R** roles, **c**/**e** create/edit, **w** set password (Pulp's local users
only), **A** enable/disable, **d** delete (refused for yourself; staff accounts warn).
Role assignments: **c** add (role name, optional object href or domain for scope), **d**
remove; owner/admin roles ask for confirmation. Groups: **o** members, **R** roles,
**c**/**d**. Roles: system roles are locked (read-only); custom roles can be created,
edited (permissions as a comma list) and deleted.

### Tasks
Tracked tasks with state, progress (only what Pulp reports; no invented percentages),
start time and errors. The top box lists client operations still running. **W** opens a
live view of the task (state, progress reports, times, error, created resources) that
refreshes until the task ends; **i** details with all progress reports, **X** cancel on
the server (confirmation), **z** dismiss finished tasks from the list (above 500 tracked
tasks, the oldest finished one goes by itself). `t` switches to all
server tasks (**a** tracks one; **W** tracks it and opens the live view).

### Logs
Everything that happened in this session, newest first, with the time in UTC. It works
without a connection and follows new rows while it is open. Three kinds of rows:
`request` (every HTTP request to Pulp: method, path, status or error, duration),
`action` (every action you started, also ones you then cancelled, for example "Delete on os-repo") and `notice` (every
message that the footer showed, with its `[ok]`/`[warning]`/`[error]` tag). `Enter` opens
a row: for a request the full request and response bodies, for the others the message.
`/` filters the rows. The polling of tasks (a GET of one task or task group, repeated
while a job runs) is hidden by default; `t` shows or hides it.

Secrets are redacted: the `Authorization` header is never recorded, JSON values of
`password`, `token`, `secret`, `client_key`, `authorization`, `headers` (of a remote) and
keys that end in `_password`, `_token` or `_secret` (for example `proxy_password` and
`sles_auth_token`), in any letter case, show as `•••`, and
uploads show only their size. A body is cut at 64 KiB.
The log keeps the last 1000 rows in memory for this session only. When it is full, the
hidden polling rows are dropped first. It is never written to disk and is gone when you
quit.

## Server profiles

The config file (`--config`, `$PULP_TUI_CONFIG`, or
`$XDG_CONFIG_HOME/pulp-tui/config.toml`) holds named profiles; `pulp-tui config-example`
prints a full example. Profile selection: `--profile` > `$PULP_TUI_PROFILE` >
`default_profile` > first profile.

Secrets are references:

| Reference | Use |
|---|---|
| `env:PULP_PASSWORD` | environment variable (CI, containers) |
| `file:/run/secrets/pulp` | mounted secret file (trailing newline is removed) |
| `keyring` | system keyring, service `pulp-tui`, user = profile name (store it with **K**; see "System keyring") |

Auth methods: `basic` (username + secret), `bearer` (secret is the token), `none`
(anonymous or identity from the mTLS client certificate). TLS options: `ca_cert`
(PEM bundle added to the system roots), `client_cert` + `client_key` (mTLS). `proxy`,
`timeout_secs`, `read_only`, `production`. Credentials never go into `url`, and `proxy`
may not hold a password: set an authenticated proxy in `HTTPS_PROXY` instead.

A profile can have its own Content View database. It takes the same fields as the
top-level `[database]` (see "Shared database"):

```toml
[profiles.prod.database]
url = "postgres://pulp_tui@db.example.com:5432/pulp_tui_prod"
secret = "keyring"            # keyring user [database]:prod
# ca_cert = "/etc/pki/tls/certs/internal-ca.pem"
```

The database of a profile is the first of these (the TUI and the command line use the same
rule):

1. the `[profiles.<name>.database]` of the profile;
2. else the top-level `[database]`;
3. else the local file `<data-dir>/pulp-tui.db`.

`url = "local"` in `[profiles.<name>.database]` keeps that profile in the local file even
when a top-level `[database]` is set. The form accepts `local` or the path of the local
file for this, and a bare absolute path such as `/srv/cv.db` for an SQLite database. A
form value that is the same as what the profile would use without a block (the inherited
`[database]`, or the local file) stores nothing, so an untouched form never adds a
`database` block. The keyring user of the `keyring` secret of a profile database is
`[database]:<profile>`; the top-level `[database]` uses the user `[database]`.

### System keyring

pulp-tui does not keep a keyring file. It gives the secret to the keyring of the operating
system:

| System | Store |
|---|---|
| Linux, BSD | Secret Service over D-Bus (`org.freedesktop.secrets`): GNOME Keyring, KDE Wallet (`ksecretd`), KeePassXC |
| macOS | Keychain |
| Windows | Credential Manager |

On Linux the secret goes to the `default` collection (unlocked first, which can show a
password prompt). Each entry has the attributes `service=pulp-tui` and `username=<user>`
and the label `keyring:<user>@pulp-tui`:

| Secret | Keyring user |
|---|---|
| Pulp server of profile `prod` | `prod` |
| `[profiles.prod.database]` | `[database]:prod` |
| top-level `[database]` | `[database]` |

* **K** on the Servers screen only writes the keyring. Set `secret = "keyring"` in the
  config too, else pulp-tui does not read the entry. **K** again replaces the secret.
* **K** cannot store the top-level `[database]` secret. Create it with the system tool.
* Builds without the `keyring` feature (`--no-default-features`, the static Linux
  release binary, the container image) give an error for `keyring`; use `env:` or `file:`.
* KDE Plasma 6: KWalletManager does not show these entries. It shows only entries with
  the KWallet attributes `server`, `user` and `type`. The entries are in the wallet
  (folder `Secret Service`) and pulp-tui reads them; use `secret-tool` to see them.

Linux examples with `secret-tool` (libsecret):

```sh
secret-tool search --all service pulp-tui          # list entries (prints the secrets)
secret-tool store --label='keyring:[database]@pulp-tui' service pulp-tui username '[database]'
secret-tool clear service pulp-tui username prod   # remove the secret of profile prod
```

## Shared database

A profile without a database (neither its own `database` nor a top-level `[database]`)
keeps Content Views, versions, environments, promotions and their history in a SQLite file
in the data directory. Only one operator uses it. With a database, several operators use one
database and see the same Content Views, versions, environments, promotions and history.
There is no pulp-tui server: each pulp-tui connects to the database directly. A change of
another operator shows after a refresh (`r`); nothing is pushed.

```toml
[database]
url = "postgres://pulp_tui@db.example.com:5432/pulp_tui"   # or "sqlite:///srv/pulp-tui/cv.db"
secret = "env:PULP_TUI_DB_PASSWORD"                         # env:VAR, file:/path or keyring
ca_cert = "/etc/pki/tls/certs/internal-ca.pem"              # optional, PostgreSQL only
```

* `[database]` is the default for all profiles. A profile with its own
  `[profiles.<name>.database]` uses that instead, and `url = "local"` there keeps the
  profile in the local file (see "Server profiles"). Profiles in one config file can
  therefore use different databases. Profiles with the same connect URL share one open
  store in a running pulp-tui.
* The password is never in the URL. A URL with a password is rejected. `secret` is a
  reference, as for profiles. For `keyring` the entry has the service `pulp-tui` and the
  user `[database]`; the **K** key of the Servers screen cannot store it, so use `env:` or
  `file:` unless you create the entry with the system keyring tool. The database of a
  profile uses the keyring user `[database]:<profile>`, and **K** can store that one (it asks
  "Pulp server" or "database"). Error messages show the URL without the password.
* PostgreSQL traffic is always encrypted and verified (certificate chain and host name),
  except through a Unix socket or to `localhost`, `127.0.0.1` or `::1`, where it does not
  leave the machine. For other hosts pulp-tui overrides any `sslmode` in the URL with
  `verify-full`. The server certificate must list the host name (or IP address) that the
  URL uses.
* `ca_cert` is a PEM file with one or more CA certificates. It is **added to the root
  certificates that are built into pulp-tui. The trust store of the operating system is not
  used for the database.** An internal CA therefore needs `ca_cert`, also when the host
  already trusts it. (This differs from the `ca_cert` of a profile, which adds to the
  system roots.)
* The database of a profile opens in the background when you connect to the profile. If it
  cannot be opened, the Pulp screens still work. The Content View screens and actions show
  `shared database unavailable: <reason>` and do nothing. pulp-tui does not use the local
  file instead, because the state would split. Fix the cause and Connect again (a failed
  open is not kept, so the next Connect tries again). This is also true for a schema that
  `db migrate` did not create or upgrade: the Content View screens show `shared database
  unavailable: postgres://pulp_tui@db.example.com:5432/pulp_tui: database not migrated
  (open Servers, Connect again to migrate, or run pulp-tui db migrate --profile prod)`, and
  Connect offers the migration (see below). The command-line Content View commands
  (`export-cv`, `import-cv`, `db copy`) stop with, for example, `... database schema 1 is
  older than this pulp-tui (5); run pulp-tui db migrate --profile prod` or `...: database is
  not initialized; run pulp-tui db migrate --profile prod`.
* **Migrate from the TUI.** When Connect finds a database with no schema, or an older one,
  it asks `Migrate database of <profile>?`. The dialog shows the database, the schema
  change (for example `empty -> 5`) and a warning that other operators then need this
  pulp-tui version or newer. Enter migrates (with the role of the connection, which must
  create tables, as for `db migrate`). Esc does not migrate; the Content View screens then
  show the message above. When you save a profile with a new database, pulp-tui opens it at
  once and asks the same question. A SQLite file that does not exist yet counts as not
  migrated: only the migration creates it, a normal open never does. A read-only profile
  gets a warning instead of the question: run `pulp-tui db migrate --profile <name>`.
* **Change the database of a profile.** When you edit a profile and change the location
  (the URL) of its database, pulp-tui reads the old database (it only reads it) and counts
  the Content Views, versions and environments of this server. If there are none, it saves
  the profile. Otherwise it shows `Database of <profile> changes`:
  **copy and switch** copies the data into the new database like `db copy` (new IDs, never
  merges), and saves the profile only after the copy worked; **switch without copy** only
  saves, and the old data stays in the old database; Esc cancels and the profile is not
  changed. Objects that the old database published in Pulp carry its database ID, so the new
  database can change them only after a copy, which records the old ID (see "Database ID").
  Nothing is deleted from the old database. A read-only profile, and an old database that
  cannot be read, get only **switch without copy**. A change of only the secret or the CA is
  saved at once. The change takes effect at the next Connect.

### Set up (administrator)

1. Create the PostgreSQL database and a role for the operators.
2. Run `pulp-tui db migrate --profile <name>` with a role that can create tables. It creates
   the schema the first time and upgrades it after you upgrade pulp-tui. A normal start
   never changes the schema. If the schema is older, the Content View screens show
   `... database not migrated (...)` (see above). If it is newer, upgrade pulp-tui. When two
   administrators migrate at the same time, the second one waits. After you upgrade to the
   pulp-tui version with lifecycle environment labels, migrate an external database that is
   at schema 2 once (the TUI offers it on Connect, or run `pulp-tui db migrate --profile
   <name>`). Schema 3 gives each environment the label of its lower-case name, so no served
   path moves. An external
   database at schema 3 needs the same migration (the TUI offer, or `db migrate`) before
   composite Content Views work: schema 4 adds their tables and columns.
   **Upgrade to the version with Deb Content Views:** this version adds schema 5: run
   `pulp-tui db migrate`; every operator of a shared database must upgrade; older versions
   stop with `database schema 5 is newer than this pulp-tui`. Schema 5 adds the column
   `cv_version_repos.signing` (the Deb signing choice of a publish). An older pulp-tui also
   cannot read a view with a Deb repository.
3. Give the operators' role only `SELECT`, `INSERT`, `UPDATE` and `DELETE`. After a migration
   that adds tables, grant again (or use `ALTER DEFAULT PRIVILEGES`).

   ```sql
   GRANT USAGE ON SCHEMA public TO pulp_tui_op;
   GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO pulp_tui_op;
   ```

4. Each operator puts the same `[database]` in the config (or the same `database` in the
   profile) and starts pulp-tui.

### Move the data of one operator

`pulp-tui --profile lab db copy` copies the Content View data of the profile `lab` from the
local file into the database of `lab`: Content Views with repositories and filters (and
composites with their components, pins and auto publish flag), versions and their
checkpoints (and the component versions that a composite version used), environments,
bindings and the audit log. The rows get new IDs. The
local profile name becomes the server key (see below). The copy runs in one transaction. It
**never merges**: if the target database already has a Content View, or an environment
other than Library, with the same name for this server, it stops and writes nothing. Task
references, replication history and locks are not copied. The source stays as it is. A
read-only profile cannot run `db copy`. Run it once for each server.

`--from <profile|local>` chooses the source. The default is `local`: the local file, also
when a profile has that name. With a profile name, `db copy` reads the database of that
profile (an external one with its server key, the local file with the profile name).
For example, `pulp-tui --profile prod db copy --from lab` copies the data of `lab` into the
database of `prod`, under the server key of `prod`. The command refuses two cases and
writes nothing:

* The source and the target are the same database, also when two URLs reach it:
  `source and target are the same database`.
* A Content View of the source has a live lock (renewed within the last 2 minutes): `Content
  View web is locked by alice@ws12 (publish); copy after it ends`. The lock of a crashed
  session expires after 2 minutes and does not block the copy.

After the copy, objects that the source published get the database ID label of the
target database on the next publish or promotion (see "Database ID" below). After that, the
old source cannot change them any more. The TUI does the same copy when you change the
database of a profile and choose "copy and switch" (see above).

### What is shared

| Shared (in the external database) | Always local (`<data-dir>/pulp-tui.db`) |
|---|---|
| Content Views, repositories and filters, versions and checkpoints, environments, bindings, audit log (with the operator), locks | Task Center references, replication history |

* **Server key.** In the shared database, rows belong to the server, not to a profile name:
  the key is the URL, the API root and the domain (for example
  `https://pulp.example.com/pulp/#default`). Two operators can name their profiles
  differently and still see the same Content Views. The local file keeps the profile name.
  Deleting a profile (Servers, **d**) changes nothing in the database.
  The key comes from the URL that you type: **every operator must use the same scheme,
  host name, port, `api_root` and domain for a server.** `https://pulp.example.com` and
  `https://pulp.example.com:443` give the same key, but `https://pulp` (a short name),
  an IP address or `http://` give a different key, and that operator sees other (or no)
  Content Views. `db copy` uses the URL of the profile that runs it. The Dashboard shows
  the key as `shared database scope: <key>`; compare it between operators.
* **Operator.** Locks and audit rows name the operator as `username@host`, for example
  `alice@ws12`. The audit lines in the Content View details show it: `<time>
  cv.publish.done web version 3 by alice@ws12`. A row without a name (written before the
  upgrade) shows no `by`; `db copy` marks such rows `by copied`. In a container the host
  name is a random container ID: start the container with `--hostname` (for example
  `--hostname ws12`), so that the lock owner and the audit actor stay the same.
* **Read-only profiles.** When the database of the profile is external (its own, or
  `[database]`), creating or editing a Content View, adding or removing repositories,
  filters and components, and creating, deleting or protecting an environment change state
  that other operators publish from. A read-only profile does not show these actions. `import-cv` and
  `db copy` refuse a read-only profile, and the TUI neither migrates its database nor
  offers "copy and switch" for it. When the profile uses the local file, these actions stay
  available, because they change only the local file. The exception is the label edit on
  Lifecycle Environments: it moves distributions in Pulp, so a read-only profile does not
  show it, also with the local file.

### Locks

* Publish, *Resume*, promote, roll back, reconcile (promote again), delete a Content View
  and delete a version take a lock on **one Content View**. A second operator gets
  `Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC` before
  anything is sent to Pulp. The lock is also taken when two pulp-tui processes use one
  local file.
* Publishing a composite locks the composite only; it reads the component versions and
  never changes them. Auto publish takes the lock of each composite after the lock of the
  component is released.
* Changing the label of an environment (Edit on Lifecycle Environments) takes the locks of
  **all** views that are bound in it, in the order of their IDs. If another operator holds
  one of them, the relabel stops before it changes anything, with that operator's name.
* Preview, compare and the drift check do not lock. Edits (create, add or remove
  repositories, filters or components, create, protect or delete an environment) do not
  lock either: the last write wins.
* The holder renews the lock every 30 seconds. A lock that is not renewed for 2 minutes is
  expired, and the next operator takes it over. Times come from the client clocks.
* The lock is released when the workflow ends (also with an error), and when you press Esc
  on its job. If pulp-tui crashes, the lock stays until it expires.
* If a holder loses the lock (someone broke it, another operator took it over after it
  expired, or the database is not reachable or does not answer a renewal within 30 s), the
  workflow stops before its next change in Pulp and before its next checkpoint: `lock on
  web lost: <reason>`. A workflow also stops when its lock was not renewed for 90 seconds,
  for example after the computer slept: `lock on web lost: not renewed for 412 s`. A
  stopped publish keeps its checkpoints, so *Resume* continues it. A stopped promotion is
  stored as `partial` (or `failed` if no repository was done); promote again. If another
  operator has changed that version or that promotion since then, the stopped workflow
  does not overwrite their result.
* **Break lock** (**U** on Content Views) removes a lock whoever holds it. It shows the
  holder and asks you to type the view name. Use it only when the session of the holder is
  dead; its workflow stops within 30 s. Only the lock that the dialog showed is removed: if
  another operator took the lock meanwhile, nothing is removed and the message shows the
  new holder. The audit log records it.

### Database ID

Each CV database (the local file too) has a random ID. Derived repositories and Content
View distributions carry it as the label `pulp_tui_db=<id>`. pulp-tui refuses to change an
object with the ID of another database: `... is owned by another pulp-tui database (<id>)`.
Objects without the label (made by an older pulp-tui) are adopted and labeled on the next
change. This stops two databases that manage the same Pulp server from taking over each
other's objects, for example an old local file after you moved to a shared database.

### SQLite as the shared database

`url = "sqlite:///srv/pulp-tui/cv.db"` is safe only when every operator runs pulp-tui on
that one host (for example several users of a jump server). **Do not put the file on a
network share (NFS, SMB):** SQLite locking and WAL do not work there and the file can be
damaged. For operators on different hosts, use PostgreSQL. `db migrate` creates the SQLite
file; a normal start does not.

Every operator's account must be able to write the file and its `-wal` and `-shm` files,
which SQLite creates next to it. Put the file in a directory of a group that has all
operators, make the directory group-writable with the setgid bit (`chmod 2770`, so new
files get that group), and after `db migrate` make the file group-writable (`chmod 660`).
SQLite gives the `-wal` and `-shm` files the permissions of the database file.

## Protect Content View objects (Pulp RBAC)

pulp-tui refuses to change Content View objects by hand (retention, sync, upload, copy,
content changes, delete). Other Pulp clients (`pulp` CLI, scripts, the API) do not know these
rules. A user with write access to a snapshot repository can break published versions:

| Change by another client | Result |
|---|---|
| add or remove content | no effect on published versions; the next publish replaces it |
| set `retain_repo_versions` | Pulp deletes old versions and their publications: older Content View versions and rollback stop working |
| delete the repository or a distribution | the environments of that view stop serving that repository |
| point a distribution to another publication | the environment serves wrong content; the drift check (**D**) reports it |

Only Pulp RBAC stops this. Set up the roles so that only pulp-tui operators can change these
objects:

1. **pulp-tui operators** get global roles (no object): `rpm.admin`, and for File
   `file.filerepository_owner`, `file.fileremote_owner`, `file.filepublication_owner`,
   `file.filedistribution_owner` (pulp_file has no `file.admin` role). Operators of one shared
   database change the objects of each other, so object-level roles are not sufficient for
   them.
2. **Other users** get only roles that do not give write access to objects of other users:
   * to see everything: `rpm.viewer`, and `file.filerepository_viewer`,
     `file.fileremote_viewer`, `file.filepublication_viewer`, `file.filedistribution_viewer`;
   * the creator roles of what they make, for example `rpm.rpmrepository_creator`,
     `rpm.rpmremote_creator`, `rpm.rpmpublication_creator`, `rpm.rpmdistribution_creator`, and
     the `file.file…_creator` roles. Pulp gives the creator the owner role of each new object
     (for example `rpm.rpmrepository_owner`) on that object only.
3. **Do not give** `rpm.admin` or a global `*_owner` role to other users. A global owner role
   gives write access to all objects of that type, also the snapshot repositories and the
   Content View distributions.

Assign roles on **Users** (or **Groups**): **R**, then **c** with the role name and an empty
object. Groups are easier to keep correct, for example a `pulp-tui-operators` group with the
operator roles.

RBAC does not stop superusers (`is_superuser`): they can always change everything. Keep their
number small. RBAC protects only the Pulp API; it has no effect on who can download served
content (use content guards for that).

## Command line

```
pulp-tui [--config FILE] [--profile NAME] [--data-dir DIR]     start the TUI
pulp-tui check                       test a profile non-interactively
pulp-tui export-cv [-o FILE]         export Content View definitions (JSON, no secrets)
pulp-tui import-cv FILE              import them (existing names are kept)
pulp-tui db migrate                  create or upgrade the schema of the profile's database
pulp-tui --profile NAME db copy [--from PROFILE|local]
                                     copy Content View data into the database of NAME, from
                                     the local file (default) or the database of another
                                     profile (never merges)
pulp-tui config-example              print an example config
```

`export-cv`, `import-cv`, `db migrate` and `db copy` use the database of the selected
profile (`--profile`, `$PULP_TUI_PROFILE`, `default_profile`): its own `database`, else
`[database]`, else the local file. `db migrate` on a profile that uses the local file stops
with `profile <name> uses the local file; db migrate is only for an external database`.

The export file has `"format": 2`. It carries composite views, their components (by view
name), the auto publish flag, and the label and protection of Library (`prior` empty; an
import uses them only when it creates Library). A pin is written as a version number, but versions are not
exported, so `import-cv` (and **I** on Servers) imports every pin as `latest` and prints a
line for each: `warning: rocky9: component os was pinned to v1; imported as latest` (the TUI
shows one warning notice). Pin the component again with **e**. A component that the import
does not find, or that is itself a composite, is skipped with a `warning:` line that names
it. A format 1 file still imports. A pulp-tui from before composites refuses a format 2
file. The database needs schema 4 (`pulp-tui db migrate`) for composites.

Logs: `<data-dir>/pulp-tui.log`; level with `PULP_TUI_LOG` (for example
`PULP_TUI_LOG=pulp_tui=debug`). Secrets are never logged.
