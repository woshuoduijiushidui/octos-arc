//! Typed, branch-local output views. Durable payload storage is supplied separately.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use octos_core::{Message, MessageRole};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model_read_receipts::ReadReceiptOwner;

pub const PAGE_BYTES: usize = 8192;
pub const MIN_PAGE_BYTES: usize = 512;
const MAX_ENTRIES: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputPolicy {
    pub enabled: bool,
}

impl OutputPolicy {
    pub fn parse(value: Option<&str>) -> Self {
        let enabled = match value.map(str::trim) {
            Some("1") => true,
            Some(v) if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") => true,
            None | Some("0") => false,
            Some(v) if v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") => false,
            Some(_) => {
                tracing::warn!("unknown OCTOS_OUTPUT_RECOVERY value; output recovery disabled");
                false
            }
        };
        Self { enabled }
    }

    pub fn from_env() -> Self {
        static POLICY: OnceLock<OutputPolicy> = OnceLock::new();
        *POLICY.get_or_init(|| Self::parse(std::env::var("OCTOS_OUTPUT_RECOVERY").ok().as_deref()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputSource {
    File { target: String, sha256: String },
    Command { run_id: String },
    Unspecified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Running,
    Complete,
    Partial,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Missing,
    Expired,
    StoreFailed,
    Corrupt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionStatus {
    NotApplicable,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    TimedOut,
    Cancelled,
    Running,
    Unknown,
}

impl ExecutionStatus {
    pub fn exited(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self::Exited {
            code: status.code(),
            signal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    File,
    Stdout,
    Stderr,
    Display,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRange {
    pub stream: OutputStream,
    pub start: u64,
    pub end: u64,
    /// Present only for complete source lines, 1-based inclusive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u64, u64)>,
}

#[derive(Clone, Debug)]
pub struct OutputPart {
    pub stream: OutputStream,
    pub text: String,
    pub start: u64,
    pub first_line: Option<u64>,
    pub total: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct OutputDocument {
    pub source: OutputSource,
    pub parts: Vec<OutputPart>,
    pub capture: CaptureState,
    pub execution: ExecutionStatus,
    pub transformed: bool,
    pub loss_reason: Option<String>,
}

impl OutputDocument {
    pub fn with_notice(mut self, notice: &str) -> Self {
        if !notice.is_empty() {
            if let Some(part) = self
                .parts
                .iter_mut()
                .find(|p| p.stream == OutputStream::Display)
            {
                part.text.push('\n');
                part.text.push_str(notice);
                return self;
            }
            self.parts.push(OutputPart {
                stream: OutputStream::Display,
                text: notice.into(),
                start: 0,
                first_line: None,
                total: None,
            });
        }
        self
    }

    pub fn timed_out() -> Self {
        let mut document = Self::unavailable(String::new());
        document.execution = ExecutionStatus::TimedOut;
        document.loss_reason = Some("timeout_output_not_captured".into());
        document
    }

    pub fn command(output: &std::process::Output) -> Self {
        let mut transformed = false;
        let parts = [
            (OutputStream::Stdout, &output.stdout),
            (OutputStream::Stderr, &output.stderr),
        ]
        .into_iter()
        .map(|(stream, bytes)| {
            let text = String::from_utf8_lossy(bytes);
            transformed |= matches!(text, std::borrow::Cow::Owned(_));
            OutputPart {
                stream,
                text: text.into_owned(),
                start: 0,
                first_line: None,
                total: Some(bytes.len() as u64),
            }
        })
        .collect();
        Self {
            source: OutputSource::Command {
                run_id: String::new(),
            },
            parts,
            capture: CaptureState::Complete,
            execution: ExecutionStatus::exited(output.status),
            transformed,
            loss_reason: None,
        }
    }

    pub fn unavailable(text: String) -> Self {
        Self {
            source: OutputSource::Unspecified,
            parts: vec![OutputPart {
                stream: OutputStream::Display,
                total: None,
                text,
                start: 0,
                first_line: None,
            }],
            capture: CaptureState::Partial,
            execution: ExecutionStatus::Unknown,
            transformed: true,
            loss_reason: Some("missing_source_metadata".into()),
        }
    }

    fn sanitize(&mut self) {
        for part in &mut self.parts {
            let safe = crate::sanitize::sanitize_tool_output(&part.text);
            if safe != part.text {
                part.text = safe;
                self.transformed = true;
            }
        }
        if self.transformed {
            for part in &mut self.parts {
                part.first_line = None;
                part.start = 0;
                part.total = None;
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Continuation {
    Next { positions: Vec<(OutputStream, u64)> },
    SelectionEnd,
    Eof,
    Pending,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputView {
    pub schema_version: u32,
    pub output_id: String,
    pub owner: ReadReceiptOwner,
    pub call_id: String,
    pub arguments_digest: String,
    pub source: OutputSource,
    pub capture: CaptureState,
    pub captured_ranges: Vec<OutputRange>,
    pub source_totals: Vec<(OutputStream, Option<u64>)>,
    pub loss_reason: Option<String>,
    pub availability: Availability,
    pub stored_ranges: Vec<OutputRange>,
    pub stored_bytes: u64,
    pub stored_sha256: Option<String>,
    pub visible_ranges: Vec<OutputRange>,
    pub transformed: bool,
    pub view_digest: String,
    pub source_proof: String,
    pub policy_version: u32,
    pub continuation: Continuation,
    pub execution: ExecutionStatus,
    pub success: bool,
    pub recoverable: bool,
}

#[derive(Clone, Debug)]
pub struct RenderedOutput {
    pub content: String,
    pub view: OutputView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputError {
    InsufficientBudget,
    SourceIncomplete,
    StorageLimit,
    Missing,
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InsufficientBudget => "insufficient_output_budget",
            Self::SourceIncomplete => "source_incomplete",
            Self::StorageLimit => "storage_limit",
            Self::Missing => "output_missing",
        })
    }
}

impl std::error::Error for OutputError {}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The remainder goes to earlier calls. Limits do not enlarge later allocations.
pub fn allocate_batch(available: usize, limits: &[usize]) -> Vec<usize> {
    if limits.is_empty() {
        return Vec::new();
    }
    let each = available / limits.len();
    let extra = available % limits.len();
    limits
        .iter()
        .enumerate()
        .map(|(i, limit)| (*limit).min(PAGE_BYTES).min(each + usize::from(i < extra)))
        .collect()
}

fn prefix_end(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn render_parts(document: &OutputDocument, budget: usize) -> (String, Vec<OutputRange>) {
    let mut body = String::new();
    let mut ranges = Vec::new();
    let nonempty: Vec<_> = document
        .parts
        .iter()
        .filter(|p| !p.text.is_empty())
        .collect();
    let allocations = allocate_batch(budget, &vec![budget; nonempty.len()]);
    for (part, allowance) in nonempty.into_iter().zip(allocations) {
        let label = if part.stream == OutputStream::File {
            String::new()
        } else {
            format!("\n--- {:?} ---\n", part.stream)
        };
        let available = allowance.saturating_sub(label.len());
        if available == 0 {
            continue;
        }
        let start = part.start;
        if let Some(first) = part.first_line {
            let mut used = 0;
            let mut count = 0;
            let mut formatted = String::new();
            for (index, line) in part.text.split_inclusive('\n').take(2000).enumerate() {
                let prefix = format!("{}│ ", first + index as u64);
                if formatted.len() + prefix.len() + line.len() > available {
                    break;
                }
                formatted.push_str(&prefix);
                formatted.push_str(line);
                used += line.len();
                count += 1;
            }
            if used > 0 {
                body.push_str(&label);
                body.push_str(&formatted);
                ranges.push(OutputRange {
                    stream: part.stream,
                    start,
                    end: start + used as u64,
                    lines: Some((first, first + count - 1)),
                });
                continue;
            }
        }
        // A long first line uses byte mode; it never skips the rest of that line.
        let end = prefix_end(&part.text, available);
        if end > 0 {
            body.push_str(&label);
            body.push_str(&part.text[..end]);
            ranges.push(OutputRange {
                stream: part.stream,
                start,
                end: start + end as u64,
                lines: None,
            });
        }
    }
    (body, ranges)
}

pub fn render(
    document: &OutputDocument,
    original: &OutputView,
    budget: usize,
) -> Result<RenderedOutput, OutputError> {
    let budget = budget.min(PAGE_BYTES);
    if budget < MIN_PAGE_BYTES {
        return Err(OutputError::InsufficientBudget);
    }
    if serde_json::to_vec(original)
        .map_err(|_| OutputError::SourceIncomplete)?
        .len()
        > MAX_METADATA_BYTES
    {
        return Err(OutputError::InsufficientBudget);
    }
    let mut body_budget = budget;
    loop {
        let (body, ranges) = render_parts(document, body_budget);
        let mut view = original.clone();
        view.visible_ranges = ranges;
        let next: Vec<_> = document
            .parts
            .iter()
            .filter(|p| p.stream != OutputStream::Display)
            .filter_map(|part| {
                let end = view
                    .visible_ranges
                    .iter()
                    .filter(|r| r.stream == part.stream)
                    .map(|r| r.end)
                    .max()
                    .unwrap_or(part.start);
                (end < part.start + part.text.len() as u64).then_some((part.stream, end))
            })
            .collect();
        view.continuation = if !next.is_empty() {
            Continuation::Next { positions: next }
        } else if document.capture == CaptureState::Running {
            Continuation::Pending
        } else if document.capture == CaptureState::Partial {
            Continuation::Unavailable
        } else if document
            .parts
            .iter()
            .filter(|p| p.stream != OutputStream::Display)
            .all(|p| p.total == Some(p.start + p.text.len() as u64))
        {
            Continuation::Eof
        } else {
            Continuation::SelectionEnd
        };
        let header = serde_json::json!({
            "output_id": view.output_id,
            "ranges": view.visible_ranges,
            "coordinates": if view.transformed { "safe_text_bytes" } else { "source_bytes_except_display" },
            "capture": view.capture,
            "execution": view.execution,
            "success": view.success,
            "next": view.continuation,
            "recoverable": false,
            "recovery_error": "recovery_tool_unavailable",
            "loss": view.loss_reason,
        });
        let header = format!("{}\n", header);
        let size = header.len() + body.len();
        if size <= budget {
            if body.is_empty() && document.parts.iter().any(|p| !p.text.is_empty()) {
                return Err(OutputError::InsufficientBudget);
            }
            let content = header + &body;
            view.view_digest = digest(content.as_bytes());
            view.source_proof = digest(
                &serde_json::to_vec(&(
                    &view.output_id,
                    &view.source,
                    &view.visible_ranges,
                    view.transformed,
                    &view.view_digest,
                    view.policy_version,
                ))
                .map_err(|_| OutputError::SourceIncomplete)?,
            );
            return Ok(RenderedOutput { content, view });
        }
        let reduced = budget.saturating_sub(header.len());
        if reduced >= body_budget || reduced == 0 {
            return Err(OutputError::InsufficientBudget);
        }
        body_budget = reduced;
    }
}

#[derive(Clone, Debug)]
struct Entry {
    document: Arc<OutputDocument>,
    rendered: RenderedOutput,
    /// Known exact projections, never parsed from model-supplied text.
    digests: VecDeque<String>,
    bytes: usize,
    budget: usize,
}

#[derive(Debug)]
pub struct OutputState {
    pub policy: OutputPolicy,
    pub owner: ReadReceiptOwner,
    entries: Mutex<VecDeque<Entry>>,
}

impl OutputState {
    pub fn for_agent(workspace: &std::path::Path) -> Arc<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        Arc::new(Self::new(
            OutputPolicy::from_env(),
            ReadReceiptOwner::new(workspace.to_string_lossy(), &id, &id, &id)
                .expect("runtime output owner"),
        ))
    }

    pub fn new(policy: OutputPolicy, owner: ReadReceiptOwner) -> Self {
        Self {
            policy,
            owner,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    pub fn supports(name: &str) -> bool {
        matches!(name, "read_file" | "shell" | "bash" | "exec_command")
    }

    pub fn finish_result(
        &self,
        id: &str,
        call_id: &str,
        name: &str,
        args: &serde_json::Value,
        result: &mut crate::tools::ToolResult,
        feedback: Option<&str>,
    ) {
        if !self.policy.enabled {
            return;
        }
        if result.output_document.is_none() && !Self::supports(name) {
            return;
        }
        let mut document = result
            .output_document
            .take()
            .unwrap_or_else(|| OutputDocument::unavailable(result.output.clone()));
        if let Some(feedback) = feedback {
            document = document.with_notice(&format!("[hook] {feedback}"));
        }
        let execution = document.execution.clone();
        result.output = match self.register(
            id.into(),
            call_id,
            args,
            document,
            result.success,
            octos_core::tool_output_limit(name),
        ) {
            Ok(rendered) => rendered.content,
            Err(error) => {
                let mut fallback = OutputDocument::unavailable(String::new());
                fallback.execution = execution;
                fallback.loss_reason = Some(error.to_string());
                self.register(
                    id.into(),
                    call_id,
                    args,
                    fallback,
                    result.success,
                    PAGE_BYTES,
                )
                .map(|r| r.content)
                .unwrap_or_else(|_| {
                    format!(
                        "{error}: output view unavailable; tool success={}",
                        result.success
                    )
                })
            }
        };
    }

    pub fn register(
        &self,
        id: String,
        call_id: &str,
        args: &serde_json::Value,
        mut document: OutputDocument,
        success: bool,
        budget: usize,
    ) -> Result<RenderedOutput, OutputError> {
        if document.parts.iter().any(|part| {
            part.start
                .checked_add(part.text.len() as u64)
                .is_none_or(|end| {
                    !document.transformed && part.total.is_some_and(|total| end > total)
                })
                || part.first_line.is_some_and(|first| {
                    first == 0
                        || first
                            .checked_add(
                                part.text.split_inclusive('\n').count().saturating_sub(1) as u64
                            )
                            .is_none()
                })
        }) {
            return Err(OutputError::SourceIncomplete);
        }
        document.sanitize();
        if let OutputSource::Command { run_id } = &mut document.source {
            *run_id = id.clone();
        }
        let bytes = document.parts.iter().map(|p| p.text.len()).sum::<usize>();
        if bytes > 16 * 1024 * 1024 {
            return Err(OutputError::StorageLimit);
        }
        let view = OutputView {
            schema_version: 1,
            output_id: id,
            owner: self.owner.clone(),
            call_id: crate::agent::normalize_tool_call_id(call_id),
            arguments_digest: digest(&serde_json::to_vec(args).unwrap_or_default()),
            source: document.source.clone(),
            capture: document.capture.clone(),
            captured_ranges: document
                .parts
                .iter()
                .filter(|p| !p.text.is_empty())
                .map(|p| OutputRange {
                    stream: p.stream,
                    start: p.start,
                    end: p.start + p.text.len() as u64,
                    lines: None,
                })
                .collect(),
            source_totals: document.parts.iter().map(|p| (p.stream, p.total)).collect(),
            loss_reason: document.loss_reason.clone(),
            availability: Availability::Missing,
            stored_ranges: vec![],
            stored_bytes: 0,
            stored_sha256: None,
            visible_ranges: vec![],
            transformed: document.transformed,
            view_digest: String::new(),
            source_proof: String::new(),
            policy_version: 1,
            continuation: Continuation::Unavailable,
            execution: document.execution.clone(),
            success,
            recoverable: false,
        };
        let rendered = render(&document, &view, budget)?;
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        while entries.len() >= MAX_ENTRIES
            || entries.iter().map(|e| e.bytes).sum::<usize>() + bytes > MAX_PAYLOAD_BYTES
        {
            entries.pop_front();
        }
        entries.push_back(Entry {
            document: Arc::new(document),
            digests: VecDeque::from([rendered.view.view_digest.clone()]),
            rendered: rendered.clone(),
            bytes,
            budget: budget.min(PAGE_BYTES),
        });
        Ok(rendered)
    }

    pub fn lookup(&self, call_id: &str, content: &str) -> Option<RenderedOutput> {
        let call_id = crate::agent::normalize_tool_call_id(call_id);
        let hash = digest(content.as_bytes());
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|e| e.rendered.view.call_id == call_id && e.digests.contains(&hash))
            .map(|e| e.rendered.clone())
    }

    pub fn project(
        &self,
        call_id: &str,
        content: &str,
        budget: usize,
    ) -> Result<Option<RenderedOutput>, OutputError> {
        let call_id = crate::agent::normalize_tool_call_id(call_id);
        let hash = digest(content.as_bytes());
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries
            .iter_mut()
            .find(|e| e.rendered.view.call_id == call_id && e.digests.contains(&hash))
        else {
            return Ok(None);
        };
        let budget = budget.min(entry.budget);
        entry.budget = budget;
        let rendered = render(&entry.document, &entry.rendered.view, budget)?;
        if !entry.digests.contains(&rendered.view.view_digest) {
            // Keep the initial execution view for history re-projection.
            if entry.digests.len() >= 8 {
                entry.digests.remove(1);
            }
            entry.digests.push_back(rendered.view.view_digest.clone());
        }
        entry.rendered = rendered.clone();
        Ok(Some(rendered))
    }

    /// Final dispatch guard, including bridge failure and direct Agent callers.
    pub fn prepare_messages(
        &self,
        messages: &mut [Message],
        available: usize,
    ) -> Result<(), OutputError> {
        if !self.policy.enabled {
            return Ok(());
        }
        let mut calls = std::collections::HashMap::new();
        let mut targets = Vec::new();
        let mut legacy_bytes = 0usize;
        for (index, message) in messages.iter().enumerate() {
            if let Some(batch) = &message.tool_calls {
                for call in batch {
                    calls.insert(call.id.clone(), call.clone());
                }
            }
            if message.role != MessageRole::Tool {
                continue;
            }
            let call = message
                .tool_call_id
                .as_ref()
                .and_then(|id| calls.remove(id));
            if let Some(call) = call {
                let known = self.lookup(&call.id, &message.content);
                if known.is_some() || Self::supports(&call.name) {
                    targets.push((index, call, known));
                    continue;
                }
            }
            legacy_bytes = legacy_bytes.saturating_add(message.content.len());
        }
        let available = available
            .checked_sub(legacy_bytes)
            .ok_or(OutputError::InsufficientBudget)?;
        let limits: Vec<_> = targets
            .iter()
            .map(|(_, call, _)| octos_core::tool_output_limit(&call.name))
            .collect();
        let allocations = allocate_batch(available, &limits);
        for ((index, call, known), budget) in targets.into_iter().zip(allocations) {
            let message = &mut messages[index];
            let valid = known.is_some_and(|known| {
                known.view.arguments_digest
                    == digest(&serde_json::to_vec(&call.arguments).unwrap_or_default())
            });
            if valid {
                let rendered = self
                    .project(&call.id, &message.content, budget)?
                    .ok_or(OutputError::Missing)?;
                message.content = rendered.content;
            } else {
                let error =
                    "source_incomplete: output metadata missing or changed; recovery unavailable";
                if budget < error.len() {
                    return Err(OutputError::InsufficientBudget);
                }
                message.content = error.into();
            }
        }
        Ok(())
    }
}
