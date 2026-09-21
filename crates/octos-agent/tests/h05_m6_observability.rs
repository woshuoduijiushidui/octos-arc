use std::collections::BTreeMap;
use std::path::Path;

use octos_agent::{DiffEditTool, EditFileTool, ToolRegistry, ToolResult, WriteFileTool};
use serde_json::{Value, json};

fn compact_bytes(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

fn observe(id: &str, tool: &str, args: &Value, result: &ToolResult) -> Value {
    let metadata = result
        .structured_metadata
        .as_ref()
        .expect("H05 mutation results must expose structured metadata");
    let code = metadata
        .get("error_code")
        .or_else(|| metadata.get("outcome"))
        .and_then(Value::as_str)
        .unwrap_or(if result.success { "success" } else { "error" });
    json!({
        "id": id,
        "tool": tool,
        "success": result.success,
        "code": code,
        "matcher": metadata.get("matcher").and_then(Value::as_str),
        "candidate_count": metadata
            .get("candidates")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        "replacement_count": if tool == "edit_file" {
            metadata
                .get("replacement_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        } else {
            0
        },
        "hunk_count": if tool == "diff_edit" {
            metadata
                .get("hunk_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        } else {
            0
        },
        "changed_lines_before": metadata
            .pointer("/changed_range/before/count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "changed_lines_after": metadata
            .pointer("/changed_range/after/count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "formatter_status": metadata
            .pointer("/formatter/status")
            .and_then(Value::as_str),
        "formatter_expanded_change": metadata
            .get("formatter_expanded_change")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "argument_bytes": compact_bytes(args),
        "result_text_bytes": result.output.len(),
        "structured_metadata_bytes": compact_bytes(metadata),
    })
}

async fn execute_observed(
    registry: &ToolRegistry,
    records: &mut Vec<Value>,
    id: &str,
    tool: &str,
    args: Value,
) -> ToolResult {
    let result = registry.execute(tool, &args).await.unwrap();
    records.push(observe(id, tool, &args, &result));
    result
}

fn increment(counts: &mut BTreeMap<String, u64>, value: Option<&str>) {
    if let Some(value) = value {
        *counts.entry(value.to_string()).or_default() += 1;
    }
}

fn aggregate(records: &[Value]) -> Value {
    let mut codes = BTreeMap::new();
    let mut matchers = BTreeMap::new();
    let mut whole_file_argument_bytes = 0u64;
    let mut local_edit_argument_bytes = 0u64;
    let mut result_text_bytes = 0u64;
    let mut structured_metadata_bytes = 0u64;
    let mut candidate_count = 0u64;
    let mut replacement_count = 0u64;
    let mut hunk_count = 0u64;
    let mut changed_lines_before = 0u64;
    let mut changed_lines_after = 0u64;
    let mut formatter_expanded_count = 0u64;

    for record in records {
        increment(&mut codes, record["code"].as_str());
        increment(&mut matchers, record["matcher"].as_str());
        let argument_bytes = record["argument_bytes"].as_u64().unwrap();
        if record["tool"] == "write_file" {
            whole_file_argument_bytes += argument_bytes;
        } else {
            local_edit_argument_bytes += argument_bytes;
        }
        result_text_bytes += record["result_text_bytes"].as_u64().unwrap();
        structured_metadata_bytes += record["structured_metadata_bytes"].as_u64().unwrap();
        candidate_count += record["candidate_count"].as_u64().unwrap();
        replacement_count += record["replacement_count"].as_u64().unwrap();
        hunk_count += record["hunk_count"].as_u64().unwrap();
        changed_lines_before += record["changed_lines_before"].as_u64().unwrap();
        changed_lines_after += record["changed_lines_after"].as_u64().unwrap();
        formatter_expanded_count +=
            u64::from(record["formatter_expanded_change"].as_bool().unwrap());
    }

    json!({
        "tool_calls": records.len(),
        "result_codes": codes,
        "matchers": matchers,
        "candidate_count": candidate_count,
        "replacement_count": replacement_count,
        "hunk_count": hunk_count,
        "changed_lines_before": changed_lines_before,
        "changed_lines_after": changed_lines_after,
        "formatter_expanded_count": formatter_expanded_count,
        "whole_file_argument_bytes": whole_file_argument_bytes,
        "local_edit_argument_bytes": local_edit_argument_bytes,
        "result_text_bytes": result_text_bytes,
        "structured_metadata_bytes": structured_metadata_bytes,
    })
}

fn write_observations(path: &Path, report: &Value) {
    let mut output = serde_json::to_vec_pretty(report).unwrap();
    output.push(b'\n');
    std::fs::write(path, output).unwrap();
}

#[tokio::test]
async fn b_observability_is_complete_and_does_not_change_tool_results() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("exact.txt"), "alpha\n").unwrap();
    std::fs::write(
        workspace.path().join("ambiguous.txt"),
        "same\nmiddle\nsame\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("candidate.txt"),
        "begin\nSENSITIVE_SOURCE_MARKER\nfinish\n",
    )
    .unwrap();
    std::fs::write(workspace.path().join("all.txt"), "x\nx\n").unwrap();
    std::fs::write(
        workspace.path().join("fuzzy.rs"),
        "fn one() {\n    launch();\n}\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("diff.txt"),
        "p1\np2\np3\np4\ntarget\nend\n",
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    registry.register(WriteFileTool::new(workspace.path()).with_local_edit_enabled(true));
    registry.register(EditFileTool::new(workspace.path()).with_local_edit_enabled(true));
    registry.register(DiffEditTool::new(workspace.path()).with_local_edit_enabled(true));

    let mut records = Vec::new();
    let created = execute_observed(
        &registry,
        &mut records,
        "write_create",
        "write_file",
        json!({"path": "created.txt", "content": "created\n"}),
    )
    .await;
    assert!(created.success);

    let write_noop = execute_observed(
        &registry,
        &mut records,
        "write_noop",
        "write_file",
        json!({"path": "created.txt", "content": "created\n"}),
    )
    .await;
    assert!(write_noop.success);
    assert!(write_noop.file_modified.is_none());

    let exact = execute_observed(
        &registry,
        &mut records,
        "edit_exact",
        "edit_file",
        json!({"path": "exact.txt", "old_string": "alpha", "new_string": "beta"}),
    )
    .await;
    assert!(exact.success);

    let ambiguous = execute_observed(
        &registry,
        &mut records,
        "edit_ambiguous",
        "edit_file",
        json!({"path": "ambiguous.txt", "old_string": "same", "new_string": "changed"}),
    )
    .await;
    assert!(!ambiguous.success);
    assert!(ambiguous.file_modified.is_none());

    let no_match = execute_observed(
        &registry,
        &mut records,
        "edit_no_match",
        "edit_file",
        json!({
            "path": "candidate.txt",
            "old_string": "begin\noutdated\nfinish",
            "new_string": "NEW_CONTENT_SHOULD_NOT_BE_RECORDED"
        }),
    )
    .await;
    assert!(!no_match.success);
    assert!(no_match.file_modified.is_none());

    let replace_all = execute_observed(
        &registry,
        &mut records,
        "edit_replace_all",
        "edit_file",
        json!({
            "path": "all.txt",
            "old_string": "x",
            "new_string": "y",
            "replace_all": true
        }),
    )
    .await;
    assert!(replace_all.success);

    let fuzzy = execute_observed(
        &registry,
        &mut records,
        "edit_fuzzy",
        "edit_file",
        json!({
            "path": "fuzzy.rs",
            "old_string": "fn one() {\nlaunch();\n}",
            "new_string": "fn one() {\n    stop();\n}"
        }),
    )
    .await;
    assert!(fuzzy.success);

    let diff = execute_observed(
        &registry,
        &mut records,
        "diff_fallback",
        "diff_edit",
        json!({
            "path": "diff.txt",
            "diff": "@@ -1 +1 @@\n-target\n+changed\n"
        }),
    )
    .await;
    assert!(diff.success);

    let edit_noop = execute_observed(
        &registry,
        &mut records,
        "edit_noop",
        "edit_file",
        json!({
            "path": "exact.txt",
            "old_string": "absent",
            "new_string": "absent"
        }),
    )
    .await;
    assert!(edit_noop.success);
    assert!(edit_noop.file_modified.is_none());

    let diff_noop = execute_observed(
        &registry,
        &mut records,
        "diff_noop",
        "diff_edit",
        json!({
            "path": "diff.txt",
            "diff": "@@ -5 +5 @@\n-changed\n+changed\n"
        }),
    )
    .await;
    assert!(diff_noop.success);
    assert!(diff_noop.file_modified.is_none());

    let totals = aggregate(&records);
    assert_eq!(totals["tool_calls"], 10);
    assert_eq!(totals["result_codes"]["modified"], 5);
    assert_eq!(totals["result_codes"]["no_change"], 3);
    assert_eq!(totals["result_codes"]["edit_ambiguous"], 1);
    assert_eq!(totals["result_codes"]["edit_no_match"], 1);
    assert_eq!(totals["matchers"]["exact"], 3);
    assert_eq!(totals["matchers"]["line_trimmed"], 1);
    assert_eq!(totals["matchers"]["diff_hunks"], 2);
    assert_eq!(totals["replacement_count"], 4);
    assert_eq!(totals["hunk_count"], 2);
    assert!(totals["candidate_count"].as_u64().unwrap() >= 3);
    assert!(totals["changed_lines_after"].as_u64().unwrap() >= 5);
    assert_eq!(totals["formatter_expanded_count"], 0);
    assert!(totals["whole_file_argument_bytes"].as_u64().unwrap() > 0);
    assert!(totals["local_edit_argument_bytes"].as_u64().unwrap() > 0);
    assert!(totals["result_text_bytes"].as_u64().unwrap() > 0);
    assert!(totals["structured_metadata_bytes"].as_u64().unwrap() > 0);

    let report = json!({
        "schema": "octos.h05-m6-agent-observability.v1",
        "records": records,
        "totals": totals,
        "privacy": {
            "source_text_recorded": false,
            "full_arguments_recorded": false,
            "credentials_recorded": false,
        }
    });
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains("SENSITIVE_SOURCE_MARKER"));
    assert!(!serialized.contains("outdated"));
    assert!(!serialized.contains("NEW_CONTENT_SHOULD_NOT_BE_RECORDED"));

    if let Some(path) = std::env::var_os("H05_M6_OBSERVABILITY_OUT") {
        write_observations(Path::new(&path), &report);
    }
}
