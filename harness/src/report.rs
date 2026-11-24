use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 2;

pub const FILE: &str = "report.json";

fn adopt_dir_owner(dir: &Path, paths: &[&Path]) {
    use std::os::unix::fs::MetadataExt;

    let reference = match dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Ok(meta) = std::fs::metadata(reference) else {
        return;
    };
    let (uid, gid) = (meta.uid(), meta.gid());
    if uid == 0 {
        return;
    }
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.uid() == uid && m.gid() == gid => continue,
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "grizzly-gate :: warning: could not stat {}: {e}",
                    path.display()
                );
                continue;
            }
        }
        if let Err(e) = std::os::unix::fs::chown(path, Some(uid), Some(gid)) {
            eprintln!(
                "grizzly-gate :: warning: could not give {} to uid {uid}: {e}",
                path.display()
            );
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Report {
    schema: u32,
    verdict: Verdict,
    failed_phase: Option<Phase>,
    honest_map: HonestMap,
    checks: Vec<Check>,
    query_hints: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    HonestMap,
    Checks,
}

#[derive(Serialize, Deserialize)]
struct HonestMap {
    ok: bool,
    violations: Vec<Violation>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Violation {
    pub class: ViolationClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum ViolationClass {
    MalformedDeclaration,
    Undeclared,
    Unsupported,
    TsWithoutTsconfig,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Tool rule / lint / advisory id (e.g. `clippy::unwrap_used`, `RUSTSEC-…`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Check {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub cmd: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub duration_secs: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub distilled: String,
    pub output: String,
}

#[derive(Debug)]
pub struct HonestMapFailure {
    pub message: String,
    pub violations: Vec<Violation>,
}

impl HonestMapFailure {
    pub fn whole(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            violations: vec![Violation {
                class: ViolationClass::MalformedDeclaration,
                language: None,
                path: Some(crate::gateconfig::FILE.to_string()),
                reason: message.clone(),
            }],
            message,
        }
    }
}

impl std::fmt::Display for HonestMapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HonestMapFailure {}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

impl Report {
    pub fn new() -> Self {
        Self {
            schema: SCHEMA,
            verdict: Verdict::Pass,
            failed_phase: None,
            honest_map: HonestMap {
                ok: true,
                violations: Vec::new(),
            },
            checks: Vec::new(),
            query_hints: Vec::new(),
        }
    }

    pub fn fail_honest_map(&mut self, violations: Vec<Violation>) {
        self.verdict = Verdict::Fail;
        self.failed_phase = Some(Phase::HonestMap);
        self.honest_map = HonestMap {
            ok: false,
            violations,
        };
    }

    pub fn set_checks(&mut self, checks: Vec<Check>) {
        if checks.iter().any(|c| !c.ok) {
            self.verdict = Verdict::Fail;
            self.failed_phase = Some(Phase::Checks);
        }
        self.checks = checks;
    }

    pub fn write(&mut self, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating report dir {}", dir.display()))?;
        let path = dir.join(FILE);
        let p = path.display();
        self.query_hints = vec![
            format!("list failed checks: jq -r '.checks[] | select(.ok==false) | .label' {p}"),
            format!(
                "one check's full output: jq -r '.checks[] | select(.label==\"<label>\") | .output' {p}"
            ),
            format!("honest-map violations: jq -c '.honest_map.violations[]' {p}"),
        ];
        let json = serde_json::to_string_pretty(self).context("serializing report")?;
        std::fs::write(&path, json)
            .with_context(|| format!("writing report {}", path.display()))?;
        adopt_dir_owner(dir, &[dir, &path]);
        Ok(path)
    }

    pub fn query_hints(&self) -> &[String] {
        &self.query_hints
    }

    pub fn read(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading report {}", path.display())),
        };
        let report = serde_json::from_str(&text)
            .with_context(|| format!("parsing report {}", path.display()))?;
        Ok(Some(report))
    }

    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    pub fn failed_phase(&self) -> Option<Phase> {
        self.failed_phase
    }

    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    pub fn honest_map_violations(&self) -> &[Violation] {
        &self.honest_map.violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gate-report-{tag}-{}", std::process::id()))
    }

    #[test]
    fn round_trips_full_output_and_verdict() {
        let dir = scratch("rt");
        let mut report = Report::new();
        report.set_checks(vec![Check {
            label: "rust:clippy".into(),
            language: Some("rust".into()),
            project: Some("svc".into()),
            cmd: "cargo clippy".into(),
            ok: false,
            exit_code: Some(101),
            duration_secs: 2.1,
            findings: Vec::new(),
            distilled: "error: this is the full clippy output\n".into(),
            output: "error: this is the full clippy output\n".into(),
        }]);
        let path = report.write(&dir).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.get("schema").unwrap(), SCHEMA, "schema");
        assert_eq!(v.get("verdict").unwrap(), "fail", "verdict");
        assert_eq!(v.get("failed_phase").unwrap(), "checks", "failed_phase");
        let out = v
            .get("checks")
            .and_then(|c| c.as_array())
            .unwrap()
            .iter()
            .find(|c| c.get("label").is_some_and(|l| l == "rust:clippy"))
            .and_then(|c| c.get("output"))
            .and_then(|o| o.as_str())
            .unwrap();
        assert!(out.contains("full clippy output"), "{out}");
        assert!(!report.query_hints().is_empty(), "query hints present");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn honest_map_failure_serializes_violations() {
        let dir = scratch("hm");
        let mut report = Report::new();
        report.fail_honest_map(vec![Violation {
            class: ViolationClass::Unsupported,
            language: Some("go".into()),
            path: Some("server.go".into()),
            reason: "no adapter".into(),
        }]);
        let path = report.write(&dir).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v.get("verdict").unwrap(), "fail", "verdict");
        assert_eq!(v.get("failed_phase").unwrap(), "honest-map", "failed_phase");
        let first = v
            .pointer("/honest_map/violations/0")
            .expect("first violation present");
        assert_eq!(
            v.pointer("/honest_map/ok").unwrap(),
            &serde_json::Value::Bool(false),
            "honest_map.ok"
        );
        assert_eq!(first.get("class").unwrap(), "unsupported", "class");
        assert_eq!(first.get("path").unwrap(), "server.go", "path");
        std::fs::remove_dir_all(&dir).ok();
    }
}
