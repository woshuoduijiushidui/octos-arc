use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::model_read_receipts::ReadReceiptOwner;
use octos_agent::output_recovery::{MIN_PAGE_BYTES, OutputPolicy, OutputState};
use octos_agent::{Agent, AgentConfig, EditFileTool, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

#[derive(Default)]
struct TypedRecoveryProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for TypedRecoveryProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        let (content, tool_calls, stop_reason) = if requests.len() == 1 {
            (
                None,
                vec![ToolCall {
                    id: "ambiguous".into(),
                    name: "edit_file".into(),
                    arguments: json!({
                        "path": "current.txt",
                        "old_string": "same",
                        "new_string": "changed",
                    }),
                    metadata: None,
                }],
                StopReason::ToolUse,
            )
        } else {
            (Some("done".into()), vec![], StopReason::EndTurn)
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
async fn typed_edit_rejection_reaches_final_request_with_bounded_non_receipt_evidence() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("current.txt");
    let original = format!("{}\n{}\n", "same ".repeat(200), "same ".repeat(200));
    std::fs::write(&path, &original).unwrap();
    let args = json!({
        "path": "current.txt",
        "old_string": "same",
        "new_string": "changed",
    });

    let mut registry = ToolRegistry::new();
    registry.register(EditFileTool::new(workspace.path()).with_local_edit_enabled(true));
    let mut direct = registry.execute("edit_file", &args).await.unwrap();
    let metadata = direct.structured_metadata.as_ref().unwrap();
    assert_eq!(metadata["error_code"], "edit_ambiguous");
    assert_eq!(metadata["matcher"], "exact");
    assert_eq!(metadata["occurrence_count"], 400);
    assert!(direct.output.contains("current=sha256:"));
    assert!(
        direct
            .output
            .contains("remedy=retry_with_current_exact_text")
    );
    assert!(
        direct
            .output_document
            .as_ref()
            .is_some_and(|document| document.file_read.is_none())
    );

    let output_state = OutputState::new(
        OutputPolicy { enabled: true },
        ReadReceiptOwner::new("workspace", "task", "session", "branch").unwrap(),
    );
    let projected = output_state
        .register(
            "tiny".into(),
            "ambiguous",
            &args,
            direct.output_document.take().unwrap(),
            false,
            MIN_PAGE_BYTES,
        )
        .unwrap();
    assert!(projected.content.len() <= MIN_PAGE_BYTES);
    assert!(projected.content.contains("[edit_ambiguous]"));
    assert!(projected.content.contains("current=sha256:"));
    assert!(
        projected
            .content
            .contains("remedy=retry_with_current_exact_text")
    );
    assert!(
        projected.content.contains("lines="),
        "{}",
        projected.content
    );
    let header: serde_json::Value =
        serde_json::from_str(projected.content.lines().next().unwrap()).unwrap();
    assert_eq!(header["recoverable"], false);
    assert!(header.get("read_file_next").is_none());

    let provider = Arc::new(TypedRecoveryProvider::default());
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".octos"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("h05-m2"), provider.clone(), registry, memory)
        .with_output_state(Arc::new(OutputState::new(
            OutputPolicy { enabled: true },
            ReadReceiptOwner::new("workspace", "task", "session", "agent").unwrap(),
        )))
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        });
    let response = agent
        .process_message("run the local fixture", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "done");
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    let result_metadata = response
        .tool_results
        .iter()
        .find(|(id, _)| id.ends_with("ambiguous"))
        .map(|(_, metadata)| metadata)
        .unwrap();
    assert_eq!(result_metadata["error_code"], "edit_ambiguous");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let visible: Vec<_> = requests[1]
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .collect();
    assert_eq!(visible.len(), 1);
    assert!(visible[0].content.contains("[edit_ambiguous]"));
    assert!(visible[0].content.contains("suggestion=true"));
    assert!(visible[0].content.len() <= 8192);
}
