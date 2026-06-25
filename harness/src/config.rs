use serde::Deserialize;

/// The declarative gate rule set. Adapters are data the harness executes, not
/// hardcoded logic — updating the checks means editing `gate.toml` and cutting
/// a new gate image tag, never touching this binary.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub detectors: Vec<Detector>,
    #[serde(default)]
    pub scanners: Vec<Scanner>,
}

/// A per-language adapter. When `marker` exists at the source root, every
/// command in `checks` runs in the source directory.
#[derive(Debug, Deserialize)]
pub struct Detector {
    pub name: String,
    /// File or directory (relative to the source root) whose presence
    /// activates this stack, e.g. `Cargo.toml`, `pyproject.toml`, `ansible`.
    pub marker: String,
    pub checks: Vec<String>,
}

/// What a scanner runs against. Source scanners always run; image scanners run
/// only when the harness is given a built image to inspect.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Source,
    Image,
}

/// A repo-wide or image-wide scanner. `cmd` may contain `{source}` and
/// `{image}` placeholders, substituted at run time.
#[derive(Debug, Deserialize)]
pub struct Scanner {
    pub name: String,
    pub scope: Scope,
    pub cmd: String,
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        toml::from_str(text).map_err(Into::into)
    }
}
