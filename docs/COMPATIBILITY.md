# Compatibility and capability matrix

## Tested versions

All live tests (`tests/run-live.sh`) passed on 2026-10-25 against the
`quay.io/pulp/pulp:latest` all-in-one image:

| Component | Version |
|---|---|
| pulpcore | 3.123.0 |
| pulp_rpm | 3.39.3 |
| pulp_file | 3.123.0 |
| pulp_deb | 3.11.1 |
| PostgreSQL (optional shared database) | 16.15 (`docker.io/library/postgres:16`), through sqlx 0.8.6 |
| Client OS | Fedora 44 (Linux 7.2), Rust 1.99 |

Other versions are not tested. pulp-tui detects capabilities at runtime and shows a
precise "not supported by this server: …" message when a screen or action needs
something the server does not have.

## Runtime detection

| Feature | Detected by | When missing |
|---|---|---|
| RPM screens | `status/` component `rpm` | screen shows "not supported: pulp_rpm plugin" |
| File screens | `status/` component `file` | same for pulp_file |
| Deb screens | `status/` component `deb` | same for pulp_deb |
| Deb Content View signing (`signing_service`, `signing_service_release_overrides`) | the field is in the JSON of the derived repository | the field is not sent; a chosen signing service stops the publish with "not supported by this server" |
| RPM copy (Content Views, copy action) | OpenAPI path `rpm/copy/` | action fails with a precise error |
| Chunked upload | OpenAPI `uploads/` | — (large uploads need it) |
| Native replication | pulpcore ≥ 3.23 and OpenAPI `upstream-pulps/`, `{upstream_pulp_href}replicate/` | Replication screen points to the fallback |
| Replication deletion policy | OpenAPI field `UpstreamPulp.policy` | field hidden |
| Distribution filter for replication | field `q_select`, else `pulp_label_select` | matching field shown |
| RPM sync policies | enum `SyncPolicyEnum` | defaults to additive / mirror_complete / mirror_content_only |
| Repository form fields (checksum type, signing services, retain package versions, manifest) | OpenAPI schema fields | field hidden |
| Current user | `login/`, else (pulpcore without it, e.g. 3.63) `users/?username=` with the Basic-auth name | header shows "anonymous": no `login/` and the user cannot read `users/`, or bearer auth |
| Users/groups/roles | OpenAPI `users/`, `roles/` | screen shows not supported |

If the OpenAPI document cannot be read (for example a 403), checks are optimistic and the
server gives the final answer; the dashboard shows a warning.

## Verified behavior (live tests)

| Area | Status |
|---|---|
| RPM: create, upload (single + idempotent repeat), remote, sync (mirror_complete, additive), versions, diff, publish, distribute, served content (metadata + package digest), update, rollback | verified |
| File: create, upload (single request and chunked with progress), remove content (old versions unchanged), sync from manifest, publish, distribute, served manifest, update, rollback, mismatch detection | verified |
| Distribution base path conflicts | refused with 409 |
| Distribution `base_path` PATCH (environment relabel): runs as a task (`ageneral_update`), moves the distribution in place (same href, same publication), the new path answers 200 and the old path 404 as soon as the task ends (no content-cache delay seen); a promotion after the relabel serves at the new label | verified |
| Content Views: multi-repo snapshot, RPM/File filters, advisory filtering, numbered versions, Library auto-binding, promotion path rules, rollback, served-content verification, retention protection, partial failure + retry, resume with re-attach to a submitted task, version and view deletion | verified |
| Deb (`live_deb`): mirror sync of the fixture, filtered structured copy, publication, distribution, served `Release`, `Packages` index and package digest; a publication with the test signing service is reused only for that service and serves `Release.gpg` and `InRelease`; a Deb Content View with `exclude name=*-dbg`, signed with a service, unsigned (`none`), resumed from a checkpoint with the service, promoted to Dev, and used by a composite; a per-release key of the source signs a `same as source` publish and a `none` publish removes it | verified |
| Composite Content Views (`live_cv::composite_content_view_rocky9`): one publication served by two distributions at once (the component's Library distribution and the composite's own), auto publish after the extras view publishes, deleting a composite version or a composite keeps the publications and repositories of its components | verified |
| Native replication: select by `q_select`, nodelete policy, rerun, upstream update | verified |
| Fallback replication: dry run, create, idempotent rerun, upstream update, conflict with native-replicated objects, loop guard, self-replication guard | verified |
| RBAC: 401 on wrong password, 403 without role, no self-escalation, model-level creator role, group object role, role removal | verified |
| Task cancellation (waiting and running), 409 on completed task, TLS errors, unreadable CA bundle | verified |
| Mock HTTP: pagination, foreign `next` links, 401/403/409, malformed JSON, read retries vs mutation non-retry, 429 Retry-After, timeouts, unreachable server, task polling, old-server capability negotiation, read-only blocking, size limits | verified |
| Shared database on PostgreSQL 16 (`live_shared_db`): explicit schema creation (`db migrate`), refusal of an uninitialized, older and newer schema, two operators on one database, lock conflict and release, 8 simultaneous acquisitions of a free lock (exactly one wins), every store method, composite views (pins, borrowed rows, the foreign keys that refuse deleting a component, `db copy` of a composite), `db copy` from a local file, a profile with its own database next to a local profile, the migration of schema 2 rows to schema 3 (labels `library`/`dev`/`dev` for `Library`/`Dev`/`dev`, a served path kept, a used label refused) | verified |
| Shared database with a role that has only `SELECT/INSERT/UPDATE/DELETE` (manual): import, export, `db migrate` on a current schema | verified |
| Shared database TLS: `ca_cert` against a server with a private CA, connected through a non-loopback address (`verify-full`), see "Database TLS" | verified once (manual) |

## Known server behaviors that affect operators

* **Publication grace period.** pulpcore keeps serving files of a previously distributed
  publication for `DISTRIBUTED_PUBLICATION_RETENTION_PERIOD` (default 3 days). After a
  rollback, repository metadata (`repomd.xml`, `PULP_MANIFEST`) switches immediately, but
  direct URLs of files that exist only in the newer publication can still answer 200
  during this period. pulp-tui verifies rollbacks through metadata and says so in the
  rollback preview.
* **Deleting a served publication or repository version.** In pulpcore 3.123.0
  (`pulpcore/app/models/publication.py`, read in the test container),
  `Publication.repository_version` is `on_delete=CASCADE`: deleting a repository version
  deletes its publications. `Distribution.publication` is `on_delete=SET_NULL`: deleting a
  publication does not fail and does not warn, the distribution silently stops serving
  (it is not refused when a distribution serves it). Because of this, pulp-tui does not
  delete a served object. One publication may be served by several distributions: a
  composite Content View serves the publication of its component under its own path while
  the component serves it too. `delete_version` keeps a publication (and its repository
  version) that any binding of the server still serves, and `delete_view` refuses to delete
  the derived repository of a publication that another view still serves.
* **Protected repository versions.** pulpcore 3.123.0 refuses to delete a repository
  version with HTTP 400 ("currently being used to distribute content";
  `RepositoryVersionViewSet.destroy`, `Repository.protected_versions()`) when a
  distribution has `repository_version` = the version, or `publication` = one of its
  publications, or follows the repository (`repository`) and: with `checkpoint: true`, the
  version has a checkpoint publication; with `checkpoint: false`, the version is the
  highest-numbered version that has a complete publication (File, RPM and Deb distributions
  serve from publications). So the latest version without a publication can be deleted,
  and an older published version that nothing pins can be deleted too. The version delete
  preview applies the same rules (`ops::protecting`) and does not offer the delete for a
  protected version. `checkpoint` on publications and distributions is new in pulpcore
  (migration 0131); older servers lack it and pulp-tui reads it as `false`.
* **mirror_complete** RPM syncs create their own publication with upstream package paths.
  Served-content checks therefore try the layout path and the original `location_href`.
  The RPM publication response has no `distributions` field (the File one has), and the
  distribution list has no `publication` filter: pulp-tui lists the distributions of the
  type and matches the bindings in the client before it deletes a publication, a version
  or a repository (checked on 3.123.0 / pulp_rpm 3.39.3).
* **Native replication** pins distributions with `repository_version`, not a publication.
  pulp-tui shows these as pinned and re-points them correctly. A distribution PATCH is
  merged with the stored object (pulpcore `serializers/publication.py`): a body with
  `publication` but without `repository_version: null` gets HTTP 400, and
  `{publication: null, repository: null}` answers 200 but keeps serving the version. Every
  binding change of pulp-tui therefore also sends `repository_version: null`.
* **A distribution that follows a repository** serves the newest complete publication of
  the repository (pulpcore `models/publication.py`: ordered by repository version, then
  creation), not the latest version. Verify compares with that publication.
* **RPM publish settings** (`metadata_signing_service`, `checksum_type`, `repo_config`,
  `compression_type`, `layout`) come from the repository when the publication request does
  not set them (pulp_rpm `viewsets/repository.py`). A Content View publish copies them from
  the source repository to the snapshot repository first.
* **`sha256` of RPM packages and File content** comes from the stored file (pulpcore
  `ContentChecksumSerializer`; read in pulp_rpm 3.39.3 and pulp_file 3.123.0), so it is
  `null` for `on_demand` and `streamed` content until a client downloads it. Verify then
  takes the RPM digest from `pkgId` when `checksum_type` is `sha256` (else it fails), and
  compares a File digest of `PULP_MANIFEST` only where the API has one (the paths always).
* **`PULP_MANIFEST` lines are not quoted** (pulp_file 3.123.0 `manifest.py`): a line is
  `relative_path,sha256,size`, written with `",".join(...)` and read with
  `rsplit(",", maxsplit=2)`. A path can contain commas. Verify reads each line from the
  right too.
* **`rpm/copy/` with a `content` list adds related units** (pulp_rpm 3.39.3
  `tasks/copy.py`, `find_children_of_content`, also without dependency solving): the
  packages and modules that a copied advisory lists, the groups of copied comps categories
  and environments, and the newest package of each group member whose name the request
  does not have. It does not add the packages of a module. The check runs per request, so a
  list sent in batches gets other units than one request. A filtered RPM Content View
  snapshot therefore adds its units with `modify/`, which adds exactly the given units.
* **Task cancellation** is asynchronous. A task blocked in a network read can end as
  `failed` before the worker processes the cancel request; pulp-tui shows Pulp's final
  state. Cancelling a completed task returns 409 and is shown as such.
* `all_tasks_dispatched` of replication task groups stays false in 3.123; see
  `docs/ARCHITECTURE.md` for how completion is detected.
* `api_root` of an Upstream Pulp is relative and needs a trailing slash (`pulp/`).
* **Clearing remote fields.** pulpcore `RemoteNetworkConfigSerializer` (`ca_cert`,
  `client_cert`, `client_key`, `username`, `password`, `proxy_username`,
  `proxy_password`), pulp_rpm `sles_auth_token` and pulp_deb `gpgkey` have
  `allow_null=True`, and the repository `remote` is nullable (3.123.0 OpenAPI). A PATCH
  with `null` unsets the value (`hidden_fields` shows `is_set: false`; checked live for a
  File remote and repository). The remote and repository Edit forms send `null` for `-`
  and for a cleared Remote.

## pulp_deb

Read in the test container (pulp_deb 3.11.1, `pulp_deb/app/`) and in its live OpenAPI:

* The package name field of `content/deb/packages/` is `package` (not `name`). The other
  fields pulp-tui reads are `version`, `architecture`, `relative_path` and `sha256`.
* `sha256` comes from the stored file (pulpcore `ContentArtifactChecksumField`), so it is
  `null` for an `on_demand` package until a client downloads it. Verify therefore takes the
  digest from the `SHA256:` line of the served `Packages` index (what apt checks), and fails
  when the API has a digest that is different.
* `deb/copy/` with `structured: true` also copies the records that a structured publication
  needs: the package-release-component (PRC) units of the copied packages, their release
  components, release architectures and releases (`tasks/copy.py`,
  `find_structured_publish_content`). pulp-tui always sends `structured: true`.
* A structured publication lists every PRC in the version and loads its package by id; it does
  not check that the package is in the version (`tasks/publishing.py`). The `modify/` endpoint
  of an APT repository runs `signed_add_and_remove` (`tasks/signing.py`). Its
  `_prepare_package_removals` adds the PRCs (and source PRCs) of each removed package in the
  base version to the removal. Seen live: removing a package alone took its PRC count from 1
  to 0. pulp-tui still sends the binary-package PRCs of a removed package itself (Content View
  snapshots and Remove content), so a pulp_deb version without that step (not checked) does not
  keep publishing the package. pulp-tui keeps releases, release components and release
  architectures, which all packages of a release share.
* `dependency_solving: true` is not implemented: the task fails with
  `FeatureNotImplementedError`. pulp-tui always sends `false`.
* `dest_base_version: 0` starts the new version from the empty version 0 of the destination
  (the viewset turns the number into the id of that version).
* A Release file is signed with the first service that exists in this order: the signing
  service of the publication, then the release override of the repository
  (`signing_service_release_overrides`), then the signing service of the repository
  (`publishing.py`, `AptRepository.release_signing_service`).
* A PATCH of `signing_service_release_overrides` merges
  (`serializers/repository_serializers.py`, `_update_signing_service_overrides`): only the
  distributions in the body change, a `null` value deletes that override, and `{}` changes
  nothing. So pulp-tui sends `null` for each per-release key that a Content View snapshot
  repository must lose (signing `none` or a service, or a key the source no longer has).
* `AptPublication` defaults are `structured=true` and `simple=false`.
* `signing-services/` is the pulpcore endpoint for all plugins. It has no type filter (only
  `name`, `q`, `pulp_href__in` and similar), so a Deb signing service cannot be told from an
  RPM one by a query.
* A publication signed with the keys of its repository stores `signing_service: null`, the
  same as an unsigned one. A `deb/copy/` with the same content as the latest version makes
  no new version, so two Content View versions with other signing choices can have the same
  derived version. Seen live: after a signed publish, a `none` publish of the same content
  reused the signed publication. So pulp-tui never reuses publications for Deb Content Views:
  each version creates its own (the task href is checkpointed before the wait).
* **Request body limit.** Pulp refuses a request body above Django's
  `DATA_UPLOAD_MAX_MEMORY_SIZE` (2,621,440 bytes, the default in the test image) with HTTP 400
  and an HTML page `Bad Request (400)`, not 413. The log shows
  `django.core.exceptions.RequestDataTooBig`. nginx in the image allows 10 MB
  (`client_max_body_size 10m`). Measured with `deb/copy/` bodies of synthetic package hrefs
  (75 bytes each in that test; pulp-tui writes compact JSON, 74 bytes each):

  | hrefs | body bytes | result |
  |---|---|---|
  | 1 | 372 | 400 with a JSON validation error (body accepted) |
  | 34,900 | 2,617,797 | 400 with a JSON validation error (body accepted) |
  | 35,000 | 2,625,297 | 400 HTML `Bad Request (400)` (body refused) |
  | 60,000 | 4,500,297 | 400 HTML `Bad Request (400)` (body refused) |

  `rpm/copy/` and `modify/` have the same limit. So pulp-tui sends a list in batches of
  at most 2,000,000 bytes of hrefs (`ops::batches`, sized by the longest href), one task
  after the other. A filtered Content View snapshot changes the latest derived version only
  where it differs from the selection: first `modify/` removes the units that are no longer
  selected, then the copy adds the missing ones. Each batch makes a version of the derived
  repository; the last one is the snapshot, and an unchanged selection makes no version. A
  resumed publish does the same from the latest derived version. After the copy, pulp-tui
  deletes the derived versions that no Content View version uses (except version 0).
  pulpcore's `delete_version` merges a deleted version's changes into the next newer
  version, so the content of the other versions does not change, and it refuses a version
  that a distribution serves directly or through a publication (`protected_versions`).
* That merge (`RepositoryVersion._squash`) locks rows with nested `IN` subqueries for the
  units that the deleted version removed and the next version adds again. In a live test
  (pulpcore with PostgreSQL 16), PostgreSQL planned it from statistics older than the new
  versions and ran one delete for more than 8 hours at full CPU; with current statistics the
  same query is a hash join. A snapshot that starts from an empty version and is completed by
  later batches makes exactly that case (every unit leaves and comes back), so pulp-tui never
  builds a snapshot that way. A copy without filters is one request on `dest_base_version: 0`:
  inside one version, pulpcore undoes the removal of a unit that is added again.

## Shared database (PostgreSQL)

### Database TLS and `ca_cert`

pulp-tui uses sqlx 0.8.6 with `tls-rustls-aws-lc-rs`. For the database connection:

* **The trust anchors are the Mozilla roots that are compiled into the binary
  (`webpki-roots`). The operating system trust store is not read.** `ca_cert` (sqlx
  `sslrootcert`) **adds** its certificates to these roots; it does not replace them (source:
  `sqlx-core/src/net/tls/tls_rustls.rs` starts from `certs_from_webpki()` and calls
  `cert_store.add` for each certificate of the file). The addition was read in the source;
  it was not tested against a PostgreSQL server with a publicly signed certificate (none was
  available). This differs from the `ca_cert` of a Pulp profile (reqwest), which adds to the
  system roots. An internal CA that the host already trusts still needs `ca_cert` for the
  database.
* A `ca_cert` file may hold several certificates.
* `sslmode=verify-full` is set by pulp-tui for every host except a Unix socket or a
  loopback address, so the host name in the URL must be in the server certificate (an IP
  address needs an IP entry in the subject alternative names).

Checked on 2026-10-23 (by hand, not part of `tests/run-live.sh`): PostgreSQL 16.15 with
`ssl=on` and a server certificate from a private CA (OpenSSL, SAN `IP:<host-ip>`,
`DNS:<host>`, `DNS:localhost`, `IP:127.0.0.1`), published on the LAN address of the
test host. `pulp-tui db migrate` connected through that non-loopback address:

| `[database]` | Result |
|---|---|
| `ca_cert` = the private CA | connects (`schema 0 -> 2`) |
| `ca_cert` = a PEM file with an unrelated CA first, then the private CA | connects |
| no `ca_cert` | `invalid peer certificate: UnknownIssuer` |
| no `ca_cert`, `SSL_CERT_FILE` = the private CA (OS store would trust it) | `invalid peer certificate: UnknownIssuer` (the OS store is not read) |
| `ca_cert` = an unrelated CA | `invalid peer certificate: UnknownIssuer` |
| `ca_cert` = the private CA, URL host `172.17.0.1` (not in the certificate) | `invalid peer certificate: certificate not valid for name "172.17.0.1"; certificate is only valid for IpAddress(<host-ip>), DnsName("<host>"), DnsName("localhost") or IpAddress(127.0.0.1)` |

The live tests connect to `127.0.0.1` without TLS, so they do not exercise this path.

### Other behaviors

* Loopback hosts keep the `sslmode` of the URL (sqlx default `prefer`).
* The URL parameter `options=-csearch_path%3D<schema>` works; the live tests use it to give
  each test its own schema.
* A role with only `SELECT`, `INSERT`, `UPDATE` and `DELETE` on the tables worked for
  `import-cv` and `export-cv` (they insert Content Views and environments, so identity
  columns need no sequence grant) and for `db migrate` on a current schema. An upgrade needs
  a role that can create tables. The other functions use the same kinds of statements.
* Identifiers are `BIGINT`; PostgreSQL has no `INTEGER` here because sqlx `Any` reads
  integers as `i64`.

## Unsupported or limited

* Plugins other than pulp_rpm, pulp_file and pulp_deb (container, python, …) are not managed.
* pulp_deb: no upload of `.deb` files, no replication (native or fallback), no verbatim
  publications, no dependency solving on copy.
* RPM publication layout `nested_by_digest`: the package probe of served-content checks is
  skipped (metadata is still checked).
* Signing: repository fields for signing services are editable, but pulp-tui does not
  create signing services (that requires server-side scripts).
* Content guards are only shown and set by href; they are not created or replicated.
* Reusable filter sets are not implemented yet. The data model keeps filters per view
  repository, so a later "filter set" table can be referenced without changing published
  versions. A composite Content View has no filters of its own and cannot be a component
  of another composite.
* The fallback replication does not delete destination objects and does not copy Pulp
  metadata; use native replication for full metadata replication.
* Pulp domains: the profile `domain` setting prefixes all API paths. It is implemented but
  was not tested against a domain-enabled server.
* Bearer tokens are sent as given; token refresh (OIDC) is not implemented.
* Windows builds are not tested.
