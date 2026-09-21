use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{
    Agent, AgentConfig, DiffEditTool, EditFileTool, FileMetadataHint, FileStateCache, FileTarget,
    FileVersion, ReadFileTool, Tool, ToolRegistry, tools::ToolContext,
};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

#[derive(Default)]
struct NoMatchThenReadProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for NoMatchThenReadProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        let index = requests.len();
        let (content, tool_calls, stop_reason) = match index {
            1 => (
                None,
                vec![ToolCall {
                    id: "missing_edit".into(),
                    name: "edit_file".into(),
                    arguments: json!({
                        "path": "current.txt",
                        "old_string": "stale value",
                        "new_string": "replacement",
                    }),
                    metadata: None,
                }],
                StopReason::ToolUse,
            ),
            2 => (
                None,
                vec![ToolCall {
                    id: "reread".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "current.txt"}),
                    metadata: None,
                }],
                StopReason::ToolUse,
            ),
            _ => (Some("done".into()), vec![], StopReason::EndTurn),
        };
        Ok(ChatResponse {
            content,
            reasoning_content: None,
            tool_calls,
            stop_reason,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "h05-local-fixture"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

#[tokio::test]
async fn h05_m0_no_match_requires_a_follow_up_read() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("current.txt"), "current value\n").unwrap();
    let provider = Arc::new(NoMatchThenReadProvider::default());
    let mut tools = ToolRegistry::new();
    tools.register(EditFileTool::new(workspace.path()));
    tools.register(ReadFileTool::new(workspace.path()));
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".octos"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("h05-m0"), provider.clone(), tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            ..Default::default()
        },
    );

    let response = agent
        .process_message("run the local fixture", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "done");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1].iter().any(|message| {
            message.role == MessageRole::Tool
                && message.tool_call_id.as_deref() == Some("call_missing_edit")
                && message.content.contains("String not found")
        }),
        "{:#?}",
        requests[1]
    );
    assert!(requests[2].iter().any(|message| {
        message.role == MessageRole::Tool
            && message.tool_call_id.as_deref() == Some("call_reread")
            && message.content.contains("current value")
    }));
}

#[tokio::test]
async fn h05_m0_exact_ambiguity_is_rejected_without_writing() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("ambiguous.txt");
    let original = "same\nmiddle\nsame\n";
    std::fs::write(&path, original).unwrap();

    let result = EditFileTool::new(workspace.path())
        .execute(&json!({
            "path": "ambiguous.txt",
            "old_string": "same",
            "new_string": "changed",
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("2 occurrences"));
    assert!(result.output.contains("exact replacer"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn h05_m0_block_anchor_can_overwrite_a_semantically_different_middle() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("authorization.rs");
    std::fs::write(
        &path,
        "fn authorize() {\n    if is_guest(user) {\n        grant_access();\n    }\n}\n",
    )
    .unwrap();

    let result = EditFileTool::new(workspace.path())
        .execute(&json!({
            "path": "authorization.rs",
            "old_string": "fn authorize() {\n    if is_admin(user) {\n        grant_access();\n    }\n}",
            "new_string": "fn authorize() {\n    deny_access();\n}",
        }))
        .await
        .unwrap();

    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("block_anchor"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "fn authorize() {\n    deny_access();\n}\n"
    );
}

#[tokio::test]
async fn h05_m0_diff_rejects_unique_context_beyond_three_lines() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("drift.txt");
    let original = "p1\np2\np3\np4\ntarget\nend\n";
    std::fs::write(&path, original).unwrap();

    let result = DiffEditTool::new(workspace.path())
        .execute(&json!({
            "path": "drift.txt",
            "diff": "@@ -1 +1 @@\n-target\n+changed\n",
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("+-3 lines"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn h05_m0_diff_ignores_trailing_whitespace_and_applies_hunks_in_reverse() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("multi.txt");
    std::fs::write(&path, "first   \nkeep\nmiddle\nkeep\nlast\t\n").unwrap();

    let result = DiffEditTool::new(workspace.path())
        .execute(&json!({
            "path": "multi.txt",
            "diff": concat!(
                "@@ -1,2 +1,2 @@\n",
                "-first\n",
                "+FIRST\n",
                " keep\n",
                "@@ -4,2 +4,2 @@\n",
                " keep\n",
                "-last\n",
                "+LAST\n",
            ),
        }))
        .await
        .unwrap();

    assert!(result.success, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "FIRST\nkeep\nmiddle\nkeep\nLAST\n"
    );
}

#[tokio::test]
async fn h05_m0_failed_multi_hunk_diff_never_reaches_disk() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("atomic.txt");
    let original = "first\nkeep\nmiddle\nkeep\nlast\n";
    std::fs::write(&path, original).unwrap();

    let result = DiffEditTool::new(workspace.path())
        .execute(&json!({
            "path": "atomic.txt",
            "diff": concat!(
                "@@ -1,2 +1,2 @@\n",
                "-first\n",
                "+FIRST\n",
                " keep\n",
                "@@ -4,2 +4,2 @@\n",
                " keep\n",
                "-missing\n",
                "+LAST\n",
            ),
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("Failed to apply diff"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn h05_m0_same_content_edit_reports_a_write_and_consumes_the_version() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("noop.txt");
    let original = b"unchanged\n";
    std::fs::write(&path, original).unwrap();
    let target = FileTarget::for_local_workspace(workspace.path(), &path).unwrap();
    let ledger = Arc::new(FileStateCache::new());
    ledger.record(FileVersion::from_bytes(
        target.clone(),
        None,
        original,
        FileMetadataHint::new(original.len() as u64, None, None, None, None),
    ));
    let mut context = ToolContext::zero();
    context.file_state_cache = Some(ledger.clone());

    let result = EditFileTool::new(workspace.path())
        .execute_with_context(
            &context,
            &json!({
                "path": "noop.txt",
                "old_string": "unchanged",
                "new_string": "unchanged",
            }),
        )
        .await
        .unwrap();

    assert!(result.success, "{}", result.output);
    assert_eq!(result.file_modified.as_deref(), Some(path.as_path()));
    assert!(
        ledger.get(&target).is_none(),
        "the current implementation consumes the observed version on a byte-identical edit"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn h05_m0_formatter_can_expand_a_local_edit() {
    if std::process::Command::new("rustfmt")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rustfmt not on PATH");
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("format.rs");
    std::fs::write(
        &path,
        "fn untouched(){let value=1;}\nfn target(){let value=1;}\n",
    )
    .unwrap();
    let mut context = ToolContext::zero();
    context.format_after_edit = true;

    let result = EditFileTool::new(workspace.path())
        .execute_with_context(
            &context,
            &json!({
                "path": "format.rs",
                "old_string": "fn target(){let value=1;}",
                "new_string": "fn target(){let value=2;}",
            }),
        )
        .await
        .unwrap();

    assert!(result.success, "{}", result.output);
    if result.output.contains("timed out") {
        eprintln!("skipping formatter assertions: rustfmt exceeded its timeout");
        return;
    }
    let formatted = std::fs::read_to_string(path).unwrap();
    assert!(formatted.contains("fn untouched() {\n    let value = 1;\n}"));
    assert!(formatted.contains("fn target() {\n    let value = 2;\n}"));
}
