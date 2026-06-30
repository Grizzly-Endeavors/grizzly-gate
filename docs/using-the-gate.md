# Using the gate

How to make a repo pass `grizzly-gate`: declare your layout honestly, satisfy the checks, read the failure report, and run the whole thing locally before you push. This is the consumer-facing companion to the design overview in [`README.md`](../README.md), the [coverage & threat model](coverage.md), and [ADR-029](decisions/029-gate-config-honest-map.md).

The gate is the reviewer: a green gate is what lets code ship without a human reading every diff. So it is strict on purpose, and it fails *closed* — anything it cannot positively verify is a failure, never a pass.

## What the gate checks

Every run has two phases, and **phase 1 must pass completely before phase 2 runs at all**:

1. **Honest map** — your repo ships a `gate-config.json` that truthfully maps which languages live where, and the gate independently walks the tree to confirm nothing is hidden or un-checkable. See [the contract](#the-gate-configjson-contract) below.
2. **Checks** — for each declared project the gate runs the pinned per-language adapter (format / lint / type / test), and across the whole repo it runs the always-on scanners: SAST (semgrep), secrets (gitleaks), dependency CVEs (osv-scanner), and filesystem/image vulnerability + SBOM (trivy). The gate forces *its own* config onto every tool — it ignores a repo's own `clippy.toml`, `eslint.config`, `ruff.toml`, etc. for the same kind. You declare; you do not relax.

For exactly which failure modes and vulnerability classes each tool covers, see [coverage.md](coverage.md) and the [coverage matrix](coverage-matrix.md).

## The `gate-config.json` contract

Every gated repo ships this file at its root. It can only ever *declare* — it cannot weaken a single check.

```json
{
  "version": 1,
  "projects": [
    { "language": "rust",   "path": "." },
    { "language": "python", "path": "services/api" },
    { "language": "node",   "path": "web", "tsconfig": "tsconfig.json" }
  ]
}
```

A minimal copy-from starting point lives at [`gate-config.example.json`](../gate-config.example.json).

**Fields:**

- `version` — must be exactly `1`. An unknown version fails closed rather than guessing at an older or newer shape.
- `language` — one of the known adapters: `rust`, `python`, `go`, `node`, `ansible`, `yaml`. Svelte and React ride the `node` adapter (a Svelte/React repo has a `package.json`), so declare them as `node`. Any other name (including an un-adapted code language like `ruby` or `java`) is rejected — it has no checks to run.
- `path` — the project directory, relative and in-tree (`.` is the repo root; `..` and absolute paths are rejected). The adapter's marker must exist there or the declaration is a lie of omission and fails. Markers: `rust` → `Cargo.toml`, `python` → `pyproject.toml`, `go` → `go.mod`, `node` → `package.json`, `ansible` → an `ansible` directory, `yaml` → a `.yamllint` file.
- `tsconfig` — **node only**, and required for any node project that contains TypeScript — including a `.svelte` component with `<script lang="ts">` (type-aware checking needs the type program; the gate fails closed without it). The gate wraps your tsconfig so its module/path resolution is honored while gate strictness is force-overridden — you cannot weaken the type bar. A JS-only node project may omit it.

**Two sharp edges that trip people up:**

- **No unknown fields.** The file is parsed with `deny_unknown_fields`. A hoped-for `exclude`, `ignore`, or `skip` key is a hard parse error, not a silently-ignored escape hatch. There is no way to exempt code from the gate in this file — that is the whole point.
- **The map must match reality.** A separate, hostile tree walk confirms the declaration. It scopes to the clean-checkout content — the git index: tracked files plus untracked-but-not-ignored files, exactly what a fresh `git clone` would hold — so locally-ignored build artifacts (`target/`, `node_modules/`, `dist/`, `.venv/`, scratch files) don't trip the local pre-check, but a **tracked** file is always seen even if it matches a `.gitignore` pattern (you can't `git add -f` code past the gate). Any `.rs`/`.py`/`.go`/TS/JS/`.svelte`/etc. file not covered by a matching declared project fails the gate, and any code in an un-adapted language hard-fails. `ansible` and `yaml` are opt-in markers (a bare `.yml` is data as often as IaC) — declare them to run at a sub-path. (On a non-repo source the walk falls back to scanning the whole filesystem, skipping only an Ops-owned `skip_dirs` list.) See [ADR-036](decisions/036-honest-map-git-listing.md).

## Fixing violations

When phase 1 fails, the gate prints every problem at once (no fix-one-rerun churn) and records each as a structured `violation` in the report. Map the `class` to the fix:

| `class` | What it means | Fix |
|---|---|---|
| `malformed-declaration` | `gate-config.json` is missing, unparseable, the wrong `version`, declares zero projects, or a project that doesn't resolve (unknown language, missing marker, out-of-tree path, bad `tsconfig`). | Correct the declaration. The `reason` field names the exact project and problem. |
| `undeclared` | Adapter-backed code (e.g. a `.py`) exists in the tree but no declared project covers it. | Add a project for it in `gate-config.json`, or remove the code. |
| `unsupported` | Code in a language the gate has no adapter for (Ruby, Java, …). | The gate cannot check it, so it cannot pass. Remove the code, or ask Ops to add an adapter (a deliberate two-part change — see [ADR-029](decisions/029-gate-config-honest-map.md)). |
| `ts-without-tsconfig` | A node project contains TypeScript but declares no `tsconfig` (also fires for a `.svelte` component using `<script lang="ts">`). | Add `"tsconfig": "<path>"` to that project. |

When phase 2 fails, the gate prints a `FAILURES` block replaying each failing check's **distilled** output (structured findings rendered as a compact YAML-style block, or noise-filtered text for tools the gate doesn't parse structurally), then the verdict. The fix is whatever the tool says — and the full, untruncated raw output is always in the report (below).

## The machine-readable report (for agents)

Every run writes `grizzly-gate-report/report.json` (override the directory with `--report-dir`). For each check it carries three views: structured **`findings`** (a normalized `{file, line, col, severity, rule, message}` per diagnostic, for every tool the gate parses — clippy, eslint, ruff, mypy, golangci-lint, semgrep, trivy, osv-scanner, gitleaks, cargo-deny, ansible-lint), the focused **`distilled`** text surface, and the **full, untruncated `output`** (raw combined stdout+stderr — the durable audit record). So an automated fix loop (or a human) can query findings, or pull *one* failing check, instead of scrolling the whole log. Reshaping is presentation-only: `ok`/`exit_code` always come from the tool's process status, never from the parse. The terminal's `FAILURES` block is the only place output is ever truncated, and it always points back here.

Shape (`schema: 2`):

```json
{
  "schema": 2,
  "verdict": "fail",
  "failed_phase": "checks",
  "honest_map": { "ok": true, "violations": [] },
  "checks": [
    { "label": "rust:clippy", "language": "rust", "project": ".",
      "cmd": "cargo clippy ...", "ok": false, "exit_code": 101,
      "duration_secs": 2.1,
      "findings": [
        { "file": "src/main.rs", "line": 42, "col": 9, "severity": "error",
          "rule": "clippy::unwrap_used", "message": "used `unwrap()` ..." }
      ],
      "output": "<full combined stdout+stderr>" }
  ],
  "query_hints": ["..."]
}
```

`findings` is omitted for a text-only tool (tsc, pytest, go test, govulncheck, yamllint, …); `distilled` is omitted when it equals the raw output. Useful `jq` queries (also embedded in the report as `query_hints`):

```sh
# which checks failed
jq -r '.checks[] | select(.ok==false) | .label' grizzly-gate-report/report.json

# the structured findings of one failing check
jq -c '.checks[] | select(.label=="rust:clippy") | .findings[]' grizzly-gate-report/report.json

# the full raw output of one failing check (the durable record)
jq -r '.checks[] | select(.label=="rust:clippy") | .output' grizzly-gate-report/report.json

# every honest-map violation
jq -c '.honest_map.violations[]' grizzly-gate-report/report.json
```

In CI, archive `grizzly-gate-report/report.json` as a build artifact so the complete output survives after the live log scrolls away.

## Running the gate locally

You don't need the full CI + signing flow to check your code — run the exact gate image against your working tree first. A local run does **everything CI does except** cosign signing and image-layer (CVE/SBOM) scanning, which need a built image and signing material. The honest-map check and every per-language + SAST/secret/dependency check run identically, because it's the same image.

The image is published to Docker Hub as **`bearflinn/grizzly-gate:latest`** — no build required; `docker pull`s happen on demand. It's multi-arch (`linux/amd64` + `linux/arm64`), so it runs natively on Apple Silicon and Intel Macs as well as Linux — Docker pulls the variant matching your machine. The wrapper auto-refreshes the floating `:latest` on each run so you don't keep running a stale cached layer; set `GRIZZLY_GATE_PULL=0` to skip the refresh (run cached / offline) or `=1` to force a refresh of any tag. Running `docker run` directly (the command above) does **not** refresh on its own — pull first, or use the wrapper.

**Run it directly** from the root of the repo you want to check:

```sh
docker run --rm -v "$PWD:/src" -w /src bearflinn/grizzly-gate:latest --source /src
```

Or use the wrapper, which does the same and forwards extra args:

```sh
/path/to/grizzly-gate/scripts/grizzly-gate-local.sh
```

Either way the gate runs with no `--sign`/`--image`, writes `grizzly-gate-report/report.json`, and exits non-zero on failure, so it composes into your own pre-commit or CI.

> **Mac/arm64 parity note.** CI builds and runs the amd64 image, so on Apple Silicon the arm64 variant is what you get by default. Every text-level check (lint, format, SAST, secrets, dependency scan) is deterministic across architectures — same pinned tool versions, same verdict. The only place a result can differ is code with architecture-conditional compilation (Rust `#[cfg(target_arch = …)]`, Go `//go:build amd64`), which the compiling checks (clippy, `go vet`/`govulncheck`) evaluate for the host arch. If your repo has arch-gated code and you want byte-exact CI reproduction on a Mac, force the amd64 variant: `DOCKER_DEFAULT_PLATFORM=linux/amd64` (or `docker run --platform linux/amd64 …`) — it runs emulated, exactly as CI does.

**Wire it into pre-commit** — the simplest path consumes this repo's hooks directly (pre-commit pulls the image for you):

```yaml
# .pre-commit-config.yaml in your repo
repos:
  - repo: https://github.com/grizzly-endeavors/grizzly-gate
    rev: <tag>          # pin to a released tag
    hooks:
      - id: grizzly-gate
```

If you'd rather call the wrapper script (e.g. to pin the image via `GRIZZLY_GATE_IMAGE`), use a `repo: local` hook instead:

```yaml
repos:
  - repo: local
    hooks:
      - id: grizzly-gate
        name: grizzly-gate (full gate pre-check)
        entry: scripts/grizzly-gate-local.sh
        language: script
        pass_filenames: false
        always_run: true
```

**Building from source instead** (only needed if you're changing the gate itself): `docker build -t grizzly-gate:local .` from this repo, then run with `GRIZZLY_GATE_IMAGE=grizzly-gate:local`.

**Gitignore the run artifacts.** A local run transiently creates `.grizzly-gate.tsconfig.json` in a node project (cleaned up after the run) and writes `grizzly-gate-report/`:

```gitignore
grizzly-gate-report/
.grizzly-gate.tsconfig.json
```

## The Claude Code plugin

If you work in these repos with Claude Code, the [`grizzly-gate` plugin](../plugin/README.md) wraps all of the above. Add this repo as a marketplace and install it:

```
/plugin marketplace add Grizzly-Endeavors/grizzly-gate
/plugin install grizzly-gate@grizzly-endeavors
```

It adds `/grizzly-gate:onboard` (write a truthful `gate-config.json` and document the gate in a repo's `CLAUDE.md`), `/grizzly-gate:check` (the local pre-check above, on demand), a Sonnet `gate-fixer` agent that reads `report.json` and fixes violations, and opt-in guardrail hooks (block-push-on-failure, plus warnings for missing docker, un-adapted languages, and added lint suppressions). All it needs is `docker` on PATH. See the [plugin README](../plugin/README.md) for the full component and configuration list.
