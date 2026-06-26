---
description: Run the grizzly-gate local pre-check against the current repo and report the verdict.
disable-model-invocation: true
---

# grizzly-gate local pre-check

Run the gate against the current working tree and report the result.

1. From the repo root, run `grizzly-gate` (it is on PATH while this plugin is enabled). It forwards extra args to the harness, so `/grizzly-gate:check $ARGUMENTS` passes `$ARGUMENTS` straight through (e.g. `--report-dir <dir>`).
2. On a clean pass (exit 0), say so plainly — a green local gate means CI will pass everything except cosign signing and image-layer CVE/SBOM scanning, which a local run cannot do.
3. On failure, read `grizzly-gate-report/report.json` and present the problems grouped by phase:
   - **Honest-map** (`failed_phase: "honest-map"`): each entry in `.honest_map.violations[]` has a `class` (`malformed-declaration`, `undeclared`, `unsupported`, `ts-without-tsconfig`) and a `reason` naming the exact project and fix.
   - **Checks** (`failed_phase: "checks"`): each failing entry in `.checks[]` where `.ok == false` has a `label`, `cmd`, `exit_code`, and the full untruncated `output`.

Do not relax any rule, disable a check, or edit the gate's config to make a repo pass — the fix is always in the scanned repo's own code or its `gate-config.json` declaration. If the work is more than a couple of fixes, hand it to the `gate-fixer` agent.
