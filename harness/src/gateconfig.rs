use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::config::Tree;
use crate::report::{HonestMapFailure, Violation, ViolationClass};

pub const FILE: &str = "gate-config.json";

pub const SUPPORTED_VERSION: u32 = 1;

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
    #[serde(default)]
    tsconfig: Option<String>,
}

#[derive(Debug)]
pub struct ResolvedProject {
    pub language: String,
    pub rel_path: PathBuf,
    pub abs_path: PathBuf,
    pub tsconfig: Option<PathBuf>,
}

pub fn load(source: &Path, tree: &Tree) -> Result<Vec<ResolvedProject>, HonestMapFailure> {
    let path = source.join(FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        HonestMapFailure::whole(format!(
            "required {FILE} not found at repo root ({}) — every gated repo must \
             ship an honest project map; refusing to pass (fail closed): {e}",
            path.display()
        ))
    })?;

    let raw: Raw = serde_json::from_str(&text)
        .map_err(|e| HonestMapFailure::whole(format!("parsing {}: {e}", path.display())))?;

    if raw.version != SUPPORTED_VERSION {
        return Err(HonestMapFailure::whole(format!(
            "{FILE} version {} unsupported (this gate understands version {SUPPORTED_VERSION})",
            raw.version
        )));
    }
    if raw.projects.is_empty() {
        return Err(HonestMapFailure::whole(format!(
            "{FILE} declares zero projects — refusing to pass (fail closed)"
        )));
    }

    let mut resolved = Vec::with_capacity(raw.projects.len());
    let mut violations: Vec<Violation> = Vec::new();
    for (i, p) in raw.projects.into_iter().enumerate() {
        match resolve(i, p, source, tree) {
            Ok(rp) => resolved.push(rp),
            Err(v) => violations.push(v),
        }
    }

    if !violations.is_empty() {
        let mut lines = vec!["gate-config.json is not valid (fail closed):".to_string()];
        for v in &violations {
            lines.push(format!("  - {}", v.reason));
        }
        return Err(HonestMapFailure {
            message: lines.join("\n"),
            violations,
        });
    }
    Ok(resolved)
}

fn resolve(
    idx: usize,
    p: RawProject,
    source: &Path,
    tree: &Tree,
) -> std::result::Result<ResolvedProject, Violation> {
    let where_ = format!(
        "{FILE} projects[{idx}] (language={:?}, path={:?})",
        p.language, p.path
    );
    let lang = p.language.clone();
    let decl_path = p.path.clone();
    let v = |reason: String| Violation {
        class: ViolationClass::MalformedDeclaration,
        language: Some(lang.clone()),
        path: Some(decl_path.clone()),
        reason,
    };

    let adapter = tree
        .adapters
        .iter()
        .find(|a| a.name == p.language)
        .ok_or_else(|| {
            v(format!(
                "{where_}: unknown language — no gate adapter exists for {:?}",
                p.language
            ))
        })?;

    let rel = normalize_rel(&p.path).map_err(|e| v(format!("{where_}: invalid path: {e}")))?;
    let abs = source.join(&rel);

    let abs = abs
        .canonicalize()
        .map_err(|e| v(format!("{where_}: path does not resolve on disk: {e}")))?;
    let source_canon = source
        .canonicalize()
        .map_err(|e| v(format!("{where_}: resolving source root: {e}")))?;
    if !abs.starts_with(&source_canon) {
        return Err(v(format!("{where_}: path escapes the repo root")));
    }
    if !abs.is_dir() {
        return Err(v(format!("{where_}: path is not a directory")));
    }

    let marker = abs.join(&adapter.marker);
    if !marker.exists() {
        return Err(v(format!(
            "{where_}: declared {} project has no {} marker at {}",
            adapter.name,
            adapter.marker,
            abs.display()
        )));
    }

    let tsconfig = match p.tsconfig {
        None => None,
        Some(_) if p.language != "node" => {
            return Err(v(format!(
                "{where_}: `tsconfig` is only valid for node projects"
            )))
        }
        Some(rel_ts) => {
            let ts_rel = normalize_rel(&rel_ts)
                .map_err(|e| v(format!("{where_}: invalid tsconfig path: {e}")))?;
            let ts_abs = abs
                .join(&ts_rel)
                .canonicalize()
                .map_err(|e| v(format!("{where_}: tsconfig does not resolve on disk: {e}")))?;
            if !ts_abs.starts_with(&source_canon) {
                return Err(v(format!("{where_}: tsconfig path escapes the repo root")));
            }
            if !ts_abs.is_file() {
                return Err(v(format!("{where_}: tsconfig is not a file")));
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
    use crate::config::{Detect, DetectRules, LanguageAdapter, Tree};

    #[test]
    fn normalize_rejects_escape_and_absolute() {
        assert!(normalize_rel("../etc").is_err());
        assert!(normalize_rel("a/../../b").is_err());
        assert!(normalize_rel("/etc/passwd").is_err());
        assert_eq!(normalize_rel(".").unwrap(), PathBuf::new());
        assert_eq!(normalize_rel("./web").unwrap(), PathBuf::from("web"));
        assert_eq!(normalize_rel("a/b").unwrap(), PathBuf::from("a/b"));
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gate-gateconfig-{tag}-{}", std::process::id()))
    }

    fn tree() -> Tree {
        Tree {
            adapters: vec![LanguageAdapter {
                name: "rust".into(),
                marker: "Cargo.toml".into(),
                config_dir: PathBuf::from("/x"),
                detect: Detect {
                    extensions: vec!["rs".into()],
                    shebangs: vec![],
                },
                checks: vec![],
            }],
            scanners: vec![],
            detect: DetectRules {
                skip_dirs: vec![],
                unsupported: vec![],
            },
        }
    }

    #[test]
    fn reports_every_bad_project_in_one_pass() {
        let root = scratch("multi-bad");
        std::fs::create_dir_all(root.join("svc")).unwrap();
        std::fs::write(
            root.join(FILE),
            r#"{"version":1,"projects":[
                {"language":"cobol","path":"."},
                {"language":"rust","path":"svc"}
            ]}"#,
        )
        .unwrap();

        let failure = load(&root, &tree()).unwrap_err();
        assert_eq!(failure.violations.len(), 2, "{}", failure.message);
        assert!(failure.message.contains("cobol"), "{}", failure.message);
        assert!(
            failure.message.contains("Cargo.toml") || failure.message.contains("marker"),
            "{}",
            failure.message
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_bad_project_among_good_still_fails_and_names_it() {
        let root = scratch("mixed");
        std::fs::create_dir_all(root.join("ok")).unwrap();
        std::fs::write(root.join("ok/Cargo.toml"), "[package]").unwrap();
        std::fs::write(
            root.join(FILE),
            r#"{"version":1,"projects":[
                {"language":"rust","path":"ok"},
                {"language":"rust","path":"missing"}
            ]}"#,
        )
        .unwrap();

        let failure = load(&root, &tree()).unwrap_err();
        assert_eq!(failure.violations.len(), 1, "{}", failure.message);
        assert!(failure.message.contains("missing"), "{}", failure.message);
        std::fs::remove_dir_all(&root).ok();
    }
}
