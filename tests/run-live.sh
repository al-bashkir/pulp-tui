#!/usr/bin/env bash
# Run the full test suite against real Pulp containers, one scenario at a time.
# Reuses running containers and built fixtures; starts pulp2 only for replication.
#
#   tests/run-live.sh            all scenarios
#   tests/run-live.sh repos cv   selected scenarios (repos cv rbac failures replication)
set -euo pipefail
cd "$(dirname "$0")/.."

ENV=tests/env/pulp-env.sh
tests/fixtures/build.sh
$ENV up pulp1
$ENV http-up

echo "== unit + mock HTTP tests"
cargo test --lib --test mock_http

scenarios=${*:-repos cv rbac failures replication}
for s in $scenarios; do
  [ "$s" = replication ] && $ENV up pulp2
  echo "== live scenario: $s"
  PULP_TUI_LIVE=1 cargo test --test "live_$s" -- --test-threads=1
done
echo "all requested scenarios passed"
