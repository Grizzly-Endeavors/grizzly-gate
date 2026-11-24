# grizzly-gate

The CI gate for the [grizzly-platform](https://github.com/Grizzly-Endeavors/grizzly-platform) project family: one versioned container image running per-language checks and scanners against a repo's source and built image, cosign-signing the image digest on a clean pass. Universal rules live in `~/Projects/AGENTS.md`.

## Layout

- `Dockerfile` — single source of truth for pinned tool versions (`ARG`); never float a tag.
- `config/` — authoritative declarative rule tree force-injected onto each tool (`languages/<lang>/`, `util/<tool>/`, and `detect.toml`).
- `harness/` — Rust orchestration binary (`grizzly-gate` CLI + `grizzly-gate mcp` stdio JSON-RPC server). Leaves the scanned working tree clean by routing per-project state to `{work}`, caches to `/cache`, and cleaning up `node_modules` / `*.egg-info` before exit.
- `plugin/` — Claude Code plugin (`/grizzly-gate:onboard`, `/grizzly-gate:check`, `gate-fixer` agent, MCP server, opt-in push-block hooks; `.grizzly-gate-disabled` disables hooks per repo).

## Working in the harness

```sh
cd harness
cargo build
cargo test
cargo clippy --all-targets --all-features
cargo fmt
```

## Build & Release

- **In-cluster image (authoritative):** `.github/workflows/build-gate-image.yaml` triggers on push to `main` and `workflow_dispatch` (`version=vX.Y.Z`), submitting the `build-gate-image` Argo `WorkflowTemplate` (owned by `grizzly-platform`) on self-hosted `lab-runners` to push `grizzly-gate:{latest,<version>,<uid>}` to the in-cluster zot registry.
- **Dev image (Docker Hub, unsigned):** `scripts/publish-image.sh` builds multi-arch `bearflinn/grizzly-gate:{latest,<sha>}` via `docker buildx` (arm64 requires `docker run --privileged --rm tonistiigi/binfmt --install arm64` once). Optional background push hook: `ln -sf ../../scripts/hooks/pre-push .git/hooks/pre-push` (skip once with `GRIZZLY_GATE_NO_PUBLISH=1`).

## Self-Gating Rules

- This repo is gated by its own gate (`gate-config.json` declares `rust` @ `harness`). Never weaken checks or add excludes to pass.
- **Template pattern for gate-owned configs:** Any gate-owned config file with an adapter-detected extension (like `config/languages/node/eslint.config.mjs.tmpl`) must ship as `.tmpl` and be materialized at runtime so the honest-map walk does not flag it as undeclared source.
- **Local pre-check:** Run `/grizzly-gate:check` or `grizzly-gate` from repo root (writes `grizzly-gate-report/report.json`). A harness change only takes effect in-cluster once the image rebuilds.
- **Relationship to `grizzly-platform`:** This repo owns gate source; `grizzly-platform` owns the Argo template, reusable `gate.yaml`, cosign keys (OpenBao + External Secrets), Kyverno admission policy, and operator runbook. Local checkout path (if present) lives in gitignored `CLAUDE.local.md`.
