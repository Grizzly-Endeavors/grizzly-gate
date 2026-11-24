#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

IMAGE="${GRIZZLY_GATE_PUBLISH_IMAGE:-bearflinn/grizzly-gate}"
SHA="$(git rev-parse --short HEAD)"
PLATFORMS="linux/amd64,linux/arm64"
BUILDER="grizzly-gate"
LOCK_FILE=".git/grizzly-gate-publish.lock"
CACHE_DIR=".git/grizzly-gate-buildcache"

exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "grizzly-gate: a publish is already running; skipping this one."
  exit 0
fi

MARKER=".git/grizzly-gate-publish.failed"
on_exit() {
  local rc=$?
  if [ "$rc" -eq 0 ]; then
    rm -f "$MARKER"
  else
    printf 'grizzly-gate: PUBLISH FAILED (exit %s) at %s\n' "$rc" "$(date -Is)" >&2
    printf 'FAILED (exit %s) at %s\n' "$rc" "$(date -Is)" >"$MARKER"
    if command -v notify-send >/dev/null 2>&1; then
      notify-send -u critical "grizzly-gate publish failed" \
        "exit $rc — see .git/grizzly-gate-publish.log" >/dev/null 2>&1 || true
    fi
  fi
}
trap on_exit EXIT

if ! docker info >/dev/null 2>&1; then
  echo "grizzly-gate: docker is not available; cannot publish." >&2
  exit 1
fi

if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  echo "grizzly-gate: creating buildx builder '${BUILDER}'"
  docker buildx create --name "$BUILDER" --driver docker-container >/dev/null
fi

if ! docker buildx inspect "$BUILDER" --bootstrap 2>/dev/null | grep -qi 'linux/arm64'; then
  echo "grizzly-gate: arm64 emulation not available. Install it once with:" >&2
  echo "  docker run --privileged --rm tonistiigi/binfmt --install arm64" >&2
  exit 1
fi

echo "grizzly-gate: building+pushing ${IMAGE}:{latest,${SHA}} for ${PLATFORMS}"
docker buildx build \
  --builder "$BUILDER" \
  --platform "$PLATFORMS" \
  --cache-from "type=local,src=${CACHE_DIR}" \
  --cache-to "type=local,dest=${CACHE_DIR},mode=max" \
  -t "${IMAGE}:latest" \
  -t "${IMAGE}:${SHA}" \
  --push \
  .

echo "grizzly-gate: published ${IMAGE}:latest (${SHA}) [${PLATFORMS}]"
