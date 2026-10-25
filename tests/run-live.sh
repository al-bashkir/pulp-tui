#!/usr/bin/env bash
# Run the full test suite against real Pulp containers, one scenario at a time.
# Reuses running containers and built fixtures; starts pulp2 only for replication and
# PostgreSQL only for shared_db; deb registers the test Deb signing service on pulp1.
#
#   tests/run-live.sh            all scenarios
#   tests/run-live.sh repos cv   selected scenarios (repos cv rbac failures replication shared_db deb)
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

scenarios=${*:-repos cv rbac failures replication shared_db deb}
for s in $scenarios; do
  [ "$s" = replication ] && $ENV up pulp2
  [ "$s" = shared_db ] && $ENV pg-up
  [ "$s" = deb ] && $ENV deb-signing-up pulp1
  echo "== live scenario: $s"
  PULP_TUI_LIVE=1 cargo test --test "live_$s" -- --test-threads=1
done
echo "all requested scenarios passed"
