#!/usr/bin/env bash
# Run grizzly-gate against the current working tree locally — a pre-check before
# the code reaches CI. It runs the exact same image CI runs, so a local pass
# means a CI pass for everything except the two things a local run deliberately
# cannot do: cosign signing and image-layer (CVE/SBOM) scanning. Everything else
# — the honest-map check and every per-language + SAST/secret/dependency check —
# runs identically.
#
# From the root of the repo you want to check:
#     /path/to/scripts/grizzly-gate-local.sh
# The default image is pulled from Docker Hub on first run — no build needed.
#
# Override the image with GRIZZLY_GATE_IMAGE (e.g. a pinned tag, or a
# locally-built `grizzly-gate:local` when testing changes to the gate itself).
# Extra args are forwarded to the harness (e.g. --report-dir <dir>); do not pass
# --sign, it has no signing material here.
set -euo pipefail

IMAGE="${GRIZZLY_GATE_IMAGE:-bearflinn/grizzly-gate:latest}"

# Pull on demand if the image isn't already present (a published tag just works;
# a local-only tag like grizzly-gate:local won't pull, which is the signal to
# build it first).
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "grizzly-gate: pulling $IMAGE …" >&2
  if ! docker pull "$IMAGE"; then
    echo "grizzly-gate: could not get image '$IMAGE'." >&2
    echo "  for a local source build:  docker build -t grizzly-gate:local <path-to-grizzly-gate>" >&2
    echo "  then:  GRIZZLY_GATE_IMAGE=grizzly-gate:local $0" >&2
    exit 1
  fi
fi

# Mount the working tree read-write and run from it: the node tsconfig wrapper
# and the report dir are written into the tree (both are gitignorable). No
# --sign / --image — a local pre-check verifies the honest map + every source
# check, and writes grizzly-gate-report/report.json, but never signs.
exec docker run --rm \
  -v "$PWD:/src" \
  -w /src \
  "$IMAGE" \
  --source /src \
  "$@"
