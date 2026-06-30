//! Honest-map verification: the trusted half of the `gate-config.json` contract.
//!
//! [`crate::gateconfig`] parses what the repo *claims*; this module walks the
//! tree and establishes what is *actually* there, then fails closed on any
//! mismatch. It is deliberately implemented in the harness (not delegated to a
//! repo-influenceable tool) and is hostile by construction:
//!
//! - it scopes to the **clean-checkout content** — the git index (tracked files
//!   plus untracked-but-not-ignored files), exactly what a fresh `git clone`
//!   would hold — so a `.gitignore` cannot hide a *tracked* file from detection
//!   (`git add -f`'d code is still listed), while a local pre-check no longer
//!   trips over build artifacts that would never reach CI. It falls back to a
//!   hostile raw-filesystem walk (skipping only an Ops-owned `skip_dirs` list)
//!   when `source` is not a git work tree — strictly more inclusive, so
//!   completeness can only tighten under fallback. See ADR-036.
//! - it does **not** follow symlinks (no escaping the tree, no loops);
//! - extension matching is case-insensitive (`.RS` == `.rs`);
//! - extensionless executables are classified by shebang interpreter.
//!
//! Three failure classes: *undeclared* adapter-backed code (a `.rs` not covered
//! by any declared rust project), *unsupported* code (a language with no adapter
//! at all — e.g. Go), and a node project that contains TypeScript but declares no
//! `tsconfig` (type-aware linting needs the TS program). Any one is fatal.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    for rel in enumerate_files(source, &tree.detect.skip_dirs)? {
        // Never treat the declaration itself as code.
        if rel == Path::new(crate::gateconfig::FILE) {
            continue;
        }
        let path = source.join(&rel);

        let hit = classify(&path, &ext, &shebang);
        match hit {
            Some(Hit::Adapter(lang)) => match covering(&rel, &lang, projects) {
                None => undeclared.push((lang, rel)),
                Some(p) => {
                    // A node project needs a declared tsconfig if it contains
                    // TypeScript — either a standalone `.ts`/`.tsx` file or a
                    // `.svelte` component whose `<script>` uses `lang="ts"`. Both
                    // need the TS program for type-aware checking (tsc / svelte-check).
                    let has_ts = is_typescript(&rel) || (is_svelte(&rel) && svelte_uses_ts(&path));
                    if lang == "node" && has_ts && p.tsconfig.is_none() {
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

/// Whether `rel` is a Svelte single-file component (`.svelte`).
fn is_svelte(rel: &Path) -> bool {
    rel.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| e == "svelte")
}

/// Whether a `.svelte` component declares a TypeScript `<script>` block. Such a
/// component needs the TS program (svelte-check) just like a standalone `.ts`
/// file, so it makes its node project require a declared tsconfig. Components are
/// small, so the whole file is read; an unreadable file is treated as not-TS (the
/// lint/check steps surface a genuinely broken file).
fn svelte_uses_ts(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|c| svelte_script_is_ts(&c))
}

/// Scan component source for a `<script ... lang="ts">` (or `'ts'`/`typescript`)
/// opening tag, tolerant of attribute order and `context="module"`. Uses only
/// iterator methods (no slicing) to stay within the gate's own lint floor.
fn svelte_script_is_ts(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.split("<script").skip(1).any(|seg| {
        let tag: String = seg
            .chars()
            .take_while(|&c| c != '>')
            .filter(|c| !c.is_whitespace())
            .collect();
        tag.contains("lang=\"ts\"")
            || tag.contains("lang='ts'")
            || tag.contains("lang=\"typescript\"")
            || tag.contains("lang='typescript'")
    })
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

/// The repo-relative regular-file paths the honest map must classify, with
/// Ops-owned `skip_dirs` (and `.git`) pruned and symlinks excluded.
///
/// In a git work tree this is the **clean-checkout content** — tracked files plus
/// untracked-but-not-ignored files (see [`git_listing`]) — so a `.gitignore`
/// cannot hide a *tracked* file from detection, while a local pre-check stops
/// tripping over build artifacts a fresh `git clone` would never hold. Falls back
/// to a hostile raw-filesystem walk when `source` is not a git work tree (or git
/// errors): that walk is strictly more inclusive, so completeness can only
/// tighten under fallback. See ADR-036.
fn enumerate_files(source: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>, HonestMapFailure> {
    match git_listing(source) {
        Some(rels) => Ok(rels
            .into_iter()
            .filter(|rel| !rel_is_skipped(rel, skip_dirs) && is_regular_file(&source.join(rel)))
            .collect()),
        None => filesystem_walk(source, skip_dirs),
    }
}

/// The git index of `source` as repo-relative paths: tracked files (`--cached`)
/// plus untracked-but-not-ignored files (`--others --exclude-standard`). `--z`
/// keeps the parse byte-exact so a non-UTF-8 filename cannot slip detection.
/// `safe.directory=*` neutralizes git's dubious-ownership guard (the local
/// pre-check mounts a host-owned tree into a root container); it governs only
/// whether git will operate, never which files are listed. `None` means "not a
/// git work tree, or git failed" — the caller then falls back to the raw walk.
fn git_listing(source: &Path) -> Option<Vec<PathBuf>> {
    let out = Command::new("git")
        .current_dir(source)
        .args([
            "-c",
            "safe.directory=*",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(OsStr::from_bytes(s)))
            .collect(),
    )
}

/// The original hostile walk: every regular file under `source`, pruning
/// Ops-owned `skip_dirs` and `.git`, never following symlinks. A walk error fails
/// closed rather than passing on a partial view of the tree.
fn filesystem_walk(source: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>, HonestMapFailure> {
    let mut out = Vec::new();
    let walker = WalkDir::new(source).follow_links(false).into_iter();
    for entry in walker.filter_entry(|e| !is_skipped_dir(e, source, skip_dirs)) {
        let entry =
            entry.map_err(|e| HonestMapFailure::whole(format!("walking source tree: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(source) {
            out.push(rel.to_path_buf());
        }
    }
    Ok(out)
}

/// Whether any component of a repo-relative path is an Ops-owned skip dir (or
/// `.git`) — the git-listing equivalent of [`is_skipped_dir`]'s directory prune.
fn rel_is_skipped(rel: &Path, skip_dirs: &[String]) -> bool {
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| name == ".git" || skip_dirs.iter().any(|d| d == name))
    })
}

/// Whether `path` is an existing regular file (not a symlink, dir, or absent).
/// `symlink_metadata` does not follow symlinks, preserving the no-tree-escape
/// guarantee for git-listed entries (which may include tracked symlinks).
fn is_regular_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_file())
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
                        extensions: vec!["ts".into(), "tsx".into(), "js".into(), "svelte".into()],
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

    #[test]
    fn svelte_ts_component_without_tsconfig_fails() {
        let root = scratch("svelte-ts-no-cfg");
        std::fs::create_dir_all(&root).unwrap();
        // A .svelte component with a TypeScript <script> needs the TS program.
        std::fs::write(
            root.join("App.svelte"),
            "<script lang=\"ts\">\n  export let n: number = 0;\n</script>\n",
        )
        .unwrap();
        let projects = vec![proj("node", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("tsconfig") && err.contains("[node]"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn svelte_ts_component_with_tsconfig_passes() {
        let root = scratch("svelte-ts-cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("App.svelte"),
            "<script context=\"module\" lang='ts'>\n  export const x: string = \"\";\n</script>\n",
        )
        .unwrap();
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
    fn plain_svelte_component_needs_no_tsconfig() {
        let root = scratch("svelte-plain");
        std::fs::create_dir_all(&root).unwrap();
        // No `lang="ts"`: a JS-script component does not require a tsconfig.
        std::fs::write(
            root.join("App.svelte"),
            "<script>\n  export let n = 0;\n</script>\n<p>{n}</p>\n",
        )
        .unwrap();
        let projects = vec![proj("node", "")];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn svelte_script_ts_detection() {
        assert!(svelte_script_is_ts("<script lang=\"ts\">let x;</script>"));
        assert!(svelte_script_is_ts(
            "<script   lang = 'ts' >let x;</script>"
        ));
        assert!(svelte_script_is_ts(
            "<script context=\"module\" lang=\"typescript\">x</script>"
        ));
        assert!(!svelte_script_is_ts("<script>let x;</script>"));
        assert!(!svelte_script_is_ts("<p>lang=\"ts\" in markup only</p>"));
    }

    // --- git-listing mode (ADR-036) -----------------------------------------
    //
    // The scratch dirs above are not git repos, so they exercise the filesystem
    // fallback. These tests init a real repo so `enumerate_files` takes the git
    // path, and pin the property that matters: a `.gitignore` cannot hide a
    // *tracked* file, while locally-ignored *untracked* files fall out of scope.

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git binary available for tests");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn git_mode_excludes_ignored_untracked_files() {
        let root = scratch("git-ignored");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "*.py\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // An untracked, ignored stray .py fails the raw walk; in git mode it is
        // not part of the clean checkout, so it is excluded and the gate passes.
        std::fs::write(root.join("scratch.py"), "x = 1").unwrap();
        let projects = vec![proj("rust", "")];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_mode_flags_force_added_ignored_file() {
        let root = scratch("git-force-add");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "*.py\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // The evasion ADR-029 closes: a .py matching .gitignore but force-added
        // (tracked) IS in the clean checkout, so it must still be flagged.
        std::fs::write(root.join("hidden.py"), "x = 1").unwrap();
        git(&root, &["add", "-f", "hidden.py"]);
        let projects = vec![proj("rust", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("hidden.py"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_mode_flags_untracked_unignored_code() {
        let root = scratch("git-untracked");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // A brand-new, not-yet-committed, not-ignored .go reaches CI once the dev
        // commits, so the local pre-check still catches it before commit.
        std::fs::write(root.join("server.go"), "package main").unwrap();
        let projects = vec![proj("rust", "")];
        let err = verify(&root, &tree(), &projects).unwrap_err().to_string();
        assert!(err.contains("server.go"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_mode_prunes_skip_dirs() {
        let root = scratch("git-skip");
        std::fs::create_dir_all(root.join("target")).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // Even a tracked file under an Ops skip_dir stays out of detection,
        // matching the fallback walk's directory prune.
        std::fs::write(root.join("target/gen.go"), "package main").unwrap();
        git(&root, &["add", "-f", "target/gen.go"]);
        let projects = vec![proj("rust", "")];
        assert!(verify(&root, &tree(), &projects).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }
}
