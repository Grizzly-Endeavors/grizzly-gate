#!/usr/bin/env bash
# Build and push the grizzly-gate image to Docker Hub for local-developer
# distribution. Safe to run manually or from the pre-push hook (it serializes
# concurrent runs with a lock, so two quick pushes won't build at once).
#
# This is the DEV-distribution image only — it signs nothing. The authoritative,
# signed image is built in-cluster (Argo + rootless BuildKit) and pushed to zot.
#
# MULTI-ARCH: builds linux/amd64 + linux/arm64 so the image runs natively on
# Apple Silicon Macs (not just under emulation). buildx builds both legs and
# pushes the manifest list atomically; a multi-arch image can't be loaded into
# the local docker engine, so this builds straight to the registry (--push).
# The amd64 leg is byte-identical to the in-cluster (BuildKit) build — local pass
# == CI pass. The arm64 leg is cross-built via QEMU on an amd64 host, so it's
# slower; that's fine here (the pre-push hook backgrounds this, never blocking).
#
# One-time host setup on a plain-Docker box (Docker Desktop already has it):
#   docker run --privileged --rm tonistiigi/binfmt --install arm64
#
# Override the target repo with GRIZZLY_GATE_PUBLISH_IMAGE (default below).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

IMAGE="${GRIZZLY_GATE_PUBLISH_IMAGE:-bearflinn/grizzly-gate}"
SHA="$(git rev-parse --short HEAD)"
PLATFORMS="linux/amd64,linux/arm64"
BUILDER="grizzly-gate"
LOCK_FILE=".git/grizzly-gate-publish.lock"
# Durable layer cache (mode=max caches intermediate stages too). The
# docker-container builder keeps an internal cache, but it can be GC'd or lost
# when the builder is recreated; this on-disk cache survives that, so a publish
# that only changed harness/ or config/ reuses the slow emulated tool-install
# layers instead of rebuilding them under QEMU. Lives in .git (untracked).
CACHE_DIR=".git/grizzly-gate-buildcache"

# Serialize: if a publish is already in flight, don't start a second build.
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "grizzly-gate: a publish is already running; skipping this one."
  exit 0
fi

# Surface failures. The pre-push hook backgrounds this and always exits 0, so a
# broken publish (e.g. a missing arm64 emulator) otherwise rots unnoticed for
# days. On any non-zero exit past this point, drop a persistent marker the next
# push will warn on, and fire a desktop notification. Clear the marker on a
# clean publish. (Installed after the lock-skip so a skipped run touches nothing.)
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

# A docker-container builder is required for multi-platform builds (the default
# "docker" driver can't emit a manifest list). Create it once; reuse thereafter.
if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  echo "grizzly-gate: creating buildx builder '${BUILDER}'"
  docker buildx create --name "$BUILDER" --driver docker-container >/dev/null
fi

# Confirm the arm64 emulator is registered, else the arm64 leg fails cryptically.
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
