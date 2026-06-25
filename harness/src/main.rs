//! grizzly-gate — the orchestration harness for the grizzly-platform CI gate.
//!
//! One artifact, centrally owned by Ops: it detects the stacks in a repo, runs
//! the pinned per-language adapters + scanners defined in the `config/` tree,
//! and — only if everything passes — signs the built image with cosign. The
//! signature is the single proof that travels forward to the deploy boundary,
//! where Kyverno refuses to admit any image lacking it.

mod config;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Scope;
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

    let source_str = source.to_string_lossy().to_string();
    let results = run_checks(&tree, &source, &source_str, cli.image.as_deref());

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
}

/// Run every applicable adapter check (marker present) and every scanner in
/// scope, returning their results in execution order.
fn run_checks(
    tree: &config::Tree,
    source: &Path,
    source_str: &str,
    image: Option<&str>,
) -> Vec<StepResult> {
    let mut results: Vec<StepResult> = Vec::new();

    // --- Language adapters -------------------------------------------------
    let mut matched_any = false;
    for adapter in &tree.adapters {
        if !source.join(&adapter.marker).exists() {
            continue;
        }
        matched_any = true;
        let cfg = adapter.config_dir.to_string_lossy().to_string();
        println!(
            "\n=== stack: {} (marker: {}) ===",
            adapter.name, adapter.marker
        );
        let subst = Subst {
            source: Some(source_str),
            image: None,
            config: Some(&cfg),
        };
        for check in &adapter.checks {
            results.push(run(
                &format!("{}:{}", adapter.name, check.name),
                &check.cmd,
                source,
                subst,
                &check.env,
            ));
        }
    }
    if !matched_any {
        println!("grizzly-gate :: no language markers matched — running scanners only");
    }

    // --- Scanners ----------------------------------------------------------
    for scanner in &tree.scanners {
        let cfg = scanner.config_dir.to_string_lossy().to_string();
        let subst = Subst {
            source: Some(source_str),
            image,
            config: Some(&cfg),
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

    results
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
