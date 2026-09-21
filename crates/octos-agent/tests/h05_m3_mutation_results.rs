use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, DiffEditTool, EditFileTool, ToolRegistry, WriteFileTool};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

#[derive(Default)]
struct MutationProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for MutationProvider {
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
            1 => tool_call(
                "edit_noop",
                "edit_file",
                json!({
                    "path": "edit.txt",
                    "old_string": "absent",
                    "new_string": "absent",
                }),
            ),
            2 => tool_call(
                "write_noop",
                "write_file",
                json!({"path": "write.txt", "content": "same\n"}),
            ),
            3 => tool_call(
                "diff_noop",
                "diff_edit",
                json!({
                    "path": "diff.txt",
                    "diff": "@@ -1 +1 @@\n-same\n+same\n",
                }),
            ),
            4 => tool_call(
                "edit_change",
                "edit_file",
                json!({
                    "path": "edit.txt",
                    "old_string": "same",
                    "new_string": "changed",
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
        "h05-m3-fixture"
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
    name: &str,
    arguments: serde_json::Value,
) -> (Option<String>, Vec<ToolCall>, StopReason) {
    (
        None,
        vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            metadata: None,
        }],
        StopReason::ToolUse,
    )
}

#[tokio::test]
async fn no_change_and_final_mutation_metadata_reach_the_next_model_request() {
    let workspace = tempfile::tempdir().unwrap();
    for name in ["edit.txt", "write.txt", "diff.txt"] {
        std::fs::write(workspace.path().join(name), "same\n").unwrap();
    }
    let provider = Arc::new(MutationProvider::default());
    let mut tools = ToolRegistry::new();
    tools.register(EditFileTool::new(workspace.path()).with_local_edit_enabled(true));
    tools.register(WriteFileTool::new(workspace.path()).with_local_edit_enabled(true));
    tools.register(DiffEditTool::new(workspace.path()).with_local_edit_enabled(true));
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".octos"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("h05-m3"), provider.clone(), tools, memory).with_config(
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
        std::fs::read_to_string(workspace.path().join("edit.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("write.txt")).unwrap(),
        "same\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("diff.txt")).unwrap(),
        "same\n"
    );

    let result = |suffix: &str| {
        response
            .tool_results
            .iter()
            .find(|(id, _)| id.ends_with(suffix))
            .map(|(_, metadata)| metadata)
            .unwrap()
    };
    for id in ["edit_noop", "write_noop", "diff_noop"] {
        assert_eq!(result(id)["outcome"], "no_change");
        assert_eq!(result(id)["file_modified"], false);
    }
    assert_eq!(result("edit_change")["outcome"], "modified");
    assert_eq!(result("edit_change")["final_state"], "confirmed");
    assert_eq!(result("edit_change")["diff_preview"][0]["op"], "update");
    assert!(
        result("edit_change")["diff_preview"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("+changed")
    );

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    let final_tools: Vec<_> = requests[4]
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .collect();
    assert_eq!(final_tools.len(), 4);
    assert!(
        final_tools
            .iter()
            .filter(|message| message.content.starts_with("[no_change]"))
            .count()
            == 3
    );
    assert!(
        final_tools
            .iter()
            .any(|message| message.content.contains("final=sha256:"))
    );
}
