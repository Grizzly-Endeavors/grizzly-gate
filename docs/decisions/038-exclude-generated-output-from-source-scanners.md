# ADR-038: Exclude pure generated build output from semgrep and gitleaks

**Date:** 2026-07-05
**Status:** accepted

## Context

A gate run failed on `scan:semgrep` with a finding inside `web/.svelte-kit/generated/client/app.js` (`javascript.lang.security.audit.unsafe-dynamic-method`) — a file SvelteKit's own tooling generates, not first-party code. The initial assumption was that this was a stale-local-tree problem ADR-036 (honest-map walk scopes to the git index) should already cover. It doesn't, and the failure reproduces on a byte-for-byte clean CI checkout, for a more specific reason:

`run_checks` (`harness/src/main.rs`) executes all language-adapter checks for a project, in order, before any `scope = "source"` scanner runs. For a node project the first check is `deps` → `npm ci` (`config/languages/node/manifest.toml`). If the project's `package.json` declares the standard SvelteKit `"prepare": "svelte-kit sync"` script — true of nearly every SvelteKit scaffold — `npm ci` runs it automatically as an npm lifecycle hook, which writes `.svelte-kit/generated/**` into the source tree. `semgrep` and `gitleaks` then scan that same `source` directory afterward and see a file the gate's *own* pipeline just created, with no dependency on whether the file is tracked, gitignored, or present in the original clone.

`detect.toml`'s `skip_dirs` already lists `.svelte-kit` (along with `dist`, `build`, `node_modules`, `target`, `vendor`, `.venv`, …) for the honest-map completeness walk, and its header comment states the util scanners are meant to keep walking those dirs regardless: `node_modules`/`vendor`/`target` can hold vendored or fetched third-party code, and scanning them for hidden secrets or malicious code is a real supply-chain check, not noise. That rationale doesn't extend to `dist`, `build`, or `.svelte-kit` — these hold pure build *output*, deterministically regenerated from first-party source, never third-party code. Scanning them buys no security signal and, as here, produces framework-internal-codegen false positives.

## Decision

**Exclude only the pure-generated-output directories — `dist`, `build`, `.svelte-kit` — from `semgrep` and `gitleaks`.** `node_modules`, `vendor`, `target`, `.venv`/`venv` are deliberately left in scope for both scanners; this is a narrower, purpose-specific list, not a reuse of the full `skip_dirs` set.

1. **semgrep** (`config/util/semgrep/manifest.toml`): added `--exclude=dist --exclude=build --exclude=.svelte-kit` to the scan command.
2. **gitleaks** (`config/util/gitleaks/gitleaks.toml`): added a `[allowlist] paths` regex block matching the same three directory names at any depth.
3. **trivy-fs** is unaffected — it scans committed dependency lockfiles/manifests, not arbitrary generated source, so this class of false positive doesn't apply there. Its existing `skip-dirs` (`.venv`, `venv`) addresses a different, already-solved problem (ADR-032).

## Consequences

- The `.svelte-kit`/`dist`/`build` false-positive class is closed, in CI as well as locally — this is not a `.gitignore`/honest-map fix, so it applies regardless of git tracking state.
- `node_modules`/`vendor`/`target` remain fully scanned by semgrep and gitleaks — no reduction in the supply-chain coverage `detect.toml`'s comment describes.
- This is a distinct, narrower list from `skip_dirs`; a future addition to `skip_dirs` for a new build-tool artifact directory does **not** automatically exclude it from these scanners, and vice versa — each list should be extended deliberately for its own reason.
