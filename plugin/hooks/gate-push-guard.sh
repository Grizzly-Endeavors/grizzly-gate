#!/usr/bin/env bash
# PreToolUse(Bash) guard: block Claude's `git push` when the local gate fails.
#
# Opt-in: does nothing unless the plugin's `block_push` user-config is on. Only
# fires on a `git push` Bash command — every other Bash call passes through
# untouched. Guards Claude's tool calls only, not a human's manual terminal push.
#
# Decision protocol (Claude Code PreToolUse): exit 0 with a
# hookSpecificOutput.permissionDecision JSON on stdout. "deny" blocks the call
# and hands permissionDecisionReason back to Claude; emitting nothing defers to
# the normal permission flow (i.e. allow).
set -euo pipefail

input="$(cat)"
cmd="$(printf '%s' "$input" | jq -r '.tool_input.command // ""')"

# Allow anything that isn't a git push.
case "$cmd" in
  *git*push*) ;;
  *) exit 0 ;;
esac

# Allow when the toggle is off (the default).
if [ "${CLAUDE_PLUGIN_OPTION_BLOCK_PUSH:-false}" != "true" ]; then
  exit 0
fi

# Run the gate against the project root. Capture output so a green run stays
# quiet and a red run can point Claude at the report.
log="$(mktemp)"
trap 'rm -f "$log"' EXIT
if "${CLAUDE_PLUGIN_ROOT}/bin/grizzly-gate" >"$log" 2>&1; then
  exit 0
fi

# Red gate → deny the push and hand the report path back to Claude.
jq -n '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: "grizzly-gate failed — fix the violations in grizzly-gate-report/report.json before pushing. Dispatch the gate-fixer agent or run /grizzly-gate:check to see them."
  }
}'
exit 0
