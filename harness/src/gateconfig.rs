//! The per-repo `gate-config.json` — the honest map a scanned repo must ship.
//!
//! This file is the repo's *declaration* of its own project layout: which
//! languages live where. It can only ever declare (it cannot relax a single
//! check — the gate forces its own tool config regardless). Its honesty is
//! verified independently by [`crate::detect`], which walks the tree and fails
//! closed if reality contains a language/project the declaration omits. A
//! missing or malformed declaration is itself a fail-closed condition.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::config::Tree;

/// Required filename at the repo root.
pub const FILE: &str = "gate-config.json";

/// Schema version this harness understands. Bumped only on a breaking change to
/// the declaration shape; an unknown version fails closed rather than guessing.
pub const SUPPORTED_VERSION: u32 = 1;

/// Raw, deserialized `gate-config.json`. `deny_unknown_fields` so a typo'd or
/// speculative key (e.g. a hoped-for `exclude`) is a hard error, never a
/// silently-ignored escape hatch.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: u32,
    projects: Vec<RawProject>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    language: String,
    path: String,
    /// node-only: the repo's own tsconfig, used for module/path *resolution*
    /// while the gate force-overrides strictness. Rejected for any other
    /// language.
    #[serde(default)]
    tsconfig: Option<String>,
}

/// A validated, resolved project: language is a known adapter, the path is
/// in-tree and carries the adapter's marker, and any tsconfig exists.
pub struct ResolvedProject {
    pub language: String,
    /// Normalized path relative to the source root (`""` for the root itself).
    pub rel_path: PathBuf,
    /// Absolute path to the project directory.
    pub abs_path: PathBuf,
    /// Absolute path to the repo tsconfig (node + `tsconfig` set only).
    pub tsconfig: Option<PathBuf>,
}

/// Load, parse, and fully validate the declaration against `tree` and the
/// on-disk `source`. Every failure here is fatal by design (fail closed).
pub fn load(source: &Path, tree: &Tree) -> Result<Vec<ResolvedProject>> {
    let path = source.join(FILE);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "required {FILE} not found at repo root ({}) — every gated repo must \
             ship an honest project map; refusing to pass (fail closed)",
            path.display()
        )
    })?;

    let raw: Raw =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    if raw.version != SUPPORTED_VERSION {
        bail!(
            "{FILE} version {} unsupported (this gate understands version {SUPPORTED_VERSION})",
            raw.version
        );
    }
    if raw.projects.is_empty() {
        bail!("{FILE} declares zero projects — refusing to pass (fail closed)");
    }

    let mut resolved = Vec::with_capacity(raw.projects.len());
    for (i, p) in raw.projects.into_iter().enumerate() {
        resolved.push(resolve(i, p, source, tree)?);
    }
    Ok(resolved)
}

fn resolve(idx: usize, p: RawProject, source: &Path, tree: &Tree) -> Result<ResolvedProject> {
    let where_ = format!(
        "{FILE} projects[{idx}] (language={:?}, path={:?})",
        p.language, p.path
    );

    // Language must be a known adapter. Unknown names (including the denylisted
    // unsupported languages) cannot be declared — they have no checks to run.
    let adapter = tree
        .adapters
        .iter()
        .find(|a| a.name == p.language)
        .with_context(|| {
            format!(
                "{where_}: unknown language — no gate adapter exists for {:?}",
                p.language
            )
        })?;

    let rel = normalize_rel(&p.path).with_context(|| format!("{where_}: invalid path"))?;
    let abs = source.join(&rel);

    // The resolved directory must actually be inside the source tree (defends
    // against symlink/`.` escapes that `normalize_rel` can't see) and be a dir.
    let abs = abs
        .canonicalize()
        .with_context(|| format!("{where_}: path does not resolve on disk"))?;
    let source_canon = source
        .canonicalize()
        .with_context(|| format!("resolving source root {}", source.display()))?;
    if !abs.starts_with(&source_canon) {
        bail!("{where_}: path escapes the repo root");
    }
    if !abs.is_dir() {
        bail!("{where_}: path is not a directory");
    }

    // The adapter's marker must be present — a declared project the gate can't
    // actually run is a lie of omission (e.g. "rust at ./svc" with no Cargo.toml).
    let marker = abs.join(&adapter.marker);
    if !marker.exists() {
        bail!(
            "{where_}: declared {} project has no {} marker at {}",
            adapter.name,
            adapter.marker,
            abs.display()
        );
    }

    // tsconfig is node-only and, when given, must exist inside the project.
    let tsconfig = match p.tsconfig {
        None => None,
        Some(_) if p.language != "node" => {
            bail!("{where_}: `tsconfig` is only valid for node projects")
        }
        Some(rel_ts) => {
            let ts_rel = normalize_rel(&rel_ts)
                .with_context(|| format!("{where_}: invalid tsconfig path"))?;
            let ts_abs = abs
                .join(&ts_rel)
                .canonicalize()
                .with_context(|| format!("{where_}: tsconfig does not resolve on disk"))?;
            if !ts_abs.starts_with(&source_canon) {
                bail!("{where_}: tsconfig path escapes the repo root");
            }
            if !ts_abs.is_file() {
                bail!("{where_}: tsconfig is not a file");
            }
            Some(ts_abs)
        }
    };

    Ok(ResolvedProject {
        language: p.language,
        rel_path: rel,
        abs_path: abs,
        tsconfig,
    })
}

/// Normalize a declared relative path: reject absolute paths and any `..` or
/// root component, collapse `.`. Returns `""` for the repo root. This is the
/// first line against path-escape evasions (canonicalization in `resolve` is
/// the second).
fn normalize_rel(raw: &str) -> Result<PathBuf> {
    let p = Path::new(raw);
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => bail!("`..` is not allowed ({raw:?})"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths are not allowed ({raw:?})")
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rejects_escape_and_absolute() {
        assert!(normalize_rel("../etc").is_err());
        assert!(normalize_rel("a/../../b").is_err());
        assert!(normalize_rel("/etc/passwd").is_err());
        assert_eq!(normalize_rel(".").unwrap(), PathBuf::new());
        assert_eq!(normalize_rel("./web").unwrap(), PathBuf::from("web"));
        assert_eq!(normalize_rel("a/b").unwrap(), PathBuf::from("a/b"));
    }
}
