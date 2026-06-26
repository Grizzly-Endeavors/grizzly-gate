#!/usr/bin/env bash
# Run grizzly-gate against the current working tree locally — a pre-check before
# the code reaches CI. A local pass means a CI pass for everything except the two
# things a local run deliberately cannot do: cosign signing and image-layer
# (CVE/SBOM) scanning. Everything else — the honest-map check and every
# per-language + SAST/secret/dependency check — runs identically.
#
# From the root of the repo you want to check:
#     /path/to/scripts/grizzly-gate-local.sh
# The default image is pulled from Docker Hub on first run — no build needed.
# Override with GRIZZLY_GATE_IMAGE; extra args forward to the harness.
#
# This is a thin shim over the plugin's bin wrapper, which is the single source
# of truth for the docker-run invocation (plugin/bin/grizzly-gate).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/../plugin/bin/grizzly-gate" "$@"
