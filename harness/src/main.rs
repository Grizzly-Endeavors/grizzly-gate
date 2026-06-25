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

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Scope;
use gateconfig::ResolvedProject;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

struct StepResult {
    label: String,
    ok: bool,
    secs: f64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_root = cli
        .config
        .clone()
        .or_else(|| std::env::var_os("GRIZZLY_GATE_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR));
    let tree = config::load_tree(&config_root)
        .with_context(|| format!("loading gate config from {}", config_root.display()))?;

    let source = cli
        .source
        .canonicalize()
        .with_context(|| format!("resolving source path {}", cli.source.display()))?;

    println!("grizzly-gate :: gating {}", source.display());
    if let Some(image) = &cli.image {
        println!("grizzly-gate :: image {image}");
    }

    // --- Honest map: required declaration, then independent verification -----
    // The repo must ship a gate-config.json that truthfully maps its layout, and
    // the tree must contain no undeclared or unsupported code. Both are fatal
    // (fail closed) and happen before any check runs — a repo that lies about
    // (or omits) its contents never reaches the checks, let alone signing.
    let projects = gateconfig::load(&source, &tree)?;
    println!(
        "grizzly-gate :: gate-config.json declares {} project(s)",
        projects.len()
    );
    for p in &projects {
        let where_ = if p.rel_path.as_os_str().is_empty() {
            ".".to_string()
        } else {
            p.rel_path.display().to_string()
        };
        println!("grizzly-gate ::   - {} @ {where_}", p.language);
    }
    detect::verify(&source, &tree, &projects).context("honest-map verification")?;
    println!("grizzly-gate :: honest-map verification passed");

    let results = run_checks(&tree, &source, cli.image.as_deref(), &projects)?;

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
    if results.is_empty() {
        bail!("gate ran zero checks — refusing to pass (fail closed)");
    }
    println!("───────────────────────────────────────────────────────────────────");

    if failed > 0 {
        bail!("gate FAILED: {failed}/{} checks failed", results.len());
    }
    println!("gate PASSED: {}/{} checks", results.len(), results.len());

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
}

/// Run each declared project's adapter checks (in its own directory) and every
/// scanner in scope, returning their results in execution order.
///
/// Adapters run per *declared project* — not by scanning the root for a marker —
/// so a Rust crate in a subdir or a second project in a monorepo is checked
/// exactly where the (already-verified) `gate-config.json` says it lives.
fn run_checks(
    tree: &config::Tree,
    source: &Path,
    image: Option<&str>,
    projects: &[ResolvedProject],
) -> Result<Vec<StepResult>> {
    let mut results: Vec<StepResult> = Vec::new();

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
        println!(
            "\n=== {} @ {where_} (marker: {}) ===",
            adapter.name, adapter.marker
        );

        // For node, resolve the tsconfig the checks use. A repo-declared tsconfig
        // is wrapped so its module/path resolution is honored while the gate's
        // strictness is force-overridden; the wrapper is cleaned up after.
        let ts = resolve_tsconfig(adapter, project)?;
        let subst = Subst {
            source: Some(&proj_str),
            image: None,
            config: Some(&cfg),
            tsconfig: ts.as_ref().map(|t| t.arg.as_str()),
        };
        for check in &adapter.checks {
            results.push(run(
                &format!("{}:{}", adapter.name, check.name),
                &check.cmd,
                &project.abs_path,
                subst,
                &check.env,
            ));
        }
        if let Some(t) = ts {
            t.cleanup();
        }
    }

    // --- Scanners ----------------------------------------------------------
    let source_str = source.to_string_lossy().to_string();
    for scanner in &tree.scanners {
        let cfg = scanner.config_dir.to_string_lossy().to_string();
        let subst = Subst {
            source: Some(&source_str),
            image,
            config: Some(&cfg),
            tsconfig: None,
        };
        let label = format!("scan:{}", scanner.name);
        match scanner.scope {
            Scope::Source => results.push(run(&label, &scanner.cmd, source, subst, &scanner.env)),
            Scope::Image if image.is_some() => {
                results.push(run(&label, &scanner.cmd, source, subst, &scanner.env));
            }
            Scope::Image => println!(
                "grizzly-gate :: skipping image scanner '{}' (no --image given)",
                scanner.name
            ),
        }
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

/// Run one command line in `cwd` after applying `subst` to the command and to
/// each env value.
fn run(
    label: &str,
    cmdline: &str,
    cwd: &Path,
    subst: Subst,
    env: &BTreeMap<String, String>,
) -> StepResult {
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
        r
    };

    let rendered = apply(cmdline);
    println!("\n── {label}\n   $ {rendered}");

    let parts = shlex::split(&rendered).unwrap_or_default();
    let Some((program, args)) = parts.split_first() else {
        eprintln!("   ! could not parse command: {rendered}");
        return StepResult {
            label: label.into(),
            ok: false,
            secs: 0.0,
        };
    };

    let start = Instant::now();
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    for (k, v) in env {
        command.env(k, apply(v));
    }
    let status = command.status();
    let secs = start.elapsed().as_secs_f64();

    let ok = match status {
        Ok(s) => s.success(),
        Err(e) => {
            eprintln!("   ! failed to spawn {program}: {e}");
            false
        }
    };
    StepResult {
        label: label.into(),
        ok,
        secs,
    }
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
    let status = Command::new("cosign")
        .args(&args)
        .status()
        .context("spawning cosign")?;
    if !status.success() {
        bail!("cosign sign failed for {image}");
    }
    Ok(())
}
