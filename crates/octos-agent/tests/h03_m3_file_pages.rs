use std::collections::HashSet;
use std::fs::{FileTimes, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::output_recovery::{OutputPolicy, OutputState};
use octos_agent::{
    Agent, AgentConfig, ModelReadReceiptStore, PromptContextManager, PromptContextReport,
    PromptContextRequest, ReadFileTool, TaskFileState, ToolRegistry,
};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{
    ChatConfig, ChatResponse, LlmCallPolicy, LlmProvider, StopReason, TokenUsage, ToolSpec,
    with_llm_call_policy,
};
use octos_memory::EpisodeStore;
use serde_json::{Value, json};

fn tool_call(id: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments,
        metadata: None,
    }
}

fn tool_use(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: calls,
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

fn tool_messages(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .collect()
}

fn output_header(message: &Message) -> Value {
    serde_json::from_str(message.content.split_once('\n').unwrap().0).unwrap()
}

async fn test_agent(
    workspace: &Path,
    provider: Arc<dyn LlmProvider>,
) -> (Agent, Arc<ModelReadReceiptStore>, Arc<OutputState>) {
    let task = TaskFileState::for_local_workspace(workspace).unwrap();
    let file_state = task.for_branch("task", "session", "root").unwrap();
    let receipts = file_state.receipts().clone();
    let owner = receipts.owner().unwrap().clone();
    let output_state = Arc::new(OutputState::new(OutputPolicy { enabled: true }, owner));
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::new(workspace));
    let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("h03-m3"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_file_state(file_state)
        .with_output_state(output_state.clone())
        .with_parent_session_key("session");
    (agent, receipts, output_state)
}

#[derive(Default)]
struct ShrinkingBridge {
    state: Mutex<Option<Arc<OutputState>>>,
}

impl PromptContextManager for ShrinkingBridge {
    fn set_output_state(&self, state: Arc<OutputState>) {
        *self.state.lock().unwrap() = Some(state);
    }

    fn prepare_prompt(
        &self,
        _request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        let state = self.state.lock().unwrap().clone().unwrap();
        for message in messages
            .iter_mut()
            .filter(|message| message.role == MessageRole::Tool)
        {
            if let Some(rendered) = state
                .project(
                    message.tool_call_id.as_deref().unwrap_or_default(),
                    &message.content,
                    1_300,
                )
                .map_err(|error| error.to_string())?
            {
                message.content = rendered.content;
            }
        }
        Ok(PromptContextReport {
            prompt_replaced: true,
            messages_before: messages.len(),
            messages_after: messages.len(),
            ..Default::default()
        })
    }

    fn tool_output_projection_policy_id(&self) -> Option<String> {
        Some("h03-m3-1300".into())
    }
}

struct PagingProvider {
    path: String,
    initial: Value,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for PagingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let Some(last) = tool_messages(messages).last().copied() else {
            return Ok(tool_use(vec![tool_call(
                "toolu_reused",
                self.initial.clone(),
            )]));
        };
        let header = output_header(last);
        let Some(next) = header.get("read_file_next") else {
            return Ok(end_turn());
        };
        let mut arguments = next["arguments"].as_object().unwrap().clone();
        arguments.insert("path".into(), json!(self.path.clone()));
        Ok(tool_use(vec![tool_call(
            "toolu_reused",
            Value::Object(arguments),
        )]))
    }

    fn model_id(&self) -> &str {
        "h03-m3-paging"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

#[tokio::test]
async fn final_provider_pages_cover_lines_long_line_and_eof_without_gaps() {
    let workspace = tempfile::tempdir().unwrap();
    let mut text = (1..=40)
        .map(|line| format!("before-{line:04}\r\n"))
        .collect::<String>();
    text.push_str(&"界".repeat(6_000));
    text.push('\n');
    text.push_str(
        &(42..=600)
            .map(|line| format!("after-{line:04}\n"))
            .collect::<String>(),
    );
    text.push_str("tail-without-newline");
    std::fs::write(workspace.path().join("long.txt"), &text).unwrap();
    let provider = Arc::new(PagingProvider {
        path: "long.txt".into(),
        initial: json!({"path": "long.txt"}),
        requests: Mutex::new(Vec::new()),
    });
    let (agent, receipts, output_state) = test_agent(workspace.path(), provider.clone()).await;
    let agent = agent.with_prompt_context_manager(Arc::new(ShrinkingBridge::default()));

    agent
        .process_message("read every page", &[], Vec::new())
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    let final_messages = requests.last().unwrap();
    let pages = tool_messages(final_messages);
    assert!(pages.len() >= 4, "expected line and byte pages");
    let mut previous_end = 0u64;
    let mut saw_line_page = false;
    let mut saw_byte_page = false;
    let mut version = None;
    let mut output_ids = HashSet::new();
    for (index, page) in pages.iter().enumerate() {
        assert!(page.content.len() <= 1_300);
        let (header_text, body) = page.content.split_once('\n').unwrap();
        let header: Value = serde_json::from_str(header_text).unwrap();
        assert!(output_ids.insert(header["output_id"].as_str().unwrap().to_owned()));
        let view = output_state
            .lookup(page.tool_call_id.as_deref().unwrap(), &page.content)
            .unwrap()
            .view;
        assert_eq!(
            view.view_digest,
            octos_agent::output_recovery::digest(page.content.as_bytes())
        );
        assert!(!view.source_proof.is_empty());
        assert_eq!(
            serde_json::to_value(&view.visible_ranges).unwrap(),
            header["ranges"]
        );
        assert_eq!(header["coordinates"], "source_bytes_except_display");
        let range = &header["ranges"][0];
        let start = range["start"].as_u64().unwrap();
        let end = range["end"].as_u64().unwrap();
        assert_eq!(
            start, previous_end,
            "page {index} skipped or repeated bytes"
        );
        assert!(end > start, "page {index} made no progress");
        let source = &text[start as usize..end as usize];
        if let Some(lines) = range["lines"].as_array() {
            saw_line_page = true;
            let first = lines[0].as_u64().unwrap();
            let expected = source
                .split_inclusive('\n')
                .enumerate()
                .map(|(offset, line)| format!("{}│ {line}", first + offset as u64))
                .collect::<String>();
            assert_eq!(body, expected);
        } else {
            saw_byte_page = true;
            assert_eq!(body, source);
        }
        previous_end = end;
        if let Some(next) = header.get("read_file_next") {
            assert_eq!(next["same_path"], true);
            let digest = next["arguments"]["source_sha256"]
                .as_str()
                .expect("continuation has a strong source version");
            if let Some(version) = &version {
                assert_eq!(digest, version);
            } else {
                version = Some(digest.to_owned());
            }
            let next_position = next["arguments"]
                .get("byte_offset")
                .or_else(|| next["arguments"].get("offset"))
                .and_then(Value::as_u64)
                .unwrap();
            if range["lines"].is_array() {
                assert_eq!(next_position, range["lines"][1].as_u64().unwrap() + 1);
            } else {
                assert_eq!(next_position, end);
            }
        } else {
            assert_eq!(index + 1, pages.len());
            assert_eq!(header["next"]["kind"], "eof");
        }
    }
    assert!(saw_line_page && saw_byte_page);
    assert_eq!(output_ids.len(), pages.len());
    assert_eq!(previous_end, text.len() as u64);
    assert_eq!(
        receipts.active_len(),
        1,
        "the bridge replaces the prompt each iteration, so only the final page remains authorized"
    );
}

#[tokio::test]
async fn bounded_byte_pages_stop_at_the_requested_utf8_boundary() {
    let workspace = tempfile::tempdir().unwrap();
    let text = "αβγ🙂\n".repeat(4_000);
    let selection_end = 22_000u64;
    std::fs::write(workspace.path().join("bytes.txt"), &text).unwrap();
    let provider = Arc::new(PagingProvider {
        path: "bytes.txt".into(),
        initial: json!({
            "path": "bytes.txt",
            "byte_offset": 0,
            "byte_limit": selection_end,
        }),
        requests: Mutex::new(Vec::new()),
    });
    let (agent, _, _) = test_agent(workspace.path(), provider.clone()).await;

    agent
        .process_message("read the selected bytes", &[], Vec::new())
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    let pages = tool_messages(requests.last().unwrap());
    assert!(pages.len() >= 3);
    let mut restored = String::new();
    let mut previous_end = 0u64;
    for (index, page) in pages.iter().enumerate() {
        let (header_text, body) = page.content.split_once('\n').unwrap();
        let header: Value = serde_json::from_str(header_text).unwrap();
        let range = &header["ranges"][0];
        let start = range["start"].as_u64().unwrap();
        let end = range["end"].as_u64().unwrap();
        assert_eq!(start, previous_end);
        assert_eq!(body, &text[start as usize..end as usize]);
        restored.push_str(body);
        previous_end = end;
        if let Some(next) = header.get("read_file_next") {
            assert_eq!(
                next["arguments"]["byte_limit"],
                selection_end.saturating_sub(end)
            );
        } else {
            assert_eq!(index + 1, pages.len());
            assert_eq!(header["next"]["kind"], "selection_end");
        }
    }
    assert_eq!(previous_end, selection_end);
    assert_eq!(restored, text[..selection_end as usize]);
}

struct ReceiptProvider {
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for ReceiptProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        match requests.len() {
            1 => Ok(tool_use(vec![tool_call(
                "call_page",
                json!({"path": "lines.txt", "offset": 30, "limit": 3000}),
            )])),
            2 => {
                let page = tool_messages(messages).last().copied().unwrap();
                let header = output_header(page);
                let lines = header["ranges"][0]["lines"].as_array().unwrap();
                let first = lines[0].as_u64().unwrap();
                let outside = lines[1].as_u64().unwrap() + 1;
                let version = header["read_file_next"]["arguments"]["source_sha256"].clone();
                assert_eq!(header["read_file_next"]["arguments"]["end_line"], 3029);
                Ok(tool_use(vec![
                    tool_call(
                        "call_inside",
                        json!({"path": "lines.txt", "offset": first, "limit": 1, "source_sha256": version}),
                    ),
                    tool_call(
                        "call_outside",
                        json!({"path": "lines.txt", "offset": outside, "limit": 1, "source_sha256": version}),
                    ),
                ]))
            }
            _ => Ok(end_turn()),
        }
    }

    fn model_id(&self) -> &str {
        "h03-m3-receipts"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

#[tokio::test]
async fn only_the_final_visible_file_range_activates_h02_receipts() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("lines.txt"),
        (1..=4_000)
            .map(|line| format!("line-{line:04}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let provider = Arc::new(ReceiptProvider {
        requests: Mutex::new(Vec::new()),
    });
    let (agent, receipts, _) = test_agent(workspace.path(), provider.clone()).await;

    agent
        .process_message("read a large range then check coverage", &[], Vec::new())
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    let last = tool_messages(requests.last().unwrap());
    assert_eq!(last.len(), 3);
    assert!(
        last[1].content.contains("[FILE_UNCHANGED]"),
        "a subrange of the exact provider-visible page should hit"
    );
    assert!(
        !last[2].content.contains("[FILE_UNCHANGED]"),
        "the first unseen line must not inherit a broader requested range"
    );
    assert!(last[2].content.contains("line-"));
    assert_eq!(receipts.active_len(), 2);
}

struct SingleReadProvider {
    path: &'static str,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for SingleReadProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        if requests.len() == 1 {
            Ok(tool_use(vec![tool_call(
                "call_sanitized",
                json!({"path": self.path}),
            )]))
        } else {
            Ok(end_turn())
        }
    }

    fn model_id(&self) -> &str {
        "h03-m3-sanitized"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn transformed_file_page_never_claims_raw_continuation_or_h02_coverage() {
    let workspace = tempfile::tempdir().unwrap();
    let secret = format!("sk-{}", "abcdefghijklmnop".repeat(1_000));
    std::fs::write(
        workspace.path().join("secret.txt"),
        format!("before\n{secret}\nafter\n"),
    )
    .unwrap();
    let provider = Arc::new(SingleReadProvider {
        path: "secret.txt",
        requests: Mutex::new(Vec::new()),
    });
    let (agent, receipts, _) = test_agent(workspace.path(), provider.clone()).await;

    agent
        .process_message("read sanitized content", &[], Vec::new())
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    let page = tool_messages(requests.last().unwrap())[0];
    let header = output_header(page);
    assert_eq!(header["coordinates"], "safe_text_bytes");
    assert_eq!(header["loss"], "source_transformed");
    assert!(header.get("read_file_next").is_none());
    assert!(!page.content.contains(&secret));
    assert_eq!(receipts.active_len(), 0);
}

struct StaleProvider {
    path: PathBuf,
    original_mtime: std::time::SystemTime,
    replacement: String,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for StaleProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        match requests.len() {
            1 => Ok(tool_use(vec![tool_call(
                "call_stale",
                json!({"path": "changing.txt"}),
            )])),
            2 => {
                let page = tool_messages(messages).last().copied().unwrap();
                let mut arguments = output_header(page)["read_file_next"]["arguments"]
                    .as_object()
                    .unwrap()
                    .clone();
                arguments.insert("path".into(), json!("changing.txt"));
                let mut file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&self.path)
                    .unwrap();
                file.write_all(self.replacement.as_bytes()).unwrap();
                file.set_times(FileTimes::new().set_modified(self.original_mtime))
                    .unwrap();
                Ok(tool_use(vec![tool_call(
                    "call_stale",
                    Value::Object(arguments),
                )]))
            }
            _ => Ok(end_turn()),
        }
    }

    fn model_id(&self) -> &str {
        "h03-m3-stale"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn context_window(&self) -> u32 {
        128_000
    }
}

#[tokio::test]
async fn a_same_size_same_mtime_replacement_rejects_the_old_continuation() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("changing.txt");
    let original = "a stable old line\n".repeat(2_000);
    let replacement = "a changed nw line\n".repeat(2_000);
    assert_eq!(original.len(), replacement.len());
    std::fs::write(&path, &original).unwrap();
    let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let provider = Arc::new(StaleProvider {
        path,
        original_mtime,
        replacement,
        requests: Mutex::new(Vec::new()),
    });
    let (agent, _, _) = test_agent(workspace.path(), provider.clone()).await;

    agent
        .process_message("continue only if the file is unchanged", &[], Vec::new())
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    let last = tool_messages(requests.last().unwrap());
    assert_eq!(last.len(), 2);
    assert!(last[1].content.contains("stale_source"));
    assert!(!last[1].content.contains("changed new line"));
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
                "call_failure",
                json!({"path": "failure.txt"}),
            )]))
        } else {
            eyre::bail!("provider unavailable")
        }
    }

    fn model_id(&self) -> &str {
        "h03-m3-failure"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn failed_provider_dispatch_does_not_activate_an_h03_page_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("failure.txt"),
        "not yet visible\n".repeat(1_000),
    )
    .unwrap();
    let provider = Arc::new(ToolThenFailProvider {
        calls: AtomicUsize::new(0),
    });
    let (agent, receipts, _) = test_agent(workspace.path(), provider).await;

    let result = with_llm_call_policy(
        LlmCallPolicy::FailFast,
        agent.process_message("read once", &[], Vec::new()),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(receipts.staged_len(), 0);
    assert_eq!(receipts.active_len(), 0);
}
