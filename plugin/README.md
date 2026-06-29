# grizzly-gate Claude Code plugin

Brings the [grizzly-gate](https://github.com/Grizzly-Endeavors/grizzly-gate) local pre-check into Claude Code: run the gate against the working tree, read its report, and optionally block the agent from pushing un-gated code. This image **signs nothing** — it's the same local pre-check flow as `scripts/grizzly-gate-local.sh`, packaged for distribution. Signing only ever happens in-cluster in CI.

## What it adds

- **`/grizzly-gate:check`** — a skill that runs the gate against the current repo and summarizes the verdict, reading `grizzly-gate-report/report.json` on failure. Won't auto-fire; you run it on demand.
- **`/grizzly-gate:onboard`** — a user-invoked skill that brings a repo under the gate: surveys its layout, writes a truthful `gate-config.json`, documents the gate in the repo's `CLAUDE.md`, and verifies it passes (handing failures to `gate-fixer`). Use on a new repo or one not yet gated.
- **`gate-fixer` agent** — a Sonnet subagent that reads the report, fixes the violations in the scanned repo's code or its `gate-config.json`, and re-runs until green. Carries the `class → fix` knowledge so it doesn't re-derive it each time.
- **`grizzly-gate` MCP server** — runs the gate over the Model Context Protocol (stdio), so Claude can run the gate and pull **one** failing check's output at a time, paginated, instead of reading the whole `report.json` into context. Tools: `run_gate` (compact verdict — counts + failing labels, no raw output), `get_check_output` (one check by label, line-paginated), `list_honest_map_violations`, `get_report_summary` (last verdict without re-running). It's the same gate image in `mcp` mode (`grizzly-gate mcp`); signs nothing. `/grizzly-gate:check` and the `gate-fixer` agent drive these tools by default.
- **`grizzly-gate` on PATH** — the docker-run wrapper is added to the Bash tool's PATH while the plugin is enabled, so any repo in the family gets the local gate with no clone or script copy.
- **Push guard (opt-in)** — a `PreToolUse` hook that runs the gate before Claude's `git push` and blocks the push if it fails. Off by default.
- **Docker check** — a `SessionStart` hook that warns when `docker` isn't on PATH, since the gate runs as a container and nothing here works without it. Silent when docker is present; always on (no toggle).
- **Unsupported-language warning** — a `SessionStart` hook that warns (you and the agent) when the repo contains code in a language the gate has no adapter for, since the gate fails closed on it. Heuristic; advisory. On by default, toggle `warn_unsupported`.
- **Suppression watcher** — a `PostToolUse` hook that flags when an edit adds any lint suppression. Blanket forms (`#[allow]`, bare `# noqa`, `@ts-ignore`, `eslint-disable`) are called out as forbidden; scoped reasoned forms (`#[expect(..., reason=)]`, `@ts-expect-error`, coded `# noqa: E501`) are surfaced too. Claude is told to refactor rather than suppress and to confirm with you before keeping any suppression. Advisory; on by default, toggle `warn_suppression`.

## Install

This repo is itself a marketplace (`.claude-plugin/marketplace.json`), so the simplest path is to add it directly:

```
/plugin marketplace add Grizzly-Endeavors/grizzly-gate
/plugin install grizzly-gate@grizzly-endeavors
```

Alternatively, reference it from another marketplace you maintain by pointing an entry at this repo's `plugin/` subdirectory:

```json
{
  "name": "grizzly-gate",
  "source": {
    "source": "git-subdir",
    "url": "Grizzly-Endeavors/grizzly-gate",
    "path": "plugin",
    "ref": "v0.1.0"
  },
  "category": "ci",
  "tags": ["gate", "ci", "pre-check"]
}
```

Then `/plugin install grizzly-gate@<your-marketplace>`. Either way, all it needs is `docker` on PATH — the image is public, so no `docker login` is required (the wrapper `docker pull`s it on first run).

## Configuration

Sensible defaults, tunable where it matters:

- **Image** — baked in as `bearflinn/grizzly-gate:latest`. Not prompted. Override with the `GRIZZLY_GATE_IMAGE` env var (e.g. a pinned tag, or a locally-built `grizzly-gate:local` when testing changes to the gate itself). The image is multi-arch, so it runs natively on Apple Silicon Macs; to reproduce amd64 CI byte-for-byte on a Mac (only matters for arch-conditional code), set `DOCKER_DEFAULT_PLATFORM=linux/amd64`.
- **Image freshness** — `GRIZZLY_GATE_PULL` controls re-pulling an image you already have. `auto` (default) refreshes only a floating `:latest` tag — which otherwise goes stale silently and keeps running an old cached layer; `1` forces a refresh of any tag; `0` never refreshes, running the cached layer (use it to pin a `:latest` you've already pulled, or to stay offline). A missing image is always pulled; a refresh that fails is non-fatal and falls back to the cached layer. A pinned-by-digest or local-only tag like `grizzly-gate:local` is never refreshed by `auto`.
- **`block_push`** — the only prompted option, a boolean that defaults to **off**. When on, Claude's `git push` runs the gate first and a failing gate blocks the push. Flip it in the `/plugin` config dialog or accept the default and rely on `/grizzly-gate:check`.

The push guard fires only on Claude's own `git push` tool calls, not a human's manual terminal push — it stops the agent from shipping red code. It's independent of the maintainer pre-push hook in `scripts/hooks/pre-push`, which publishes the dev image and is a separate concern.

## Disabling the plugin for a repo

Some repos aren't the gate's business — a scratch repo, or one written entirely in a language the gate has no adapter for, where the session-start warning is just noise. Drop a `.grizzly-gate-disabled` file at the repo root and every plugin **hook** goes silently inert there: no session-start warnings, no suppression watcher, and no push guard even if `block_push` is on. Commit the marker to disable the plugin for the whole team, or add it to `.gitignore` (or `.git/info/exclude`) to disable it only for yourself.

```sh
touch .grizzly-gate-disabled        # this repo is none of the gate's business
```

This silences the hooks, which is where the per-session noise comes from. The `grizzly-gate` PATH wrapper and the MCP tools stay available but dormant — they only ever act when you explicitly invoke them, so they don't nag. Delete the marker to re-enable.
