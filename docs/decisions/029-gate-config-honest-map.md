# ADR-029: Mandatory `gate-config.json` honest map for the CI gate

**Date:** 2026-06-25
**Status:** accepted
**Amends:** [ADR-028](028-centralized-ci-gate.md)
**Adapted from:** [grizzly-platform ADR-029](https://github.com/Grizzly-Endeavors/grizzly-platform/blob/master/docs/decisions/029-gate-config-honest-map.md).

## Context

The CI gate ([ADR-028](028-centralized-ci-gate.md)) ran its language adapters by **presence of a marker at the source root**: a `Cargo.toml` at the root activated the Rust adapter, a `pyproject.toml` the Python one, and so on. Only the universal scanners (gitleaks, semgrep) ran unconditionally. This left a structural hole, called out as gap #89 in the coverage doc: **scope was presence-based, so anything the marker didn't see went un-adapted.** A repo could evade a whole class of checks without ever touching a lint config — the thing the gate is otherwise hardened against:

- Put the Rust crate in `svc/` so there's no root `Cargo.toml` → clippy/deny/test never run.
- Ship code in a language the gate has no adapter for (Go, Ruby, …) → only the two universal scanners look at it.
- Omit the `.yamllint` marker → no YAML linting.
- A monorepo with nested projects → only whatever the root marker happens to match gets adapted.

The gate forces its own *config* onto each tool so a repo cannot relax the rules — but none of that matters for code the gate never points a tool at. The missing guarantee is **completeness of scope**: a green gate must mean *every line was actually checked*, not *the code at the root was checked*.

A lazy or hostile author (including an LLM taking every shortcut not explicitly denied) reaches for the same master keys to defeat scope: hide code in a subdir, hide it behind `.gitignore`, use an un-adapted language, or exclude the offending path.

## Decision

**Every gated repo must ship a root `gate-config.json` that honestly maps its project layout, and the trusted Rust harness independently verifies that map against the actual tree — failing closed on any mismatch, before any check runs.**

1. **Required declaration.** A `gate-config.json` at the repo root is mandatory. Missing, malformed, wrong-`version`, or zero-`projects` ⇒ fail closed. It is parsed with `deny_unknown_fields`, so a speculative or typo'd key (e.g. a hoped-for `exclude`) is a hard error, never a silent escape hatch. The declaration can only ever *declare* — it cannot relax a single check (the gate still forces its own tool config).

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

2. **Independent verification in the harness, hostile by construction.** The harness walks the whole tree and establishes what is *actually* present, then compares to the declaration. It is deliberately implemented in the trusted harness, not delegated to a repo-influenceable tool, and:
   - does **not** honor the repo's `.gitignore` (a repo cannot ignore its way out of detection — only an Ops-owned `skip_dirs` list of dependency/build/VCS dirs is skipped);
   - does **not** follow symlinks (no tree escape, no loops);
   - matches extensions case-insensitively, and classifies extensionless executables by shebang interpreter.

   Two fatal classes: **undeclared** adapter-backed code (a `.rs` not covered by any declared rust project — coverage is component-wise so `web` does not cover `web2/`) and **unsupported** code (a language with no adapter at all).

3. **Detection rules live in the config tree (Ops-owned), not the harness.** Each `languages/<lang>/manifest.toml` carries a `[detect]` block (the extensions/shebangs that count as mandatory evidence of that language) for the unambiguous *code* languages — rust/python/node. A new Ops-owned `detect.toml` carries `skip_dirs` plus the **denylist** of code languages with no adapter (go, ruby, java, …). `detect.toml` is required: a stripped-down config dir fails closed rather than silently disabling detection.

4. **Unsupported languages hard-fail; there is no repo-side override.** If detection finds code in a language the gate cannot check, the gate refuses to pass — full stop. The only way forward is Ops adding a real adapter under `languages/`. This keeps "green gate = fully checked" literally true.

5. **No repo-controlled exclusions.** The repo cannot exclude paths from detection or checking. Exclusion is the single most powerful evasion vector, so it is closed entirely. The only skips are the Ops-owned `skip_dirs` (dependencies/build artifacts), which are still covered by the whole-tree secret and SAST scanners.

6. **Adapters run per declared project.** Checks run in each declared project's directory rather than only at the root, so a crate in a subdir or a second project in a monorepo is checked exactly where the (verified) map says it lives.

7. **Repo-aware TypeScript without relaxing strictness.** A node project may declare its own `tsconfig` for module/path *resolution*. The harness wraps it: a generated tsconfig `extends` the repo's (inheriting its `paths`/`baseUrl`) while **force-overriding the full `strict` compiler family** locally (which wins over `extends`), and relies on tsc's default `include` so the repo cannot shrink the typechecked set. The repo gets its layout honored; it cannot weaken the type bar. Without a declared `tsconfig`, the gate's strict `tsconfig.base.json` is used as before.

   *(Extended in gate v0.4.0:)* the same wrapped tsconfig also drives **type-aware ESLint** (`typescript-eslint` `strictTypeChecked` + `stylisticTypeChecked` — `no-floating-promises`, `no-unsafe-*`, etc.), bringing TypeScript to parity with the Rust/Python regimes. Because type-aware linting needs the TypeScript program, a node project **containing TypeScript must declare its `tsconfig`** — the honest-map verifier (`detect.rs`) adds this as a third fail-closed class (alongside undeclared/unsupported). A JS-only node project may still omit it.

## Alternatives Considered

- **Deny-unknown detection** (every extension must be adapter-claimed or on an Ops "benign" allowlist; anything else fails). Hardest to evade, but the benign allowlist grows huge, becomes its own relax point, and false-positives on every `.md`/`.svg`/lockfile. Rejected for a **hybrid**: adapter-backed extensions must be declared, a denylist of known code languages hard-fails, and benign non-code is ignored.
- **Adapter-list-only detection** (detect only languages that have adapters; ignore everything else). Simplest, but a Go/Ruby file with no adapter slips through silently — the weakest anti-evasion, defeating the point. Rejected.
- **Allow constrained repo exclusions** (excluded paths still secret/SAST-scanned, can't cover a whole language). More practical for generated code, but reintroduces a narrowed evasion surface. Rejected — generated code that can't pass is regenerated or the adapter is adjusted by Ops.
- **An Ops "acknowledged-unsupported" list** to tolerate specific languages (scan-only). More flexible, but "green gate" would stop meaning "fully linted" for those languages. Rejected in favour of the absolute hard-fail.
- **Forcing repo tsconfig via `tsc -p … --strict` flags.** Combining `--project` with compiler flags is rejected or ignored on some tsc versions, and a repo could still opt out of an individual `strict` sub-flag. The generated `extends` wrapper is well-defined across versions and overrides per-key. Chosen.

## Consequences

- **Completeness is now a structural guarantee.** Gap #89 is closed: a green gate means every first-party line was checked by an adapter, or the gate failed. The "hide it in a subdir / un-adapted language / behind .gitignore" evasions all fail closed.
- **Every gated repo must add a `gate-config.json`.** This is a breaking change for consumers: a repo that bumps to the new gate tag without adding the file will fail. Mitigated by the per-caller `gate_version` pin (repos adopt on their own schedule) and by enforcement still being in Audit. Onboarding gains one step (see the [platform runbook](https://github.com/Grizzly-Endeavors/grizzly-platform/blob/master/docs/runbooks/ci-gate.md)).
- **Adding a new language is now a two-part Ops change:** an adapter under `languages/` *and* its `[detect]` block (plus removing it from the `detect.toml` denylist if it was there). The coverage doc and this constraint keep the two in sync.
- **One more required file in the config tree** (`detect.toml`); like the rest of the tree it fails closed if absent.
- **The denylist is maintenance.** A code language not in the denylist and not adapter-backed would slip by; the list is seeded broadly (go/ruby/java/kotlin/scala/c/cpp/csharp/php/swift/perl/lua/elixir/dart) and extended as needed. This is the acknowledged boundary of the hybrid model.
- **`skip_dirs` is the one detection boundary an attacker could exploit** (hide code under `node_modules/` etc.). Kept tight and dependency-specific; the whole-tree scanners still cover those paths for secrets/SAST.
