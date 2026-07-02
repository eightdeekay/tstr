#!/usr/bin/env bash
# Run tstr's live Kafka integration tests against a throwaway Redpanda broker.
#
# Spins Redpanda up in Docker (if not already running), points the feature-gated
# live tests at it via TSTR_KAFKA_TEST_BROKER, and tears it down on exit. One
# command, no manual docker dance.
#
#   scripts/kafka-it.sh            # up → test → down
#   KEEP=1 scripts/kafka-it.sh     # leave the broker running afterwards
#
# The hermetic kafka unit tests run without a broker as part of the normal
# `cargo test --features kafka`; this script only adds the live round-trips.
set -euo pipefail

BROKER_NAME=tstr-redpanda
BROKER_ADDR=127.0.0.1:9092
KEEP=${KEEP:-0}

started=0
cleanup() {
  if [ "$started" = 1 ] && [ "$KEEP" != 1 ]; then
    docker rm -f "$BROKER_NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if docker ps --format '{{.Names}}' | grep -qx "$BROKER_NAME"; then
  echo "reusing running $BROKER_NAME"
else
  echo "starting Redpanda ($BROKER_NAME)…"
  docker rm -f "$BROKER_NAME" >/dev/null 2>&1 || true
  docker run -d --name "$BROKER_NAME" -p 9092:9092 -p 9644:9644 \
    redpandadata/redpanda:latest \
    redpanda start --mode dev-container --smp 1 --default-log-level=warn \
    --kafka-addr PLAINTEXT://0.0.0.0:9092 \
    --advertise-kafka-addr PLAINTEXT://127.0.0.1:9092 >/dev/null
  started=1

  echo -n "waiting for broker"
  for _ in $(seq 1 30); do
    if docker exec "$BROKER_NAME" rpk cluster health 2>/dev/null | grep -q 'Healthy:.*true'; then
      echo " ready"
      break
    fi
    echo -n "."
    sleep 1
  done
fi

echo "running live Kafka tests…"
TSTR_KAFKA_TEST_BROKER="$BROKER_ADDR" cargo test --features kafka kafka:: -- --nocapture
