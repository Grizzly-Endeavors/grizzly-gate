use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Filename of the Ops-owned language-detection ruleset at the config root.
const DETECT: &str = "detect.toml";

/// The full gate rule set, loaded from a config *tree* rather than a single
/// file. Ops owns the tree: each tool carries its own `manifest.toml` (what to
/// run) next to the native config file(s) the gate forces it to use. Updating
/// the rules means editing the tree and cutting a new gate tag, never touching
/// this binary.
pub struct Tree {
    pub adapters: Vec<LanguageAdapter>,
    pub scanners: Vec<Scanner>,
    /// Ops-owned detection ruleset (`detect.toml`): vendor/build dirs the
    /// honest-map walk skips, plus the denylist of code languages the gate has
    /// no adapter for (any evidence of which fails closed).
    pub detect: DetectRules,
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
    /// File extensions / shebang interpreters that count as mandatory evidence
    /// of this language for the honest-map verification. Present only for
    /// unambiguous *code* languages (rust/python/node); empty for opt-in,
    /// marker-only adapters (ansible/yaml) whose files are too ambiguous
    /// (`.yml` is data as often as `IaC`) to mandate by extension.
    pub detect: Detect,
    pub checks: Vec<Check>,
}

/// Extension/shebang evidence used to detect a language's presence in a tree.
/// Matching is case-insensitive on the extension (no leading dot).
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub shebangs: Vec<String>,
}

/// Ops-owned detection ruleset loaded from `<config-root>/detect.toml`.
pub struct DetectRules {
    /// Directory *names* never descended into during the honest-map walk
    /// (VCS, dependency, and build-artifact dirs). Vendored code is not
    /// first-party; the secret/SAST scanners still cover it.
    pub skip_dirs: Vec<String>,
    /// Code languages the gate cannot actually check. Any evidence of one in
    /// the tree fails the gate closed — a green gate must mean "fully checked".
    pub unsupported: Vec<UnsupportedLang>,
}

/// A code language with no gate adapter. Detected the same way as a supported
/// one, but its presence is a hard failure rather than a run trigger.
pub struct UnsupportedLang {
    pub name: String,
    pub detect: Detect,
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
    /// How to distill this check's captured output for surfacing. Default (no
    /// `[output]` / `[checks.output]` block) leaves the output untouched.
    #[serde(default)]
    pub output: OutputSpec,
}

/// Declarative, opt-in recipe for turning a tool's raw stdout/stderr into a
/// focused surface. Presentation-only: it never affects the gate verdict (which
/// is the process exit code). See [`crate::distill`].
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// Built-in structured parser id (e.g. `clippy`, `cargo-deny`). When set, the
    /// harness parses the tool's JSON on stdout into normalized findings; an
    /// unparseable result falls back to raw text (never a silent "0 findings").
    #[serde(default)]
    pub parser: Option<String>,
    /// Regexes; any captured line matching one is dropped from the distilled
    /// text (tool progress/noise). Applied on the text path (no `parser`).
    #[serde(default)]
    pub drop: Vec<String>,
    /// Strip ANSI escape sequences from the distilled text. Set only for tools
    /// that emit color even to a non-TTY.
    #[serde(default)]
    pub strip_ansi: bool,
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
    /// How to distill this scanner's captured output for surfacing (see [`Check`]).
    pub output: OutputSpec,
}

const MANIFEST: &str = "manifest.toml";

/// Deserialized shape of a `languages/<lang>/manifest.toml`.
#[derive(Debug, Deserialize)]
struct AdapterManifest {
    name: String,
    marker: String,
    #[serde(default)]
    detect: Detect,
    #[serde(default)]
    checks: Vec<Check>,
}

/// Deserialized shape of `detect.toml`.
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

/// Deserialized shape of a `util/<tool>/manifest.toml`.
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
    let detect = load_detect(&root.join(DETECT))?;
    Ok(Tree {
        adapters,
        scanners,
        detect,
    })
}

/// Load the Ops-owned detection ruleset. Required: a missing or unparseable
/// `detect.toml` is fatal, so a stripped-down config dir can never silently
/// disable undeclared-language detection.
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
}
