# ADR-037: cargo-deny license allow-list broadened to reduce fleet maintenance toil

**Date:** 2026-07-05
**Status:** accepted
**Amends:** [ADR-034](034-sca-vuln-only-license-loosening.md)

## Context

ADR-034 broadened `deny.toml`'s `[licenses] allow` list from a narrow permissive set to a wider permissive + weak-copyleft set, and dropped osv-scanner's license enforcement entirely. In practice this still wasn't enough: the owner kept having to hand-edit the allow-list whenever a new dependency carried an ordinary, non-copyleft license that just hadn't been listed yet — the same maintenance toil ADR-034 set out to fix, just at a smaller scale. The desired end state is "block on real security-known-unsafe (known vulnerabilities), don't block on license identity except the one family that's a deliberate, surfaced policy call (strong copyleft)."

The natural framing is "flip the allow-list to a deny-list" — allow everything by default, explicitly deny only GPL/AGPL/SSPL. That framing does not map onto the actual tool: **cargo-deny 0.19.x's `[licenses]` section has no `deny`, `copyleft`, or `allow-osi-fsf-free` field** — verified directly against the pinned version's own schema (`cargo deny init` on the locally installed 0.19.0 binary emits the full documented field set: `allow`, `confidence-threshold`, `exceptions`, `clarify`, `private`, nothing else). A license not present in `allow` is denied by cargo-deny's hardcoded default; there is no config knob to invert that default. This differs from cargo-deny's older (pre-`version = 2`) config shape, which is likely the source of the "just flip it" assumption.

## Decision

**Since a true deny-list isn't a config option, broaden `allow` to the practical equivalent: a maximally inclusive list of permissive and weak-copyleft SPDX identifiers actually seen across the fleet's ecosystems**, so an ordinary new dependency license essentially never needs a manual add going forward. Strong copyleft (GPL-family, AGPL, SSPL) remains absent from the list — the one license family the gate continues to block on, by omission rather than an explicit `deny` entry (which doesn't exist).

Every added identifier was validated against the actual pinned cargo-deny binary (`cargo deny check licenses` against a real `Cargo.lock`, plus a deliberately-invalid identifier to confirm the tool errors on unrecognized SPDX ids at config-parse time — it does: `error[custom]: unknown term`). This guards against a typo'd identifier silently doing nothing (harmless) or, worse, breaking config parsing for every gated repo.

Added to `config/languages/rust/deny.toml`'s `[licenses] allow`: `Apache-1.1`, `AFL-2.1`, `AFL-3.0`, `Artistic-2.0`, `BSD-3-Clause-Attribution`, `BSD-4-Clause`, `CC-BY-3.0`, `CC-PDDC`, `EPL-1.0`, `EPL-2.0`, `EUPL-1.2`, `HPND`, `ICU`, `IJG`, `Libpng`, `MulanPSL-2.0`, `NCSA`, `OFL-1.1`, `Ruby`, `Vim`, `X11`, `Xnet`, `Zend-2.0`, `curl`, `FTL`, `Fair`, `Info-ZIP`.

## Consequences

- Ordinary permissive-license churn (the recurring "add this new-but-fine license" commit) should become rare rather than routine.
- The one remaining, deliberate block is strong copyleft (GPL/AGPL/SSPL) — unchanged from ADR-034, still absent from `allow`, still fails closed if pulled in.
- Because this is an allow-list, not a true deny-list, a sufficiently obscure or brand-new SPDX identifier can still require a manual add — the tool's ceiling, not a gap in this change. If that keeps recurring, the next lever is osv-scanner/deps.dev's broader license classification data (currently unused for enforcement per ADR-034), not further cargo-deny allow-list growth.
- `unused-allowed-license = "allow"` (unchanged) means an allow-list entry no dependency currently uses produces no warning — the list can stay broad without noise.
