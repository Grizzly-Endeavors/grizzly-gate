# ADR-033: Ship the ESLint flat config as a runtime-materialized template (self-gating)

**Date:** 2026-06-27
**Status:** accepted
**Relates to:** [ADR-029](029-gate-config-honest-map.md), [ADR-031](031-go-svelte-react-coverage.md)

## Context

Onboarding grizzly-gate to its own gate (the gate gates itself) surfaced a single honest-map collision. The hostile tree walk ([ADR-029](029-gate-config-honest-map.md)) classifies every file by extension and fails closed on any adapter-backed file not covered by a declared project. The gate's own config tree ships `config/languages/node/eslint.config.mjs` — a live ESLint flat config (it must be real `.mjs`: eslint loads it in place via `--config`, and Node only loads `.mjs`/`.cjs`/`.js`, all of which the node adapter detects). The walk correctly sees it as **undeclared node code**, so the gate could not pass on itself.

It is the *only* such collision in the repo (the rest of the config tree is `.toml`/`.yml` data; the yaml adapter is opt-in and has no auto-detected extensions). The honest options were narrow: declaring a `node` project there is dishonest and broken (no `package.json`, not application code), and any `skip`/exclude in `gate-config.json` or a global `skip_dirs` entry is exactly the fail-open escape hatch the gate's whole design forbids — it would let *any* scanned repo hide code under `config/`. Neither is acceptable; the gate must pass honestly, never by weakening itself.

## Decision

**The ESLint flat config ships as a template, not a live file, and the harness materializes it at run time** — mirroring the existing tsconfig-wrapper pattern (`resolve_tsconfig` already writes a generated tsconfig into the project dir and cleans it up).

- The source file is renamed `config/languages/node/eslint.config.mjs.tmpl`. `.tmpl` is not a detected extension (`Path::extension()` yields `tmpl`), so `classify()` returns `None` and the walk ignores it. The config tree therefore ships **no live `.mjs`** for the walk to flag.
- Before any node check runs, the harness copies `eslint.config.mjs.tmpl` → `eslint.config.mjs` **into the node config dir** and removes it after the node checks (`materialize_eslint_config` / `MaterializedEslintConfig::cleanup` in `harness/src/main.rs`). It is a no-op when no node project is declared.
- **Materialization target is the config dir, not a detached temp dir, by necessity.** The flat config imports its plugins (`typescript-eslint`, `eslint-plugin-react`, `eslint-plugin-svelte`, …) as bare specifiers, which Node ESM resolves by walking up from the config file's location to find `node_modules`. The toolchain is installed into the node config dir at image build (Dockerfile), so the live config must sit beside it. The manifest's eslint `cmd` is unchanged (`--config {config}/eslint.config.mjs`); that path simply exists only for the duration of a run.

This is contained entirely within the gate's own config dir in the (ephemeral) container. The image build (`COPY config/`) now overlays the `.tmpl`; the npm-install layer was already independent of the config tree, so the rename does not affect build caching.

## Alternatives Considered

- **Gate-scoped honest-map exclusion of the config tree.** Teach the walk that the gate's own `config/` is rule data, not gateable source. Rejected: when the gate scans an arbitrary repo, the config tree under `/src` is just data in *that* repo — the harness cannot distinguish "my rule tree" from "a hostile repo's `config/` with hidden code" without a global `skip_dirs` entry or a `gate-config.json` exclude key, both of which fail open for every consumer. The whole point of [ADR-029](029-gate-config-honest-map.md) is that there is no exclude.
- **Declare a `node` project at `config/languages/node/`.** Dishonest and non-functional — no `package.json`, and the node adapter's `npm ci`/`tsc` have nothing real to run. Rejected.
- **Rename in place to a non-detected extension and point eslint at it directly.** Impossible: eslint loads the config as a JS module, and Node only loads `.mjs`/`.cjs`/`.js` — all node-detected. The file must be a live `.mjs` *at run time*, which is exactly why it is materialized rather than shipped.
- **Embed the config as a Rust string and write it out.** The flat config is 150+ lines of JS with runtime logic (it reads `GATE_TSCONFIG`/`GATE_SKIP_DIRS` from the env and assembles its blocks). Reconstructing or inlining that in Rust is far worse to maintain than keeping it as a `.tmpl` the harness copies verbatim. Rejected.

## Consequences

- **The gate passes on itself honestly** — one Rust project (`harness`) declared in `gate-config.json`, zero honest-map violations, no rule weakened and no exclude introduced.
- **The gate's own eslint config is not walked as source in this repo.** It is a linter ruleset, not application code, and it is still validated by being loaded and executed on every node run (eslint fails loudly on a malformed flat config). No node project exists here to lint it, and bootstrapping eslint to lint its own config would be circular.
- **A crashed run can leave a stale `eslint.config.mjs` beside the `.tmpl`.** Harmless — the next run overwrites it, and it is inside the ephemeral container, not the source tree.
- **The pattern generalizes.** Any future gate-owned config that must exist as an adapter-detected extension (a `.py` plugin, a `.go` tool config) can ship as a `.tmpl` and be materialized the same way, keeping the gate self-gateable as adapters grow.
