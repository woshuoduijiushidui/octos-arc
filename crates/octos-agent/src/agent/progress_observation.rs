use octos_core::ToolCall;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::output_recovery::{ExecutionStatus, OutputSource, OutputView};
use crate::tools::ToolResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OperationFamily {
    Read,
    Search,
    Mutate,
    Validate,
    Execute,
    Wait,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObservationStatus {
    Success,
    Failed,
    Blocked,
    TimedOut,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MutationOutcome {
    Modified,
    NoChange,
    NoMatch,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObservationConfidence {
    Typed,
    ExactTextFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObservationDiagnostic {
    ConflictingMutationFields,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProgressObservation {
    pub call_id: String,
    pub family: OperationFamily,
    pub target_key: String,
    pub target_label: String,
    pub status: ObservationStatus,
    pub outcome: Option<MutationOutcome>,
    pub state_digest: Option<String>,
    pub evidence_key: String,
    pub validation_key: Option<String>,
    pub wait_key: Option<String>,
    pub error_kind: Option<String>,
    pub confidence: ObservationConfidence,
    pub diagnostic: Option<ObservationDiagnostic>,
}

pub(super) struct ObservationFacts(ProgressObservation);

impl ObservationFacts {
    pub fn from_result(call: &ToolCall, result: &ToolResult) -> Self {
        let mut observation = ProgressObservation::base(call);
        observation.status = if result
            .output_document
            .as_ref()
            .is_some_and(|document| matches!(&document.execution, ExecutionStatus::TimedOut))
        {
            ObservationStatus::TimedOut
        } else if result.success {
            ObservationStatus::Success
        } else {
            ObservationStatus::Failed
        };

        let metadata = result.structured_metadata.as_ref();
        if observation.family == OperationFamily::Mutate {
            let outcome = metadata
                .and_then(|m| m.get("outcome"))
                .and_then(Value::as_str)
                .and_then(|value| match value {
                    "modified" => Some(MutationOutcome::Modified),
                    "no_change" => Some(MutationOutcome::NoChange),
                    "no_match" => Some(MutationOutcome::NoMatch),
                    "ambiguous" => Some(MutationOutcome::Ambiguous),
                    _ => None,
                })
                .or_else(|| {
                    metadata
                        .and_then(|m| m.get("error_code"))
                        .and_then(Value::as_str)
                        .and_then(|code| match code {
                            "edit_no_match" | "diff_context_no_match" => {
                                Some(MutationOutcome::NoMatch)
                            }
                            "edit_ambiguous" | "diff_context_ambiguous" => {
                                Some(MutationOutcome::Ambiguous)
                            }
                            _ => None,
                        })
                });
            let meta_modified = metadata
                .and_then(|m| m.get("file_modified"))
                .and_then(Value::as_bool);
            let path_modified = result.file_modified.is_some();
            let conflicts = meta_modified.is_some_and(|value| value != path_modified)
                || matches!(outcome, Some(MutationOutcome::Modified)) && !path_modified
                || matches!(outcome, Some(MutationOutcome::NoChange)) && path_modified
                || matches!(
                    outcome,
                    Some(MutationOutcome::NoMatch | MutationOutcome::Ambiguous)
                ) && path_modified;
            if conflicts {
                observation.diagnostic = Some(ObservationDiagnostic::ConflictingMutationFields);
            } else {
                observation.outcome = outcome.or_else(|| {
                    (result.success && path_modified).then_some(MutationOutcome::Modified)
                });
                if observation.outcome.is_some() {
                    observation.confidence = ObservationConfidence::Typed;
                }
                if matches!(
                    observation.outcome,
                    Some(MutationOutcome::Modified | MutationOutcome::NoChange)
                ) && metadata
                    .and_then(|m| m.get("final_state"))
                    .and_then(Value::as_str)
                    == Some("confirmed")
                {
                    observation.state_digest = metadata
                        .and_then(|m| m.pointer("/final_version/content_sha256"))
                        .and_then(Value::as_str)
                        .filter(|value| valid_sha256(value))
                        .map(str::to_owned);
                }
            }
        }

        if let Some(code) = metadata
            .and_then(|m| m.get("error_code"))
            .and_then(Value::as_str)
            .filter(|_| {
                observation.family == OperationFamily::Mutate && observation.diagnostic.is_none()
            })
        {
            observation.error_kind = Some(bounded(code, 128));
            let evidence = (
                code,
                metadata.and_then(|m| m.get("current_version")),
                metadata.and_then(|m| m.get("searched_context_digest")),
                metadata.and_then(|m| m.get("expected_line")),
                metadata.and_then(|m| m.get("occurrence_count")),
            );
            observation.evidence_key = digest(&serde_json::to_vec(&evidence).unwrap_or_default());
            observation.confidence = ObservationConfidence::Typed;
        }
        Self(observation)
    }

    pub fn from_error(call: &ToolCall, error_kind: &str) -> Self {
        let mut observation = ProgressObservation::base(call);
        observation.status = ObservationStatus::Failed;
        observation.error_kind = Some(bounded(error_kind, 128));
        Self(observation)
    }

    pub fn finish(self, visible: &str, view: Option<&OutputView>) -> ProgressObservation {
        let successful = self.0.status == ObservationStatus::Success;
        self.finish_with_source(
            visible,
            view.filter(|view| successful && view.success).map(|view| {
                (
                    &view.source,
                    view.visible_ranges.as_slice(),
                    view.transformed,
                )
            }),
        )
    }

    fn finish_with_source(
        mut self,
        visible: &str,
        source: Option<(&OutputSource, &[crate::output_recovery::OutputRange], bool)>,
    ) -> ProgressObservation {
        if self.0.evidence_key.is_empty() {
            if let Some((source, ranges, transformed)) = source
                && let OutputSource::File { sha256, .. } = source
                && valid_sha256(sha256)
                && !transformed
            {
                self.0.state_digest = Some(sha256.clone());
                self.0.evidence_key =
                    digest(&serde_json::to_vec(&(source, ranges)).unwrap_or_default());
                self.0.confidence = ObservationConfidence::Typed;
            } else {
                self.0.evidence_key = digest(visible.as_bytes());
            }
        }
        self.0
    }
}

impl ProgressObservation {
    pub fn downgrade_ambiguous_read(&mut self, visible: &str) {
        if self.family == OperationFamily::Read {
            self.state_digest = None;
            self.evidence_key = digest(visible.as_bytes());
            self.confidence = ObservationConfidence::ExactTextFallback;
        }
    }

    fn base(call: &ToolCall) -> Self {
        let path = call
            .arguments
            .get("path")
            .or_else(|| call.arguments.get("file_path"))
            .and_then(Value::as_str);
        let target_key = if let Some(path) = path {
            digest(&serde_json::to_vec(&(call.name.as_str(), path)).unwrap_or_default())
        } else {
            digest(&serde_json::to_vec(&(call.name.as_str(), &call.arguments)).unwrap_or_default())
        };
        let target_label = path
            .and_then(|p| p.rsplit(['/', '\\']).find(|part| !part.is_empty()))
            .map(|p| bounded(p, 96))
            .unwrap_or_else(|| bounded(&call.name, 96));
        Self {
            call_id: call.id.clone(),
            family: family(&call.name),
            target_key,
            target_label,
            status: ObservationStatus::Unknown,
            outcome: None,
            state_digest: None,
            evidence_key: String::new(),
            validation_key: None,
            wait_key: None,
            error_kind: None,
            confidence: ObservationConfidence::ExactTextFallback,
            diagnostic: None,
        }
    }

    pub fn placeholder(call: &ToolCall, status: ObservationStatus, visible: &str) -> Self {
        let mut observation = Self::base(call);
        observation.status = status;
        observation.evidence_key = digest(visible.as_bytes());
        observation
    }
}

fn family(name: &str) -> OperationFamily {
    match name {
        "read_file" | "recall" | "read_task_output" | "list_dir" => OperationFamily::Read,
        "grep" | "glob" | "web_search" | "search" => OperationFamily::Search,
        "write_file" | "edit_file" | "diff_edit" | "apply_patch" => OperationFamily::Mutate,
        "check" | "check_workspace_contract" => OperationFamily::Validate,
        "shell" | "bash" => OperationFamily::Execute,
        "peer_gather" | "peer_list" | "check_background_tasks" => OperationFamily::Wait,
        _ => OperationFamily::Other,
    }
}

fn valid_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn digest(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_recovery::{OutputRange, OutputStream};
    use serde_json::json;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: args,
            metadata: None,
        }
    }

    #[test]
    fn h07_m1_reads_typed_mutation_facts_before_rendering() {
        let call = call("edit_file", json!({"path": "/workspace/src/main.rs"}));
        let version = format!("sha256:{}", "a".repeat(64));
        let result = ToolResult {
            output: "changed".into(),
            success: true,
            file_modified: Some("/workspace/src/main.rs".into()),
            structured_metadata: Some(json!({
                "outcome": "modified", "file_modified": true,
                "final_state": "confirmed", "final_version": {"content_sha256": version}
            })),
            ..Default::default()
        };
        let observation = ObservationFacts::from_result(&call, &result).finish("redacted", None);
        assert_eq!(observation.family, OperationFamily::Mutate);
        assert_eq!(observation.outcome, Some(MutationOutcome::Modified));
        assert_eq!(observation.state_digest.as_deref(), Some(version.as_str()));
        assert_eq!(observation.target_label, "main.rs");
        assert!(!observation.target_key.contains("/workspace"));
        assert_eq!(observation.validation_key, None);
    }

    #[test]
    fn h07_m1_no_change_missing_and_conflicting_fields_stay_distinct() {
        let call = call("edit_file", json!({"path": "a.rs"}));
        let no_change = ToolResult {
            success: true,
            structured_metadata: Some(json!({"outcome": "no_change", "file_modified": false})),
            ..Default::default()
        };
        let known = ObservationFacts::from_result(&call, &no_change).finish("same", None);
        assert_eq!(known.outcome, Some(MutationOutcome::NoChange));
        assert_eq!(known.confidence, ObservationConfidence::Typed);
        let missing = ObservationFacts::from_result(
            &call,
            &ToolResult {
                success: true,
                ..Default::default()
            },
        )
        .finish("same", None);
        assert_eq!(missing.outcome, None);
        let conflicting = ToolResult {
            file_modified: Some("a.rs".into()),
            ..no_change
        };
        let observation = ObservationFacts::from_result(&call, &conflicting).finish("same", None);
        assert_eq!(observation.outcome, None);
        assert_eq!(observation.state_digest, None);
        assert_eq!(
            observation.diagnostic,
            Some(ObservationDiagnostic::ConflictingMutationFields)
        );
    }

    #[test]
    fn h07_m1_typed_error_code_is_not_parsed_from_output() {
        let call = call("diff_edit", json!({"path": "a.rs"}));
        let result = ToolResult {
            output: "Error: edit_ambiguous".into(),
            success: false,
            structured_metadata: Some(
                json!({"error_code": "diff_context_no_match", "file_modified": false,
                "current_version": {"content_sha256": format!("sha256:{}", "b".repeat(64))},
                "expected_line": 9}),
            ),
            ..Default::default()
        };
        let observation =
            ObservationFacts::from_result(&call, &result).finish("Error: edit_ambiguous", None);
        assert_eq!(observation.outcome, Some(MutationOutcome::NoMatch));
        assert_eq!(
            observation.error_kind.as_deref(),
            Some("diff_context_no_match")
        );
        assert_eq!(observation.confidence, ObservationConfidence::Typed);
        let plain = ObservationFacts::from_result(
            &call,
            &ToolResult {
                output: result.output,
                success: false,
                ..Default::default()
            },
        )
        .finish("Error: edit_ambiguous", None);
        assert_eq!(plain.outcome, None);
        assert_eq!(plain.error_kind, None);
    }

    #[test]
    fn h07_m1_read_uses_source_version_and_visible_range_when_trusted() {
        let call = call("read_file", json!({"path": "a.rs"}));
        let source = OutputSource::File {
            target: "/workspace/a.rs".into(),
            sha256: format!("sha256:{}", "c".repeat(64)),
        };
        let first = [OutputRange {
            stream: OutputStream::File,
            start: 0,
            end: 10,
            lines: Some((1, 2)),
        }];
        let next = [OutputRange {
            stream: OutputStream::File,
            start: 10,
            end: 20,
            lines: Some((3, 4)),
        }];
        let facts = ObservationFacts::from_result(
            &call,
            &ToolResult {
                success: true,
                ..Default::default()
            },
        );
        let observed = facts.finish_with_source("same", Some((&source, &first, false)));
        assert_eq!(observed.state_digest.as_deref(), Some(source_sha(&source)));
        assert_eq!(observed.confidence, ObservationConfidence::Typed);
        let changed_range = ObservationFacts::from_result(
            &call,
            &ToolResult {
                success: true,
                ..Default::default()
            },
        )
        .finish_with_source("same", Some((&source, &next, false)));
        assert_ne!(observed.evidence_key, changed_range.evidence_key);
        let fallback = ObservationFacts::from_result(
            &call,
            &ToolResult {
                success: true,
                ..Default::default()
            },
        )
        .finish_with_source("same", Some((&source, &first, true)));
        assert_eq!(
            fallback.confidence,
            ObservationConfidence::ExactTextFallback
        );
        assert_eq!(fallback.evidence_key, digest(b"same"));
    }

    fn source_sha(source: &OutputSource) -> &str {
        match source {
            OutputSource::File { sha256, .. } => sha256,
            _ => unreachable!(),
        }
    }

    #[test]
    fn h07_m1_labels_are_bounded_on_utf8_and_unknown_tools_are_other() {
        let path = format!("/workspace/{}", "界".repeat(100));
        let custom_call = call("custom_edit_like_tool", json!({"path": path}));
        let observation =
            ObservationFacts::from_error(&custom_call, &"错".repeat(100)).finish("visible", None);
        assert_eq!(observation.family, OperationFamily::Other);
        assert!(observation.target_label.len() <= 96);
        assert!(observation.error_kind.as_ref().unwrap().len() <= 128);
        assert_eq!(observation.status, ObservationStatus::Failed);
        assert_eq!(observation.validation_key, None);
        assert_eq!(observation.wait_key, None);
        let windows = call("read_file", json!({"path": "C:\\private\\source.rs"}));
        let windows = ObservationFacts::from_result(
            &windows,
            &ToolResult {
                success: true,
                ..Default::default()
            },
        )
        .finish("visible", None);
        assert_eq!(windows.target_label, "source.rs");
    }
}
