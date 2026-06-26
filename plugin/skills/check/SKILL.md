---
description: Run the grizzly-gate local pre-check against the current repo and report the verdict.
---

# grizzly-gate local pre-check

Run the gate against the current working tree and report the result. Prefer the **`gate` MCP tools** (provided by this plugin) over shelling out, because they keep each failing check's (potentially huge) output out of context until you ask for it. Fall back to the `grizzly-gate` CLI only if the MCP server is unavailable.

1. Call the **`run_gate`** tool. It runs the gate and returns a *compact* verdict only: `verdict`, `failed_phase`, `checks_total`, `checks_failed`, `failing_check_labels`, and any `honest_map_violations` (class/language/path). It deliberately does **not** include raw output.
2. On a clean pass (`verdict: "pass"`), say so plainly — a green local gate means CI will pass everything except cosign signing and image-layer CVE/SBOM scanning, which a local run cannot do.
3. On failure, work from the compact verdict and pull detail only where needed:
   - **`failed_phase: "honest-map"`** — call **`list_honest_map_violations`** for the full `{class, language, path, reason}` of each. Classes: `malformed-declaration`, `undeclared`, `unsupported`, `ts-without-tsconfig`. The fix is in the repo's `gate-config.json` or by removing/adapting the offending code.
   - **`failed_phase: "checks"`** — for each label in `failing_check_labels`, call **`get_check_output`** with that `label` to read its output. It is paginated: start at `offset_lines: 0`, and if `has_more` is true, fetch the next page (`offset_lines` += `returned_lines`) rather than asking for one giant blob. Read only the labels you're actually fixing.

To recall the most recent verdict without re-running, use **`get_report_summary`**.

If invoked as a CLI fallback: run `grizzly-gate` (on PATH while this plugin is enabled; it forwards extra args, so `$ARGUMENTS` passes straight through), then read `grizzly-gate-report/report.json` on failure.

Do not relax any rule, disable a check, or edit the gate's config to make a repo pass — the fix is always in the scanned repo's own code or its `gate-config.json` declaration. If the work is more than a couple of fixes, hand it to the `gate-fixer` agent.
