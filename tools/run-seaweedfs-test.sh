#!/usr/bin/env bash
set -euo pipefail

if ! command -v weed >/dev/null 2>&1; then
  echo "weed binary is required for make test-s3-seaweedfs" >&2
  exit 127
fi

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

PORT="${ROPENDAL_SEAWEEDFS_PORT:-$(pick_port)}"
MASTER_PORT="${ROPENDAL_SEAWEEDFS_MASTER_PORT:-$(pick_port)}"
FILER_PORT="${ROPENDAL_SEAWEEDFS_FILER_PORT:-$(pick_port)}"
VOLUME_PORT="${ROPENDAL_SEAWEEDFS_VOLUME_PORT:-$(pick_port)}"
ADMIN_PORT="${ROPENDAL_SEAWEEDFS_ADMIN_PORT:-$(pick_port)}"
DATA_DIR="${ROPENDAL_SEAWEEDFS_DATA_DIR:-$(mktemp -d -t ropendal-seaweedfs-data-XXXXXX)}"
LOG_FILE="${ROPENDAL_SEAWEEDFS_LOG_FILE:-$(mktemp -t ropendal-seaweedfs-log-XXXXXX)}"
CONFIG_FILE="$DATA_DIR/s3-identities.json"
BUCKET="${ROPENDAL_S3_SEAWEEDFS_BUCKET:-ropendal-test}"
ACCESS_KEY="${ROPENDAL_S3_SEAWEEDFS_ACCESS_KEY_ID:-ropendal-test}"
SECRET_KEY="${ROPENDAL_S3_SEAWEEDFS_SECRET_ACCESS_KEY:-ropendal-test-secret}"
REGION="${ROPENDAL_S3_SEAWEEDFS_REGION:-us-east-1}"
ROOT="${ROPENDAL_S3_SEAWEEDFS_ROOT:-ropendal-$(date +%s)-$$}"
ENDPOINT="http://127.0.0.1:${PORT}"
WEED_PID=""

cleanup() {
  set +e
  if [ -n "$WEED_PID" ]; then
    kill "$WEED_PID" >/dev/null 2>&1 || true
    wait "$WEED_PID" >/dev/null 2>&1 || true
  fi
  if [ -z "${ROPENDAL_SEAWEEDFS_DATA_DIR:-}" ]; then
    rm -rf "$DATA_DIR"
  fi
  if [ -z "${ROPENDAL_SEAWEEDFS_LOG_FILE:-}" ]; then
    rm -f "$LOG_FILE"
  else
    echo "seaweedfs log: $LOG_FILE" >&2
  fi
}
trap cleanup EXIT

# SeaweedFS S3 gateway identities: one fixture user with full object access.
cat >"$CONFIG_FILE" <<JSON
{"identities":[{"name":"ropendal-test","credentials":[{"accessKey":"$ACCESS_KEY","secretKey":"$SECRET_KEY"}],"actions":["Admin","Read","Write","List","Tagging"]}]}
JSON

# `weed mini` runs master, volume, filer, admin, and S3 in one process.
weed mini \
  -dir="$DATA_DIR" \
  -ip=127.0.0.1 \
  -ip.bind=127.0.0.1 \
  -master.port="$MASTER_PORT" \
  -filer.port="$FILER_PORT" \
  -volume.port="$VOLUME_PORT" \
  -admin.port="$ADMIN_PORT" \
  -admin.port.legacy=0 \
  -s3.port="$PORT" \
  -s3.config="$CONFIG_FILE" \
  -bucket="$BUCKET" \
  >"$LOG_FILE" 2>&1 &
WEED_PID=$!

# Anonymous requests to the S3 gateway return 403 once it is serving, so any
# HTTP response (not a connection refusal) means the gateway is up.
ready=false
for _ in $(seq 1 120); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "$ENDPOINT/" || true)"
  if [ "$code" != "000" ] && [ -n "$code" ]; then
    ready=true
    break
  fi
  if ! kill -0 "$WEED_PID" >/dev/null 2>&1; then
    echo "seaweedfs exited before becoming ready" >&2
    tail -n 80 "$LOG_FILE" >&2 || true
    exit 1
  fi
  sleep 0.5
done

if [ "$ready" != true ]; then
  echo "seaweedfs did not become ready" >&2
  tail -n 80 "$LOG_FILE" >&2 || true
  exit 1
fi

ROPENDAL_TEST_NETWORK=true \
ROPENDAL_TEST_S3_SEAWEEDFS=true \
ROPENDAL_S3_SEAWEEDFS_ENDPOINT="$ENDPOINT" \
ROPENDAL_S3_SEAWEEDFS_BUCKET="$BUCKET" \
ROPENDAL_S3_SEAWEEDFS_REGION="$REGION" \
ROPENDAL_S3_SEAWEEDFS_ACCESS_KEY_ID="$ACCESS_KEY" \
ROPENDAL_S3_SEAWEEDFS_SECRET_ACCESS_KEY="$SECRET_KEY" \
ROPENDAL_S3_SEAWEEDFS_ROOT="$ROOT" \
R -e "tinytest::test_package('Ropendal', testdir = 'inst/tinytest')"
