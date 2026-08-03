# ADR-042: A gate run leaves the scanned tree as it found it

**Date:** 2026-08-03
**Status:** accepted

## Context

The gate mounts the scanned working tree read-write (`plugin/bin/grizzly-gate`, and the `docker run` documented in `README.md` / `docs/using-the-gate.md`) and the container has no `USER`, so every check runs as root. Anything a check writes beside the source therefore survives the run, in the caller's repo, owned by root.

Measured across the local checkouts the gate had been run against: 465 root-owned paths in grizzly-gate itself, ~34k in career-scanner, ~23k in grizzly-gameservers, ~4.4k in grizzly-invite. Four sources, in descending volume:

1. **`target/`** — `cargo clippy` / `cargo test` build output. The largest by far (25.6k files in career-scanner alone). Worse than clutter: root-owned entries inside a developer's own `target/` make their subsequent host `cargo build` fail on permissions.
2. **`node_modules/`** — the node adapter's `npm ci` (8.5k files under `career-scanner/web`).
3. **`.venv/`** plus `__pycache__`, `.mypy_cache`, `.ruff_cache`, `.pytest_cache` — the python adapter's deps check and the tools it feeds.
4. **`grizzly-gate-report/report.json`** — the harness's own report.

The scanners were already clean: gitleaks writes its report to a `mktemp` file and the rest (semgrep, osv-scanner, trivy) only read the tree.

Two framings were considered and rejected before landing on this one.

**Chown the tree back at the end of a run.** Cheap (one walk, `lchown` anything owned by uid 0 back to the source root's owner) and it would have fixed the ownership complaint completely. But ownership was the symptom, not the problem: the gate has no business leaving a multi-gigabyte `target/` and a full `node_modules/` in a repo it was asked to *check*. It also does nothing about the collateral damage below.

**Run the container as the invoking user (`--user $(id -u):$(id -g)`).** Correct by construction for ownership, but it requires the image's gate config dirs to be writable by an arbitrary uid — the harness materializes `eslint.config.mjs` and `osv-scanner.toml` into them at run time (ADR-033). Repo-supplied code runs as that same uid (pytest, npm lifecycle scripts), so it could rewrite a later check's config mid-run. That is an integrity regression in a security gate, traded for a cosmetic win. It also only fixes the wrapper, not the raw `docker run` both docs publish.

A third fact reframed the problem. The gate was already *destroying* developer state, not merely adding to it: `npm ci` deletes and reinstalls `node_modules`, and `uv venv --clear` rebuilt `.venv` from scratch on every run (the `--clear` was mandatory precisely *because* the venv persisted in the tree — `uv venv`, unlike `npm ci`, errors on a pre-existing venv). So "leave it, just fix the ownership" was never neutral, and "delete it afterwards" would have left the developer with no installed dependencies at all.

Two empirical checks decided the mechanism, both run against the pinned toolchain in the published image:

- **Can `node_modules` be relocated by symlinking it out of the tree?** No. With `node_modules` a symlink to a directory elsewhere, `npm ci` replaces the *symlink itself* with a real directory and installs into the tree anyway. Nor can the install be moved to a scratch dir wholesale: `npm ci` runs the repo's own `prepare` script, and SvelteKit's `svelte-kit sync` generates `.svelte-kit/tsconfig.json` — which the repo's tsconfig extends, and which `resolve_tsconfig` depends on existing post-install.
- **Does the python deps check write anything else into the tree?** Yes, and not where a top-level check would find it: `uv pip install -e .` on a `src/`-layout package leaves `src/<pkg>.egg-info`, a subdirectory the venv relocation does not reach.

## Decision

**Nothing the gate creates outlives the run except the report.** Three mechanisms, in order of preference — relocate if possible, undo if not, and hand back what must stay.

**1. Relocate.** State that does not have to live beside the source is pointed elsewhere:

- **Shared, content-addressed caches → `/cache`** (`Dockerfile` `ENV`): `CARGO_TARGET_DIR`, `npm_config_cache`, `UV_CACHE_DIR`. Both wrappers back this with a named `grizzly-gate-cache` docker volume, so repeat local runs stay warm without a `target/` in the repo. An invocation that mounts nothing there still works — it just loses the cache with the container.
- **Per-project state → the run's `{work}` dir**: a new substitution token, alongside `{config}`/`{source}`/`{tsconfig}`/`{skip_dirs}`, resolving to a per-project scratch dir inside the container (`WorkRoot`, `harness/src/main.rs`). The python virtualenv (`{work}/venv`), ruff's `--cache-dir`, and mypy's `--cache-dir` moved here. Deliberately *not* `/cache`: these vary with the scanned repo, and a cache two different repos could share is a wrong-verdict risk in a tool whose entire job is verdicts. Only content-addressed caches are shared.

  The work root is `/gate-work` (mode 0700, root-owned, created in the `Dockerfile`; `GRIZZLY_GATE_WORK_DIR` overrides it for running the harness outside its container) rather than the system temp dir. The first implementation used `std::env::temp_dir()` with a `grizzly-gate.<pid>` name and the gate caught it when run against itself — `rust.lang.security.temp-dir`, correctly: the harness runs as root, so a predictably-named directory under a world-writable `/tmp` lets any local user pre-plant a symlink and redirect what the gate writes through it. A private directory the image owns removes the class instead of racing it, and needs no suppression.
- **Switched off rather than moved**: `PYTHONDONTWRITEBYTECODE=1` (no `__pycache__` throughout the source; the gate compiles each file once per run, so a bytecode cache saves nothing) and pytest's `-p no:cacheprovider` (nothing reads `.pytest_cache` — the gate never runs `--last-failed`).

With the venv out of the tree, `uv venv --clear` is dropped: `{work}` is fresh per run, so there is no pre-existing venv to clear.

**2. Undo what cannot be relocated.** Two things must be written into the project dir, and both are reverted when that project's checks finish:

- **`node_modules`** (`NodeModulesStash`): any existing one is renamed aside first — a rename within a single directory, so it is atomic and costs nothing however large the tree is — `npm ci` installs fresh, and the gate's install is removed and the developer's moved back. A gate run therefore never costs a reinstall, which is strictly better than the previous behavior. If a stash is found already present at the start of a run, it is a previous run's, killed before it could restore; it holds the repo's real dependencies, so it is kept and the live directory discarded, which self-heals that interruption.
- **`*.egg-info`** (`EggInfo`): the set present before the adapter runs is recorded, and any that appears afterwards — the editable install's — is removed. Snapshot-and-diff rather than blanket removal, so a repo that checks one in keeps it.

**3. Hand back the report.** `grizzly-gate-report/report.json` is the one artifact a run is *meant* to leave. `Report::write` chowns it, and its directory, to the owner of the enclosing directory (`adopt_dir_owner`, `harness/src/report.rs`), so the developer who ran the gate can read and replace it. It is a no-op when that owner is root (CI, where the checkout is root's anyway) and when the paths are already owned correctly — which is what makes it silently inert outside the container, where the `chown` would fail for want of privilege.

## Consequences

- A local gate run no longer leaves anything in the repo but its report, and the report is owned by the caller. Existing root-owned droppings from earlier runs are not retroactively cleaned — that is a one-time `sudo chown`/`rm` per affected checkout.
- Local Rust runs get *faster*, not slower: `CARGO_TARGET_DIR` on a persistent volume is warm across runs and across repos, where before each repo carried its own in-tree `target/`.
- Adding a language means asking the third question alongside detection rules and an adapter: does any check write into the project dir, and if so, can it be pointed at `{work}` — and if it cannot, what undoes it? The two escape hatches here are documented in their own manifests rather than centralized, so the answer lives next to the check that needs it.
- A run killed hard (`docker kill`, OOM) can leave a `.grizzly-gate.node_modules` stash behind. The next run restores from it, so the failure mode is a visible directory for one run, not lost dependencies.
- `.svelte-kit/` generated by a repo's own `prepare` script during `npm ci` is still left behind. It is small, gitignored by every SvelteKit repo, and identical to what the developer's own `npm install` produces — not worth a third undo mechanism.
