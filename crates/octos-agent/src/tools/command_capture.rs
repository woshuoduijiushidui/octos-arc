//! Bounded foreground-command capture shared by shell-compatible tools.

use std::collections::VecDeque;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::ToolContext;
use crate::output_recovery::{
    CaptureState, ExecutionStatus, OutputDocument, OutputPart, OutputSource, OutputState,
    OutputStream, PAGE_BYTES,
};

// During pipe draining, read buffer + tail + UTF-8 validation scratch stays
// within the frozen 64 KiB per-stream capture-memory budget.
const READ_BUFFER_BYTES: usize = 15 * 1024;
const TAIL_BUFFER_BYTES: usize = 32 * 1024;
const STREAM_BYTES: usize = crate::output_store::STREAM_BYTES;
const ACTIVE_CAPTURES: usize = 8;

const TERMINATION_NONE: u8 = 0;
const TERMINATION_TIMED_OUT: u8 = 1;
const TERMINATION_CANCELLED: u8 = 2;

fn capture_slot() -> Option<OwnedSemaphorePermit> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(ACTIVE_CAPTURES)))
        .clone()
        .try_acquire_owned()
        .ok()
}

#[derive(Clone, Debug, Default)]
pub(super) struct CaptureTermination(Arc<AtomicU8>);

impl CaptureTermination {
    pub(super) fn mark_timed_out(&self) {
        let _ = self.0.compare_exchange(
            TERMINATION_NONE,
            TERMINATION_TIMED_OUT,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(super) fn mark_cancelled(&self) {
        self.0.store(TERMINATION_CANCELLED, Ordering::Release);
    }

    fn status(&self) -> u8 {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub(super) struct RecoveryRegistration {
    state: Arc<OutputState>,
    output_id: String,
    call_id: String,
    args: serde_json::Value,
}

impl RecoveryRegistration {
    pub(super) fn from_context(ctx: &ToolContext, args: &serde_json::Value) -> Option<Self> {
        let state = ctx
            .output_state
            .as_ref()
            .filter(|state| state.policy.enabled)?
            .clone();
        if ctx.output_id.is_empty() || ctx.tool_id.is_empty() {
            return None;
        }
        Some(Self {
            state,
            output_id: ctx.output_id.clone(),
            call_id: ctx.tool_id.clone(),
            args: args.clone(),
        })
    }

    pub(super) fn current(args: &serde_json::Value) -> Option<Self> {
        super::TOOL_CTX
            .try_with(|ctx| Self::from_context(ctx, args))
            .ok()
            .flatten()
    }

    pub(super) fn register_running(&self) {
        if let Err(error) = self.state.register(
            self.output_id.clone(),
            &self.call_id,
            &self.args,
            OutputDocument::running_command(),
            false,
            PAGE_BYTES,
        ) {
            tracing::warn!(%error, "failed to publish running command capture");
        }
    }

    fn register_cancelled(&self, capture: &CapturedCommand) {
        if let Err(error) = self.state.register(
            self.output_id.clone(),
            &self.call_id,
            &self.args,
            capture.document(),
            false,
            PAGE_BYTES,
        ) {
            tracing::warn!(%error, "failed to publish cancelled command capture");
        }
    }
}

#[derive(Debug)]
struct CapturedStream {
    stream: OutputStream,
    text: String,
    start: u64,
    total: Option<u64>,
    unrecoverable_tail: bool,
    limited: bool,
    failed: bool,
    transformed: bool,
}

impl CapturedStream {
    fn part(&self) -> OutputPart {
        OutputPart {
            stream: self.stream,
            text: self.text.clone(),
            start: self.start,
            first_line: None,
            total: self.total,
        }
    }

    fn tail_notice(&self) -> Option<String> {
        if !self.unrecoverable_tail {
            return None;
        }
        // The omitted middle and this tail were not sanitized as one value.
        // Showing either fragment could expose a secret split across the gap.
        Some(format!(
            "{:?} tail is not recoverable; output beyond the saved range was drained but not retained",
            self.stream
        ))
    }
}

#[derive(Debug)]
pub(super) struct CapturedCommand {
    stdout: CapturedStream,
    stderr: CapturedStream,
    status: Option<ExitStatus>,
    wait_error: Option<String>,
    termination: CaptureTermination,
}

impl CapturedCommand {
    pub(super) fn success(&self) -> bool {
        self.termination.status() == TERMINATION_NONE
            && self.status.as_ref().is_some_and(ExitStatus::success)
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        self.status.as_ref().and_then(ExitStatus::code)
    }

    pub(super) fn wait_error(&self) -> Option<&str> {
        self.wait_error.as_deref()
    }

    pub(super) fn text(&self) -> String {
        let mut text = self.stdout.text.clone();
        if !self.stderr.text.is_empty() {
            if !text.is_empty() {
                text.push_str("\n--- stderr ---\n");
            }
            text.push_str(&self.stderr.text);
        }
        if text.is_empty() {
            text.push_str("(no output)");
        }
        text
    }

    pub(super) fn document(&self) -> OutputDocument {
        let execution = match self.termination.status() {
            TERMINATION_TIMED_OUT => ExecutionStatus::TimedOut,
            TERMINATION_CANCELLED => ExecutionStatus::Cancelled,
            _ => self
                .status
                .map(ExecutionStatus::exited)
                .unwrap_or(ExecutionStatus::Unknown),
        };
        let mut reasons = Vec::new();
        match self.termination.status() {
            TERMINATION_TIMED_OUT => reasons.push("command_timed_out"),
            TERMINATION_CANCELLED => reasons.push("command_cancelled"),
            _ if self.wait_error.is_some() => reasons.push("command_wait_failed"),
            _ => {}
        }
        if self.stdout.limited || self.stderr.limited {
            reasons.push("capture_limit");
        }
        if self.stdout.failed || self.stderr.failed {
            reasons.push("capture_store_or_read_failed");
        }
        let complete = reasons.is_empty();
        let mut parts = vec![self.stdout.part(), self.stderr.part()];
        for stream in [&self.stdout, &self.stderr] {
            if let Some(notice) = stream.tail_notice() {
                parts.push(OutputPart {
                    stream: OutputStream::Display,
                    text: notice,
                    start: 0,
                    first_line: None,
                    total: None,
                });
            }
        }
        OutputDocument {
            source: OutputSource::Command {
                run_id: String::new(),
            },
            parts,
            capture: if complete {
                CaptureState::Complete
            } else {
                CaptureState::Partial
            },
            execution,
            transformed: self.stdout.transformed || self.stderr.transformed,
            loss_reason: (!reasons.is_empty()).then(|| reasons.join(",")),
            file_read: None,
        }
    }
}

#[derive(Default)]
struct Utf8Validator {
    pending: Vec<u8>,
    invalid: bool,
}

impl Utf8Validator {
    fn push(&mut self, chunk: &[u8]) {
        let mut bytes = Vec::with_capacity(self.pending.len() + chunk.len());
        bytes.extend_from_slice(&self.pending);
        bytes.extend_from_slice(chunk);
        self.pending.clear();
        let mut remaining = bytes.as_slice();
        loop {
            match std::str::from_utf8(remaining) {
                Ok(_) => return,
                Err(error) => {
                    let valid = error.valid_up_to();
                    if let Some(length) = error.error_len() {
                        self.invalid = true;
                        remaining = &remaining[valid + length..];
                    } else {
                        self.pending.extend_from_slice(&remaining[valid..]);
                        return;
                    }
                }
            }
        }
    }

    fn finish(mut self) -> bool {
        if !self.pending.is_empty() {
            self.invalid = true;
        }
        !self.invalid
    }
}

fn append_tail(tail: &mut VecDeque<u8>, chunk: &[u8]) {
    if chunk.len() >= TAIL_BUFFER_BYTES {
        tail.clear();
        tail.extend(&chunk[chunk.len() - TAIL_BUFFER_BYTES..]);
        return;
    }
    let overflow = tail
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(TAIL_BUFFER_BYTES);
    tail.drain(..overflow);
    tail.extend(chunk);
}

async fn drain_stream<R>(mut reader: R, stream: OutputStream, spool_allowed: bool) -> CapturedStream
where
    R: AsyncRead + Unpin,
{
    let mut spool = spool_allowed
        .then(tempfile::tempfile)
        .transpose()
        .ok()
        .flatten()
        .map(tokio::fs::File::from_std);
    let mut failed = spool.is_none();
    let mut total = 0_u64;
    let mut eof = false;
    let mut tail = VecDeque::with_capacity(TAIL_BUFFER_BYTES);
    let mut validator = Utf8Validator::default();
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES];

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(read) => read,
            Err(_) => {
                failed = true;
                break;
            }
        };
        let chunk = &buffer[..read];
        validator.push(chunk);
        append_tail(&mut tail, chunk);
        let before = total;
        total = total.saturating_add(read as u64);
        if let Some(file) = spool.as_mut() {
            let remaining = STREAM_BYTES.saturating_sub(before.min(STREAM_BYTES as u64) as usize);
            let write = remaining.min(read);
            if write > 0 && file.write_all(&chunk[..write]).await.is_err() {
                failed = true;
                spool = None;
            }
        }
    }

    let valid_utf8 = validator.finish();
    let mut captured = Vec::new();
    if let Some(mut file) = spool {
        if file.flush().await.is_err()
            || file.rewind().await.is_err()
            || file.read_to_end(&mut captured).await.is_err()
        {
            failed = true;
            captured.clear();
        }
    }
    let tail: Vec<u8> = tail.into_iter().collect();
    let (start, source) = if captured.is_empty() && total > 0 {
        if total == tail.len() as u64 {
            (0_u64, tail.as_slice())
        } else {
            (0_u64, &[][..])
        }
    } else {
        (0_u64, captured.as_slice())
    };
    let text = String::from_utf8_lossy(source);
    let transformed = !valid_utf8 || matches!(text, std::borrow::Cow::Owned(_));
    let text = text.into_owned();
    let unrecoverable_tail = !eof || start.saturating_add(text.len() as u64) < total;

    CapturedStream {
        stream,
        text,
        start,
        total: eof.then_some(total),
        unrecoverable_tail,
        limited: total > STREAM_BYTES as u64,
        failed,
        transformed,
    }
}

fn missing_stream(stream: OutputStream) -> CapturedStream {
    CapturedStream {
        stream,
        text: String::new(),
        start: 0,
        total: None,
        unrecoverable_tail: true,
        limited: false,
        failed: true,
        transformed: false,
    }
}

async fn wait_child(child: &mut Child) -> std::io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

pub(super) async fn capture_child(
    mut child: Child,
    termination: CaptureTermination,
    registration: Option<RecoveryRegistration>,
) -> CapturedCommand {
    let capture_slot = capture_slot();
    let spool_allowed = capture_slot.is_some();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        match stdout {
            Some(stdout) => drain_stream(stdout, OutputStream::Stdout, spool_allowed).await,
            None => missing_stream(OutputStream::Stdout),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(stderr) => drain_stream(stderr, OutputStream::Stderr, spool_allowed).await,
            None => missing_stream(OutputStream::Stderr),
        }
    });
    // Polling `try_wait` keeps reaping deterministic when separate
    // current-thread Tokio runtimes execute command tests concurrently.
    let waited = wait_child(&mut child).await;
    let stdout = stdout_task
        .await
        .unwrap_or_else(|_| missing_stream(OutputStream::Stdout));
    let stderr = stderr_task
        .await
        .unwrap_or_else(|_| missing_stream(OutputStream::Stderr));
    drop(capture_slot);
    let capture = CapturedCommand {
        stdout,
        stderr,
        status: waited.as_ref().ok().copied(),
        wait_error: waited.err().map(|error| error.to_string()),
        termination,
    };
    if capture.termination.status() == TERMINATION_CANCELLED {
        if let Some(registration) = registration {
            registration.register_cancelled(&capture);
        }
    }
    capture
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    #[test]
    fn utf8_validator_keeps_characters_split_across_reads() {
        let bytes = "前缀🙂末尾".as_bytes();
        let mut validator = Utf8Validator::default();
        for byte in bytes {
            validator.push(std::slice::from_ref(byte));
        }
        assert!(validator.finish());

        let mut validator = Utf8Validator::default();
        validator.push(b"valid");
        validator.push(&[0xff]);
        assert!(!validator.finish());
    }

    #[tokio::test]
    async fn spool_failure_keeps_draining_with_bounded_tail() {
        let bytes = vec![b'x'; TAIL_BUFFER_BYTES * 4];
        let expected = bytes.len() as u64;
        let (mut writer, reader) = tokio::io::duplex(4096);
        let write = tokio::spawn(async move {
            writer.write_all(&bytes).await.expect("write");
            writer.shutdown().await.expect("shutdown");
        });
        let captured = drain_stream(reader, OutputStream::Stdout, false).await;
        write.await.expect("writer");

        assert_eq!(captured.total, Some(expected));
        assert!(captured.failed);
        assert!(captured.text.is_empty());
        assert_eq!(captured.start, 0);
        assert!(captured.unrecoverable_tail);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_large_stdout_and_short_stderr_without_losing_exit_status() {
        let bytes = STREAM_BYTES + TAIL_BUFFER_BYTES * 2;
        let tail_marker = "stdout-tail-marker\n";
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                &format!(
                    "yes A | head -c {bytes}; printf '{tail_marker}'; \
                     printf 'unique-stderr-marker\\n' >&2; exit 7"
                ),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("spawn");
        let capture = capture_child(child, CaptureTermination::default(), None).await;

        assert_eq!(capture.exit_code(), Some(7));
        assert!(!capture.success());
        assert_eq!(capture.stdout.text.len(), STREAM_BYTES);
        assert_eq!(
            capture.stdout.total,
            Some((bytes + tail_marker.len()) as u64)
        );
        assert!(capture.stdout.limited);
        assert!(capture.stderr.text.contains("unique-stderr-marker"));

        let document = capture.document();
        assert_eq!(document.capture, CaptureState::Partial);
        assert_eq!(
            document.execution,
            ExecutionStatus::Exited {
                code: Some(7),
                signal: None,
            }
        );
        assert!(
            document
                .loss_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("capture_limit"))
        );
        assert!(
            document
                .parts
                .iter()
                .any(|part| part.stream == OutputStream::Display
                    && part.text.contains("not recoverable"))
        );
        assert!(
            !document
                .parts
                .iter()
                .any(|part| part.text.contains(tail_marker.trim()))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_utf8_is_lossy_and_never_claims_raw_coordinates() {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "printf '\\377broken\\n'"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("spawn");
        let capture = capture_child(child, CaptureTermination::default(), None).await;
        let document = capture.document();

        assert!(document.transformed);
        let owner = crate::model_read_receipts::ReadReceiptOwner::new(
            "workspace",
            "task",
            "session",
            "branch",
        )
        .expect("owner");
        let state = OutputState::new(
            crate::output_recovery::OutputPolicy { enabled: true },
            owner,
        );
        let rendered = state
            .register(
                uuid::Uuid::new_v4().to_string(),
                "call",
                &serde_json::json!({}),
                document,
                true,
                PAGE_BYTES,
            )
            .expect("register");
        assert!(rendered.view.transformed);
        assert!(
            rendered
                .view
                .source_totals
                .iter()
                .all(|(_, total)| total.is_none())
        );
        assert!(
            rendered
                .view
                .captured_ranges
                .iter()
                .all(|range| range.start == 0)
        );
    }
}
