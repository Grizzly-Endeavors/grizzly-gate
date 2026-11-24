#!/usr/bin/env bash

grizzly_gate_disabled() {
  local root="${CLAUDE_PROJECT_DIR:-.}"
  [ -f "$root/.grizzly-gate-disabled" ]
}
