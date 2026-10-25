#!/usr/bin/env bash
# Demo data for docs/demo/demo.tape: two RPM repositories, each with a default remote (not synced).
# Find-or-create by name, so it can run again. Needs pulp1 and the fixture http server
# (tests/env/pulp-env.sh up pulp1; tests/env/pulp-env.sh http-up).
set -euo pipefail

API=${PULP_URL:-http://localhost:8080}/pulp/api/v3
AUTH=admin:${PULP_PASSWORD:-password}
UPSTREAM=${PULP_TUI_TEST_UPSTREAM:-http://host.containers.internal:8000}

# find_or_create <collection> <name> <json body>: prints the pulp_href.
find_or_create() {
  local href
  href=$(curl -fsS -u "$AUTH" "$API/$1/?name=$2" | jq -r '.results[0].pulp_href // empty')
  [ -n "$href" ] || href=$(curl -fsS -u "$AUTH" -H 'Content-Type: application/json' -d "$3" "$API/$1/" | jq -r .pulp_href)
  echo "$href"
}

for pair in demo-baseos:rpm demo-updates:rpm2; do
  name=${pair%%:*}
  remote=$(find_or_create remotes/rpm/rpm "$name" "{\"name\":\"$name\",\"url\":\"$UPSTREAM/${pair#*:}/\"}")
  find_or_create repositories/rpm/rpm "$name" "{\"name\":\"$name\",\"remote\":\"$remote\"}" >/dev/null
done
echo "demo repositories ready: demo-baseos demo-updates"
