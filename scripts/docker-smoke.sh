#!/usr/bin/env bash
# Start an image with only its built-in defaults and check that it becomes
# healthy and answers on its published port.
#
#   scripts/docker-smoke.sh IMAGE [EXPECTED_VERSION]
set -euo pipefail

image=${1:?usage: docker-smoke.sh IMAGE [EXPECTED_VERSION]}
expected=${2:-}
name=channelloom-smoke-$$
port=${SMOKE_PORT:-18409}

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- container logs" >&2
    docker logs "$name" 2>&1 | tail -n 80 >&2 || true
  fi
  docker rm -f "$name" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT

docker run -d --name "$name" -p "127.0.0.1:$port:8409" \
  -e AWESOMETV_ADMIN_PASSWORD=smoke-test-password \
  "$image" >/dev/null

user=$(docker inspect -f '{{.Config.User}}' "$name")
[ "$user" = "1000:1000" ] || { echo "image runs as '$user', expected 1000:1000" >&2; exit 1; }

health=starting
for _ in $(seq 1 60); do
  health=$(docker inspect -f '{{.State.Health.Status}}' "$name")
  running=$(docker inspect -f '{{.State.Running}}' "$name")
  [ "$running" = true ] || { echo "container exited" >&2; exit 1; }
  [ "$health" = healthy ] && break
  sleep 2
done
[ "$health" = healthy ] || { echo "container health is '$health' after 120s" >&2; exit 1; }

body=$(curl -fsS "http://127.0.0.1:$port/healthz")
[ "$body" = ok ] || { echo "/healthz returned '$body'" >&2; exit 1; }

version_json=$(curl -fsS "http://127.0.0.1:$port/api/v1/public/version")
echo "version: $version_json"
if [ -n "$expected" ]; then
  python3 -c 'import json,sys; v=json.loads(sys.argv[1])["version"]; sys.exit(0 if v==sys.argv[2] else f"version {v} != {sys.argv[2]}")' \
    "$version_json" "$expected"
fi

discover=$(curl -fsS -H 'Host: tv.example.test:8409' "http://127.0.0.1:$port/discover.json")
case "$discover" in
  *'http://tv.example.test:8409'*) ;;
  *) echo "discover.json did not follow the request host: $discover" >&2; exit 1 ;;
esac

echo "smoke OK: $image"
