# ADR-040: Reconcile the ruff↔semgrep static multi-line string conflict

**Date:** 2026-07-08
**Status:** accepted
**Relates to:** [ADR-034](034-sca-vuln-only-license-loosening.md)

## Context

A full gate pass on a Python/FastAPI project (issue #6) hit a rule conflict with **no source shape that satisfies both tools**. The trigger is a static (no interpolation) multi-line string too long for one line — e.g. long SQL text passed as the sole argument to a call inside a list:

```python
conditions = [
    text(
        "h.as_of_date >= DATE_SUB("
        "(SELECT MAX(as_of_date) FROM some_table "
        "WHERE target_table='inventory'), "
        "INTERVAL :days DAY)"
    )
]
```

There are three ways to write that string in Python, and each trips a different active check:

1. **Implicit adjacent-literal concatenation** (above) — the form ruff *prefers* (`FLY002` recommends it over `.join(...)` for static literals) and *permits* (`ISC001` single-line implicit is already ignored as the formatter's job; `ISC002` multi-line implicit is **dormant** because ruff's `flake8-implicit-str-concat` `allow-multiline` defaults `true`). So this form is ruff-clean. But semgrep's `python.lang.correctness.common-mistakes.string-concat-in-list` fires on it: that shape is *also* the signature of a missing comma between two list elements that silently merges into one string, and semgrep cannot tell an intentional multi-line string from a lost comma.
2. **An f-string** — `F541` (f-string without placeholders) fires, since there's nothing to interpolate.
3. **Explicit `+` concatenation** — ruff `ISC003` fires ("explicitly concatenated string should be implicitly concatenated" — the literal opposite of semgrep's push), and turning the constant into a runtime expression can flip semgrep's `avoid-sqlalchemy-text` from silent to firing.

Verified empirically against the pinned gate image (semgrep `1.168.0` + the vendored `semgrep-rules` set; ruff via `config/languages/python/ruff.toml`): the implicit-concat form above produces exactly one finding — semgrep `string-concat-in-list` — and is otherwise ruff-clean. This is not a one-off; the reporter saw the same shape recur across six files, all for the same reason (long SQL), none an actual missing-comma bug.

The reporter cannot fix this in their own code, and (correctly, by design — [ADR-029](029-gate-config-honest-map.md)/[ADR-033](033-self-gate-eslint-config-template.md)) cannot relax it via their own config: the gate forces its own tool config. The resolution therefore has to be central, like [ADR-034](034-sca-vuln-only-license-loosening.md)'s vuln-only loosening — tune the gate's own config so a clean path exists.

## Decision

**Exclude the single semgrep rule `string-concat-in-list` by id, making implicit adjacent-literal concatenation the one clean canonical form for static multi-line strings.** No ruff change is needed — that form is already ruff-clean.

`config/util/semgrep/manifest.toml`: added `--exclude-rule=etc.grizzly-gate.config.util.semgrep.rules.python.lang.correctness.common-mistakes.string-concat-in-list` to the semgrep `cmd`. The id is the vendored ruleset's path-derived id (rules install at the fixed `/etc/grizzly-gate/config/util/semgrep/rules` path baked in the Dockerfile), confirmed by running semgrep against a fixture and reading the `check_id` off the finding.

Chose `--exclude-rule` in the manifest over a `find … -rm` prune of the rule's YAML in the Dockerfile semgrep-vendoring step (the mechanism used for the non-rule and AI-maintainability files):

- The decision lives in `config/` next to the other semgrep flags, discoverable and reviewed with the rest of the gate config, not buried in an image-build layer.
- It keys on the rule **id**, so it survives the ruleset being re-vendored from the (unpinned) `develop` branch even if the upstream file path moves — whereas a path-based `rm` would silently stop excluding if upstream relocated the rule.

Verified against the pinned image: with the exclusion, the SQL fixture yields **zero** semgrep findings and a clean exit, and stays ruff-clean.

## Consequences

- Implicit adjacent-literal concatenation is now the single canonical form for a static multi-line string in gated Python — ruff-clean and semgrep-clean. `ISC003` (explicit `+`) and `F541` (empty f-string) stay enabled, so the *other* two shapes still fail; there is exactly one clean path, which is the point.
- **Coverage traded:** the gate no longer flags a genuinely missing comma between two adjacent string literals in a list/call. This is a real but low-severity class that ruff does not otherwise catch, and one that a merged string usually surfaces downstream anyway (it changes runtime behavior, so tests/type-checks tend to break). Accepted as friction-without-a-clean-fix, consistent with ADR-034's posture.
- `--exclude-rule` **silently no-ops on a mistyped id** (the rule would keep firing with no error). The exact id was confirmed empirically here; a future ruleset change that renamed this rule's path would re-introduce the finding, caught by a gate run rather than by config validation.
- Scope is one rule id. The rest of the vendored semgrep set — including the whole `python.lang.security` tree and the SQL-injection rules — is unchanged and still fails closed under `--error`.
