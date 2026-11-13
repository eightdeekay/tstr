#!/usr/bin/env bash
# Run tstr's live Valkey integration tests against a throwaway Valkey.
#
# Spins Valkey up in Docker (if not already running), points the feature-gated
# live tests at it via TSTR_VALKEY_TEST_URL, and tears it down on exit. One
# command, no manual docker dance.
#
#   scripts/valkey-it.sh            # up → test → down
#   KEEP=1 scripts/valkey-it.sh     # leave the server running afterwards
#   VK_PORT=6390 VK_IMAGE=valkey/valkey:8.1 scripts/valkey-it.sh   # other port / image
#
# The hermetic valkey unit tests run without a server as part of the normal
# `cargo test`; this script only adds the live round-trips.
set -euo pipefail

VK_NAME=tstr-valkey
VK_PORT=${VK_PORT:-6379}
VK_IMAGE=${VK_IMAGE:-valkey/valkey:latest}
VK_URL="valkey://127.0.0.1:${VK_PORT}"
KEEP=${KEEP:-0}

started=0
cleanup() {
  if [ "$started" = 1 ] && [ "$KEEP" != 1 ]; then
    docker rm -f "$VK_NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if docker ps --format '{{.Names}}' | grep -qx "$VK_NAME"; then
  echo "reusing running $VK_NAME"
else
  echo "starting Valkey ($VK_NAME)…"
  docker rm -f "$VK_NAME" >/dev/null 2>&1 || true
  docker run -d --name "$VK_NAME" -p "${VK_PORT}:6379" "$VK_IMAGE" >/dev/null
  started=1

  echo -n "waiting for valkey"
  for _ in $(seq 1 30); do
    if docker exec "$VK_NAME" valkey-cli ping 2>/dev/null | grep -q PONG; then
      echo " ready"
      break
    fi
    echo -n "."
    sleep 1
  done
fi

echo "running live Valkey tests…"
TSTR_VALKEY_TEST_URL="$VK_URL" cargo test --features valkey valkey:: -- --nocapture
