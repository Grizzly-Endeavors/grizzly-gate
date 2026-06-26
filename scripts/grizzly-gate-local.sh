#!/usr/bin/env bash
# Run grizzly-gate against the current working tree locally — a pre-check before
# the code reaches CI. It runs the exact same image CI runs, so a local pass
# means a CI pass for everything except the two things a local run deliberately
# cannot do: cosign signing and image-layer (CVE/SBOM) scanning. Everything else
# — the honest-map check and every per-language + SAST/secret/dependency check —
# runs identically.
#
# Build the image once (from the grizzly-gate repo root):
#     docker build -t grizzly-gate:local .
# Then, from the root of the repo you want to check:
#     /path/to/scripts/grizzly-gate-local.sh
#
# Override the image (e.g. a published, pinned tag) with GRIZZLY_GATE_IMAGE.
# Extra args are forwarded to the harness (e.g. --report-dir <dir>); do not pass
# --sign, it has no signing material here.
set -euo pipefail

IMAGE="${GRIZZLY_GATE_IMAGE:-grizzly-gate:local}"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "grizzly-gate: image '$IMAGE' not found locally." >&2
  echo "  build it:  docker build -t grizzly-gate:local <path-to-grizzly-gate>" >&2
  echo "  or set GRIZZLY_GATE_IMAGE to a reachable tag." >&2
  exit 1
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
