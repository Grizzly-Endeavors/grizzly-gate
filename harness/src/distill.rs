//! Output distillation: turn a tool's raw stdout/stderr into focused findings
//! and/or cleaned text for surfacing — **presentation-only**.
//!
//! The gate verdict is the process exit code (see [`crate::main`]); nothing here
//! changes it. Distillation fails closed: a structured parser that cannot read
//! its tool's output yields **zero findings plus a visible marker over the raw
//! text**, never a silent "clean" result that could mask a real failure.
//!
//! Two paths, selected by [`OutputSpec`]:
//! - `parser = "<id>"` → parse the tool's JSON (on stdout) into [`Finding`]s.
//! - otherwise → text path: drop noise lines (regex) and optionally strip ANSI.

use regex_lite::Regex;
use serde_json::Value;

use crate::config::OutputSpec;
use crate::report::Finding;

/// Distill a check's captured output per its [`OutputSpec`]. Returns the
/// structured findings (empty on the text path) and the focused text surface.
///
/// Parsers scan the **combined** stdout+stderr: tools disagree on which stream
/// carries the JSON (clippy emits it on stdout, cargo-deny on stderr), and the
/// NDJSON line scanners filter strictly by record shape, so combining is both
/// robust and stream-agnostic. With no spec configured the distilled text is the
/// raw combined output unchanged, preserving the pre-distillation behaviour.
pub fn apply(spec: &OutputSpec, stdout: &str, stderr: &str) -> (Vec<Finding>, String) {
    let combined = format!("{stdout}{stderr}");
    if let Some(parser) = spec.parser.as_deref() {
        return match parse(parser, stdout, stderr) {
            // On success the structured `findings` ARE the surface; the text
            // rendering is produced at display time (see `display_text`), so it
            // is not frozen into `report.json`. Distilled text stays empty here.
            Ok(findings) => (findings, String::new()),
            // Fail closed to raw: surface a loud marker, keep the verdict honest.
            Err(reason) => {
                let raw = clean_text(spec, &combined);
                let marker = format!(
                    "[grizzly-gate: could not parse {parser} output ({reason}); showing raw]\n"
                );
                (Vec::new(), format!("{marker}{raw}"))
            }
        };
    }
    (Vec::new(), clean_text(spec, &combined))
}

/// The text surface for a check, rendered at display time (terminal `FAILURES`
/// block, MCP text fallback). Structured findings render to a YAML-style block;
/// a text-path/fallback check shows its already-distilled text. Keeping this out
/// of `report.json` means the human format is never frozen into the artifact.
pub fn display_text(findings: &[Finding], distilled: &str) -> String {
    if findings.is_empty() {
        distilled.to_string()
    } else {
        render(findings)
    }
}

/// Dispatch to a per-tool structured parser. `Err` carries a short reason used in
/// the fallback marker; an unknown id is a manifest bug, surfaced the same way.
///
/// Two output shapes: cargo/rustc emit newline-delimited JSON (one record per
/// line, on either stream — clippy stdout, cargo-deny stderr — so those scan the
/// combined text), while the scanners emit a single pretty-printed JSON document
/// on stdout, parsed whole.
fn parse(parser: &str, stdout: &str, stderr: &str) -> Result<Vec<Finding>, String> {
    let combined = || format!("{stdout}{stderr}");
    match parser {
        "clippy" => parse_clippy(&combined()),
        "cargo-deny" => parse_cargo_deny(&combined()),
        "semgrep" => parse_semgrep(stdout),
        "trivy" => parse_trivy(stdout),
        "osv-scanner" => parse_osv(stdout),
        "gitleaks" => parse_gitleaks(stdout),
        "eslint" => parse_eslint(stdout),
        other => Err(format!("unknown parser '{other}'")),
    }
}

/// Text path: drop lines matching any `drop` regex, and optionally strip ANSI.
/// Invalid `drop` regexes are skipped (a manifest bug must not crash a run) —
/// they simply filter nothing.
fn clean_text(spec: &OutputSpec, combined: &str) -> String {
    let stripped = if spec.strip_ansi {
        strip_ansi(combined)
    } else {
        combined.to_string()
    };
    if spec.drop.is_empty() {
        return stripped;
    }
    let droppers: Vec<Regex> = spec
        .drop
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();
    if droppers.is_empty() {
        return stripped;
    }
    let kept: Vec<&str> = stripped
        .lines()
        .filter(|line| !droppers.iter().any(|re| re.is_match(line)))
        .collect();
    let mut out = kept.join("\n");
    if stripped.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Remove ANSI escape sequences (CSI/SGR and the like) from `s`. If the static
/// pattern somehow fails to compile, the text passes through unstripped rather
/// than panicking — distillation is presentation-only and must never abort a run.
fn strip_ansi(s: &str) -> String {
    match static_ansi() {
        Some(re) => re.replace_all(s, "").into_owned(),
        None => s.to_string(),
    }
}

/// Compile the ANSI regex once: `\x1b\[` introducer, optional parameter and
/// intermediate bytes, then a final byte in the `@`–`~` range.
fn static_ansi() -> Option<&'static Regex> {
    use std::sync::OnceLock;
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").ok())
        .as_ref()
}

/// Render findings to a YAML-style block, one list item per finding, for the
/// terminal `FAILURES` block and the MCP text fallback. `loc` (combined
/// `file:line:col`) and `rule` are omitted when absent (e.g. cargo-deny
/// advisories carry no source location). This is a human-readable surface, not
/// strict YAML — agents consume the structured `findings` JSON instead.
pub fn render(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let mut blocks: Vec<String> = Vec::with_capacity(findings.len());
    for f in findings {
        // Build only the fields the tool actually provided, in a stable order —
        // a missing severity/loc/rule is omitted (not defaulted), matching the
        // structured `findings` JSON. `message` is always present.
        let mut fields: Vec<(&str, String)> = Vec::new();
        if let Some(sev) = f.severity.as_deref() {
            fields.push(("severity", sev.to_string()));
        }
        match (f.file.as_deref(), f.line, f.col) {
            (Some(file), Some(line), Some(col)) => {
                fields.push(("loc", format!("{file}:{line}:{col}")));
            }
            (Some(file), Some(line), None) => fields.push(("loc", format!("{file}:{line}"))),
            (Some(file), _, _) => fields.push(("loc", file.to_string())),
            (None, _, _) => {}
        }
        if let Some(rule) = f.rule.as_deref() {
            fields.push(("rule", rule.to_string()));
        }
        // First line of the message only; the full text stays in raw `output`.
        fields.push((
            "message",
            f.message.lines().next().unwrap_or("").to_string(),
        ));
        // YAML-style list item: `- ` on the first field, `  ` on the rest.
        let lines: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (k, v))| format!("{} {k}: {v}", if i == 0 { "-" } else { " " }))
            .collect();
        blocks.push(lines.join("\n"));
    }
    let mut out = blocks.join("\n");
    out.push('\n');
    out
}

// --- Per-tool parsers -------------------------------------------------------

/// Clippy / rustc via `cargo ... --message-format=json`: newline-delimited cargo
/// messages. Keep `reason == "compiler-message"` whose level is a real
/// diagnostic (error/warning), mapping the primary span to `file:line:col`.
fn parse_clippy(combined: &str) -> Result<Vec<Finding>, String> {
    let mut findings = Vec::new();
    let mut saw_message = false;
    for line in combined.lines().filter(|l| !l.trim().is_empty()) {
        // cargo interleaves non-JSON lines only on error; tolerate them but
        // require at least one parseable compiler-message overall (below).
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        saw_message = true;
        let Some(msg) = v.get("message") else {
            continue;
        };
        let level = msg.get("level").and_then(Value::as_str).unwrap_or("");
        let Some(severity) = normalize_rustc_level(level) else {
            continue; // skip "help"/"note"-only and final "aborting" summaries
        };
        let rule = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let text = msg
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let primary = msg
            .get("spans")
            .and_then(Value::as_array)
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
            });
        let (file, lineno, col) = match primary {
            Some(s) => (
                s.get("file_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                s.get("line_start")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                s.get("column_start")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
            ),
            None => (None, None, None),
        };
        // Skip rustc's trailing summaries ("aborting due to N previous errors",
        // "N warnings emitted"): they carry neither a lint code nor a source
        // span. Every genuine diagnostic has at least one of the two.
        if rule.is_none() && file.is_none() {
            continue;
        }
        findings.push(Finding {
            file,
            line: lineno,
            col,
            severity: Some(severity.to_string()),
            rule,
            message: text,
        });
    }
    if !saw_message && !combined.trim().is_empty() && !looks_like_json_lines(combined) {
        return Err("no JSON compiler messages found".to_string());
    }
    Ok(findings)
}

/// Map a rustc/clippy `level` onto the normalized severity set, or `None` for
/// non-diagnostic levels we don't surface as findings.
fn normalize_rustc_level(level: &str) -> Option<&'static str> {
    match level {
        "error" | "error: internal compiler error" => Some("error"),
        "warning" => Some("warning"),
        _ => None,
    }
}

/// cargo-deny via `--format json`: newline-delimited diagnostics. Each line is a
/// `{ "type": "diagnostic", "fields": { severity, code, message, … } }` record.
fn parse_cargo_deny(combined: &str) -> Result<Vec<Finding>, String> {
    let mut findings = Vec::new();
    let mut saw_diag = false;
    for line in combined.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("diagnostic") {
            continue;
        }
        let Some(fields) = v.get("fields") else {
            continue;
        };
        saw_diag = true;
        let severity = fields
            .get("severity")
            .and_then(Value::as_str)
            .map(normalize_deny_severity)
            .map(str::to_string);
        let rule = fields
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = fields
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        findings.push(Finding {
            file: None,
            line: None,
            col: None,
            severity,
            rule,
            message,
        });
    }
    if !saw_diag && !combined.trim().is_empty() && !looks_like_json_lines(combined) {
        return Err("no JSON diagnostics found".to_string());
    }
    Ok(findings)
}

/// cargo-deny severities (`error`/`warning`/`note`/`help`) onto the normalized
/// set; unknown values pass through as `note`.
fn normalize_deny_severity(s: &str) -> &'static str {
    match s {
        "error" => "error",
        "warning" => "warning",
        _ => "note",
    }
}

/// Heuristic: does this output look like NDJSON (starts with `{`)? Used to keep
/// the fail-closed marker from firing on genuinely-empty/clean output while
/// still flagging a tool that emitted plain text instead of the JSON we asked
/// for.
fn looks_like_json_lines(s: &str) -> bool {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim_start().starts_with('{'))
}

// --- Single-document scanner parsers ----------------------------------------

/// Parse a scanner's whole-stdout JSON document. Empty stdout is a parse failure
/// (the tool produced no report), surfaced via the fail-closed marker.
fn parse_doc(stdout: &str, tool: &str) -> Result<Value, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!("{tool} produced no JSON on stdout"));
    }
    serde_json::from_str(trimmed).map_err(|e| format!("{tool} JSON parse error: {e}"))
}

/// Borrow `v[key]` as a slice of values, or an empty slice when absent/not an
/// array — lets the parsers walk nested arrays without cloning or unwrapping.
fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map_or(&[][..], |a| a)
}

/// Read a `u64` JSON field down to `u32` (line/column numbers), `None` on absence
/// or overflow.
fn u32_field(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// semgrep `--json`: `results[]` with `path`, `start.{line,col}`, `check_id`, and
/// `extra.{severity,message}`. The gate points `--config` at a directory, so
/// `check_id` is prefixed with the gate's config path — trim it to the rule tail.
fn parse_semgrep(stdout: &str) -> Result<Vec<Finding>, String> {
    let doc = parse_doc(stdout, "semgrep")?;
    let mut findings = Vec::new();
    for r in arr(&doc, "results") {
        let start = r.get("start");
        let extra = r.get("extra");
        findings.push(Finding {
            file: r.get("path").and_then(Value::as_str).map(str::to_string),
            line: start.and_then(|s| u32_field(s, "line")),
            col: start.and_then(|s| u32_field(s, "col")),
            severity: extra
                .and_then(|e| e.get("severity"))
                .and_then(Value::as_str)
                .map(|s| normalize_semgrep_severity(s).to_string()),
            rule: r
                .get("check_id")
                .and_then(Value::as_str)
                .map(shorten_semgrep_rule),
            message: extra
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(findings)
}

/// semgrep `ERROR`/`WARNING`/`INFO` onto the normalized severity set.
fn normalize_semgrep_severity(s: &str) -> &'static str {
    match s {
        "ERROR" => "error",
        "WARNING" => "warning",
        _ => "note",
    }
}

/// Drop the gate's vendored-rules config-path prefix from a semgrep `check_id`,
/// keeping the meaningful rule path (everything after the last `.rules.`).
fn shorten_semgrep_rule(id: &str) -> String {
    id.rsplit_once(".rules.")
        .map_or_else(|| id.to_string(), |(_, tail)| tail.to_string())
}

/// trivy `--format json`: `Results[].Vulnerabilities[]`. One finding per vuln,
/// located at the lockfile (`Target`); no line number. Severity is trivy's
/// CRITICAL/HIGH/… scale, normalized; the package + fix is folded into the message.
fn parse_trivy(stdout: &str) -> Result<Vec<Finding>, String> {
    let doc = parse_doc(stdout, "trivy")?;
    let mut findings = Vec::new();
    for res in arr(&doc, "Results") {
        let target = res.get("Target").and_then(Value::as_str);
        for v in arr(res, "Vulnerabilities") {
            let pkg = v.get("PkgName").and_then(Value::as_str).unwrap_or("");
            let installed = v
                .get("InstalledVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let title = v.get("Title").and_then(Value::as_str).unwrap_or("");
            let fixed = v.get("FixedVersion").and_then(Value::as_str).unwrap_or("");
            let fix_note = if fixed.is_empty() {
                String::new()
            } else {
                format!(" (fix: {fixed})")
            };
            findings.push(Finding {
                file: target.map(str::to_string),
                line: None,
                col: None,
                severity: v
                    .get("Severity")
                    .and_then(Value::as_str)
                    .map(|s| normalize_trivy_severity(s).to_string()),
                rule: v
                    .get("VulnerabilityID")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message: format!("{pkg} {installed}: {title}{fix_note}"),
            });
        }
    }
    Ok(findings)
}

/// trivy CRITICAL/HIGH/MEDIUM/LOW/UNKNOWN onto the normalized set. The gate fails
/// on any vuln (exit code), so this only orders findings for the reader.
fn normalize_trivy_severity(s: &str) -> &'static str {
    match s {
        "CRITICAL" | "HIGH" => "error",
        "MEDIUM" => "warning",
        _ => "note",
    }
}

/// osv-scanner `--format json`: `results[].packages[]`, one finding per advisory
/// `group` (groups collapse aliases of the same vuln). No source location;
/// osv has no error/warning severity, so it's left unset and the CVSS score (when
/// present) rides in the message alongside the package and advisory ids.
fn parse_osv(stdout: &str) -> Result<Vec<Finding>, String> {
    let doc = parse_doc(stdout, "osv-scanner")?;
    let mut findings = Vec::new();
    for res in arr(&doc, "results") {
        for p in arr(res, "packages") {
            let pkg = p.get("package");
            let name = pkg
                .and_then(|x| x.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = pkg
                .and_then(|x| x.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let eco = pkg
                .and_then(|x| x.get("ecosystem"))
                .and_then(Value::as_str)
                .unwrap_or("");
            for g in arr(p, "groups") {
                let ids: Vec<&str> = arr(g, "ids").iter().filter_map(Value::as_str).collect();
                let cvss = g
                    .get("max_severity")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" [CVSS {s}]"))
                    .unwrap_or_default();
                findings.push(Finding {
                    file: None,
                    line: None,
                    col: None,
                    severity: None,
                    rule: ids.first().map(|s| (*s).to_string()),
                    message: format!("{name} {version} ({eco}): {}{cvss}", ids.join(", ")),
                });
            }
        }
    }
    Ok(findings)
}

/// gitleaks `--report-format json`: a top-level array of secret findings, each
/// with `File`/`StartLine`/`StartColumn`, `RuleID`, and `Description`. Secrets
/// have no severity (any leak is blocking); the value is `--redact`ed upstream.
fn parse_gitleaks(stdout: &str) -> Result<Vec<Finding>, String> {
    let doc = parse_doc(stdout, "gitleaks")?;
    let items = doc.as_array().map_or(&[][..], |a| a);
    let mut findings = Vec::new();
    for f in items {
        findings.push(Finding {
            file: f.get("File").and_then(Value::as_str).map(str::to_string),
            line: u32_field(f, "StartLine"),
            col: u32_field(f, "StartColumn"),
            severity: None,
            rule: f.get("RuleID").and_then(Value::as_str).map(str::to_string),
            message: f
                .get("Description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(findings)
}

/// eslint `-f json`: a top-level array of file entries, each with `messages[]`
/// carrying `ruleId`, integer `severity` (1=warn, 2=error), `message`, `line`,
/// `column`. `ruleId` is null for parse/syntax errors — then the rule is omitted.
fn parse_eslint(stdout: &str) -> Result<Vec<Finding>, String> {
    let doc = parse_doc(stdout, "eslint")?;
    let files = doc.as_array().map_or(&[][..], |a| a);
    let mut findings = Vec::new();
    for fe in files {
        let path = fe.get("filePath").and_then(Value::as_str);
        for m in arr(fe, "messages") {
            findings.push(Finding {
                file: path.map(str::to_string),
                line: u32_field(m, "line"),
                col: u32_field(m, "column"),
                severity: m
                    .get("severity")
                    .and_then(Value::as_u64)
                    .map(|s| normalize_eslint_severity(s).to_string()),
                rule: m.get("ruleId").and_then(Value::as_str).map(str::to_string),
                message: m
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }
    Ok(findings)
}

/// eslint integer severity (1=warning, 2=error) onto the normalized set.
fn normalize_eslint_severity(s: u64) -> &'static str {
    match s {
        2 => "error",
        1 => "warning",
        _ => "note",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(parser: &str) -> OutputSpec {
        OutputSpec {
            parser: Some(parser.to_string()),
            drop: Vec::new(),
            strip_ansi: false,
        }
    }

    #[test]
    fn empty_spec_passes_output_through_unchanged() {
        let spec = OutputSpec::default();
        let (findings, text) = apply(&spec, "out\n", "err\n");
        assert!(findings.is_empty());
        assert_eq!(text, "out\nerr\n");
    }

    #[test]
    fn drop_regex_removes_matching_lines() {
        let spec = OutputSpec {
            parser: None,
            drop: vec!["^\\s*Compiling ".into(), "^\\s*Downloading ".into()],
            strip_ansi: false,
        };
        let raw = "   Compiling foo v1.0\n   Downloading bar\nreal: a problem\n";
        let (_f, text) = apply(&spec, raw, "");
        assert_eq!(text, "real: a problem\n");
    }

    #[test]
    fn strip_ansi_removes_escapes() {
        let spec = OutputSpec {
            parser: None,
            drop: Vec::new(),
            strip_ansi: true,
        };
        let raw = "\x1b[31merror\x1b[0m: boom\n";
        let (_f, text) = apply(&spec, raw, "");
        assert_eq!(text, "error: boom\n");
    }

    #[test]
    fn unknown_parser_fails_closed_to_raw_with_marker() {
        let (findings, text) = apply(&spec("nope"), "some real output\n", "");
        assert!(findings.is_empty());
        assert!(text.contains("could not parse nope output"));
        assert!(text.contains("some real output"));
    }

    #[test]
    fn clippy_parses_compiler_message() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"used `unwrap()`","code":{"code":"clippy::unwrap_used"},"spans":[{"is_primary":true,"file_name":"src/main.rs","line_start":42,"column_start":9}]}}"#;
        let other = r#"{"reason":"compiler-artifact","target":{"name":"x"}}"#;
        let stdout = format!("{other}\n{line}\n");
        let (findings, distilled) = apply(&spec("clippy"), &stdout, "compiling noise\n");
        assert_eq!(findings.len(), 1);
        assert!(
            distilled.is_empty(),
            "structured tools store no distilled text"
        );
        let f = findings.first().unwrap();
        assert_eq!(f.file.as_deref(), Some("src/main.rs"));
        assert_eq!(f.line, Some(42));
        assert_eq!(f.col, Some(9));
        assert_eq!(f.severity.as_deref(), Some("error"));
        assert_eq!(f.rule.as_deref(), Some("clippy::unwrap_used"));
        let rendered = render(&findings);
        assert!(rendered.contains("loc: src/main.rs:42:9"), "{rendered}");
        assert!(rendered.contains("rule: clippy::unwrap_used"), "{rendered}");
    }

    #[test]
    fn clippy_clean_run_yields_no_findings() {
        // Only artifact/build messages, no diagnostics — a green clippy run.
        let stdout = "{\"reason\":\"compiler-artifact\"}\n{\"reason\":\"build-finished\",\"success\":true}\n";
        let (findings, _text) = apply(&spec("clippy"), stdout, "");
        assert!(findings.is_empty());
    }

    #[test]
    fn clippy_non_json_output_fails_closed() {
        let (findings, text) = apply(&spec("clippy"), "error[E0599]: no method\n", "");
        assert!(findings.is_empty());
        assert!(text.contains("could not parse clippy output"));
    }

    #[test]
    fn cargo_deny_parses_diagnostic() {
        let line = r#"{"type":"diagnostic","fields":{"severity":"error","code":"vulnerability","message":"RUSTSEC-2024-0001 in foo"}}"#;
        let (findings, _distilled) = apply(&spec("cargo-deny"), &format!("{line}\n"), "");
        assert_eq!(findings.len(), 1);
        let f = findings.first().unwrap();
        assert_eq!(f.severity.as_deref(), Some("error"));
        assert_eq!(f.rule.as_deref(), Some("vulnerability"));
        assert!(render(&findings).contains("RUSTSEC-2024-0001"));
    }

    #[test]
    fn clippy_drops_trailing_summary_message() {
        // rustc's "aborting due to N previous errors" summary has level error but
        // neither a code nor a span — it must not become a finding.
        let summary = r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error","code":null,"spans":[]}}"#;
        let (findings, _t) = apply(&spec("clippy"), &format!("{summary}\n"), "");
        assert!(findings.is_empty(), "summary message must be dropped");
    }

    // --- Real captured tool output (fixtures) -------------------------------

    /// Real `cargo clippy --message-format=json` stdout (an unwrap on a None).
    const CLIPPY_FIXTURE: &str = include_str!("distill/fixtures/clippy.jsonl");
    /// Real `cargo deny --format json check` stderr (a banned crate + summary).
    const CARGO_DENY_FIXTURE: &str = include_str!("distill/fixtures/cargo-deny.jsonl");
    /// Real scanner JSON captured from the gate image against a bad-sample repo.
    const SEMGREP_FIXTURE: &str = include_str!("distill/fixtures/semgrep.json");
    const TRIVY_FIXTURE: &str = include_str!("distill/fixtures/trivy.json");
    const OSV_FIXTURE: &str = include_str!("distill/fixtures/osv-scanner.json");
    const GITLEAKS_FIXTURE: &str = include_str!("distill/fixtures/gitleaks.json");
    const ESLINT_FIXTURE: &str = include_str!("distill/fixtures/eslint.json");

    #[test]
    fn eslint_fixture_parses_and_maps_severity() {
        let (findings, _d) = apply(&spec("eslint"), ESLINT_FIXTURE, "");
        assert!(!findings.is_empty(), "eslint messages yield findings");
        let f = findings.first().unwrap();
        assert!(f.file.is_some() && f.line.is_some(), "located: {f:?}");
        assert_eq!(f.rule.as_deref(), Some("no-var"));
        // eslint severity 2 -> error.
        assert_eq!(f.severity.as_deref(), Some("error"));
    }

    #[test]
    fn semgrep_fixture_parses_and_shortens_rule() {
        let (findings, _d) = apply(&spec("semgrep"), SEMGREP_FIXTURE, "");
        assert!(!findings.is_empty(), "semgrep results yield findings");
        let f = findings.first().unwrap();
        assert!(f.file.is_some() && f.line.is_some(), "located: {f:?}");
        // The gate's config-path prefix is stripped from the rule id.
        assert!(
            f.rule
                .as_deref()
                .is_some_and(|r| !r.contains("etc.grizzly-gate")),
            "rule shortened: {:?}",
            f.rule
        );
        assert!(
            findings
                .iter()
                .any(|c| c.severity.as_deref() == Some("error")),
            "ERROR severity normalized"
        );
    }

    #[test]
    fn trivy_fixture_parses_vulnerabilities() {
        let (findings, _d) = apply(&spec("trivy"), TRIVY_FIXTURE, "");
        assert!(!findings.is_empty(), "trivy vulns yield findings");
        let f = findings.first().unwrap();
        assert!(
            f.rule
                .as_deref()
                .is_some_and(|r| r.starts_with("CVE-") || r.starts_with("GHSA-")),
            "rule is the advisory id: {:?}",
            f.rule
        );
        assert!(f.file.is_some(), "located at the lockfile target");
    }

    #[test]
    fn osv_fixture_parses_groups_without_severity() {
        let (findings, _d) = apply(&spec("osv-scanner"), OSV_FIXTURE, "");
        assert!(!findings.is_empty(), "osv vulns yield findings");
        let f = findings.first().unwrap();
        assert!(f.rule.is_some(), "advisory id present");
        assert!(
            f.severity.is_none(),
            "osv carries no error/warning severity"
        );
        assert!(
            f.message.contains('('),
            "message names the ecosystem: {}",
            f.message
        );
    }

    #[test]
    fn gitleaks_fixture_parses_secrets() {
        let (findings, _d) = apply(&spec("gitleaks"), GITLEAKS_FIXTURE, "");
        assert!(!findings.is_empty(), "gitleaks leaks yield findings");
        let f = findings.first().unwrap();
        assert!(f.file.is_some() && f.line.is_some(), "located: {f:?}");
        assert!(f.rule.is_some(), "the gitleaks RuleID is the rule");
    }

    #[test]
    fn gitleaks_clean_run_is_empty_not_a_failure() {
        // gitleaks writes `[]` on a clean run — valid JSON, zero findings, no marker.
        let (findings, distilled) = apply(&spec("gitleaks"), "[]\n", "");
        assert!(findings.is_empty());
        assert!(
            distilled.is_empty(),
            "clean structured run has no fallback text"
        );
    }

    #[test]
    fn scanner_empty_stdout_fails_closed() {
        // A scanner that emits nothing on stdout (e.g. crashed) must not look clean.
        let (findings, distilled) = apply(&spec("trivy"), "", "panic: boom\n");
        assert!(findings.is_empty());
        assert!(
            distilled.contains("could not parse trivy output"),
            "{distilled}"
        );
        assert!(distilled.contains("panic: boom"), "raw stderr preserved");
    }

    #[test]
    fn clippy_fixture_parses_real_output() {
        let (findings, _distilled) = apply(&spec("clippy"), CLIPPY_FIXTURE, "");
        assert!(!findings.is_empty(), "real clippy output yields findings");
        assert!(
            findings
                .iter()
                .any(|f| f.rule.as_deref() == Some("clippy::unwrap_used")),
            "the unwrap_used lint is captured: {findings:?}"
        );
        assert!(
            findings
                .iter()
                .all(|f| f.file.is_some() && f.line.is_some()),
            "every finding has a source location: {findings:?}"
        );
        assert!(
            render(&findings).contains("loc: src/main.rs"),
            "rendered block cites the file"
        );
    }

    #[test]
    fn cargo_deny_fixture_parses_real_stderr_output() {
        // cargo-deny emits JSON on stderr — verify the combined-stream scan finds
        // it, and the trailing `type:summary` line produces no finding.
        let (findings, _distilled) = apply(&spec("cargo-deny"), "", CARGO_DENY_FIXTURE);
        assert_eq!(
            findings.len(),
            1,
            "one diagnostic, summary excluded: {findings:?}"
        );
        let f = findings.first().unwrap();
        assert_eq!(f.severity.as_deref(), Some("error"));
        assert_eq!(f.rule.as_deref(), Some("banned"));
        assert!(render(&findings).contains("rule: banned"));
    }

    /// Write `distill-samples.txt` at the harness root showing, per tool, the
    /// EXACT bytes each surface emits from real captured tool output: the literal
    /// terminal FAILURES text, and the verbatim agent payload (the JSON string an
    /// MCP `get_check_output` call returns). Both go through the real code paths
    /// (`distill::display_text`, `mcp::findings_payload`) so the review can't
    /// drift from runtime. Ignored by default; run explicitly to regenerate:
    ///   `cargo test -- generate_distill_samples --ignored --nocapture`
    #[test]
    #[ignore = "writes the review artifact on demand, not part of the normal suite"]
    fn generate_distill_samples() {
        use crate::report::Check;

        // (label, cmd, parser, combined-stream fixture)
        let samples: &[(&str, &str, &str, &str)] = &[
            (
                "rust:harness:clippy",
                "cargo clippy --all-targets --all-features --message-format=json -- ...",
                "clippy",
                CLIPPY_FIXTURE,
            ),
            (
                "rust:harness:deny",
                "cargo deny --format json check --config .../deny.toml",
                "cargo-deny",
                CARGO_DENY_FIXTURE,
            ),
            (
                "scan:semgrep",
                "semgrep scan --json --quiet --config .../rules <src>",
                "semgrep",
                SEMGREP_FIXTURE,
            ),
            (
                "scan:trivy-fs",
                "trivy fs --format json --quiet --no-progress --exit-code 1 ... <src>",
                "trivy",
                TRIVY_FIXTURE,
            ),
            (
                "scan:osv-scanner",
                "osv-scanner scan source -r --format json ... <src>",
                "osv-scanner",
                OSV_FIXTURE,
            ),
            (
                "scan:gitleaks",
                "gitleaks detect ... (temp file) --report-format json | cat",
                "gitleaks",
                GITLEAKS_FIXTURE,
            ),
            (
                "node:web:eslint",
                ".../node_modules/.bin/eslint --config ... -f json .",
                "eslint",
                ESLINT_FIXTURE,
            ),
        ];
        let header = "grizzly-gate distilled output — VERBATIM surfaces from real captured tool output.\nFor each check: the literal terminal text, then the exact JSON an agent receives from get_check_output.\n";
        let sections: Vec<String> = samples
            .iter()
            .map(|(label, cmd, parser, fixture)| {
                let (findings, distilled) = apply(&spec(parser), fixture, "");
                let check = Check {
                    label: (*label).to_string(),
                    language: None,
                    project: None,
                    cmd: (*cmd).to_string(),
                    ok: false,
                    exit_code: Some(1),
                    duration_secs: 0.0,
                    findings,
                    distilled,
                    output: (*fixture).to_string(),
                };
                let terminal = display_text(&check.findings, &check.distilled);
                let agent = crate::mcp::findings_payload(&check, None, 0, 200).to_string();
                format!(
                    "\n================ {label} ================\n\n--- terminal (FAILURES block, verbatim) ---\n{terminal}\n--- agent (get_check_output JSON, verbatim) ---\n{agent}\n"
                )
            })
            .collect();
        let out = format!("{header}{}", sections.join(""));
        let path = format!("{}/distill-samples.txt", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(&path, out).unwrap();
        println!("wrote {path}");
    }
}
