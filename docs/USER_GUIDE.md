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
| `:` or `Ctrl-P` | command palette: go to a screen, connect to a server, dismiss finished tasks |
| `?` | help |
| `N` | notification log |
| `q` / `Ctrl-C` | quit (q asks first when client operations still run) |
| `↑ ↓ j k PgUp PgDn g G` | move |
| `Enter` | action menu of the selected item (shows the shortcut letter of each action; `*` marks actions that change the server) |
| `/` | server-side search (empty clears) |
| `s` / `S` | sort by next column / reverse (current page) |
| `n` / `p` | next / previous page (100 items per page) |
| `Space` | mark item (compare, bulk remove or add content) |
| `t` | toggle RPM/File, content type, tracked/server tasks, or task polling in Logs |
| `r` | refresh |
| `Esc` | cancel a loading request, clear marks, or go back |

In forms: `Tab`/`↑↓` move, `Space` toggles a checkbox, `←→` change a choice, `Enter`
goes to the next field (submits on the last), `Ctrl-S` submits, `Ctrl-U` clears a field,
`Esc` cancels. In confirmations: `y`/`Enter` confirms, `n`/`Esc` cancels; destructive
operations require typing the shown name; `↑↓ PgUp PgDn` scroll long previews.

**Pickers.** A field shown as `[ value ▾ ]` picks an existing object (repository, remote,
distribution, role, user, ...) from the server. `Enter` or `Space` opens the list, and
typing a letter opens it with that filter. In the list, type to filter, `↑↓ PgUp PgDn`
move, and `Enter` selects and moves to the next field (submits on the last). In a
multi-select list `Space` marks items and `Enter` takes all marked items (or the
highlighted one if none is marked). `Esc` closes the list and keeps the old value.
`Backspace` on the closed field clears it. If the list cannot be loaded, the popup shows
the error and `Enter` takes the typed text as the value (with no text, it closes the list
and keeps the old value).

### Three kinds of "cancel"

1. **Esc on a loading list** cancels the HTTP request in the client.
2. **Quitting** stops waiting for client operations. Pulp tasks continue on the server and
   are re-attached in the Task Center at the next start.
3. **Tasks → X** asks Pulp to cancel a server task. The Task Center then shows the state
   that Pulp reports (`canceled`, or `failed`/`completed` if the task ended first).

## Screens

### Dashboard
Server health (database, Redis, workers, content apps, storage), the current user,
installed plugins with versions, detected capabilities, counts of repositories,
distributions, content and tasks, and the number of Content Views and environments (it
says if they are in the local file or in the shared database). `r` refreshes.

### Servers
Named profiles from the config file. Actions: **o** connect, **T** test connection (shows
user, content origin, plugin versions), **c**/**e** create/edit, **d** remove from the
config (after typing the name; Pulp is not changed, and the Content View definitions of the
server stay in the local file, or in the shared database when `[database]` is set),
**K** store a password/token in the system keyring, **E**/**I** export/import Content
View definitions (with a shared database, import writes to it, so a read-only profile
cannot import). See "Server profiles" and "Shared database" below.

### RPM Repositories / File Repositories
Columns: name, latest version, default remote, retention, description. Details explain
retention and mark Content View snapshot repositories.

| Key | Action |
|---|---|
| v | versions of the repository |
| o | browse content of the latest version |
| f | sync: choose remote (empty = default) and policy (RPM: additive, mirror_complete, mirror_content_only; File: additive, mirror). Mirror policies show a warning first. |
| H | sync history (tasks that touched this repository) |
| u | upload a local file (RPM package or File content with relative path); large files use chunked upload with progress; the server digest is verified; failed chunked uploads are deleted |
| P | publish the latest version (reuses an existing publication) |
| D | distribute: pin the latest version (publication) or follow the repository latest |
| C | copy content from another repository (filter rules; RPM can copy dependencies) |
| c / e / d | create / edit / delete (delete shows versions, publications, dependent distributions and Content View snapshots first) |
| i | full JSON |

Edit sends only changed fields. Retention cannot be set on Content View snapshot
repositories. Sync, upload and copy are refused on snapshot repositories.

### Remotes
`t` switches RPM/File. Create/edit: URL (File: URL of `PULP_MANIFEST`), download policy,
TLS validation, CA/client certificate and key (file paths; content is uploaded), username
and password (write-only: empty keeps the server value), proxy, concurrency, rate limit,
labels, SLES token (RPM). Delete lists repositories that use the remote.

### Repository Versions
From a repository (**v**) or for all repositories. Columns: number, created, present /
added / removed counts. Actions: **o** content, **P** publish this version, **D**
distribute it, **b** point a distribution to it (rollback, with preview), **m** compare
with the marked version (`Space` on one, `m` on the other; shows added/removed NEVRAs or
files, older → newer), **d** delete (refused for Content View snapshots; never needed for
rollback).

### Content browser
Opened from a repository or version. `t` cycles the RPM content types (packages,
advisories, modulemds, package groups, distribution trees, repo metadata files). `/`
searches package names or file paths. **i** shows all metadata of a unit (requires,
provides, files, advisory package lists, …). **x** removes the marked (or selected)
units from the repository; **a** adds them to another repository. Both create a new
repository version; old versions are unchanged.

### Publications
`t` switches RPM/File. **c** create from repository name and optional version number,
**D** distribute, **d** delete (warns when distributions serve it).

### Distributions
`t` switches RPM/File. Details say whether a distribution is **PINNED** (publication or
repository version) or **FOLLOWS LATEST** (repository). **y** copies the content URL
(OSC 52 terminal clipboard), **V** verifies what is served, **b** re-points to a version
(preview first), **c**/**e** create/edit (name, base path, binding, content guard,
labels), **d** delete.

### Content Views
Managed by pulp-tui (see `docs/ARCHITECTURE.md`). Columns: repositories, latest version,
environments with bound versions and states.

| Key | Action |
|---|---|
| c / e / d | create / edit description / delete (removes the view's distributions and snapshot repositories, never source repositories) |
| a | add repositories: a multi-select picker of RPM and File repositories (those already in the view are not listed) and filter rules; the rules apply to each selected repository, so mixed RPM and File selections need empty rules |
| F | edit the filter rules of one repository |
| x | remove a repository from the definition (published versions are unchanged) |
| w | preview: exact content the draft selects compared with the latest published version |
| P | publish a new version: notes → preview → confirm; the version becomes active in Library |
| v | versions of the view |
| M | promote a version to an environment (preview of every distribution; protected environments require typing their name) |
| b | roll back an environment to an earlier version |
| D | verify all environments (drift check) |
| U | break the lock of the selected view (shows who holds it; you type the view name). See "Shared database". |

Publish, promote, roll back, delete (the view or a version) and *Resume* take a lock on
the view while they run; a second operator (or a second pulp-tui on this host) gets
`Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC` before
anything is sent to Pulp. Preview, compare and the drift check do not lock.

Filter syntax (rules separated by `;`):

```
include name=kernel* arch=x86_64 evr>=5.14-1
exclude name=*-debuginfo
exclude path=isos/*            # File repositories
```

A unit is selected when (no include rule exists or one matches) and no exclude rule
matches. All criteria of one rule must match. `evr` supports `= < <= > >=` with rpm
version comparison; `epoch:` is optional; without a release only epoch and version are
compared.

### Content View versions
**M** promote, **b** roll back, **m** compare with the marked version, **R** resume a
publish that failed or was interrupted (continues from checkpoints and re-attaches to a
submitted task; with a shared database it can also continue a publish that another
operator started), **d** delete (refused while bound), **i** details (source versions,
snapshot versions, publications, unit counts).

### Lifecycle Environments
Library exists automatically. **c** creates an environment after an existing one
(paths can branch). **L** toggles protection. **d** deletes an unused environment. With a
shared database, environments belong to the server and every operator sees them.

### Replication
Native Upstream Pulp objects (tech preview). **c**/**e** create/edit: upstream base URL
as reachable from *this* Pulp server, relative API root (`pulp/`), domain, credentials,
CA/client certificates, TLS validation, distribution filter (`q_select`, e.g.
`name__startswith="rhel9"`), deletion policy (`nodelete` default; `all` needs typed
confirmation). **R** replicates and shows the task group result. **H** shows recent
replication task groups. **F** starts the fallback: choose another configured profile as
source, types, a name filter and additive or mirror mode; a dry run lists
CREATE / SYNC / SKIP per distribution before anything changes; reruns are safe.

### Users, Groups, Roles
Users: **R** roles, **c**/**e** create/edit, **w** set password (Pulp's local users
only), **A** enable/disable, **d** delete (refused for yourself; staff accounts warn).
Role assignments: **c** add (role name, optional object href or domain for scope), **d**
remove; owner/admin roles ask for confirmation. Groups: **o** members, **R** roles,
**c**/**d**. Roles: system roles are locked (read-only); custom roles can be created,
edited (permissions as a comma list) and deleted.

### Tasks
Tracked tasks with state, progress (only what Pulp reports; no invented percentages),
start time and errors. The top box lists client operations still running. **i** details
with all progress reports, **X** cancel on the server (confirmation), **z** dismiss
finished tasks from the list. `t` switches to all server tasks (**a** tracks one).

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
`password`, `token`, `secret`, `client_key` and keys that end in `_password`, `_token`
or `_secret` (for example `proxy_password` and `sles_auth_token`) show as `•••`, and
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
| `keyring` | system keyring, service `pulp-tui`, user = profile name (store it with **K**) |

Auth methods: `basic` (username + secret), `bearer` (secret is the token), `none`
(anonymous or identity from the mTLS client certificate). TLS options: `ca_cert`
(PEM bundle added to the system roots), `client_cert` + `client_key` (mTLS). `proxy`,
`timeout_secs`, `read_only`, `production`.

## Shared database

Without a `[database]` section, Content Views, versions, environments, promotions and
their history live in a SQLite file in the data directory. Only one operator uses it.
With a `[database]` section, several operators use one database and see the same Content
Views, versions, environments, promotions and history. There is no pulp-tui server: each
pulp-tui connects to the database directly. A change of another operator shows after a
refresh (`r`); nothing is pushed.

```toml
[database]
url = "postgres://pulp_tui@db.example.com:5432/pulp_tui"   # or "sqlite:///srv/pulp-tui/cv.db"
secret = "env:PULP_TUI_DB_PASSWORD"                         # env:VAR, file:/path or keyring
ca_cert = "/etc/pki/tls/certs/internal-ca.pem"              # optional, PostgreSQL only
```

* There is one `[database]` for the whole config file, not one per profile.
* The password is never in the URL. A URL with a password is rejected. `secret` is a
  reference, as for profiles. For `keyring` the entry has the service `pulp-tui` and the
  user `[database]`; the **K** key of the Servers screen cannot store it, so use `env:` or
  `file:` unless you create the entry with the system keyring tool. Error messages show the
  URL without the password.
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
* If the database cannot be opened, the Pulp screens still work. The Content View screens
  and actions show `shared database unavailable: <reason>` and do nothing. pulp-tui does not
  use the local file instead, because the state would split. Fix the cause and restart.
  This is also true for a schema that `db migrate` did not create or upgrade. The TUI
  starts and the Content View screens show, for example, `shared database unavailable:
  postgres://pulp_tui@db.example.com:5432/pulp_tui: database schema 1 is older than this
  pulp-tui (2); run pulp-tui db migrate`, or `...: database is not initialized; run
  pulp-tui db migrate`. The command-line Content View commands (`export-cv`, `import-cv`,
  `db copy`) stop with the same message.

### Set up (administrator)

1. Create the PostgreSQL database and a role for the operators.
2. Run `pulp-tui db migrate` with a role that can create tables. It creates the schema the
   first time and upgrades it after you upgrade pulp-tui. A normal start never changes the
   schema. If the schema is older, the Content View screens show `... database schema 1 is
   older than this pulp-tui (2); run pulp-tui db migrate` (see above). If it is newer,
   upgrade pulp-tui. When two administrators migrate at the same time, the second one waits.
3. Give the operators' role only `SELECT`, `INSERT`, `UPDATE` and `DELETE`. After a migration
   that adds tables, grant again (or use `ALTER DEFAULT PRIVILEGES`).

   ```sql
   GRANT USAGE ON SCHEMA public TO pulp_tui_op;
   GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO pulp_tui_op;
   ```

4. Each operator puts the same `[database]` in the config and starts pulp-tui.

### Move the data of one operator

`pulp-tui --profile lab db copy` copies the Content View data of the profile `lab` from the
local file into the shared database: Content Views with repositories and filters, versions
and their checkpoints, environments, bindings and the audit log. The rows get new IDs. The
local profile name becomes the server key (see below). The copy runs in one transaction. It
**never merges**: if the shared database already has a Content View, or an environment
other than Library, with the same name for this server, it stops and writes nothing. Task
references, replication history and locks are not copied. The local file stays as it is. A
read-only profile cannot run `db copy`. Run it once for each server.

After the copy, objects that the local file published get the database ID label of the
shared database on the next publish or promotion (see "Database ID" below). After that, the
old local file cannot change them any more.

### What is shared

| Shared (in `[database]`) | Always local (`<data-dir>/pulp-tui.db`) |
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
* **Base path prefix.** `cv_base_path_prefix` is stored per server the first time a
  promotion uses it. Later promotions use the stored value. If your setting is different,
  the workflow log says so and pulp-tui uses the stored value, so served URLs do not move.
* **Read-only profiles.** With a shared database, creating or editing a Content View,
  adding or removing repositories and filters, and creating, deleting or protecting an
  environment change state that other operators publish from. A read-only profile does not
  show these actions. `import-cv` and `db copy` refuse a read-only profile. Without a
  shared database these actions stay available, because they change only the local file.

### Locks

* Publish, *Resume*, promote, roll back, reconcile (promote again), delete a Content View
  and delete a version take a lock on **one Content View**. A second operator gets
  `Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC` before
  anything is sent to Pulp. The lock is also taken when two pulp-tui processes use one
  local file.
* Preview, compare and the drift check do not lock. Edits (create, add or remove
  repositories, filters, environments) do not lock either: the last write wins.
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

## Command line

```
pulp-tui [--config FILE] [--profile NAME] [--data-dir DIR]     start the TUI
pulp-tui check                       test a profile non-interactively
pulp-tui export-cv [-o FILE]         export Content View definitions (JSON, no secrets)
pulp-tui import-cv FILE              import them (existing names are kept)
pulp-tui db migrate                  create or upgrade the schema of the [database]
pulp-tui --profile NAME db copy      copy that profile's Content View data from the local file
                                     into the [database] (never merges)
pulp-tui config-example              print an example config
```

Logs: `<data-dir>/pulp-tui.log`; level with `PULP_TUI_LOG` (for example
`PULP_TUI_LOG=pulp_tui=debug`). Secrets are never logged.
