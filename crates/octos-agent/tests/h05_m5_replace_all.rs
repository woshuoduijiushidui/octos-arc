use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, EditFileTool, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

#[derive(Default)]
struct ReplaceAllProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for ReplaceAllProvider {
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
                "replace_all",
                json!({
                    "path": "all.txt",
                    "old_string": "same",
                    "new_string": "changed",
                    "replace_all": true,
                }),
            ),
            2 => tool_call(
                "fuzzy_refused",
                json!({
                    "path": "fuzzy.rs",
                    "old_string": "fn one() {\nlaunch();\n}",
                    "new_string": "fn one() {\n    stop();\n}",
                    "replace_all": true,
                }),
            ),
            3 => tool_call(
                "default_ambiguous",
                json!({
                    "path": "default.txt",
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
        "h05-m5-fixture"
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
            name: "edit_file".into(),
            arguments,
            metadata: None,
        }],
        StopReason::ToolUse,
    )
}

#[tokio::test]
async fn explicit_replace_all_and_rejections_reach_the_next_model_request() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("all.txt"), "same\nsame\nsame\n").unwrap();
    let fuzzy = "fn one() {\n    launch();\n}\n";
    std::fs::write(workspace.path().join("fuzzy.rs"), fuzzy).unwrap();
    let duplicate = "same\nsame\n";
    std::fs::write(workspace.path().join("default.txt"), duplicate).unwrap();

    let provider = Arc::new(ReplaceAllProvider::default());
    let mut tools = ToolRegistry::new();
    tools.register(EditFileTool::new(workspace.path()).with_local_edit_enabled(true));
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".octos"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("h05-m5"), provider.clone(), tools, memory).with_config(
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
        std::fs::read_to_string(workspace.path().join("all.txt")).unwrap(),
        "changed\nchanged\nchanged\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("fuzzy.rs")).unwrap(),
        fuzzy
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("default.txt")).unwrap(),
        duplicate
    );

    let result = |suffix: &str| {
        response
            .tool_results
            .iter()
            .find(|(id, _)| id.ends_with(suffix))
            .map(|(_, metadata)| metadata)
            .unwrap()
    };
    assert_eq!(result("replace_all")["outcome"], "modified");
    assert_eq!(result("replace_all")["replace_all"], true);
    assert_eq!(result("replace_all")["replacement_count"], 3);
    assert_eq!(
        result("replace_all")["replacement_locations"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        result("fuzzy_refused")["reason"],
        "replace_all_no_exact_match"
    );
    assert_eq!(
        result("fuzzy_refused")["candidates"][0]["matcher"],
        "line_trimmed"
    );
    assert_eq!(result("default_ambiguous")["error_code"], "edit_ambiguous");

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
            .any(|message| message.content.contains("replacements=3"))
    );
    assert!(
        visible
            .iter()
            .any(|message| message.content.contains("[edit_no_match]"))
    );
    assert!(
        visible
            .iter()
            .any(|message| message.content.contains("[edit_ambiguous]"))
    );
}
