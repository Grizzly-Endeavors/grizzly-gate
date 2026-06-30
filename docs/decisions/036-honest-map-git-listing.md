# ADR-036: The honest-map walk scopes to the clean-checkout content (git index), not the raw filesystem

**Date:** 2026-06-30
**Status:** accepted
**Amends:** [ADR-029](029-gate-config-honest-map.md)

## Context

[ADR-029](029-gate-config-honest-map.md) hardened *scope*: every gated repo ships a `gate-config.json` honest map, and the trusted harness (`detect.rs`) independently walks the tree and fails closed on any undeclared adapter-backed code or unsupported language. To shut the "hide it behind `.gitignore`" evasion, that walk deliberately **did not honor `.gitignore`** — it walked the raw filesystem under `source`, skipping only an Ops-owned `skip_dirs` list.

Walking the raw filesystem conflates two different things:

- **What CI gates.** CI runs the gate against a fresh `git clone` — i.e. the *tracked* tree. Build artifacts, dependency dirs, and scratch files are simply not present.
- **What a local pre-check sees.** The local pre-check (`plugin/bin/grizzly-gate`) mounts the developer's *working tree* into the gate image and walks it. That tree is dirty by definition: `target/`, `dist/`, coverage output, `.venv`, ad-hoc scratch files, generated code — none of which exist in CI.

The raw-filesystem walk therefore makes the local gate trip on files that would never reach CI, and the only escape valve is hand-maintaining `skip_dirs` to chase each new build/scratch dir. That hand-rolled denylist is both painful (every project grows new ignorable dirs) and pointless against its stated threat: in CI those untracked files do not exist, so there is nothing for `.gitignore` to "hide." The local pass therefore diverges from the CI pass for reasons that have nothing to do with what ships.

Crucially, the anti-evasion property ADR-029 actually needs does **not** come from ignoring `.gitignore` wholesale. The real vector is a **tracked file that also matches a `.gitignore` pattern** — committed via `git add -f`, or committed before a later-added ignore pattern. That file *is* in the clean CI checkout, so it must be detected. Honoring `.gitignore` for *tracked* files would reopen exactly that hole; ignoring it for *untracked* files buys nothing, because untracked files never ship.

## Decision

**The honest-map walk enumerates the clean-checkout content of `source` — the git index — rather than the raw filesystem.** Concretely, in a git work tree the candidate file set is:

```
git ls-files -z --cached --others --exclude-standard
```

i.e. **tracked files** (`--cached`) **plus untracked-but-not-ignored files** (`--others --exclude-standard`), paths relative to `source`, NUL-delimited so non-UTF-8 names cannot slip the parser.

This set is exactly what a fresh `git clone` would contain, plus the new files a developer is about to commit. The consequences:

1. **The anti-evasion guarantee is preserved.** A tracked file is always listed by `--cached`, regardless of any `.gitignore` pattern — so a `git add -f`'d file (the real evasion) is still detected and still fails closed. `.gitignore` never enters the decision for tracked files. The security-relevant set (what ships) is unchanged from ADR-029.
2. **The local pre-check stops tripping on non-CI files.** `target/`, `node_modules/`, `.venv/`, `dist/`, coverage output, and scratch files are untracked-and-ignored, so they fall out of the candidate set automatically — no `skip_dirs` entry required. The developer's `.gitignore` is the single source of truth for "not part of this repo," which is what it already means.
3. **"Local pass == CI pass" gets *stronger*.** The local walk now operates on the same fileset CI does (tracked), plus the untracked-non-ignored files the developer will commit — so a new source file is still caught before commit, while build artifacts no longer cause false failures. The two runs converge instead of diverging.
4. **In CI the change is a no-op.** A fresh clone has no untracked files and no local modifications, so `git ls-files` returns precisely the committed tree and `--others` is empty. The CI walk is byte-for-byte the set the raw filesystem walk saw (minus `.git/`), so CI behavior — and its security posture — is unchanged.

**`safe.directory=*` neutralizes git's dubious-ownership guard.** The local pre-check mounts a host-owned tree (`uid 1000`) into a root container, which trips git's ownership protection. The harness passes `-c safe.directory=*` on the `ls-files` invocation. This governs only *whether git will operate on the repo*, never *which files are listed*, so it cannot be used by a repo to influence detection.

**Fallback to the hostile filesystem walk.** When `source` is not a git work tree (an extracted tarball, a non-repo directory) or git is unavailable/errors, the harness falls back to the original raw-filesystem `WalkDir` (skipping `skip_dirs` and `.git`, never following symlinks). That walk is **strictly more inclusive** than the git listing, so completeness can only tighten under fallback, never loosen — failing closed in the safe direction.

**`skip_dirs` and the no-symlink-follow rule still apply in both modes.** A path whose components include a `skip_dir` (or `.git`) is dropped, and symlinks are excluded by `symlink_metadata` rather than followed — preserving the "no tree escape, no loops" property for the git listing exactly as the `WalkDir` walk had it. `skip_dirs` is now largely redundant in git mode (ignored build dirs already fall out), but it is retained so the fallback walk and any force-tracked build artifact behave identically to before.

## Alternatives Considered

- **Honor `.gitignore` directly in the `WalkDir` walk** (e.g. via the `ignore` crate). Simplest local-UX fix, but it would skip *tracked* files that match an ignore pattern — reopening the `git add -f` evasion ADR-029 exists to close. Rejected: it honors `.gitignore` for the one case (tracked files) where doing so is a hole.
- **Keep the raw walk; expand `skip_dirs` aggressively.** No new git dependency, but it is an endless hand-rolled denylist that never converges (every project invents new build/scratch dirs), and it still walks files CI never sees. Rejected as the status quo this ADR removes.
- **Walk tracked files only (`--cached`, drop `--others`).** Matches the CI checkout exactly and is the tightest security set, but a brand-new uncommitted source file would be invisible to the *local* pre-check, so "local pass == CI pass" would break the moment the developer commits. Rejected in favour of including untracked-non-ignored files, which costs nothing security-wise (untracked files never reach CI) and catches new code before commit.
- **Fail closed when `source` is not a git repo** instead of falling back to the filesystem walk. More dogmatic, but the filesystem walk is strictly more hostile, so falling back to it is already the fail-closed direction and keeps the gate usable on non-repo sources. Rejected.

## Consequences

- **`detect.rs` gains a git dependency at run time** (the `git` binary, already present in the gate image). It is invoked once per gate run via `std::process::Command`; a failure degrades to the filesystem walk rather than erroring.
- **The "does not honor `.gitignore`" wording in ADR-029, the `detect.rs` module doc, `README.md`, `docs/coverage.md`, and `docs/using-the-gate.md` is now imprecise** and is updated to the accurate framing: the walk scopes to the clean-checkout content, so `.gitignore` cannot hide a *tracked* file from detection (the property that matters), while locally-ignored untracked artifacts correctly fall out of scope.
- **`skip_dirs` becomes mostly vestigial in the common (git) path.** It is kept for the filesystem fallback and the force-tracked-build-artifact edge, not removed — removing it would change fallback behavior. A future ADR could retire it once the fallback path is judged unnecessary.
- **No change to the CI security posture.** The set CI checks is identical to before; this ADR only changes which files a *dirty local tree* contributes to the walk.
