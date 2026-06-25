# grizzly-gate — the grizzly-platform CI gate image.
#
# One versioned artifact: the Rust orchestration harness + every per-language
# adapter and pinned scanner it drives via gate.toml. CI pulls this image,
# runs it against the source + built image, and on pass the harness signs the
# image digest with cosign. Update the gate = bump the pins here + the tag.

# ── Stage 1: build the harness ──────────────────────────────────────────────
FROM rust:1.83-slim-bookworm AS harness
WORKDIR /build
COPY harness ./harness
COPY gate.toml ./gate.toml
RUN cargo build --release --manifest-path harness/Cargo.toml \
    && cp harness/target/release/grizzly-gate /usr/local/bin/grizzly-gate

# ── Stage 2: runtime with all adapters + scanners ───────────────────────────
FROM debian:bookworm-slim

# Pinned tool versions — bump deliberately, never float.
ARG RUST_VERSION=1.83.0
ARG CARGO_DENY_VERSION=0.16.3
ARG COSIGN_VERSION=2.4.1
ARG TRIVY_VERSION=0.58.1
ARG GITLEAKS_VERSION=8.21.2
ARG NODE_VERSION=20.18.1
ARG SEMGREP_VERSION=1.97.0
ARG RUFF_VERSION=0.8.4
ARG MYPY_VERSION=1.13.0
ARG PYTEST_VERSION=8.3.4
ARG ANSIBLE_LINT_VERSION=24.12.2
ARG YAMLLINT_VERSION=1.35.1
ARG ESLINT_VERSION=9.17.0
ARG TYPESCRIPT_VERSION=5.7.2
ARG SEMGREP_RULES_REF=v1.97.0

ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/usr/local/cargo/bin:/usr/local/node/bin:/usr/local/bin:$PATH \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo

# Base OS deps + shellcheck (apt-pinned to the bookworm release).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils \
        build-essential pkg-config libssl-dev \
        python3 python3-pip python3-venv \
        shellcheck \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain (clippy + rustfmt) via rustup, plus cargo-deny (prebuilt).
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y \
        --default-toolchain "${RUST_VERSION}" \
        --profile minimal --component clippy rustfmt \
    && curl -fsSL "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
        | tar -xz -C /tmp \
    && mv "/tmp/cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl/cargo-deny" /usr/local/cargo/bin/cargo-deny

# Node (pinned tarball) + global eslint/typescript.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
        | tar -xJ -C /usr/local \
    && mv "/usr/local/node-v${NODE_VERSION}-linux-x64" /usr/local/node \
    && npm install -g "eslint@${ESLINT_VERSION}" "typescript@${TYPESCRIPT_VERSION}"

# Python tooling in an isolated venv on PATH.
RUN python3 -m venv /usr/local/pyenv \
    && /usr/local/pyenv/bin/pip install --no-cache-dir \
        "semgrep==${SEMGREP_VERSION}" \
        "ruff==${RUFF_VERSION}" \
        "mypy==${MYPY_VERSION}" \
        "pytest==${PYTEST_VERSION}" \
        "ansible-lint==${ANSIBLE_LINT_VERSION}" \
        "yamllint==${YAMLLINT_VERSION}"
ENV PATH=/usr/local/pyenv/bin:$PATH

# cosign, trivy, gitleaks (prebuilt binaries, pinned).
RUN curl -fsSL "https://github.com/sigstore/cosign/releases/download/v${COSIGN_VERSION}/cosign-linux-amd64" \
        -o /usr/local/bin/cosign && chmod +x /usr/local/bin/cosign \
    && curl -fsSL "https://github.com/aquasecurity/trivy/releases/download/v${TRIVY_VERSION}/trivy_${TRIVY_VERSION}_Linux-64bit.tar.gz" \
        | tar -xz -C /usr/local/bin trivy \
    && curl -fsSL "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz" \
        | tar -xz -C /usr/local/bin gitleaks

# Vendor a pinned Semgrep ruleset (offline, no registry fetch at scan time)
# and warm the Trivy vuln DB so scans are reproducible at this build's point.
RUN git clone --depth 1 --branch "${SEMGREP_RULES_REF}" \
        https://github.com/semgrep/semgrep-rules /etc/grizzly-gate/semgrep \
    && rm -rf /etc/grizzly-gate/semgrep/.git \
    && trivy image --download-db-only

COPY --from=harness /usr/local/bin/grizzly-gate /usr/local/bin/grizzly-gate
COPY gate.toml /etc/grizzly-gate/gate.toml

# Default config path so callers can just `grizzly-gate --source ... --image ...`.
ENV GRIZZLY_GATE_DEFAULT_CONFIG=/etc/grizzly-gate/gate.toml
ENTRYPOINT ["/usr/local/bin/grizzly-gate"]
