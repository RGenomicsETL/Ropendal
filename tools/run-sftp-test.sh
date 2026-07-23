#!/usr/bin/env bash
set -euo pipefail

case "$(uname -s)" in
  Linux|Darwin) ;;
  *)
    echo "SFTP integration tests require a Unix host" >&2
    exit 1
    ;;
esac

for cmd in docker ssh ssh-keygen python3; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "$cmd is required for make test-sftp" >&2
    exit 127
  fi
done

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

PORT="${ROPENDAL_SFTP_PORT:-$(pick_port)}"
IMAGE="${ROPENDAL_SFTP_IMAGE:-atmoz/sftp:alpine}"
CONTAINER="ropendal-sftp-$$"
WORK_DIR="${ROPENDAL_SFTP_TEST_DIR:-$(mktemp -d -t ropendal-sftp-XXXXXX)}"
KEY_PATH="$WORK_DIR/id_ed25519"

cleanup() {
  set +e
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  if [ -z "${ROPENDAL_SFTP_TEST_DIR:-}" ]; then
    rm -rf "$WORK_DIR"
  else
    echo "SFTP test files: $WORK_DIR" >&2
  fi
}
trap cleanup EXIT

ssh-keygen -q -t ed25519 -N "" -f "$KEY_PATH"

docker run --rm --name "$CONTAINER" -d \
  -p "127.0.0.1:${PORT}:22" \
  -v "${KEY_PATH}.pub:/home/ropendal/.ssh/keys/ropendal.pub:ro" \
  "$IMAGE" \
  "ropendal::1001:1001:upload" >/dev/null

ready=false
for _ in $(seq 1 60); do
  if (echo >/dev/tcp/127.0.0.1/"$PORT") >/dev/null 2>&1; then
    ready=true
    break
  fi
  if ! docker container inspect "$CONTAINER" >/dev/null 2>&1; then
    echo "SFTP fixture exited before becoming ready" >&2
    docker logs "$CONTAINER" >&2 || true
    exit 1
  fi
  sleep 0.25
done

if [ "$ready" != true ]; then
  echo "SFTP fixture did not become ready" >&2
  docker logs "$CONTAINER" >&2 || true
  exit 1
fi

# Keep the fixture's accepted host key out of the caller's known_hosts file.
mkdir -p "$WORK_DIR/home"
HOME="$WORK_DIR/home" \
ROPENDAL_TEST_NETWORK=true \
ROPENDAL_TEST_SFTP=true \
ROPENDAL_SFTP_ENDPOINT="ssh://127.0.0.1:${PORT}" \
ROPENDAL_SFTP_ROOT="upload" \
ROPENDAL_SFTP_USER="ropendal" \
ROPENDAL_SFTP_KEY="$KEY_PATH" \
ROPENDAL_SFTP_KNOWN_HOSTS_STRATEGY="accept" \
R -e "tinytest::test_package('Ropendal', testdir = 'inst/tinytest')"
