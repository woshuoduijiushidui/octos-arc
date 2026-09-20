use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{
    Agent, AgentConfig, FileStateCache, HookConfig, HookEvent, HookExecutor, ModelReadReceiptStore,
    PromptContextManager, PromptContextReport, PromptContextRequest, ReadFileTool,
    ReadReceiptOwner, Tool, ToolRegistry, tools::ToolContext,
};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{
    ChatConfig, ChatResponse, LlmCallPolicy, LlmProvider, StopReason, TokenUsage, ToolSpec,
    with_llm_call_policy,
};
use octos_memory::EpisodeStore;

struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.requests.lock().unwrap().push(messages.to_vec());
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
        "h02-m2"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct ToolThenFailProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for ToolThenFailProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(tool_use(vec![tool_call(
                "call_read",
                serde_json::json!({"path": "notes.txt"}),
            )]))
        } else {
            eyre::bail!("provider unavailable")
        }
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "h02-m2-fail"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct TruncateToolOutput;

impl PromptContextManager for TruncateToolOutput {
    fn prepare_prompt(
        &self,
        _request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        let before = messages.len();
        for message in messages.iter_mut() {
            if message.role == MessageRole::Tool && message.content.len() > 8 * 1024 {
                octos_core::truncate_utf8(
                    &mut message.content,
                    8 * 1024,
                    "\n...[context projection truncated]",
                );
            }
        }
        Ok(PromptContextReport {
            prompt_replaced: true,
            messages_before: before,
            messages_after: messages.len(),
            ..Default::default()
        })
    }
}

struct DropToolOutput;

impl PromptContextManager for DropToolOutput {
    fn prepare_prompt(
        &self,
        _request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        let before = messages.len();
        messages.retain(|message| message.role != MessageRole::Tool);
        Ok(PromptContextReport {
            prompt_replaced: true,
            messages_before: before,
            messages_after: messages.len(),
            ..Default::default()
        })
    }
}

fn tool_call(id: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "read_file".to_owned(),
        arguments,
        metadata: None,
    }
}

fn tool_use(tool_calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls,
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

async fn test_agent(
    workspace: &std::path::Path,
    provider: Arc<dyn LlmProvider>,
    receipts: Arc<ModelReadReceiptStore>,
) -> Agent {
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::new(workspace));
    let memory = Arc::new(EpisodeStore::open(workspace.join(".octos")).await.unwrap());
    Agent::new(AgentId::new("h02-m2"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_file_state_cache(Arc::new(FileStateCache::new()))
        .with_model_read_receipts(receipts)
}

fn tool_outputs(response: &octos_agent::ConversationResponse) -> Vec<&str> {
    response
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .map(|message| message.content.as_str())
        .collect()
}

fn receipt_store(workspace: &std::path::Path) -> Arc<ModelReadReceiptStore> {
    let workspace_id = format!(
        "local:{}",
        std::fs::canonicalize(workspace).unwrap().display()
    );
    Arc::new(ModelReadReceiptStore::for_owner(
        ReadReceiptOwner::new(workspace_id, "task", "session", "branch").unwrap(),
    ))
}

#[tokio::test]
async fn direct_repeated_reads_do_not_activate_staged_candidates() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = receipt_store(workspace.path());
    let ledger = Arc::new(FileStateCache::new());
    let mut context = ToolContext::zero();
    context.tool_id = "call_direct".to_owned();
    context.file_state_cache = Some(ledger);
    context.model_read_receipts = Some(receipts.clone());
    let tool = ReadFileTool::new(workspace.path());
    let args = serde_json::json!({"path": "notes.txt"});

    let first = tool.execute_with_context(&context, &args).await.unwrap();
    let second = tool.execute_with_context(&context, &args).await.unwrap();

    assert!(first.output.contains("alpha"));
    assert!(second.output.contains("alpha"));
    assert!(!second.output.contains("[FILE_UNCHANGED]"));
    assert_eq!(receipts.active_len(), 0);
    assert_eq!(receipts.staged_len(), 2);
}

#[tokio::test]
async fn successful_provider_dispatch_enables_next_matching_read_stub() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = receipt_store(workspace.path());
    let scripted = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call(
            "call_1",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        tool_use(vec![tool_call(
            "call_2",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), scripted.clone(), receipts.clone()).await;

    let response = agent
        .process_message("read twice", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert!(outputs[0].contains("alpha"));
    assert!(outputs[1].starts_with("[FILE_UNCHANGED]"), "{}", outputs[1]);
    assert!(outputs[1].contains("version=sha256:"));
    assert!(!outputs[1].contains("[hex-redacted]"));
    assert_eq!(receipts.active_len(), 1);
    assert_eq!(
        receipts.staged_len(),
        0,
        "a stub must not stage a candidate"
    );
    assert!(
        scripted.requests.lock().unwrap()[1]
            .iter()
            .any(|message| message.role == MessageRole::Tool && message.content.contains("alpha"))
    );
}

#[tokio::test]
async fn failed_provider_dispatch_does_not_activate_a_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ToolThenFailProvider {
        calls: AtomicUsize::new(0),
    });
    let agent = test_agent(workspace.path(), provider, receipts.clone()).await;

    let result = with_llm_call_policy(
        LlmCallPolicy::FailFast,
        agent.process_message("read once", &[], vec![]),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(receipts.active_len(), 0);
    assert_eq!(receipts.staged_len(), 0);
}

#[tokio::test]
async fn eight_kib_projection_does_not_activate_a_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\n".repeat(2_000)).unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call(
            "call_1",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        tool_use(vec![tool_call(
            "call_2",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), provider, receipts.clone())
        .await
        .with_prompt_context_manager(Arc::new(TruncateToolOutput));

    let response = agent
        .process_message("read twice", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert!(outputs[0].contains("alpha"));
    assert!(outputs[1].contains("alpha"));
    assert!(!outputs[1].contains("[FILE_UNCHANGED]"));
    assert_eq!(receipts.active_len(), 0);
}

#[tokio::test]
async fn context_pressure_source_drop_does_not_activate_a_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call(
            "call_1",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        tool_use(vec![tool_call(
            "call_2",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), provider, receipts.clone())
        .await
        .with_prompt_context_manager(Arc::new(DropToolOutput));

    let response = agent
        .process_message("read twice", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert!(outputs[0].contains("alpha"));
    assert!(outputs[1].contains("alpha"));
    assert!(!outputs[1].contains("[FILE_UNCHANGED]"));
    assert_eq!(receipts.active_len(), 0);
}

#[tokio::test]
async fn read_file_internal_100kb_truncation_does_not_stage_a_candidate() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("large.txt"), "x".repeat(120_000)).unwrap();
    let receipts = receipt_store(workspace.path());
    let ledger = Arc::new(FileStateCache::new());
    let mut context = ToolContext::zero();
    context.tool_id = "call_large".to_owned();
    context.file_state_cache = Some(ledger);
    context.model_read_receipts = Some(receipts.clone());

    let result = ReadFileTool::new(workspace.path())
        .execute_with_context(
            &context,
            &serde_json::json!({
                "path": "large.txt",
                "start_line": 1,
                "end_line": 1
            }),
        )
        .await
        .unwrap();

    assert!(result.success);
    assert!(result.output.contains("content truncated"));
    assert_eq!(receipts.staged_len(), 0);
}

#[tokio::test]
async fn execution_50kb_truncation_does_not_activate_a_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("large.txt"), "x".repeat(60_000)).unwrap();
    let receipts = receipt_store(workspace.path());
    let args = serde_json::json!({"path": "large.txt", "start_line": 1, "end_line": 1});
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call("call_1", args.clone())]),
        tool_use(vec![tool_call("call_2", args)]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), provider, receipts.clone()).await;

    let response = agent
        .process_message("read twice", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert_eq!(outputs.len(), 2);
    assert!(
        outputs
            .iter()
            .all(|output| !output.contains("[FILE_UNCHANGED]"))
    );
    assert_eq!(receipts.active_len(), 0);
}

#[tokio::test]
async fn sanitized_output_does_not_activate_a_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("secret.txt"),
        "OPENAI_API_KEY=sk-proj-abc123def456ghi789jklmnopqrst\n",
    )
    .unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call(
            "call_1",
            serde_json::json!({"path": "secret.txt"}),
        )]),
        tool_use(vec![tool_call(
            "call_2",
            serde_json::json!({"path": "secret.txt"}),
        )]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), provider, receipts.clone()).await;

    let response = agent
        .process_message("read twice", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert!(
        outputs
            .iter()
            .all(|output| output.contains("[credential-redacted]"))
    );
    assert!(
        outputs
            .iter()
            .all(|output| !output.contains("[FILE_UNCHANGED]"))
    );
    assert_eq!(receipts.active_len(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn after_tool_hook_feedback_does_not_activate_a_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call(
            "call_1",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        tool_use(vec![tool_call(
            "call_2",
            serde_json::json!({"path": "notes.txt"}),
        )]),
        end_turn(),
    ]));
    let hooks = Arc::new(HookExecutor::new(vec![HookConfig {
        event: HookEvent::AfterToolCall,
        command: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf hook-feedback >&2; exit 1".to_owned(),
        ],
        timeout_ms: 5_000,
        tool_filter: vec!["read_file".to_owned()],
        path_filter: Vec::new(),
        requires_bin: None,
    }]));
    let agent = test_agent(workspace.path(), provider, receipts.clone())
        .await
        .with_hooks(hooks);

    let response = agent
        .process_message("read twice", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert_eq!(outputs.len(), 2);
    assert!(outputs.iter().all(|output| output.contains("[hook]")));
    assert!(
        outputs
            .iter()
            .all(|output| !output.contains("[FILE_UNCHANGED]"))
    );
    assert_eq!(receipts.active_len(), 0);
}

#[tokio::test]
async fn partial_receipt_covers_only_the_visible_line_range() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("lines.txt"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![tool_call(
            "call_1",
            serde_json::json!({"path": "lines.txt", "start_line": 2, "end_line": 4}),
        )]),
        tool_use(vec![tool_call(
            "call_2",
            serde_json::json!({"path": "lines.txt", "start_line": 3, "end_line": 3}),
        )]),
        tool_use(vec![tool_call(
            "call_3",
            serde_json::json!({"path": "lines.txt", "start_line": 1, "end_line": 5}),
        )]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), provider, receipts).await;

    let response = agent
        .process_message("read ranges", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert!(outputs[0].contains("two"));
    assert!(outputs[1].starts_with("[FILE_UNCHANGED]"), "{}", outputs[1]);
    assert!(outputs[2].contains("one") && outputs[2].contains("five"));
    assert!(!outputs[2].contains("[FILE_UNCHANGED]"));
}

#[tokio::test]
async fn parallel_reads_do_not_authorize_each_other_before_dispatch() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let receipts = receipt_store(workspace.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(vec![
            tool_call(
                "call_1",
                serde_json::json!({"path": "notes.txt", "start_line": 1, "end_line": 1}),
            ),
            tool_call(
                "call_2",
                serde_json::json!({"path": "notes.txt", "start_line": 2, "end_line": 2}),
            ),
        ]),
        end_turn(),
    ]));
    let agent = test_agent(workspace.path(), provider, receipts).await;

    let response = agent
        .process_message("read twice in parallel", &[], vec![])
        .await
        .unwrap();
    let outputs = tool_outputs(&response);

    assert_eq!(outputs.len(), 2);
    assert!(outputs[0].contains("alpha"));
    assert!(outputs[1].contains("beta"));
    assert!(
        outputs
            .iter()
            .all(|output| !output.contains("[FILE_UNCHANGED]"))
    );
}
