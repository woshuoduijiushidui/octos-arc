use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::model_read_receipts::ReadReceiptOwner;
use octos_agent::output_recovery::{OutputPolicy, OutputState, PAGE_BYTES};
use octos_agent::tools::RecallTool;
use octos_agent::{Agent, AgentConfig, ReadFileTool, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::{Value, json};

struct SearchProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

fn tool_use(id: &str, name: &str, arguments: Value) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
        provider_index: None,
    }
}

fn end_turn() -> ChatResponse {
    ChatResponse {
        content: Some("done".into()),
        reasoning_content: None,
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
        provider_index: None,
    }
}

fn latest_tool(messages: &[Message]) -> &Message {
    messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result reaches provider")
}

#[async_trait]
impl LlmProvider for SearchProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        Ok(match requests.len() {
            1 => tool_use("read-call", "read_file", json!({"path": "large.txt"})),
            2 => {
                let header: Value =
                    serde_json::from_str(latest_tool(messages).content.lines().next().unwrap())?;
                tool_use(
                    "search-call",
                    "recall",
                    json!({
                        "output_id": header["output_id"],
                        "stream": "file",
                        "query": "FINAL_PROVIDER_SEARCH_TARGET",
                    }),
                )
            }
            3 => {
                let search: Value = serde_json::from_str(&latest_tool(messages).content)?;
                tool_use(
                    "recall-call",
                    "recall",
                    json!({
                        "output_id": search["output_id"],
                        "stream": search["stream"],
                        "offset": search["matches"][0]["start"],
                        "limit": 128,
                    }),
                )
            }
            _ => end_turn(),
        })
    }

    fn model_id(&self) -> &str {
        "h03-m7-search"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

#[tokio::test]
async fn h03_m7_search_then_recall_reaches_the_final_provider_without_new_artifacts() {
    let workspace = tempfile::tempdir().unwrap();
    let prefix = "ordinary source line\n".repeat(700);
    let marker = "FINAL_PROVIDER_SEARCH_TARGET";
    std::fs::write(
        workspace.path().join("large.txt"),
        format!("{prefix}{marker}\n{}", "tail line\n".repeat(700)),
    )
    .unwrap();
    let state = Arc::new(OutputState::new(
        OutputPolicy { enabled: true },
        ReadReceiptOwner::new("workspace", "task", "session", "branch").unwrap(),
    ));
    state.enable_store(workspace.path()).unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::new(workspace.path()));
    tools.register(RecallTool::for_output_recovery(OutputPolicy {
        enabled: true,
    }));
    let provider = Arc::new(SearchProvider {
        requests: Mutex::new(Vec::new()),
    });
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join("memory"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("search"), provider.clone(), tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_output_state(state);

    agent
        .process_message("find and recover the target", &[], Vec::new())
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let search = latest_tool(&requests[2]);
    assert!(search.content.len() <= PAGE_BYTES);
    let search: Value = serde_json::from_str(&search.content).unwrap();
    assert_eq!(search["search_complete"], true);
    assert_eq!(search["matches"][0]["start"], prefix.len() as u64);
    let recalled = latest_tool(&requests[3]);
    assert!(recalled.content.contains(marker));
    let index: Value = serde_json::from_slice(
        &std::fs::read(
            workspace
                .path()
                .join("context_ledgers/tool-output/recovery-v1/index.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["records"].as_array().unwrap().len(),
        1,
        "search and recall must not create new artifacts"
    );
}
