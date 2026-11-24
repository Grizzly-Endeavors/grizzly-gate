mod config;
mod detect;
mod distill;
mod gateconfig;
mod mcp;
mod report;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Scope;
use gateconfig::ResolvedProject;
use report::Report;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Instant;

const DEFAULT_CONFIG_DIR: &str = "/etc/grizzly-gate/config";

#[derive(Parser)]
#[command(
    name = "grizzly-gate",
    version,
    about = "grizzly-platform CI gate harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, default_value = ".")]
    source: PathBuf,

    #[arg(long)]
    image: Option<String>,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    sign: bool,

    #[arg(long)]
    cosign_key: Option<String>,

    /// Allow cosign to talk to a plain-HTTP / self-signed registry (the
    /// in-cluster zot is HTTP-only).
    #[arg(long)]
    insecure_registry: bool,

    #[arg(long, default_value = "./grizzly-gate-report")]
    report_dir: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    Mcp(McpArgs),
}

#[derive(clap::Args)]
struct McpArgs {
    #[arg(long, default_value = ".")]
    source: PathBuf,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long, default_value = "./grizzly-gate-report")]
    report_dir: PathBuf,
}

fn resolve_config_root(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("GRIZZLY_GATE_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR))
}

pub struct GateRun {
    pub report: Report,
    pub honest_map_message: Option<String>,
    results: Vec<StepResult>,
}

struct StepResult {
    label: String,
    language: Option<String>,
    project: Option<String>,
    cmd: String,
    ok: bool,
    exit_code: Option<i32>,
    secs: f64,
    findings: Vec<report::Finding>,
    distilled: String,
    output: String,
}

impl StepResult {
    fn to_report(&self) -> report::Check {
        report::Check {
            label: self.label.clone(),
            language: self.language.clone(),
            project: self.project.clone(),
            cmd: self.cmd.clone(),
            ok: self.ok,
            exit_code: self.exit_code,
            duration_secs: self.secs,
            findings: self.findings.clone(),
            distilled: self.distilled.clone(),
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

fn run_once(cli: &Cli) -> Result<()> {
    let config_root = resolve_config_root(cli.config.as_deref());
    let tree = config::load_tree(&config_root)
        .with_context(|| format!("loading gate config from {}", config_root.display()))?;

    let source = cli
        .source
        .canonicalize()
        .with_context(|| format!("resolving source path {}", cli.source.display()))?;

    let run = {
        let mut out = std::io::stdout().lock();
        execute(&mut out, &tree, &source, cli.image.as_deref())?
    };
    let GateRun {
        mut report,
        honest_map_message,
        results,
    } = run;

    if let Some(message) = honest_map_message {
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

const FAILURE_TAIL_LINES: usize = 200;
const FAILURE_TAIL_BYTES: usize = 16 * 1024;

fn print_failures(results: &[StepResult], report: &Report, report_path: &Path) {
    println!("\n──────────────────────────── FAILURES ────────────────────────────");
    for r in results.iter().filter(|r| !r.ok) {
        println!("\n▼ {}", r.label);
        let display = distill::display_text(&r.findings, &r.distilled);
        let (tail, truncated) = tail_cap(&display, FAILURE_TAIL_LINES, FAILURE_TAIL_BYTES);
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

fn tail_cap(s: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let line_start = s
        .match_indices('\n')
        .rev()
        .nth(max_lines)
        .map_or(0, |(i, _)| i + 1);
    let cut = start.max(line_start);
    (s.get(cut..).unwrap_or_default().to_string(), cut > 0)
}

#[derive(Clone, Copy)]
struct Subst<'a> {
    source: Option<&'a str>,
    image: Option<&'a str>,
    config: Option<&'a str>,
    tsconfig: Option<&'a str>,
    skip_dirs: Option<&'a str>,
    work: Option<&'a str>,
}

const DEFAULT_WORK_DIR: &str = "/gate-work";

struct WorkRoot {
    path: PathBuf,
}

impl WorkRoot {
    fn new() -> Result<Self> {
        let base = std::env::var_os("GRIZZLY_GATE_WORK_DIR")
            .map_or_else(|| PathBuf::from(DEFAULT_WORK_DIR), PathBuf::from);
        let path = base.join(std::process::id().to_string());
        remove_path(&path)
            .with_context(|| format!("clearing stale work dir {}", path.display()))?;
        std::fs::create_dir_all(&path).with_context(|| {
            format!(
                "creating work dir {} (set GRIZZLY_GATE_WORK_DIR to relocate it \
                 when running the harness outside the gate container)",
                path.display()
            )
        })?;
        Ok(Self { path })
    }

    fn project_dir(&self, language: &str, idx: usize) -> Result<PathBuf> {
        let path = self.path.join(format!("{language}-{idx}"));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating project work dir {}", path.display()))?;
        Ok(path)
    }
}

impl Drop for WorkRoot {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "grizzly-gate :: warning: could not remove work dir {}: {e}",
                self.path.display()
            );
        }
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

const NODE_MODULES: &str = "node_modules";
const NODE_MODULES_STASH: &str = ".grizzly-gate.node_modules";

/// Keeps a node project's installed dependencies out of the scanned repo's
/// steady state.
///
/// Unlike the Python virtualenv, `node_modules` cannot be relocated to
/// [`WorkRoot`]: node resolves it beside `package.json`, `npm ci` deletes and
/// recreates it in the project dir (verified — it replaces even a symlink
/// pointing elsewhere with a real directory), and a repo's own `prepare` script
/// legitimately generates files there that its tsconfig extends, e.g. `SvelteKit`'s
/// `svelte-kit sync`. So the install happens in the tree and is undone
/// afterwards. Whatever was already installed is renamed aside first — a rename
/// within one directory, so it is atomic and costs nothing however large the dir
/// is — and moved back by [`Self::restore`]. See ADR-042.
struct NodeModulesStash {
    live: PathBuf,
    stashed: Option<PathBuf>,
}

impl NodeModulesStash {
    fn park(project_dir: &Path) -> Result<Self> {
        let live = project_dir.join(NODE_MODULES);
        let stash = project_dir.join(NODE_MODULES_STASH);
        if live.symlink_metadata().is_err() {
            return Ok(Self {
                live,
                stashed: None,
            });
        }
        if stash.symlink_metadata().is_ok() {
            remove_path(&live)
                .with_context(|| format!("removing stale gate install {}", live.display()))?;
        } else {
            std::fs::rename(&live, &stash).with_context(|| {
                format!("parking {} aside as {}", live.display(), stash.display())
            })?;
        }
        Ok(Self {
            live,
            stashed: Some(stash),
        })
    }

    fn restore(self) {
        if let Err(e) = remove_path(&self.live) {
            eprintln!(
                "grizzly-gate :: warning: could not remove gate-installed {}: {e}",
                self.live.display()
            );
            return;
        }
        if let Some(stash) = self.stashed {
            if let Err(e) = std::fs::rename(&stash, &self.live) {
                eprintln!(
                    "grizzly-gate :: warning: could not restore {} from {}: {e}",
                    self.live.display(),
                    stash.display()
                );
            }
        }
    }
}

struct EggInfo {
    root: PathBuf,
    skip_dirs: Vec<String>,
    before: BTreeSet<PathBuf>,
}

impl EggInfo {
    fn snapshot(root: &Path, skip_dirs: &[String]) -> Self {
        Self {
            root: root.to_path_buf(),
            skip_dirs: skip_dirs.to_vec(),
            before: egg_info_dirs(root, skip_dirs),
        }
    }

    fn remove_generated(self) {
        for path in egg_info_dirs(&self.root, &self.skip_dirs) {
            if self.before.contains(&path) {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(&path) {
                eprintln!(
                    "grizzly-gate :: warning: could not remove generated {}: {e}",
                    path.display()
                );
            }
        }
    }
}

fn egg_info_dirs(root: &Path, skip_dirs: &[String]) -> BTreeSet<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            name != ".git" && !skip_dirs.iter().any(|d| *d == name)
        })
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_dir() && e.file_name().to_string_lossy().ends_with(".egg-info")
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

fn run_adapter(
    log: &mut dyn Write,
    tree: &config::Tree,
    adapter: &config::LanguageAdapter,
    project: &ResolvedProject,
    work_dir: &Path,
    skip_dirs: &str,
) -> Result<Vec<StepResult>> {
    let mut results: Vec<StepResult> = Vec::new();
    let cfg = adapter.config_dir.to_string_lossy().to_string();
    let proj_str = project.abs_path.to_string_lossy().to_string();
    let work_str = work_dir.to_string_lossy().to_string();
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

    let node_modules = (adapter.name == "node")
        .then(|| NodeModulesStash::park(&project.abs_path))
        .transpose()?;
    let egg_info = (adapter.name == "python")
        .then(|| EggInfo::snapshot(&project.abs_path, &tree.detect.skip_dirs));

    let mut ts: Option<ResolvedTsconfig> = None;
    let mut ts_resolved = false;
    for check in &adapter.checks {
        let needs_ts = check.cmd.contains("{tsconfig}")
            || check.env.values().any(|v| v.contains("{tsconfig}"));
        if needs_ts && !ts_resolved {
            ts = resolve_tsconfig(adapter, project, &tree.detect.skip_dirs)?;
            ts_resolved = true;
        }
        let subst = Subst {
            source: Some(&proj_str),
            image: None,
            config: Some(&cfg),
            tsconfig: ts.as_ref().map(|t| t.arg.as_str()),
            skip_dirs: Some(skip_dirs),
            work: Some(&work_str),
        };
        let mut result = run(
            log,
            &format!("{}:{}", adapter.name, check.name),
            &check.cmd,
            &project.abs_path,
            subst,
            &check.env,
            &check.output,
        )?;
        result.language = Some(adapter.name.clone());
        result.project = Some(where_.clone());
        results.push(result);
    }
    if let Some(t) = ts {
        t.cleanup();
    }
    if let Some(nm) = node_modules {
        nm.restore();
    }
    if let Some(e) = egg_info {
        e.remove_generated();
    }
    Ok(results)
}

fn run_checks(
    log: &mut dyn Write,
    tree: &config::Tree,
    source: &Path,
    image: Option<&str>,
    projects: &[ResolvedProject],
) -> Result<Vec<StepResult>> {
    let mut results: Vec<StepResult> = Vec::new();

    let skip_dirs = tree.detect.skip_dirs.join(",");

    let eslint_config = materialize_eslint_config(tree, projects)?;

    let work = WorkRoot::new()?;

    for (idx, project) in projects.iter().enumerate() {
        let adapter = tree
            .adapters
            .iter()
            .find(|a| a.name == project.language)
            .with_context(|| format!("no adapter for declared language {:?}", project.language))?;
        let work_dir = work.project_dir(&adapter.name, idx)?;
        results.extend(run_adapter(
            log, tree, adapter, project, &work_dir, &skip_dirs,
        )?);
    }

    if let Some(cfg) = eslint_config {
        cfg.cleanup();
    }

    let osv_config = materialize_osv_config(tree, projects, &tree.detect.skip_dirs)?;

    let source_str = source.to_string_lossy().to_string();
    for scanner in &tree.scanners {
        let cfg = scanner.config_dir.to_string_lossy().to_string();
        let subst = Subst {
            source: Some(&source_str),
            image,
            config: Some(&cfg),
            tsconfig: None,
            skip_dirs: Some(&skip_dirs),
            work: None,
        };
        let label = format!("scan:{}", scanner.name);
        match scanner.scope {
            Scope::Source => {
                results.push(run(
                    log,
                    &label,
                    &scanner.cmd,
                    source,
                    subst,
                    &scanner.env,
                    &scanner.output,
                )?);
            }
            Scope::Image if image.is_some() => {
                results.push(run(
                    log,
                    &label,
                    &scanner.cmd,
                    source,
                    subst,
                    &scanner.env,
                    &scanner.output,
                )?);
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

struct ResolvedTsconfig {
    arg: String,
    temp: Option<PathBuf>,
}

impl ResolvedTsconfig {
    fn cleanup(self) {
        if let Some(p) = self.temp {
            if let Err(e) = std::fs::remove_file(&p) {
                eprintln!(
                    "grizzly-gate :: warning: could not remove tsconfig wrapper {}: {e}",
                    p.display()
                );
            }
        }
    }
}

const TS_WRAPPER: &str = ".grizzly-gate.tsconfig.json";

fn resolve_tsconfig(
    adapter: &config::LanguageAdapter,
    project: &ResolvedProject,
    skip_dirs: &[String],
) -> Result<Option<ResolvedTsconfig>> {
    if adapter.name != "node" {
        return Ok(None);
    }
    let Some(repo_ts) = &project.tsconfig else {
        let base = adapter.config_dir.join("tsconfig.base.json");
        return Ok(Some(ResolvedTsconfig {
            arg: base.to_string_lossy().to_string(),
            temp: None,
        }));
    };

    let repo_exclude = repo_tsconfig_exclude(adapter, project, repo_ts)?;
    let mut exclude: Vec<String> = skip_dirs.to_vec();
    for d in repo_exclude {
        if !exclude.contains(&d) {
            exclude.push(d);
        }
    }

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
        },
        "exclude": exclude,
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

/// The `exclude` array the repo's own tsconfig resolves to, per `tsc`'s own
/// config resolution (`--showConfig`, which prints the effective config
/// without emitting anything). Using the real, pinned `tsc` binary — rather
/// than parsing `repo_ts` by hand — gets JSONC tolerance (comments/trailing
/// commas) and transitive `extends` resolution for free, both verified
/// empirically (see `resolve_tsconfig` and ADR-039). Returns an empty list if
/// the repo's tsconfig declares no `exclude` anywhere in its `extends` chain
/// (confirmed: an absent `exclude` shows up as a missing key in `--showConfig`
/// output, not a populated default list — tsc's default excludes are applied
/// later, at file-resolution time, not reflected here).
fn repo_tsconfig_exclude(
    adapter: &config::LanguageAdapter,
    project: &ResolvedProject,
    repo_ts: &Path,
) -> Result<Vec<String>> {
    let tsc = adapter.config_dir.join("node_modules/.bin/tsc");
    let output = ProcessCommand::new(&tsc)
        .arg("--showConfig")
        .arg("--project")
        .arg(repo_ts)
        .current_dir(&project.abs_path)
        .output()
        .with_context(|| {
            format!(
                "running {} --showConfig on the repo tsconfig {}",
                tsc.display(),
                repo_ts.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "{} --showConfig --project {} failed (exit {:?}): {}",
            tsc.display(),
            repo_ts.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "parsing `{} --showConfig --project {}` output as JSON",
            tsc.display(),
            repo_ts.display()
        )
    })?;
    Ok(parsed
        .get("exclude")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

const ESLINT_TEMPLATE: &str = "eslint.config.mjs.tmpl";
const ESLINT_CONFIG: &str = "eslint.config.mjs";

struct MaterializedEslintConfig {
    path: PathBuf,
}

impl MaterializedEslintConfig {
    fn cleanup(self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            eprintln!(
                "grizzly-gate :: warning: could not remove eslint config {}: {e}",
                self.path.display()
            );
        }
    }
}

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

const OSV_CONFIG: &str = "osv-scanner.toml";

const OSV_IGNORES: &str = "ignored-advisories.toml";

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
        body.push_str("\n[[PackageOverrides]]\nname = \"");
        body.push_str(&name);
        body.push_str(
            "\"\nlicense.ignore = true\n\
             reason = \"the scanned project's own package; the gate checks dependency licenses, not the repo's own\"\n",
        );
    }

    let ignores = scanner.config_dir.join(OSV_IGNORES);
    match std::fs::read_to_string(&ignores) {
        Ok(content) => {
            body.push('\n');
            body.push_str(&content);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("reading osv ignores {}", ignores.display()));
        }
    }

    let dst = scanner.config_dir.join(OSV_CONFIG);
    std::fs::write(&dst, body).with_context(|| format!("writing osv config {}", dst.display()))?;
    Ok(Some(MaterializedOsvConfig { path: dst }))
}

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

fn crate_name_from_manifest(text: &str) -> Result<Option<String>, toml::de::Error> {
    let manifest: CargoManifest = toml::from_str(text)?;
    Ok(manifest.package.map(|p| p.name))
}

#[derive(serde::Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
}

#[derive(serde::Deserialize)]
struct CargoPackage {
    name: String,
}

fn run(
    log: &mut dyn Write,
    label: &str,
    cmdline: &str,
    cwd: &Path,
    subst: Subst,
    env: &BTreeMap<String, String>,
    out_spec: &config::OutputSpec,
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
        if let Some(v) = subst.work {
            r = r.replace("{work}", v);
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
            findings: Vec::new(),
            distilled: msg.clone(),
            output: msg,
        });
    };

    let start = Instant::now();
    let mut command = ProcessCommand::new(program);
    command.args(args).current_dir(cwd);
    for (k, v) in env {
        command.env(k, apply(v));
    }
    let output = command.output();
    let secs = start.elapsed().as_secs_f64();

    let (ok, exit_code, findings, distilled, captured) = match output {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            let mut buf = out.clone().into_owned();
            buf.push_str(&err);
            write!(log, "{buf}")?;
            if !buf.is_empty() && !buf.ends_with('\n') {
                writeln!(log)?;
            }
            let (mut findings, distilled) = distill::apply(out_spec, &out, &err);
            distill::relativize(&mut findings, &cwd.to_string_lossy());
            (
                o.status.success(),
                o.status.code(),
                findings,
                distilled,
                buf,
            )
        }
        Err(e) => {
            let msg = format!("failed to spawn {program}: {e}");
            eprintln!("   ! {msg}");
            (false, None, Vec::new(), msg.clone(), msg)
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
        findings,
        distilled,
        output: captured,
    })
}

fn sign_image(image: &str, key: &str, insecure: bool) -> Result<()> {
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
        work: None,
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
            &config::OutputSpec::default(),
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
        let subst = Subst {
            source: None,
            image: None,
            config: None,
            tsconfig: None,
            skip_dirs: Some("dist,build,.svelte-kit"),
            work: None,
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
            &config::OutputSpec::default(),
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
    fn work_token_substituted_in_cmd_and_env() {
        let subst = Subst {
            source: None,
            image: None,
            config: None,
            tsconfig: None,
            skip_dirs: None,
            work: Some("/tmp/gate-work/python-0"),
        };
        let mut env = BTreeMap::new();
        env.insert("W".to_string(), "{work}/venv".to_string());
        let r = run(
            &mut std::io::sink(),
            "t:work",
            "sh -c 'echo cmd={work}; echo env=$W'",
            Path::new("."),
            subst,
            &env,
            &config::OutputSpec::default(),
        )
        .expect("logging to a sink cannot fail");
        assert!(
            r.output.contains("cmd=/tmp/gate-work/python-0"),
            "the {{work}} token is replaced in the command: {:?}",
            r.output
        );
        assert!(
            r.output.contains("env=/tmp/gate-work/python-0/venv"),
            "the {{work}} token is replaced in env values: {:?}",
            r.output
        );
    }

    fn fake_npm_ci(project: &Path, marker: &str) {
        let dir = project.join(NODE_MODULES);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("creating the gate's node_modules");
        std::fs::write(dir.join("installed-by"), marker).expect("writing the install marker");
    }

    #[test]
    fn node_modules_stash_restores_the_repos_own_install() {
        let root = scratch("nm-restore");
        std::fs::remove_dir_all(&root).ok();
        let project = root.join("web");
        std::fs::create_dir_all(project.join(NODE_MODULES)).expect("creating the repo's deps");
        std::fs::write(
            project.join(NODE_MODULES).join("installed-by"),
            "the developer",
        )
        .expect("writing the repo's marker");

        let stash = NodeModulesStash::park(&project).expect("parking the repo's node_modules");
        assert!(
            !project.join(NODE_MODULES).exists(),
            "parking clears the way for a fresh install"
        );
        fake_npm_ci(&project, "the gate");
        stash.restore();

        assert_eq!(
            std::fs::read_to_string(project.join(NODE_MODULES).join("installed-by"))
                .expect("the repo's node_modules is back"),
            "the developer",
            "restore returns the developer's install, not the gate's"
        );
        assert!(
            !project.join(NODE_MODULES_STASH).exists(),
            "no stash is left behind in the scanned tree"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn node_modules_stash_is_removed_when_the_repo_had_none() {
        let root = scratch("nm-none");
        std::fs::remove_dir_all(&root).ok();
        let project = root.join("web");
        std::fs::create_dir_all(&project).expect("creating the project dir");

        let stash = NodeModulesStash::park(&project).expect("parking with nothing to park");
        fake_npm_ci(&project, "the gate");
        stash.restore();

        assert!(
            !project.join(NODE_MODULES).exists(),
            "the gate's install does not outlive the run"
        );
        assert!(
            !project.join(NODE_MODULES_STASH).exists(),
            "and neither does a stash"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn node_modules_stash_recovers_from_an_interrupted_run() {
        let root = scratch("nm-interrupted");
        std::fs::remove_dir_all(&root).ok();
        let project = root.join("web");
        std::fs::create_dir_all(project.join(NODE_MODULES_STASH)).expect("creating a stale stash");
        std::fs::write(
            project.join(NODE_MODULES_STASH).join("installed-by"),
            "the developer",
        )
        .expect("writing the repo's marker");
        fake_npm_ci(&project, "a killed gate run");

        let stash = NodeModulesStash::park(&project).expect("parking with a stale stash present");
        fake_npm_ci(&project, "the gate");
        stash.restore();

        assert_eq!(
            std::fs::read_to_string(project.join(NODE_MODULES).join("installed-by"))
                .expect("the repo's node_modules is back"),
            "the developer",
            "the stale stash is the developer's copy and is what gets restored"
        );
        assert!(
            !project.join(NODE_MODULES_STASH).exists(),
            "the interruption is healed, not carried forward"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn egg_info_removes_only_what_the_run_generated() {
        // `uv pip install -e .` writes an *.egg-info beside the package it
        // describes — inside the tree, where {work} cannot reach (ADR-042).
        // Snapshot-and-diff so a repo that checks one in keeps it.
        let root = scratch("egg-info");
        std::fs::remove_dir_all(&root).ok();
        let src = root.join("src");
        let committed = src.join("committed.egg-info");
        std::fs::create_dir_all(&committed).expect("creating the repo's own egg-info");

        let skip_dirs = vec!["node_modules".to_string()];
        let egg = EggInfo::snapshot(&root, &skip_dirs);
        let generated = src.join("generated.egg-info");
        std::fs::create_dir_all(&generated).expect("creating the install's egg-info");
        egg.remove_generated();

        assert!(
            committed.exists(),
            "an egg-info that predates the run is the repo's and is left alone"
        );
        assert!(
            !generated.exists(),
            "one that appeared during the run is the gate's and is removed"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gate-main-{tag}-{}", std::process::id()))
    }

    #[test]
    fn resolve_tsconfig_unions_skip_dirs_with_the_repos_own_exclude() {
        // Follow-up to the eslint/ruff/mypy/golangci-lint skip_dirs wiring
        // (ADR-039, issue #3): the wrapper tsconfig `resolve_tsconfig` writes
        // must exclude the Ops-owned skip_dirs WITHOUT dropping whatever the
        // repo's own tsconfig already excludes. `extends` does not merge
        // `exclude` between parent and child (verified with a real `tsc
        // --showConfig`, pinned 6.0.3 — the child's value fully replaces the
        // parent's), so naively setting `"exclude": [...skip_dirs]` on the
        // wrapper would silently drop a repo's real exclude entries.
        //
        // A stub `tsc` stands in for `--showConfig` here (real TypeScript is
        // not a harness build dependency), but it exercises the actual
        // `resolve_tsconfig`/`repo_tsconfig_exclude` code path — real
        // `Command` spawn, real file write, real JSON parse of the result —
        // not a mocked-out substitute for that logic.
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("resolve-tsconfig");
        let config_dir = root.join("config/languages/node");
        let bin_dir = config_dir.join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let stub_tsc = bin_dir.join("tsc");
        std::fs::write(
            &stub_tsc,
            "#!/bin/sh\ncat <<'EOF'\n{\"exclude\": [\"something-else\"]}\nEOF\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub_tsc).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub_tsc, perms).unwrap();

        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let repo_ts = project_dir.join("tsconfig.json");
        // Content is irrelevant — the stub tsc ignores its `--project` arg and
        // always answers with the canned `exclude` above.
        std::fs::write(&repo_ts, "{}").unwrap();

        let adapter = config::LanguageAdapter {
            name: "node".to_string(),
            marker: "package.json".to_string(),
            config_dir,
            detect: config::Detect::default(),
            checks: Vec::new(),
        };
        let project = ResolvedProject {
            language: "node".to_string(),
            rel_path: PathBuf::new(),
            abs_path: project_dir,
            tsconfig: Some(repo_ts),
        };
        let skip_dirs = vec![
            "dist".to_string(),
            "build".to_string(),
            ".svelte-kit".to_string(),
        ];

        let resolved = resolve_tsconfig(&adapter, &project, &skip_dirs)
            .expect("resolve_tsconfig must succeed against the stub tsc")
            .expect("a declared repo tsconfig always produces a wrapper");

        let written = std::fs::read_to_string(&resolved.arg).expect("wrapper file must exist");
        let wrapper: serde_json::Value =
            serde_json::from_str(&written).expect("wrapper must be valid JSON");
        let exclude: Vec<String> = wrapper
            .get("exclude")
            .and_then(|v| v.as_array())
            .expect("wrapper must set `exclude`")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

        for want in ["dist", "build", ".svelte-kit", "something-else"] {
            assert!(
                exclude.iter().any(|e| e == want),
                "wrapper `exclude` must contain {want:?} (skip_dirs ∪ repo's own exclude): {exclude:?}"
            );
        }

        resolved.cleanup();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tsconfig_resolution_waits_for_the_deps_check() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("lazy-tsconfig");
        let config_dir = root.join("config/languages/node");
        let bin_dir = config_dir.join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(config_dir.join(ESLINT_TEMPLATE), "export default [];\n").unwrap();

        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let repo_ts = project_dir.join("tsconfig.json");
        std::fs::write(&repo_ts, "{}").unwrap();

        let marker = project_dir.join("generated-by-deps");
        let stub_tsc = bin_dir.join("tsc");
        std::fs::write(
            &stub_tsc,
            format!(
                "#!/bin/sh\n[ -f {} ] || exit 1\ncat <<'EOF'\n{{\"exclude\": []}}\nEOF\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub_tsc).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub_tsc, perms).unwrap();

        let adapter = config::LanguageAdapter {
            name: "node".to_string(),
            marker: "package.json".to_string(),
            config_dir,
            detect: config::Detect::default(),
            checks: vec![
                config::Check {
                    name: "deps".to_string(),
                    cmd: "touch generated-by-deps".to_string(),
                    env: BTreeMap::new(),
                    output: config::OutputSpec::default(),
                },
                config::Check {
                    name: "tsc".to_string(),
                    cmd: "true {tsconfig}".to_string(),
                    env: BTreeMap::new(),
                    output: config::OutputSpec::default(),
                },
            ],
        };
        let tree = config::Tree {
            adapters: vec![adapter],
            scanners: Vec::new(),
            detect: config::DetectRules {
                skip_dirs: Vec::new(),
                unsupported: Vec::new(),
            },
        };
        let projects = vec![ResolvedProject {
            language: "node".to_string(),
            rel_path: PathBuf::new(),
            abs_path: project_dir,
            tsconfig: Some(repo_ts),
        }];

        std::env::set_var("GRIZZLY_GATE_WORK_DIR", root.join("work"));

        let mut log = Vec::new();
        let results = run_checks(&mut log, &tree, &root, None, &projects)
            .expect("tsconfig resolution must run after the deps check has generated its file");
        assert!(
            results.iter().all(|r| r.ok),
            "both checks must pass once resolution is deferred: {:?}",
            results
                .iter()
                .map(|r| (r.label.clone(), r.ok))
                .collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&root).ok();
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
