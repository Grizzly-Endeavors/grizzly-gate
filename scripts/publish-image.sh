#!/usr/bin/env bash
# Build and push the grizzly-gate image to Docker Hub for local-developer
# distribution. Safe to run manually or from the pre-push hook (it serializes
# concurrent runs with a lock, so two quick pushes won't build at once).
#
# This is the DEV-distribution image only — it signs nothing. The authoritative,
# signed image is still built in-cluster (Argo + Kaniko) and pushed to zot.
#
# Override the target repo with GRIZZLY_GATE_PUBLISH_IMAGE (default below).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

IMAGE="${GRIZZLY_GATE_PUBLISH_IMAGE:-bearflinn/grizzly-gate}"
SHA="$(git rev-parse --short HEAD)"
LOCK_FILE=".git/grizzly-gate-publish.lock"

# Serialize: if a publish is already in flight, don't start a second build.
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "grizzly-gate: a publish is already running; skipping this one."
  exit 0
fi

if ! docker info >/dev/null 2>&1; then
  echo "grizzly-gate: docker is not available; cannot publish." >&2
  exit 1
fi

echo "grizzly-gate: building ${IMAGE}:latest and ${IMAGE}:${SHA}"
docker build -t "${IMAGE}:latest" -t "${IMAGE}:${SHA}" .

echo "grizzly-gate: pushing ${IMAGE}:latest and ${IMAGE}:${SHA}"
docker push "${IMAGE}:latest"
docker push "${IMAGE}:${SHA}"

echo "grizzly-gate: published ${IMAGE}:latest (${SHA})"
