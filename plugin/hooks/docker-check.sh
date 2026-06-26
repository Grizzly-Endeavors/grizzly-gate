#!/usr/bin/env bash
# SessionStart guard: warn if docker is missing. The gate runs as a docker
# container, so without docker on PATH none of /grizzly-gate:check, the push
# guard, or the gate-fixer agent can run. Stays silent when docker is present,
# so there is no toggle — it only speaks when something is actually wrong.
# SessionStart cannot block; it only informs.
set -euo pipefail

input="$(cat)"
src="$(printf '%s' "$input" | jq -r '.source // ""')"
# Only on a fresh/resumed/cleared session — not on every compaction mid-work.
case "$src" in startup | resume | clear) ;; *) exit 0 ;; esac

command -v docker >/dev/null 2>&1 && exit 0

jq -n '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    systemMessage: "⚠️ grizzly-gate: docker not found on PATH. The gate runs as a docker container, so /grizzly-gate:check, the push guard, and the gate-fixer agent cannot run until docker is installed and available.",
    additionalContext: "grizzly-gate note: docker is not available on this machine. The gate runs as a docker container — do not attempt to run grizzly-gate or the local pre-check until docker is installed; it will fail."
  }
}'
