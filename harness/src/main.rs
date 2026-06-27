//! grizzly-gate — the orchestration harness for the grizzly-platform CI gate.
//!
//! One artifact, centrally owned by Ops: it detects the stacks in a repo, runs
//! the pinned per-language adapters + scanners defined in the `config/` tree,
//! and — only if everything passes — signs the built image with cosign. The
//! signature is the single proof that travels forward to the deploy boundary,
//! where Kyverno refuses to admit any image lacking it.

mod config;
mod detect;
mod gateconfig;
mod mcp;
mod report;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Scope;
use gateconfig::ResolvedProject;
use report::Report;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Instant;

/// Config tree baked into the image; overridable per-invocation with `--config`
/// or the `GRIZZLY_GATE_CONFIG_DIR` env var.
const DEFAULT_CONFIG_DIR: &str = "/etc/grizzly-gate/config";

#[derive(Parser)]
#[command(
    name = "grizzly-gate",
    version,
    about = "grizzly-platform CI gate harness"
)]
struct Cli {
    /// Optional subcommand. With none, the gate runs once against `--source` and
    /// exits non-zero on a failed verdict (the default CI / local pre-check flow).
    #[command(subcommand)]
    command: Option<Command>,

    /// Repository checkout to gate.
    #[arg(long, default_value = ".")]
    source: PathBuf,

    /// Built image reference (pin to a digest) to scan and, on pass, sign.
    #[arg(long)]
    image: Option<String>,

    /// Path to a config root directory (with `languages/` and/or `util/`);
    /// falls back to `GRIZZLY_GATE_CONFIG_DIR`, then the tree baked into the
    /// image.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Sign the image with cosign on pass. Requires --image and --cosign-key.
    #[arg(long)]
    sign: bool,

    /// cosign private-key reference (file path, or `env://`/`openbao://` ref).
    #[arg(long)]
    cosign_key: Option<String>,

    /// Allow cosign to talk to a plain-HTTP / self-signed registry (the
    /// in-cluster zot is HTTP-only).
    #[arg(long)]
    insecure_registry: bool,

    /// Directory for the machine-readable run report (`report.json`). Written on
    /// every run; holds the full untruncated output of every check.
    #[arg(long, default_value = "./grizzly-gate-report")]
    report_dir: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the gate over the Model Context Protocol (newline-delimited
    /// JSON-RPC on stdio). Lets an agent run the gate and pull one check's
    /// output at a time, instead of ingesting the whole report into context.
    /// Never signs — the same source-only surface as a local pre-check.
    Mcp(McpArgs),
}

#[derive(clap::Args)]
struct McpArgs {
    /// Repository checkout to gate.
    #[arg(long, default_value = ".")]
    source: PathBuf,

    /// Config root override (see `--config` on the top-level command).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Directory the run report is written to and read back from.
    #[arg(long, default_value = "./grizzly-gate-report")]
    report_dir: PathBuf,
}

/// Resolve the config root: explicit `--config`, else `GRIZZLY_GATE_CONFIG_DIR`,
/// else the tree baked into the image.
fn resolve_config_root(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("GRIZZLY_GATE_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR))
}

/// The outcome of one gate run, decoupled from how it is presented: the report
/// (carrying every check's full output), plus the in-memory step results for the
/// run-mode FAILURES replay. On a phase-1 honest-map failure, `honest_map_message`
/// holds the rich human explanation that the report's structured violations don't.
pub struct GateRun {
    pub report: Report,
    pub honest_map_message: Option<String>,
    results: Vec<StepResult>,
}

struct StepResult {
    label: String,
    /// Language adapter this step belongs to (`None` for repo-wide scanners).
    language: Option<String>,
    /// Project path the step ran in (`None` for repo-wide scanners).
    project: Option<String>,
    /// The rendered command line that ran (placeholders already substituted).
    cmd: String,
    ok: bool,
    /// Process exit code, or `None` if the tool could not be spawned/parsed.
    exit_code: Option<i32>,
    secs: f64,
    /// Full, untruncated combined stdout+stderr — the durable record copied
    /// verbatim into `report.json`.
    output: String,
}

impl StepResult {
    /// Build the report row for this step.
    fn to_report(&self) -> report::Check {
        report::Check {
            label: self.label.clone(),
            language: self.language.clone(),
            project: self.project.clone(),
            cmd: self.cmd.clone(),
            ok: self.ok,
            exit_code: self.exit_code,
            duration_secs: self.secs,
            output: self.output.clone(),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Mcp(args)) => run_mcp(args),
        None => run_once(&cli),
    }
}

/// Serve the gate over MCP on stdio. Loads the config tree and resolves the
/// source once, then hands control to the protocol loop. All of the gate's own
/// human logging is routed to stderr so stdout carries only JSON-RPC.
fn run_mcp(args: &McpArgs) -> Result<()> {
    let config_root = resolve_config_root(args.config.as_deref());
    let tree = config::load_tree(&config_root)
        .with_context(|| format!("loading gate config from {}", config_root.display()))?;
    let source = args
        .source
        .canonicalize()
        .with_context(|| format!("resolving source path {}", args.source.display()))?;
    mcp::serve(&tree, &source, &args.report_dir)
}

/// The default flow: run the gate once, print the human verdict, write the
/// report, sign on a clean pass, and exit non-zero on any failure.
fn run_once(cli: &Cli) -> Result<()> {
    let config_root = resolve_config_root(cli.config.as_deref());
    let tree = config::load_tree(&config_root)
        .with_context(|| format!("loading gate config from {}", config_root.display()))?;

    let source = cli
        .source
        .canonicalize()
        .with_context(|| format!("resolving source path {}", cli.source.display()))?;

    // The gate's progress log goes to stdout in this mode. Scope the lock so the
    // verdict block below can use `println!` freely once the run has finished.
    let run = {
        let mut out = std::io::stdout().lock();
        execute(&mut out, &tree, &source, cli.image.as_deref())?
    };
    let GateRun {
        mut report,
        honest_map_message,
        results,
    } = run;

    // --- Phase-1 honest-map failure: record, point at the report, fail closed.
    if let Some(message) = honest_map_message {
        // The report is best-effort here; a write failure must not mask the
        // actual honest-map failure, so surface it as a warning and continue.
        match report.write(&cli.report_dir) {
            Ok(path) => println!(
                "\ngrizzly-gate :: honest-map violations recorded to {} \
                 (query: jq -c '.honest_map.violations[]' {})",
                path.display(),
                path.display()
            ),
            Err(e) => eprintln!("grizzly-gate :: warning: could not write report: {e:#}"),
        }
        bail!("{message}");
    }

    let report_path = report.write(&cli.report_dir)?;

    // --- Verdict -----------------------------------------------------------
    println!("\n────────────────────────── gate summary ──────────────────────────");
    let mut failed = 0_usize;
    for r in &results {
        let tag = if r.ok { "PASS" } else { "FAIL" };
        println!("  [{tag}] {:<40} {:>7.1}s", r.label, r.secs);
        if !r.ok {
            failed += 1;
        }
    }
    println!("───────────────────────────────────────────────────────────────────");

    if failed > 0 {
        print_failures(&results, &report, &report_path);
        bail!("gate FAILED: {failed}/{} checks failed", results.len());
    }
    println!("gate PASSED: {}/{} checks", results.len(), results.len());
    println!(
        "grizzly-gate :: report written to {}",
        report_path.display()
    );

    // --- Sign on pass ------------------------------------------------------
    if cli.sign {
        let image = cli
            .image
            .as_deref()
            .context("--sign requires --image (sign the built image by digest)")?;
        let key = cli
            .cosign_key
            .as_deref()
            .context("--sign requires --cosign-key")?;
        sign_image(image, key, cli.insecure_registry)?;
        println!("grizzly-gate :: signed {image}");
    }

    Ok(())
}

/// Run both gate phases against `source`, logging progress to `log`, and return
/// the structured outcome without presenting or signing it. Shared by the
/// run-once flow (logs to stdout) and the MCP server (logs to stderr, keeping
/// stdout clean for JSON-RPC). Writing the report and acting on the verdict are
/// the caller's job.
pub fn execute(
    log: &mut dyn Write,
    tree: &config::Tree,
    source: &Path,
    image: Option<&str>,
) -> Result<GateRun> {
    writeln!(log, "grizzly-gate :: gating {}", source.display())?;
    if let Some(image) = image {
        writeln!(log, "grizzly-gate :: image {image}")?;
    }

    let mut report = Report::new();

    // --- Phase 1 — Honest map: required declaration, then independent
    // verification. The repo must ship a gate-config.json that truthfully maps
    // its layout, and the tree must contain no undeclared or unsupported code.
    // Phase 1 must pass *completely* before Phase 2 runs — a repo that lies
    // about (or omits) its contents never reaches the checks, let alone signing.
    // Each stage reports *all* of its problems at once (no first-failure churn).
    writeln!(log, "\n── Phase 1: honest map ──")?;
    let projects = match gateconfig::load(source, tree) {
        Ok(projects) => projects,
        Err(failure) => return Ok(fail_phase1(report, failure)),
    };
    writeln!(
        log,
        "grizzly-gate :: gate-config.json declares {} project(s)",
        projects.len()
    )?;
    for p in &projects {
        let where_ = if p.rel_path.as_os_str().is_empty() {
            ".".to_string()
        } else {
            p.rel_path.display().to_string()
        };
        writeln!(log, "grizzly-gate ::   - {} @ {where_}", p.language)?;
    }
    if let Err(failure) = detect::verify(source, tree, &projects) {
        return Ok(fail_phase1(report, failure));
    }
    writeln!(log, "grizzly-gate :: honest-map verification passed")?;

    // --- Phase 2 — Checks. Every adapter check and scanner runs; none of them
    // short-circuit the run, so the verdict reflects every failure at once.
    writeln!(log, "\n── Phase 2: checks ──")?;
    let results = run_checks(log, tree, source, image, &projects)?;
    if results.is_empty() {
        bail!("gate ran zero checks — refusing to pass (fail closed)");
    }
    report.set_checks(results.iter().map(StepResult::to_report).collect());

    Ok(GateRun {
        report,
        honest_map_message: None,
        results,
    })
}

/// Fold a phase-1 honest-map failure into a `GateRun`: record the structured
/// violations on the report and carry the rich human message for the caller to
/// present. Phase 2 never runs.
fn fail_phase1(mut report: Report, failure: report::HonestMapFailure) -> GateRun {
    let report::HonestMapFailure {
        message,
        violations,
    } = failure;
    report.fail_honest_map(violations);
    GateRun {
        report,
        honest_map_message: Some(message),
        results: Vec::new(),
    }
}

/// Maximum tail of a failing check's output replayed in the FAILURES block. The
/// full, untruncated output always remains in report.json — this cap only keeps
/// a pathological tool from flooding the terminal verdict.
const FAILURE_TAIL_LINES: usize = 200;
const FAILURE_TAIL_BYTES: usize = 16 * 1024;

/// Replay each failing check's output (tail-capped) under the verdict, so the
/// actionable detail sits with the result instead of scrolled away. Always
/// points at the full report and the `jq` query for the complete output.
fn print_failures(results: &[StepResult], report: &Report, report_path: &Path) {
    println!("\n──────────────────────────── FAILURES ────────────────────────────");
    for r in results.iter().filter(|r| !r.ok) {
        println!("\n▼ {}", r.label);
        let (tail, truncated) = tail_cap(&r.output, FAILURE_TAIL_LINES, FAILURE_TAIL_BYTES);
        print!("{tail}");
        if !tail.is_empty() && !tail.ends_with('\n') {
            println!();
        }
        if truncated {
            println!(
                "… output truncated — full output in {} (query: jq -r '.checks[] | \
                 select(.label==\"{}\") | .output' {}); or re-run locally (scripts/grizzly-gate-local.sh).",
                report_path.display(),
                r.label,
                report_path.display()
            );
        }
    }
    println!("───────────────────────────────────────────────────────────────────");
    println!("\ngrizzly-gate :: full report at {}", report_path.display());
    for hint in report.query_hints() {
        println!("grizzly-gate ::   {hint}");
    }
}

/// Return the last `max_lines`/`max_bytes` of `s` (whichever bound bites first)
/// plus whether anything was dropped. Splits on a char boundary so the slice is
/// always valid UTF-8.
fn tail_cap(s: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    // Byte bound: keep the trailing `max_bytes`, advanced to a char boundary.
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    // Line bound: keep the trailing `max_lines` lines from there.
    let line_start = s
        .match_indices('\n')
        .rev()
        .nth(max_lines)
        .map_or(0, |(i, _)| i + 1);
    let cut = start.max(line_start);
    (s.get(cut..).unwrap_or_default().to_string(), cut > 0)
}

/// Placeholder substitutions for a command line and its env values: each
/// `{source}`/`{image}`/`{config}` token is replaced with the corresponding
/// value when present. `config` is the gate's config dir for the tool — the
/// mechanism by which gate-owned config is forced onto it (via flags or env).
#[derive(Clone, Copy)]
struct Subst<'a> {
    source: Option<&'a str>,
    image: Option<&'a str>,
    config: Option<&'a str>,
    /// Path passed to `tsc --project` for node projects: either the repo's own
    /// tsconfig wrapped to force gate strictness, or the gate's base tsconfig.
    tsconfig: Option<&'a str>,
    /// Ops-owned `skip_dirs` (the honest-map walk's excluded dir names), joined
    /// with commas. A check can forward this to its tool — via the `{skip_dirs}`
    /// token in its cmd or env — so the tool ignores the same dirs the gate does
    /// not treat as first-party code (e.g. eslint, which otherwise lints
    /// build/dist/.svelte-kit). Single-sourced from `detect.toml`.
    skip_dirs: Option<&'a str>,
}

/// Run each declared project's adapter checks (in its own directory) and every
/// scanner in scope, returning their results in execution order.
///
/// Adapters run per *declared project* — not by scanning the root for a marker —
/// so a Rust crate in a subdir or a second project in a monorepo is checked
/// exactly where the (already-verified) `gate-config.json` says it lives.
fn run_checks(
    log: &mut dyn Write,
    tree: &config::Tree,
    source: &Path,
    image: Option<&str>,
    projects: &[ResolvedProject],
) -> Result<Vec<StepResult>> {
    let mut results: Vec<StepResult> = Vec::new();

    // Ops-owned skip_dirs, comma-joined, so a check can hand its tool the same
    // exclude list the honest-map walk uses (via the `{skip_dirs}` token).
    let skip_dirs = tree.detect.skip_dirs.join(",");

    // The gate's own ESLint flat config ships as a `.tmpl` (so the honest-map
    // walk doesn't flag the gate's config tree as undeclared node code when the
    // gate gates itself). Materialize the live `.mjs` beside its node_modules
    // before any node check runs; the guard removes it afterwards.
    let eslint_config = materialize_eslint_config(tree, projects)?;

    // --- Language adapters, per declared project ---------------------------
    for project in projects {
        let adapter = tree
            .adapters
            .iter()
            .find(|a| a.name == project.language)
            .with_context(|| format!("no adapter for declared language {:?}", project.language))?;

        let cfg = adapter.config_dir.to_string_lossy().to_string();
        let proj_str = project.abs_path.to_string_lossy().to_string();
        let where_ = if project.rel_path.as_os_str().is_empty() {
            ".".to_string()
        } else {
            project.rel_path.display().to_string()
        };
        writeln!(
            log,
            "\n=== {} @ {where_} (marker: {}) ===",
            adapter.name, adapter.marker
        )?;

        // For node, resolve the tsconfig the checks use. A repo-declared tsconfig
        // is wrapped so its module/path resolution is honored while the gate's
        // strictness is force-overridden; the wrapper is cleaned up after.
        let ts = resolve_tsconfig(adapter, project)?;
        let subst = Subst {
            source: Some(&proj_str),
            image: None,
            config: Some(&cfg),
            tsconfig: ts.as_ref().map(|t| t.arg.as_str()),
            skip_dirs: Some(&skip_dirs),
        };
        for check in &adapter.checks {
            let mut result = run(
                log,
                &format!("{}:{}", adapter.name, check.name),
                &check.cmd,
                &project.abs_path,
                subst,
                &check.env,
            )?;
            result.language = Some(adapter.name.clone());
            result.project = Some(where_.clone());
            results.push(result);
        }
        if let Some(t) = ts {
            t.cleanup();
        }
    }

    // Node checks are done; drop the materialized eslint config. Scanners walk
    // the scanned source (not the gate's config dir), so this never affects them.
    if let Some(cfg) = eslint_config {
        cfg.cleanup();
    }

    // The gate license-checks *dependencies*, not the scanned repo's own packages.
    // osv-scanner resolves licenses from the registry, so a project's own
    // (unpublished) crate(s) resolve to UNKNOWN and trip the license allowlist.
    // Generate an osv config that license-ignores those crate names for this run.
    let osv_config = materialize_osv_config(tree, projects, &tree.detect.skip_dirs)?;

    // --- Scanners ----------------------------------------------------------
    let source_str = source.to_string_lossy().to_string();
    for scanner in &tree.scanners {
        let cfg = scanner.config_dir.to_string_lossy().to_string();
        let subst = Subst {
            source: Some(&source_str),
            image,
            config: Some(&cfg),
            tsconfig: None,
            skip_dirs: Some(&skip_dirs),
        };
        let label = format!("scan:{}", scanner.name);
        match scanner.scope {
            Scope::Source => {
                results.push(run(log, &label, &scanner.cmd, source, subst, &scanner.env)?);
            }
            Scope::Image if image.is_some() => {
                results.push(run(log, &label, &scanner.cmd, source, subst, &scanner.env)?);
            }
            Scope::Image => writeln!(
                log,
                "grizzly-gate :: skipping image scanner '{}' (no --image given)",
                scanner.name
            )?,
        }
    }

    if let Some(cfg) = osv_config {
        cfg.cleanup();
    }

    Ok(results)
}

/// The tsconfig a node project's `tsc` check should use, plus any temp wrapper
/// to clean up afterwards. Non-node adapters get `None`.
struct ResolvedTsconfig {
    /// Value substituted for `{tsconfig}` (an absolute path).
    arg: String,
    /// Wrapper file to delete after the check (when a repo tsconfig was wrapped).
    temp: Option<PathBuf>,
}

impl ResolvedTsconfig {
    fn cleanup(self) {
        if let Some(p) = self.temp {
            // Best-effort: the wrapper lives in an ephemeral CI checkout, but a
            // failed unlink is surfaced rather than silently swallowed.
            if let Err(e) = std::fs::remove_file(&p) {
                eprintln!(
                    "grizzly-gate :: warning: could not remove tsconfig wrapper {}: {e}",
                    p.display()
                );
            }
        }
    }
}

/// Generated wrapper filename written into a node project to force gate
/// strictness on top of the repo's own tsconfig.
const TS_WRAPPER: &str = ".grizzly-gate.tsconfig.json";

fn resolve_tsconfig(
    adapter: &config::LanguageAdapter,
    project: &ResolvedProject,
) -> Result<Option<ResolvedTsconfig>> {
    if adapter.name != "node" {
        return Ok(None);
    }
    let Some(repo_ts) = &project.tsconfig else {
        // No repo tsconfig declared: use the gate's strict base config as-is.
        let base = adapter.config_dir.join("tsconfig.base.json");
        return Ok(Some(ResolvedTsconfig {
            arg: base.to_string_lossy().to_string(),
            temp: None,
        }));
    };

    // Wrap the repo tsconfig: `extends` it for module/path resolution, then
    // force every strict compiler option locally. `extends` is overridden
    // per-key by these locals, and `strict` is expanded into its full family so
    // a repo cannot opt out of an individual sub-flag (e.g. strictNullChecks).
    // tsc's default `include` (every TS file under the wrapper's dir, minus
    // node_modules) means a repo cannot shrink the typechecked set either.
    let wrapper = serde_json::json!({
        "extends": repo_ts.to_string_lossy(),
        "compilerOptions": {
            "noEmit": true,
            "strict": true,
            "noImplicitAny": true,
            "strictNullChecks": true,
            "strictFunctionTypes": true,
            "strictBindCallApply": true,
            "strictPropertyInitialization": true,
            "noImplicitThis": true,
            "useUnknownInCatchVariables": true,
            "alwaysStrict": true,
            "forceConsistentCasingInFileNames": true,
            "skipLibCheck": true,
        }
    });
    let path = project.abs_path.join(TS_WRAPPER);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&wrapper).context("serializing tsconfig wrapper")?,
    )
    .with_context(|| format!("writing tsconfig wrapper {}", path.display()))?;

    Ok(Some(ResolvedTsconfig {
        arg: path.to_string_lossy().to_string(),
        temp: Some(path),
    }))
}

/// Source-of-truth `ESLint` flat config filename (a template, not a live `.mjs`)
/// and the live filename the harness materializes it to at run time. See
/// [`materialize_eslint_config`] and ADR-033.
const ESLINT_TEMPLATE: &str = "eslint.config.mjs.tmpl";
const ESLINT_CONFIG: &str = "eslint.config.mjs";

/// The live `eslint.config.mjs` the harness writes into the node config dir for
/// the duration of a run, plus the path to remove afterwards.
struct MaterializedEslintConfig {
    path: PathBuf,
}

impl MaterializedEslintConfig {
    fn cleanup(self) {
        // Best-effort: the config dir lives inside the ephemeral gate container,
        // but a failed unlink is surfaced rather than silently swallowed.
        if let Err(e) = std::fs::remove_file(&self.path) {
            eprintln!(
                "grizzly-gate :: warning: could not remove eslint config {}: {e}",
                self.path.display()
            );
        }
    }
}

/// Materialize the gate's `ESLint` flat config from its shipped `.tmpl` into the
/// node config dir, so eslint's `--config {config}/eslint.config.mjs` finds a
/// real file whose bare plugin imports resolve against the sibling `node_modules`
/// installed at image build. The config tree ships only the `.tmpl` (a non-detected
/// extension) so the gate's honest-map walk never flags the gate's own config tree
/// as undeclared node code when the gate gates itself. No-op (returns `None`) when
/// no node project is declared, since no eslint check will run.
fn materialize_eslint_config(
    tree: &config::Tree,
    projects: &[ResolvedProject],
) -> Result<Option<MaterializedEslintConfig>> {
    if !projects.iter().any(|p| p.language == "node") {
        return Ok(None);
    }
    let adapter = tree
        .adapters
        .iter()
        .find(|a| a.name == "node")
        .context("node project declared but no node adapter in the config tree")?;
    let tmpl = adapter.config_dir.join(ESLINT_TEMPLATE);
    let dst = adapter.config_dir.join(ESLINT_CONFIG);
    std::fs::copy(&tmpl, &dst).with_context(|| {
        format!(
            "materializing eslint config {} from {}",
            dst.display(),
            tmpl.display()
        )
    })?;
    Ok(Some(MaterializedEslintConfig { path: dst }))
}

/// Config filename the harness generates into the osv-scanner config dir for the
/// duration of a run (the manifest passes it via `--config`, which also disables
/// osv-scanner's auto-discovery of a repo's own `osv-scanner.toml`).
const OSV_CONFIG: &str = "osv-scanner.toml";

/// The generated `osv-scanner.toml`, plus the path to remove after the scanners.
struct MaterializedOsvConfig {
    path: PathBuf,
}

impl MaterializedOsvConfig {
    fn cleanup(self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            eprintln!(
                "grizzly-gate :: warning: could not remove osv config {}: {e}",
                self.path.display()
            );
        }
    }
}

/// Generate an `osv-scanner.toml` that excludes the scanned repo's OWN packages
/// from license checking. osv-scanner resolves licenses from the registry, so a
/// project's own unpublished crate(s) resolve to `UNKNOWN` and trip the license
/// allowlist — but the gate license-checks *dependencies*, not the repo itself.
/// The repo's crate names are collected by walking each declared Rust project for
/// `Cargo.toml` `[package].name` (covering workspaces and nested crates); each is
/// written as a `license.ignore` override. Returns `None` when no osv-scanner
/// scanner is configured.
fn materialize_osv_config(
    tree: &config::Tree,
    projects: &[ResolvedProject],
    skip_dirs: &[String],
) -> Result<Option<MaterializedOsvConfig>> {
    let Some(scanner) = tree.scanners.iter().find(|s| s.name == "osv-scanner") else {
        return Ok(None);
    };

    let mut body = String::from(
        "# Generated by grizzly-gate at run time — do not edit.\n\
         # The scanned repo's OWN packages are excluded from LICENSE checking only:\n\
         # osv-scanner resolves licenses from the registry, so a project's own\n\
         # unpublished package resolves to UNKNOWN. The gate license-checks\n\
         # dependencies, not the repo itself (vuln scanning is unaffected).\n",
    );
    for name in local_crate_names(projects, skip_dirs) {
        // Cargo enforces crate names to `[A-Za-z0-9_-]`, so the name needs no TOML
        // string escaping; embed it directly in a basic string.
        body.push_str("\n[[PackageOverrides]]\nname = \"");
        body.push_str(&name);
        body.push_str(
            "\"\nlicense.ignore = true\n\
             reason = \"the scanned project's own package; the gate checks dependency licenses, not the repo's own\"\n",
        );
    }

    let dst = scanner.config_dir.join(OSV_CONFIG);
    std::fs::write(&dst, body).with_context(|| format!("writing osv config {}", dst.display()))?;
    Ok(Some(MaterializedOsvConfig { path: dst }))
}

/// Names of the repo's own crates across all declared Rust projects: every
/// `Cargo.toml` under each project (minus the Ops-owned `skip_dirs` and `.git`)
/// contributing its `[package].name`. Deduplicated and sorted. A `Cargo.toml`
/// that cannot be read or parsed is skipped with a warning (worst case: a spurious
/// license finding for that crate — fail-closed, never a missed dependency CVE).
fn local_crate_names(
    projects: &[ResolvedProject],
    skip_dirs: &[String],
) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    for project in projects.iter().filter(|p| p.language == "rust") {
        let walker = walkdir::WalkDir::new(&project.abs_path)
            .follow_links(false)
            .into_iter();
        for entry in walker.filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !(e.file_type().is_dir()
                && (n == ".git" || skip_dirs.iter().any(|d| d.as_str() == n.as_ref())))
        }) {
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_file() && entry.file_name() == "Cargo.toml" {
                if let Some(name) = read_crate_name(entry.path()) {
                    names.insert(name);
                }
            }
        }
    }
    names
}

/// `[package].name` from a `Cargo.toml`, or `None` for a virtual workspace manifest
/// (no `[package]`) or an unreadable/unparseable file (warned, not fatal).
fn read_crate_name(path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "grizzly-gate :: warning: could not read {} for crate name: {e}",
                path.display()
            );
            return None;
        }
    };
    match crate_name_from_manifest(&text) {
        Ok(name) => name,
        Err(e) => {
            eprintln!(
                "grizzly-gate :: warning: could not parse {} for crate name: {e}",
                path.display()
            );
            None
        }
    }
}

/// Parse `[package].name` from `Cargo.toml` text. `Ok(None)` for a virtual
/// workspace manifest (no `[package]`); `Err` for unparseable TOML. Split from the
/// IO in [`read_crate_name`] so the parse mapping is unit-testable without the
/// filesystem (the directory walk itself is covered end-to-end by a gate run).
fn crate_name_from_manifest(text: &str) -> Result<Option<String>, toml::de::Error> {
    let manifest: CargoManifest = toml::from_str(text)?;
    Ok(manifest.package.map(|p| p.name))
}

/// The slice of a `Cargo.toml` the gate needs: just `[package].name` (absent for a
/// virtual workspace manifest). Unknown fields are ignored by default.
#[derive(serde::Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
}

#[derive(serde::Deserialize)]
struct CargoPackage {
    name: String,
}

/// Run one command line in `cwd` after applying `subst` to the command and to
/// each env value. Progress and the tool's captured output are written to `log`;
/// the only fallible part is that logging, so the returned `Result` reflects a
/// log write failure, not the check's own pass/fail (which lives in the
/// `StepResult`).
fn run(
    log: &mut dyn Write,
    label: &str,
    cmdline: &str,
    cwd: &Path,
    subst: Subst,
    env: &BTreeMap<String, String>,
) -> Result<StepResult> {
    let apply = |s: &str| -> String {
        let mut r = s.to_string();
        if let Some(v) = subst.source {
            r = r.replace("{source}", v);
        }
        if let Some(v) = subst.image {
            r = r.replace("{image}", v);
        }
        if let Some(v) = subst.config {
            r = r.replace("{config}", v);
        }
        if let Some(v) = subst.tsconfig {
            r = r.replace("{tsconfig}", v);
        }
        if let Some(v) = subst.skip_dirs {
            r = r.replace("{skip_dirs}", v);
        }
        r
    };

    let rendered = apply(cmdline);
    writeln!(log, "\n── {label}\n   $ {rendered}")?;

    let parts = shlex::split(&rendered).unwrap_or_default();
    let Some((program, args)) = parts.split_first() else {
        let msg = format!("could not parse command: {rendered}");
        eprintln!("   ! {msg}");
        return Ok(StepResult {
            label: label.into(),
            language: None,
            project: None,
            cmd: rendered,
            ok: false,
            exit_code: None,
            secs: 0.0,
            output: msg,
        });
    };

    let start = Instant::now();
    let mut command = ProcessCommand::new(program);
    command.args(args).current_dir(cwd);
    for (k, v) in env {
        command.env(k, apply(v));
    }
    // Capture-only: collect the tool's combined output rather than streaming it,
    // so the full text can be replayed in the FAILURES block and written
    // verbatim to report.json. Checks run sequentially, so printing each one's
    // captured output as it finishes preserves the same per-check ordering a
    // live stream would have shown.
    let output = command.output();
    let secs = start.elapsed().as_secs_f64();

    let (ok, exit_code, captured) = match output {
        Ok(o) => {
            let mut buf = String::from_utf8_lossy(&o.stdout).into_owned();
            buf.push_str(&String::from_utf8_lossy(&o.stderr));
            write!(log, "{buf}")?;
            if !buf.is_empty() && !buf.ends_with('\n') {
                writeln!(log)?;
            }
            (o.status.success(), o.status.code(), buf)
        }
        Err(e) => {
            let msg = format!("failed to spawn {program}: {e}");
            eprintln!("   ! {msg}");
            (false, None, msg)
        }
    };
    Ok(StepResult {
        label: label.into(),
        language: None,
        project: None,
        cmd: rendered,
        ok,
        exit_code,
        secs,
        output: captured,
    })
}

/// cosign sign by digest. `COSIGN_PASSWORD` is inherited from the environment
/// (delivered to the runner by ESO from `OpenBao`).
fn sign_image(image: &str, key: &str, insecure: bool) -> Result<()> {
    // --tlog-upload=false keeps signing self-contained: no dependency on (and no
    // digest leakage to) the public Rekor transparency log. Verification is
    // key-based, so a transparency log isn't needed.
    let mut args = vec!["sign", "--yes", "--tlog-upload=false", "--key", key];
    if insecure {
        args.push("--allow-insecure-registry");
    }
    args.push(image);
    let status = ProcessCommand::new("cosign")
        .args(&args)
        .status()
        .context("spawning cosign")?;
    if !status.success() {
        bail!("cosign sign failed for {image}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_SUBST: Subst<'static> = Subst {
        source: None,
        image: None,
        config: None,
        tsconfig: None,
        skip_dirs: None,
    };

    #[test]
    fn run_captures_failing_tool_output() {
        let env = BTreeMap::new();
        let r = run(
            &mut std::io::sink(),
            "t:fail",
            "sh -c 'echo boom >&2; exit 3'",
            Path::new("."),
            NO_SUBST,
            &env,
        )
        .expect("logging to a sink cannot fail");
        assert!(!r.ok, "a non-zero exit must mark the step failed");
        assert!(
            r.output.contains("boom"),
            "the tool's real output is captured: {:?}",
            r.output
        );
        assert_eq!(r.exit_code, Some(3), "the exit code is recorded");
    }

    #[test]
    fn skip_dirs_token_substituted_in_cmd_and_env() {
        // The {skip_dirs} token must be forwarded both on the command line and in
        // env values, so a check (e.g. eslint via GATE_SKIP_DIRS) can hand its
        // tool the Ops-owned skip list. Mirrors the {tsconfig} wiring.
        let subst = Subst {
            source: None,
            image: None,
            config: None,
            tsconfig: None,
            skip_dirs: Some("dist,build,.svelte-kit"),
        };
        let mut env = BTreeMap::new();
        env.insert("SD".to_string(), "{skip_dirs}".to_string());
        let r = run(
            &mut std::io::sink(),
            "t:skip",
            "sh -c 'echo cmd={skip_dirs}; echo env=$SD'",
            Path::new("."),
            subst,
            &env,
        )
        .expect("logging to a sink cannot fail");
        assert!(r.ok, "echo succeeds: {:?}", r.output);
        assert!(
            r.output.contains("cmd=dist,build,.svelte-kit"),
            "the {{skip_dirs}} token is replaced in the command: {:?}",
            r.output
        );
        assert!(
            r.output.contains("env=dist,build,.svelte-kit"),
            "the {{skip_dirs}} token is replaced in env values: {:?}",
            r.output
        );
    }

    #[test]
    fn crate_name_parses_package_workspace_and_rejects_garbage() {
        assert_eq!(
            crate_name_from_manifest("[package]\nname = \"thing\"\nversion = \"1.0.0\"\n")
                .expect("a valid manifest parses"),
            Some("thing".to_string()),
            "[package].name is read from a normal manifest"
        );
        assert_eq!(
            crate_name_from_manifest("[workspace]\nmembers = [\"a\"]\n")
                .expect("a valid manifest parses"),
            None,
            "a virtual workspace manifest has no package name"
        );
        assert!(
            crate_name_from_manifest("not = valid = toml [[[").is_err(),
            "unparseable TOML is an error (the caller warns and skips, never fatal)"
        );
    }

    #[test]
    fn tail_cap_returns_whole_small_input_untruncated() {
        let body = "line one\nline two\n";
        let (out, truncated) = tail_cap(body, 200, 16 * 1024);
        assert_eq!(out, body, "small input is returned whole");
        assert!(!truncated, "nothing dropped from a small input");
    }

    #[test]
    fn tail_cap_keeps_only_the_tail_when_line_capped() {
        use std::fmt::Write as _;
        let mut body = String::new();
        for i in 0..20 {
            writeln!(body, "line {i}").unwrap();
        }
        let (out, truncated) = tail_cap(&body, 3, 16 * 1024);
        assert!(truncated, "an over-long input is flagged truncated");
        assert!(
            out.contains("line 19") && !out.contains("line 0\n"),
            "only the trailing lines are kept: {out:?}"
        );
    }
}
