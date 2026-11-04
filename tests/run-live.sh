#!/usr/bin/env bash
# Run the full test suite against real Pulp containers, one scenario at a time.
# Reuses running containers and built fixtures; starts pulp2 only for cv (import-cv) and replication, and
# PostgreSQL only for shared_db; deb and repos register the test Deb and RPM signing
# services on pulp1.
#
#   tests/run-live.sh            all scenarios
#   tests/run-live.sh repos cv   selected scenarios (repos cv rbac failures replication shared_db deb export content admin)
#   tests/run-live.sh real       real Debian main and Rocky 9 upstreams (needs internet; not in "all")
set -euo pipefail
cd "$(dirname "$0")/.."

ENV=tests/env/pulp-env.sh
tests/fixtures/build.sh
$ENV up pulp1
$ENV http-up

# Test-only credentials of the PostgreSQL container (pg-up); override with PG_PASSWORD.
export PULP_TUI_TEST_PG=${PULP_TUI_TEST_PG:-postgres://pulp_tui@127.0.0.1:55432/pulp_tui}
export PULP_TUI_TEST_PG_PASSWORD=${PULP_TUI_TEST_PG_PASSWORD:-${PG_PASSWORD:-pulp_tui_test}}

echo "== unit + mock HTTP tests"
cargo test --lib --test mock_http

scenarios=${*:-repos cv rbac failures replication shared_db deb export content admin}
for s in $scenarios; do
  case $s in cv | replication) $ENV up pulp2 ;; esac
  [ "$s" = shared_db ] && $ENV pg-up
  [ "$s" = deb ] && $ENV deb-signing-up pulp1
  [ "$s" = repos ] && $ENV rpm-signing-up pulp1
  real=; [ "$s" = real ] && real=1
  echo "== live scenario: $s"
  PULP_TUI_LIVE=1 PULP_TUI_REAL=$real cargo test --test "live_$s" -- --test-threads=1
done
echo "all requested scenarios passed"
