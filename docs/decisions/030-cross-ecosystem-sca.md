# ADR-030: Cross-ecosystem dependency SCA (OSV-Scanner + Trivy fs), fetched fresh

**Date:** 2026-06-25
**Status:** accepted
**Relates to:** [ADR-028](028-centralized-ci-gate.md)
**Adapted from:** [grizzly-platform ADR-030](https://github.com/Grizzly-Endeavors/grizzly-platform/blob/master/docs/decisions/030-cross-ecosystem-sca.md).

## Context

The gate had real dependency Software-Composition-Analysis (advisories, licenses, sources) only for Rust, via `cargo-deny`. For every other ecosystem the only coverage was incidental: the image-scope Trivy scanner sees language packages that happen to land in the *built image's* library layer. That misses anything dropped by a multi-stage build, catches nothing pre-build, and does **no** license checking outside Rust. The coverage doc's "no SCA outside Rust" claim was, if anything, generous — and the gap matters precisely because the gate's whole premise is "deny anything potentially unsafe without a human reading every line."

Two design forces, made explicit by the owner:

1. **Maximal denial over convenience.** The point is to refuse unsafe dependencies, not to minimise false positives. Parity with `cargo-deny` means: any known advisory fails (all severities, including unfixable), and licenses are an allow-list (deny by default), not a detect-and-report.
2. **Offline/reproducibility is *not* a goal.** An earlier "everything runs offline against a baked DB" property was incidental, never an ADR'd decision. For SCA specifically, freshness beats reproducibility: a build should fail the moment a new advisory lands against one of its dependencies, not at the next gate-image rebuild.

## Decision

**Add two source-scope SCA scanners that fetch advisory/license data fresh at scan time, configured for maximal denial.**

1. **OSV-Scanner (Google) — the cross-ecosystem `cargo-deny` analog.** `osv-scanner scan source -r` over the repo's committed lockfiles, in one pass:
   - **Vulnerabilities:** any known advisory in OSV.dev fails — **all severities, including unfixable**. OSV-Scanner has no severity threshold, which *is* the cargo-deny model (curated advisories, any hit fails).
   - **Licenses:** `--licenses="<allowlist>"` enforces a deny-by-default allow-list mirroring the Rust fleet's `deny.toml` (MIT, Apache-2.0 [+ LLVM-exception], BSD-2/3-Clause, ISC, Zlib, Unicode-3.0/DFS-2016, MPL-2.0). An unmapped or `non-standard` license is denied too — the deny-unknown stance.
   - `--allow-no-lockfiles` so a repo with no dependency manifests (pure Ansible/YAML) passes cleanly rather than erroring.
2. **Trivy filesystem mode — a second, broader vuln DB.** `trivy fs` over the source, all severities, `ignore-unfixed: false`, `library` packages only (the source-SCA complement to the OS-focused image scanner). Trivy's DB aggregates GitHub advisories + OSV + vendor feeds, so running it *alongside* OSV-Scanner yields union coverage — more denial, the explicit goal. (The two overlap on vulns by design; OSV-Scanner is the policy owner for licenses.)
3. **Fresh, fail-closed.** Both fetch current advisory/license data at scan time. A newly-disclosed CVE therefore fails a previously-green build — intended. If the data can't be fetched, the tool errors and the step fails closed (no silent skip). This deliberately trades the old incidental offline/reproducible property for freshness, per the context above.
4. **Source scope, universal.** Both are `util/` scanners that run on every invocation regardless of language markers — like gitleaks and semgrep — so a repo with no gate adapter still gets dependency-scanned.

## Alternatives Considered

- **`npm audit` / `yarn audit` / `pip-audit`.** Per-ecosystem (fragmented failure semantics, a different tool per language), historically noisy, and still online at scan time — so they cost the offline property *without* the consolidation OSV-Scanner gives. Rejected: OSV-Scanner covers every ecosystem in one tool with one policy.
- **OSV-Scanner only (drop Trivy fs).** Cleaner, but loses the union vuln coverage from Trivy's broader aggregated DB. Given "maximal denial," the redundant second engine is a feature, not waste. Kept both.
- **Trivy fs only (skip OSV-Scanner).** Trivy's license scanning is detect-and-categorise, not a deny-by-default allow-list — it wouldn't give cargo-deny-style license *policy* parity. OSV-Scanner's `--licenses` allow-list does. Kept OSV-Scanner as the license-policy owner.
- **Keep it offline (baked OSV/Trivy DBs).** OSV-Scanner supports `--offline` with pre-downloaded databases and Trivy can `--skip-db-update`. Rejected for SCA: a frozen DB means a build stays green against a dependency that was disclosed-vulnerable an hour ago. Freshness is the whole point of a supply-chain gate.

## Consequences

- **The "no SCA outside Rust" gap is closed.** Node/Python/Go/etc. now get vuln scanning (two DBs) and license-allowlist enforcement at the source, pre-build. Coverage doc updated accordingly.
- **Verdicts are now time-varying by design.** The same gate tag against the same commit can pass today and fail tomorrow if an advisory lands. This breaks the old "same tag ⇒ same verdict" reproducibility note (also updated in the coverage doc). For a security gate this is correct; for debugging a sudden failure, check whether a new advisory dropped.
- **License-allowlist denial is the noisiest knob.** Deny-by-default plus "non-standard ⇒ deny" will fail repos pulling deps with unrecognised or copyleft licenses. This is intentional but the most likely thing to tune; the allow-list and the unfixable-vuln stance are the two dials to relax first if it proves impractical for a given ecosystem (npm's transitive sprawl especially).
- **Two new scan-time network dependencies** (OSV.dev, deps.dev). They are now in the gate's critical path; a fetch failure fails the build closed.
- **Rust is double-covered** (cargo-deny + the two new scanners also read `Cargo.lock`). Harmless and additive for vulns; `cargo-deny` remains the authoritative Rust policy.

## Correction (2026-06-26)

The original allow-list above listed **`Apache-2.0 WITH LLVM-exception`**, but osv-scanner 2.4.0's `--licenses` flag accepts only plain SPDX license IDs and **errors out the entire scan** on any `<id> WITH <exception>` expression — which silently broke `scan:osv-scanner` for every repo. The `WITH LLVM-exception` entry has been removed from the manifest (plain `Apache-2.0` remains); see the note in `config/util/osv-scanner/manifest.toml`. Consequence: a dependency whose license is *exactly* the LLVM-exception form is now flagged (rare). If the fleet needs it back, it requires an osv-scanner mechanism that supports exception expressions, not a flag tweak.
