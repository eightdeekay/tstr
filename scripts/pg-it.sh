#!/usr/bin/env bash
# Run tstr's live PostgreSQL integration tests against a throwaway Postgres.
#
# Spins Postgres up in Docker (if not already running), points the feature-gated
# live tests at it via TSTR_PG_TEST_URL, and tears it down on exit. One command,
# no manual docker dance.
#
#   scripts/pg-it.sh            # up → test → down
#   KEEP=1 scripts/pg-it.sh     # leave the server running afterwards
#
# The hermetic postgres unit tests run without a server as part of the normal
# `cargo test --features postgres`; this script only adds the live round-trips.
set -euo pipefail

PG_NAME=tstr-postgres
PG_PORT=5432
PG_USER=tstr
PG_PASS=tstr
PG_DB=tstr
PG_URL="postgres://${PG_USER}:${PG_PASS}@127.0.0.1:${PG_PORT}/${PG_DB}"
KEEP=${KEEP:-0}

started=0
cleanup() {
  if [ "$started" = 1 ] && [ "$KEEP" != 1 ]; then
    docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if docker ps --format '{{.Names}}' | grep -qx "$PG_NAME"; then
  echo "reusing running $PG_NAME"
else
  echo "starting Postgres ($PG_NAME)…"
  docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
  docker run -d --name "$PG_NAME" -p "${PG_PORT}:5432" \
    -e POSTGRES_USER="$PG_USER" \
    -e POSTGRES_PASSWORD="$PG_PASS" \
    -e POSTGRES_DB="$PG_DB" \
    postgres:latest >/dev/null
  started=1

  echo -n "waiting for postgres"
  for _ in $(seq 1 30); do
    if docker exec "$PG_NAME" pg_isready -U "$PG_USER" -d "$PG_DB" >/dev/null 2>&1; then
      echo " ready"
      break
    fi
    echo -n "."
    sleep 1
  done
fi

echo "running live Postgres tests…"
TSTR_PG_TEST_URL="$PG_URL" cargo test --features postgres postgres:: -- --nocapture
