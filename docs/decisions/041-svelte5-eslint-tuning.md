# ADR-041: Svelte 5 ESLint tuning (rune-aware prefer-const, `.svelte.ts` parser, external links)

**Date:** 2026-07-08
**Status:** accepted
**Relates to:** [ADR-031](031-go-svelte-react-coverage.md), [ADR-033](033-self-gate-eslint-config-template.md)

## Context

A full gate pass on a SvelteKit project (issue #6) hit three ESLint failures on standard, framework-encouraged Svelte 5 patterns, each with **no code-level fix**. The reporter tried fixing all three in their own `eslint.config.js` and saw zero change — correctly, because the gate forces its own flat config with `--no-config-lookup` and ignores the repo's ([ADR-033](033-self-gate-eslint-config-template.md)). So the fix is central, in `config/languages/node/eslint.config.mjs.tmpl`. All three were reproduced and the fixes verified against the pinned gate image (eslint `9.39.4`, typescript-eslint `8.62.0`, eslint-plugin-svelte `3.20.0`, svelte-eslint-parser `1.8.0`).

**2a — `prefer-const` on `$props()` destructuring.** Svelte 5 bindable props must be declared inline in one destructuring statement, and because a bindable field needs `let`, the *whole* statement is `let`:

```svelte
let { open = $bindable(false), filters, onPick } = $props();
```

Core `prefer-const` flags `filters`/`onPick` as "never reassigned, use const" — it can't see that `open` is reassigned by the framework via `$bindable`. The reporter's instinct, `prefer-const: ["error", { destructuring: "all" }]`, does **not** fix it: ESLint sees *none* of the bindings reassigned (the `$bindable` reassignment is compiler-level, invisible to it), so `"all"` still wants `const` on all of them. Splitting into two statements breaks the component (Svelte synthesizes `$$ComponentProps` only from the first `$props()` call). Reproduced: baseline flags every non-bindable field.

**2b — `.svelte.ts` rune modules fail to parse.** Svelte 5 allows reactive state in a plain module named `*.svelte.ts` — 100% ordinary TypeScript, no template syntax. The gate reported `Parsing error: The keyword 'interface' is reserved` (or `type`, or a generic param) on every such file. Root cause: `.svelte.ts` is classified by its last extension (`ts`) and matches the type-aware TS block, but eslint-plugin-svelte's recommended `setup-for-svelte-script` block (matching `*.svelte.{js,ts}`) runs *later* in the flat-config array and routes these files back to svelte-eslint-parser; with no TS sub-parser configured for the double extension (the gate's `**/*.svelte` override doesn't match it), that parser chokes on TS syntax. Reproduced: baseline emits the fatal parse error.

**2c — `svelte/no-navigation-without-resolve` on external links.** SvelteKit's `resolve()` is for type-safe *internal* route links against the app's own route manifest; it doesn't accept arbitrary external URLs. The rule (from svelte's recommended set) nonetheless flags plain external links like `<a href={record.externalUrl} target="_blank">`. There's no code fix (you can't and shouldn't wrap an external URL in `resolve()`) and no rule option to distinguish internal from external hrefs.

## Decision

Two overrides after `...svelte.configs.recommended` in the flat config, splitting **components** from **rune modules** because they need different parsers:

1. **`*.svelte` components** (existing override, extended). Keep svelte-eslint-parser with the typescript-eslint sub-parser (so `<script lang="ts">` and rune-aware rules work), and tune two rules:
   - `prefer-const: "off"` + `svelte/prefer-const: "error"` — the rune-aware rule understands `$props`/`$bindable` and does not flag the forced-`let` destructure, while still enforcing `const` everywhere else (**2a**). Verified: it stays silent on the `$props` pattern and still flags a genuine const-able `let` in a component.
   - `svelte/no-navigation-without-resolve: "off"` — a SvelteKit internal-link nicety, not a security/correctness rule, and simply wrong for external links (**2c**).
2. **`*.svelte.{js,ts}` rune modules** (new override) — route straight to the typescript-eslint parser (`languageOptions.parser = tseslint.parser`). These files carry no template syntax, so they don't need the Svelte-aware parser, and using it causes problems: besides the parse error, svelte-eslint-parser *even with a TS sub-parser* mishandles type-only references, producing a false `no-unused-vars` on a type used only in annotations (verified: the plain TS parser is clean where the Svelte parser falsely flags it). The plain-TS route fixes the parse error (**2b**) with no false positives, and these files still inherit `strictTypeChecked` + the security core from the TS block (they match `**/*.ts`). No rune-aware `prefer-const` swap is needed here — in a module `$state` is reassigned in source (core `prefer-const` is satisfied) and `$derived` is `const`; the forced-`let` problem is component-only. Verified: a well-typed `.svelte.ts` using `interface`/`type`/generics + `$state`/`$derived` is clean, and core `prefer-const` still flags a genuine const-able `let`.

## Consequences

- The three Svelte 5 patterns — bindable `$props`, `.svelte.ts` modules, external links — now pass with no source change and no suppression, verified end-to-end against the pinned image. Both file types keep full discipline: `strictTypeChecked` + the security core on both, rune-aware `svelte/prefer-const` on components, core `prefer-const` on modules; teeth confirmed on both.
- `svelte/no-navigation-without-resolve` is off **fleet-wide** — gated SvelteKit projects lose that internal-link type-safety check. It's a convenience rule, not a security/correctness one, and there is no way to keep it for internal links without also breaking external ones. Accepted, like [ADR-034](034-sca-vuln-only-license-loosening.md), as removing friction-without-a-clean-fix.
- This is central config tuning; the repo's own `eslint.config.js` remains ignored by design ([ADR-033](033-self-gate-eslint-config-template.md)). The change takes effect only once a new gate image is built (the flat config is baked in), so a local pre-check reflects it after the rebuild.
- Splitting the Svelte override into component vs. module blocks is now a load-bearing ordering detail: both must sit after `...svelte.configs.recommended` so their parser choice wins over the recommended `setup-for-svelte-script` block.
