use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::model_read_receipts::ReadReceiptOwner;
use octos_agent::output_recovery::*;
use octos_agent::{
    Agent, AgentConfig, PromptContextManager, PromptContextReport, PromptContextRequest,
    ToolRegistry,
};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

fn state(enabled: bool) -> Arc<OutputState> {
    Arc::new(OutputState::new(
        OutputPolicy { enabled },
        ReadReceiptOwner::new("workspace", "task", "session", "branch").unwrap(),
    ))
}

fn document(stream: OutputStream, text: &str) -> OutputDocument {
    OutputDocument {
        source: if stream == OutputStream::File {
            OutputSource::File {
                target: "/long/".repeat(1000),
                sha256: digest(text.as_bytes()),
            }
        } else {
            OutputSource::Command {
                run_id: String::new(),
            }
        },
        parts: vec![OutputPart {
            stream,
            text: text.into(),
            start: 0,
            first_line: (stream == OutputStream::File).then_some(1),
            total: Some(text.len() as u64),
        }],
        capture: CaptureState::Complete,
        execution: if stream == OutputStream::File {
            ExecutionStatus::NotApplicable
        } else {
            ExecutionStatus::Exited {
                code: Some(7),
                signal: None,
            }
        },
        transformed: false,
        loss_reason: None,
    }
}

fn header(text: &str) -> serde_json::Value {
    serde_json::from_str(text.split_once('\n').unwrap().0).unwrap()
}

#[test]
fn file_and_log_share_total_budget_including_all_metadata() {
    for stream in [OutputStream::File, OutputStream::Stdout] {
        for budget in [0, 1, 511, 512, 768, 1024, 4096, 8192] {
            let state = state(true);
            let text = "中文🙂\r\n".repeat(3000);
            let rendered = state.register(
                "id".into(),
                "call",
                &json!({}),
                document(stream, &text),
                false,
                budget,
            );
            if budget < MIN_PAGE_BYTES {
                assert_eq!(rendered.unwrap_err(), OutputError::InsufficientBudget);
                continue;
            }
            let rendered = rendered.unwrap();
            assert!(rendered.content.len() <= budget);
            let h = header(&rendered.content);
            assert_eq!(h["success"], false);
            assert_eq!(h["recoverable"], false);
            assert!(!rendered.content.contains("/long/"));
            assert!(!rendered.view.visible_ranges.is_empty());
            let range = &rendered.view.visible_ranges[0];
            assert!(text.is_char_boundary(range.end as usize));
            assert!(range.end > 0);
            assert_eq!(
                rendered.view.view_digest,
                digest(rendered.content.as_bytes())
            );
            assert!(matches!(
                rendered.view.continuation,
                Continuation::Next { .. }
            ));
        }
    }
}

#[test]
fn smaller_projection_restarts_from_source_and_updates_proof() {
    let state = state(true);
    let text = "中🙂文".repeat(4000);
    let large = state
        .register(
            "id".into(),
            "call",
            &json!({}),
            document(OutputStream::File, &text),
            true,
            PAGE_BYTES,
        )
        .unwrap();
    let small = state.project("call", &large.content, 900).unwrap().unwrap();
    assert!(small.content.len() <= 900);
    assert!(small.view.visible_ranges[0].end < large.view.visible_ranges[0].end);
    assert_ne!(small.view.view_digest, large.view.view_digest);
    assert_ne!(small.view.source_proof, large.view.source_proof);
    assert_eq!(small.view.output_id, large.view.output_id);
    assert_eq!(
        small.content,
        state
            .project("call", &large.content, PAGE_BYTES)
            .unwrap()
            .unwrap()
            .content
    );
    assert_eq!(
        header(&small.content)["next"]["positions"][0][1],
        small.view.visible_ranges[0].end
    );
}

#[test]
fn empty_crlf_bom_and_no_newline_have_exact_source_ranges() {
    for text in ["", "\u{feff}中文\r\nlast", "only", "\r\n", "last\n"] {
        let state = state(true);
        let output = state
            .register(
                "id".into(),
                "call",
                &json!({}),
                document(OutputStream::File, text),
                true,
                PAGE_BYTES,
            )
            .unwrap();
        assert_eq!(output.view.continuation, Continuation::Eof);
        if text.is_empty() {
            assert!(output.view.visible_ranges.is_empty());
        } else {
            assert_eq!(output.view.visible_ranges[0].end, text.len() as u64);
            assert_eq!(
                output.view.visible_ranges[0].lines,
                Some((1, text.split_inclusive('\n').count() as u64))
            );
            assert!(
                output
                    .content
                    .contains(text.split_inclusive('\n').next().unwrap())
            );
        }
    }
}

#[test]
fn capture_availability_execution_and_pending_are_independent() {
    let state = state(true);
    for capture in [
        CaptureState::Running,
        CaptureState::Partial,
        CaptureState::Complete,
    ] {
        let mut doc = document(OutputStream::Stdout, "");
        doc.capture = capture.clone();
        doc.parts[0].total = None;
        doc.execution = ExecutionStatus::Unknown;
        let out = state
            .register(
                format!("{capture:?}"),
                "call",
                &json!({}),
                doc,
                false,
                PAGE_BYTES,
            )
            .unwrap();
        assert_eq!(out.view.availability, Availability::Missing);
        assert_eq!(out.view.execution, ExecutionStatus::Unknown);
        assert!(out.view.stored_ranges.is_empty());
        assert!(!out.view.recoverable);
        assert_eq!(
            out.view.continuation,
            match capture {
                CaptureState::Running => Continuation::Pending,
                CaptureState::Partial => Continuation::Unavailable,
                CaptureState::Complete => Continuation::SelectionEnd,
            }
        );
    }
}

#[test]
fn metadata_over_budget_and_unknown_config_fail_closed() {
    let state = state(true);
    let mut doc = document(OutputStream::File, "body");
    doc.source = OutputSource::File {
        target: "x".repeat(17000),
        sha256: "hash".into(),
    };
    assert_eq!(
        state
            .register("id".into(), "call", &json!({}), doc, true, PAGE_BYTES)
            .unwrap_err(),
        OutputError::InsufficientBudget
    );
    for value in [None, Some(""), Some("typo"), Some("0"), Some("off")] {
        assert!(!OutputPolicy::parse(value).enabled);
    }
    for value in ["1", "true", "on", " TRUE "] {
        assert!(OutputPolicy::parse(Some(value)).enabled);
    }
}

#[test]
fn identities_and_owners_do_not_follow_content_hashes() {
    let a = state(true);
    let b = state(true);
    let one = a
        .register(
            "one".into(),
            "reused",
            &json!({}),
            document(OutputStream::Stdout, "same"),
            true,
            PAGE_BYTES,
        )
        .unwrap();
    let two = a
        .register(
            "two".into(),
            "reused",
            &json!({}),
            document(OutputStream::Stdout, "same"),
            true,
            PAGE_BYTES,
        )
        .unwrap();
    assert_ne!(one.view.output_id, two.view.output_id);
    assert_eq!(
        a.lookup("reused", &one.content).unwrap().view.output_id,
        "one"
    );
    assert!(b.lookup("reused", &one.content).is_none());
    assert!(a.lookup("different", &one.content).is_none());
}

#[test]
fn batch_allocation_is_repeatable_and_bounded() {
    assert_eq!(allocate_batch(2001, &[8192, 8192]), vec![1001, 1000]);
    assert_eq!(allocate_batch(2001, &[100, 8192]), vec![100, 1000]);
    assert_eq!(allocate_batch(usize::MAX, &[8192, 400]), vec![8192, 400]);
    let state = state(true);
    let mut messages = vec![Message::assistant("")];
    let calls: Vec<_> = (0..3)
        .map(|i| ToolCall {
            id: format!("call{i}"),
            name: "read_file".into(),
            arguments: json!({}),
            metadata: None,
        })
        .collect();
    messages[0].tool_calls = Some(calls.clone());
    for call in calls {
        let out = state
            .register(
                call.id.clone(),
                &call.id,
                &call.arguments,
                document(OutputStream::File, &"line\n".repeat(3000)),
                true,
                PAGE_BYTES,
            )
            .unwrap();
        messages.push(Message::tool_with_thread(
            out.content,
            call.id,
            octos_core::ThreadId::new("thread"),
        ));
    }
    state.prepare_messages(&mut messages, 3002).unwrap();
    assert!(messages[1..].iter().map(|m| m.content.len()).sum::<usize>() <= 3002);
    assert!(
        messages[1..]
            .iter()
            .all(|m| header(&m.content)["ranges"].is_array())
    );
    assert_eq!(
        state.prepare_messages(&mut messages, 100),
        Err(OutputError::InsufficientBudget)
    );
    assert_eq!(
        state.prepare_messages(&mut messages, usize::MAX),
        Err(OutputError::InsufficientBudget)
    );
}

struct CaptureProvider {
    calls: Vec<ToolCall>,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for CaptureProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        let first = requests.len() == 1;
        Ok(ChatResponse {
            content: (!first).then(|| "done".into()),
            reasoning_content: None,
            tool_calls: if first { self.calls.clone() } else { vec![] },
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

struct FailingBridge;
impl PromptContextManager for FailingBridge {
    fn prepare_prompt(
        &self,
        _: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        for m in messages.iter_mut().filter(|m| m.role == MessageRole::Tool) {
            let mut end = m.content.len().min(100);
            while !m.content.is_char_boundary(end) {
                end -= 1;
            }
            m.content.truncate(end);
        }
        Err("fixture bridge error".into())
    }
}

async fn run_agent(
    enabled: bool,
    failing_bridge: bool,
    calls: Vec<ToolCall>,
) -> (Vec<Message>, Arc<OutputState>) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "中文🙂\r\n".repeat(2000)).unwrap();
    let provider = Arc::new(CaptureProvider {
        calls,
        requests: Mutex::new(vec![]),
    });
    let mut tools = ToolRegistry::new();
    tools.register(octos_agent::tools::ReadFileTool::new(temp.path()));
    tools.register(octos_agent::tools::ShellTool::new(temp.path()));
    tools.register(octos_agent::tools::coding_tools::BashTool::new(
        temp.path(),
        Arc::new(octos_agent::sandbox::NoSandbox),
    ));
    tools.register(octos_agent::tools::coding_tools::ExecCommandTool::new(
        temp.path(),
        Arc::new(octos_agent::sandbox::NoSandbox),
    ));
    let memory = Arc::new(
        EpisodeStore::open(temp.path().join("memory"))
            .await
            .unwrap(),
    );
    let output_state = state(enabled);
    let mut agent = Agent::new(AgentId::new("test"), provider.clone(), tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_output_state(output_state.clone());
    if failing_bridge {
        agent = agent.with_prompt_context_manager(Arc::new(FailingBridge));
    }
    agent
        .process_message("run fixture", &[], vec![])
        .await
        .unwrap();
    let messages = provider.requests.lock().unwrap().last().unwrap().clone();
    (messages, output_state)
}

#[tokio::test]
async fn real_file_and_shell_reach_final_provider_with_typed_ranges_and_status() {
    let calls = vec![
        ToolCall {
            id: "file".into(),
            name: "read_file".into(),
            arguments: json!({"path":"file.txt"}),
            metadata: None,
        },
        ToolCall {
            id: "shell".into(),
            name: "shell".into(),
            arguments: json!({"command":"printf 'hello'; printf 'failure' >&2; exit 7"}),
            metadata: None,
        },
    ];
    let (messages, state) = run_agent(true, false, calls).await;
    let tools: Vec<_> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();
    assert_eq!(tools.len(), 2);
    for message in tools {
        assert!(message.content.len() <= PAGE_BYTES);
        let view = state
            .lookup(message.tool_call_id.as_deref().unwrap(), &message.content)
            .unwrap_or_else(|| {
                panic!(
                    "unbound output {:?}: {}",
                    message.tool_call_id, message.content
                )
            })
            .view;
        assert_eq!(view.view_digest, digest(message.content.as_bytes()));
        if message.tool_call_id.as_deref() == Some("call_shell") {
            assert_eq!(
                view.execution,
                ExecutionStatus::Exited {
                    code: Some(7),
                    signal: None
                }
            );
            assert!(!view.success);
            assert!(message.content.contains("failure"));
            assert!(
                view.visible_ranges
                    .iter()
                    .any(|r| r.stream == OutputStream::Stderr)
            );
        } else {
            assert!(matches!(view.source, OutputSource::File { .. }));
            assert!(view.visible_ranges[0].lines.is_some());
        }
    }
}

#[tokio::test]
async fn bridge_failure_never_sends_a_cut_range_declaration() {
    let calls = vec![ToolCall {
        id: "file".into(),
        name: "read_file".into(),
        arguments: json!({"path":"file.txt"}),
        metadata: None,
    }];
    let (messages, _) = run_agent(true, true, calls).await;
    let tool = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert!(tool.content.starts_with("source_incomplete:"));
    assert!(!tool.content.contains("ranges"));
}

#[tokio::test]
async fn recovery_off_preserves_file_output() {
    let calls = vec![ToolCall {
        id: "file".into(),
        name: "read_file".into(),
        arguments: json!({"path":"file.txt","offset":1,"limit":3}),
        metadata: None,
    }];
    let (messages, _) = run_agent(false, false, calls).await;
    let tool = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert!(tool.content.starts_with("1│ 中文🙂\n"));
    assert!(!tool.content.contains("output_id"));
}

#[tokio::test]
async fn command_aliases_preserve_nonzero_exit_and_both_streams() {
    for name in ["bash", "exec_command"] {
        let (messages, state) = run_agent(
            true,
            false,
            vec![ToolCall {
                id: "toolu_alias".into(),
                name: name.into(),
                arguments: json!({"cmd": "printf out; printf err >&2; exit 7"}),
                metadata: None,
            }],
        )
        .await;
        let message = messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .unwrap();
        let view = state.lookup("call_alias", &message.content).unwrap().view;
        assert_eq!(
            view.execution,
            ExecutionStatus::Exited {
                code: Some(7),
                signal: None
            }
        );
        assert_eq!(view.continuation, Continuation::Eof);
        assert!(!view.success);
        assert!(message.content.contains("out"));
        assert!(message.content.contains("err"));
        assert!(
            view.visible_ranges
                .iter()
                .any(|r| r.stream == OutputStream::Stdout)
        );
        assert!(
            view.visible_ranges
                .iter()
                .any(|r| r.stream == OutputStream::Stderr)
        );
    }
}

#[test]
fn sanitization_and_hook_text_are_budgeted_without_source_coverage() {
    let state = state(true);
    let secret = format!("sk-{}", "abcdefghijklmnop".repeat(50));
    let source = format!("start\n{secret}\nend\n");
    let mut result = octos_agent::ToolResult {
        output_document: Some(document(OutputStream::File, &source)),
        success: true,
        ..Default::default()
    };
    state.finish_result(
        "id",
        "call",
        "read_file",
        &json!({}),
        &mut result,
        Some(&"notice ".repeat(3000)),
    );
    assert!(result.success);
    assert!(result.output.len() <= PAGE_BYTES);
    assert!(!result.output.contains(&secret));
    let view = state.lookup("call", &result.output).unwrap().view;
    assert!(view.transformed);
    assert!(view.visible_ranges.iter().all(|r| r.lines.is_none()));
    assert!(result.output.contains("[hook]"));
}

#[tokio::test]
async fn approved_tool_uses_same_identity_and_projection_path() {
    use octos_agent::approval::*;
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "data\n".repeat(3000)).unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(octos_agent::tools::ReadFileTool::new(temp.path()));
    let provider = Arc::new(CaptureProvider {
        calls: vec![],
        requests: Mutex::new(vec![]),
    });
    let memory = Arc::new(
        EpisodeStore::open(temp.path().join("memory"))
            .await
            .unwrap(),
    );
    let state = state(true);
    let agent = Agent::new(AgentId::new("approved"), provider, tools, memory)
        .with_output_state(state.clone());
    let pending = PendingApproval {
        request: ApprovalRequestEnvelope {
            request_id: "req".into(),
            tool_name: "read_file".into(),
            tool_args_digest: "digest".into(),
            title: "read".into(),
            summary: "read".into(),
            risk_level: ApprovalRiskLevel::Normal,
            authorized_approvers: vec![],
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(1),
            on_timeout: ApprovalTimeoutBehavior::Notify,
        },
        room_id: "room".into(),
        requester: "user".into(),
        tool_id: "toolu_approved".into(),
        tool_args: json!({"path": "file.txt"}),
    };
    let first = agent.execute_approved_tool(&pending).await.unwrap();
    let second = agent.execute_approved_tool(&pending).await.unwrap();
    assert!(first.success && second.success);
    assert!(first.output.len() <= PAGE_BYTES);
    let one = state.lookup("call_approved", &first.output).unwrap().view;
    let two = state.lookup("call_approved", &second.output).unwrap().view;
    assert_ne!(one.output_id, two.output_id);
    assert_eq!(one.source, two.source);
}
