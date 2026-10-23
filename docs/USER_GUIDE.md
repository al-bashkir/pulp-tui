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
| `t` | toggle RPM/File, content type, or tracked/server tasks |
| `r` | refresh |
| `Esc` | cancel a loading request, clear marks, or go back |

In forms: `Tab`/`↑↓` move, `Space` toggles a checkbox, `←→` change a choice, `Enter`
goes to the next field (submits on the last), `Ctrl-S` submits, `Ctrl-U` clears a field,
`Esc` cancels. In confirmations: `y`/`Enter` confirms, `n`/`Esc` cancels; destructive
operations require typing the shown name; `↑↓ PgUp PgDn` scroll long previews.

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
distributions, content and tasks, and the number of local Content Views. `r` refreshes.

### Servers
Named profiles from the config file. Actions: **o** connect, **T** test connection (shows
user, content origin, plugin versions), **c**/**e** create/edit, **d** remove from the
config, **K** store a password/token in the system keyring, **E**/**I** export/import
Content View definitions. See "Server profiles" below.

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
| a | add repository: type, name, filter rules |
| F | edit the filter rules of one repository |
| x | remove a repository from the definition (published versions are unchanged) |
| w | preview: exact content the draft selects compared with the latest published version |
| P | publish a new version: notes → preview → confirm; the version becomes active in Library |
| v | versions of the view |
| M | promote a version to an environment (preview of every distribution; protected environments require typing their name) |
| b | roll back an environment to an earlier version |
| D | verify all environments (drift check) |

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
submitted task), **d** delete (refused while bound), **i** details (source versions,
snapshot versions, publications, unit counts).

### Lifecycle Environments
Library exists automatically. **c** creates an environment after an existing one
(paths can branch). **L** toggles protection. **d** deletes an unused environment.

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

## Command line

```
pulp-tui [--config FILE] [--profile NAME] [--data-dir DIR]     start the TUI
pulp-tui check                       test a profile non-interactively
pulp-tui export-cv [-o FILE]         export Content View definitions (JSON, no secrets)
pulp-tui import-cv FILE              import them (existing names are kept)
pulp-tui config-example              print an example config
```

Logs: `<data-dir>/pulp-tui.log`; level with `PULP_TUI_LOG` (for example
`PULP_TUI_LOG=pulp_tui=debug`). Secrets are never logged.
