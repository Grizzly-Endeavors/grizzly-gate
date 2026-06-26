# ADR-031: Add Go, Svelte, and React coverage

**Date:** 2026-06-26
**Status:** accepted
**Relates to:** [ADR-028](028-centralized-ci-gate.md), [ADR-029](029-gate-config-honest-map.md), [ADR-030](030-cross-ecosystem-sca.md)

## Context

The gate adapted Rust, Python, Node (TS/JS), Ansible, and YAML. Three ecosystems in active use across the platform family were second-class: **Go** sat on the `detect.toml` denylist, so any `.go` file hard-failed the honest map (a green gate was impossible for a Go repo); **Svelte** components (`.svelte`) were undetected entirely; and **React** (`.jsx`/`.tsx`) was linted only as generic Node, with none of its real correctness rules (rules-of-hooks). Adding a language is, per [ADR-029](029-gate-config-honest-map.md), a deliberate two-part change (an adapter *and* its detection rules) — a repo cannot will a language into scope — so this is that change, made for all three at once.

Two forces shaped the design, made explicit by the owner:

1. **Match the Rust strictness floor wherever the toolchain allows.** The strict configs here are the product, not over-engineering. Each new language should reach for the same max-denial bar the Rust adapter sets (warnings-as-errors, security lints, suppression hygiene), not a token "linter runs" tier.
2. **Nothing stale.** The coverage docs, the matrix, and the Claude Code plugin all enumerate the supported set; every one had to move in lockstep so a green gate's meaning stays honest.

## Decision

### Go — a new, standalone adapter (first compiled non-Rust language)

`config/languages/go/` with marker `go.mod` and `[detect] extensions = ["go"]`; removed from the `detect.toml` denylist. Checks mirror the Rust adapter's shape and reach for the same floor:

- **`golangci-lint run`** forced onto the gate's own `.golangci.yml` via `-c` (which also disables discovery of the repo's config — the config-forcing guarantee). `default: none` plus an explicit aggressive enable list maps to the Rust failure classes: `errcheck`/`errorlint`/`nilerr` (silent-error-swallowing), `gosec` (in-language security; `G103` flags `unsafe`), `govet`/`staticcheck`/`revive`/`gocritic` (correctness + pedantic), resource-leak/footgun linters, and **`nolintlint` with `require-explanation` + `require-specific`** (the `allow_attributes_without_reason` analog — no blanket `//nolint`).
- **`golangci-lint fmt --diff`** enforces gofumpt + goimports (the `rustfmt --check` analog).
- **`govulncheck ./...`** — call-graph-aware, reachability-filtered dependency vuln scan, fetched fresh (consistent with [ADR-030](030-cross-ecosystem-sca.md)'s freshness stance). It complements, rather than replaces, the always-on osv-scanner + trivy-fs, which already cover `go.mod`/`go.sum` from the lockfile.
- **`go test ./...`** runs the suite (same caveat as Rust/Python: runs tests, doesn't mandate good ones).

`GOTOOLCHAIN=local` pins the Go toolchain so a scanned repo's `go.mod` `toolchain` directive can't pull a different compiler — gate determinism.

### Svelte — folded into the Node adapter, not a new adapter

A Svelte repo *is* a Node project (it has a `package.json`), so Svelte rides the existing `node` adapter rather than getting its own marker/root:

- `.svelte` is added to the node adapter's `[detect].extensions`, so a `.svelte` file must be covered by a declared `node` project.
- A **`svelte-check`** step (guarded to no-op when a project has no `.svelte` files) typechecks components against the **same wrapped tsconfig** `tsc` uses, so the gate's forced strictness applies to component `<script lang="ts">`. `eslint-plugin-svelte` lints markup/logic.
- **Strict tsconfig rule (harness change).** Because a component's TypeScript is invisible to the `.ts`/`.tsx` extension check, `detect.rs` content-sniffs `.svelte` files: a component with `<script lang="ts">` makes its node project **require a declared `tsconfig`** or the gate fails closed — the exact analog of the existing TS-without-tsconfig rule (reuses the same `TsWithoutTsconfig` violation class). This was chosen over the pragmatic "fall back to the gate base tsconfig" option so Svelte's type bar is genuinely equal to TypeScript's.
- The Svelte toolchain (`svelte`, `svelte-check`, `eslint-plugin-svelte`, `svelte-eslint-parser`) installs into the **same** `node_modules` as the existing TS toolchain — which is also why it *must* live under the node config dir (binary path + plugin resolution).

### React — an eslint enrichment of the Node adapter, no new anything

`.jsx`/`.tsx` are already node-detected, so React needs no detect change, no new check, and no marker. The node adapter's `eslint.config.mjs` gains a `files: ["**/*.{jsx,tsx}"]` block: `react-hooks` `rules-of-hooks` + `exhaustive-deps` as **errors** (the genuine bug classes) plus `eslint-plugin-react`'s recommended set. `prop-types` is off (tsc / type-aware eslint own prop typing) and the automatic JSX runtime means `react-in-jsx-scope` is off. `jsx-a11y` is intentionally excluded — accessibility is a quality nicety, not a correctness/security bar.

## Alternatives Considered

- **Svelte as its own top-level adapter (`languages/svelte/`).** Rejected: it would duplicate the Node `npm`/`node_modules` install, and two adapters sharing one `package.json` marker and project root muddy the honest map (which adapter "owns" `.svelte`?). Folding into Node keeps one marker, one project, multiple checks — exactly how `tsc` and `eslint` already coexist.
- **Pragmatic Svelte tsconfig (base-config fallback, no harness change).** Rejected by the owner in favor of the strict rule above: equal strictness to TS was worth the small `detect.rs` change + content-sniff.
- **Rely on osv-scanner alone for Go deps (skip govulncheck).** Consistent with how no other language gets a dedicated dep auditor, but govulncheck's reachability filtering is materially more precise for Go and the owner opted to add it. Kept both (lockfile SCA + call-graph SCA).
- **A `forbidigo` "no `fmt.Print`" lint for Go.** Rejected for consistency: the Rust adapter intentionally *allows* prints (`print_stderr` is not denied), so denying them in Go would be an inconsistent, noisy bar for CLI tools.

## Consequences

- **Go repos can now pass.** A `.go` file moves from "hard fail closed" to "fully checked"; a Go repo declares `{"language":"go","path":"…"}` like any other. The denylist seed list and both coverage docs are updated.
- **Svelte/React are first-class without column sprawl.** They remain part of the Node tier (TS/JS), with svelte-check / react-hooks called out in the matrix rather than as separate languages.
- **One new structural rule in the harness.** `.svelte`-with-TS now participates in the tsconfig-required honest-map check; this is the only Rust change — Go and React are pure config/Dockerfile/eslint.
- **Image weight grows.** The Go toolchain + golangci-lint + govulncheck and the extra npm plugins enlarge the gate image. Acceptable: the gate is a versioned artifact built in-cluster, not a hot path.
- **Go's lint floor is the most likely tuning surface.** `default: none` + a broad enable list (especially `gosec`, `revive`, `gocritic`) is aggressive by design; if a specific linter proves impractical for the fleet, scope it in `.golangci.yml` with a reason rather than relaxing the whole set.
