use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config;
use crate::report::{Check, Finding, Report};

const PROTOCOL_VERSION: &str = "2024-11-05";

const DEFAULT_LIMIT_LINES: usize = 200;

pub fn serve(tree: &config::Tree, source: &Path, report_dir: &Path) -> Result<()> {
    let mut server = Server {
        tree,
        source,
        report_dir,
        cached: None,
    };
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("reading MCP request line from stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{response}").context("writing MCP response to stdout")?;
            out.flush().context("flushing MCP response")?;
        }
    }
    Ok(())
}

struct Server<'a> {
    tree: &'a config::Tree,
    source: &'a Path,
    report_dir: &'a Path,
    cached: Option<Report>,
}

impl Server<'_> {
    fn handle_line(&mut self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(
                    &Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ));
            }
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let id = msg.get("id").cloned()?;

        Some(match self.handle_request(method, &params) {
            Ok(result) => result_response(&id, &result),
            Err((code, message)) => error_response(&id, code, &message),
        })
    }

    fn handle_request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(initialize_result(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools_list()),
            "tools/call" => self.tools_call(params),
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }

    fn tools_call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        match name {
            "run_gate" => Ok(self.run_gate()),
            "get_report_summary" => Ok(self.report_summary()),
            "get_check_output" => Ok(self.check_output(&args)),
            "list_honest_map_violations" => Ok(self.honest_map()),
            other => Err((-32602, format!("unknown tool: {other}"))),
        }
    }

    fn run_gate(&mut self) -> Value {
        let mut log = std::io::stderr().lock();
        let run = match crate::execute(&mut log, self.tree, self.source, None) {
            Ok(run) => run,
            Err(e) => return tool_error(&format!("gate failed to run: {e:#}")),
        };
        let mut report = run.report;
        if let Err(e) = report.write(self.report_dir) {
            return tool_error(&format!(
                "gate ran but its report could not be written: {e:#}"
            ));
        }
        let summary = summary(&report);
        self.cached = Some(report);
        tool_result(&summary)
    }

    fn report_summary(&mut self) -> Value {
        self.ensure_cached();
        match &self.cached {
            Some(report) => tool_result(&summary(report)),
            None => {
                tool_error("no gate run available — call run_gate first (no report.json found)")
            }
        }
    }

    fn check_output(&mut self, args: &Value) -> Value {
        let Some(label) = args.get("label").and_then(Value::as_str) else {
            return tool_error("get_check_output requires a string `label` argument");
        };
        let offset = json_usize(args, "offset_lines").unwrap_or(0);
        let limit = json_usize(args, "limit_lines").unwrap_or(DEFAULT_LIMIT_LINES);
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        let severity = args.get("severity").and_then(Value::as_str);

        self.ensure_cached();
        let Some(report) = &self.cached else {
            return tool_error(
                "no gate run available — call run_gate first (no report.json found)",
            );
        };
        let Some(check) = report.checks().iter().find(|c| c.label == label) else {
            let known: Vec<&str> = report.checks().iter().map(|c| c.label.as_str()).collect();
            return tool_error(&format!(
                "no check labelled {label:?}. known labels: {}",
                known.join(", ")
            ));
        };

        if !raw && !check.findings.is_empty() {
            return tool_result(&findings_payload(check, severity, offset, limit));
        }
        tool_result(&text_payload(check, raw, offset, limit))
    }

    fn honest_map(&mut self) -> Value {
        self.ensure_cached();
        let Some(report) = &self.cached else {
            return tool_error(
                "no gate run available — call run_gate first (no report.json found)",
            );
        };
        let violations: Vec<Value> = report
            .honest_map_violations()
            .iter()
            .map(|v| {
                json!({
                    "class": enum_str(v.class),
                    "language": v.language,
                    "path": v.path,
                    "reason": v.reason,
                })
            })
            .collect();
        tool_result(&json!({ "count": violations.len(), "violations": violations }))
    }

    fn ensure_cached(&mut self) {
        if self.cached.is_none() {
            self.cached = Report::read(self.report_dir).ok().flatten();
        }
    }
}

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "grizzly-gate", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The `tools/list` result: the four gate tools and their input schemas. The
/// descriptions steer an agent toward the context-efficient flow — summary
/// first, then one check's output at a time.
fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "run_gate",
                "description": "Run the grizzly-gate local pre-check against the working tree and return a COMPACT verdict only: pass/fail, which phase failed, check counts, the labels of failing checks, and any honest-map violation classes. Does NOT return raw check output — call get_check_output for a specific failing label. Never signs or scans an image (source-only, same as a local pre-check).",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
            },
            {
                "name": "get_check_output",
                "description": "Return one check's FOCUSED output by label. For tools the gate parses structurally (clippy, cargo-deny, …) this returns {mode:'findings', total, has_more, findings:[{file,line,col,severity,rule,message}]} — optionally filtered by `severity` — paginated by finding. For text tools it returns {mode:'distilled', total_lines, has_more, output} (noise-filtered text, paginated by line). Pass raw=true for {mode:'raw', …} verbatim unfiltered tool output (the durable record). Page with offset_lines/limit_lines. Use the labels from run_gate's failing_check_labels.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string", "description": "The check label, e.g. 'rust:clippy' or 'scan:gitleaks'." },
                        "raw": { "type": "boolean", "description": "Return the verbatim combined stdout+stderr instead of the focused surface. Default false." },
                        "severity": { "type": "string", "enum": ["error", "warning", "note"], "description": "In findings mode, return only findings of this severity." },
                        "offset_lines": { "type": "integer", "minimum": 0, "description": "First item to return (0-based): line in text/raw mode, finding in findings mode. Default 0." },
                        "limit_lines": { "type": "integer", "minimum": 1, "description": "Max items to return. Default 200." }
                    },
                    "required": ["label"],
                    "additionalProperties": false
                }
            },
            {
                "name": "list_honest_map_violations",
                "description": "Return the full structured phase-1 honest-map violations (class, language, path, reason). Use when run_gate reports failed_phase 'honest-map'.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
            },
            {
                "name": "get_report_summary",
                "description": "Return the COMPACT verdict for the most recent run WITHOUT re-running the gate (reads the last report.json). Use to recall the verdict; use run_gate to produce a fresh one.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
            }
        ]
    })
}

pub(crate) fn findings_payload(
    check: &Check,
    severity: Option<&str>,
    offset: usize,
    limit: usize,
) -> Value {
    let selected: Vec<&Finding> = check
        .findings
        .iter()
        .filter(|f| severity.is_none_or(|s| f.severity.as_deref() == Some(s)))
        .collect();
    let total = selected.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let page: Vec<Value> = selected
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .filter_map(|f| serde_json::to_value(f).ok())
        .collect();
    json!({
        "mode": "findings",
        "total": total,
        "has_more": end < total,
        "findings": page,
    })
}

pub(crate) fn text_payload(check: &Check, raw: bool, offset: usize, limit: usize) -> Value {
    let (mode, body) = if raw {
        ("raw", &check.output)
    } else {
        ("distilled", &check.distilled)
    };
    let page = paginate(body, offset, limit);
    json!({
        "mode": mode,
        "total_lines": page.total,
        "has_more": page.has_more,
        "output": page.text,
    })
}

fn summary(report: &Report) -> Value {
    let checks = report.checks();
    let failing: Vec<&str> = checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.label.as_str())
        .collect();
    let failing_findings: Vec<Value> = checks
        .iter()
        .filter(|c| !c.ok && !c.findings.is_empty())
        .map(|c| json!({ "label": c.label, "findings": c.findings.len() }))
        .collect();
    let violations: Vec<Value> = report
        .honest_map_violations()
        .iter()
        .map(|v| {
            json!({
                "class": enum_str(v.class),
                "language": v.language,
                "path": v.path,
            })
        })
        .collect();
    json!({
        "verdict": enum_str(report.verdict()),
        "failed_phase": report.failed_phase().map(enum_str),
        "checks_total": checks.len(),
        "checks_failed": failing.len(),
        "failing_check_labels": failing,
        "failing_check_findings": failing_findings,
        "honest_map_violation_count": violations.len(),
        "honest_map_violations": violations,
    })
}

fn tool_result(value: &Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.to_string() }] })
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn result_response(id: &Value, result: &Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

struct Page {
    total: usize,
    has_more: bool,
    text: String,
}

fn paginate(output: &str, offset: usize, limit: usize) -> Page {
    let all: Vec<&str> = output.lines().collect();
    let total = all.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let page = all.get(start..end).unwrap_or_default();
    Page {
        total,
        has_more: end < total,
        text: page.join("\n"),
    }
}

fn json_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
}

fn enum_str<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Check;

    fn check(label: &str, ok: bool, output: &str) -> Check {
        Check {
            label: label.into(),
            language: None,
            project: None,
            cmd: "cmd".into(),
            ok,
            exit_code: Some(i32::from(!ok)),
            duration_secs: 0.0,
            findings: Vec::new(),
            distilled: output.into(),
            output: output.into(),
        }
    }

    #[test]
    fn paginate_returns_whole_small_output() {
        let p = paginate("a\nb\nc", 0, 200);
        assert_eq!(p.total, 3);
        assert!(!p.has_more, "nothing left after a full page");
        assert_eq!(p.text, "a\nb\nc");
    }

    #[test]
    fn paginate_pages_through_a_long_output() {
        let body = "0\n1\n2\n3\n4\n5\n6\n7\n8\n9";
        let first = paginate(body, 0, 4);
        assert_eq!(first.text, "0\n1\n2\n3");
        assert!(first.has_more, "more lines remain after the first page");

        let mid = paginate(body, 4, 4);
        assert_eq!(mid.text, "4\n5\n6\n7");
        assert!(mid.has_more);

        let last = paginate(body, 8, 4);
        assert_eq!(last.text, "8\n9");
        assert!(!last.has_more, "the tail page is the end");
    }

    #[test]
    fn paginate_past_the_end_is_empty_not_an_error() {
        let p = paginate("a\nb", 99, 10);
        assert!(!p.has_more);
        assert_eq!(p.text, "", "offset past the end yields an empty page");
    }

    #[test]
    fn summary_reports_counts_and_failing_labels_only() {
        let mut report = Report::new();
        report.set_checks(vec![
            check("rust:fmt", true, "ok"),
            check("rust:clippy", false, "error: boom\nmore detail"),
        ]);
        let s = summary(&report);
        assert_eq!(s.get("verdict").and_then(Value::as_str), Some("fail"));
        assert_eq!(
            s.get("failed_phase").and_then(Value::as_str),
            Some("checks")
        );
        assert_eq!(s.get("checks_total").and_then(Value::as_u64), Some(2));
        assert_eq!(s.get("checks_failed").and_then(Value::as_u64), Some(1));
        assert_eq!(s.get("failing_check_labels"), Some(&json!(["rust:clippy"])));
        assert!(
            !s.to_string().contains("boom"),
            "summary leaked raw output: {s}"
        );
    }

    #[test]
    fn tools_list_advertises_every_tool() {
        let list = tools_list();
        let names: Vec<&str> = list
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(names.len(), 4, "all four tools listed: {names:?}");
        for want in [
            "run_gate",
            "get_check_output",
            "list_honest_map_violations",
            "get_report_summary",
        ] {
            assert!(names.contains(&want), "missing tool {want}");
        }
    }

    #[test]
    fn initialize_echoes_requested_protocol_version() {
        let init = initialize_result(&json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(
            init.get("protocolVersion").and_then(Value::as_str),
            Some("2025-06-18")
        );
        assert_eq!(
            init.pointer("/serverInfo/name").and_then(Value::as_str),
            Some("grizzly-gate")
        );
        assert!(init
            .pointer("/capabilities/tools")
            .is_some_and(Value::is_object));
    }

    #[test]
    fn error_response_carries_id_and_code() {
        let resp = error_response(&json!(7), -32601, "method not found: bogus");
        let v: Value = serde_json::from_str(&resp).expect("valid JSON-RPC");
        assert_eq!(v.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
        assert_eq!(v.get("id").and_then(Value::as_i64), Some(7));
        assert_eq!(
            v.pointer("/error/code").and_then(Value::as_i64),
            Some(-32601)
        );
    }
}
