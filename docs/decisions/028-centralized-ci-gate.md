# ADR-028: Centralized CI Gate with Cosign Attestation and Kyverno Admission

**Date:** 2026-06-25
**Status:** accepted
**Adapted from:** [grizzly-platform ADR-028](https://github.com/Grizzly-Endeavors/grizzly-platform/blob/master/docs/decisions/028-centralized-ci-gate.md) — the original decision record, made in the platform that runs this gate. This copy keeps the gate-design rationale; the platform-integration specifics (ARC, OpenBao, Kyverno, Argo) describe that canonical instance.

## Context

Every app repo in `grizzly-endeavors` hand-rolls its own CI: lint/test steps drift between repos, several have no checks at all, and there is zero supply-chain enforcement — Flux + the kubelet will deploy any image that lands in the registry. The goal is to let agents ship code without per-PR human review by making CI itself the reviewer: one centrally-owned set of checks, and a deploy boundary that refuses anything those checks didn't bless.

The platform already has the substrate: ARC v2 self-hosted runners with a DinD sidecar that pushes to the in-cluster registry, Argo+Kaniko for in-cluster image builds, OpenBao+ESO for secrets, and Flux for delivery. What's missing is (a) a single artifact that owns the checks, (b) proof that the checks ran, and (c) something at the deploy boundary that demands that proof.

## Decision

**A versioned gate container image owns the checks; a cosign signature on the image digest is the proof; Kyverno verifies the signature at admission.**

1. **The gate is a container image, not a service.** This repo (`Dockerfile` + `config/` + `harness/`) — a Rust orchestration harness plus every pinned per-language adapter and scanner it drives. It runs *against* the source and the already-built image; it does not re-clone or re-build (CI did that). Updating the rules = bump pins + cut a new tag. Built via the existing Argo+Kaniko path (`build-gate-image` WorkflowTemplate), pushed to the in-cluster registry, pinned by tag.
2. **Rules are data, not code — and the config is the gate's.** The harness executes a `config/` tree, one self-describing dir per tool under `languages/` (marker → checks, e.g. `Cargo.toml` → `cargo fmt/clippy -D warnings/deny/test`) and `util/` (gitleaks, an offline-pinned Semgrep ruleset, Trivy image SBOM/CVE). Each dir holds a `manifest.toml` (what to run) next to the tool's own native config; the manifest forces that gate-owned config onto the tool (`--config`/`--config-file`/`CLIPPY_CONF_DIR`/…) so the scanned repo's own config of the same kind is ignored — a repo cannot weaken the checks it is being gated by. The harness fails closed (zero checks run ⇒ fail) and only signs on a clean pass. *(Amended: the original design kept the rule set in a single `gate.toml` and let tools auto-discover repo-local config; superseded by the per-tool tree + authoritative config.)*
3. **Execution runs as a required CI job** via a reusable workflow (`.github/workflows/gate.yaml`, `workflow_call`, in the platform repo) that any repo calls after it builds+pushes by digest. The gate runs as `docker run grizzly-gate` on `lab-runners` through the existing DinD path; it reaches the registry via cluster DNS exactly as image pushes already do.
4. **Key-based cosign, key in OpenBao.** The gate signs the image *digest* with `cosign sign --key env://COSIGN_PRIVATE_KEY`. The private key + password live at `secret/grizzly-platform/cicd/cosign`, synced by ESO into `arc-runners` and exposed on the runner pod (`optional: true`, so runners still start before bootstrap). The signature decouples "checks ran" from "deploy allowed" — it is the only proof that travels forward.
5. **Enforcement is Kyverno admission.** `verify-gate-signature` ClusterPolicy (`verifyImages`, rule-level `failureAction`) verifies the cosign signature against the embedded public key, scoped to namespaces labelled `grizzly.io/gated=true`. Ships in **Audit**, flips to **Enforce** once all live first-party images are signed.
6. **Signature only for v1.** Kyverno verifies the signature exists and is valid; it does not yet require an SBOM attestation, nor enforce the platform "rule delta" (probes, resource requests, naming, ingress conventions). Those are scaffolded in `kyverno-policies/platform-rules.yaml.disabled` for a later phase.

## Alternatives Considered

- **A gate service that ingests repos.** Rejected — it re-does CI's clone-and-build in a stateful, bottlenecked service. CI already has the source and the built image; the gate consumes their output.
- **Keyless cosign (GitHub OIDC + public Fulcio/Rekor).** Stronger (identity-bound, no key to manage) but adds an egress dependency on public Sigstore to the CI/deploy path and anchors trust in GitHub. Rejected for a self-contained homelab; key-in-OpenBao matches existing secret patterns. Reconsider if key rotation burden grows.
- **Self-hosted Sigstore (Fulcio+Rekor).** Keyless benefits with no public dependency, but a substantial stateful service to run/back up — disproportionate at this scale.
- **sigstore policy-controller for enforcement.** Purpose-built for signature verification but narrower; Kyverno does signature verification *and* will carry the platform policy rules, so one engine covers both.
- **Per-repo copies of the checks.** Rejected — that is exactly the drift this replaces. Rules live in one image.
- **Centralizing the language toolchains in the runner image.** Rejected — it couples the runner to every language's tooling and there's no single versioned artifact to pin. The gate image is that artifact.

## Consequences

- **One place to change the rules.** A new lint or scanner = add/edit a tool dir under `config/` (+ Dockerfile pins), cut a tag, bump the `gate_version` apps pin. No N-pipeline edits.
- **The signature is the contract.** A green gate signs; an un-green gate doesn't; an unsigned image is refused at the (gated) boundary. Agents can merge/deploy without a human in the loop because the gate is the gate.
- **Trust assumption on the shared runner pool.** The cosign key is exposed to every job on `lab-runners` — acceptable for a single-tenant org; revisit with a dedicated gate runner pool if that changes. Documented in the [platform runbook](https://github.com/Grizzly-Endeavors/grizzly-platform/blob/master/docs/runbooks/ci-gate.md).
- **Kyverno is now load-bearing.** With `failurePolicy: Fail`, Kyverno being down blocks admission to gated namespaces — alerted as critical (`KyvernoDown`).
- **Rollout is staged.** Audit → sign live images → Enforce. The embedded public key ships as a placeholder; until the real key is in place and images are signed, the policy stays in Audit (see runbook).
- Registry implications (OCI referrers for storing signatures) are covered in [grizzly-platform ADR-027](https://github.com/Grizzly-Endeavors/grizzly-platform/blob/master/docs/decisions/027-registry-zot.md).
