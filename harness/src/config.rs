use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const DETECT: &str = "detect.toml";

pub struct Tree {
    pub adapters: Vec<LanguageAdapter>,
    pub scanners: Vec<Scanner>,
    pub detect: DetectRules,
}

pub struct LanguageAdapter {
    pub name: String,
    pub marker: String,
    pub config_dir: PathBuf,
    pub detect: Detect,
    pub checks: Vec<Check>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub shebangs: Vec<String>,
}

pub struct DetectRules {
    pub skip_dirs: Vec<String>,
    pub unsupported: Vec<UnsupportedLang>,
}

pub struct UnsupportedLang {
    pub name: String,
    pub detect: Detect,
}

#[derive(Debug, Deserialize)]
pub struct Check {
    pub name: String,
    pub cmd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub output: OutputSpec,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    #[serde(default)]
    pub parser: Option<String>,
    #[serde(default)]
    pub drop: Vec<String>,
    #[serde(default)]
    pub strip_ansi: bool,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Source,
    Image,
}

pub struct Scanner {
    pub name: String,
    pub scope: Scope,
    pub config_dir: PathBuf,
    pub cmd: String,
    pub env: BTreeMap<String, String>,
    pub output: OutputSpec,
}

const MANIFEST: &str = "manifest.toml";

#[derive(Debug, Deserialize)]
struct AdapterManifest {
    name: String,
    marker: String,
    #[serde(default)]
    detect: Detect,
    #[serde(default)]
    checks: Vec<Check>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DetectManifest {
    #[serde(default)]
    skip_dirs: Vec<String>,
    #[serde(default)]
    unsupported: Vec<UnsupportedManifest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnsupportedManifest {
    name: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    shebangs: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ScannerManifest {
    name: String,
    scope: Scope,
    cmd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    output: OutputSpec,
}

pub fn load_tree(root: &Path) -> Result<Tree> {
    let adapters = load_adapters(&root.join("languages"))?;
    let scanners = load_scanners(&root.join("util"))?;
    if adapters.is_empty() && scanners.is_empty() {
        bail!(
            "no gate manifests found under {} — refusing to pass (fail closed)",
            root.display()
        );
    }
    let detect = load_detect(&root.join(DETECT))?;
    Ok(Tree {
        adapters,
        scanners,
        detect,
    })
}

fn load_detect(path: &Path) -> Result<DetectRules> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} (required for detection)", path.display()))?;
    let m: DetectManifest =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let unsupported = m
        .unsupported
        .into_iter()
        .map(|u| UnsupportedLang {
            name: u.name,
            detect: Detect {
                extensions: u.extensions,
                shebangs: u.shebangs,
            },
        })
        .collect();
    Ok(DetectRules {
        skip_dirs: m.skip_dirs,
        unsupported,
    })
}

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
            detect: m.detect,
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
            output: m.output,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
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
            "name = \"rust\"\nmarker = \"Cargo.toml\"\n\n[detect]\nextensions = [\"rs\"]\n\n[[checks]]\nname = \"fmt\"\ncmd = \"cargo fmt --check --config-path {config}/rustfmt.toml\"\n",
        )
        .unwrap();
        std::fs::write(
            util.join(MANIFEST),
            "name = \"gitleaks\"\nscope = \"source\"\ncmd = \"gitleaks detect --source {source}\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join(DETECT),
            "skip_dirs = [\"target\"]\n\n[[unsupported]]\nname = \"go\"\nextensions = [\"go\"]\n",
        )
        .unwrap();

        let tree = load_tree(&root).unwrap();
        assert_eq!(tree.adapters.len(), 1);
        let adapter = tree.adapters.first().unwrap();
        assert_eq!(adapter.name, "rust");
        assert_eq!(adapter.detect.extensions, ["rs"]);
        assert_eq!(adapter.checks.len(), 1);
        assert!(adapter.config_dir.ends_with("languages/rust"));
        assert_eq!(tree.scanners.len(), 1);
        assert_eq!(tree.scanners.first().unwrap().scope, Scope::Source);
        assert_eq!(tree.detect.skip_dirs, ["target"]);
        assert_eq!(tree.detect.unsupported.len(), 1);
        assert_eq!(tree.detect.unsupported.first().unwrap().name, "go");

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

    fn real_config_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../config")
    }

    #[test]
    fn ruff_and_mypy_forward_skip_dirs() {
        let tree = load_tree(&real_config_root()).expect("real config tree must load");
        let python = tree
            .adapters
            .iter()
            .find(|a| a.name == "python")
            .expect("python adapter must exist");

        let ruff = python
            .checks
            .iter()
            .find(|c| c.name == "ruff")
            .expect("ruff check must exist");
        assert!(
            ruff.cmd.contains("{skip_dirs}"),
            "ruff must forward the Ops-owned skip_dirs: {:?}",
            ruff.cmd
        );
        assert!(
            ruff.cmd.contains("--extend-exclude"),
            "ruff must use --extend-exclude, not --exclude (which REPLACES \
             ruff's own default excludes instead of adding to them): {:?}",
            ruff.cmd
        );

        let mypy = python
            .checks
            .iter()
            .find(|c| c.name == "mypy")
            .expect("mypy check must exist");
        assert_eq!(
            mypy.env.get("GATE_SKIP_DIRS").map(String::as_str),
            Some("{skip_dirs}"),
            "mypy must receive the Ops-owned skip_dirs via GATE_SKIP_DIRS: {:?}",
            mypy.env
        );
        assert!(
            mypy.cmd.contains("--exclude") && mypy.cmd.contains("$GATE_SKIP_DIRS"),
            "mypy must forward an --exclude regex built from GATE_SKIP_DIRS: {:?}",
            mypy.cmd
        );
    }

    #[test]
    fn golangci_lint_config_excludes_skip_dirs() {
        // golangci-lint v2 has no CLI flag or config-file env-var expansion to
        // receive the harness's {skip_dirs} token (verified against v2.12.2 —
        // the v1 `issues.exclude-dirs`/`run.skip-dirs` keys were removed and
        // never replaced with one), so `.golangci.yml` carries a hand-maintained
        // mirror of detect.toml's skip_dirs instead (see ADR-039). This test
        // guards that mirror doesn't silently drift: every skip_dirs name must
        // appear under BOTH `linters.exclusions.paths` (golangci-lint run) and
        // `formatters.exclusions.paths` (golangci-lint fmt), since they are
        // separate config sections in v2 and each is read independently.
        let detect_text = std::fs::read_to_string(real_config_root().join(DETECT))
            .expect("detect.toml must exist");
        let detect: DetectManifest = toml::from_str(&detect_text).expect("detect.toml must parse");

        let golangci_text =
            std::fs::read_to_string(real_config_root().join("languages/go/.golangci.yml"))
                .expect(".golangci.yml must exist");
        let (linters_part, formatters_part) = golangci_text
            .split_once("\nformatters:")
            .expect(".golangci.yml must have a top-level `formatters:` section");

        for dir in &detect.skip_dirs {
            assert!(
                linters_part.contains(dir.as_str()),
                "skip_dirs entry {dir:?} missing from linters.exclusions.paths"
            );
            assert!(
                formatters_part.contains(dir.as_str()),
                "skip_dirs entry {dir:?} missing from formatters.exclusions.paths"
            );
        }
    }
}
