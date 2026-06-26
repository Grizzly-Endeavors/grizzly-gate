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
- `language` — one of the known adapters: `rust`, `python`, `node`, `ansible`, `yaml`. Any other name (including an un-adapted code language like `go` or `java`) is rejected — it has no checks to run.
- `path` — the project directory, relative and in-tree (`.` is the repo root; `..` and absolute paths are rejected). The adapter's marker must exist there or the declaration is a lie of omission and fails. Markers: `rust` → `Cargo.toml`, `python` → `pyproject.toml`, `node` → `package.json`, `ansible` → an `ansible` directory, `yaml` → a `.yamllint` file.
- `tsconfig` — **node only**, and required for any node project that contains TypeScript (type-aware linting needs the type program; the gate fails closed without it). The gate wraps your tsconfig so its module/path resolution is honored while gate strictness is force-overridden — you cannot weaken the type bar. A JS-only node project may omit it.

**Two sharp edges that trip people up:**

- **No unknown fields.** The file is parsed with `deny_unknown_fields`. A hoped-for `exclude`, `ignore`, or `skip` key is a hard parse error, not a silently-ignored escape hatch. There is no way to exempt code from the gate in this file — that is the whole point.
- **The map must match reality.** A separate, hostile tree walk (it does *not* honor your `.gitignore`) confirms the declaration. Any `.rs`/`.py`/TS/JS/etc. file not covered by a matching declared project fails the gate, and any code in an un-adapted language hard-fails. `ansible` and `yaml` are opt-in markers (a bare `.yml` is data as often as IaC) — declare them to run at a sub-path. Only an Ops-owned `skip_dirs` list (vendor/build/VCS dirs) is skipped.

## Fixing violations

When phase 1 fails, the gate prints every problem at once (no fix-one-rerun churn) and records each as a structured `violation` in the report. Map the `class` to the fix:

| `class` | What it means | Fix |
|---|---|---|
| `malformed-declaration` | `gate-config.json` is missing, unparseable, the wrong `version`, declares zero projects, or a project that doesn't resolve (unknown language, missing marker, out-of-tree path, bad `tsconfig`). | Correct the declaration. The `reason` field names the exact project and problem. |
| `undeclared` | Adapter-backed code (e.g. a `.py`) exists in the tree but no declared project covers it. | Add a project for it in `gate-config.json`, or remove the code. |
| `unsupported` | Code in a language the gate has no adapter for (Go, Ruby, Java, …). | The gate cannot check it, so it cannot pass. Remove the code, or ask Ops to add an adapter (a deliberate two-part change — see [ADR-029](decisions/029-gate-config-honest-map.md)). |
| `ts-without-tsconfig` | A node project contains TypeScript but declares no `tsconfig`. | Add `"tsconfig": "<path>"` to that project. |

When phase 2 fails, the gate prints a `FAILURES` block replaying each failing check's output, then the verdict. The fix is whatever the tool says — and the full, untruncated output is always in the report (below).

## The machine-readable report (for agents)

Every run writes `grizzly-gate-report/report.json` (override the directory with `--report-dir`). It holds the **full, untruncated** output of every check and every honest-map violation — so an automated fix loop (or a human) can pull *one* failing check instead of scrolling the whole log. The terminal's `FAILURES` block is the only place output is ever truncated, and it always points back here.

Shape:

```json
{
  "schema": 1,
  "verdict": "fail",
  "failed_phase": "checks",
  "honest_map": { "ok": true, "violations": [] },
  "checks": [
    { "label": "rust:clippy", "language": "rust", "project": ".",
      "cmd": "cargo clippy ...", "ok": false, "exit_code": 101,
      "duration_secs": 2.1, "output": "<full combined stdout+stderr>" }
  ],
  "query_hints": ["..."]
}
```

Useful `jq` queries (also embedded in the report as `query_hints`):

```sh
# which checks failed
jq -r '.checks[] | select(.ok==false) | .label' grizzly-gate-report/report.json

# the full output of one failing check
jq -r '.checks[] | select(.label=="rust:clippy") | .output' grizzly-gate-report/report.json

# every honest-map violation
jq -c '.honest_map.violations[]' grizzly-gate-report/report.json
```

In CI, archive `grizzly-gate-report/report.json` as a build artifact so the complete output survives after the live log scrolls away.

## Running the gate locally

You don't need the full CI + signing flow to check your code — run the exact gate image against your working tree first. A local run does **everything CI does except** cosign signing and image-layer (CVE/SBOM) scanning, which need a built image and signing material. The honest-map check and every per-language + SAST/secret/dependency check run identically, because it's the same image.

**1. Build the image once** (from a checkout of this repo):

```sh
docker build -t grizzly-gate:local .
```

(Once the image is published to a registry your machine can pull from, skip this and set `GRIZZLY_GATE_IMAGE` to that tag instead.)

**2. Run it** from the root of the repo you want to check:

```sh
/path/to/grizzly-gate/scripts/grizzly-gate-local.sh
```

The wrapper mounts your working tree, runs the gate with no `--sign`/`--image`, and writes `grizzly-gate-report/report.json`. It exits non-zero on failure, so it composes into your own pre-commit or CI.

**3. Wire it into pre-commit** (works today with the locally-built image):

```yaml
# .pre-commit-config.yaml in your repo
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

(Once the image is published, you can instead consume this repo's hosted [`.pre-commit-hooks.yaml`](../.pre-commit-hooks.yaml) via a `repo:`/`rev:` entry.)

**Gitignore the run artifacts.** A local run transiently creates `.grizzly-gate.tsconfig.json` in a node project (cleaned up after the run) and writes `grizzly-gate-report/`:

```gitignore
grizzly-gate-report/
.grizzly-gate.tsconfig.json
```
