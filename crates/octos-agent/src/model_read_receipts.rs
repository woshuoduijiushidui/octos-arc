//! Branch-local proof that a model received an exact `read_file` result.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use octos_core::{Message, MessageRole};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::file_state_cache::FileVersion;

const MAX_STAGED_CANDIDATES: usize = 256;
const MAX_ACTIVE_RECEIPTS: usize = 256;

/// Stable identity of one model-visible history branch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReadReceiptOwner {
    workspace_id: String,
    task_id: String,
    logical_session_id: String,
    model_branch_id: String,
}

impl ReadReceiptOwner {
    /// Build an owner only when every isolation dimension is present.
    pub fn new(
        workspace_id: impl Into<String>,
        task_id: impl Into<String>,
        logical_session_id: impl Into<String>,
        model_branch_id: impl Into<String>,
    ) -> Option<Self> {
        let owner = Self {
            workspace_id: workspace_id.into(),
            task_id: task_id.into(),
            logical_session_id: logical_session_id.into(),
            model_branch_id: model_branch_id.into(),
        };
        if [
            owner.workspace_id.as_str(),
            owner.task_id.as_str(),
            owner.logical_session_id.as_str(),
            owner.model_branch_id.as_str(),
        ]
        .into_iter()
        .any(|part| part.trim().is_empty())
        {
            return None;
        }
        Some(owner)
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn logical_session_id(&self) -> &str {
        &self.logical_session_id
    }

    pub fn model_branch_id(&self) -> &str {
        &self.model_branch_id
    }
}

/// Destructive lifecycle transition that revoked branch-local receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptClearReason {
    ContextTrim,
    ToolResultReplacement,
    Compaction,
    PromptReplacement,
    ProjectionPolicyChanged,
    Rewind,
    Rollback,
    Resume,
    Teleport,
    Fork,
    TaskSwitch,
    ColdRestore,
    LockPoisoned,
}

/// Targeted reason for revoking receipts without clearing the whole branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptRevokeReason {
    Mutation,
}

impl ReceiptRevokeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mutation => "mutation",
        }
    }
}

impl ReceiptClearReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextTrim => "context_trim",
            Self::ToolResultReplacement => "tool_result_replacement",
            Self::Compaction => "compaction",
            Self::PromptReplacement => "prompt_replacement",
            Self::ProjectionPolicyChanged => "projection_policy_changed",
            Self::Rewind => "rewind",
            Self::Rollback => "rollback",
            Self::Resume => "resume",
            Self::Teleport => "teleport",
            Self::Fork => "fork",
            Self::TaskSwitch => "task_switch",
            Self::ColdRestore => "cold_restore",
            Self::LockPoisoned => "lock_poisoned",
        }
    }
}

/// Last observed receipt clear, exposed for lifecycle verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptClearEvent {
    pub reason: ReceiptClearReason,
    pub staged_removed: usize,
    pub active_removed: usize,
}

/// File range represented by a successful `read_file` result.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FileView {
    Full,
    Lines { start: u64, end: u64 },
    Bytes { start: u64, end: u64 },
}

impl FileView {
    fn covers(&self, requested: &Self) -> bool {
        match (self, requested) {
            (Self::Full, Self::Full | Self::Lines { .. }) => true,
            (
                Self::Lines {
                    start: visible_start,
                    end: visible_end,
                },
                Self::Lines {
                    start: requested_start,
                    end: requested_end,
                },
            )
            | (
                Self::Bytes {
                    start: visible_start,
                    end: visible_end,
                },
                Self::Bytes {
                    start: requested_start,
                    end: requested_end,
                },
            ) => visible_start <= requested_start && visible_end >= requested_end,
            _ => false,
        }
    }
}

impl fmt::Display for FileView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("full"),
            Self::Lines { start, end } => write!(formatter, "lines:{start}-{end}"),
            Self::Bytes { start, end } => write!(formatter, "bytes:{start}-{end}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ReadSourceSignature {
    tool_call_id: String,
    arguments_sha256: String,
    output_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceProof {
    signature: ReadSourceSignature,
    occurrence: u64,
}

#[derive(Clone, Debug)]
struct ReadCandidate {
    candidate_id: String,
    owner: ReadReceiptOwner,
    signature: ReadSourceSignature,
    file_version: FileVersion,
    requested_view: FileView,
    returned_view: FileView,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelReadReceipt {
    candidate_id: String,
    owner: ReadReceiptOwner,
    file_version: FileVersion,
    model_visible_view: FileView,
    projection_policy_id: String,
    source_proof: SourceProof,
}

impl ModelReadReceipt {
    pub(crate) fn candidate_id(&self) -> &str {
        &self.candidate_id
    }

    pub(crate) fn model_visible_view(&self) -> &FileView {
        &self.model_visible_view
    }

    pub(crate) fn source_occurrence(&self) -> u64 {
        self.source_proof.occurrence
    }
}

#[derive(Debug, Default)]
struct Inner {
    generation: u64,
    next_candidate_id: u64,
    staged: Vec<ReadCandidate>,
    active: Vec<ModelReadReceipt>,
    projection_policy_id: Option<String>,
    clear_count: u64,
    last_clear: Option<ReceiptClearEvent>,
}

/// Pending receipts derived from one exact provider request.
///
/// Dropping this value rejects the candidates. Only a successful provider
/// response may pass it to [`ModelReadReceiptStore::activate`].
#[derive(Debug, Default)]
pub(crate) struct PendingReadReceipts {
    owner: Option<ReadReceiptOwner>,
    generation: u64,
    receipts: Vec<ModelReadReceipt>,
}

/// Branch-local read candidates and active model-visible receipts.
#[derive(Debug)]
pub struct ModelReadReceiptStore {
    owner: Option<ReadReceiptOwner>,
    inner: Mutex<Inner>,
}

impl Default for ModelReadReceiptStore {
    fn default() -> Self {
        Self {
            owner: None,
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl ModelReadReceiptStore {
    /// Build a disabled store. Deduplication requires [`Self::for_owner`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an enabled store isolated to one complete model branch owner.
    pub fn for_owner(owner: ReadReceiptOwner) -> Self {
        Self {
            owner: Some(owner),
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.owner.is_some()
    }

    pub fn owner(&self) -> Option<&ReadReceiptOwner> {
        self.owner.as_ref()
    }

    pub fn active_len(&self) -> usize {
        self.lock_fail_closed().active.len()
    }

    pub fn staged_len(&self) -> usize {
        self.lock_fail_closed().staged.len()
    }

    pub fn clear_count(&self) -> u64 {
        self.lock_fail_closed().clear_count
    }

    pub fn last_clear(&self) -> Option<ReceiptClearEvent> {
        self.lock_fail_closed().last_clear.clone()
    }

    /// Revoke all candidates and receipts after a destructive history change.
    pub fn clear(&self, reason: ReceiptClearReason) {
        let mut inner = self.lock_fail_closed();
        Self::clear_inner(&mut inner, reason);
    }

    /// Revoke staged and active receipts for one canonical provider target.
    ///
    /// The generation bump also prevents a provider request prepared before
    /// the mutation from activating an out-of-date pending receipt afterward.
    pub(crate) fn revoke_target(
        &self,
        target: &crate::file_state_cache::FileTarget,
        reason: ReceiptRevokeReason,
    ) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        if target.workspace_id() != owner.workspace_id() {
            return;
        }
        let mut inner = self.lock_fail_closed();
        let staged_before = inner.staged.len();
        let active_before = inner.active.len();
        inner
            .staged
            .retain(|candidate| candidate.file_version.target() != target);
        inner
            .active
            .retain(|receipt| receipt.file_version.target() != target);
        inner.generation = inner.generation.saturating_add(1);
        let removed = staged_before.saturating_sub(inner.staged.len())
            + active_before.saturating_sub(inner.active.len());
        metrics::counter!(
            "octos_model_read_receipt_target_revocations_total",
            "reason" => reason.as_str().to_string(),
        )
        .increment(1);
        metrics::counter!(
            "octos_model_read_receipt_entries_revoked_total",
            "reason" => reason.as_str().to_string(),
        )
        .increment(removed as u64);
    }

    pub(crate) fn stage(
        &self,
        tool_call_id: &str,
        arguments: &Value,
        file_version: FileVersion,
        requested_view: FileView,
        returned_view: FileView,
        output: &str,
    ) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        if tool_call_id.is_empty() || file_version.target().workspace_id() != owner.workspace_id() {
            return;
        }
        let mut inner = self.lock_fail_closed();
        inner.next_candidate_id = inner.next_candidate_id.saturating_add(1);
        let candidate_id = format!("read-candidate-{}", inner.next_candidate_id);
        if inner.staged.len() >= MAX_STAGED_CANDIDATES {
            inner.staged.remove(0);
            metrics::counter!(
                "octos_model_read_receipt_evictions_total",
                "kind" => "candidate".to_string(),
            )
            .increment(1);
        }
        inner.staged.push(ReadCandidate {
            candidate_id,
            owner: owner.clone(),
            signature: ReadSourceSignature {
                tool_call_id: crate::normalize_tool_call_id(tool_call_id),
                arguments_sha256: hash_json(arguments),
                output_sha256: hash_bytes(output.as_bytes()),
            },
            file_version,
            requested_view,
            returned_view,
        });
    }

    pub(crate) fn receipt_for(
        &self,
        file_version: &FileVersion,
        requested_view: &FileView,
    ) -> Option<ModelReadReceipt> {
        let owner = self.owner.as_ref()?;
        if file_version.target().workspace_id() != owner.workspace_id() {
            return None;
        }
        let mut inner = self.lock_fail_closed();
        inner.active.retain(|receipt| {
            receipt.file_version.target() != file_version.target()
                || receipt.file_version == *file_version
        });
        inner
            .active
            .iter()
            .rev()
            .find(|receipt| {
                receipt.owner == *owner
                    && receipt.file_version == *file_version
                    && receipt.model_visible_view.covers(requested_view)
            })
            .cloned()
    }

    pub(crate) fn prepare_dispatch(
        &self,
        messages: &[Message],
        projection_policy_id: &str,
    ) -> PendingReadReceipts {
        let Some(owner) = self.owner.as_ref() else {
            return PendingReadReceipts::default();
        };
        let sources = visible_read_sources(messages);
        let visible_proofs = sources
            .iter()
            .map(|(_, proof)| proof.clone())
            .collect::<HashSet<_>>();
        let mut sources_by_signature: HashMap<ReadSourceSignature, Vec<SourceProof>> =
            HashMap::new();
        for (signature, proof) in sources {
            sources_by_signature
                .entry(signature)
                .or_default()
                .push(proof);
        }

        let mut inner = self.lock_fail_closed();
        if inner
            .projection_policy_id
            .as_deref()
            .is_some_and(|current| current != projection_policy_id)
        {
            Self::clear_inner(&mut inner, ReceiptClearReason::ProjectionPolicyChanged);
        }
        inner.projection_policy_id = Some(projection_policy_id.to_owned());
        inner.active.retain(|receipt| {
            receipt.projection_policy_id == projection_policy_id
                && visible_proofs.contains(&receipt.source_proof)
        });
        let generation = inner.generation;
        let staged = std::mem::take(&mut inner.staged);
        drop(inner);

        let mut pending = Vec::new();
        for candidate in staged.into_iter().rev() {
            if !candidate.requested_view.covers(&candidate.returned_view) {
                continue;
            }
            let Some(proof) = sources_by_signature
                .get_mut(&candidate.signature)
                .and_then(Vec::pop)
            else {
                continue;
            };
            pending.push(ModelReadReceipt {
                candidate_id: candidate.candidate_id,
                owner: candidate.owner,
                file_version: candidate.file_version,
                model_visible_view: candidate.returned_view,
                projection_policy_id: projection_policy_id.to_owned(),
                source_proof: proof,
            });
        }
        pending.reverse();
        PendingReadReceipts {
            owner: Some(owner.clone()),
            generation,
            receipts: pending,
        }
    }

    pub(crate) fn activate(&self, pending: PendingReadReceipts) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        let mut inner = self.lock_fail_closed();
        if pending.owner.as_ref() != Some(owner) || pending.generation != inner.generation {
            return;
        }
        for receipt in pending.receipts {
            if receipt.owner != *owner {
                continue;
            }
            inner.active.retain(|existing| {
                if existing.file_version.target() == receipt.file_version.target()
                    && existing.file_version != receipt.file_version
                {
                    return false;
                }
                existing.file_version != receipt.file_version
                    || existing.model_visible_view != receipt.model_visible_view
            });
            if inner.active.len() >= MAX_ACTIVE_RECEIPTS {
                inner.active.remove(0);
                metrics::counter!(
                    "octos_model_read_receipt_evictions_total",
                    "kind" => "receipt".to_string(),
                )
                .increment(1);
            }
            inner.active.push(receipt);
        }
    }

    fn clear_inner(inner: &mut Inner, reason: ReceiptClearReason) {
        let event = ReceiptClearEvent {
            reason,
            staged_removed: inner.staged.len(),
            active_removed: inner.active.len(),
        };
        inner.generation = inner.generation.saturating_add(1);
        inner.staged.clear();
        inner.active.clear();
        inner.clear_count = inner.clear_count.saturating_add(1);
        inner.last_clear = Some(event.clone());
        metrics::counter!(
            "octos_model_read_receipt_lifecycle_clears_total",
            "reason" => reason.as_str().to_string(),
        )
        .increment(1);
        metrics::counter!(
            "octos_model_read_receipt_entries_cleared_total",
            "reason" => reason.as_str().to_string(),
        )
        .increment((event.staged_removed + event.active_removed) as u64);
    }

    fn lock_fail_closed(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => {
                let mut inner = poisoned.into_inner();
                Self::clear_inner(&mut inner, ReceiptClearReason::LockPoisoned);
                inner
            }
        }
    }
}

fn visible_read_sources(messages: &[Message]) -> Vec<(ReadSourceSignature, SourceProof)> {
    #[derive(Debug)]
    struct PendingCall {
        tool_call_id: String,
        tool_name: String,
        arguments_sha256: String,
    }

    let mut open_calls = VecDeque::new();
    let mut occurrence_by_signature: HashMap<ReadSourceSignature, u64> = HashMap::new();
    let mut sources = Vec::new();

    for message in messages {
        match message.role {
            MessageRole::Assistant => {
                open_calls.clear();
                for call in message.tool_calls.iter().flatten() {
                    open_calls.push_back(PendingCall {
                        tool_call_id: crate::normalize_tool_call_id(&call.id),
                        tool_name: call.name.clone(),
                        arguments_sha256: hash_json(&call.arguments),
                    });
                }
            }
            MessageRole::Tool => {
                let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                    continue;
                };
                let tool_call_id = crate::normalize_tool_call_id(tool_call_id);
                let Some(index) = open_calls
                    .iter()
                    .position(|call| call.tool_call_id == tool_call_id)
                else {
                    continue;
                };
                let Some(call) = open_calls.remove(index) else {
                    continue;
                };
                if call.tool_name != "read_file" {
                    continue;
                }
                let signature = ReadSourceSignature {
                    tool_call_id,
                    arguments_sha256: call.arguments_sha256,
                    output_sha256: hash_bytes(message.content.as_bytes()),
                };
                let occurrence = occurrence_by_signature
                    .entry(signature.clone())
                    .and_modify(|value| *value = value.saturating_add(1))
                    .or_insert(1);
                let proof = SourceProof {
                    signature: signature.clone(),
                    occurrence: *occurrence,
                };
                sources.push((signature, proof));
            }
            MessageRole::System | MessageRole::User => {
                open_calls.clear();
            }
        }
    }

    sources
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn hash_json(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_json(value, &mut canonical);
    hash_bytes(canonical.as_bytes())
}

fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => {
            output
                .push_str(&serde_json::to_string(value).expect("serializing a string cannot fail"));
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).expect("serializing an object key cannot fail"),
                );
                output.push(':');
                write_canonical_json(&values[key], output);
            }
            output.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_state_cache::{FileMetadataHint, FileTarget};
    use octos_core::ToolCall;

    fn store() -> ModelReadReceiptStore {
        ModelReadReceiptStore::for_owner(
            ReadReceiptOwner::new("workspace", "task", "session", "branch").unwrap(),
        )
    }

    fn version(bytes: &[u8]) -> FileVersion {
        FileVersion::from_bytes(
            FileTarget::new("workspace", "/workspace/file.txt"),
            None,
            bytes,
            FileMetadataHint::new(bytes.len() as u64, None, None, None, None),
        )
    }

    fn assistant(call_id: &str, arguments: Value) -> Message {
        let mut message = Message::assistant("");
        message.tool_calls = Some(vec![ToolCall {
            id: call_id.to_owned(),
            name: "read_file".to_owned(),
            arguments,
            metadata: None,
        }]);
        message
    }

    fn tool(call_id: &str, output: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: output.to_owned(),
            media: Vec::new(),
            tool_calls: None,
            tool_call_id: Some(call_id.to_owned()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn arguments_hash_ignores_object_key_order() {
        let left = serde_json::json!({"path": "a", "start_line": 1});
        let right: Value =
            serde_json::from_str(r#"{"start_line":1,"path":"a"}"#).expect("valid json");
        assert_eq!(hash_json(&left), hash_json(&right));
    }

    #[test]
    fn exact_source_activates_only_after_success_is_committed() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let file_version = version(b"body");
        store.stage(
            "call_1",
            &args,
            file_version.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );

        let pending = store.prepare_dispatch(
            &[assistant("call_1", args), tool("call_1", "body")],
            "policy-v1",
        );
        assert_eq!(store.staged_len(), 0);
        assert_eq!(store.active_len(), 0);

        store.activate(pending);
        let receipt = store
            .receipt_for(&file_version, &FileView::Full)
            .expect("matching source should activate a receipt");
        assert_eq!(&receipt.owner, store.owner().unwrap());
        assert!(
            store
                .receipt_for(&file_version, &FileView::Bytes { start: 0, end: 4 })
                .is_none(),
            "a line-numbered full view must not authorize raw byte mode"
        );
    }

    #[test]
    fn mutation_revokes_only_the_target_and_invalidates_prepared_receipts() {
        let store = store();
        let first_args = serde_json::json!({"path": "file.txt"});
        let second_args = serde_json::json!({"path": "other.txt"});
        let first = version(b"body");
        let second = FileVersion::from_bytes(
            FileTarget::new("workspace", "/workspace/other.txt"),
            None,
            b"other",
            FileMetadataHint::new(5, None, None, None, None),
        );
        store.stage(
            "call_1",
            &first_args,
            first.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );
        store.stage(
            "call_2",
            &second_args,
            second.clone(),
            FileView::Full,
            FileView::Full,
            "other",
        );
        let pending = store.prepare_dispatch(
            &[
                assistant("call_1", first_args.clone()),
                tool("call_1", "body"),
                assistant("call_2", second_args.clone()),
                tool("call_2", "other"),
            ],
            "policy-v1",
        );
        store.activate(pending);
        assert_eq!(store.active_len(), 2);

        store.stage(
            "call_3",
            &first_args,
            first.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );
        let prepared_before_mutation = store.prepare_dispatch(
            &[
                assistant("call_3", first_args),
                tool("call_3", "body"),
                assistant("call_2", second_args),
                tool("call_2", "other"),
            ],
            "policy-v1",
        );
        store.revoke_target(first.target(), ReceiptRevokeReason::Mutation);
        store.activate(prepared_before_mutation);

        assert!(store.receipt_for(&first, &FileView::Full).is_none());
        assert!(store.receipt_for(&second, &FileView::Full).is_some());
        assert_eq!(store.active_len(), 1);
    }

    #[test]
    fn changed_or_missing_output_consumes_candidate_without_receipt() {
        for messages in [
            vec![
                assistant("call_1", serde_json::json!({"path": "file.txt"})),
                tool("call_1", "projected body"),
            ],
            vec![assistant("call_1", serde_json::json!({"path": "file.txt"}))],
        ] {
            let store = store();
            store.stage(
                "call_1",
                &serde_json::json!({"path": "file.txt"}),
                version(b"body"),
                FileView::Full,
                FileView::Full,
                "body",
            );
            let pending = store.prepare_dispatch(&messages, "policy-v1");
            store.activate(pending);
            assert_eq!(store.staged_len(), 0);
            assert_eq!(store.active_len(), 0);
        }
    }

    #[test]
    fn partial_receipt_covers_only_matching_coordinate_space_and_subranges() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt", "start_line": 2, "end_line": 5});
        let file_version = version(b"one\ntwo\nthree\nfour\nfive\n");
        store.stage(
            "call_1",
            &args,
            file_version.clone(),
            FileView::Lines { start: 2, end: 5 },
            FileView::Lines { start: 2, end: 5 },
            "body",
        );
        let pending = store.prepare_dispatch(
            &[assistant("call_1", args), tool("call_1", "body")],
            "policy-v1",
        );
        store.activate(pending);

        assert!(
            store
                .receipt_for(&file_version, &FileView::Lines { start: 3, end: 4 })
                .is_some()
        );
        assert!(
            store
                .receipt_for(&file_version, &FileView::Lines { start: 1, end: 5 })
                .is_none()
        );
        assert!(
            store
                .receipt_for(&file_version, &FileView::Bytes { start: 2, end: 5 })
                .is_none()
        );
        assert!(store.receipt_for(&file_version, &FileView::Full).is_none());
    }

    #[test]
    fn repeated_call_id_binds_new_candidate_to_latest_exact_occurrence() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let file_version = version(b"body");
        store.stage(
            "call_1",
            &args,
            file_version,
            FileView::Full,
            FileView::Full,
            "body",
        );
        let both_occurrences = [
            assistant("call_1", args.clone()),
            tool("call_1", "body"),
            assistant("call_1", args.clone()),
            tool("call_1", "body"),
        ];
        let pending = store.prepare_dispatch(&both_occurrences, "policy-v1");

        assert_eq!(pending.receipts.len(), 1);
        assert_eq!(pending.receipts[0].source_occurrence(), 2);
        store.activate(pending);
        assert_eq!(store.active_len(), 1);

        let pending = store.prepare_dispatch(
            &[assistant("call_1", args), tool("call_1", "body")],
            "policy-v1",
        );
        store.activate(pending);
        assert_eq!(
            store.active_len(),
            0,
            "a different repeated-id occurrence must not preserve the proof"
        );
    }

    #[test]
    fn later_dispatch_without_source_revokes_active_receipt() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let file_version = version(b"body");
        store.stage(
            "call_1",
            &args,
            file_version.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );
        let pending = store.prepare_dispatch(
            &[assistant("call_1", args), tool("call_1", "body")],
            "policy-v1",
        );
        store.activate(pending);
        assert_eq!(store.active_len(), 1);

        let pending = store.prepare_dispatch(&[Message::user("new prompt")], "policy-v1");
        store.activate(pending);
        assert_eq!(store.active_len(), 0);
        assert!(store.receipt_for(&file_version, &FileView::Full).is_none());
    }

    #[test]
    fn changed_projection_policy_revokes_active_receipt() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let file_version = version(b"body");
        let source = [assistant("call_1", args.clone()), tool("call_1", "body")];
        store.stage(
            "call_1",
            &args,
            file_version,
            FileView::Full,
            FileView::Full,
            "body",
        );
        let pending = store.prepare_dispatch(&source, "policy-v1");
        store.activate(pending);
        assert_eq!(store.active_len(), 1);

        let pending = store.prepare_dispatch(&source, "policy-v2");
        store.activate(pending);
        assert_eq!(store.active_len(), 0);
        assert_eq!(
            store.last_clear().map(|event| event.reason),
            Some(ReceiptClearReason::ProjectionPolicyChanged)
        );
    }

    #[test]
    fn changed_file_version_cannot_use_an_active_receipt() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let old_version = version(b"body");
        let current_version = version(b"B0dy");
        let source = [assistant("call_1", args.clone()), tool("call_1", "body")];
        store.stage(
            "call_1",
            &args,
            old_version,
            FileView::Full,
            FileView::Full,
            "body",
        );
        let pending = store.prepare_dispatch(&source, "policy-v1");
        store.activate(pending);

        assert!(
            store
                .receipt_for(&current_version, &FileView::Full)
                .is_none()
        );
        assert_eq!(store.active_len(), 0);
    }

    #[test]
    fn clear_consumes_pending_receipts_and_records_reason() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let file_version = version(b"body");
        store.stage(
            "call_1",
            &args,
            file_version.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );
        let pending = store.prepare_dispatch(
            &[assistant("call_1", args), tool("call_1", "body")],
            "policy-v1",
        );

        store.clear(ReceiptClearReason::Compaction);
        store.activate(pending);

        assert_eq!(store.active_len(), 0);
        assert_eq!(store.staged_len(), 0);
        assert_eq!(store.clear_count(), 1);
        assert_eq!(
            store.last_clear(),
            Some(ReceiptClearEvent {
                reason: ReceiptClearReason::Compaction,
                staged_removed: 0,
                active_removed: 0,
            })
        );
    }

    #[test]
    fn owner_workspace_mismatch_never_stages_or_hits() {
        let store = ModelReadReceiptStore::for_owner(
            ReadReceiptOwner::new("workspace-a", "task", "session", "branch").unwrap(),
        );
        let file_version = version(b"body");

        store.stage(
            "call_1",
            &serde_json::json!({"path": "file.txt"}),
            file_version.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );

        assert_eq!(store.staged_len(), 0);
        assert!(store.receipt_for(&file_version, &FileView::Full).is_none());
    }

    #[test]
    fn bounded_receipt_eviction_is_a_safe_miss() {
        let store = store();
        let first_version = FileVersion::from_bytes(
            FileTarget::new("workspace", "/workspace/file-0.txt"),
            None,
            b"body-0",
            FileMetadataHint::new(6, None, None, None, None),
        );
        let mut messages = Vec::new();

        for index in 0..=MAX_ACTIVE_RECEIPTS {
            let path = format!("/workspace/file-{index}.txt");
            let body = format!("body-{index}");
            let args = serde_json::json!({"path": path});
            let file_version = FileVersion::from_bytes(
                FileTarget::new("workspace", path),
                None,
                body.as_bytes(),
                FileMetadataHint::new(body.len() as u64, None, None, None, None),
            );
            let call_id = format!("call_{index}");
            store.stage(
                &call_id,
                &args,
                file_version,
                FileView::Full,
                FileView::Full,
                &body,
            );
            messages.push(assistant(&call_id, args));
            messages.push(tool(&call_id, &body));
            let pending = store.prepare_dispatch(&messages, "policy-v1");
            store.activate(pending);
        }

        assert_eq!(store.active_len(), MAX_ACTIVE_RECEIPTS);
        assert!(store.receipt_for(&first_version, &FileView::Full).is_none());
    }

    #[test]
    fn bounded_candidate_eviction_drops_the_oldest_unconfirmed_read() {
        let store = store();
        let mut messages = Vec::new();

        for index in 0..=MAX_STAGED_CANDIDATES {
            let path = format!("/workspace/file-{index}.txt");
            let body = format!("body-{index}");
            let args = serde_json::json!({"path": path});
            let file_version = FileVersion::from_bytes(
                FileTarget::new("workspace", path),
                None,
                body.as_bytes(),
                FileMetadataHint::new(body.len() as u64, None, None, None, None),
            );
            let call_id = format!("call_{index}");
            store.stage(
                &call_id,
                &args,
                file_version,
                FileView::Full,
                FileView::Full,
                &body,
            );
            messages.push(assistant(&call_id, args));
            messages.push(tool(&call_id, &body));
        }

        let pending = store.prepare_dispatch(&messages, "policy-v1");

        assert_eq!(pending.receipts.len(), MAX_STAGED_CANDIDATES);
        assert!(
            pending
                .receipts
                .iter()
                .all(|receipt| receipt.candidate_id() != "read-candidate-1")
        );
    }

    #[test]
    fn poisoned_store_clears_receipts_before_any_future_hit() {
        let store = store();
        let args = serde_json::json!({"path": "file.txt"});
        let file_version = version(b"body");
        store.stage(
            "call_1",
            &args,
            file_version.clone(),
            FileView::Full,
            FileView::Full,
            "body",
        );
        let pending = store.prepare_dispatch(
            &[assistant("call_1", args), tool("call_1", "body")],
            "policy-v1",
        );
        store.activate(pending);
        assert_eq!(store.active_len(), 1);

        let _ = std::panic::catch_unwind(|| {
            let _guard = store.inner.lock().unwrap();
            panic!("poison receipt store");
        });

        assert!(store.receipt_for(&file_version, &FileView::Full).is_none());
        assert_eq!(
            store.last_clear().map(|event| event.reason),
            Some(ReceiptClearReason::LockPoisoned)
        );
    }
}
