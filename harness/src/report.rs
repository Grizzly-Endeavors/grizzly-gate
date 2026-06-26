//! The structured, machine-queryable run report (`grizzly-gate-report/report.json`).
//!
//! Both gate phases feed one artifact, written on every run (pass or fail) so
//! tooling can rely on its presence. Its reason for existing: a failing gate
//! emits a lot of tool output, and an automated fix loop (or a human) should be
//! able to pull *one* failing check's output with `jq` instead of ingesting the
//! whole log. The full, untruncated output of every check lives here — the
//! terminal's `FAILURES` block is the only place truncation happens, and it
//! always points back to this file.
//!
//! - [`crate::gateconfig`] / [`crate::detect`] surface honest-map [`Violation`]s
//!   via [`HonestMapFailure`] (which also carries the rich human message).
//! - [`crate::main`] feeds [`Check`] rows and writes the file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Schema version of the emitted report. Bumped only on a breaking shape change
/// so a consumer can refuse an unknown layout rather than misread it.
pub const SCHEMA: u32 = 1;

/// Report filename written inside the report dir.
pub const FILE: &str = "report.json";

/// One gate run, serialized to `report.json`.
#[derive(Serialize, Deserialize)]
pub struct Report {
    schema: u32,
    verdict: Verdict,
    /// Which phase failed, if any. `None` on a clean pass.
    failed_phase: Option<Phase>,
    honest_map: HonestMap,
    checks: Vec<Check>,
    /// `jq` recipes for reading this report, embedded so the query travels with
    /// the artifact. Filled in at [`Report::write`] time from the real path.
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

/// One honest-map problem, structured for querying. The same problems are also
/// rendered to a human message on [`HonestMapFailure`].
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
    /// The declaration itself is invalid (parse/version error, or a project
    /// that doesn't resolve: unknown language, missing marker, bad path…).
    MalformedDeclaration,
    /// Adapter-backed code in the tree not covered by any declared project.
    Undeclared,
    /// A code language the gate has no adapter for — cannot be gated.
    Unsupported,
    /// A node project containing TypeScript but declaring no `tsconfig`.
    TsWithoutTsconfig,
}

/// One executed check (language adapter step or scanner). `output` is the full,
/// untruncated combined stdout+stderr — this is the durable record.
#[derive(Serialize, Deserialize, Clone)]
pub struct Check {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub cmd: String,
    pub ok: bool,
    /// Process exit code, or `None` if the tool could not be spawned/parsed.
    pub exit_code: Option<i32>,
    pub duration_secs: f64,
    pub output: String,
}

/// A fatal honest-map failure: the rich human message *and* the structured
/// violations behind it. Phase-1 code returns this so `main` can both print the
/// message and record the violations in the report before failing closed.
#[derive(Debug)]
pub struct HonestMapFailure {
    pub message: String,
    pub violations: Vec<Violation>,
}

impl HonestMapFailure {
    /// A whole-declaration failure that isn't per-project (missing file, parse
    /// error, version mismatch, zero projects): one malformed-declaration
    /// violation whose reason is the human message.
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
    /// A fresh report defaulting to a clean pass; mutate as phases run.
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

    /// Record a phase-1 honest-map failure.
    pub fn fail_honest_map(&mut self, violations: Vec<Violation>) {
        self.verdict = Verdict::Fail;
        self.failed_phase = Some(Phase::HonestMap);
        self.honest_map = HonestMap {
            ok: false,
            violations,
        };
    }

    /// Record the executed checks; flips the verdict to fail if any check failed.
    pub fn set_checks(&mut self, checks: Vec<Check>) {
        if checks.iter().any(|c| !c.ok) {
            self.verdict = Verdict::Fail;
            self.failed_phase = Some(Phase::Checks);
        }
        self.checks = checks;
    }

    /// Serialize to `<dir>/report.json`, creating `dir` if needed. Fills in the
    /// `query_hints` from the real path first. Returns the written path.
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
        Ok(path)
    }

    /// The `jq` recipes for this report (also embedded in the JSON). Printed in
    /// the terminal `FAILURES` block so the query recipe is always at hand.
    pub fn query_hints(&self) -> &[String] {
        &self.query_hints
    }

    /// Read a previously-written `report.json` from `dir`, if one exists. Returns
    /// `Ok(None)` when no report has been written there yet (e.g. an MCP session
    /// that queries the summary before its first run). A present-but-corrupt file
    /// is surfaced as an error rather than silently treated as absent.
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

    /// The run verdict (pass/fail).
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Which phase failed, if any.
    pub fn failed_phase(&self) -> Option<Phase> {
        self.failed_phase
    }

    /// The executed checks (empty when phase 1 failed before any check ran).
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// The recorded honest-map violations (empty on a clean phase 1).
    pub fn honest_map_violations(&self) -> &[Violation] {
        &self.honest_map.violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        // nosemgrep: temp-dir
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
            output: "error: this is the full clippy output\n".into(),
        }]);
        let path = report.write(&dir).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.get("schema").unwrap(), SCHEMA, "schema");
        assert_eq!(v.get("verdict").unwrap(), "fail", "verdict");
        assert_eq!(v.get("failed_phase").unwrap(), "checks", "failed_phase");
        // The full output is retrievable by a single-check lookup, untruncated.
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
