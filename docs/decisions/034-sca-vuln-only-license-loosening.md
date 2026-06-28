# ADR-034: Dependency SCA is vuln-only; license enforcement loosened

**Date:** 2026-06-28
**Status:** accepted
**Relates to:** [ADR-030](030-cross-ecosystem-sca.md)

## Context

[ADR-030](030-cross-ecosystem-sca.md) set a maximal-denial posture for dependency SCA: any advisory of any class fails (vulnerability, **unmaintained**, **yanked**), and licenses are a deny-by-default allow-list across `cargo-deny` and `osv-scanner`. That posture is correct for *vulnerabilities* — a known-vulnerable dependency must not ship. But two of its sub-policies turned out to make repos undeployable for reasons that are **not security/vulnerability** issues, and that a downstream repo often cannot fix:

- **Unmaintained / yanked advisories.** A transitive crate whose maintainer walked away (e.g. `derivative`, pulled by `poise`; `proc-macro-error2`) carries a RUSTSEC *unmaintained* advisory with **no upgrade path**. The consuming repo can't fix it without forking its whole framework. The gate's "document it in your own tree" escape hatch doesn't exist for `cargo-deny` (which has no per-repo override the gate honours) and is pure friction for `osv-scanner`.
- **License allow-list.** Deny-by-default + deny-unknown means a single dependency with an unmapped or merely uncommon license blocks the deploy. Keeping the allow-list correct across the whole fleet is ongoing maintenance toil, and a license mismatch is a legal/compliance signal, not a runtime-security one.

The owner sanctioned loosening both: **the gate should block on known vulnerabilities, and treat unmaintained/yanked/license as non-blocking.**

## Decision

**Relegate dependency SCA to known vulnerabilities only; stop blocking on unmaintained/yanked advisories and on license terms.** Real vulnerabilities still fail closed, fresh, across every ecosystem (ADR-030 unchanged for vulns).

1. **cargo-deny (`config/languages/rust/deny.toml`).** `[advisories] unmaintained = "none"`, `yanked = "warn"` — only RUSTSEC *vulnerabilities* error. `[licenses] allow` broadened to a wide permissive + weak-copyleft set (strong copyleft GPL/AGPL still absent — a deliberate, surfaced decision, not a silent allow). `[bans] wildcards = "deny"` and `[sources]` are **unchanged** — those are reproducibility/supply-chain hygiene, not the license/advisory pain points.
2. **osv-scanner (`config/util/osv-scanner/manifest.toml`).** `--licenses` dropped entirely — no license enforcement. Vulnerability scanning across all ecosystems is unchanged.
3. **Unmaintained advisories in osv-scanner.** osv-scanner has no advisory-*type* filter (only IDs), so it still reports RUSTSEC unmaintained advisories as findings. A gate-owned **data file** — `config/util/osv-scanner/ignored-advisories.toml`, a list of `[[IgnoredVulns]]` with reasons — is appended to the harness-generated osv config at run time (`materialize_osv_config`). This is fleet-wide accepted-advisory data, edited by Ops, not per-repo and not code. It is the one place the vuln-only posture is relaxed; entries are removed when a real fix lands or an advisory is reclassified as a vulnerability.
4. **trivy-fs** is already vuln-only (`scanners: [vuln]`); unchanged.

## Consequences

- A repo no longer fails the gate because a transitive dependency is unmaintained, yanked, or carries an uncommon license — the friction-without-security-value cases. Known vulnerabilities still fail closed, fresh, in every ecosystem (cargo-deny + osv-scanner + trivy-fs).
- The accepted-advisory list (`ignored-advisories.toml`) is a small, auditable, justified data file — the deliberate, visible relaxation point. cargo-deny's `unmaintained = "none"` covers the Rust category wholesale; the data file is only needed because osv-scanner lacks the equivalent toggle.
- Supply-chain *source* gating (`wildcards`/`unknown-registry`/`unknown-git`) is retained — loosening was scoped to licenses and advisory-class, not to reproducibility.
- Strong copyleft is still not allow-listed, so a GPL/AGPL dependency still fails cargo-deny licenses — intentionally surfaced rather than silently permitted.
