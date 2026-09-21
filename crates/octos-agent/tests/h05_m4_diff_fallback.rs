use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, DiffEditTool, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

#[derive(Default)]
struct DiffFallbackProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for DiffFallbackProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        let (content, tool_calls, stop_reason) = match requests.len() {
            1 => tool_call(
                "fallback",
                json!({
                    "path": "unique.txt",
                    "diff": "@@ -1 +1 @@\n-target\n+changed\n",
                }),
            ),
            2 => tool_call(
                "ambiguous",
                json!({
                    "path": "duplicate.txt",
                    "diff": "@@ -1 +1 @@\n-target\n+changed\n",
                }),
            ),
            3 => tool_call(
                "missing",
                json!({
                    "path": "missing.txt",
                    "diff": "@@ -3 +3 @@\n-absent\n+changed\n",
                }),
            ),
            _ => (Some("done".into()), Vec::new(), StopReason::EndTurn),
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
        "h05-m4-fixture"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

fn tool_call(
    id: &str,
    arguments: serde_json::Value,
) -> (Option<String>, Vec<ToolCall>, StopReason) {
    (
        None,
        vec![ToolCall {
            id: id.into(),
            name: "diff_edit".into(),
            arguments,
            metadata: None,
        }],
        StopReason::ToolUse,
    )
}

#[tokio::test]
async fn fallback_and_typed_failures_reach_the_next_model_request() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("unique.txt"),
        "p1\np2\np3\np4\ntarget\nend\n",
    )
    .unwrap();
    let duplicate = "p1\np2\np3\np4\ntarget\np6\np7\np8\np9\ntarget\n";
    std::fs::write(workspace.path().join("duplicate.txt"), duplicate).unwrap();
    let missing = "one\ntwo\nthree\nfour\nfive\n";
    std::fs::write(workspace.path().join("missing.txt"), missing).unwrap();

    let provider = Arc::new(DiffFallbackProvider::default());
    let mut tools = ToolRegistry::new();
    tools.register(DiffEditTool::new(workspace.path()).with_local_edit_enabled(true));
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".octos"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("h05-m4"), provider.clone(), tools, memory).with_config(
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
    assert_eq!(response.files_modified.len(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("unique.txt")).unwrap(),
        "p1\np2\np3\np4\nchanged\nend\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("duplicate.txt")).unwrap(),
        duplicate
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("missing.txt")).unwrap(),
        missing
    );

    let result = |suffix: &str| {
        response
            .tool_results
            .iter()
            .find(|(id, _)| id.ends_with(suffix))
            .map(|(_, metadata)| metadata)
            .unwrap()
    };
    assert_eq!(result("fallback")["outcome"], "modified");
    assert_eq!(result("fallback")["hunk_matches"][0]["expected_line"], 1);
    assert_eq!(result("fallback")["hunk_matches"][0]["actual_line"], 5);
    assert_eq!(
        result("fallback")["hunk_matches"][0]["matcher"],
        "full_file_line_exact"
    );
    assert_eq!(result("ambiguous")["error_code"], "diff_context_ambiguous");
    assert_eq!(result("ambiguous")["occurrence_count"], 2);
    assert_eq!(result("missing")["error_code"], "diff_context_no_match");
    assert_eq!(
        result("missing")["candidates"][0]["matcher"],
        "expected_location"
    );

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let visible = requests[3]
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 3);
    assert!(
        visible
            .iter()
            .any(|message| message.content.contains("positions=1->5"))
    );
    assert!(
        visible
            .iter()
            .any(|message| message.content.contains("[diff_context_ambiguous]"))
    );
    assert!(
        visible
            .iter()
            .any(|message| message.content.contains("[diff_context_no_match]"))
    );
}
