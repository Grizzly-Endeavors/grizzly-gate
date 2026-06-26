# ADR-032: Install a repo's Python deps before mypy/pytest (uv)

**Date:** 2026-06-26
**Status:** accepted
**Relates to:** [ADR-029](029-gate-config-honest-map.md), [ADR-030](030-cross-ecosystem-sca.md), [ADR-031](031-go-svelte-react-coverage.md)

## Context

A real gate run against a FastAPI/Pydantic backend exposed a structural gap in the Python adapter: it ran `ruff`/`mypy`/`pytest` against the source **without ever installing the repo's dependencies**.

- **mypy** produced 25 errors — all `untyped-decorator` / `Class cannot subclass "BaseModel" (has type "Any")`. With `pydantic`/`fastapi` not importable, every third-party import resolved to `Any`; under `strict` (which includes `disallow_subclassing_any`), subclassing `BaseModel` then failed and cascaded. This would fail essentially every FastAPI/Pydantic repo.
- **pytest** produced 12 collection errors — `ModuleNotFoundError: No module named 'backend'`. The repo's own package was never installed, and `pytest -c {config}/pytest.ini` *replaced* the repo's `pytest.ini` (config-forcing, [ADR-029](029-gate-config-honest-map.md)) — discarding its `pythonpath`, so `from backend.api import …` could not resolve by rootdir either.

The Node adapter already solved the analogous problem: a `deps` check runs `npm ci` so `eslint`/`tsc` resolve types against the repo's real `node_modules`. Python had no equivalent. The complication unique to Python is that the gate's tools live in **isolated pipx venvs** (Dockerfile) — mypy and pytest cannot see a `pip install` done elsewhere — so "install the deps" is not enough; the tools have to be *pointed at* where the deps live.

This ADR adds the missing install step. The owner picked **uv** as the installer and the **hybrid** first-party-resolution contract (below).

## Decision

### A `deps` check that builds a per-project venv with uv

The Python adapter gains a `deps` check (first in the list, mirroring Node's `npm ci`) that creates `.venv` in the project dir and installs the repo into it:

```sh
uv venv .venv
[ -f requirements.txt ] && uv pip install -r requirements.txt
grep -qE "^\[(project|build-system)\]" pyproject.toml && uv pip install -e .
uv pip install "pytest==$GATE_PYTEST_VERSION"
```

- **uv, not pip**, for speed (this runs on every gate) and a real resolver. uv is a **gate-side implementation detail**: it uses the *pip interface* (`uv pip install` against a standard `requirements.txt` / `pyproject.toml`), **not** the uv-native project interface (`uv sync`/`uv.lock`). A scanned repo is therefore **never required to adopt uv** — Poetry/PDM/plain-pip repos install identically. uv is pinned in the Dockerfile like every other tool, and `UV_PYTHON_PREFERENCE=only-system` + `UV_PYTHON_DOWNLOADS=never` force it onto the image's pinned Python 3.11 (no network Python download → matches `mypy python_version = 3.11`).
- **Hybrid first-party resolution.** If the repo is an installable package (`pyproject.toml` has `[project]` or `[build-system]`) it is editable-installed (`-e .`), which covers `src/` layouts and resolves first-party imports via the venv. If it is a loose tree (e.g. a bare `backend/` package with a `requirements.txt` and `pythonpath = .`), the deps come from `requirements.txt` and first-party code resolves by **rootdir** (see the pytest/mypy wiring). Both are installed when both are present (editable picks up `[project.dependencies]`; `requirements.txt` covers apps that pin there). This was chosen over *strict editable-only* (rejected: breaks the very common loose-layout FastAPI backend until it adds packaging) and *deps+pythonpath only* (rejected: no editable means `src/` layouts break).
- **pytest must run *inside* the venv.** mypy has `--python-executable` to resolve imports against another interpreter while running from its own pipx venv; pytest has no such flag — it has to import the test modules and their deps in-process. So the gate-pinned pytest is installed into `.venv` and pytest is invoked as `.venv/bin/pytest`. The version is single-sourced from the Dockerfile's `PYTEST_VERSION` ARG, surfaced as the `GATE_PYTEST_VERSION` image env so the manifest can pin it without a second source of truth.

### Wiring the existing checks to the venv

- **mypy** gains `--python-executable .venv/bin/python`, so third-party imports resolve against the installed deps. `ignore_missing_imports = True` stays (a library that genuinely ships no stubs is not a code-quality signal); the difference now is that `pydantic`/`fastapi`/etc. *do* resolve, with their real `py.typed` types, so strict mode is meaningful instead of an `Any` cascade.
- **pytest** runs as `.venv/bin/pytest -c {config}/pytest.ini -q`, and the gate-owned `pytest.ini` gains `pythonpath = .` so the loose-layout case resolves first-party `backend.*` by rootdir. The gate config stays authoritative (config-forcing intact); `pythonpath = .` is a gate-set default, not the repo's discarded value being honored. (Test **discovery** layout — `testpaths` — remains the known-fragile, repo-shaped case flagged in `pytest.ini`; this ADR fixes *import* resolution, not discovery.)
- **ruff** is unchanged — a linter needs no import resolution.

### `.venv` is honest-map- and SCA-safe

- `.venv` is already in `detect.toml`'s Ops-owned `skip_dirs`, and the honest-map walk runs **before** the adapters, so the gate-created venv is never seen as first-party code (and could not be used to smuggle code in).
- The venv is **excluded from `trivy-fs`** (`scan.skip-dirs: [.venv, venv]`). This matters: unlike Node's `npm ci` (which installs *only* the repo's declared deps), our `.venv` also contains the **gate-injected pytest** and its transitive deps. Without the exclusion, trivy-fs's installed-package analyzer would scan them and a CVE in the gate's own pytest toolchain would be falsely attributed to the repo. No real coverage is lost: the repo's committed manifests/lockfiles are still scanned directly by both trivy-fs and osv-scanner. (Node is left as-is — its install is purely repo deps, so it has no false-attribution surface.)
- **osv-scanner** is the one scanner with no clean path-exclude flag in 2.4.0, but it only parses *recognized lockfile filenames* (requirements.txt, poetry.lock, uv.lock, …) — it does **not** read installed site-packages metadata the way trivy-fs's analyzer does, so an installed `.venv` is largely invisible to it, and it respects `.gitignore` (where `.venv` is conventionally listed). The narrow residual — a dependency that *vendors* a recognized lockfile inside its site-packages dir in a repo that doesn't gitignore `.venv` — fails closed (a spurious extra finding, never a missed one) and is acceptable; the clean fix if it ever bites is the out-of-tree-venv alternative below.

## Alternatives Considered

- **Strict editable-only contract.** `uv pip install -e .` as the only path; loose `backend/` + `requirements.txt` trees fail until they add PEP 621/517 packaging. Most "gate-like," but rejected by the owner as too much friction for a layout that is extremely common in the fleet's FastAPI services.
- **Deps + `pythonpath` only (never editable).** Simplest deps step, but `src/` layouts (package not at rootdir) break. Rejected.
- **Require a lockfile (`uv pip sync`), fail closed otherwise.** Most reproducible, but imposes a lockfile mandate on every Python consumer. Deferred: the hybrid installs from whatever standard inputs exist; a lockfile-required tightening can layer on later without re-architecting.
- **A new `{venv}` harness placeholder + out-of-tree venv (like the tsconfig wrapper).** Would keep the venv out of the working tree, but adds a Rust change and a cleanup path for no real gain: `.venv` is already a `skip_dirs` name, the read-write local mount already tolerates in-tree artifacts (`node_modules`, the tsconfig wrapper, the report dir), and the trivy-fs exclusion handles the one scanner that reads installed packages. Kept it pure config + Dockerfile.
- **Install pytest unpinned into the venv.** Rejected — floats a tool version, violating the pinned-tools guarantee. Single-sourced from the Dockerfile ARG via `GATE_PYTEST_VERSION`.

## Consequences

- **FastAPI/Pydantic (and any deps-bearing Python) repos can pass a green gate honestly.** mypy strict is now a real type bar instead of an `Any` cascade, and pytest collects against installed first-party + third-party code.
- **The gate now executes the repo's build backend** (editable install runs `setup.py`/PEP 517 build) at gate time. This is not a new trust boundary — the gate already runs the repo's code (pytest runs its tests, `npm ci` runs install scripts).
- **Network at gate time for Python**, consistent with the SCA scanners' deliberate freshness ([ADR-030](030-cross-ecosystem-sca.md)). A deps-install failure fails the `deps` check closed — correct: types/tests can't be verified without deps.
- **Local runs write `.venv` into the working tree**, like `node_modules` and the report dir — gitignorable, and already in `skip_dirs`.
- **Lockfile-less repos get less precise SCA than they could.** Excluding `.venv` from trivy-fs means we don't scan concrete installed versions; the committed manifest is the SCA source of truth. The contract remains "commit a lockfile for precise dependency SCA" — a candidate for the deferred lockfile-required tightening.
