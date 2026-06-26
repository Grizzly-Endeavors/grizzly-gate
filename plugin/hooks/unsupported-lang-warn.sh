#!/usr/bin/env bash
# SessionStart guard: warn when the repo contains code in languages the gate has
# no adapter for. The gate fails *closed* on un-adapted code, so such a repo can
# never pass until the code is removed or Ops adds an adapter.
#
# This is a fast heuristic: it matches tracked-file extensions against a baked
# list of common un-adapted languages. The authoritative denylist lives in the
# image's config/detect.toml — a consumer repo cannot read it without running the
# gate, so this never claims to be authoritative. Run /grizzly-gate:check for the
# real verdict. SessionStart cannot block; it only informs.
#
# Opt-out: set the plugin's warn_unsupported user-config to false, or disable the
# whole plugin for the repo with a .grizzly-gate-disabled marker.
set -euo pipefail

source "${CLAUDE_PLUGIN_ROOT}/hooks/_common.sh"
grizzly_gate_disabled && exit 0

[ "${CLAUDE_PLUGIN_OPTION_WARN_UNSUPPORTED:-true}" = "true" ] || exit 0

input="$(cat)"
src="$(printf '%s' "$input" | jq -r '.source // ""')"
# Only on a fresh/resumed/cleared session — not on every compaction mid-work.
case "$src" in startup | resume | clear) ;; *) exit 0 ;; esac

files="$(git ls-files 2>/dev/null || true)"
[ -n "$files" ] || exit 0

# Common code languages with no gate adapter (supported: rust, python, go, node
# — incl. svelte/react under the node adapter — ansible, yaml). Keep in rough
# sync with config/detect.toml; the image is authoritative.
unsupported="$(printf '%s\n' "$files" \
  | grep -iE '\.(rb|java|kt|kts|swift|php|cs|c|h|cc|cpp|cxx|hpp|hh|scala|ex|exs|clj|cljs|hs|pl|pm|lua|dart|zig|m|mm|groovy|erl)$' \
  || true)"
[ -n "$unsupported" ] || exit 0

exts="$(printf '%s\n' "$unsupported" | sed -E 's/.*\.([^.]+)$/\1/' | tr 'A-Z' 'a-z' | sort -u | paste -sd', ' -)"
count="$(printf '%s\n' "$unsupported" | grep -c . || true)"

jq -n --arg e "$exts" --arg n "$count" '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    systemMessage: ("⚠️ grizzly-gate: this repo has " + $n + " file(s) in languages the gate has no adapter for (" + $e + "). The gate fails closed on un-adapted code, so it cannot pass until that code is removed or an adapter is added. Heuristic — run /grizzly-gate:check for the authoritative verdict."),
    additionalContext: ("grizzly-gate note: " + $n + " file(s) in un-adapted languages (" + $e + ") are present in this repo. The gate fails closed on these — do not add more code in unsupported languages, it cannot be checked and will block the gate. Adding an adapter is a deliberate Ops change, not a workaround.")
  }
}'
