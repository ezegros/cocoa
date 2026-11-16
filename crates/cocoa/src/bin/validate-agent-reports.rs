//! Offline, bounded validation of captured agent reports. Never executes captured commands.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, ValueEnum};
use roxmltree::{Document, Node, ParsingOptions};
use serde::{
    Deserializer, Serialize,
    de::{self, DeserializeSeed},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const INPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
const DRAFT_BYTES_MAX: usize = 64 * 1024;
const CHECKS_MAX: usize = 256;
const METRICS: [&str; 5] = ["passed", "failed", "errors", "skipped", "executed"];

#[derive(Parser)]
#[command(
    about = "Validate captured agent reports against a contract and evidence without executing captured commands.",
    args_override_self = true,
    infer_long_args = true
)]
struct Arguments {
    #[arg(long, allow_negative_numbers = true)]
    contract: PathBuf,
    #[arg(long, allow_negative_numbers = true)]
    evidence: PathBuf,
    #[arg(long, allow_negative_numbers = true)]
    draft: PathBuf,
    #[arg(long, value_enum, default_value = "json")]
    format: OutputFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
    Markdown,
}

#[derive(Debug, thiserror::Error)]
enum Problem {
    #[error("{0}")]
    Failed(&'static str),
    #[error("{0}")]
    Incomplete(&'static str),
}

impl Problem {
    fn state(&self) -> &'static str {
        match self {
            Self::Failed(_) => "failed",
            Self::Incomplete(_) => "incomplete",
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Failed(code) | Self::Incomplete(code) => code,
        }
    }
}

#[derive(Default)]
struct Report {
    checks: Vec<Value>,
    states: BTreeMap<&'static str, BTreeSet<&'static str>>,
    omitted_checks: u64,
    facts: BTreeMap<String, Facts>,
    review_required: bool,
    citations: Vec<Value>,
    commentary: String,
    participants: BTreeMap<String, Value>,
}

impl Report {
    fn add(&mut self, scope: &'static str, state: &'static str, code: &str, subject: &str) {
        self.add_details(scope, state, code, subject, Value::Null, Value::Null);
    }

    fn add_details(
        &mut self,
        scope: &'static str,
        state: &'static str,
        code: &str,
        subject: &str,
        expected: Value,
        observed: Value,
    ) {
        self.states.entry(scope).or_default().insert(state);
        if self.checks.len() < CHECKS_MAX {
            self.checks.push(json!({
                "scope": scope, "state": state, "code": code,
                "subject": truncate(subject, 256),
                "expected": detail(expected), "observed": detail(observed),
            }));
        } else {
            self.omitted_checks += 1;
        }
    }

    fn result(&self) -> (u8, Value) {
        let contains = |state| self.states.values().any(|states| states.contains(state));
        let code = if contains("failed") {
            1
        } else if contains("incomplete") {
            2
        } else if self.review_required {
            3
        } else {
            0
        };
        let verdict = |scope| match self.states.get(scope) {
            Some(states) if states.contains("failed") => "failed",
            Some(states) if states.contains("incomplete") => "incomplete",
            Some(states) if !states.is_empty() => "passed",
            _ => "not_checked",
        };
        (
            code,
            json!({
                "schema_version": 1, "exit_code": code,
                "status": match code { 1 => "rejected", 2 => "incomplete", 3 => "review_pending", _ => "passed" },
                "accuracy": verdict("accuracy"), "task": verdict("task"), "evidence": verdict("evidence"),
                "review": if self.review_required { "pending" } else { "not_required" },
                "facts": self.facts, "checks": self.checks, "omitted_checks": self.omitted_checks,
                "citations": self.citations, "unreviewed_commentary": self.commentary,
                "participants": self.participants,
            }),
        )
    }
}

#[derive(Default, Serialize)]
struct Counts {
    passed: u64,
    failed: u64,
    errors: u64,
    skipped: Option<u64>,
    executed: u64,
}

impl Counts {
    fn metric(&self, metric: &str) -> Option<u64> {
        match metric {
            "passed" => Some(self.passed),
            "failed" => Some(self.failed),
            "errors" => Some(self.errors),
            "skipped" => self.skipped,
            "executed" => Some(self.executed),
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct Facts {
    binaries: BTreeMap<String, Counts>,
    totals: Counts,
    skip_reasons: Vec<Value>,
    skip_reasons_omitted: u64,
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let mut report = Report::default();
    let mut inputs = BTreeMap::new();
    for (name, path, limit, schema) in [
        (
            "contract",
            &arguments.contract,
            INPUT_BYTES_MAX,
            contract_schema as fn(&Value) -> Result<(), Problem>,
        ),
        (
            "evidence",
            &arguments.evidence,
            INPUT_BYTES_MAX,
            evidence_schema,
        ),
        ("draft", &arguments.draft, DRAFT_BYTES_MAX, draft_schema),
    ] {
        match read_document(path, limit).and_then(|value| {
            schema(&value)?;
            Ok(value)
        }) {
            Ok(value) => {
                inputs.insert(name, value);
            }
            Err(problem) => report.add(
                if name == "evidence" {
                    "evidence"
                } else {
                    "accuracy"
                },
                problem.state(),
                problem.code(),
                name,
            ),
        }
    }
    if let (Some(contract), Some(evidence)) = (inputs.get("contract"), inputs.get("evidence"))
        && let Err(problem) = validate(contract, evidence, inputs.get("draft"), &mut report)
    {
        report.add("evidence", problem.state(), problem.code(), "inputs");
    }
    let (code, result) = report.result();
    let output = match arguments.format {
        OutputFormat::Json => python_json(&result, true),
        OutputFormat::Markdown => render(&result),
    };
    match writeln!(std::io::stdout().lock(), "{output}") {
        Ok(()) => ExitCode::from(code),
        Err(error) => {
            eprintln!("validate-agent-reports: cannot write report: {error}");
            ExitCode::FAILURE
        }
    }
}

fn validate(
    contract: &Value,
    evidence: &Value,
    draft: Option<&Value>,
    report: &mut Report,
) -> Result<(), Problem> {
    require(contract["run_id"] == evidence["run_id"], "run_mismatch")?;
    validate_participants(contract, evidence, report)?;
    validate_outcomes(contract, evidence, report);
    for (name, expected) in object(&contract["test_runs"]) {
        let Some(captured) = evidence["test_runs"].get(name) else {
            report.add("evidence", "incomplete", "missing_test_run", name);
            continue;
        };
        if captured["participant"] != expected["participant"]
            || captured["command"] != expected["command"]
        {
            report.add("evidence", "failed", "command_mismatch", name);
            continue;
        }
        if captured["complete"] == false {
            report.add("evidence", "incomplete", "capture_incomplete", name);
        }
        let status = captured.get("status");
        if status.is_none_or(|status| status["kind"] == "unknown") {
            report.add("evidence", "incomplete", "process_status_missing", name);
        } else {
            let success =
                status == Some(&json!({"kind": "exited", "code": expected["expected_exit_code"]}));
            report.add("task", matched(success), "process_outcome", name);
        }
        let runner = &captured["runner"];
        if runner["name"] != "cargo-nextest"
            || runner["version"] != "0.9.143"
            || runner["retries"] != 0
            || runner["partitioned"] != false
        {
            report.add("evidence", "incomplete", "unsupported_runner", name);
            continue;
        }
        let text = text_value(&captured["junit"]);
        if text.is_empty() {
            report.add("evidence", "incomplete", "missing_junit", name);
            continue;
        }
        let mut facts = match junit_facts(text, text_value(&expected["report_name"])) {
            Ok(facts) => facts,
            Err(problem) => {
                report.add("evidence", problem.state(), problem.code(), name);
                continue;
            }
        };
        if status == Some(&json!({"kind": "exited", "code": 0}))
            && (facts.totals.failed != 0 || facts.totals.errors != 0)
        {
            report.add("evidence", "failed", "process_artifact_mismatch", name);
        }
        if expected["expected_exit_code"] == 0 {
            let failures = facts.totals.failed + facts.totals.errors;
            report.add_details(
                "task",
                matched(failures == 0),
                "test_case_outcome",
                name,
                json!(0),
                json!(failures),
            );
        }
        if runner["report_skipped"] != "all" {
            report.add("evidence", "incomplete", "skips_unknown", name);
            facts.totals.skipped = None;
            for counts in facts.binaries.values_mut() {
                counts.skipped = None;
            }
        }
        report.facts.insert(name.clone(), facts);
        report.add("evidence", "passed", "junit_cases_checked", name);
    }
    validate_citations(contract, evidence, draft, report);
    let Some(draft) = draft else {
        return Ok(());
    };
    let selection: BTreeSet<_> = array(&draft["test_runs"]).iter().map(text_value).collect();
    let expected: BTreeSet<_> = object(&contract["test_runs"])
        .keys()
        .map(String::as_str)
        .collect();
    report.add(
        "accuracy",
        matched(selection == expected),
        "test_run_selection",
        "draft/test_runs",
    );
    for assertion in array(&draft["assertions"]) {
        let run = text_value(&assertion["test_run"]);
        if contract["test_runs"].get(run).is_none() {
            report.add("accuracy", "failed", "unknown_assertion_run", run);
            continue;
        }
        let Some(facts) = report.facts.get(run) else {
            report.add("accuracy", "incomplete", "assertion_evidence_missing", run);
            continue;
        };
        let binary = text_value(&assertion["binary"]);
        let Some(counts) = facts.binaries.get(binary) else {
            report.add("accuracy", "failed", "unknown_assertion_binary", run);
            continue;
        };
        let metric = text_value(&assertion["metric"]);
        if !METRICS.contains(&metric) {
            report.add("accuracy", "incomplete", "unsupported_assertion", run);
            continue;
        }
        let actual = counts.metric(metric);
        let state = actual.map_or("incomplete", |actual| matched(assertion["value"] == actual));
        report.add_details(
            "accuracy",
            state,
            "assertion_mismatch",
            &format!("{run}/{binary}/{metric}"),
            assertion["value"].clone(),
            json!(actual),
        );
    }
    Ok(())
}

fn contract_schema(value: &Value) -> Result<(), Problem> {
    fields(
        value,
        &[
            "schema_version",
            "run_id",
            "participants",
            "aliases",
            "sources",
            "test_runs",
            "require_citations",
            "require_commentary",
        ],
        &["notes"],
    )?;
    version(value)?;
    identifier(&value["run_id"])?;
    mapping(&value["participants"], 3)?;
    require(
        !object(&value["participants"]).is_empty(),
        "participants_required",
    )?;
    for (name, participant) in object(&value["participants"]) {
        identifier(&json!(name))?;
        fields(
            participant,
            &["cells_max", "answer_chars_max_exclusive", "expected_status"],
            &[],
        )?;
        integer(&participant["cells_max"], 0, 4096)?;
        integer(
            &participant["answer_chars_max_exclusive"],
            1,
            INPUT_BYTES_MAX as u64,
        )?;
        choice(
            &participant["expected_status"],
            &["completed", "failed", "cancelled"],
            "invalid_status",
        )?;
    }
    mapping(&value["aliases"], 32)?;
    for (alias, name) in object(&value["aliases"]) {
        string(&json!(alias), 128, 0)?;
        require(
            name.as_str()
                .is_some_and(|name| value["participants"].get(name).is_some()),
            "unknown_participant",
        )?;
    }
    mapping(&value["sources"], 32)?;
    for (path, digest) in object(&value["sources"]) {
        require(relative_path(path), "invalid_source_path")?;
        require(
            digest.as_str().is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }),
            "invalid_digest",
        )?;
    }
    mapping(&value["test_runs"], 32)?;
    for (name, run) in object(&value["test_runs"]) {
        identifier(&json!(name))?;
        fields(
            run,
            &[
                "participant",
                "command",
                "report_name",
                "expected_exit_code",
            ],
            &[],
        )?;
        require(
            run["participant"]
                .as_str()
                .is_some_and(|name| value["participants"].get(name).is_some()),
            "unknown_participant",
        )?;
        string(&run["command"], 8192, 1)?;
        string(&run["report_name"], 256, 1)?;
        require(
            format!("{}/{name}", text_value(&value["run_id"])) == text_value(&run["report_name"]),
            "report_name_not_run_bound",
        )?;
        integer(&run["expected_exit_code"], 0, 255)?;
    }
    for key in ["require_citations", "require_commentary"] {
        boolean(&value[key])?;
    }
    if let Some(notes) = value.get("notes") {
        mapping(notes, 32)?;
        for (path, name) in object(notes) {
            require(relative_path(path), "invalid_note_path")?;
            require(
                name.as_str()
                    .is_some_and(|name| value["participants"].get(name).is_some()),
                "unknown_participant",
            )?;
        }
    }
    Ok(())
}

fn evidence_schema(value: &Value) -> Result<(), Problem> {
    fields(
        value,
        &[
            "schema_version",
            "run_id",
            "participants",
            "sources",
            "test_runs",
        ],
        &["wait_results", "repeat_results", "saved_outcomes", "notes"],
    )?;
    version(value)?;
    identifier(&value["run_id"])?;
    mapping(&value["participants"], 3)?;
    let mut events = 0;
    for (name, participant) in object(&value["participants"]) {
        identifier(&json!(name))?;
        fields(participant, &["complete", "calls"], &["outcome"])?;
        boolean(&participant["complete"])?;
        sequence(&participant["calls"], 4096)?;
        events += array(&participant["calls"]).len();
        require(events <= 4096, "event_limit")?;
        for call in array(&participant["calls"]) {
            fields(call, &["invocation", "call_id", "code", "state"], &[])?;
            integer(&call["invocation"], 1, u64::MAX)?;
            string(&call["call_id"], 128, 1)?;
            string(&call["code"], INPUT_BYTES_MAX, 0)?;
            choice(
                &call["state"],
                &[
                    "selected",
                    "not_started",
                    "completed",
                    "failed",
                    "interrupted",
                ],
                "invalid_cell_state",
            )?;
        }
        if let Some(outcome) = participant.get("outcome") {
            outcome_schema(outcome)?;
        }
    }
    for (key, code) in [
        ("sources", "invalid_source_path"),
        ("notes", "invalid_note_path"),
    ] {
        if let Some(snapshots) = value.get(key) {
            mapping(snapshots, 32)?;
            for (path, snapshot) in object(snapshots) {
                require(relative_path(path), code)?;
                fields(snapshot, &["text"], &[])?;
                string(&snapshot["text"], INPUT_BYTES_MAX, 0)?;
            }
        }
    }
    mapping(&value["test_runs"], 32)?;
    for (name, run) in object(&value["test_runs"]) {
        identifier(&json!(name))?;
        fields(
            run,
            &["participant", "command", "complete", "runner"],
            &["status", "junit", "stdout", "stderr"],
        )?;
        identifier(&run["participant"])?;
        string(&run["command"], 8192, 1)?;
        boolean(&run["complete"])?;
        fields(
            &run["runner"],
            &["name", "version", "retries", "partitioned"],
            &["report_skipped"],
        )?;
        for key in ["name", "version", "report_skipped"] {
            if let Some(value) = run["runner"].get(key) {
                string(value, 128, 0)?;
            }
        }
        integer(&run["runner"]["retries"], 0, 1_000_000)?;
        boolean(&run["runner"]["partitioned"])?;
        for key in ["junit", "stdout", "stderr"] {
            if let Some(value) = run.get(key) {
                string(value, INPUT_BYTES_MAX, 0)?;
            }
        }
        if let Some(status) = run.get("status") {
            require(status.is_object(), "invalid_process_status")?;
            let kind = text_value(&status["kind"]);
            fields(
                status,
                match kind {
                    "exited" => &["kind", "code"],
                    "signalled" => &["kind", "signal"],
                    _ => &["kind"],
                },
                &[],
            )?;
            choice(
                &status["kind"],
                &["exited", "signalled", "unknown"],
                "invalid_process_status",
            )?;
            match kind {
                "exited" => integer(&status["code"], 0, 255)?,
                "signalled" => integer(&status["signal"], 1, 255)?,
                _ => {}
            }
        }
    }
    for key in ["wait_results", "repeat_results", "saved_outcomes"] {
        if let Some(outcomes) = value.get(key) {
            mapping(outcomes, 32)?;
            for (alias, outcome) in object(outcomes) {
                string(&json!(alias), 128, 0)?;
                outcome_schema(outcome)?;
            }
        }
    }
    Ok(())
}

fn draft_schema(value: &Value) -> Result<(), Problem> {
    fields(
        value,
        &["schema_version", "test_runs", "citations", "commentary"],
        &["assertions"],
    )?;
    version(value)?;
    sequence(&value["test_runs"], 32)?;
    let mut names = BTreeSet::new();
    for name in array(&value["test_runs"]) {
        identifier(name)?;
        names.insert(text_value(name));
    }
    require(
        names.len() == array(&value["test_runs"]).len(),
        "duplicate_test_run_reference",
    )?;
    if let Some(assertions) = value.get("assertions") {
        sequence(assertions, 256)?;
        for assertion in array(assertions) {
            fields(assertion, &["test_run", "binary", "metric", "value"], &[])?;
            identifier(&assertion["test_run"])?;
            string(&assertion["binary"], 256, 1)?;
            string(&assertion["metric"], 128, 1)?;
            integer(&assertion["value"], 0, 1_000_000_000)?;
        }
    }
    sequence(&value["citations"], 128)?;
    for citation in array(&value["citations"]) {
        fields(citation, &["path", "first_line", "last_line", "quote"], &[])?;
        string(&citation["path"], 512, 1)?;
        integer(&citation["first_line"], 0, 1_000_000_000)?;
        integer(&citation["last_line"], 0, 1_000_000_000)?;
        string(&citation["quote"], 8192, 0)?;
    }
    string(&value["commentary"], DRAFT_BYTES_MAX, 0)
}

fn outcome_schema(value: &Value) -> Result<(), Problem> {
    require(value.is_object(), "invalid_outcome")?;
    let status = text_value(&value["status"]);
    choice(
        &value["status"],
        &["completed", "failed", "cancelled", "pending"],
        "invalid_status",
    )?;
    fields(
        value,
        match status {
            "completed" => &["name", "status", "answer"],
            "failed" => &["name", "status", "error"],
            "pending" => &["name", "status", "state"],
            _ => &["name", "status"],
        },
        &[],
    )?;
    identifier(&value["name"])?;
    match status {
        "completed" => string(&value["answer"], INPUT_BYTES_MAX, 0)?,
        "failed" => {
            fields(&value["error"], &["code", "message"], &[])?;
            string(&value["error"]["code"], 128, 1)?;
            string(&value["error"]["message"], INPUT_BYTES_MAX, 0)?;
        }
        "pending" => choice(
            &value["state"],
            &["starting", "running", "cancelling"],
            "invalid_status",
        )?,
        _ => {}
    }
    Ok(())
}

fn fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<(), Problem> {
    require(
        value.as_object().is_some_and(|value| {
            required.iter().all(|name| value.contains_key(*name))
                && value.keys().all(|name| {
                    required.contains(&name.as_str()) || optional.contains(&name.as_str())
                })
        }),
        "invalid_fields",
    )
}

fn version(value: &Value) -> Result<(), Problem> {
    require(
        value["schema_version"].as_u64() == Some(1),
        "unsupported_schema",
    )
}
fn mapping(value: &Value, maximum: usize) -> Result<(), Problem> {
    require(
        value
            .as_object()
            .is_some_and(|value| value.len() <= maximum),
        "map_limit_or_type",
    )
}
fn sequence(value: &Value, maximum: usize) -> Result<(), Problem> {
    require(
        value.as_array().is_some_and(|value| value.len() <= maximum),
        "list_limit_or_type",
    )
}
fn string(value: &Value, maximum: usize, minimum: usize) -> Result<(), Problem> {
    require(
        value
            .as_str()
            .is_some_and(|value| (minimum..=maximum).contains(&value.chars().count())),
        "string_limit_or_type",
    )
}
fn boolean(value: &Value) -> Result<(), Problem> {
    require(value.is_boolean(), "invalid_boolean")
}
fn identifier(value: &Value) -> Result<(), Problem> {
    require(
        value.as_str().is_some_and(|value| {
            (1..=64).contains(&value.len())
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        }),
        "invalid_name",
    )
}
fn integer(value: &Value, minimum: u64, maximum: u64) -> Result<(), Problem> {
    require(
        value
            .as_u64()
            .is_some_and(|value| (minimum..=maximum).contains(&value)),
        "integer_limit_or_type",
    )
}
fn choice(value: &Value, choices: &[&str], code: &'static str) -> Result<(), Problem> {
    require(
        value.as_str().is_some_and(|value| choices.contains(&value)),
        code,
    )
}

fn validate_participants(
    contract: &Value,
    evidence: &Value,
    report: &mut Report,
) -> Result<(), Problem> {
    require(
        object(&evidence["participants"])
            .keys()
            .all(|name| contract["participants"].get(name).is_some()),
        "unexpected_participant",
    )?;
    for (name, expected) in object(&contract["participants"]) {
        let Some(captured) = evidence["participants"].get(name) else {
            report.add("evidence", "incomplete", "missing_participant", name);
            continue;
        };
        if captured["complete"] == false {
            report.add(
                "evidence",
                "incomplete",
                "participant_capture_incomplete",
                name,
            );
        }
        let mut calls: BTreeMap<(u64, &str), &Value> = BTreeMap::new();
        for call in array(&captured["calls"]) {
            let identity = (number(&call["invocation"]), text_value(&call["call_id"]));
            if let Some(previous) = calls.get(&identity) {
                let compatible = previous["state"] == call["state"]
                    || previous["state"] == "selected"
                    || call["state"] == "selected";
                require(
                    previous["code"] == call["code"] && compatible,
                    "conflicting_selection",
                )?;
                if call["state"] == "selected" {
                    continue;
                }
            }
            calls.insert(identity, call);
        }
        let count = |states: &[&str]| {
            calls
                .values()
                .filter(|call| states.contains(&text_value(&call["state"])))
                .count()
        };
        if count(&["selected"]) != 0 {
            report.add("evidence", "incomplete", "cell_settlement_missing", name);
        }
        report.add(
            "task",
            matched(calls.len() as u64 <= number(&expected["cells_max"])),
            "cell_budget",
            name,
        );
        let mut measured = json!({"cells": {
            "selected": calls.len(), "started": count(&["completed", "failed", "interrupted"]),
            "completed": count(&["completed", "failed"]), "failed": count(&["failed"]),
            "interrupted": count(&["interrupted"]), "not_started": count(&["not_started"]),
        }, "answer_chars": null});
        report.participants.insert(name.clone(), measured.clone());
        let Some(outcome) = captured.get("outcome") else {
            report.add("evidence", "incomplete", "missing_outcome", name);
            continue;
        };
        require(outcome["name"] == *name, "outcome_name_mismatch")?;
        report.add(
            "task",
            matched(outcome["status"] == expected["expected_status"]),
            "agent_outcome",
            name,
        );
        if outcome["status"] == "completed" {
            let length = text_value(&outcome["answer"]).chars().count() as u64;
            measured["answer_chars"] = json!(length);
            report.participants.insert(name.clone(), measured);
            report.add(
                "task",
                matched(length < number(&expected["answer_chars_max_exclusive"])),
                "answer_budget",
                name,
            );
        }
    }
    Ok(())
}

fn validate_outcomes(contract: &Value, evidence: &Value, report: &mut Report) {
    for collection in ["wait_results", "repeat_results", "saved_outcomes"] {
        let Some(actual) = evidence.get(collection) else {
            report.add(
                "evidence",
                "incomplete",
                "missing_outcome_capture",
                collection,
            );
            continue;
        };
        let matches = object(actual)
            .keys()
            .eq(object(&contract["aliases"]).keys());
        report.add("accuracy", matched(matches), "outcome_aliases", collection);
        for (alias, name) in object(&contract["aliases"]) {
            let original = evidence["participants"]
                .get(text_value(name))
                .and_then(|participant| participant.get("outcome"));
            let subject = format!("{collection}/{alias}");
            match original {
                None => report.add(
                    "accuracy",
                    "incomplete",
                    "original_outcome_missing",
                    &subject,
                ),
                Some(original) => report.add(
                    "accuracy",
                    matched(actual.get(alias) == Some(original)),
                    "outcome_mismatch",
                    &subject,
                ),
            }
        }
    }
    for (path, name) in object(&contract["notes"]) {
        let note = evidence["notes"].get(path);
        let original = evidence["participants"]
            .get(text_value(name))
            .and_then(|participant| participant.get("outcome"));
        match (note, original) {
            (Some(note), Some(original)) if original["status"] == "completed" => report.add(
                "accuracy",
                matched(note["text"] == original["answer"]),
                "note_answer_mismatch",
                path,
            ),
            (Some(_), Some(_)) => {
                report.add("accuracy", "incomplete", "note_answer_unavailable", path)
            }
            _ => report.add("evidence", "incomplete", "missing_note_evidence", path),
        }
    }
}

fn validate_citations(
    contract: &Value,
    evidence: &Value,
    draft: Option<&Value>,
    report: &mut Report,
) {
    report.review_required = contract["require_commentary"] == true;
    let mut snapshots = BTreeMap::new();
    for (path, digest) in object(&contract["sources"]) {
        let Some(source) = evidence["sources"].get(path) else {
            report.add("evidence", "incomplete", "missing_source", path);
            continue;
        };
        let source = text_value(&source["text"]);
        let captured_digest: String = Sha256::digest(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if captured_digest != text_value(digest) {
            report.add("evidence", "failed", "source_digest_mismatch", path);
            continue;
        }
        snapshots.insert(
            path.as_str(),
            source.split_inclusive('\n').collect::<Vec<_>>(),
        );
        report.add("evidence", "passed", "source_digest_checked", path);
    }
    let Some(draft) = draft else {
        return;
    };
    if contract["require_citations"] == true && array(&draft["citations"]).is_empty() {
        report.add("accuracy", "failed", "citations_required", "draft");
    }
    let mut excerpt_bytes = 0;
    for citation in array(&draft["citations"]) {
        let path = text_value(&citation["path"]);
        let first = number(&citation["first_line"]) as usize;
        let last = number(&citation["last_line"]) as usize;
        if !relative_path(path) || contract["sources"].get(path).is_none() {
            report.add("accuracy", "failed", "source_not_allowed", "citation");
            continue;
        }
        let Some(lines) = snapshots.get(path) else {
            report.add("accuracy", "incomplete", "citation_source_missing", path);
            continue;
        };
        if first < 1 || first > last || last > lines.len() {
            report.add("accuracy", "failed", "citation_bounds", path);
            continue;
        }
        let excerpt = lines[first - 1..last].concat();
        if excerpt.len() > 8192 || excerpt_bytes + excerpt.len() > DRAFT_BYTES_MAX {
            report.add("accuracy", "incomplete", "excerpt_limit", path);
            continue;
        }
        excerpt_bytes += excerpt.len();
        let matches = excerpt == text_value(&citation["quote"]);
        report.add("accuracy", matched(matches), "citation_quote", path);
        report.citations.push(json!({"path": path, "first_line": first, "last_line": last, "excerpt": excerpt, "quote_matches": matches}));
    }
    report.commentary = text_value(&draft["commentary"]).to_owned();
    if report.review_required && report.commentary.chars().all(python_whitespace) {
        report.add("accuracy", "failed", "commentary_required", "draft");
    }
}

fn relative_path(value: &str) -> bool {
    (1..=512).contains(&value.chars().count())
        && !value.contains(['\\', ':', '\0'])
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

fn junit_facts(text: &str, report_name: &str) -> Result<Facts, Problem> {
    require(!text.contains('\0'), "xml_encoding_forbidden")?;
    let upper = text.to_ascii_uppercase();
    for (offset, _) in upper.match_indices("<!") {
        let tail = upper[offset + 2..].trim_start_matches(python_whitespace);
        for declaration in ["DOCTYPE", "ENTITY"] {
            if let Some(rest) = tail.strip_prefix(declaration) {
                require(
                    rest.chars()
                        .next()
                        .is_some_and(|character| character.is_alphanumeric() || character == '_'),
                    "xml_declaration_forbidden",
                )?;
            }
        }
    }
    // Text and comments count as nodes too. The byte ceiling bounds those without
    // reducing the separate element limit for documents containing comments.
    let document = Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: INPUT_BYTES_MAX as u32 + 1,
            ..ParsingOptions::default()
        },
    )
    .map_err(|error| match error {
        roxmltree::Error::NodesLimitReached => Problem::Failed("xml_structure_limit"),
        roxmltree::Error::DtdDetected => Problem::Failed("xml_declaration_forbidden"),
        _ => Problem::Incomplete("incomplete_junit"),
    })?;
    let mut nodes = 0;
    for node in document.descendants().filter(Node::is_element) {
        nodes += 1;
        require(
            nodes <= 65536 && node.ancestors().filter(Node::is_element).count() <= 32,
            "xml_structure_limit",
        )?;
    }
    let root = document.root_element();
    supported_xml(
        tag(root, "testsuites")
            && attributes(
                root,
                &[
                    "name",
                    "tests",
                    "failures",
                    "errors",
                    "skipped",
                    "uuid",
                    "timestamp",
                    "time",
                ],
            ),
    )?;
    require(
        root.attribute("name") == Some(report_name),
        "artifact_run_mismatch",
    )?;
    let mut binaries = BTreeMap::new();
    let mut skip_reasons = Vec::new();
    let mut skipped_count = 0;
    for suite in root.children().filter(Node::is_element) {
        supported_xml(
            tag(suite, "testsuite")
                && attributes(
                    suite,
                    &[
                        "name",
                        "tests",
                        "failures",
                        "errors",
                        "skipped",
                        "timestamp",
                        "time",
                        "hostname",
                    ],
                ),
        )?;
        let name = suite.attribute("name").unwrap_or_default();
        require(
            (1..=256).contains(&name.chars().count()) && !binaries.contains_key(name),
            "duplicate_binary",
        )?;
        let mut counts = Counts {
            skipped: Some(0),
            ..Counts::default()
        };
        let mut names = BTreeSet::new();
        for case in suite.children().filter(Node::is_element) {
            if ["system-out", "system-err", "properties"]
                .iter()
                .any(|name| tag(case, name))
            {
                continue;
            }
            supported_xml(
                tag(case, "testcase")
                    && case.attribute("name").is_some()
                    && attributes(case, &["name", "classname", "timestamp", "time"]),
            )?;
            require(
                case.attribute("classname") == Some(name),
                "testcase_binary_mismatch",
            )?;
            let case_name = case.attribute("name").unwrap_or_default();
            require(names.insert(case_name), "duplicate_testcase")?;
            let results: Vec<_> = case
                .children()
                .filter(Node::is_element)
                .filter(|child| {
                    ["failure", "error", "skipped"]
                        .iter()
                        .any(|name| tag(*child, name))
                })
                .collect();
            require(results.len() <= 1, "conflicting_testcase_results")?;
            supported_xml(case.children().filter(Node::is_element).all(|child| {
                ["failure", "error", "skipped", "system-out", "system-err"]
                    .iter()
                    .any(|name| tag(child, name))
            }))?;
            match results.first() {
                Some(result) if tag(*result, "failure") => counts.failed += 1,
                Some(result) if tag(*result, "error") => counts.errors += 1,
                Some(result) if tag(*result, "skipped") => {
                    counts.skipped = Some(counts.skipped.unwrap_or_default() + 1);
                    skipped_count += 1;
                    if skip_reasons.len() < CHECKS_MAX {
                        skip_reasons.push(json!({"binary": name, "test": truncate(case_name, 512), "reason": skip_reason(*result)}));
                    }
                }
                _ => counts.passed += 1,
            }
        }
        counts.executed = counts.passed + counts.failed + counts.errors;
        check_aggregates(suite, &counts)?;
        binaries.insert(name.to_owned(), counts);
    }
    let mut totals = Counts {
        skipped: Some(0),
        ..Counts::default()
    };
    for counts in binaries.values() {
        totals.passed += counts.passed;
        totals.failed += counts.failed;
        totals.errors += counts.errors;
        totals.executed += counts.executed;
        totals.skipped =
            Some(totals.skipped.unwrap_or_default() + counts.skipped.unwrap_or_default());
    }
    check_aggregates(root, &totals)?;
    let skip_reasons_omitted = skipped_count - skip_reasons.len() as u64;
    Ok(Facts {
        binaries,
        totals,
        skip_reasons,
        skip_reasons_omitted,
    })
}

fn tag(node: Node<'_, '_>, name: &str) -> bool {
    node.tag_name().namespace().is_none() && node.tag_name().name() == name
}
fn attributes(node: Node<'_, '_>, names: &[&str]) -> bool {
    node.attributes()
        .all(|attribute| attribute.namespace().is_none() && names.contains(&attribute.name()))
}

fn skip_reason(node: Node<'_, '_>) -> String {
    if let Some(message) = node.attribute("message") {
        return truncate(message, 512);
    }
    let mut reason = String::new();
    let mut remaining = 512;
    for child in node.children() {
        if child.is_element() || remaining == 0 {
            break;
        }
        // ElementTree discards comments and processing instructions, joining the
        // surrounding leading text. Preserve that captured reason representation.
        if child.is_text()
            && let Some(text) = child.text()
        {
            for character in text.chars().take(remaining) {
                reason.push(character);
                remaining -= 1;
            }
        }
    }
    reason
}

fn check_aggregates(element: Node<'_, '_>, counts: &Counts) -> Result<(), Problem> {
    for (name, value) in [
        (
            "tests",
            counts.executed + counts.skipped.unwrap_or_default(),
        ),
        ("failures", counts.failed),
        ("errors", counts.errors),
        ("skipped", counts.skipped.unwrap_or_default()),
    ] {
        let field = element
            .attribute(name)
            .ok_or(Problem::Incomplete("missing_aggregate"))?;
        require(
            (1..=9).contains(&field.len()) && field.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid_aggregate",
        )?;
        require(
            field.parse::<u64>().ok() == Some(value),
            "aggregate_mismatch",
        )?;
    }
    Ok(())
}

fn supported_xml(condition: bool) -> Result<(), Problem> {
    if condition {
        Ok(())
    } else {
        Err(Problem::Incomplete("unsupported_junit"))
    }
}

fn read_document(path: &Path, limit: usize) -> Result<Value, Problem> {
    #[cfg(unix)]
    let file = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from);
    #[cfg(not(unix))]
    let file: Result<File, std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "regular nofollow inputs require Unix",
    ));
    let file = file.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Problem::Incomplete("missing_input")
        } else {
            Problem::Failed("invalid_json")
        }
    })?;
    require(
        file.metadata()
            .map_err(|_| Problem::Failed("invalid_json"))?
            .is_file(),
        "input_not_regular",
    )?;
    let mut data = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| Problem::Failed("invalid_json"))?;
    require(data.len() <= limit, "input_too_large")?;
    let text = std::str::from_utf8(&data).map_err(|_| Problem::Failed("invalid_json"))?;
    check_json_structure(text)?;
    let (normalized, mut number_problems) = normalize_numbers(text);
    let mut parser = serde_json::Deserializer::from_str(&normalized);
    let value = StrictValueSeed {
        number_problems: &mut number_problems,
    }
    .deserialize(&mut parser)
    .map_err(json_problem)?;
    parser.end().map_err(json_problem)?;
    Ok(value)
}

fn check_json_structure(text: &str) -> Result<(), Problem> {
    let mut depth = 0_i32;
    let mut separators = 0;
    let mut quoted = false;
    let mut escaped = false;
    for character in text.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
        } else {
            if matches!(character, '[' | '{') {
                depth += 1;
            } else if matches!(character, ']' | '}') {
                depth -= 1;
            }
            if matches!(character, '[' | '{' | ':' | ',') {
                separators += 1;
            }
            require(depth <= 32 && separators <= 65536, "json_structure_limit")?;
        }
    }
    Ok(())
}

// Numeric errors must follow parse order, rather than the resource preflight.
// Rejection markers let serde validate surrounding syntax before reporting them.
fn normalize_numbers(text: &str) -> (String, VecDeque<&'static str>) {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut problems = VecDeque::new();
    let mut copied = 0;
    let mut offset = 0;
    let mut quoted = false;
    let mut escaped = false;
    while offset < bytes.len() {
        let byte = bytes[offset];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else {
            let start = offset;
            let constant = [b"NaN".as_slice(), b"Infinity", b"-Infinity"]
                .into_iter()
                .find(|constant| bytes[offset..].starts_with(constant));
            let (end, floating) = number_end(bytes, offset);
            let replacement = if let Some(constant) = constant {
                offset += constant.len();
                problems.push_back("invalid_json_constant");
                Some("0.0")
            } else if end > start {
                offset = end;
                let token = &text[start..offset];
                if floating || token.len() > 20 {
                    problems.push_back(if floating {
                        "invalid_json_constant"
                    } else {
                        "integer_limit_or_type"
                    });
                    Some("0.0")
                } else if token.parse::<i64>().is_err() && token.parse::<u64>().is_err() {
                    // No schema field accepts integers outside serde_json's range.
                    // Null retains the field-specific diagnostic instead of rounding to f64.
                    Some("null")
                } else if token == "-0" {
                    Some("0")
                } else {
                    None
                }
            } else {
                offset += 1;
                continue;
            };
            if let Some(replacement) = replacement {
                output.push_str(&text[copied..start]);
                output.push(' ');
                output.push_str(replacement);
                output.push(' ');
                copied = offset;
            }
            continue;
        }
        offset += 1;
    }
    output.push_str(&text[copied..]);
    (output, problems)
}

fn number_end(bytes: &[u8], start: usize) -> (usize, bool) {
    let mut offset = start;
    if bytes.get(offset) == Some(&b'-') {
        offset += 1;
    }
    if !bytes.get(offset).is_some_and(u8::is_ascii_digit) {
        return (start, false);
    }
    if bytes[offset] == b'0' {
        offset += 1;
    } else {
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
    }
    let mut floating = false;
    if bytes.get(offset) == Some(&b'.') && bytes.get(offset + 1).is_some_and(u8::is_ascii_digit) {
        floating = true;
        offset += 2;
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
    }
    if matches!(bytes.get(offset), Some(b'e' | b'E')) {
        let mut exponent = offset + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        if bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            floating = true;
            offset = exponent + 1;
            while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                offset += 1;
            }
        }
    }
    (offset, floating)
}

struct StrictValueSeed<'a> {
    number_problems: &'a mut VecDeque<&'static str>,
}

impl<'de> DeserializeSeed<'de> for StrictValueSeed<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct Visitor<'a> {
            number_problems: &'a mut VecDeque<&'static str>,
        }
        impl<'de> de::Visitor<'de> for Visitor<'_> {
            type Value = Value;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("strict JSON")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(json!(value))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(json!(value))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(json!(value))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Err(E::custom(
                    self.number_problems
                        .pop_front()
                        .unwrap_or("invalid_json_constant"),
                ))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(json!(value))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(Value::String(value))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = access.next_element_seed(StrictValueSeed {
                    number_problems: self.number_problems,
                })? {
                    values.push(value);
                }
                Ok(Value::Array(values))
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                let mut duplicate = false;
                while let Some(key) = access.next_key::<String>()? {
                    let value = access.next_value_seed(StrictValueSeed {
                        number_problems: self.number_problems,
                    })?;
                    duplicate |= values.insert(key, value).is_some();
                }
                if duplicate {
                    return Err(de::Error::custom("duplicate_json_key"));
                }
                Ok(Value::Object(values))
            }
        }
        deserializer.deserialize_any(Visitor {
            number_problems: self.number_problems,
        })
    }
}

fn json_problem(error: serde_json::Error) -> Problem {
    let message = error.to_string();
    if message.starts_with("duplicate_json_key") {
        Problem::Failed("duplicate_json_key")
    } else if message.starts_with("invalid_json_constant") {
        Problem::Failed("invalid_json_constant")
    } else if message.starts_with("integer_limit_or_type") {
        Problem::Failed("integer_limit_or_type")
    } else {
        Problem::Failed("invalid_json")
    }
}

fn require(condition: bool, code: &'static str) -> Result<(), Problem> {
    if condition {
        Ok(())
    } else {
        Err(Problem::Failed(code))
    }
}
fn matched(condition: bool) -> &'static str {
    if condition { "passed" } else { "failed" }
}
fn text_value(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
fn number(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}
fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}
fn object(value: &Value) -> &Map<String, Value> {
    static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
    value.as_object().unwrap_or(&EMPTY)
}
fn truncate(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}
fn python_whitespace(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

fn detail(value: Value) -> Value {
    let serialized = python_json(&value, false);
    if serialized.len() <= 256 {
        value
    } else {
        json!(format!("{}...", truncate(&serialized, 253)))
    }
}

fn python_json(value: &Value, pretty: bool) -> String {
    let mut output = String::new();
    let formatter = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    let serialized = match formatter {
        Ok(serialized) => serialized,
        Err(error) => unreachable!("JSON values cannot fail serialization: {error}"),
    };
    let mut quoted = false;
    let mut escaped = false;
    for character in serialized.chars() {
        if character as u32 >= 127 {
            for unit in character.encode_utf16(&mut [0; 2]) {
                use std::fmt::Write;
                if let Err(error) = write!(output, "\\u{unit:04x}") {
                    unreachable!("writing to a String cannot fail: {error}");
                }
            }
        } else {
            output.push(character);
            if !pretty && !quoted && matches!(character, ':' | ',') {
                output.push(' ');
            }
        }
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
        }
    }
    output
}

fn render(result: &Value) -> String {
    let mut table = vec![
        "| Run | Binary | Passed | Failed | Errors | Skipped | Executed |".to_owned(),
        "| - | - | -: | -: | -: | -: | -: |".to_owned(),
    ];
    let mut skips = Map::new();
    for (name, facts) in object(&result["facts"]) {
        for (binary, counts) in object(&facts["binaries"]) {
            let label = python_json(&json!(binary), false)
                .replace('|', "\\u007c")
                .replace('`', "\\u0060")
                .replace('<', "\\u003c");
            let metrics = METRICS
                .map(|metric| {
                    if counts[metric].is_null() {
                        "unknown".to_owned()
                    } else {
                        counts[metric].to_string()
                    }
                })
                .join(" | ");
            table.push(format!("| {name} | `{label}` | {metrics} |"));
        }
        skips.insert(name.clone(), json!({"skip_reasons": facts["skip_reasons"], "skip_reasons_omitted": facts["skip_reasons_omitted"]}));
    }
    let checks = json!({"accuracy": result["accuracy"], "task": result["task"], "evidence": result["evidence"], "review": result["review"], "checks": result["checks"], "omitted_checks": result["omitted_checks"]});
    [
        "# Report validation".to_owned(),
        format!("Status: {}", text_value(&result["status"])),
        "## Measured test facts".to_owned(),
        table.join("\n"),
        "## Captured skip reasons".to_owned(),
        literal(&Value::Object(skips)),
        "## Participant budgets".to_owned(),
        literal(&result["participants"]),
        "## Checks".to_owned(),
        literal(&checks),
        "## Captured source excerpts".to_owned(),
        literal(&result["citations"]),
        "## Unreviewed commentary".to_owned(),
        literal(&result["unreviewed_commentary"]),
    ]
    .join("\n\n")
}

fn literal(value: &Value) -> String {
    python_json(value, true)
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
