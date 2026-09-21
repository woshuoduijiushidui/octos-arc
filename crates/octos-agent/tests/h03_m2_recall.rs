use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::model_read_receipts::ReadReceiptOwner;
use octos_agent::output_recovery::*;
use octos_agent::tools::{RecallTool, ToolOutputLedger};
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

struct NoLegacy;
impl ToolOutputLedger for NoLegacy {
    fn fetch(&self, _: &str) -> Option<String> {
        panic!("typed recall must not fetch the whole legacy string")
    }
}

struct Provider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        let step = requests.len();
        let calls = if step == 1 {
            vec![ToolCall {
                id: "toolu_reused".into(),
                name: "read_file".into(),
                arguments: json!({"path": "file.txt", "offset": 1, "limit": 5000}),
                metadata: None,
            }]
        } else if step <= 4 {
            let previous = messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::Tool)
                .unwrap();
            let header: serde_json::Value =
                serde_json::from_str(previous.content.split_once('\n').unwrap().0).unwrap();
            vec![ToolCall {
                id: "toolu_reused".into(),
                name: "recall".into(),
                arguments: if step == 2 {
                    json!({"output_id": header["output_id"], "offset": 40_000})
                } else {
                    header["recall"].clone()
                },
                metadata: None,
            }]
        } else {
            vec![]
        };
        Ok(ChatResponse {
            content: calls.is_empty().then(|| "done".into()),
            reasoning_content: None,
            stop_reason: if calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
            tool_calls: calls,
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

#[tokio::test]
async fn h03_m2_real_agent_recall_retains_identity_through_final_projection() {
    let dir = tempfile::tempdir().unwrap();
    let text = "line with useful output\n".repeat(5000);
    std::fs::write(dir.path().join("file.txt"), &text).unwrap();
    let state = Arc::new(OutputState::new(
        OutputPolicy { enabled: true },
        ReadReceiptOwner::new("workspace", "task", "session", "branch").unwrap(),
    ));
    state.enable_store(dir.path()).unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(octos_agent::tools::ReadFileTool::new(dir.path()));
    tools.register(RecallTool::new(Arc::new(NoLegacy)));
    let provider = Arc::new(Provider {
        requests: Mutex::new(vec![]),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("test"), provider.clone(), tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_output_state(state.clone());
    agent
        .process_message("read and recall fixture", &[], vec![])
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    let mut id = None;
    for message in requests
        .last()
        .unwrap()
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
    {
        assert!(message.content.len() <= PAGE_BYTES);
        let view = state
            .lookup(message.tool_call_id.as_deref().unwrap(), &message.content)
            .unwrap()
            .view;
        assert_eq!(view.view_digest, digest(message.content.as_bytes()));
        assert!(view.recoverable);
        if let Some(id) = &id {
            assert_eq!(id, &view.output_id);
        } else {
            id = Some(view.output_id.clone());
        }
        if view.historical {
            let range = &view.visible_ranges[0];
            assert!(range.start >= 40_000);
            assert_eq!(
                message.content.split_once('\n').unwrap().1,
                &text[range.start as usize..range.end as usize]
            );
        }
    }
    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            dir.path()
                .join("context_ledgers/tool-output/recovery-v1/index.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["records"].as_array().unwrap().len(),
        1,
        "recall must never spill itself"
    );
}
