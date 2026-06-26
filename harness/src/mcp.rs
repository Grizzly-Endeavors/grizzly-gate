//! MCP (Model Context Protocol) server mode for the gate.
//!
//! The gate normally runs once and prints a verdict; on a failure that verdict
//! carries the full, untruncated output of every failing check — exactly the
//! kind of payload that overwhelms an agent's context window when it's read
//! wholesale. This module serves the gate over MCP instead, so an agent can run
//! the gate and then pull *one* check's output (a page at a time) on demand,
//! rather than ingesting the whole report.
//!
//! Transport is the MCP stdio convention: newline-delimited JSON-RPC 2.0, one
//! message per line, requests on stdin and responses on stdout. The gate's own
//! human progress log is routed to **stderr** (see [`crate::execute`]) so it can
//! never corrupt the protocol stream on stdout.
//!
//! The protocol surface is deliberately tiny — `initialize`, `tools/list`,
//! `tools/call`, `ping` — and hand-rolled on `serde_json` rather than pulling in
//! an MCP SDK + async runtime, keeping this lean binary dependency-light. The
//! four tools are:
//!
//! - `run_gate` — run the gate and return a compact verdict (counts + failing
//!   labels + honest-map violation classes), never the raw output.
//! - `get_check_output` — fetch one check's output by label, line-paginated.
//! - `list_honest_map_violations` — the structured phase-1 violations, in full.
//! - `get_report_summary` — the last run's compact verdict without re-running.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config;
use crate::report::Report;

/// MCP protocol revision advertised when the client doesn't request one. The
/// client's requested version is echoed back when present, for forward compat.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Default page size (lines) for `get_check_output` when the caller gives no
/// `limit_lines` — large enough to be useful, small enough to stay context-safe.
const DEFAULT_LIMIT_LINES: usize = 200;

/// Serve the gate over MCP on stdio until stdin closes. `tree` and `source` are
/// resolved once by the caller; `report_dir` is where each run's `report.json`
/// is written and where a prior run is read back from.
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

/// Per-session server state: the resolved config + source, the report directory,
/// and the last run's report (lazily loaded from disk so a summary query before
/// the first in-session `run_gate` can still answer from a prior run).
struct Server<'a> {
    tree: &'a config::Tree,
    source: &'a Path,
    report_dir: &'a Path,
    cached: Option<Report>,
}

impl Server<'_> {
    /// Parse one JSON-RPC line and produce the response line, or `None` for a
    /// notification (which gets no reply). A malformed line is answered with a
    /// parse error rather than killing the session.
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

        // No id ⇒ a notification: act on it if we care, but never reply.
        let id = msg.get("id").cloned()?;

        Some(match self.handle_request(method, &params) {
            Ok(result) => result_response(&id, &result),
            Err((code, message)) => error_response(&id, code, &message),
        })
    }

    /// Dispatch a request method to its handler. `Ok` carries the JSON-RPC
    /// `result`; `Err((code, message))` becomes a JSON-RPC error object.
    fn handle_request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(initialize_result(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools_list()),
            "tools/call" => self.tools_call(params),
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }

    /// Handle `tools/call`: route by tool name to the matching gate operation.
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

    /// Run the gate once against the source tree, write the report, cache it, and
    /// return the compact verdict. Never signs and never scans an image — the
    /// same source-only surface as a local pre-check.
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

    /// Return the last run's compact verdict without re-running, loading a prior
    /// run's report from disk if this session hasn't run the gate yet.
    fn report_summary(&mut self) -> Value {
        self.ensure_cached();
        match &self.cached {
            Some(report) => tool_result(&summary(report)),
            None => {
                tool_error("no gate run available — call run_gate first (no report.json found)")
            }
        }
    }

    /// Fetch one check's output by label, paginated by line. Keeps the big
    /// output blobs out of context until a specific failing check is inspected.
    fn check_output(&mut self, args: &Value) -> Value {
        let Some(label) = args.get("label").and_then(Value::as_str) else {
            return tool_error("get_check_output requires a string `label` argument");
        };
        let offset = json_usize(args, "offset_lines").unwrap_or(0);
        let limit = json_usize(args, "limit_lines").unwrap_or(DEFAULT_LIMIT_LINES);

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

        let page = paginate(&check.output, offset, limit);
        tool_result(&json!({
            "label": check.label,
            "ok": check.ok,
            "exit_code": check.exit_code,
            "cmd": check.cmd,
            "total_lines": page.total,
            "offset_lines": page.start,
            "returned_lines": page.returned,
            "has_more": page.has_more,
            "output": page.text,
        }))
    }

    /// Return the full structured phase-1 honest-map violations (class, language,
    /// path, reason). These are short, so unlike check output they're returned
    /// whole.
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

    /// Populate `cached` from `report.json` on disk if it isn't already set. A
    /// read/parse failure leaves the cache empty; the caller surfaces "no run".
    fn ensure_cached(&mut self) {
        if self.cached.is_none() {
            self.cached = Report::read(self.report_dir).ok().flatten();
        }
    }
}

/// The `initialize` result: advertise tool capability and echo the client's
/// requested protocol version (or our default) back.
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
                "description": "Return one check's full output by label, paginated by line, so a large log never lands in context wholesale. Returns total_lines/offset_lines/returned_lines/has_more for paging. Use the labels from run_gate's failing_check_labels.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string", "description": "The check label, e.g. 'rust:clippy' or 'scan:gitleaks'." },
                        "offset_lines": { "type": "integer", "minimum": 0, "description": "First output line to return (0-based). Default 0." },
                        "limit_lines": { "type": "integer", "minimum": 1, "description": "Max lines to return. Default 200." }
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

/// The compact verdict shared by `run_gate` and `get_report_summary`: counts and
/// labels, never raw check output.
fn summary(report: &Report) -> Value {
    let checks = report.checks();
    let failing: Vec<&str> = checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.label.as_str())
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
        "honest_map_violation_count": violations.len(),
        "honest_map_violations": violations,
    })
}

/// Wrap a JSON value as a successful `tools/call` result: a single text content
/// block carrying the compact JSON, which is what an MCP client renders.
fn tool_result(value: &Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.to_string() }] })
}

/// A `tools/call` result flagged as an error (MCP convention: tool-level errors
/// are results with `isError`, not JSON-RPC protocol errors).
fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// A full JSON-RPC success response line.
fn result_response(id: &Value, result: &Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// A full JSON-RPC error response line.
fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

/// One page of a check's output, plus the metadata a caller needs to keep
/// paging without ever loading the whole blob.
struct Page {
    /// First line index actually returned (clamped into range).
    start: usize,
    /// Total lines in the check's output.
    total: usize,
    /// Lines in this page.
    returned: usize,
    /// Whether lines remain after this page.
    has_more: bool,
    /// The page text (lines re-joined with `\n`).
    text: String,
}

/// Slice `output` into a page of at most `limit` lines starting at line `offset`
/// (0-based). An out-of-range `offset` yields an empty page rather than an error,
/// so a caller can page to the end without tracking the exact length itself.
fn paginate(output: &str, offset: usize, limit: usize) -> Page {
    let all: Vec<&str> = output.lines().collect();
    let total = all.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let page = all.get(start..end).unwrap_or_default();
    Page {
        start,
        total,
        returned: page.len(),
        has_more: end < total,
        text: page.join("\n"),
    }
}

/// Read an unsigned integer argument as `usize`, clamping an over-large value to
/// `usize::MAX` rather than truncating it.
fn json_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
}

/// Render a serializable enum (Verdict/Phase/ViolationClass) to its serde string
/// form, so the wire spelling stays in lockstep with the report schema instead
/// of being duplicated in a match here.
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
            output: output.into(),
        }
    }

    #[test]
    fn paginate_returns_whole_small_output() {
        let p = paginate("a\nb\nc", 0, 200);
        assert_eq!(p.total, 3);
        assert_eq!(p.returned, 3);
        assert!(!p.has_more, "nothing left after a full page");
        assert_eq!(p.text, "a\nb\nc");
    }

    #[test]
    fn paginate_pages_through_a_long_output() {
        let body = "0\n1\n2\n3\n4\n5\n6\n7\n8\n9";
        let first = paginate(body, 0, 4);
        assert_eq!(first.returned, 4);
        assert_eq!(first.text, "0\n1\n2\n3");
        assert!(first.has_more, "more lines remain after the first page");

        let mid = paginate(body, 4, 4);
        assert_eq!(mid.start, 4);
        assert_eq!(mid.text, "4\n5\n6\n7");
        assert!(mid.has_more);

        let last = paginate(body, 8, 4);
        assert_eq!(last.text, "8\n9");
        assert!(!last.has_more, "the tail page is the end");
    }

    #[test]
    fn paginate_past_the_end_is_empty_not_an_error() {
        let p = paginate("a\nb", 99, 10);
        assert_eq!(p.start, 2, "offset is clamped to the line count");
        assert_eq!(p.returned, 0);
        assert!(!p.has_more);
        assert_eq!(p.text, "");
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
        // The compact verdict must NOT carry raw check output.
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
