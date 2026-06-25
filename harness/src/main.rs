//! grizzly-gate — the orchestration harness for the grizzly-platform CI gate.
//!
//! One artifact, centrally owned by Ops: it detects the stacks in a repo, runs
//! the pinned per-language adapters + scanners defined in `gate.toml`, and — only
//! if everything passes — signs the built image with cosign. The signature is the
//! single proof that travels forward to the deploy boundary, where Kyverno refuses
//! to admit any image lacking it.

mod config;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::{Config, Scope};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Rule set baked into the image; overridable per-invocation with `--config`.
const DEFAULT_CONFIG: &str = include_str!("../../gate.toml");

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

    /// Path to a gate.toml; falls back to the config baked into the image.
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

    let config_text = match &cli.config {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading gate config {}", path.display()))?,
        None => DEFAULT_CONFIG.to_string(),
    };
    let config = Config::parse(&config_text).context("parsing gate config")?;

    let source = cli
        .source
        .canonicalize()
        .with_context(|| format!("resolving source path {}", cli.source.display()))?;

    println!("grizzly-gate :: gating {}", source.display());
    if let Some(image) = &cli.image {
        println!("grizzly-gate :: image {image}");
    }

    let mut results: Vec<StepResult> = Vec::new();
    let source_str = source.to_string_lossy().to_string();

    // --- Language adapters -------------------------------------------------
    let mut matched_any = false;
    for detector in &config.detectors {
        if !source.join(&detector.marker).exists() {
            continue;
        }
        matched_any = true;
        println!(
            "\n=== stack: {} (marker: {}) ===",
            detector.name, detector.marker
        );
        for check in &detector.checks {
            results.push(run(
                &format!("{}:{}", detector.name, short(check)),
                check,
                &source,
                Some(&source_str),
                None,
            ));
        }
    }
    if !matched_any {
        println!("grizzly-gate :: no language markers matched — running scanners only");
    }

    // --- Scanners ----------------------------------------------------------
    for scanner in &config.scanners {
        match scanner.scope {
            Scope::Source => {
                results.push(run(
                    &format!("scan:{}", scanner.name),
                    &scanner.cmd,
                    &source,
                    Some(&source_str),
                    cli.image.as_deref(),
                ));
            }
            Scope::Image => match &cli.image {
                Some(image) => results.push(run(
                    &format!("scan:{}", scanner.name),
                    &scanner.cmd,
                    &source,
                    Some(&source_str),
                    Some(image),
                )),
                None => println!(
                    "grizzly-gate :: skipping image scanner '{}' (no --image given)",
                    scanner.name
                ),
            },
        }
    }

    // --- Verdict -----------------------------------------------------------
    println!("\n────────────────────────── gate summary ──────────────────────────");
    let mut failed = 0usize;
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

/// Run one command line in `cwd`, substituting `{source}`/`{image}` placeholders.
fn run(
    label: &str,
    cmdline: &str,
    cwd: &Path,
    source: Option<&str>,
    image: Option<&str>,
) -> StepResult {
    let mut rendered = cmdline.to_string();
    if let Some(s) = source {
        rendered = rendered.replace("{source}", s);
    }
    if let Some(i) = image {
        rendered = rendered.replace("{image}", i);
    }
    println!("\n── {label}\n   $ {rendered}");

    let parts = match shlex::split(&rendered) {
        Some(p) if !p.is_empty() => p,
        _ => {
            eprintln!("   ! could not parse command: {rendered}");
            return StepResult {
                label: label.into(),
                ok: false,
                secs: 0.0,
            };
        }
    };

    let start = Instant::now();
    let status = Command::new(&parts[0])
        .args(&parts[1..])
        .current_dir(cwd)
        .status();
    let secs = start.elapsed().as_secs_f64();

    let ok = match status {
        Ok(s) => s.success(),
        Err(e) => {
            eprintln!("   ! failed to spawn {}: {e}", parts[0]);
            false
        }
    };
    StepResult {
        label: label.into(),
        ok,
        secs,
    }
}

/// cosign sign by digest. COSIGN_PASSWORD is inherited from the environment
/// (delivered to the runner by ESO from OpenBao).
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

/// First two tokens of a command, for compact but distinct summary labels
/// (`cargo fmt` vs `cargo clippy` rather than just `cargo`).
fn short(cmd: &str) -> String {
    cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}
