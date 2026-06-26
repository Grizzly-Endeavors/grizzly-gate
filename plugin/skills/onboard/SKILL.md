---
description: Onboard a repo to grizzly-gate — write a truthful gate-config.json, document the gate in the repo's CLAUDE.md, and confirm it passes. Use for a new repo or one not yet gated.
disable-model-invocation: true
---

# Onboard a repo to grizzly-gate

Bring the current repo under the gate: declare its layout honestly in `gate-config.json`, document the gate in the repo's `CLAUDE.md`, and verify it passes. Work top-down and confirm each artifact with the user before writing.

The gate is the reviewer: a green gate is what lets code ship without a human reading every diff. It is strict on purpose and fails **closed** — anything it cannot positively verify fails. You are making the repo *honestly* pass, never weakening the gate.

## 1. Survey the repo

Walk the tree and find every project root — the directory holding each adapter's marker:

| language | marker | `tsconfig` |
|---|---|---|
| `rust` | `Cargo.toml` | — |
| `python` | `pyproject.toml` | — |
| `node` | `package.json` | required if the project contains any TypeScript |
| `ansible` | an `ansible/` directory | — |
| `yaml` | a `.yamllint` file | — |

Also scan for **un-adapted code** — any `.go`, `.rb`, `.java`, `.kt`, `.php`, `.cs`, `.c/.cpp`, etc. The gate has no adapter for these and hard-fails on them. If present, tell the user plainly: the repo cannot pass until that code is removed or Ops adds an adapter (a deliberate two-part change, not something this skill does). Do not try to hide it — a hostile tree walk will find it.

## 2. Write `gate-config.json` at the repo root

Declare one project per root found. Example:

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

Rules to honor:

- `version` must be exactly `1`.
- `path` is relative and in-tree (`.` is the repo root; `..` and absolute paths are rejected). The marker must actually exist there.
- A `node` project containing TypeScript **must** set `tsconfig` (relative to that project) — type-aware linting needs it and the gate fails closed without it. A JS-only node project may omit it.
- **No other fields.** The file is parsed with `deny_unknown_fields`; a hoped-for `exclude` / `ignore` / `skip` key is a hard error, not an escape hatch. There is no way to exempt code here — that is the point.
- **The map must match reality.** Every adapter-backed file in the tree must be covered by a declared project, or the gate fails. `ansible` and `yaml` are opt-in markers — declare them to run at a sub-path.

## 3. Document the gate in the repo's `CLAUDE.md`

Add a `## grizzly-gate (CI gate)` section to the repo's `CLAUDE.md` (create the file if missing). Use this template, adapting the run command and paths to the repo:

```markdown
## grizzly-gate (CI gate)

This repo is gated by [grizzly-gate](https://github.com/Grizzly-Endeavors/grizzly-gate): one container image runs every per-language check + scanner and, on a clean pass, cosign-signs the image. The gate is the reviewer — a green gate is what lets code ship without a human reading every diff. It is strict on purpose and fails **closed**: anything it cannot positively verify fails.

**The honest map (`gate-config.json`).** The repo root ships `gate-config.json` declaring which languages live where. It can only *declare*, never weaken a check. A hostile tree walk confirms the declaration, so it must match reality — any adapter-backed file not covered by a declared project fails, and any un-adapted language hard-fails.

**Before you push.** Run the gate locally with `/grizzly-gate:check` (or `grizzly-gate` from the repo root). It runs the exact CI image against your working tree and writes `grizzly-gate-report/report.json`. A local pass means a CI pass for everything except cosign signing and image-layer CVE/SBOM scanning.

**When it fails.** Hand it to the `gate-fixer` agent — it reads the report and fixes violations in this repo's own code or its `gate-config.json`. Never relax a rule, disable a check, or add an ignore/exclude to get past the gate. A lint suppression is a last resort that needs the user's sign-off — prefer refactoring the code so it is not needed.
```

If the repo follows the family convention of keeping the markdown un-hard-wrapped (one line per paragraph/bullet), match it.

## 4. Verify

Run `grizzly-gate` from the repo root. On a clean pass, report it. If it fails, dispatch the **gate-fixer** agent to read `grizzly-gate-report/report.json` and resolve the violations, then re-run until green. Do not hand-wave a failure — the repo is onboarded only when the gate passes.
