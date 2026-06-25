use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The full gate rule set, loaded from a config *tree* rather than a single
/// file. Ops owns the tree: each tool carries its own `manifest.toml` (what to
/// run) next to the native config file(s) the gate forces it to use. Updating
/// the rules means editing the tree and cutting a new gate tag, never touching
/// this binary.
pub struct Tree {
    pub adapters: Vec<LanguageAdapter>,
    pub scanners: Vec<Scanner>,
}

/// A per-language adapter (`config/languages/<lang>/manifest.toml`). When
/// `marker` exists at the source root, every command in `checks` runs in the
/// source directory with the gate's config injected via `{config}`.
pub struct LanguageAdapter {
    pub name: String,
    /// File or directory (relative to the source root) whose presence activates
    /// this stack, e.g. `Cargo.toml`, `pyproject.toml`, `ansible`.
    pub marker: String,
    /// Absolute path to this tool's config dir, injected by the loader (not
    /// deserialized) and substituted for `{config}` in commands/env.
    pub config_dir: PathBuf,
    pub checks: Vec<Check>,
}

/// One command within an adapter. `cmd` and `env` values may contain
/// `{config}`/`{source}`/`{image}` placeholders, substituted at run time.
#[derive(Debug, Deserialize)]
pub struct Check {
    pub name: String,
    pub cmd: String,
    /// Env vars for tools that take config via the environment rather than a
    /// flag (e.g. clippy's `CLIPPY_CONF_DIR`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// What a scanner runs against. Source scanners always run; image scanners run
/// only when the harness is given a built image to inspect.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Source,
    Image,
}

/// A repo-wide or image-wide scanner (`config/util/<tool>/manifest.toml`).
pub struct Scanner {
    pub name: String,
    pub scope: Scope,
    pub config_dir: PathBuf,
    pub cmd: String,
    pub env: BTreeMap<String, String>,
}

const MANIFEST: &str = "manifest.toml";

/// Deserialized shape of a `languages/<lang>/manifest.toml`.
#[derive(Debug, Deserialize)]
struct AdapterManifest {
    name: String,
    marker: String,
    #[serde(default)]
    checks: Vec<Check>,
}

/// Deserialized shape of a `util/<tool>/manifest.toml`.
#[derive(Debug, Deserialize)]
struct ScannerManifest {
    name: String,
    scope: Scope,
    cmd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Load the gate rule set from a config root: `<root>/languages/*/manifest.toml`
/// → adapters, `<root>/util/*/manifest.toml` → scanners. Fails closed if the
/// root yields zero manifests, so a missing/empty tree can never silently pass.
pub fn load_tree(root: &Path) -> Result<Tree> {
    let adapters = load_adapters(&root.join("languages"))?;
    let scanners = load_scanners(&root.join("util"))?;
    if adapters.is_empty() && scanners.is_empty() {
        bail!(
            "no gate manifests found under {} — refusing to pass (fail closed)",
            root.display()
        );
    }
    Ok(Tree { adapters, scanners })
}

/// Tool dirs (those containing a `manifest.toml`) under a category, sorted by
/// path for deterministic check ordering. A missing category is empty, not an
/// error — a tree may legitimately ship only `languages/` or only `util/`.
fn tool_dirs(category: &Path) -> Result<Vec<PathBuf>> {
    if !category.exists() {
        return Ok(Vec::new());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(category)
        .with_context(|| format!("reading {}", category.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(MANIFEST).is_file())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn load_adapters(dir: &Path) -> Result<Vec<LanguageAdapter>> {
    let mut out = Vec::new();
    for d in tool_dirs(dir)? {
        let manifest = d.join(MANIFEST);
        let text = std::fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?;
        let m: AdapterManifest =
            toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
        let config_dir = d
            .canonicalize()
            .with_context(|| format!("resolving {}", d.display()))?;
        out.push(LanguageAdapter {
            name: m.name,
            marker: m.marker,
            config_dir,
            checks: m.checks,
        });
    }
    Ok(out)
}

fn load_scanners(dir: &Path) -> Result<Vec<Scanner>> {
    let mut out = Vec::new();
    for d in tool_dirs(dir)? {
        let manifest = d.join(MANIFEST);
        let text = std::fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?;
        let m: ScannerManifest =
            toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
        let config_dir = d
            .canonicalize()
            .with_context(|| format!("resolving {}", d.display()))?;
        out.push(Scanner {
            name: m.name,
            scope: m.scope,
            config_dir,
            cmd: m.cmd,
            env: m.env,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        // Ephemeral, uniquely-named test scratch dir (removed at end of each
        // test); not a security-sensitive temp file, so the predictable-name /
        // shared-tmp concern the rule targets doesn't apply. The nosemgrep token
        // must sit on the line immediately above the finding to take effect.
        // nosemgrep: temp-dir
        std::env::temp_dir().join(format!("gate-{tag}-{}", std::process::id()))
    }

    #[test]
    fn loads_tree_and_injects_config_dir() {
        let root = scratch("tree");
        let lang = root.join("languages/rust");
        let util = root.join("util/gitleaks");
        std::fs::create_dir_all(&lang).unwrap();
        std::fs::create_dir_all(&util).unwrap();
        std::fs::write(
            lang.join(MANIFEST),
            "name = \"rust\"\nmarker = \"Cargo.toml\"\n\n[[checks]]\nname = \"fmt\"\ncmd = \"cargo fmt --check --config-path {config}/rustfmt.toml\"\n",
        )
        .unwrap();
        std::fs::write(
            util.join(MANIFEST),
            "name = \"gitleaks\"\nscope = \"source\"\ncmd = \"gitleaks detect --source {source}\"\n",
        )
        .unwrap();

        let tree = load_tree(&root).unwrap();
        assert_eq!(tree.adapters.len(), 1);
        let adapter = tree.adapters.first().unwrap();
        assert_eq!(adapter.name, "rust");
        assert_eq!(adapter.checks.len(), 1);
        assert!(adapter.config_dir.ends_with("languages/rust"));
        assert_eq!(tree.scanners.len(), 1);
        assert_eq!(tree.scanners.first().unwrap().scope, Scope::Source);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_root_fails_closed() {
        let root = scratch("empty");
        std::fs::create_dir_all(&root).unwrap();
        assert!(load_tree(&root).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_root_fails_closed() {
        let root = scratch("missing");
        std::fs::remove_dir_all(&root).ok();
        assert!(load_tree(&root).is_err());
    }
}
