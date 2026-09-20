use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{
    Agent, AgentConfig, PromptContextManager, PromptContextReport, PromptContextRequest, Tool,
    ToolRegistry, ToolResult,
};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

#[derive(Default)]
struct CaptureProvider {
    requests: Mutex<Vec<(Vec<Message>, Vec<ToolSpec>)>>,
}

#[async_trait]
impl LlmProvider for CaptureProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push((messages.to_vec(), tools.to_vec()));
        let first = requests.len() == 1;
        Ok(ChatResponse {
            content: (!first).then(|| "done".into()),
            reasoning_content: None,
            tool_calls: if first {
                vec![ToolCall {
                    id: "call_metadata".into(),
                    name: "metadata_fixture".into(),
                    arguments: json!({}),
                    metadata: None,
                }]
            } else {
                vec![]
            },
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "h03-local-fixture"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

struct MetadataTool;

#[async_trait]
impl Tool for MetadataTool {
    fn name(&self) -> &str {
        "metadata_fixture"
    }

    fn description(&self) -> &str {
        "Local metadata fixture"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _: &serde_json::Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            output: "fixture body".into(),
            success: true,
            structured_metadata: Some(json!({"typed_marker": "metadata-only"})),
            ..Default::default()
        })
    }
}

struct FailingBridge {
    modify_before_error: bool,
}

impl PromptContextManager for FailingBridge {
    fn prepare_prompt(
        &self,
        _: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        if self.modify_before_error {
            for message in messages.iter_mut().filter(|m| m.role == MessageRole::Tool) {
                message.content = "bridge partial mutation".into();
            }
        }
        Err("deterministic bridge failure".into())
    }
}

#[tokio::test]
async fn h03_m0_metadata_goes_to_response_not_provider_messages() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(CaptureProvider::default());
    let mut tools = ToolRegistry::new();
    tools.register(MetadataTool);
    let memory = Arc::new(
        EpisodeStore::open(temp.path().join("memory"))
            .await
            .unwrap(),
    );
    let agent =
        Agent::new(AgentId::new("h03"), provider.clone(), tools, memory).with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        });
    let response = agent
        .process_message("run fixture", &[], vec![])
        .await
        .unwrap();
    assert_eq!(
        response.tool_results,
        vec![(
            "call_metadata".into(),
            json!({"typed_marker": "metadata-only"})
        )]
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .1
            .iter()
            .any(|tool| tool.name == "metadata_fixture")
    );
    assert!(
        requests[1]
            .0
            .iter()
            .all(|m| !m.content.contains("metadata-only"))
    );
    assert!(
        requests[1]
            .0
            .iter()
            .any(|m| m.role == MessageRole::Tool && m.content == "fixture body")
    );
}

#[tokio::test]
async fn h03_m0_bridge_error_uses_current_vector_without_rollback() {
    for modify_before_error in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(CaptureProvider::default());
        let mut tools = ToolRegistry::new();
        tools.register(MetadataTool);
        let memory = Arc::new(
            EpisodeStore::open(temp.path().join("memory"))
                .await
                .unwrap(),
        );
        let agent = Agent::new(AgentId::new("h03"), provider.clone(), tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                ..Default::default()
            })
            .with_prompt_context_manager(Arc::new(FailingBridge {
                modify_before_error,
            }));
        agent
            .process_message("run fixture", &[], vec![])
            .await
            .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let output = requests[1]
            .0
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .unwrap();
        assert_eq!(
            output.content,
            if modify_before_error {
                "bridge partial mutation"
            } else {
                "fixture body"
            }
        );
    }
}
