#!/usr/bin/env bash
set -euo pipefail

source "${CLAUDE_PLUGIN_ROOT}/hooks/_common.sh"
grizzly_gate_disabled && exit 0

input="$(cat)"
cmd="$(printf '%s' "$input" | jq -r '.tool_input.command // ""')"

case "$cmd" in
  *git*push*) ;;
  *) exit 0 ;;
esac

if [ "${CLAUDE_PLUGIN_OPTION_BLOCK_PUSH:-false}" != "true" ]; then
  exit 0
fi

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
if "${CLAUDE_PLUGIN_ROOT}/bin/grizzly-gate" >"$log" 2>&1; then
  exit 0
fi

jq -n '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: "grizzly-gate failed — fix the violations in grizzly-gate-report/report.json before pushing. Dispatch the gate-fixer agent or run /grizzly-gate:check to see them."
  }
}'
exit 0
