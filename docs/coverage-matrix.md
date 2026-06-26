# CI Gate — Coverage Matrix

The skim-friendly companion to [coverage.md](coverage.md). That doc explains *why* each check exists and where the gaps are; this one is the at-a-glance grid: pick a threat class (SQL injection, strict typing, vulnerable dependencies, …) and a language, and read off whether the gate blocks it. Everything here is derived from the same config snapshot in `config/` — if a cell and the config disagree, the config wins and this doc is the bug.

**Legend:** ✅ enforced and blocking · ⚠️ partial or conditional (see note) · — not covered / not applicable. "Blocking" means a violation fails the gate, so the image is never signed.

A note on the language columns: **Rust / Python / Go / TypeScript / JavaScript** are adapter-backed code languages. **React** (`.jsx`/`.tsx`) and **Svelte** (`.svelte`) are not separate columns — they ride the node adapter (TS/JS), with React-hooks and svelte-check rows called out in §4. **Ansible / YAML** are opt-in config-language adapters (activated by an `ansible/` dir and a `.yamllint` marker respectively). Code in a language with *no* adapter (Ruby, Java, …) does not "get scanned and pass" — the honest-map walk **fails the gate closed** so it can never ship. The only un-adapted things that ride through are non-code files and anything under the Ops-owned `skip_dirs`, and those still get the always-on secret + SAST + SCA scanners below.

## 1. Always-on source scanners (every repo, every language)

These run on every invocation regardless of which adapters fire — they are the floor even for a repo the gate has no language adapter for. Tool in parentheses; all are source-scope unless noted.

| Threat class | Tool | Rust | Python | Go | TS | JS | Ansible | YAML | Notes |
|---|---|:--:|:--:|:--:|:--:|:--:|:--:|:--:|---|
| Committed secrets / credentials / API keys | gitleaks | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | Default ruleset, `--redact`; cloud keys, private keys, VCS/Slack/Stripe tokens, `.env` leaks |
| SQL / command / template injection | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | Depth is rule-dependent per language; config langs get only the rules that exist for them |
| Unsafe deserialization (pickle/yaml.load/etc.) | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | |
| Weak / broken crypto (MD5, SHA1, ECB, hardcoded IV) | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | |
| SSRF / path traversal / XXE | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | |
| `eval`/`exec` on untrusted input | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | Also caught by in-language linters (§4) |
| Disabled TLS / cert verification | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | |
| Insecure temp-file creation | semgrep | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | |

The semgrep ruleset is vendored offline, so these verdicts are reproducible within a gate tag. ⚠️ on Ansible/YAML reflects that semgrep's SAST rules mostly target code languages — config files are covered only where a matching rule exists.

## 2. AI / LLM application-security matrix (every repo, always-on)

The gate is the control plane for AI-generated code and the family ships AI apps, so the semgrep ruleset's `ai/ai-best-practices/` set (~100 rules, all `category: security`) is first-class and runs on every invocation as part of the always-on semgrep pass. All cells are blocking (`semgrep --error`) and reproducible (ruleset vendored offline). These check **how** an app uses an LLM — distinct from "this repo uses AI," which the gate deliberately does *not* flag (see the note below).

| Threat class | Rules (`ai/ai-best-practices/`) | Targets | Notes |
|---|---|---|---|
| LLM output → code execution (model-controlled RCE) | `llm-output-to-exec`, `langchain-dangerous-exec` | Python, JS | Highest-severity LLM bug class — a model response reaching `exec`/`eval`/a shell |
| Prompt injection via untrusted system prompt | `*-user-input-in-system-prompt` | OpenAI/Anthropic/Gemini/Cohere/Mistral · Py/JS | User-controlled data concatenated into the system prompt — root of most jailbreak chains |
| MCP server: command injection / SSRF | `mcp-command-injection`, `mcp-ssrf` | generic | The gate ships its own MCP server; every family MCP tool held to this |
| MCP server: credential / data leakage | `mcp-credential-in-response`, `mcp-unsanitized-return`, `mcp-hardcoded-config-secret`, `mcp-tool-poisoning` | generic | |
| Claude Code hooks abuse (RCE / exfiltration) | `hooks-wget-pipe-bash`, `hooks-dns-exfiltration`, `hooks-sensitive-file-access`, `hooks-path-traversal`, `hooks-no-input-validation`, `hooks-unquoted-variable`, `hooks-unconditional-allow`, `hooks-relative-script-path` | bash / Python (hooks) | This repo ships hooks — the exact ways one becomes a code-exec/exfil vector |
| Agent / settings over-authority | `claude-settings-bypass-permissions`, `claude-settings-env-url-override`, `claude-settings-auto-enable-mcp`, `agent-unbounded-loop`, `ide-settings-executable-path` | config / generic | Config that silently widens agent authority or lets it spin unbounded |
| Hidden-unicode instruction injection | `ai-config-hidden-unicode` | AI config / prompt files | Invisible/bidi unicode smuggled into instructions |
| Hardcoded LLM API keys | per-provider `*-hardcoded-api-key`, `llm-api-key-in-source` | Py/JS/Go/Java/Ruby | Provider-specific complement to the always-on gitleaks scan |
| Missing LLM safety controls (robustness) | `*-missing-moderation`, `*-missing-refusal-check`, `*-missing-safety-settings`, `*-missing-max-tokens`, `*-no-error-handling` | per provider · Py/JS | Best-practice/robustness — still blocking under `--error` |

**Not flagged, by design:** the ruleset's *other* `ai/` half — 25 *shadow-AI inventory* rules (`ai/**/detect-*`, `category: maintainability`, severity `INFO`) that merely detect a repo *uses* an AI SDK (one even matches the literal strings `claude`/`anthropic` in any file) — is **dropped at image-build**. Under `--error` they would fail every legitimate AI-using repo and this repo's own docs. Detecting *that* AI is present is not a security signal; the rows above (how it's used) are. See [coverage.md](coverage.md) §AI / LLM application security.

## 3. Dependency & supply-chain matrix

SCA reads **committed lockfiles** and fetches **fresh** advisory/license data at scan time (so a newly-disclosed CVE fails a previously-green build); it fails closed if data can't be fetched. A repo with no lockfile gets no dependency resolution to scan (`--allow-no-lockfiles` lets a depless repo pass cleanly).

| Threat class | Tool | Rust | Python | Go | TS/JS | Ansible/YAML | Notes |
|---|---|:--:|:--:|:--:|:--:|:--:|---|
| Known-vulnerable dependency (CVE) — source | osv-scanner + trivy-fs | ✅ | ✅ | ✅ | ✅ | ⚠️ | All severities incl. unfixable; npm/PyPI/Go/Maven/RubyGems/Cargo/…; two DBs for union coverage. ⚠️ = only if the config repo commits a lockfile |
| Reachable dependency vuln (call-graph) | govulncheck | — | — | ✅ | — | — | Go-only; reachability-filtered, fresh DB at scan time; complements the lockfile-based osv-scanner/trivy-fs |
| Disallowed dependency license (copyleft/unknown) | osv-scanner | ✅ | ✅ | ✅ | ✅ | ⚠️ | Deny-by-default allowlist (MIT/Apache/BSD/ISC/Zlib/Unicode/MPL-2.0 + permissive PSF-2.0/Python-2.0/0BSD/BlueOak-1.0.0); unmapped/`non-standard` license is denied |
| Untrusted registry / git source | cargo-deny | ✅ | — | — | — | — | Rust-only; allowlist restricted to crates.io. **No Go/npm/PyPI equivalent** |
| Wildcard version requirement | cargo-deny | ✅ | — | — | — | — | Rust-only (`wildcards = "deny"`) |
| Unmaintained / yanked dependency | cargo-deny | ✅ | — | — | — | — | Rust-only (RUSTSEC unmaintained, `yanked = "deny"`) |
| Base-image / bundled CVE in built image | trivy (image) | ✅ | ✅ | ✅ | ✅ | ✅ | **Image scope** — only runs with `--image`; fails on *fixable* HIGH/CRITICAL (`ignore-unfixed: true`), os + library packages |

The image-scope row applies to whatever the built container actually contains, independent of language. The biggest asymmetry to remember: **dependency-source gating (registry/git/wildcard/unmaintained) is Rust-only** — Go gets reachability-aware vuln scanning (govulncheck) on top of the cross-ecosystem lockfile SCA, but no source-trust gate.

## 4. Per-language code analysis matrix

This is the adapter layer — what each language's linter/typechecker/test step enforces. Columns are blank (—) where the language has no such concept or no step.

| Capability | Rust (clippy/rustfmt/cargo) | Python (ruff/mypy/pytest) | Go (golangci-lint/gofumpt/govulncheck/go test) | TypeScript (eslint/tsc) | JavaScript (eslint) | Ansible (ansible-lint) | YAML (yamllint) |
|---|:--:|:--:|:--:|:--:|:--:|:--:|:--:|
| General linting, warnings-as-errors | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Strict static typing | ✅ (native + clippy) | ✅ (mypy `strict`) | ✅ (native, compiler-enforced) | ✅ (tsc full `strict` + type-aware eslint) | — | — | — |
| In-language security lint rules | ✅ (safety lints) | ✅ (ruff `S` / bandit port) | ✅ (gosec) | ✅ (no-eval, no-new-func, no-script-url + type-aware) | ✅ (no-eval family) | ✅ (production profile) | ⚠️ (structural hazards only) |
| Runtime-panic / unwrap safety | ✅ (`unwrap_used`, `panic`, `indexing_slicing`, …) | — | — | — | — | — | — |
| Memory-safety (`unsafe` denied) | ✅ (`-D unsafe_code`) | — | ⚠️ (gosec `G103` flags `unsafe`) | — | — | — | — |
| Silent error swallowing | ✅ (`map_err_ignore`, `let_underscore_must_use`) | — | ✅ (`errcheck`, `nilerr`, `errorlint`) | ⚠️ (empty-catch via `no-empty`) | ⚠️ (empty-catch via `no-empty`) | — | — |
| Async safety (floating/misused promises) | — | — | — | ✅ (`no-floating-promises`, `no-misused-promises`) | — | — | — |
| Debug / placeholder code blocked | ✅ (`dbg_macro`, `todo`, `unimplemented`) | ✅ (ruff debug/print rules) | — (prints allowed, as in Rust) | ✅ (`no-debugger`, `no-alert`) | ✅ (`no-debugger`, `no-alert`) | — | — |
| Suppression hygiene (no blanket suppress) | ✅ (`allow_attributes*` → must use `#[expect(reason)]`) | ⚠️ (bare `noqa` not blocked) | ✅ (`nolintlint` → must name linter + explain) | ⚠️ (eslint-disable not blocked) | ⚠️ (eslint-disable not blocked) | — | — |
| Formatting enforced | ✅ (rustfmt `--check`) | ⚠️ (via ruff lint rules) | ✅ (gofumpt + goimports) | ⚠️ (eslint stylistic) | ⚠️ (eslint stylistic) | — | — |
| Test suite executed | ✅ (`cargo test`) | ✅ (pytest) | ✅ (`go test ./...`) | — | — | — | — |
| React hooks correctness (rules-of-hooks) | — | — | — | ✅ (`.tsx`) | ✅ (`.jsx`) | — | — |
| Svelte component check (svelte-check + eslint-svelte) | — | — | — | ✅ (`.svelte` w/ `lang="ts"`) | ✅ (`.svelte`) | — | — |
| IaC secret-logging hygiene (`no_log`) | — | — | — | — | — | ✅ | — |
| YAML structural hazards (dup keys, octal trap, truthy) | — | — | — | — | — | — | ✅ |

Notes:

- **TS vs JS:** TypeScript files get the full type-aware tier (`strictTypeChecked` + `stylisticTypeChecked` + `tsc --strict`); JavaScript files get the discipline/security core plus JS-only correctness rules but no type program, so strict typing, async-promise safety, and the `no-unsafe-*` family don't apply. A node project containing TypeScript **must declare its `tsconfig`** in `gate-config.json` or the gate fails closed.
- **React / Svelte:** both ride the node adapter. React adds `rules-of-hooks` + `exhaustive-deps` (errors) on `.jsx`/`.tsx`. Svelte is typechecked by `svelte-check` (against the same wrapped tsconfig) and linted by eslint-plugin-svelte; a `.svelte` component using `<script lang="ts">` triggers the same **tsconfig-required** honest-map rule as a standalone `.ts` file (the harness content-sniffs `.svelte` for `lang="ts"`).
- **Tests:** Rust, Python, and Go run the existing suite (a failing suite fails the gate); the node adapter has no test step. None of them *mandate* that meaningful tests exist — see the gaps list in [coverage.md](coverage.md#what-the-gate-does-not-prevent-gaps--non-goals).
- **Config forcing:** every cell above runs against the gate's own config, force-injected; the scanned repo's `.clippy.toml`/`ruff.toml`/`tsconfig.json`/`.yamllint`/`deny.toml`/etc. are ignored, so a repo cannot relax any of these by editing its own config.

## 5. What no column covers

These hold for every language — a green gate does **not** promise them. Full detail in [coverage.md](coverage.md#what-the-gate-does-not-prevent-gaps--non-goals).

- Meaningful tests exist (only that the existing suite passes) · test-coverage thresholds.
- Runtime / behavioral / business-logic correctness — the reviewer is a strict linter, not a human.
- IaC / Kubernetes-manifest / Dockerfile / Terraform security posture (no `trivy config` / Checkov / kube-linter step).
- Integrity of the gate image or the signing key (supply-chain trust root, handled by pinning + secret store + Kyverno).
- Dependency *source* trust for non-Rust ecosystems (registry/git/wildcard gating is cargo-deny-only).

## Keeping this matrix honest

This grid is derived from `config/` — the manifests, lint levels, and scanner flags. When you change a check, add/remove a tool, or add a language, update the matching cell here **and** the prose in [coverage.md](coverage.md) in the same change, then bump the gate tag.
