use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{
    Agent, AgentConfig, FileStateCache, FileTarget, ModelReadReceiptStore, PromptContextManager,
    PromptContextReport, PromptContextRequest, ReadFileTool, ReadReceiptOwner, ReceiptClearReason,
    Tool, ToolRegistry,
};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;

struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("no scripted response");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "h02-m3"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct ReplaceOnThirdPrompt {
    calls: AtomicUsize,
}

impl PromptContextManager for ReplaceOnThirdPrompt {
    fn prepare_prompt(
        &self,
        _request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let before = messages.len();
        if call == 2 {
            messages.retain(|message| message.role != MessageRole::Tool);
        }
        Ok(PromptContextReport {
            prompt_replaced: call == 2,
            messages_before: before,
            messages_after: messages.len(),
            ..Default::default()
        })
    }
}

fn tool_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "read_file".to_owned(),
        arguments: serde_json::json!({"path": "notes.txt"}),
        metadata: None,
    }
}

fn tool_use(id: &str) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: vec![tool_call(id)],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
        provider_index: None,
    }
}

fn end_turn() -> ChatResponse {
    ChatResponse {
        content: Some("done".to_owned()),
        reasoning_content: None,
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
        provider_index: None,
    }
}

fn owner(workspace: &std::path::Path, task: &str, session: &str, branch: &str) -> ReadReceiptOwner {
    let target = FileTarget::for_local_workspace(workspace, &workspace.join("notes.txt")).unwrap();
    ReadReceiptOwner::new(target.workspace_id(), task, session, branch)
        .expect("complete owner must enable receipts")
}

fn store(
    workspace: &std::path::Path,
    task: &str,
    session: &str,
    branch: &str,
) -> Arc<ModelReadReceiptStore> {
    Arc::new(ModelReadReceiptStore::for_owner(owner(
        workspace, task, session, branch,
    )))
}

async fn agent(
    workspace: &std::path::Path,
    provider: Arc<dyn LlmProvider>,
    receipts: Arc<ModelReadReceiptStore>,
    ledger: Arc<FileStateCache>,
    memory: Arc<EpisodeStore>,
) -> Agent {
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::new(workspace));
    Agent::new(AgentId::new("h02-m3"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_file_state_cache(ledger)
        .with_model_read_receipts(receipts)
}

fn last_tool_output(response: &octos_agent::ConversationResponse) -> &str {
    response
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Tool)
        .expect("expected a tool result")
        .content
        .as_str()
}

#[test]
fn incomplete_owner_keeps_receipts_disabled() {
    assert!(ReadReceiptOwner::new("", "task", "session", "branch").is_none());
    assert!(ReadReceiptOwner::new("workspace", "", "session", "branch").is_none());
    assert!(ReadReceiptOwner::new("workspace", "task", "", "branch").is_none());
    assert!(ReadReceiptOwner::new("workspace", "task", "session", "").is_none());
    assert!(!ModelReadReceiptStore::new().is_enabled());
}

#[tokio::test]
async fn disabled_store_never_suppresses_file_content() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = Arc::new(ModelReadReceiptStore::new());
    let mut context = octos_agent::tools::ToolContext::zero();
    context.tool_id = "call_direct".to_owned();
    context.file_state_cache = Some(Arc::new(FileStateCache::new()));
    context.model_read_receipts = Some(receipts.clone());
    let tool = ReadFileTool::new(workspace.path());
    let args = serde_json::json!({"path": "notes.txt"});

    let first = tool.execute_with_context(&context, &args).await.unwrap();
    let second = tool.execute_with_context(&context, &args).await.unwrap();

    assert!(first.output.contains("alpha"));
    assert!(second.output.contains("alpha"));
    assert!(!second.output.contains("[FILE_UNCHANGED]"));
    assert_eq!(receipts.staged_len(), 0);
    assert_eq!(receipts.active_len(), 0);
}

#[tokio::test]
async fn prompt_replacement_clears_receipts_before_the_next_read() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = store(workspace.path(), "task-a", "session-a", "branch-a");
    let ledger = Arc::new(FileStateCache::new());
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".octos"))
            .await
            .unwrap(),
    );
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
        responses: Mutex::new(vec![
            tool_use("call_1"),
            end_turn(),
            tool_use("call_2"),
            end_turn(),
        ]),
    });
    let agent = agent(workspace.path(), provider, receipts.clone(), ledger, memory)
        .await
        .with_prompt_context_manager(Arc::new(ReplaceOnThirdPrompt {
            calls: AtomicUsize::new(0),
        }));

    let first = agent
        .process_message("read once", &[], vec![])
        .await
        .unwrap();
    assert_eq!(receipts.active_len(), 1);

    let second = agent
        .process_message("read after replacement", &first.messages, vec![])
        .await
        .unwrap();

    assert!(last_tool_output(&second).contains("alpha"));
    assert!(!last_tool_output(&second).contains("[FILE_UNCHANGED]"));
    assert_eq!(
        receipts.last_clear().map(|event| event.reason),
        Some(ReceiptClearReason::PromptReplacement)
    );
}

#[tokio::test]
async fn task_session_branch_and_workspace_stores_do_not_share_receipts() {
    let workspace_a = tempfile::tempdir().unwrap();
    let workspace_b = tempfile::tempdir().unwrap();
    std::fs::write(workspace_a.path().join("notes.txt"), "same body\n").unwrap();
    std::fs::write(workspace_b.path().join("notes.txt"), "same body\n").unwrap();
    let shared_ledger = Arc::new(FileStateCache::new());
    let memory = Arc::new(
        EpisodeStore::open(workspace_a.path().join(".octos"))
            .await
            .unwrap(),
    );

    let parent_receipts = store(workspace_a.path(), "task-a", "session-a", "parent");
    let parent_provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
        responses: Mutex::new(vec![tool_use("call_parent"), end_turn()]),
    });
    let parent = agent(
        workspace_a.path(),
        parent_provider,
        parent_receipts.clone(),
        shared_ledger.clone(),
        memory.clone(),
    )
    .await;
    parent
        .process_message("parent read", &[], vec![])
        .await
        .unwrap();
    assert_eq!(parent_receipts.active_len(), 1);

    for (workspace, task, session, branch) in [
        (workspace_a.path(), "task-b", "session-a", "parent"),
        (workspace_a.path(), "task-a", "session-b", "parent"),
        (workspace_a.path(), "task-a", "session-a", "child"),
        (workspace_b.path(), "task-a", "session-a", "parent"),
    ] {
        let child_receipts = store(workspace, task, session, branch);
        let child_provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![tool_use("call_child"), end_turn()]),
        });
        let child = agent(
            workspace,
            child_provider,
            child_receipts.clone(),
            shared_ledger.clone(),
            memory.clone(),
        )
        .await;
        let response = child
            .process_message("isolated read", &[], vec![])
            .await
            .unwrap();
        assert!(last_tool_output(&response).contains("same body"));
        assert!(!last_tool_output(&response).contains("[FILE_UNCHANGED]"));
        assert_eq!(child_receipts.active_len(), 1);
    }

    assert_eq!(parent_receipts.active_len(), 1);

    let restored_receipts = store(workspace_a.path(), "task-a", "session-a", "parent");
    let restored_provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
        responses: Mutex::new(vec![tool_use("call_restored"), end_turn()]),
    });
    let restored = agent(
        workspace_a.path(),
        restored_provider,
        restored_receipts.clone(),
        shared_ledger.clone(),
        memory.clone(),
    )
    .await;
    let response = restored
        .process_message("cold restore read", &[], vec![])
        .await
        .unwrap();

    assert!(last_tool_output(&response).contains("same body"));
    assert!(!last_tool_output(&response).contains("[FILE_UNCHANGED]"));
    assert_eq!(restored_receipts.active_len(), 1);

    let child_first_receipts = store(
        workspace_a.path(),
        "task-child-first",
        "session-child-first",
        "child",
    );
    let child_first = agent(
        workspace_a.path(),
        Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![tool_use("call_child_first"), end_turn()]),
        }),
        child_first_receipts,
        shared_ledger.clone(),
        memory.clone(),
    )
    .await;
    child_first
        .process_message("child reads first", &[], vec![])
        .await
        .unwrap();

    let fresh_parent_receipts = store(
        workspace_a.path(),
        "task-parent-after-child",
        "session-parent-after-child",
        "parent",
    );
    let fresh_parent = agent(
        workspace_a.path(),
        Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![tool_use("call_parent_after_child"), end_turn()]),
        }),
        fresh_parent_receipts,
        shared_ledger,
        memory,
    )
    .await;
    let response = fresh_parent
        .process_message("parent reads after child", &[], vec![])
        .await
        .unwrap();

    assert!(last_tool_output(&response).contains("same body"));
    assert!(!last_tool_output(&response).contains("[FILE_UNCHANGED]"));
}
