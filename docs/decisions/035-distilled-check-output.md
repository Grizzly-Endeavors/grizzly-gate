# ADR-035: Distilled, structured check output

**Date:** 2026-06-29
**Status:** accepted

## Context

The gate runs ~19 distinct checker tools and, until now, captured each one's combined stdout+stderr **verbatim** into `report.json`. The terminal `FAILURES` block replayed that raw text (tail-capped), and the MCP `get_check_output` tool line-paginated it. That raw output is noisy in tool-specific ways: clippy buries its diagnostics under crate-compile/download chatter, semgrep's table garbles, cargo-deny dumps duplicate-version warnings, and every tool has a different shape. A human — or, more importantly, an automated fix loop reading over MCP — had to sift decorator text to find the actual findings, and could not query them (by file, severity, rule).

Most of the tools can emit machine-readable JSON; a uniform, queryable findings view is achievable without weakening anything the gate checks.

## Decision

**Add presentation-only output distillation.** For each tool the gate can parse, normalize its output into a uniform findings schema; for the rest, keep (optionally noise-filtered) text. This is layered onto the existing capture — it never changes the verdict.

1. **Normalized findings.** A `Finding` is `{file?, line?, col?, severity?, rule?, message}`. The harness parses each JSON-capable tool's native shape into this schema: clippy, cargo-deny (NDJSON); semgrep, trivy (fs+image), osv-scanner, gitleaks, eslint, ruff, golangci-lint, ansible-lint (single-document JSON); mypy (NDJSON). Each tool's per-severity vocabulary is normalized to `error`/`warning`/`note`; a severity/loc/rule a tool doesn't provide is simply omitted.
2. **Opt-in, self-describing config.** A tool's `manifest.toml` declares an `[output]` block — `parser = "<id>"` (a built-in Rust parser, keyed by id) or text filters (`drop` regexes, `strip_ansi`). No block = unchanged passthrough. See `harness/src/distill.rs` and `harness/src/config.rs::OutputSpec`.
3. **Three views in `report.json` (`schema` 2).** Per check: structured `findings`, the focused `distilled` text surface (rendered at display time for structured tools, so the human format is never frozen into the artifact), and the **full, untruncated raw `output`** — kept verbatim as the durable audit record. MCP `get_check_output` returns findings by default (optionally `severity`-filtered), the distilled text for text tools, or the verbatim output under `raw=true`; the envelope is trimmed to `{mode, total, has_more, findings|output}`.
4. **Verdict safety (the load-bearing invariant).** `ok`/`exit_code` come **solely** from the tool's process status — never from a parse. A parser that cannot read its tool's output fails closed: zero findings **plus a visible marker over the raw text**, never a silent "clean" result. A tool whose JSON mode drops the failing exit code is excluded from the JSON path — **govulncheck** stays text-distilled for exactly this reason (`-json` exits 0). Every wired tool's exit code was verified to still drive the verdict.
5. **Path normalization.** Tools disagree on absolute vs relative paths; `run()` strips the tool's working directory (the project dir for adapters, the source root for scanners) from each finding path, so all tools report repo-relative paths. golangci-lint additionally needs `--path-mode abs` — it otherwise reports paths relative to the (gate-forced, deep-under-`/etc`) config file, yielding `../../../../../src/...`.

## Consequences

- A green→red gate now hands a human or agent **focused, queryable findings** instead of a wall of tool chatter; the fix loop can pull one check's findings (or filter by severity) over MCP without ingesting raw logs.
- The verdict is unchanged and remains exit-code-driven; distillation is strictly presentation. The full raw output is still the durable record in `report.json`, so nothing is lost — a parser regression degrades presentation (with a loud marker), never the pass/fail decision.
- Adding or bumping a tool now includes a small per-tool parser (proven against a real captured-output fixture under `harness/src/distill/fixtures/`) and an `[output]` block. One new dependency: `regex-lite` (the text-filter path).
- Text-only tools (tsc, svelte-check, pytest, go test, govulncheck, cargo test, yamllint, the `fmt`/deps steps) are unaffected — their already line-oriented output passes through, and they can opt into `drop`/`strip_ansi` later if a noise case appears.
