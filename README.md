# pulp-tui

A keyboard-driven terminal application to manage one or more **Pulp 3** servers through
the Pulp REST API:

* **RPM, File and Deb repositories**: repositories and remotes (separate objects), sync with
  policy previews, uploads (chunked, digest-verified; not for Deb), content browsing and
  metadata, add/remove/copy content, versions and diffs, publications (Deb: optional signing
  service), distributions (pinned or following latest), rollback by re-pointing,
  served-content verification.
* **Content Views and lifecycle environments** (Katello-inspired, implemented by pulp-tui
  on top of Pulp; not a Pulp or Katello API): filtered multi-repository snapshots,
  numbered immutable versions, promotion along `Library -> Dev -> … -> Prod`, rollback,
  drift detection, checkpointed and resumable workflows. Several operators can share
  them through an optional PostgreSQL database, with a lock per Content View.
* **Replication** from an upstream Pulp: native Upstream Pulp replication (tech preview)
  and a content-sync fallback with dry run and idempotent reruns.
* **Users, groups and roles** with escalation and self-lockout warnings.
* **Task Center**: tracks Pulp tasks with adaptive polling, survives restarts, and
  separates client cancellation from server-side task cancellation.

Safety: TLS is always verified, secrets are referenced (env, file, keyring) and never
stored or logged, read-only profiles, a red banner for production servers, impact
previews and typed confirmations for destructive actions.

![pulp-tui demo: dashboard, sync a repository, browse content and versions, Task Center](docs/demo/demo.gif)

## Disclaimer

This project is written with Claude, the AI assistant by Anthropic, using the Claude Opus
models. The project has unit, mock HTTP and live tests (see "Tests"), but the code can
still contain errors. Read the code and try it on a test Pulp server before you use it on a
production server. You use it at your own risk.

## Build and run

Requires Rust ≥ 1.99 (edition 2024). Linux and macOS.

```sh
cargo build --release
./target/release/pulp-tui config-example > ~/.config/pulp-tui/config.toml   # then edit
PULP_PASSWORD=… ./target/release/pulp-tui --profile lab
```

`--no-default-features` builds without system keyring support (for headless use).

Prebuilt binaries are on the GitHub Releases page: Linux x86_64 and aarch64 (static, with
keyring support) and macOS arm64.

## Container

```sh
podman build -t pulp-tui -f Containerfile .          # or docker build (BuildKit)
podman secret create pulp-pw ./password.txt           # profile: secret = "file:/run/secrets/pulp"
podman run --rm -it --read-only \
  -v ./config.toml:/config/config.toml:ro,Z \
  --secret pulp-pw,target=/run/secrets/pulp,uid=10001,mode=0400 \
  -v pulp-tui-data:/data \
  pulp-tui --profile lab
```

* `-it` is required: the TUI needs a TTY. Without one pulp-tui exits with a message.
* The image (about 30 MB) runs as UID 10001 with a read-only root filesystem.
  `/config/config.toml` is mounted read-only; `/data` holds the SQLite database and the
  log (Content Views, task references). Without a volume, that state is lost at exit.
  With a `[database]` section (or a `database` in the profile) the Content Views live in
  that database instead, and `/data` keeps only the task references and the log. Add
  `--hostname <name>` then: locks and audit rows name the operator as `user@host`, and
  without it the host is a new container ID each run.
* Secrets: a container secret readable by UID 10001 (above), or `-e PULP_PASSWORD` with
  `secret = "env:PULP_PASSWORD"`. A plain bind-mounted file with mode 0600 is not readable
  by the container user under rootless Podman. The container build has no keyring.
* Inside the container, `localhost` is the container. Use the server's real name, or
  `host.containers.internal` for a Pulp on the same host.
* Custom CAs: mount the PEM file and set `ca_cert` in the profile.
* Non-interactive check: `podman run --rm … pulp-tui check --profile lab`.

## Documentation

* [User guide](docs/USER_GUIDE.md): every screen, keys, profiles, workflows.
* [Architecture](docs/ARCHITECTURE.md): layers, native vs application-managed concepts,
  storage, failure recovery, security model.
* [Compatibility](docs/COMPATIBILITY.md): tested versions, detected capabilities, known
  server behaviors, unsupported operations.
* [Status](docs/STATUS.md): verified workflows, test results, limitations and risks.
* [AGENTS.md](AGENTS.md): conventions for contributors and coding agents.

## Tests

```sh
cargo test                     # unit + mock HTTP tests (live tests skip themselves)
tests/run-live.sh              # starts Pulp in Podman/Docker, runs all live scenarios
tests/run-live.sh repos cv     # selected scenarios (repos cv rbac failures replication shared_db deb)
tests/run-live.sh real         # real Debian main and Rocky 9 repositories (needs internet; not in "all")
tests/env/pulp-env.sh up-all     # start pulp1, pulp2, PostgreSQL and the fixture http server
tests/env/pulp-env.sh down-all   # remove all of them (containers and volumes)
```

The live environment uses `quay.io/pulp/pulp` (pulpcore, pulp_rpm, pulp_file, pulp_deb),
local RPM, File and Deb fixtures built with `rpmbuild`/`createrepo_c` (and GNU `ar` and `tar`
for Deb), and a static HTTP server as the upstream repository. A second Pulp instance starts
only for replication tests, and a PostgreSQL 16 container (`127.0.0.1:55432`, test password
`pulp_tui_test`) only for the `shared_db` scenario. The `deb` scenario registers a test-only
Deb signing service (with a test GPG key made in the container) on pulp1.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
