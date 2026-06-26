#!/usr/bin/env bash
# PostToolUse(Edit|Write|MultiEdit) guard: flag when an edit adds ANY lint
# suppression — blanket or scoped — so nothing slips past unseen.
#
# Two classes, different stance:
#   [blanket]  #[allow], bare # noqa / # type: ignore, @ts-ignore, eslint-disable.
#              Forbidden — the gate fails closed on these. Must be removed.
#   [scoped]   reasoned forms the gate tolerates: Rust #[expect(..., reason=)],
#              Python # noqa: CODE / # type: ignore[code], TS @ts-expect-error.
#              Still surfaced, because the right default is a code fix, not a
#              suppression.
#
# This is an advisory nudge: it does not revert the edit. It tells the user a
# suppression landed, and tells Claude to refactor rather than suppress and to
# confirm with the user before keeping any suppression. The gate stays the
# authoritative enforcer.
#
# Opt-out: set the plugin's warn_suppression user-config to false, or disable the
# whole plugin for the repo with a .grizzly-gate-disabled marker.
set -euo pipefail

source "${CLAUDE_PLUGIN_ROOT}/hooks/_common.sh"
grizzly_gate_disabled && exit 0

[ "${CLAUDE_PLUGIN_OPTION_WARN_SUPPRESSION:-true}" = "true" ] || exit 0

input="$(cat)"
file="$(printf '%s' "$input" | jq -r '.tool_input.file_path // ""')"
# Only the text this edit introduced: Edit.new_string, Write.content, or each
# MultiEdit edit's new_string.
added="$(printf '%s' "$input" | jq -r '[.tool_input.new_string, .tool_input.content, (.tool_input.edits[]?.new_string)] | map(select(. != null)) | join("\n")')"
[ -n "$added" ] || exit 0

blanket=""
scoped=""
case "$file" in
  *.rs)
    blanket="$(printf '%s\n' "$added" | grep -nE '#!?\[allow\(' || true)"
    # expect WITH a reason is scoped; expect WITHOUT one is not properly scoped.
    scoped="$(printf '%s\n' "$added" | grep -nE '#!?\[expect\(' | grep -E 'reason[[:space:]]*=' || true)"
    no_reason="$(printf '%s\n' "$added" | grep -nE '#!?\[expect\(' | grep -vE 'reason[[:space:]]*=' || true)"
    blanket="$(printf '%s\n%s\n' "$blanket" "$no_reason" | grep -vE '^$' || true)"
    ;;
  *.py)
    blanket="$(printf '%s\n' "$added" | grep -nE '#[[:space:]]*(noqa([[:space:]]*$|[^:])|type:[[:space:]]*ignore([[:space:]]*$|[^[])|pylint:[[:space:]]*disable|ruff:[[:space:]]*noqa([[:space:]]*$|[^:])|mypy:[[:space:]]*ignore)' || true)"
    scoped="$(printf '%s\n' "$added" | grep -nE '#[[:space:]]*(noqa:[[:space:]]*[A-Z]|type:[[:space:]]*ignore\[)' || true)"
    ;;
  *.ts | *.tsx | *.js | *.jsx | *.mjs | *.cjs)
    blanket="$(printf '%s\n' "$added" | grep -nE 'eslint-disable|@ts-ignore|@ts-nocheck' || true)"
    scoped="$(printf '%s\n' "$added" | grep -nE '@ts-expect-error' || true)"
    ;;
  *.yml | *.yaml)
    blanket="$(printf '%s\n' "$added" | grep -nE 'yamllint[[:space:]]+disable([[:space:]]|$)|ansible-lint.*disable|#[[:space:]]*noqa' | grep -vE 'rule:' || true)"
    scoped="$(printf '%s\n' "$added" | grep -nE 'yamllint[[:space:]]+disable-line[[:space:]]+rule:' || true)"
    ;;
  *)
    exit 0
    ;;
esac

blanket="$(printf '%s\n' "$blanket" | grep -vE '^$' || true)"
scoped="$(printf '%s\n' "$scoped" | grep -vE '^$' || true)"
[ -n "$blanket$scoped" ] || exit 0

report=""
[ -n "$blanket" ] && report="$(printf '%s' "$blanket" | sed 's/^/[blanket] /')"
if [ -n "$scoped" ]; then
  scoped_tagged="$(printf '%s' "$scoped" | sed 's/^/[scoped]  /')"
  report="$(printf '%s\n%s' "$report" "$scoped_tagged" | grep -vE '^$' || true)"
fi

has_blanket=false
[ -n "$blanket" ] && has_blanket=true

jq -n --arg f "$file" --arg r "$report" --argjson blanket "$has_blanket" '{
  hookSpecificOutput: {
    hookEventName: "PostToolUse",
    additionalContext: (
      "You added a lint suppression to " + $f + ":\n" + $r + "\n\n" +
      "Gate policy here: the default is to fix or refactor the code so the suppression is unnecessary, not to suppress the lint. " +
      "[blanket] forms are forbidden — the gate fails closed on them; remove them and resolve the underlying issue. " +
      "[scoped] reasoned forms are tolerated by the gate, but they are not the easy path: before keeping ANY suppression, stop and ask the user to confirm that suppressing — rather than refactoring — is the right call, and that the stated reason is accurate. Do not leave a suppression in place on your own judgment."
    ),
    systemMessage: (
      if $blanket then
        "⚠️ grizzly-gate: blanket lint suppression added to " + $f + " — forbidden (gate fails closed). Claude has been told to refactor instead and to check with you."
      else
        "⚠️ grizzly-gate: scoped lint suppression added to " + $f + " — Claude has been told to prefer a refactor and confirm with you before keeping it."
      end
    )
  }
}'
