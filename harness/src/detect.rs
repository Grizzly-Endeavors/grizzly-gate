//! Honest-map verification: the trusted half of the `gate-config.json` contract.
//!
//! [`crate::gateconfig`] parses what the repo *claims*; this module walks the
//! tree and establishes what is *actually* there, then fails closed on any
//! mismatch. It is deliberately implemented in the harness (not delegated to a
//! repo-influenceable tool) and is hostile by construction:
//!
//! - it does **not** honor the repo's `.gitignore` (a repo cannot ignore its
//!   way out of detection — only an Ops-owned `skip_dirs` list is skipped);
//! - it does **not** follow symlinks (no escaping the tree, no loops);
//! - extension matching is case-insensitive (`.RS` == `.rs`);
//! - extensionless executables are classified by shebang interpreter.
//!
//! Three failure classes: *undeclared* adapter-backed code (a `.rs` not covered
//! by any declared rust project), *unsupported* code (a language with no adapter
//! at all — e.g. Go), and a node project that contains TypeScript but declares no
//! `tsconfig` (type-aware linting needs the TS program). Any one is fatal.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::config::Tree;
use crate::gateconfig::ResolvedProject;
use crate::report::{HonestMapFailure, Violation, ViolationClass};

/// How a single file was classified by the detection ruleset.
enum Hit {
    /// Belongs to a language with an adapter — must be covered by a declaration.
    Adapter(String),
    /// Belongs to a code language the gate cannot check — always fatal.
    Unsupported(String),
}

/// Walk `source`, classify every file, and fail closed on undeclared or
/// unsupported code. `projects` is the already-validated declaration. On
/// failure, returns a [`HonestMapFailure`] carrying both the rich human message
/// and the structured violations for `report.json`.
pub fn verify(
    source: &Path,
    tree: &Tree,
    projects: &[ResolvedProject],
) -> std::result::Result<(), HonestMapFailure> {
    // Pre-index extension/shebang → language for O(1) lookups. A later rule
    // never shadows an earlier one; adapters are indexed before the unsupported
    // denylist so a supported language can't be mis-flagged.
    let mut ext: HashMap<String, Hit> = HashMap::new();
    let mut shebang: HashMap<String, Hit> = HashMap::new();
    for a in &tree.adapters {
        for e in &a.detect.extensions {
            ext.entry(e.to_ascii_lowercase())
                .or_insert_with(|| Hit::Adapter(a.name.clone()));
        }
        for s in &a.detect.shebangs {
            shebang
                .entry(s.clone())
                .or_insert_with(|| Hit::Adapter(a.name.clone()));
        }
    }
    for u in &tree.detect.unsupported {
        for e in &u.detect.extensions {
            ext.entry(e.to_ascii_lowercase())
                .or_insert_with(|| Hit::Unsupported(u.name.clone()));
        }
        for s in &u.detect.shebangs {
            shebang
                .entry(s.clone())
                .or_insert_with(|| Hit::Unsupported(u.name.clone()));
        }
    }

    let mut undeclared: Vec<(String, PathBuf)> = Vec::new();
    let mut unsupported: Vec<(String, PathBuf)> = Vec::new();
    // node projects shown to contain TypeScript but missing a `tsconfig`
    // declaration: type-aware linting needs the program, so this is fatal.
    let mut ts_without_config: std::collections::BTreeSet<PathBuf> =
        std::collections::BTreeSet::new();

    let walker = WalkDir::new(source).follow_links(false).into_iter();
    for entry in walker.filter_entry(|e| !is_skipped_dir(e, source, &tree.detect.skip_dirs)) {
        // A walk error means we cannot establish what's actually in the tree —
        // fail closed rather than pass on a partial view.
        let entry =
            entry.map_err(|e| HonestMapFailure::whole(format!("walking source tree: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = match path.strip_prefix(source) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        // Never treat the declaration itself as code.
        if rel == Path::new(crate::gateconfig::FILE) {
            continue;
        }

        let hit = classify(path, &ext, &shebang);
        match hit {
            Some(Hit::Adapter(lang)) => match covering(&rel, &lang, projects) {
                None => undeclared.push((lang, rel)),
                Some(p) => {
                    if lang == "node" && is_typescript(&rel) && p.tsconfig.is_none() {
                        ts_without_config.insert(p.rel_path.clone());
                    }
                }
            },
            Some(Hit::Unsupported(lang)) => unsupported.push((lang, rel)),
            None => {}
        }
    }

    if unsupported.is_empty() && undeclared.is_empty() && ts_without_config.is_empty() {
        return Ok(());
    }
    Err(build_failure(unsupported, undeclared, &ts_without_config))
}

/// Fold the three violation classes into one [`HonestMapFailure`]: the rich
/// human message *and* the structured violations, both built from the same
/// (deduplicated, sorted, capped) lists so they never disagree. Called only
/// when at least one list is non-empty.
fn build_failure(
    mut unsupported: Vec<(String, PathBuf)>,
    mut undeclared: Vec<(String, PathBuf)>,
    ts_without_config: &std::collections::BTreeSet<PathBuf>,
) -> HonestMapFailure {
    unsupported.sort();
    undeclared.sort();
    let unsupported = dedup_head(&unsupported);
    let undeclared = dedup_head(&undeclared);

    let mut violations: Vec<Violation> = Vec::new();
    let mut sections: Vec<String> = vec!["honest-map verification failed (fail closed):".into()];
    if !unsupported.is_empty() {
        sections.push(format!(
            "  Unsupported languages (gate has no adapter — cannot be gated):\n{}",
            render_violations(&unsupported)
        ));
        violations.extend(unsupported.iter().map(|(lang, p)| Violation {
            class: ViolationClass::Unsupported,
            language: Some(lang.clone()),
            path: Some(p.display().to_string()),
            reason: format!("{lang} code present but the gate has no adapter for it"),
        }));
    }
    if !undeclared.is_empty() {
        sections.push(format!(
            "  Undeclared code (present in tree but not mapped in {}):\n{}\n\n  \
             Declare each in {} (or remove the code).",
            crate::gateconfig::FILE,
            render_violations(&undeclared),
            crate::gateconfig::FILE,
        ));
        violations.extend(undeclared.iter().map(|(lang, p)| Violation {
            class: ViolationClass::Undeclared,
            language: Some(lang.clone()),
            path: Some(p.display().to_string()),
            reason: format!("{lang} code not covered by any declared project"),
        }));
    }
    if !ts_without_config.is_empty() {
        let lines = ts_without_config
            .iter()
            .map(|p| format!("    [node] {}", display_rel(p)))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "  TypeScript projects missing a tsconfig declaration (needed for \
             type-aware linting):\n{lines}\n\n  Add \"tsconfig\": \"<path>\" to each \
             in {}.",
            crate::gateconfig::FILE,
        ));
        violations.extend(ts_without_config.iter().map(|p| Violation {
            class: ViolationClass::TsWithoutTsconfig,
            language: Some("node".to_string()),
            path: Some(display_rel(p)),
            reason: "node project contains TypeScript but declares no tsconfig".to_string(),
        }));
    }
    HonestMapFailure {
        message: sections.join("\n\n"),
        violations,
    }
}

/// Render a project-relative path for display: `"."` for the repo root.
fn display_rel(p: &Path) -> String {
    if p.as_os_str().is_empty() {
        ".".to_string()
    } else {
        p.display().to_string()
    }
}

/// TypeScript source extensions (a subset of the node adapter's extensions).
fn is_typescript(rel: &Path) -> bool {
    rel.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| matches!(e.as_str(), "ts" | "tsx" | "mts" | "cts"))
}

/// Render a sorted violation list to `    [lang] path` lines, capped via
/// [`dedup_head`].
fn render_violations(items: &[(String, PathBuf)]) -> String {
    dedup_head(items)
        .iter()
        .map(|(lang, p)| format!("    [{lang}] {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a directory entry is an Ops-owned skip dir (vendor/build/VCS). `.git`
/// is always skipped regardless of the configured list.
fn is_skipped_dir(entry: &walkdir::DirEntry, source: &Path, skip_dirs: &[String]) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    // Never skip the source root itself (its file_name may match a skip entry).
    if entry.path() == source {
        return false;
    }
    let Some(name) = entry.file_name().to_str() else {
        return false;
    };
    name == ".git" || skip_dirs.iter().any(|d| d == name)
}

/// Classify a file by extension first, then (only if it has none, or an
/// unrecognized one) by shebang. Returns `None` for benign non-code.
fn classify(
    path: &Path,
    ext_map: &HashMap<String, Hit>,
    shebang_map: &HashMap<String, Hit>,
) -> Option<Hit> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(hit) = ext_map.get(&ext.to_ascii_lowercase()) {
            return Some(clone_hit(hit));
        }
        // A recognized-but-irrelevant extension (e.g. `.md`): don't shebang-scan.
        return None;
    }
    // Extensionless file: a script masquerading without a suffix. Read the
    // shebang and map its interpreter.
    let interp = read_shebang_interpreter(path)?;
    shebang_map.get(&interp).map(clone_hit)
}

fn clone_hit(hit: &Hit) -> Hit {
    match hit {
        Hit::Adapter(s) => Hit::Adapter(s.clone()),
        Hit::Unsupported(s) => Hit::Unsupported(s.clone()),
    }
}

/// Read the interpreter basename from a `#!` line, handling the `env` form.
/// `#!/usr/bin/env python3` → `python3`; `#!/usr/bin/ruby` → `ruby`. Reads only
/// the first 256 bytes and never errors out the walk (unreadable → `None`).
fn read_shebang_interpreter(path: &Path) -> Option<String> {
    let mut buf = [0_u8; 256];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut buf).ok()?;
    let head = buf.get(..n)?;
    if !head.starts_with(b"#!") {
        return None;
    }
    let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    // `2` (past `#!`) ≤ `line_end` ≤ `head.len()`, so this slice is in-bounds;
    // `get` keeps it panic-free regardless.
    let line = std::str::from_utf8(head.get(2..line_end)?).ok()?;
    let mut toks = line.split_whitespace();
    let first = toks.next()?;
    let first_base = basename(first);
    // `env` defers to the next token as the real interpreter.
    let interp = if first_base == "env" {
        basename(toks.next()?)
    } else {
        first_base
    };
    Some(interp.to_string())
}

fn basename(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}

/// The declared project covering `rel` for `lang`, if any: a project of the same
/// language whose path is an ancestor (a root project, `rel_path == ""`, covers
/// everything). Path comparison is component-wise, so `web` does not cover
/// `web2/`. When several match, the first declared wins (callers only need
/// existence and the project's tsconfig state).
fn covering<'a>(
    rel: &Path,
    lang: &str,
    projects: &'a [ResolvedProject],
) -> Option<&'a ResolvedProject> {
    projects.iter().find(|p| {
        p.language == lang && (p.rel_path.as_os_str().is_empty() || rel.starts_with(&p.rel_path))
    })
}

/// Cap a sorted, deduplicated violation list so a pathological tree can't
/// produce a multi-thousand-line error. Reports the first 50.
fn dedup_head(items: &[(String, PathBuf)]) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for it in items {
        if out.last() != Some(it) {
            out.push(it.clone());
        }
        if out.len() >= 50 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Detect, DetectRules, LanguageAdapter, Tree, UnsupportedLang};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        // nosemgrep: temp-dir
        std::env::temp_dir().join(format!("gate-detect-{tag}-{}", std::process::id()))
    }

    fn tree() -> Tree {
        Tree {
            adapters: vec![
                LanguageAdapter {
                    name: "rust".into(),
                    marker: "Cargo.toml".into(),
                    config_dir: PathBuf::from("/x"),
                    detect: Detect {
                        extensions: vec!["rs".into()],
                        shebangs: vec![],
                    },
                    checks: vec![],
                },
                LanguageAdapter {
                    name: "python".into(),
                    marker: "pyproject.toml".into(),
                    config_dir: PathBuf::from("/x"),
                    detect: Detect {
                        extensions: vec!["py".into()],
                        shebangs: vec!["python3".into()],
                    },
                    checks: vec![],
                },
                LanguageAdapter {
                    name: "node".into(),
                    marker: "package.json".into(),
                    config_dir: PathBuf::from("/x"),
                    detect: Detect {
                        extensions: vec!["ts".into(), "tsx".into(), "js".into()],
                        shebangs: vec!["node".into()],
                    },
                    checks: vec![],
                },
            ],
            scanners: vec![],
            detect: DetectRules {
                skip_dirs: vec!["target".into()],
                unsupported: vec![UnsupportedLang {
                    name: "go".into(),
                    detect: Detect {
                        extensions: vec!["go".into()],
                        shebangs: vec![],
                    },
                }],
            },
        }
    }

    fn proj(lang: &str, rel: &str) -> ResolvedProject {
        ResolvedProject {
            language: lang.into(),
            rel_path: PathBuf::from(rel),
            abs_path: PathBuf::from("/unused"),
            tsconfig: None,
        }
    }

    #[test]
    fn passes_when_fully_declared() {
        let root = scratch("ok");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();
        let projects = vec![proj("rust", "")];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn fails_on_undeclared_language() {
        let root = scratch("undeclared");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("helper.py"), "x = 1").unwrap();
        // Only rust declared; the stray .py must fail.
        let projects = vec![proj("rust", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("helper.py"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn fails_on_unsupported_language() {
        let root = scratch("unsupported");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("server.go"), "package main").unwrap();
        let projects = vec![proj("rust", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("server.go") && err.contains("go"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn subpath_coverage_is_component_wise() {
        let root = scratch("subpath");
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::create_dir_all(root.join("web2")).unwrap();
        std::fs::write(root.join("web/a.py"), "x=1").unwrap();
        // web2 is NOT covered by a `web` project — must fail.
        std::fs::write(root.join("web2/b.py"), "x=1").unwrap();
        let projects = vec![proj("python", "web")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("web2/b.py"), "{err}");
        assert!(!err.contains("web/a.py"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skips_ops_owned_dirs_but_not_repo_gitignore() {
        let root = scratch("skip");
        std::fs::create_dir_all(root.join("target")).unwrap();
        // A stray .go inside an Ops skip dir (target/) is ignored...
        std::fs::write(root.join("target/gen.go"), "package main").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        let projects = vec![proj("rust", "")];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn detects_extensionless_shebang_script() {
        let root = scratch("shebang");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // Extensionless python script, undeclared → must fail.
        std::fs::write(root.join("tool"), "#!/usr/bin/env python3\nx=1\n").unwrap();
        let projects = vec![proj("rust", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("tool") && err.contains("python"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ts_project_without_tsconfig_fails() {
        let root = scratch("ts-no-cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.ts"), "export const x = 1;").unwrap();
        // node project declared but no tsconfig → type-aware lint impossible.
        let projects = vec![proj("node", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("tsconfig") && err.contains("[node]"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ts_project_with_tsconfig_passes() {
        let root = scratch("ts-cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.ts"), "export const x = 1;").unwrap();
        let projects = vec![ResolvedProject {
            language: "node".into(),
            rel_path: PathBuf::new(),
            abs_path: PathBuf::from("/unused"),
            tsconfig: Some(PathBuf::from("/unused/tsconfig.json")),
        }];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn js_only_project_needs_no_tsconfig() {
        let root = scratch("js-only");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.js"), "const x = 1;").unwrap();
        // Plain JS node project: no tsconfig required.
        let projects = vec![proj("node", "")];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }
}
