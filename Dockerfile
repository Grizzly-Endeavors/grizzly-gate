# grizzly-gate — the grizzly-platform CI gate image.
#
# One versioned artifact: the Rust orchestration harness + every per-language
# adapter and pinned scanner it drives via the config/ tree. CI pulls this
# image, runs it against the source + built image, and on pass the harness signs
# the image digest with cosign. Update the gate = bump the pins here + the tag.

# Single source of truth for the Rust toolchain version (the harness build AND
# the clippy/rustfmt the gate runs). Hard floor is 1.85: a transitive dep (clap)
# needs edition2024 cargo support, stabilized in 1.85.
ARG RUST_IMAGE=rust:1.96-slim-bookworm

# ── Stage 1: build the harness ──────────────────────────────────────────────
FROM ${RUST_IMAGE} AS harness
WORKDIR /build
COPY harness ./harness
RUN cargo build --release --manifest-path harness/Cargo.toml \
    && cp harness/target/release/grizzly-gate /usr/local/bin/grizzly-gate

# ── Stage 2: Rust lint toolchain (clippy + rustfmt) ─────────────────────────
# Copied into the runtime stage below instead of installed via rustup-init —
# rustup-init aborts under arm64 QEMU on the (amd64) multi-arch build host. The
# official rust image already ships a per-arch toolchain (its arm64 variant is
# built natively), and `rustup component add` runs fine emulated; only the
# rustup-init bootstrap is the problem.
FROM ${RUST_IMAGE} AS rusttoolchain
RUN rustup component add clippy rustfmt

# ── Stage 3: runtime with all adapters + scanners ───────────────────────────
FROM debian:bookworm-slim

# Target architecture, populated automatically by buildx (amd64|arm64). Declared
# WITHOUT a default on purpose: a Dockerfile default would override buildx's
# auto-populated value and pin every leg to one arch. Each tool download below
# maps this to that tool's own arch naming via an inline `case` whose `*)` branch
# is the amd64 token — so a builder that doesn't set TARGETARCH at all (e.g.
# Kaniko on the amd64 in-cluster build, where it's empty) still gets the exact
# amd64 image. That `*)` fallback, not an ARG default, is the local-amd64 == CI
# linchpin.
ARG TARGETARCH

# Pinned tool versions — bump deliberately, never float. (The Rust toolchain
# version is RUST_IMAGE, above the first stage.)
# 0.19.x required: older cargo-deny can't parse RUSTSEC advisories that use
# CVSS 4.0 vectors (fails advisory-db load on current entries).
ARG CARGO_DENY_VERSION=0.19.9
# Held at the latest 2.x line: cosign 3.x flips on the new OCI-1.1 referrers +
# protobuf signature format by default, which is a cross-repo migration with the
# platform's Kyverno verifier (tracked in issue #1) — not a drop-in bump.
ARG COSIGN_VERSION=2.6.3
ARG TRIVY_VERSION=0.71.2
ARG GITLEAKS_VERSION=8.30.1
# OSV-Scanner: cross-ecosystem dependency SCA (vulns + license allowlist).
ARG OSV_SCANNER_VERSION=2.4.0
# Node 22 (Active LTS "Jod"): Node 20 reached end-of-life in Apr 2026.
ARG NODE_VERSION=22.23.1
ARG SEMGREP_VERSION=1.168.0
ARG RUFF_VERSION=0.15.20
ARG MYPY_VERSION=2.1.0
ARG PYTEST_VERSION=9.1.1
ARG ANSIBLE_LINT_VERSION=26.4.0
# ansible-lint needs ansible-core>=2.16.14; pin it exactly for determinism.
# Capped at the 2.19.x line: ansible-core 2.20+ requires Python >=3.12, but this
# image's Debian bookworm base ships Python 3.11. (Moving past this means bumping
# the base Python, a separate change.)
ARG ANSIBLE_CORE_VERSION=2.19.11
ARG YAMLLINT_VERSION=1.38.0
# Held at ESLint 9 (latest 9.x): eslint-plugin-react still caps at ^9.7, so
# ESLint 10 is blocked on that one plugin (tracked in issue #2).
ARG ESLINT_VERSION=9.39.4
ARG TYPESCRIPT_VERSION=6.0.3
# typescript-eslint (parser + plugin) for type-aware TS linting. 8.62 supports
# ESLint 8/9/10 and TypeScript up to <6.1 — covers the TS 6.0 + ESLint 9 pins.
ARG TS_ESLINT_VERSION=8.62.0
# React + Svelte eslint toolchain, installed into the node config dir alongside
# the TS stack (all peer-compatible with ESLint 9). React: rules-of-hooks + the
# recommended correctness set. Svelte: eslint-plugin-svelte (needs the parser) +
# svelte + svelte-check (the `.svelte` type-aware checker, the tsc analog).
ARG ESLINT_PLUGIN_REACT_VERSION=7.37.5
ARG ESLINT_PLUGIN_REACT_HOOKS_VERSION=7.1.1
ARG ESLINT_PLUGIN_SVELTE_VERSION=3.20.0
ARG SVELTE_ESLINT_PARSER_VERSION=1.8.0
ARG SVELTE_VERSION=5.56.4
ARG SVELTE_CHECK_VERSION=4.7.1
# Go toolchain + golangci-lint (the clippy-grade lint/format floor) + govulncheck
# (reachability-aware dependency vuln scan).
ARG GO_VERSION=1.26.4
ARG GOLANGCI_LINT_VERSION=2.12.2
ARG GOVULNCHECK_VERSION=1.5.0
# semgrep-rules has its own ref scheme (not aligned to the semgrep CLI version);
# its default branch is `develop`. TODO: pin to a commit SHA for reproducibility.
ARG SEMGREP_RULES_REF=develop

ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/usr/local/go/bin:/usr/local/cargo/bin:/usr/local/node/bin:/usr/local/bin:$PATH \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    # GOTOOLCHAIN=local pins to the toolchain installed below — a scanned repo's
    # go.mod `toolchain` directive can't pull a different Go (gate determinism).
    GOTOOLCHAIN=local

# Base OS deps + shellcheck (apt-pinned to the bookworm release).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils \
        build-essential pkg-config libssl-dev \
        python3 python3-pip python3-venv \
        shellcheck \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain (clippy + rustfmt): copied from the rusttoolchain stage rather
# than bootstrapped via rustup-init (which aborts under arm64 QEMU). RUSTUP_HOME
# and CARGO_HOME match the rust image's paths (set in the ENV block above).
COPY --from=rusttoolchain /usr/local/rustup /usr/local/rustup
COPY --from=rusttoolchain /usr/local/cargo /usr/local/cargo

# cargo-deny (prebuilt static-musl binary; arch-mapped).
RUN case "$TARGETARCH" in arm64) DENY_ARCH=aarch64 ;; *) DENY_ARCH=x86_64 ;; esac \
    && curl -fsSL "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/cargo-deny-${CARGO_DENY_VERSION}-${DENY_ARCH}-unknown-linux-musl.tar.gz" \
        | tar -xz -C /tmp \
    && mv "/tmp/cargo-deny-${CARGO_DENY_VERSION}-${DENY_ARCH}-unknown-linux-musl/cargo-deny" /usr/local/cargo/bin/cargo-deny

# Node (pinned tarball). The eslint/typescript/typescript-eslint toolchain is
# NOT installed globally: ESLint flat config resolves plugins from the config
# file's node_modules, so it's installed into the node config dir after the
# config tree is copied in (below).
RUN case "$TARGETARCH" in arm64) NODE_ARCH=arm64 ;; *) NODE_ARCH=x64 ;; esac \
    && curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
        | tar -xJ -C /usr/local \
    && mv "/usr/local/node-v${NODE_VERSION}-linux-${NODE_ARCH}" /usr/local/node

# Python tooling — each CLI in its own isolated venv via pipx, so their
# transitive deps can't conflict (semgrep and ansible-lint are not
# co-installable in one environment). pipx entrypoints land in /usr/local/bin.
ENV PIPX_HOME=/opt/pipx \
    PIPX_BIN_DIR=/usr/local/bin
RUN pip install --no-cache-dir --break-system-packages pipx \
    && pipx install "semgrep==${SEMGREP_VERSION}" \
    && pipx inject --force semgrep "setuptools<81" \
    && pipx install "ruff==${RUFF_VERSION}" \
    && pipx install "mypy==${MYPY_VERSION}" \
    && pipx install "pytest==${PYTEST_VERSION}" \
    && pipx install "ansible-lint==${ANSIBLE_LINT_VERSION}" \
    && pipx inject --force ansible-lint "ansible-core==${ANSIBLE_CORE_VERSION}" \
    && pipx install "yamllint==${YAMLLINT_VERSION}"

# cosign, trivy, gitleaks, osv-scanner (prebuilt binaries, pinned).
RUN case "$TARGETARCH" in \
        arm64) COSIGN_ARCH=arm64; TRIVY_ARCH=ARM64; GITLEAKS_ARCH=arm64; OSV_ARCH=arm64 ;; \
        *)     COSIGN_ARCH=amd64; TRIVY_ARCH=64bit; GITLEAKS_ARCH=x64; OSV_ARCH=amd64 ;; \
    esac \
    && curl -fsSL "https://github.com/sigstore/cosign/releases/download/v${COSIGN_VERSION}/cosign-linux-${COSIGN_ARCH}" \
        -o /usr/local/bin/cosign && chmod +x /usr/local/bin/cosign \
    && curl -fsSL "https://github.com/aquasecurity/trivy/releases/download/v${TRIVY_VERSION}/trivy_${TRIVY_VERSION}_Linux-${TRIVY_ARCH}.tar.gz" \
        | tar -xz -C /usr/local/bin trivy \
    && curl -fsSL "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_linux_${GITLEAKS_ARCH}.tar.gz" \
        | tar -xz -C /usr/local/bin gitleaks \
    && curl -fsSL "https://github.com/google/osv-scanner/releases/download/v${OSV_SCANNER_VERSION}/osv-scanner_linux_${OSV_ARCH}" \
        -o /usr/local/bin/osv-scanner && chmod +x /usr/local/bin/osv-scanner

# Go toolchain (drives `go test` + govulncheck's package loading), golangci-lint
# (prebuilt v2 release — the lint/format floor), and govulncheck (`go install`ed
# to /usr/local/bin so it lands on PATH). Go is on PATH via the ENV block above.
RUN case "$TARGETARCH" in arm64) GO_ARCH=arm64 ;; *) GO_ARCH=amd64 ;; esac \
    && curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-${GO_ARCH}.tar.gz" \
        | tar -xz -C /usr/local \
    && curl -fsSL "https://github.com/golangci/golangci-lint/releases/download/v${GOLANGCI_LINT_VERSION}/golangci-lint-${GOLANGCI_LINT_VERSION}-linux-${GO_ARCH}.tar.gz" \
        | tar -xz -C /tmp \
    && mv "/tmp/golangci-lint-${GOLANGCI_LINT_VERSION}-linux-${GO_ARCH}/golangci-lint" /usr/local/bin/golangci-lint \
    # CGO_ENABLED=0: govulncheck is pure Go, and a cgo build invokes the C
    # compiler, which fails when this leg is cross-built under QEMU emulation
    # (gcc gets an `-m64` it doesn't recognize). Yields a static binary; the
    # amd64 leg is unaffected in behavior (and Kaniko builds the same file).
    && GOBIN=/usr/local/bin CGO_ENABLED=0 go install "golang.org/x/vuln/cmd/govulncheck@v${GOVULNCHECK_VERSION}"

# Ship the gate's config tree (per-tool manifests + native configs), then vendor
# a pinned Semgrep ruleset into the semgrep tool dir (offline, no registry fetch
# at scan time) and warm the Trivy vuln DB so scans are reproducible at this
# build's point.
COPY config/ /etc/grizzly-gate/config/

# Install the gate's Node lint toolchain into the node config dir, so the flat
# config (`eslint.config.mjs`) resolves typescript-eslint, the React plugins, and
# eslint-plugin-svelte from a node_modules beside it (and `.svelte` files get the
# svelte-check typechecker). Pinned, exact versions — the Dockerfile ARGs are the
# single source of truth (no committed package.json).
RUN cd /etc/grizzly-gate/config/languages/node \
    && npm init -y >/dev/null 2>&1 \
    && npm install --no-fund --no-audit --save-exact \
        "eslint@${ESLINT_VERSION}" \
        "typescript@${TYPESCRIPT_VERSION}" \
        "typescript-eslint@${TS_ESLINT_VERSION}" \
        "eslint-plugin-react@${ESLINT_PLUGIN_REACT_VERSION}" \
        "eslint-plugin-react-hooks@${ESLINT_PLUGIN_REACT_HOOKS_VERSION}" \
        "eslint-plugin-svelte@${ESLINT_PLUGIN_SVELTE_VERSION}" \
        "svelte-eslint-parser@${SVELTE_ESLINT_PARSER_VERSION}" \
        "svelte@${SVELTE_VERSION}" \
        "svelte-check@${SVELTE_CHECK_VERSION}"

RUN SEMGREP_RULES=/etc/grizzly-gate/config/util/semgrep/rules \
    && git clone --depth 1 --branch "${SEMGREP_RULES_REF}" \
        https://github.com/semgrep/semgrep-rules "${SEMGREP_RULES}" \
    && rm -rf "${SEMGREP_RULES}/.git" \
    # semgrep loads every YAML under --config as a rule and hard-fails on any
    # non-rule file (the repo ships meta/CI/test YAML: .pre-commit-config.yaml,
    # stats/, .github/, *.test.yaml, …). Drop every YAML lacking a top-level
    # `rules:` key. Content-based rather than path-based so an upstream layout
    # change on the (unpinned) ref can't silently reintroduce a breaking file.
    && find "${SEMGREP_RULES}" -type f \( -name '*.yml' -o -name '*.yaml' \) \
        -exec sh -c 'grep -qE "^rules:" "$1" || rm -f "$1"' _ {} \; \
    && trivy image --download-db-only

COPY --from=harness /usr/local/bin/grizzly-gate /usr/local/bin/grizzly-gate

# Default config root so callers can just `grizzly-gate --source ... --image ...`.
ENV GRIZZLY_GATE_CONFIG_DIR=/etc/grizzly-gate/config
ENTRYPOINT ["/usr/local/bin/grizzly-gate"]
