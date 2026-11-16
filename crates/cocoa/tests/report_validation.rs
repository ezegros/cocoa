//! Exercise the offline report validator through its public command interface.

#[path = "support/process.rs"]
mod process;

use std::{
    error::Error,
    fs,
    path::Path,
    process::{Command, Output},
    time::Duration,
};

use process::CapturedProcess;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[test]
fn binary_counts_and_assertion_details_come_from_runner_cases() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let result = fixture.run(0)?;
    assert_eq!(
        result["facts"]["lifecycle"]["binaries"]["cocoa::agents"]["passed"],
        11
    );
    assert_eq!(
        result["facts"]["lifecycle"]["totals"],
        json!({"passed":15,"failed":0,"errors":0,"skipped":30,"executed":15})
    );
    assert_eq!(
        result["facts"]["lifecycle"]["skip_reasons"][0],
        json!({"binary":"cocoa::cli","test":"case_4","reason":"filter mismatch"})
    );
    fixture.draft["assertions"] = json!([
        {"test_run":"lifecycle","binary":"cocoa::agents","metric":"passed","value":12},
        {"test_run":"lifecycle","binary":"cocoa::cli","metric":"passed","value":3},
    ]);
    let result = fixture.run(1)?;
    let check = checks(&result)?
        .iter()
        .find(|check| check["code"] == "assertion_mismatch")
        .ok_or("missing mismatch")?;
    assert_eq!(check["expected"], 12);
    assert_eq!(check["observed"], 11);
    assert_eq!(result["facts"]["lifecycle"]["totals"]["passed"], 15);
    let markdown = fixture.execute("markdown")?;
    let markdown = String::from_utf8(markdown.stdout)?;
    assert!(markdown.contains("| Passed | Failed | Errors | Skipped | Executed |"));
    assert!(markdown.contains("| 11 | 0 | 0 | 0 | 11 |"));
    Ok(())
}

#[test]
fn runner_failures_unknown_skips_and_incomplete_artifacts_stay_distinct()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.evidence["test_runs"]["lifecycle"]["complete"] = json!(false);
    assert_check(&fixture.run(2)?, "capture_incomplete")?;
    fixture.evidence["test_runs"]["lifecycle"]["complete"] = json!(true);
    fixture.evidence["test_runs"]["lifecycle"]["runner"]["report_skipped"] = json!("none");
    let result = fixture.run(2)?;
    assert!(result["facts"]["lifecycle"]["totals"]["skipped"].is_null());
    assert!(result["facts"]["lifecycle"]["binaries"]["cocoa::agents"]["skipped"].is_null());
    fixture.evidence["test_runs"]["lifecycle"]["runner"]["report_skipped"] = json!("all");
    fixture.evidence["test_runs"]["lifecycle"]["junit"] = json!("<testsuites");
    assert_check(&fixture.run(2)?, "incomplete_junit")?;
    remove(&mut fixture.evidence["test_runs"]["lifecycle"], "junit")?;
    assert_check(&fixture.run(2)?, "missing_junit")?;
    let mut fixture = Fixture::new()?;
    fixture.evidence["test_runs"]["lifecycle"]["junit"] = json!(junit(1, 1, 30));
    fixture.evidence["test_runs"]["lifecycle"]["status"]["code"] = json!(100);
    fixture.draft["assertions"] =
        json!([{ "test_run":"lifecycle","binary":"cocoa::agents","metric":"passed","value":9 }]);
    let result = fixture.run(1)?;
    assert_eq!(result["task"], "failed");
    assert_eq!(result["accuracy"], "passed");
    assert_eq!(
        result["facts"]["lifecycle"]["totals"],
        json!({"passed":13,"failed":1,"errors":1,"skipped":30,"executed":15})
    );
    fixture.evidence["test_runs"]["lifecycle"]["status"]["code"] = json!(0);
    let result = fixture.run(1)?;
    assert_eq!(result["task"], "failed");
    assert_eq!(result["evidence"], "failed");
    assert_check(&result, "process_artifact_mismatch")?;
    Ok(())
}

#[test]
fn citations_keep_exact_snapshot_bytes_and_do_not_approve_commentary() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    fixture.contract["require_citations"] = json!(true);
    fixture.contract["require_commentary"] = json!(true);
    fixture.draft["citations"] = json!([citation(
        "sample.rs",
        2,
        2,
        "Children stop on session closure.\r\n"
    )]);
    fixture.draft["commentary"] =
        json!("Children survive process exit. <script>alert('x')</script>\n# Approved");
    let result = fixture.run(3)?;
    assert_eq!(result["review"], "pending");
    assert_eq!(
        result["citations"][0]["excerpt"],
        "Children stop on session closure.\r\n"
    );
    let markdown = String::from_utf8(fixture.execute("markdown")?.stdout)?;
    assert!(markdown.contains("## Unreviewed commentary"));
    assert!(!markdown.contains("<script>"));
    fixture.draft["citations"][0]["quote"] = json!("Children stop on session closure.\n");
    let result = fixture.run(1)?;
    assert_eq!(
        result["citations"][0]["excerpt"],
        "Children stop on session closure.\r\n"
    );
    assert_eq!(result["citations"][0]["quote_matches"], false);
    for (path, first, last) in [
        ("../sample.rs", 1, 1),
        ("/sample.rs", 1, 1),
        ("other.rs", 1, 1),
        ("sample.rs", 0, 1),
        ("sample.rs", 3, 4),
        ("sample.rs", 2, 1),
    ] {
        fixture.draft["citations"] = json!([citation(path, first, last, "")]);
        fixture.run(1)?;
    }
    fixture.draft["citations"] = json!([citation("sample.rs", 1, 1, "First line.\r\n")]);
    fixture.evidence["sources"]["sample.rs"]["text"] = json!("changed source");
    assert_check(&fixture.run(1)?, "source_digest_mismatch")?;
    for (source, first, last, quote, expected) in [
        ("x\n", 1, 1, "x\n", 3),
        ("x\n", 2, 2, "", 1),
        ("", 1, 1, "", 1),
        ("a\u{2028}b\n", 1, 1, "a\u{2028}b\n", 3),
    ] {
        fixture.source(source);
        fixture.draft["citations"] = json!([citation("sample.rs", first, last, quote)]);
        fixture.run(expected)?;
    }
    Ok(())
}

#[test]
fn selected_cell_budgets_include_not_started_work_and_deduplicate_identity()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    for name in ["root", "reviewer", "tester"] {
        let maximum = fixture.contract["participants"][name]["cells_max"]
            .as_u64()
            .ok_or("missing limit")?;
        let original = fixture.evidence["participants"][name]["calls"][0].clone();
        let mut calls = Vec::new();
        for index in 0..maximum {
            let mut call = original.clone();
            call["call_id"] = json!(index.to_string());
            calls.push(call);
        }
        calls.last_mut().ok_or("no calls")?["state"] = json!("not_started");
        calls.push(calls[0].clone());
        let mut selected = calls[0].clone();
        selected["state"] = json!("selected");
        calls.push(selected);
        fixture.evidence["participants"][name]["calls"] = json!(calls);
    }
    let result = fixture.run(0)?;
    assert_eq!(
        result["participants"]["root"]["cells"],
        json!({"selected":8,"started":7,"completed":7,"failed":0,"interrupted":0,"not_started":1})
    );
    let mut extra = fixture.evidence["participants"]["root"]["calls"][0].clone();
    extra["invocation"] = json!(2);
    fixture.evidence["participants"]["root"]["calls"]
        .as_array_mut()
        .ok_or("not calls")?
        .push(extra);
    assert_eq!(
        fixture.run(1)?["participants"]["root"]["cells"]["selected"],
        9
    );
    Ok(())
}

#[test]
fn full_original_answers_use_unicode_code_points_and_exclusive_limits() -> Result<(), Box<dyn Error>>
{
    for (name, maximum) in [("root", 100), ("reviewer", 1800), ("tester", 1200)] {
        for (answer, exit) in [
            ("😀".repeat(maximum - 1), 0),
            (format!("{} \n", "x".repeat(maximum - 2)), 1),
        ] {
            let mut fixture = Fixture::new()?;
            fixture.answer(name, &answer)?;
            assert_eq!(
                fixture.run(exit)?["participants"][name]["answer_chars"],
                answer.chars().count()
            );
        }
    }
    let mut fixture = Fixture::new()?;
    fixture.answer("root", "a\u{301}😀\r\n ")?;
    assert_eq!(fixture.run(0)?["participants"]["root"]["answer_chars"], 6);
    Ok(())
}

#[test]
fn outcome_aliases_envelopes_and_saved_note_bytes_remain_exact() -> Result<(), Box<dyn Error>> {
    for collection in ["wait_results", "repeat_results", "saved_outcomes"] {
        let mut fixture = Fixture::new()?;
        fixture.evidence[collection]["extra"] = fixture.evidence[collection]["audit"].clone();
        fixture.run(1)?;
        for replacement in [
            None,
            Some(json!({"name":"reviewer","status":"pending","state":"running"})),
            Some(json!({"name":"reviewer","status":"completed","answer":"changed"})),
            Some(
                json!({"name":"reviewer","status":"completed","answer":"reviewer answer","metadata":{"approved":true}}),
            ),
            Some(json!({"name":"other","status":"completed","answer":"reviewer answer"})),
        ] {
            let mut fixture = Fixture::new()?;
            match replacement {
                None => remove(&mut fixture.evidence[collection], "audit")?,
                Some(replacement) => fixture.evidence[collection]["audit"] = replacement,
            }
            fixture.run(1)?;
        }
    }
    let mut fixture = Fixture::new()?;
    fixture.contract["notes"] = json!({"report.md":"reviewer"});
    fixture.evidence["notes"] = json!({"report.md":{"text":"reviewer answer"}});
    fixture.write()?;
    let saved = serde_json::to_string(&fixture.evidence)?;
    let reversed = saved.replace(r#""audit":{"answer":"reviewer answer","name":"reviewer","status":"completed"},"tests":{"answer":"tester answer","name":"tester","status":"completed"}"#,
        r#""tests":{"status":"completed","name":"tester","answer":"tester answer"},"audit":{"status":"completed","name":"reviewer","answer":"reviewer answer"}"#);
    assert_ne!(saved, reversed);
    fs::write(fixture.directory.path().join("evidence.json"), reversed)?;
    fixture.result(0)?;
    fixture.evidence["notes"]["report.md"]["text"] = json!("reviewer answer\n");
    assert_check(&fixture.run(1)?, "note_answer_mismatch")?;
    fixture.evidence["notes"] = json!({});
    assert_check(&fixture.run(2)?, "missing_note_evidence")?;
    Ok(())
}

#[test]
fn strict_json_schemas_reject_malformed_drafts() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    for (field, value, diagnostic) in [
        ("extra", json!(true), "invalid_fields"),
        ("schema_version", json!(true), "unsupported_schema"),
        ("schema_version", json!(2), "unsupported_schema"),
        ("commentary", json!([]), "string_limit_or_type"),
        ("citations", json!([{}]), "invalid_fields"),
        (
            "citations",
            json!(vec![citation("sample.rs", 1, 1, "x"); 129]),
            "list_limit_or_type",
        ),
        (
            "commentary",
            json!("x".repeat(64 * 1024)),
            "input_too_large",
        ),
    ] {
        let mut draft = fixture.draft.clone();
        draft[field] = value;
        fixture.write()?;
        fs::write(
            fixture.directory.path().join("draft.json"),
            serde_json::to_vec(&draft)?,
        )?;
        assert_check(&fixture.result(1)?, diagnostic)?;
    }
    let draft = serde_json::to_string(&fixture.draft)?;
    let malformed = [
        (
            br#"{"schema_version":1,"schema_version":1}"#.to_vec(),
            "duplicate_json_key",
        ),
        (format!("{draft} trailing").into_bytes(), "invalid_json"),
        (
            format!("```json\n{draft}\n```").into_bytes(),
            "invalid_json",
        ),
        (vec![0xff], "invalid_json"),
        (
            br#"{"schema_version":NaN}"#.to_vec(),
            "invalid_json_constant",
        ),
        (
            format!(r#"{},"other":"\ud800"}}"#, &draft[..draft.len() - 1]).into_bytes(),
            "invalid_json",
        ),
        (
            format!("{}{}", "[".repeat(64), "]".repeat(64)).into_bytes(),
            "json_structure_limit",
        ),
    ];
    for (bytes, diagnostic) in malformed {
        fixture.write()?;
        fs::write(fixture.directory.path().join("draft.json"), bytes)?;
        assert_check(&fixture.result(1)?, diagnostic)?;
    }
    Ok(())
}

#[test]
fn rejected_incomplete_and_review_precedence_survives_omitted_checks() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    remove(&mut fixture.evidence["test_runs"]["lifecycle"], "junit")?;
    fixture.evidence["sources"] = json!({});
    fixture.draft["extra"] = json!(true);
    let result = fixture.run(1)?;
    assert_eq!(result["evidence"], "incomplete");
    assert_check(&result, "missing_junit")?;
    assert_check(&result, "missing_source")?;
    remove(&mut fixture.draft, "extra")?;
    fixture.contract["require_commentary"] = json!(true);
    fixture.draft["commentary"] = json!("This interpretation needs review.");
    assert_eq!(fixture.run(2)?["review"], "pending");
    for field in ["participants", "sources"] {
        let mut fixture = Fixture::new()?;
        if field == "participants" {
            fixture.evidence[field]["extra"] = fixture.evidence[field]["root"].clone();
        } else {
            fixture.evidence[field] = json!({"../escape":{"text":"x"}});
        }
        fixture.run(1)?;
    }
    let mut fixture = Fixture::new()?;
    let assertion =
        json!({"test_run":"lifecycle","binary":"cocoa::agents","metric":"passed","value":11});
    let mut assertions = vec![assertion; 256];
    assertions[255]["value"] = json!(12);
    fixture.draft["assertions"] = json!(assertions);
    let result = fixture.run(1)?;
    assert_eq!(checks(&result)?.len(), 256);
    assert!(
        result["omitted_checks"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_eq!(result["accuracy"], "failed");
    Ok(())
}

#[test]
fn process_termination_and_runner_policy_are_not_inferred_from_artifacts()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    remove(&mut fixture.evidence["test_runs"]["lifecycle"], "status")?;
    assert_check(&fixture.run(2)?, "process_status_missing")?;
    for (status, expected) in [
        (json!({"kind":"unknown"}), 2),
        (json!({"kind":"signalled","signal":15}), 1),
    ] {
        fixture.evidence["test_runs"]["lifecycle"]["status"] = status;
        fixture.run(expected)?;
    }
    fixture.evidence["test_runs"]["lifecycle"]["status"] = json!({"kind":"exited","code":100});
    remove(&mut fixture.evidence["test_runs"]["lifecycle"], "junit")?;
    let result = fixture.run(1)?;
    assert_eq!(result["evidence"], "incomplete");
    assert_eq!(result["task"], "failed");
    for (key, value) in [
        ("retries", json!(1)),
        ("partitioned", json!(true)),
        ("version", json!("0.9.144")),
        ("name", json!("other")),
    ] {
        let mut fixture = Fixture::new()?;
        fixture.evidence["test_runs"]["lifecycle"]["runner"][key] = value;
        let result = fixture.run(2)?;
        assert_eq!(result["facts"], json!({}));
        assert_check(&result, "unsupported_runner")?;
    }
    Ok(())
}

#[test]
fn junit_checks_attribution_aggregates_retry_shapes_and_declarations() -> Result<(), Box<dyn Error>>
{
    let original = junit(0, 0, 30);
    let first = r#"<testcase name="case_0" classname="cocoa::agents"></testcase>"#;
    for (xml, exit, diagnostic) in [
        (
            original.replacen(first, &first.repeat(2), 1),
            1,
            "duplicate_testcase",
        ),
        (
            original.replacen(
                r#"classname="cocoa::agents""#,
                r#"classname="cocoa::cli""#,
                1,
            ),
            1,
            "testcase_binary_mismatch",
        ),
        (
            original.replacen(r#"tests="11""#, r#"tests="12""#, 1),
            1,
            "aggregate_mismatch",
        ),
        (
            original.replace("case-1/lifecycle", "another-run/lifecycle"),
            1,
            "artifact_run_mismatch",
        ),
        (
            original.replacen(
                first,
                &first.replace("</testcase>", "<flakyFailure/></testcase>"),
                1,
            ),
            2,
            "unsupported_junit",
        ),
        (
            original.replacen("<testcase ", "<testcase retries=\"1\" ", 1),
            2,
            "unsupported_junit",
        ),
        (
            original.replacen(" skipped=\"30\"", "", 1),
            2,
            "missing_aggregate",
        ),
        (
            format!(
                r#"<!DOCTYPE testsuites [<!ENTITY data SYSTEM "file:///etc/passwd">]>{original}"#
            ),
            1,
            "xml_declaration_forbidden",
        ),
        (
            format!(r#"<!DOCTYPE testsuites [<!ENTITY unused "value">]>{original}"#)
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .map(char::from)
                .collect(),
            1,
            "xml_encoding_forbidden",
        ),
        (
            original.replacen(
                first,
                &first.replace("</testcase>", "<failure/><error/></testcase>"),
                1,
            ),
            1,
            "conflicting_testcase_results",
        ),
        (
            original.replacen(r#"tests="45""#, r#"tests="+45""#, 1),
            1,
            "invalid_aggregate",
        ),
        (
            original.replacen(r#"name="cocoa::cli""#, r#"name="cocoa::agents""#, 1),
            1,
            "duplicate_binary",
        ),
    ] {
        let mut fixture = Fixture::new()?;
        fixture.evidence["test_runs"]["lifecycle"]["junit"] = json!(xml);
        assert_check(&fixture.run(exit)?, diagnostic)?;
    }
    Ok(())
}

#[test]
fn command_selection_and_assertion_targets_are_checked() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.contract["test_runs"]["other"] = fixture.contract["test_runs"]["lifecycle"].clone();
    fixture.contract["test_runs"]["other"]["report_name"] = json!("case-1/other");
    fixture.evidence["test_runs"]["lifecycle"]["command"] = json!("different command");
    fixture.draft["test_runs"] = json!(["lifecycle", "other"]);
    let result = fixture.run(1)?;
    assert_eq!(result["evidence"], "failed");
    assert_check(&result, "missing_test_run")?;
    assert_check(&result, "command_mismatch")?;
    for (run, binary, metric, exit, diagnostic) in [
        (
            "lifecycle",
            "cocoa::agents",
            "unsupported",
            2,
            "unsupported_assertion",
        ),
        (
            "unknown",
            "cocoa::agents",
            "unsupported",
            1,
            "unknown_assertion_run",
        ),
        (
            "lifecycle",
            "unknown",
            "unsupported",
            1,
            "unknown_assertion_binary",
        ),
    ] {
        let mut fixture = Fixture::new()?;
        fixture.draft["assertions"] =
            json!([{ "test_run":run,"binary":binary,"metric":metric,"value":1 }]);
        assert_check(&fixture.run(exit)?, diagnostic)?;
    }
    Ok(())
}

#[test]
fn structure_event_and_file_byte_limits_are_bounded() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let properties = format!(
        "<properties>{}{}</properties>",
        "<property>".repeat(40),
        "</property>".repeat(40)
    );
    fixture.evidence["test_runs"]["lifecycle"]["junit"] =
        json!(junit(0, 0, 30).replacen("</testsuite>", &format!("{properties}</testsuite>"), 1));
    assert_check(&fixture.run(1)?, "xml_structure_limit")?;
    let mut fixture = Fixture::new()?;
    let call = fixture.evidence["participants"]["root"]["calls"][0].clone();
    fixture.evidence["participants"]["root"]["calls"] = json!(vec![call.clone(); 4094]);
    assert_eq!(
        fixture.run(0)?["participants"]["root"]["cells"]["selected"],
        1
    );
    fixture.evidence["participants"]["root"]["calls"]
        .as_array_mut()
        .ok_or("not calls")?
        .push(call);
    assert_check(&fixture.run(1)?, "event_limit")?;
    let fixture = Fixture::new()?;
    for (key, maximum) in [
        ("draft", 64 * 1024),
        ("evidence", 4 * 1024 * 1024),
        ("contract", 4 * 1024 * 1024),
    ] {
        for (size, expected) in [(maximum, 0), (maximum + 1, 1)] {
            fixture.write()?;
            let path = fixture.directory.path().join(format!("{key}.json"));
            let mut bytes = fs::read(&path)?;
            bytes.resize(size, b' ');
            fs::write(path, bytes)?;
            let result = fixture.result(expected)?;
            if expected == 1 {
                assert_check(&result, "input_too_large")?;
            }
        }
    }
    Ok(())
}

#[test]
fn offline_validation_is_deterministic_and_never_mutates_or_executes_inputs()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let sentinel = fixture.directory.path().join("must-not-exist");
    let command = format!("touch {}", sentinel.display());
    fixture.contract["test_runs"]["lifecycle"]["command"] = json!(command);
    fixture.evidence["test_runs"]["lifecycle"]["command"] =
        fixture.contract["test_runs"]["lifecycle"]["command"].clone();
    fixture.run(0)?;
    let paths = ["contract.json", "evidence.json", "draft.json"];
    let before = paths
        .map(|name| fs::read(fixture.directory.path().join(name)))
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let first = fixture.execute("json")?;
    let second = fixture.execute("json")?;
    assert!(first.status.success() && second.status.success());
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        fixture.execute("markdown")?.stdout,
        fixture.execute("markdown")?.stdout
    );
    for (name, bytes) in paths.iter().zip(before) {
        assert_eq!(fs::read(fixture.directory.path().join(name))?, bytes);
    }
    assert!(!sentinel.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn input_files_reject_leaf_symlinks_directories_and_nonblocking_fifos() -> Result<(), Box<dyn Error>>
{
    let fixture = Fixture::new()?;
    fixture.write()?;
    let evidence = fixture.directory.path().join("evidence.json");
    let packet = fixture.directory.path().join("packet.json");
    fs::rename(&evidence, &packet)?;
    std::os::unix::fs::symlink(&packet, &evidence)?;
    assert_check(&fixture.result(1)?, "invalid_json")?;
    fs::remove_file(&evidence)?;
    assert_check(&fixture.result(2)?, "missing_input")?;
    fs::create_dir(&evidence)?;
    assert_check(&fixture.result(1)?, "input_not_regular")?;
    fs::remove_dir(&evidence)?;
    let output = CapturedProcess::spawn(Command::new("mkfifo").arg(&evidence))?
        .finish(Duration::from_secs(5))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_check(&fixture.result(1)?, "input_not_regular")?;
    Ok(())
}

#[test]
fn invalid_numbers_surrogates_and_duplicate_keys_have_specific_diagnostics()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    for (raw, diagnostic) in [
        ("0.5", "invalid_json_constant"),
        ("1e999", "invalid_json_constant"),
        ("Infinity", "invalid_json_constant"),
        ("-Infinity", "invalid_json_constant"),
        ("123456789012345678901", "integer_limit_or_type"),
        ("18446744073709551616", "unsupported_schema"),
        ("-9999999999999999999", "unsupported_schema"),
        ("1-0", "invalid_json"),
        ("1NaN", "invalid_json"),
        ("-NaN", "invalid_json"),
        ("--Infinity", "invalid_json"),
        ("01", "invalid_json"),
        ("1.", "invalid_json"),
    ] {
        fixture.write()?;
        fs::write(
            fixture.directory.path().join("draft.json"),
            format!(r#"{{"schema_version":{raw},"test_runs":[],"citations":[],"commentary":""}}"#),
        )?;
        assert_check(&fixture.result(1)?, diagnostic)?;
    }
    for raw in [r#"{"outer":{"same":1,"same":2}}"#, r#"{"a":1,"\u0061":2}"#] {
        fixture.write()?;
        fs::write(fixture.directory.path().join("draft.json"), raw)?;
        assert_check(&fixture.result(1)?, "duplicate_json_key")?;
    }
    for raw in [r#"{"\udfff":1}"#, r#"{"a":"\ud800"}"#] {
        fixture.write()?;
        fs::write(fixture.directory.path().join("draft.json"), raw)?;
        assert_check(&fixture.result(1)?, "invalid_json")?;
    }
    let mut fixture = Fixture::new()?;
    fixture.evidence["participants"]["root"]["calls"][0]["invocation"] = json!(u64::MAX);
    fixture.run(0)?;
    let malformed =
        serde_json::to_string(&fixture.contract)?.replace("\"cells_max\":8", "\"cells_max\":8-0");
    fs::write(fixture.directory.path().join("contract.json"), malformed)?;
    assert_check(&fixture.result(1)?, "invalid_json")?;
    Ok(())
}

#[test]
fn xml_elements_depth_and_skip_reason_limits_keep_their_exact_boundaries()
-> Result<(), Box<dyn Error>> {
    for (depth, exit) in [(29, 0), (30, 1)] {
        let mut fixture = Fixture::new()?;
        let properties = format!(
            "<properties>{}{}</properties>",
            "<property>".repeat(depth),
            "</property>".repeat(depth)
        );
        fixture.evidence["test_runs"]["lifecycle"]["junit"] =
            json!(junit(0, 0, 0).replacen("</testsuite>", &format!("{properties}</testsuite>"), 1));
        let result = fixture.run(exit)?;
        if exit == 1 {
            assert_check(&result, "xml_structure_limit")?;
        }
    }
    for (elements, exit) in [(65536, 0), (65537, 1)] {
        let mut fixture = Fixture::new()?;
        let properties = format!(
            "<properties>{}</properties>",
            "<property/>".repeat(elements - 19)
        );
        fixture.evidence["test_runs"]["lifecycle"]["junit"] =
            json!(junit(0, 0, 0).replacen("</testsuite>", &format!("{properties}</testsuite>"), 1));
        let result = fixture.run(exit)?;
        if exit == 1 {
            assert_check(&result, "xml_structure_limit")?;
        }
    }
    let mut fixture = Fixture::new()?;
    fixture.evidence["test_runs"]["lifecycle"]["junit"] =
        json!(junit(0, 0, 300).replace("filter mismatch", &"😀".repeat(600)));
    let result = fixture.run(0)?;
    let reasons = result["facts"]["lifecycle"]["skip_reasons"]
        .as_array()
        .ok_or("not reasons")?;
    assert_eq!(reasons.len(), 256);
    assert_eq!(result["facts"]["lifecycle"]["skip_reasons_omitted"], 44);
    assert_eq!(
        reasons[0]["reason"]
            .as_str()
            .ok_or("not reason")?
            .chars()
            .count(),
        512
    );
    Ok(())
}

#[test]
fn excerpt_limits_use_utf8_bytes_and_keep_omission_explicit() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let source = "😀".repeat(2048);
    fixture.source(&source);
    fixture.draft["citations"] = json!([citation("sample.rs", 1, 1, &source)]);
    assert_eq!(fixture.run(0)?["citations"][0]["excerpt"], source);
    fixture.source(&format!("{source}x"));
    assert_check(&fixture.run(2)?, "excerpt_limit")?;
    fixture.source(&source);
    fixture.draft["citations"] = json!(vec![citation("sample.rs", 1, 1, ""); 9]);
    let result = fixture.run(1)?;
    assert_eq!(
        result["citations"].as_array().ok_or("not citations")?.len(),
        8
    );
    assert_check(&result, "excerpt_limit")?;
    Ok(())
}

#[test]
fn json_structural_separators_are_counted_outside_strings() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.answer("root", &"[{:,\\\"".repeat(10))?;
    fixture.run(0)?;
    for (items, diagnostic) in [(65534, "invalid_fields"), (65535, "json_structure_limit")] {
        fixture.write()?;
        let raw = format!("{{\"items\":[{}0]}}", "0,".repeat(items - 1));
        fs::write(fixture.directory.path().join("evidence.json"), raw)?;
        assert_check(&fixture.result(1)?, diagnostic)?;
    }
    Ok(())
}

#[test]
fn selection_settlement_and_conflicts_are_not_inferred_from_code() -> Result<(), Box<dyn Error>> {
    for (state, completed, failed, interrupted) in [
        ("completed", 1, 0, 0),
        ("failed", 1, 1, 0),
        ("interrupted", 0, 0, 1),
        ("not_started", 0, 0, 0),
    ] {
        let mut fixture = Fixture::new()?;
        let mut selected = fixture.evidence["participants"]["root"]["calls"][0].clone();
        selected["state"] = json!("selected");
        let mut settled = selected.clone();
        settled["state"] = json!(state);
        fixture.evidence["participants"]["root"]["calls"] = json!([selected, settled]);
        let result = fixture.run(0)?;
        assert_eq!(result["participants"]["root"]["cells"]["selected"], 1);
        assert_eq!(
            result["participants"]["root"]["cells"]["completed"],
            completed
        );
        assert_eq!(result["participants"]["root"]["cells"]["failed"], failed);
        assert_eq!(
            result["participants"]["root"]["cells"]["interrupted"],
            interrupted
        );
    }
    for (key, changed) in [
        ("state", json!("failed")),
        ("code", json!("different code")),
    ] {
        let mut fixture = Fixture::new()?;
        let original = fixture.evidence["participants"]["root"]["calls"][0].clone();
        let mut conflicting = original.clone();
        conflicting[key] = changed;
        fixture.evidence["participants"]["root"]["calls"] = json!([original, conflicting]);
        assert_check(&fixture.run(1)?, "conflicting_selection")?;
    }
    let mut fixture = Fixture::new()?;
    fixture.evidence["participants"]["root"]["calls"][0]["state"] = json!("selected");
    assert_check(&fixture.run(2)?, "cell_settlement_missing")?;
    Ok(())
}

#[test]
fn output_escapes_unicode_and_markdown_labels_without_losing_original_text()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.draft["commentary"] = json!("😀\u{7f}<script>&\n```\na\u{2028}b");
    fixture.evidence["test_runs"]["lifecycle"]["junit"] =
        json!(junit(0, 0, 30).replace("cocoa::agents", "🦀|`&lt;&amp;"));
    fixture.run(0)?;
    let output = fixture.execute("json")?;
    assert!(output.stdout.is_ascii());
    let output = String::from_utf8(output.stdout)?;
    assert!(output.contains("\\ud83d\\ude00\\u007f"));
    let markdown = String::from_utf8(fixture.execute("markdown")?.stdout)?;
    assert!(markdown.contains("\\ud83e\\udd80\\u007c\\u0060\\u003c&"));
    assert!(!markdown.contains("<script>"));
    assert!(markdown.contains("\\u003cscript\\u003e\\u0026"));
    Ok(())
}

#[test]
fn skip_reasons_join_leading_text_around_comments_and_processing_instructions()
-> Result<(), Box<dyn Error>> {
    for (xml, reason) in [
        ("<skipped>text</skipped>", "text"),
        ("<skipped><!--ignored-->text</skipped>", "text"),
        (
            "<skipped>before<!--ignored-->after</skipped>",
            "beforeafter",
        ),
        (
            "<skipped>before<?pi ignored?>after<child/>tail</skipped>",
            "beforeafter",
        ),
        ("<skipped message=\"\">text</skipped>", ""),
        ("<skipped><![CDATA[a]]>b</skipped>", "ab"),
    ] {
        let mut fixture = Fixture::new()?;
        fixture.evidence["test_runs"]["lifecycle"]["junit"] =
            json!(junit(0, 0, 30).replace("<skipped message=\"filter mismatch\"/>", xml));
        assert_eq!(
            fixture.run(0)?["facts"]["lifecycle"]["skip_reasons"][0]["reason"],
            reason
        );
    }
    Ok(())
}

#[test]
fn usage_errors_use_stderr_and_exit_two_without_a_report() -> Result<(), Box<dyn Error>> {
    for arguments in [vec![], vec!["--format", "other"], vec!["--unknown"]] {
        let output = CapturedProcess::spawn(
            Command::new(env!("CARGO_BIN_EXE_validate-agent-reports")).args(arguments),
        )?
        .finish(Duration::from_secs(5))?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    let fixture = Fixture::new()?;
    fixture.write()?;
    fs::rename(
        fixture.directory.path().join("contract.json"),
        fixture.directory.path().join("-1"),
    )?;
    let output = CapturedProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_validate-agent-reports"))
            .current_dir(fixture.directory.path())
            .args([
                "--contr",
                "-1",
                "--ev",
                "missing.json",
                "--evidence",
                "evidence.json",
                "--dr",
                "draft.json",
            ]),
    )?
    .finish(Duration::from_secs(5))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["exit_code"],
        0
    );
    Ok(())
}

struct Fixture {
    directory: tempfile::TempDir,
    contract: Value,
    evidence: Value,
    draft: Value,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let source = "First line.\r\nChildren stop on session closure.\r\nLast line.";
        let contract = json!({
            "schema_version":1,"run_id":"case-1",
            "participants":{
                "root":{"cells_max":8,"answer_chars_max_exclusive":100,"expected_status":"completed"},
                "reviewer":{"cells_max":5,"answer_chars_max_exclusive":1800,"expected_status":"completed"},
                "tester":{"cells_max":5,"answer_chars_max_exclusive":1200,"expected_status":"completed"},
            },
            "aliases":{"audit":"reviewer","tests":"tester"},
            "sources":{"sample.rs":digest(source)},
            "test_runs":{"lifecycle":{"participant":"tester","command":"./scripts/test --fixed-filter","report_name":"case-1/lifecycle","expected_exit_code":0}},
            "require_citations":false,"require_commentary":false,
        });
        let mut participants = serde_json::Map::new();
        for name in ["root", "reviewer", "tester"] {
            participants.insert(name.to_owned(),json!({"complete":true,
                "outcome":{"name":name,"status":"completed","answer":format!("{name} answer")},
                "calls":[{"invocation":1,"call_id":"first","code":"print('cocoa.spawn(')","state":"completed"}],
            }));
        }
        let outcomes = json!({"audit":participants["reviewer"]["outcome"],"tests":participants["tester"]["outcome"]});
        let evidence = json!({"schema_version":1,"run_id":"case-1","participants":participants,
            "sources":{"sample.rs":{"text":source}},
            "test_runs":{"lifecycle":{"participant":"tester","command":"./scripts/test --fixed-filter","complete":true,
                "status":{"kind":"exited","code":0},
                "runner":{"name":"cargo-nextest","version":"0.9.143","report_skipped":"all","retries":0,"partitioned":false},
                "junit":junit(0,0,30),"stdout":"runner output","stderr":"",
            }},"wait_results":outcomes,"repeat_results":outcomes,"saved_outcomes":outcomes,
        });
        Ok(Self {
            directory: tempfile::tempdir()?,
            contract,
            evidence,
            draft: json!({"schema_version":1,"test_runs":["lifecycle"],"assertions":[],"citations":[],"commentary":""}),
        })
    }

    fn write(&self) -> Result<(), Box<dyn Error>> {
        for (name, value) in [
            ("contract", &self.contract),
            ("evidence", &self.evidence),
            ("draft", &self.draft),
        ] {
            fs::write(
                self.directory.path().join(format!("{name}.json")),
                serde_json::to_vec(value)?,
            )?;
        }
        Ok(())
    }

    fn run(&self, expected: i32) -> Result<Value, Box<dyn Error>> {
        self.write()?;
        self.result(expected)
    }

    fn result(&self, expected: i32) -> Result<Value, Box<dyn Error>> {
        let output = self.execute("json")?;
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let result: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["exit_code"], expected);
        Ok(result)
    }

    fn execute(&self, format: &str) -> Result<Output, Box<dyn Error>> {
        execute(self.directory.path(), format)
    }

    fn source(&mut self, source: &str) {
        self.evidence["sources"]["sample.rs"]["text"] = json!(source);
        self.contract["sources"]["sample.rs"] = json!(digest(source));
    }

    fn answer(&mut self, name: &str, answer: &str) -> Result<(), Box<dyn Error>> {
        self.evidence["participants"][name]["outcome"]["answer"] = json!(answer);
        for collection in ["wait_results", "repeat_results", "saved_outcomes"] {
            for outcome in self.evidence[collection]
                .as_object_mut()
                .ok_or("not outcomes")?
                .values_mut()
            {
                if outcome["name"] == name {
                    outcome["answer"] = json!(answer);
                }
            }
        }
        Ok(())
    }
}

fn execute(directory: &Path, format: &str) -> Result<Output, Box<dyn Error>> {
    let output = CapturedProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_validate-agent-reports"))
            .arg("--contract")
            .arg(directory.join("contract.json"))
            .arg("--evidence")
            .arg(directory.join("evidence.json"))
            .arg("--draft")
            .arg(directory.join("draft.json"))
            .args(["--format", format]),
    )?
    .finish(Duration::from_secs(5))?;
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn checks(result: &Value) -> Result<&Vec<Value>, Box<dyn Error>> {
    result["checks"]
        .as_array()
        .ok_or_else(|| "not checks".into())
}
fn assert_check(result: &Value, diagnostic: &str) -> Result<(), Box<dyn Error>> {
    assert!(
        checks(result)?
            .iter()
            .any(|check| check["code"] == diagnostic),
        "{diagnostic}: {result}"
    );
    Ok(())
}
fn remove(value: &mut Value, key: &str) -> Result<(), Box<dyn Error>> {
    value.as_object_mut().ok_or("not object")?.remove(key);
    Ok(())
}
fn digest(source: &str) -> String {
    Sha256::digest(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn citation(path: &str, first: u64, last: u64, quote: &str) -> Value {
    json!({"path":path,"first_line":first,"last_line":last,"quote":quote})
}

fn junit(failed: u64, errors: u64, skipped: u64) -> String {
    let total = 15 + skipped;
    let mut xml = format!(
        r#"<testsuites name="case-1/lifecycle" tests="{total}" failures="{failed}" errors="{errors}" skipped="{skipped}">"#
    );
    for (binary, cases, suite_skips, failures, suite_errors) in [
        ("cocoa::agents", 11, 0, failed, errors),
        ("cocoa::cli", 4 + skipped, skipped, 0, 0),
    ] {
        xml.push_str(&format!(r#"<testsuite name="{binary}" tests="{cases}" failures="{failures}" errors="{suite_errors}" skipped="{suite_skips}">"#));
        for index in 0..cases {
            xml.push_str(&format!(
                r#"<testcase name="case_{index}" classname="{binary}">"#
            ));
            if index < failures {
                xml.push_str("<failure/>");
            } else if index < failures + suite_errors {
                xml.push_str("<error/>");
            } else if index >= cases - suite_skips {
                xml.push_str(r#"<skipped message="filter mismatch"/>"#);
            }
            xml.push_str("</testcase>");
        }
        xml.push_str("</testsuite>");
    }
    xml.push_str("</testsuites>");
    xml
}
