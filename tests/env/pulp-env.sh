#!/usr/bin/env bash
# Reproducible Pulp test environment (pulpcore + pulp_rpm + pulp_file all-in-one image).
#
#   tests/env/pulp-env.sh up   [pulp1|pulp2]   start (reuses a running container)
#   tests/env/pulp-env.sh down [pulp1|pulp2]   stop and remove container + volumes
#   tests/env/pulp-env.sh status
#   tests/env/pulp-env.sh pg-up | pg-down   PostgreSQL for the shared-database tests
#
# pulp1 (port 8080) is the main/downstream server. pulp2 (port 8081) is only
# needed by the replication tests (upstream). Both use admin/password.
# PostgreSQL (pg-up) listens on 127.0.0.1:55432, user and database pulp_tui,
# password $PG_PASSWORD (default pulp_tui_test, a test-only value).
set -euo pipefail

RUNTIME=${CONTAINER_RUNTIME:-podman}
IMAGE=${PULP_IMAGE:-quay.io/pulp/pulp:latest}
ENV_DIR=${PULP_ENV_DIR:-$(cd "$(dirname "$0")/../.." && pwd)/target/pulp-env}
ADMIN_PASSWORD=${PULP_ADMIN_PASSWORD:-password}
PG_IMAGE=${PG_IMAGE:-docker.io/library/postgres:16}
PG_PASSWORD=${PG_PASSWORD:-pulp_tui_test}
PG_NAME=pulp-tui-pg

port_of() { case "$1" in pulp1) echo 8080 ;; pulp2) echo 8081 ;; *) echo "unknown instance $1" >&2; exit 2 ;; esac; }

# The content origin must be reachable from the host (tests fetch served content)
# and, for the upstream, from the other container (replication downloads).
origin_of() {
  case "$1" in
    pulp1) echo "http://localhost:$(port_of pulp1)" ;;
    pulp2) echo "http://host.containers.internal:$(port_of pulp2)" ;;
  esac
}

wait_ready() {
  local port=$1 i
  for i in $(seq 1 120); do
    if curl -fsS -o /dev/null "http://localhost:$port/pulp/api/v3/status/"; then return 0; fi
    sleep 2
  done
  echo "Pulp on port $port did not become ready" >&2
  return 1
}

up() {
  local name=$1 port
  port=$(port_of "$name")
  if [ "$($RUNTIME inspect -f '{{.State.Running}}' "$name" 2>/dev/null || true)" = "true" ]; then
    echo "$name already running on port $port"
    return 0
  fi
  $RUNTIME rm -f "$name" >/dev/null 2>&1 || true
  mkdir -p "$ENV_DIR/$name"
  # Test-only key; recent images no longer generate one.
  cat >"$ENV_DIR/$name/settings.py" <<EOF
CONTENT_ORIGIN = "$(origin_of "$name")"
SECRET_KEY = "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
TOKEN_AUTH_DISABLED = True
EOF
  local extra=()
  # Podman provides host.containers.internal; Docker on Linux needs an explicit mapping.
  [ "$(basename "$RUNTIME")" = docker ] && extra+=(--add-host=host.containers.internal:host-gateway)
  $RUNTIME run --detach --name "$name" --publish "$port:80" "${extra[@]}" \
    --volume "$ENV_DIR/$name:/etc/pulp:Z" \
    --volume "$name-storage:/var/lib/pulp" \
    --volume "$name-pgsql:/var/lib/pgsql" \
    --volume "$name-containers:/var/lib/containers" \
    "$IMAGE" >/dev/null
  wait_ready "$port"
  $RUNTIME exec "$name" pulpcore-manager reset-admin-password --password "$ADMIN_PASSWORD" >/dev/null
  echo "$name ready: http://localhost:$port (admin/$ADMIN_PASSWORD)"
}

down() {
  local name=$1
  $RUNTIME rm -f "$name" >/dev/null 2>&1 || true
  $RUNTIME volume rm -f "$name-storage" "$name-pgsql" "$name-containers" >/dev/null 2>&1 || true
  echo "$name removed"
}

pg_up() {
  if [ "$($RUNTIME inspect -f '{{.State.Running}}' "$PG_NAME" 2>/dev/null || true)" = "true" ]; then
    echo "$PG_NAME already running on port 55432"
    return 0
  fi
  $RUNTIME rm -f "$PG_NAME" >/dev/null 2>&1 || true
  $RUNTIME run --detach --name "$PG_NAME" --publish 127.0.0.1:55432:5432 \
    --env POSTGRES_USER=pulp_tui --env POSTGRES_DB=pulp_tui --env POSTGRES_PASSWORD="$PG_PASSWORD" \
    --volume "$PG_NAME-data:/var/lib/postgresql/data" "$PG_IMAGE" >/dev/null
  # The first start runs a temporary server on the socket only; a TCP answer means the real one is up.
  local i
  for i in $(seq 1 60); do
    if $RUNTIME exec "$PG_NAME" pg_isready -q -h 127.0.0.1 -U pulp_tui -d pulp_tui; then
      echo "$PG_NAME ready: postgres://pulp_tui@127.0.0.1:55432/pulp_tui (password in \$PG_PASSWORD, default pulp_tui_test)"
      return 0
    fi
    sleep 1
  done
  echo "PostgreSQL did not become ready" >&2
  return 1
}

pg_down() {
  $RUNTIME rm -f "$PG_NAME" >/dev/null 2>&1 || true
  $RUNTIME volume rm -f "$PG_NAME-data" >/dev/null 2>&1 || true
  echo "$PG_NAME removed"
}

# Static HTTP server for the upstream fixture repositories (port 8000). Pulp containers
# reach it as http://host.containers.internal:8000/.
http_up() {
  local pidf="$ENV_DIR/http.pid"
  if [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; then echo "fixture http already running"; return 0; fi
  mkdir -p "$ENV_DIR"
  nohup python3 -m http.server 8000 --bind 0.0.0.0 --directory "$ENV_DIR/../fixtures/upstream" >"$ENV_DIR/http.log" 2>&1 &
  echo $! >"$pidf"
  echo "fixture http on :8000"
}
http_down() {
  local pidf="$ENV_DIR/http.pid"
  [ -f "$pidf" ] && kill "$(cat "$pidf")" 2>/dev/null || true
  rm -f "$pidf"
}

cmd=${1:-status}
shift || true
case "$cmd" in
  up) up "${1:-pulp1}" ;;
  down) down "${1:-pulp1}" ;;
  pg-up) pg_up ;;
  pg-down) pg_down ;;
  http-up) http_up ;;
  http-down) http_down ;;
  status) $RUNTIME ps -a --filter name=pulp --format '{{.Names}} {{.Status}} {{.Ports}}' ;;
  *) echo "usage: $0 up|down|status [pulp1|pulp2] | pg-up|pg-down | http-up|http-down" >&2; exit 2 ;;
esac
