#!/usr/bin/env bash
# Shared helpers for grizzly-gate plugin hooks. Sourced, not executed.
#
# Per-repo opt-out: when a repo has a `.grizzly-gate-disabled` file at its root,
# every plugin hook goes silently inert — the "this repo is not the gate's
# business" switch. It disables ALL hooks, including the push guard, regardless
# of their individual toggles. Commit the marker to disable the plugin for the
# whole team, or gitignore it to disable it only for yourself.

# True (exit 0) when the opt-out marker is present in the project root. Hooks run
# with CLAUDE_PROJECT_DIR pointing at that root; fall back to the cwd if unset.
grizzly_gate_disabled() {
  local root="${CLAUDE_PROJECT_DIR:-.}"
  [ -f "$root/.grizzly-gate-disabled" ]
}
