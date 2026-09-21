//! Typed, branch-local output views. Durable payload storage is supplied separately.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use octos_core::{Message, MessageRole};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file_state_cache::FileVersion;
use crate::model_read_receipts::{FileView, ModelReadReceiptStore, ReadReceiptOwner};

pub const PAGE_BYTES: usize = 8192;
pub const MIN_PAGE_BYTES: usize = 512;
const MAX_ENTRIES: usize = crate::output_store::SESSION_ENTRIES;
const MAX_PAYLOAD_BYTES: usize = crate::output_store::SESSION_BYTES as usize;
const MAX_METADATA_BYTES: usize = crate::output_store::MANIFEST_BYTES;
pub(crate) const OBSERVATION_ENTRIES: usize = 128;
const OUTPUT_RECOVERY_POLICY_ID: &str = "output_recovery_v1";

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
pub struct FileReadEvidence {
    version: FileVersion,
    requested_view: FileView,
    line_end: Option<u64>,
    selection_end: u64,
    bounded: bool,
}

impl FileReadEvidence {
    pub(crate) fn new(
        version: FileVersion,
        requested_view: FileView,
        line_end: Option<u64>,
        selection_end: u64,
        bounded: bool,
    ) -> Self {
        Self {
            version,
            requested_view,
            line_end,
            selection_end,
            bounded,
        }
    }

    fn next_arguments(&self, range: &OutputRange) -> Option<serde_json::Value> {
        if range.end >= self.selection_end {
            return None;
        }
        let mut arguments = serde_json::json!({
            "source_sha256": self.version.content_sha256(),
        });
        if let Some((_, line_end)) = range.lines {
            arguments["offset"] = serde_json::json!(line_end + 1);
            if let Some(end) = self.line_end {
                arguments["end_line"] = serde_json::json!(end);
            }
        } else {
            arguments["byte_offset"] = serde_json::json!(range.end);
            if self.bounded {
                arguments["byte_limit"] =
                    serde_json::json!(self.selection_end.saturating_sub(range.end));
            }
        }
        Some(arguments)
    }

    fn visible_view(&self, view: &OutputView) -> Option<FileView> {
        if view.transformed {
            return None;
        }
        if self.requested_view == FileView::Full
            && self.version.size() == 0
            && view.visible_ranges.is_empty()
        {
            return Some(FileView::Full);
        }
        let range = view
            .visible_ranges
            .iter()
            .find(|range| range.stream == OutputStream::File)?;
        if self.requested_view == FileView::Full
            && range.start == 0
            && range.end == self.version.size()
        {
            return Some(FileView::Full);
        }
        match range.lines {
            Some((start, end)) => Some(FileView::Lines { start, end }),
            None => Some(FileView::Bytes {
                start: range.start,
                end: range.end,
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutputDocument {
    pub source: OutputSource,
    pub parts: Vec<OutputPart>,
    pub capture: CaptureState,
    pub execution: ExecutionStatus,
    pub transformed: bool,
    pub loss_reason: Option<String>,
    pub file_read: Option<FileReadEvidence>,
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

    pub(crate) fn running_command() -> Self {
        Self {
            source: OutputSource::Command {
                run_id: String::new(),
            },
            parts: vec![
                OutputPart {
                    stream: OutputStream::Stdout,
                    text: String::new(),
                    start: 0,
                    first_line: None,
                    total: None,
                },
                OutputPart {
                    stream: OutputStream::Stderr,
                    text: String::new(),
                    start: 0,
                    first_line: None,
                    total: None,
                },
            ],
            capture: CaptureState::Running,
            execution: ExecutionStatus::Running,
            transformed: false,
            loss_reason: Some("safe_text_pending_finalization".into()),
            file_read: None,
        }
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
            file_read: None,
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
            file_read: None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_boundary: Option<(OutputStream, u64, String)>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub historical: bool,
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
    StorageFailed,
    Corrupt,
    Expired,
    OwnerMismatch,
    InvalidCursor,
    StaleSource,
    OutOfRange,
    AmbiguousCallId,
    UnsupportedSchema,
    RecoveryUnavailable,
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InsufficientBudget => "insufficient_output_budget",
            Self::SourceIncomplete => "source_incomplete",
            Self::StorageLimit => "storage_limit",
            Self::Missing => "output_missing",
            Self::StorageFailed => "storage_failed",
            Self::Corrupt => "output_corrupt",
            Self::Expired => "output_expired",
            Self::OwnerMismatch => "owner_mismatch",
            Self::InvalidCursor => "invalid_cursor",
            Self::StaleSource => "stale_source",
            Self::OutOfRange => "out_of_range",
            Self::AmbiguousCallId => "ambiguous_call_id",
            Self::UnsupportedSchema => "unsupported_output_schema",
            Self::RecoveryUnavailable => "recovery_tool_unavailable",
        })
    }
}

impl std::error::Error for OutputError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutputRecoveryObservation {
    pub(crate) operation: &'static str,
    pub(crate) layer: &'static str,
    pub(crate) outcome: &'static str,
    pub(crate) reason: &'static str,
    pub(crate) policy_id: &'static str,
    pub(crate) captured_bytes: u64,
    pub(crate) stored_bytes: u64,
    pub(crate) visible_bytes: u64,
    pub(crate) known_omitted_bytes: u64,
    pub(crate) unknown_total: bool,
    pub(crate) stream: Option<OutputStream>,
    pub(crate) range: Option<(u64, u64)>,
    pub(crate) repeated: bool,
    pub(crate) termination: Option<&'static str>,
    pub(crate) artifact: Option<&'static str>,
}

impl OutputRecoveryObservation {
    fn event(
        operation: &'static str,
        layer: &'static str,
        outcome: &'static str,
        reason: &'static str,
    ) -> Self {
        Self {
            operation,
            layer,
            outcome,
            reason,
            policy_id: OUTPUT_RECOVERY_POLICY_ID,
            captured_bytes: 0,
            stored_bytes: 0,
            visible_bytes: 0,
            known_omitted_bytes: 0,
            unknown_total: false,
            stream: None,
            range: None,
            repeated: false,
            termination: None,
            artifact: None,
        }
    }
}

#[derive(Debug, Default)]
struct ObservationState {
    events: VecDeque<OutputRecoveryObservation>,
    recall_digests: VecDeque<String>,
    terminal_command_ids: VecDeque<String>,
}

fn output_error_reason(error: OutputError) -> &'static str {
    match error {
        OutputError::InsufficientBudget => "insufficient_budget",
        OutputError::SourceIncomplete => "source_incomplete",
        OutputError::StorageLimit => "storage_limit",
        OutputError::Missing => "missing",
        OutputError::StorageFailed => "storage_failed",
        OutputError::Corrupt => "corrupt",
        OutputError::Expired => "expired",
        OutputError::OwnerMismatch => "owner_mismatch",
        OutputError::InvalidCursor => "invalid_cursor",
        OutputError::StaleSource => "stale_source",
        OutputError::OutOfRange => "out_of_range",
        OutputError::AmbiguousCallId => "ambiguous_call_id",
        OutputError::UnsupportedSchema => "unsupported_schema",
        OutputError::RecoveryUnavailable => "recovery_unavailable",
    }
}

fn availability_reason(availability: &Availability) -> &'static str {
    match availability {
        Availability::Available => "available",
        Availability::Missing => "missing",
        Availability::Expired => "expired",
        Availability::StoreFailed => "store_failed",
        Availability::Corrupt => "corrupt",
    }
}

fn continuation_reason(continuation: &Continuation) -> &'static str {
    match continuation {
        Continuation::Next { .. } => "next",
        Continuation::SelectionEnd => "selection_end",
        Continuation::Eof => "eof",
        Continuation::Pending => "pending",
        Continuation::Unavailable => "unavailable",
    }
}

fn capture_reason(capture: &CaptureState) -> &'static str {
    match capture {
        CaptureState::Running => "running",
        CaptureState::Complete => "complete",
        CaptureState::Partial => "partial",
    }
}

fn execution_reason(execution: &ExecutionStatus) -> &'static str {
    match execution {
        ExecutionStatus::NotApplicable => "not_applicable",
        ExecutionStatus::Exited { code: Some(0), .. } => "exited_zero",
        ExecutionStatus::Exited { code: Some(_), .. } => "exited_nonzero",
        ExecutionStatus::Exited { code: None, .. } => "exited_unknown",
        ExecutionStatus::TimedOut => "timed_out",
        ExecutionStatus::Cancelled => "cancelled",
        ExecutionStatus::Running => "running",
        ExecutionStatus::Unknown => "unknown",
    }
}

fn range_bytes(ranges: &[OutputRange]) -> u64 {
    ranges
        .iter()
        .map(|range| range.end.saturating_sub(range.start))
        .sum()
}

fn captured_bytes(document: &OutputDocument) -> (u64, bool) {
    let command = matches!(document.source, OutputSource::Command { .. });
    document
        .parts
        .iter()
        .filter(|part| part.stream != OutputStream::Display)
        .fold((0u64, false), |(bytes, unknown), part| {
            let captured = if command {
                part.total.unwrap_or(part.text.len() as u64)
            } else {
                part.text.len() as u64
            };
            (
                bytes.saturating_add(captured),
                unknown || part.total.is_none(),
            )
        })
}

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

fn suffix_start(text: &str, limit: usize) -> usize {
    let mut start = text.len().saturating_sub(limit);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

fn render_command_head_tail(
    part: &OutputPart,
    allowance: usize,
) -> Option<(String, Vec<OutputRange>)> {
    const LABEL_AND_GAP_RESERVE: usize = 256;
    let payload = allowance.checked_sub(LABEL_AND_GAP_RESERVE)?;
    if payload < 128 {
        return None;
    }
    let head_end = prefix_end(&part.text, payload * 2 / 3);
    let tail_start = suffix_start(&part.text, payload - head_end);
    if head_end == 0 || tail_start <= head_end || tail_start >= part.text.len() {
        return None;
    }
    let head_start = part.start;
    let head_upper = head_start + head_end as u64;
    let tail_lower = head_start + tail_start as u64;
    let tail_upper = head_start + part.text.len() as u64;
    let body = format!(
        "\n--- {:?} [{head_start}..{head_upper}] ---\n{}\n\
         ... [{:?} bytes {head_upper}..{tail_lower} omitted from this preview; use recall] ...\n\
         --- {:?} [{tail_lower}..{tail_upper}] ---\n{}",
        part.stream,
        &part.text[..head_end],
        part.stream,
        part.stream,
        &part.text[tail_start..],
    );
    if body.len() > allowance {
        return None;
    }
    Some((
        body,
        vec![
            OutputRange {
                stream: part.stream,
                start: head_start,
                end: head_upper,
                lines: None,
            },
            OutputRange {
                stream: part.stream,
                start: tail_lower,
                end: tail_upper,
                lines: None,
            },
        ],
    ))
}

fn render_parts(
    document: &OutputDocument,
    original: &OutputView,
    budget: usize,
) -> (String, Vec<OutputRange>) {
    let mut body = String::new();
    let mut ranges = Vec::new();
    let nonempty: Vec<_> = document
        .parts
        .iter()
        .filter(|p| !p.text.is_empty())
        .collect();
    let allocations = allocate_batch(budget, &vec![budget; nonempty.len()]);
    for (part, allowance) in nonempty.into_iter().zip(allocations) {
        let head_tail = matches!(&document.source, OutputSource::Command { .. })
            && !original.historical
            && matches!(part.stream, OutputStream::Stdout | OutputStream::Stderr);
        if head_tail && let Some((rendered, visible)) = render_command_head_tail(part, allowance) {
            body.push_str(&rendered);
            ranges.extend(visible);
            continue;
        }
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
        let (body, ranges) = render_parts(document, original, body_budget);
        let mut view = original.clone();
        view.visible_ranges = ranges;
        let next: Vec<_> = document
            .parts
            .iter()
            .filter(|p| p.stream != OutputStream::Display)
            .filter_map(|part| {
                let mut end = part.start;
                for range in view
                    .visible_ranges
                    .iter()
                    .filter(|range| range.stream == part.stream)
                {
                    if range.start > end {
                        break;
                    }
                    end = end.max(range.end);
                }
                let upper = view
                    .recovery_boundary
                    .as_ref()
                    .filter(|(stream, _, _)| *stream == part.stream)
                    .map(|(_, upper, _)| *upper)
                    .unwrap_or(part.start + part.text.len() as u64);
                (end < upper).then_some((part.stream, end))
            })
            .collect();
        view.continuation = if !next.is_empty() {
            Continuation::Next { positions: next }
        } else if view.capture == CaptureState::Running {
            Continuation::Pending
        } else if view.capture == CaptureState::Partial {
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
        let mut header = serde_json::json!({
            "output_id": view.output_id,
            "ranges": view.visible_ranges,
            "source_totals": view.source_totals,
            "coordinates": if view.transformed { "safe_text_bytes" } else { "source_bytes_except_display" },
            "capture": view.capture,
            "execution": view.execution,
            "success": view.success,
            "next": view.continuation,
            "recoverable": view.recoverable,
            "recovery_error": if view.recoverable { None } else { Some("recovery_tool_unavailable") },
            "loss": view.loss_reason,
        });
        if !view.transformed
            && matches!(view.continuation, Continuation::Next { .. })
            && let Some(arguments) = document.file_read.as_ref().and_then(|evidence| {
                view.visible_ranges
                    .iter()
                    .find(|range| range.stream == OutputStream::File)
                    .and_then(|range| evidence.next_arguments(range))
            })
        {
            header["read_file_next"] = serde_json::json!({
                "same_path": true,
                "arguments": arguments,
            });
        }
        if view.recoverable {
            header["stored"] = serde_json::json!(view.stored_ranges);
            header["historical"] = serde_json::json!(view.historical);
            if !view.historical || view.continuation == Continuation::Pending {
                header["recall"] = serde_json::json!({"output_id": view.output_id});
            }
            if let Continuation::Next { positions } = &view.continuation
                && let Some((stream, position)) = positions.first()
            {
                header["recall"] = if let Some(cursor) =
                    crate::output_store::continuation_cursor(&view, *stream, *position)
                {
                    serde_json::json!({"cursor": cursor})
                } else {
                    serde_json::json!({
                        "output_id": view.output_id,
                        "stream": stream,
                        "offset": position,
                    })
                };
            } else if view.historical
                && view.continuation == Continuation::SelectionEnd
                && let Some(range) = view.visible_ranges.first()
                && view
                    .stored_ranges
                    .iter()
                    .any(|stored| stored.stream == range.stream && stored.end > range.end)
            {
                header["recall"] = serde_json::json!({
                    "output_id": view.output_id,
                    "stream": range.stream,
                    "offset": range.end,
                });
            }
        }
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
    /// Final provider projections already offered to the H02 receipt state.
    receipt_digests: VecDeque<String>,
    bytes: usize,
    budget: usize,
}

#[derive(Debug)]
pub struct OutputState {
    pub policy: OutputPolicy,
    pub owner: ReadReceiptOwner,
    entries: Mutex<VecDeque<Entry>>,
    store: Mutex<Option<Arc<crate::output_store::OutputStore>>>,
    observations: Mutex<ObservationState>,
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
            store: Mutex::new(None),
            observations: Mutex::new(ObservationState::default()),
        }
    }

    fn observe(&self, observation: OutputRecoveryObservation) {
        metrics::counter!(
            "octos_output_recovery_events_total",
            "operation" => observation.operation,
            "layer" => observation.layer,
            "outcome" => observation.outcome,
            "reason" => observation.reason,
            "policy" => observation.policy_id,
        )
        .increment(1);
        match observation.operation {
            "capture" => {
                metrics::counter!(
                    "octos_output_recovery_bytes_total",
                    "stage" => "captured",
                    "policy" => observation.policy_id,
                )
                .increment(observation.captured_bytes);
            }
            "save" => {
                metrics::counter!(
                    "octos_output_recovery_bytes_total",
                    "stage" => "stored",
                    "policy" => observation.policy_id,
                )
                .increment(observation.stored_bytes);
                metrics::counter!(
                    "octos_output_recovery_bytes_total",
                    "stage" => "omitted_at_save",
                    "policy" => observation.policy_id,
                )
                .increment(observation.known_omitted_bytes);
            }
            "render" => {
                metrics::counter!(
                    "octos_output_recovery_bytes_total",
                    "stage" => "visible",
                    "policy" => observation.policy_id,
                )
                .increment(observation.visible_bytes);
                metrics::counter!(
                    "octos_output_recovery_bytes_total",
                    "stage" => "omitted_at_render",
                    "policy" => observation.policy_id,
                )
                .increment(observation.known_omitted_bytes);
            }
            _ => {}
        }
        if observation.unknown_total {
            metrics::counter!(
                "octos_output_recovery_unknown_totals_total",
                "stage" => observation.operation,
                "policy" => observation.policy_id,
            )
            .increment(1);
        }
        tracing::debug!(
            target: "octos.output_recovery",
            operation = observation.operation,
            layer = observation.layer,
            outcome = observation.outcome,
            reason = observation.reason,
            policy = observation.policy_id,
            captured_bytes = observation.captured_bytes,
            stored_bytes = observation.stored_bytes,
            visible_bytes = observation.visible_bytes,
            known_omitted_bytes = observation.known_omitted_bytes,
            unknown_total = observation.unknown_total,
            stream = ?observation.stream,
            range_start = observation.range.map(|range| range.0),
            range_end = observation.range.map(|range| range.1),
            repeated = observation.repeated,
            termination = observation.termination.unwrap_or("none"),
            artifact = observation.artifact.unwrap_or("none"),
            "output recovery event"
        );
        let mut state = self
            .observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.events.len() >= OBSERVATION_ENTRIES {
            state.events.pop_front();
        }
        state.events.push_back(observation);
    }

    fn observe_render(
        &self,
        layer: &'static str,
        captured: u64,
        unknown: bool,
        result: &Result<RenderedOutput, OutputError>,
    ) {
        let mut observation = match result {
            Ok(rendered) => {
                let visible = range_bytes(&rendered.view.visible_ranges);
                let reason = if rendered.view.transformed {
                    "source_transformed"
                } else if visible < captured {
                    "output_budget"
                } else {
                    continuation_reason(&rendered.view.continuation)
                };
                let mut event =
                    OutputRecoveryObservation::event("render", layer, "success", reason);
                event.stored_bytes = rendered.view.stored_bytes;
                event.visible_bytes = visible;
                event.known_omitted_bytes = if unknown {
                    0
                } else {
                    captured.saturating_sub(visible)
                };
                event.stream = rendered
                    .view
                    .visible_ranges
                    .first()
                    .map(|range| range.stream);
                event.range = rendered
                    .view
                    .visible_ranges
                    .first()
                    .map(|range| (range.start, range.end));
                event
            }
            Err(error) => OutputRecoveryObservation::event(
                "render",
                layer,
                "failure",
                output_error_reason(*error),
            ),
        };
        observation.captured_bytes = captured;
        observation.unknown_total = unknown;
        self.observe(observation);
    }

    pub(crate) fn observe_recall_request(
        &self,
        request: &crate::output_store::RecallRequest,
    ) -> bool {
        let request_digest = digest(&serde_json::to_vec(request).unwrap_or_default());
        let repeated = {
            let mut state = self
                .observations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let repeated = state.recall_digests.contains(&request_digest);
            if !repeated {
                if state.recall_digests.len() >= OBSERVATION_ENTRIES {
                    state.recall_digests.pop_front();
                }
                state.recall_digests.push_back(request_digest);
            }
            repeated
        };
        let reason = if request.cursor.is_some() {
            "cursor"
        } else if request.page.is_some() {
            "page"
        } else if request.offset.is_some() || request.limit.is_some() {
            "range"
        } else if request.tool_call_id.is_some() {
            "legacy_call"
        } else {
            "output"
        };
        let mut observation =
            OutputRecoveryObservation::event("recall", "request", "attempt", reason);
        observation.stream = request.stream;
        observation.range = request.offset.and_then(|start| {
            request
                .limit
                .and_then(|limit| start.checked_add(limit).map(|end| (start, end)))
        });
        observation.repeated = repeated;
        self.observe(observation);
        repeated
    }

    pub(crate) fn observe_recall_result(
        &self,
        result: &Result<RenderedOutput, OutputError>,
        repeated: bool,
    ) {
        let mut observation = match result {
            Ok(rendered) => {
                let visible = range_bytes(&rendered.view.visible_ranges);
                let reason = if matches!(rendered.view.continuation, Continuation::Next { .. }) {
                    if rendered
                        .view
                        .visible_ranges
                        .first()
                        .is_some_and(|range| range.end > range.start)
                    {
                        "strict_progress"
                    } else {
                        "no_progress"
                    }
                } else {
                    continuation_reason(&rendered.view.continuation)
                };
                let mut event =
                    OutputRecoveryObservation::event("recall", "result", "success", reason);
                event.stored_bytes = rendered.view.stored_bytes;
                event.visible_bytes = visible;
                event.unknown_total = rendered.view.transformed
                    || rendered
                        .view
                        .source_totals
                        .iter()
                        .any(|(_, total)| total.is_none());
                event.stream = rendered
                    .view
                    .visible_ranges
                    .first()
                    .map(|range| range.stream);
                event.range = rendered
                    .view
                    .visible_ranges
                    .first()
                    .map(|range| (range.start, range.end));
                event.artifact = Some(availability_reason(&rendered.view.availability));
                event
            }
            Err(error) => OutputRecoveryObservation::event(
                "recall",
                "result",
                "failure",
                output_error_reason(*error),
            ),
        };
        observation.repeated = repeated;
        metrics::counter!(
            "octos_output_recovery_recall_total",
            "outcome" => observation.outcome,
            "reason" => observation.reason,
            "repeated" => if repeated { "true" } else { "false" },
            "policy" => observation.policy_id,
        )
        .increment(1);
        self.observe(observation);
    }

    fn observe_command_terminal(&self, view: &OutputView) {
        if !matches!(view.source, OutputSource::Command { .. })
            || matches!(view.execution, ExecutionStatus::Running)
        {
            return;
        }
        let first = {
            let mut state = self
                .observations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.terminal_command_ids.contains(&view.output_id) {
                false
            } else {
                if state.terminal_command_ids.len() >= OBSERVATION_ENTRIES {
                    state.terminal_command_ids.pop_front();
                }
                state.terminal_command_ids.push_back(view.output_id.clone());
                true
            }
        };
        if !first {
            return;
        }
        let termination = execution_reason(&view.execution);
        let artifact = availability_reason(&view.availability);
        metrics::counter!(
            "octos_output_recovery_command_executions_total",
            "termination" => termination,
            "artifact" => artifact,
            "policy" => OUTPUT_RECOVERY_POLICY_ID,
        )
        .increment(1);
        let mut observation =
            OutputRecoveryObservation::event("command", "execution", "terminal", termination);
        observation.stored_bytes = view.stored_bytes;
        observation.termination = Some(termination);
        observation.artifact = Some(artifact);
        self.observe(observation);
    }

    #[cfg(test)]
    pub(crate) fn observations(&self) -> Vec<OutputRecoveryObservation> {
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .events
            .iter()
            .cloned()
            .collect()
    }

    pub fn enable_store(&self, data_dir: &std::path::Path) -> Result<(), OutputError> {
        if self.policy.enabled {
            let store = crate::output_store::OutputStore::open(data_dir, self.owner.clone())?;
            *self.store.lock().map_err(|_| OutputError::StorageFailed)? = Some(store);
        }
        Ok(())
    }

    pub fn store(&self) -> Option<Arc<crate::output_store::OutputStore>> {
        self.store.lock().ok()?.clone()
    }

    pub fn supports(name: &str) -> bool {
        matches!(
            name,
            "read_file" | "shell" | "bash" | "exec_command" | "recall"
        )
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
        if name == "recall" && self.lookup(call_id, &result.output).is_some() {
            // Already registered by the typed recall entry. Do not sanitize or spill again.
            if let Some(feedback) = feedback {
                let content_digest = digest(result.output.as_bytes());
                let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(entry) = entries.iter_mut().rev().find(|e| {
                    e.rendered.view.call_id == crate::agent::normalize_tool_call_id(call_id)
                        && e.digests.contains(&content_digest)
                }) {
                    entry.document = Arc::new((*entry.document).clone().with_notice(
                        &crate::sanitize::sanitize_tool_output(&format!("[hook] {feedback}")),
                    ));
                    let (captured, unknown_total) = captured_bytes(&entry.document);
                    let rendered = render(&entry.document, &entry.rendered.view, entry.budget);
                    self.observe_render("hook", captured, unknown_total, &rendered);
                    if let Ok(rendered) = rendered {
                        entry.digests.push_back(rendered.view.view_digest.clone());
                        result.output = rendered.content.clone();
                        entry.rendered = rendered;
                    } else {
                        result.output = OutputError::InsufficientBudget.to_string();
                    }
                }
            }
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
        let (captured, source_total_unknown) = captured_bytes(&document);
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
            let mut observation = OutputRecoveryObservation::event(
                "capture",
                "source",
                "failure",
                "source_incomplete",
            );
            observation.captured_bytes = captured;
            observation.unknown_total = source_total_unknown;
            self.observe(observation);
            return Err(OutputError::SourceIncomplete);
        }
        let mut capture_observation = OutputRecoveryObservation::event(
            "capture",
            "source",
            "success",
            capture_reason(&document.capture),
        );
        capture_observation.captured_bytes = captured;
        capture_observation.unknown_total = source_total_unknown;
        self.observe(capture_observation);
        document.sanitize();
        let projection_total_unknown = source_total_unknown || document.transformed;
        if document.transformed && document.file_read.is_some() && document.loss_reason.is_none() {
            document.loss_reason = Some("source_transformed".into());
        }
        if let OutputSource::Command { run_id } = &mut document.source {
            *run_id = id.clone();
        }
        let mut view = OutputView {
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
                .filter(|p| p.stream != OutputStream::Display && !p.text.is_empty())
                .map(|p| OutputRange {
                    stream: p.stream,
                    start: p.start,
                    end: p.start + p.text.len() as u64,
                    lines: None,
                })
                .collect(),
            source_totals: document
                .parts
                .iter()
                .filter(|p| p.stream != OutputStream::Display)
                .map(|p| (p.stream, p.total))
                .collect(),
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
            recovery_boundary: None,
            historical: false,
        };
        let mut save_outcome = "skipped";
        let mut save_reason = "store_unavailable";
        if let Some(store) = self.store() {
            match store.save(&view, &document) {
                Ok(stored) => {
                    view = stored;
                    save_outcome = "success";
                    save_reason = "available";
                }
                Err(error) => {
                    view.availability = if error == OutputError::Corrupt {
                        Availability::Corrupt
                    } else {
                        Availability::StoreFailed
                    };
                    view.loss_reason = Some(error.to_string());
                    save_outcome = "failure";
                    save_reason = output_error_reason(error);
                }
            }
        }
        let mut save_observation =
            OutputRecoveryObservation::event("save", "artifact", save_outcome, save_reason);
        save_observation.captured_bytes = captured;
        save_observation.stored_bytes = view.stored_bytes;
        save_observation.known_omitted_bytes = if projection_total_unknown {
            0
        } else {
            captured.saturating_sub(view.stored_bytes)
        };
        save_observation.unknown_total = projection_total_unknown;
        save_observation.artifact = Some(availability_reason(&view.availability));
        self.observe(save_observation);
        self.observe_command_terminal(&view);
        let mut remaining = crate::output_store::OUTPUT_BYTES;
        for part in &mut document.parts {
            let stored = view
                .recoverable
                .then(|| {
                    view.stored_ranges
                        .iter()
                        .find(|range| range.stream == part.stream && range.start == part.start)
                        .map(|range| range.end.saturating_sub(range.start) as usize)
                })
                .flatten()
                .unwrap_or(part.text.len());
            let end = prefix_end(&part.text, remaining.min(stored));
            if end < part.text.len() {
                part.text.truncate(end);
                if !view.recoverable {
                    view.loss_reason = Some(OutputError::StorageLimit.to_string());
                    view.capture = CaptureState::Partial;
                }
            }
            remaining -= end;
        }
        let rendered = render(&document, &view, budget);
        self.observe_render("initial", captured, projection_total_unknown, &rendered);
        let rendered = rendered?;
        self.remember(document, rendered.clone(), budget);
        Ok(rendered)
    }

    pub fn register_recalled(
        &self,
        call_id: &str,
        args: &serde_json::Value,
        mut recalled: crate::output_store::RecalledOutput,
    ) -> Result<RenderedOutput, OutputError> {
        let (captured, unknown_total) = captured_bytes(&recalled.document);
        if recalled.rendered.view.owner != self.owner {
            self.observe_render(
                "recall",
                captured,
                unknown_total,
                &Err(OutputError::OwnerMismatch),
            );
            return Err(OutputError::OwnerMismatch);
        }
        recalled.rendered.view.call_id = crate::agent::normalize_tool_call_id(call_id);
        recalled.rendered.view.arguments_digest =
            digest(&serde_json::to_vec(args).unwrap_or_default());
        let rendered = render(&recalled.document, &recalled.rendered.view, PAGE_BYTES);
        self.observe_render("recall", captured, unknown_total, &rendered);
        let rendered = rendered?;
        self.remember(recalled.document, rendered.clone(), PAGE_BYTES);
        Ok(rendered)
    }

    fn remember(&self, document: OutputDocument, rendered: RenderedOutput, budget: usize) {
        let bytes = document.parts.iter().map(|p| p.text.len()).sum::<usize>();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        while entries.len() >= MAX_ENTRIES
            || entries.iter().map(|e| e.bytes).sum::<usize>() + bytes > MAX_PAYLOAD_BYTES
        {
            entries.pop_front();
        }
        entries.push_back(Entry {
            document: Arc::new(document),
            digests: VecDeque::from([rendered.view.view_digest.clone()]),
            receipt_digests: VecDeque::new(),
            rendered: rendered.clone(),
            bytes,
            budget: budget.min(PAGE_BYTES),
        });
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
        let (captured, unknown_total) = captured_bytes(&entry.document);
        let rendered = render(&entry.document, &entry.rendered.view, budget);
        self.observe_render("projection", captured, unknown_total, &rendered);
        let rendered = rendered?;
        if !entry.digests.contains(&rendered.view.view_digest) {
            // Keep the initial execution view for history re-projection.
            if entry.digests.len() >= 8 {
                if let Some(removed) = entry.digests.remove(1) {
                    entry.receipt_digests.retain(|digest| digest != &removed);
                }
            }
            entry.digests.push_back(rendered.view.view_digest.clone());
        }
        entry.rendered = rendered.clone();
        Ok(Some(rendered))
    }

    /// Stage only file ranges present in the exact request passed to the main model.
    pub(crate) fn stage_file_reads(&self, messages: &[Message], receipts: &ModelReadReceiptStore) {
        if !self.policy.enabled {
            return;
        }
        let mut open_calls = VecDeque::new();
        for message in messages {
            match message.role {
                MessageRole::Assistant => {
                    open_calls.clear();
                    open_calls.extend(message.tool_calls.iter().flatten().cloned());
                }
                MessageRole::Tool => {
                    let Some(call_id) = message.tool_call_id.as_deref() else {
                        continue;
                    };
                    let call_id = crate::agent::normalize_tool_call_id(call_id);
                    let Some(index) = open_calls
                        .iter()
                        .position(|call| crate::agent::normalize_tool_call_id(&call.id) == call_id)
                    else {
                        continue;
                    };
                    let Some(call) = open_calls.remove(index) else {
                        continue;
                    };
                    if call.name != "read_file" {
                        continue;
                    }
                    let content_digest = digest(message.content.as_bytes());
                    let arguments_digest =
                        digest(&serde_json::to_vec(&call.arguments).unwrap_or_default());
                    let candidate = {
                        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
                        entries
                            .iter_mut()
                            .find(|entry| {
                                entry.rendered.view.call_id == call_id
                                    && entry.rendered.view.arguments_digest == arguments_digest
                                    && entry.rendered.view.view_digest == content_digest
                                    && entry.digests.contains(&content_digest)
                                    && !entry.receipt_digests.contains(&content_digest)
                                    && entry.document.file_read.is_some()
                            })
                            .and_then(|entry| {
                                let evidence = entry.document.file_read.as_ref()?;
                                let returned_view = evidence.visible_view(&entry.rendered.view)?;
                                let requested_view =
                                    match (&evidence.requested_view, &returned_view) {
                                        (
                                            FileView::Full | FileView::Lines { .. },
                                            FileView::Bytes { .. },
                                        ) => returned_view.clone(),
                                        _ => evidence.requested_view.clone(),
                                    };
                                entry.receipt_digests.push_back(content_digest.clone());
                                Some((evidence.version.clone(), requested_view, returned_view))
                            })
                    };
                    if let Some((version, requested_view, returned_view)) = candidate {
                        receipts.stage(
                            &call.id,
                            &call.arguments,
                            version,
                            requested_view,
                            returned_view,
                            &message.content,
                        );
                    }
                }
                MessageRole::System | MessageRole::User => open_calls.clear(),
            }
        }
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
                self.observe(OutputRecoveryObservation::event(
                    "render",
                    "provider",
                    "failure",
                    "source_incomplete",
                ));
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
