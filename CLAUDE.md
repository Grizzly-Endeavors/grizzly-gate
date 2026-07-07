# grizzly-gate

The CI gate for the [grizzly-platform](https://github.com/Grizzly-Endeavors/grizzly-platform) project family: one versioned container image that runs every per-language check + scanner against a repo's source and built image, and on a clean pass cosign-signs the image digest. **The gate is the reviewer** — a green gate is what lets code ship without a human reading every diff. See `README.md` for the design overview, `docs/coverage.md` for the threat model, and `docs/decisions/` for the ADRs.

## Layout

- `Dockerfile` — the single versioned artifact: the harness binary + every pinned adapter and scanner. **Tool versions are pinned here via `ARG` and are the single source of truth** — bump deliberately, never float a tag.
- `config/` — the declarative rule tree the harness executes. One self-describing dir per tool: `languages/<lang>/` (adapter: `manifest.toml` + native config + `[detect]` block) and `util/<tool>/` (always-on scanners). `detect.toml` holds Ops-owned `skip_dirs` + the denylist of un-adapted code languages.
- `harness/` — the Rust orchestration binary (`src/{main,config,detect,gateconfig,report,mcp}.rs`). Loads the config tree (fails closed on empty), verifies the repo's `gate-config.json` honest map against a hostile tree walk, runs adapters per declared project, then signs on a clean pass. The shared run logic lives in `main::execute` (logs to an injectable writer); `mcp.rs` reuses it to serve the gate over MCP (stdio JSON-RPC) via the `grizzly-gate mcp` subcommand.
- `plugin/` — a Claude Code plugin that wraps the local pre-check for the family: `/grizzly-gate:onboard` + `/grizzly-gate:check` skills, a Sonnet `gate-fixer` agent, a `grizzly-gate` MCP server (the gate image in `mcp` mode — lets Claude run the gate and pull one check's output at a time instead of swallowing `report.json` whole), and opt-in guardrail hooks (push-block, plus docker/un-adapted-language/suppression warnings). `bin/grizzly-gate` is the single source of truth for the local docker-run invocation (`scripts/grizzly-gate-local.sh` is a thin shim over it); `bin/grizzly-gate-mcp` is its MCP-mode sibling. A `.grizzly-gate-disabled` marker at a consumer repo's root turns every plugin hook inert there. `.claude-plugin/marketplace.json` at the repo root publishes it as the `grizzly-endeavors` marketplace. See `plugin/README.md`.

## Working in the harness

```sh
cd harness
cargo build            # debug build
cargo test             # unit tests live in config.rs / detect.rs / gateconfig.rs
cargo clippy --all-targets --all-features
cargo fmt
```

The harness is itself gated by these exact rules — `harness/clippy.toml`-class strictness is dogfooded. Treat the strict lint/deny config as the floor (see "Agent discipline" below).

## How the image is built & released

This repo holds the source; the **image is built in-cluster by grizzly-platform** (Argo Workflows + rootless BuildKit), not by this repo's CI directly:

- `.github/workflows/build-gate-image.yaml` (here) triggers on push to `main` and on `workflow_dispatch` — it submits the `build-gate-image` Argo `WorkflowTemplate` (which lives in grizzly-platform) and polls it.
- That template clones this repo, builds from the repo root, and pushes `grizzly-gate:{latest, <version>, <uid>}` to the in-cluster zot registry.
- **Cut a release:** `workflow_dispatch` with `version=vX.Y.Z`. Apps pin via `gate_version` on the reusable `gate.yaml` workflow.
- Runs on the org's self-hosted `lab-runners` (ARC), which reach the in-cluster Argo server.

**Dev-distribution image (Docker Hub).** Separately from the authoritative in-cluster build, a convenience image is published to `bearflinn/grizzly-gate:{latest,<sha>}` so local developers can pull and run the gate as a pre-check (`scripts/grizzly-gate-local.sh`, `docs/using-the-gate.md`). This image **signs nothing** — it's for local pre-checks only. It's published **multi-arch** (`linux/amd64` + `linux/arm64`) via `docker buildx` so it runs natively on Apple Silicon Macs; the amd64 leg stays byte-identical to the in-cluster BuildKit build (local pass == CI pass). Maintainers publish it with `scripts/publish-image.sh` (which builds both arches — the arm64 leg cross-builds via QEMU, so a plain-Docker publish host needs `docker run --privileged --rm tonistiigi/binfmt --install arm64` once); activate the automatic on-push publish per-machine with `ln -sf ../../scripts/hooks/pre-push .git/hooks/pre-push` (backgrounded, never blocks `git push`; skip one push with `GRIZZLY_GATE_NO_PUBLISH=1`). Requires `docker login` to the target repo.

## Changing the rules

Edit the relevant tool dir under `config/`, bump pins in the `Dockerfile` if needed, update `docs/coverage.md` in the same change, then cut a new tag. The gate's config is **authoritative** — it is force-injected onto each tool and ignores the scanned repo's own config of the same kind. Adding a language is a deliberate two-part change: a new adapter under `languages/` **and** its detection rules (`[detect]` block + removal from the `detect.toml` denylist). See [ADR-029](docs/decisions/029-gate-config-honest-map.md).

## Agent discipline

The strict lint configs, `-D warnings`, `deny.toml`, and max-denial scanner settings here are **intentional, not over-engineering** — they're the product. If you find lazy code, add a guardrail that prevents the whole class, don't just fix the instance. **Do not relax lint rules, disable checks, or bypass hooks** without explicit approval. For a genuinely-wrong rule in a specific spot, use a scoped `#[expect(..., reason = "...")]` (or the tool's equivalent with a written reason), never a blanket allow.

## Relationship to grizzly-platform

This repo owns the gate *source*. The platform owns the *integration*: the Argo build template, the reusable `gate.yaml` consumer workflow, the cosign signing key (OpenBao + External Secrets), Kyverno admission enforcement, and the operator runbook. When a local checkout of the platform is available, its path is recorded in `CLAUDE.local.md` (gitignored).

## grizzly-gate (CI gate)

This repo is gated by **its own gate** — grizzly-gate runs every per-language check + scanner against itself, and the gate is the reviewer: a green gate is what lets code ship without a human reading every diff. It is strict on purpose and fails **closed** — anything it cannot positively verify fails. We do not get to weaken the gate to make the gate pass.

**The honest map (`gate-config.json`).** The repo root ships `gate-config.json` declaring one project: `rust` @ `harness`. The rest of the tree is the config rule tree (`config/`), the plugin, docs, and scripts — no other adapter-backed source. It can only *declare*, never weaken a check; a hostile tree walk confirms it, so it must match reality.

**Self-gating wrinkle (ADR-033).** The gate's own ESLint flat config would be a live `.mjs` in `config/languages/node/` — which the honest-map walk would flag as undeclared node code. So it ships as `eslint.config.mjs.tmpl` (a non-detected extension) and the harness materializes the live `.mjs` beside its `node_modules` at run time, then removes it. When you add another gate-owned config that must carry an adapter-detected extension, follow the same template pattern — never an exclude.

**Checking locally.** When the plugin's push-block hook is enabled it runs the gate automatically and blocks Claude's `git push` on a red result — so you don't pre-run it just to push. Run `/grizzly-gate:check` (or `grizzly-gate` from the repo root) yourself while iterating on a fix, or before a manual terminal push (the hook only guards Claude's pushes). It runs the CI image against your working tree and writes `grizzly-gate-report/report.json`; a local pass means a CI pass for everything except cosign signing and image-layer CVE/SBOM scanning. Note: a harness change only takes effect once a new image is built, so end-to-end node behavior is validated in CI after the image rebuilds.

**When it fails.** Hand it to the `gate-fixer` agent — it reads the report and fixes violations in this repo's own code or its `gate-config.json`. Never relax a rule, disable a check, or add an ignore/exclude to get past the gate. A lint suppression is a last resort that needs the user's sign-off — prefer refactoring the code so it is not needed.

## Markdown

When writing or editing `.md` files, don't hard-wrap paragraphs — let each paragraph and each bullet be one continuous line that soft-wraps. Keep newlines only between blocks (paragraphs, list items, headings).
