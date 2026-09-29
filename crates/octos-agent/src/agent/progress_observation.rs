use octos_core::ToolCall;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

use crate::harness_errors::HarnessError;
use crate::output_recovery::{ExecutionStatus, OutputSource, OutputView};
use crate::tools::{ToolRegistry, ToolResult};

pub(crate) const H07_TERMINAL_CODE: &str = "h07_terminal_non_retryable";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct H07Terminal {
    pub message: String,
}

impl H07Terminal {
    fn from_observation(observation: &ProgressObservation) -> Self {
        let family = match observation.family {
            OperationFamily::Read => "read",
            OperationFamily::Search => "search",
            OperationFamily::Mutate => "mutation",
            OperationFamily::Validate => "validation",
            OperationFamily::Execute => "execution",
            OperationFamily::Wait => "wait",
            OperationFamily::Other => "tool operation",
        };
        Self {
            message: bounded(
                &format!(
                    "Stopped after repeated {family} at {} returned unchanged evidence after a strategy change.",
                    observation.target_label
                ),
                MAX_HINT_BYTES,
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VerifiedWaitFact {
    task_handle: String,
    status: String,
}

/// Extract a wait fact only from declared async surfaces and only while the
/// runtime supervisor confirms that the referenced handle is live.
pub(super) fn verified_wait_fact(
    tools: &ToolRegistry,
    call: &ToolCall,
    visible: Option<&str>,
) -> Option<VerifiedWaitFact> {
    let task_handle = if call.name == "read_task_output" {
        call.arguments
            .get("task_handle")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else if tools.is_spawn_only(&call.name) {
        visible
            .and_then(|body| serde_json::from_str::<Value>(body).ok())
            .and_then(|value| {
                value
                    .get("task_handle")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
    } else {
        None
    }?;
    let task = tools.supervisor().get_task(&task_handle)?;
    task.status.is_active().then(|| VerifiedWaitFact {
        task_handle,
        status: task.status.as_str().to_owned(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationFamily {
    Read,
    Search,
    Mutate,
    Validate,
    Execute,
    Wait,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationStatus {
    Success,
    Failed,
    Blocked,
    TimedOut,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationOutcome {
    Modified,
    NoChange,
    NoMatch,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationConfidence {
    Typed,
    TrustedAdapter,
    ExactTextFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationDiagnostic {
    ConflictingMutationFields,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgressObservation {
    pub call_id: String,
    pub family: OperationFamily,
    pub target_key: String,
    pub target_label: String,
    pub status: ObservationStatus,
    pub outcome: Option<MutationOutcome>,
    pub state_digest: Option<String>,
    pub evidence_key: String,
    pub validation_key: Option<String>,
    pub validation_score: Option<ValidationScore>,
    pub wait_key: Option<String>,
    pub error_kind: Option<String>,
    pub confidence: ObservationConfidence,
    pub diagnostic: Option<ObservationDiagnostic>,
    pub semantic_eligible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ValidationScore {
    pub errors: u64,
    pub warnings: u64,
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
                    if result.success && path_modified {
                        Some(MutationOutcome::Modified)
                    } else if result.success && meta_modified == Some(false) {
                        Some(MutationOutcome::NoChange)
                    } else {
                        None
                    }
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
                    if let Some(state_digest) = observation.state_digest.as_deref() {
                        // The final confirmed version is authoritative. Do not
                        // use write_version or changed_range: a formatter may
                        // change both after the tool write, and a range alone
                        // does not prove a distinct final state.
                        let outcome = match observation.outcome {
                            Some(MutationOutcome::Modified) => "modified",
                            Some(MutationOutcome::NoChange) => "no_change",
                            _ => "unknown",
                        };
                        observation.evidence_key = digest(
                            &serde_json::to_vec(&(outcome, state_digest)).unwrap_or_default(),
                        );
                        observation.semantic_eligible = true;
                    }
                }
            }
        }

        if observation.family == OperationFamily::Validate
            && let Some(validation) = metadata.and_then(|value| value.get("validation"))
            && validation.get("schema").and_then(Value::as_str) == Some("octos.validation.v1")
            && validation.get("adapter").and_then(Value::as_str) == Some("check")
            && validation.get("ran").and_then(Value::as_bool) == Some(true)
            && let (Some(errors), Some(warnings)) = (
                validation.get("errors").and_then(Value::as_u64),
                validation.get("warnings").and_then(Value::as_u64),
            )
        {
            let score = ValidationScore { errors, warnings };
            let key = digest(
                &serde_json::to_vec(&(observation.target_key.as_str(), errors, warnings))
                    .unwrap_or_default(),
            );
            observation.validation_key = Some(key.clone());
            observation.validation_score = Some(score);
            observation.evidence_key = key;
            observation.confidence = ObservationConfidence::Typed;
            observation.semantic_eligible = true;
        }

        if call.name == "recall"
            && (call.arguments.get("query").is_some()
                || call.arguments.get("max_matches").is_some())
            && result.success
            && let Ok(search) = serde_json::from_str::<Value>(&result.output)
            && let (Some(output_id), Some(stream), Some(query)) = (
                search.get("output_id").and_then(Value::as_str),
                search.get("stream").and_then(Value::as_str),
                call.arguments.get("query").and_then(Value::as_str),
            )
        {
            // H03 recall-search adapter allowlist. Match coordinates and the
            // bounded search window are stable evidence; snippets, limits,
            // timing and recovery IDs are presentation/control fields.
            let matches: Vec<_> = search
                .get("matches")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|entry| (entry.get("start").cloned(), entry.get("end").cloned()))
                .collect();
            observation.family = OperationFamily::Search;
            observation.target_key = digest(
                &serde_json::to_vec(&(
                    "recall_search",
                    output_id,
                    stream,
                    digest(query.as_bytes()),
                ))
                .unwrap_or_default(),
            );
            observation.target_label = "saved output search".to_owned();
            observation.evidence_key = digest(
                &serde_json::to_vec(&(
                    search.get("searched_range"),
                    matches,
                    search.get("search_complete"),
                    search.get("artifact_complete"),
                    search.get("next_offset"),
                ))
                .unwrap_or_default(),
            );
            observation.confidence = ObservationConfidence::TrustedAdapter;
            observation.semantic_eligible = true;
        }

        if let Some(code) = metadata
            .and_then(|m| m.get("error_code"))
            .and_then(Value::as_str)
            .filter(|_| {
                observation.family == OperationFamily::Mutate && observation.diagnostic.is_none()
            })
        {
            observation.error_kind = Some(bounded(code, 128));
            // H05 producer allowlist: path is taken from the call, while the
            // error code, matcher, reason, location/count, candidates' line
            // locations, and current file version are stable failure facts.
            // Deliberately ignore attempted text/context digests, candidate
            // score/excerpt, limits, timing and request IDs. Those are not
            // evidence that the failure location or file changed.
            if matches!(
                code,
                "edit_no_match"
                    | "edit_ambiguous"
                    | "diff_context_no_match"
                    | "diff_context_ambiguous"
            ) {
                let mut candidates: Vec<_> = metadata
                    .and_then(|m| m.get("candidates"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(8)
                    .map(|candidate| {
                        (
                            candidate.get("line_range").cloned(),
                            candidate.get("matcher").cloned(),
                        )
                    })
                    .collect();
                // Ranking/score may reorder otherwise identical locations.
                candidates
                    .sort_by_key(|candidate| serde_json::to_string(candidate).unwrap_or_default());
                let m = metadata.expect("typed error has metadata");
                let evidence = (
                    code,
                    m.get("matcher"),
                    m.get("reason"),
                    m.get("hunk_index"),
                    m.get("expected_line"),
                    m.get("actual_line"),
                    m.get("expected"),
                    m.get("actual"),
                    m.get("occurrence_count"),
                    m.pointer("/current_version/content_sha256"),
                    candidates,
                );
                observation.evidence_key =
                    digest(&serde_json::to_vec(&evidence).unwrap_or_default());
                observation.semantic_eligible =
                    matches!(
                        observation.outcome,
                        Some(MutationOutcome::NoMatch | MutationOutcome::Ambiguous)
                    ) && m.get("matcher").and_then(Value::as_str).is_some()
                        && m.pointer("/current_version/content_sha256")
                            .and_then(Value::as_str)
                            .is_some_and(valid_sha256);
            }
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

    pub fn from_harness_error(
        call: &ToolCall,
        error: &HarnessError,
        stable_reason: Option<&str>,
    ) -> Self {
        let mut facts = Self::from_error(call, error.variant_name());
        if let Some(reason) = stable_reason {
            facts.0.evidence_key = digest(
                &serde_json::to_vec(&(
                    error.variant_name(),
                    error.recovery_hint().as_str(),
                    reason,
                ))
                .unwrap_or_default(),
            );
            facts.0.confidence = ObservationConfidence::Typed;
            facts.0.semantic_eligible = true;
        }
        facts
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
                && let OutputSource::File { target, sha256 } = source
                && valid_sha256(sha256)
                && !transformed
            {
                self.0.target_key = digest(
                    &serde_json::to_vec(&(OperationFamily::Read as u8, target)).unwrap_or_default(),
                );
                self.0.target_label = target
                    .rsplit(['/', '\\'])
                    .find(|part| !part.is_empty())
                    .map(|part| bounded(part, 96))
                    .unwrap_or_else(|| "file".to_owned());
                self.0.state_digest = Some(sha256.clone());
                self.0.evidence_key =
                    digest(&serde_json::to_vec(&(source, ranges)).unwrap_or_default());
                self.0.confidence = ObservationConfidence::TrustedAdapter;
                self.0.semantic_eligible = true;
            } else {
                self.0.evidence_key = digest(visible.as_bytes());
            }
        }
        self.0
    }
}

impl ProgressObservation {
    pub(super) fn verified_wait(call: &ToolCall, fact: &VerifiedWaitFact, visible: &str) -> Self {
        let wait_key = digest(
            &serde_json::to_vec(&(fact.task_handle.as_str(), fact.status.as_str()))
                .unwrap_or_default(),
        );
        Self {
            call_id: call.id.clone(),
            family: OperationFamily::Wait,
            target_key: digest(fact.task_handle.as_bytes()),
            target_label: bounded(&call.name, 96),
            status: ObservationStatus::Success,
            outcome: None,
            state_digest: None,
            evidence_key: digest(
                &serde_json::to_vec(&(wait_key.as_str(), digest(visible.as_bytes())))
                    .unwrap_or_default(),
            ),
            validation_key: None,
            validation_score: None,
            wait_key: Some(wait_key),
            error_kind: None,
            confidence: ObservationConfidence::TrustedAdapter,
            diagnostic: None,
            semantic_eligible: true,
        }
    }

    pub fn downgrade_ambiguous_read(&mut self, visible: &str) {
        if self.family == OperationFamily::Read {
            self.state_digest = None;
            self.evidence_key = digest(visible.as_bytes());
            self.confidence = ObservationConfidence::ExactTextFallback;
            self.semantic_eligible = false;
        }
    }

    fn base(call: &ToolCall) -> Self {
        let operation_family = if call.name == "recall"
            && (call.arguments.get("query").is_some()
                || call.arguments.get("max_matches").is_some())
        {
            OperationFamily::Search
        } else {
            family(&call.name)
        };
        let path = call
            .arguments
            .get("path")
            .or_else(|| call.arguments.get("file_path"))
            .and_then(Value::as_str);
        let target_key = if let Some(path) = path {
            digest(&serde_json::to_vec(&(operation_family as u8, path)).unwrap_or_default())
        } else {
            digest(&serde_json::to_vec(&(call.name.as_str(), &call.arguments)).unwrap_or_default())
        };
        let target_label = path
            .and_then(|p| p.rsplit(['/', '\\']).find(|part| !part.is_empty()))
            .map(|p| bounded(p, 96))
            .unwrap_or_else(|| bounded(&call.name, 96));
        Self {
            call_id: call.id.clone(),
            family: operation_family,
            target_key,
            target_label,
            status: ObservationStatus::Unknown,
            outcome: None,
            state_digest: None,
            evidence_key: String::new(),
            validation_key: None,
            validation_score: None,
            wait_key: None,
            error_kind: None,
            confidence: ObservationConfidence::ExactTextFallback,
            diagnostic: None,
            semantic_eligible: false,
        }
    }

    pub fn placeholder(call: &ToolCall, status: ObservationStatus, visible: &str) -> Self {
        let mut observation = Self::base(call);
        observation.status = status;
        observation.evidence_key = digest(visible.as_bytes());
        observation
    }

    pub fn has_authoritative_progress(&self) -> bool {
        self.confidence != ObservationConfidence::ExactTextFallback || self.diagnostic.is_some()
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

const MAX_EPISODES: usize = 32;
const MAX_SAMPLES: usize = 4;
const MAX_HINT_BYTES: usize = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgressClass {
    ValidationImproved,
    StateChanged,
    NoProgress,
    EvidenceChanged,
    VerifiedWait,
    Regressed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgressDecision {
    Continue,
    Hint,
    SwitchRequired,
    TerminalNonRetryable,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EpisodeOutcome {
    pub class: ProgressClass,
    pub decision: ProgressDecision,
    pub hint: Option<String>,
    pub request_reflection: Option<SemanticReflectionRequest>,
    pub episode_created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SemanticReflectionRequest {
    pub category: String,
    pub target: String,
    pub evidence: String,
}

impl SemanticReflectionRequest {
    fn from_observation(observation: &ProgressObservation) -> Self {
        Self {
            category: format!("{:?}/no_progress", observation.family).to_lowercase(),
            target: bounded(&observation.target_label, 96),
            evidence: bounded(
                &format!(
                    "status={:?}; outcome={:?}; error={}; evidence={}",
                    observation.status,
                    observation.outcome,
                    observation.error_kind.as_deref().unwrap_or("none"),
                    observation.evidence_key,
                ),
                MAX_HINT_BYTES,
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EpisodeKey {
    family: OperationFamily,
    target_key: String,
    status: ObservationStatus,
    outcome: Option<MutationOutcome>,
    error_kind: Option<String>,
    evidence_key: String,
    validation_score: Option<ValidationScore>,
}

impl EpisodeKey {
    fn from_observation(observation: &ProgressObservation) -> Self {
        Self {
            family: observation.family,
            target_key: observation.target_key.clone(),
            status: observation.status,
            outcome: observation.outcome,
            error_kind: observation.error_kind.clone(),
            evidence_key: observation.evidence_key.clone(),
            validation_score: observation.validation_score,
        }
    }

    fn same_scope(&self, other: &Self) -> bool {
        self.family == other.family
            && self.target_key == other.target_key
            && self.status == other.status
            && self.outcome == other.outcome
            && self.error_kind == other.error_kind
    }
}

#[derive(Debug)]
struct Episode {
    key: EpisodeKey,
    samples: VecDeque<String>,
    observations: u8,
    hinted: bool,
    switch_required: bool,
    reflection_requested: bool,
    post_switch_probe: bool,
    terminal: bool,
}

/// Local to one LoopDetector, with LRU eviction. The full 256-bit digests
/// participate in equality; labels and short prefixes never do.
#[derive(Default)]
pub(crate) struct EpisodeTracker {
    episodes: VecDeque<Episode>,
}

impl EpisodeTracker {
    pub fn observe(&mut self, observation: &ProgressObservation) -> EpisodeOutcome {
        let key = EpisodeKey::from_observation(observation);
        for episode in &mut self.episodes {
            if episode.switch_required && episode.key != key {
                episode.post_switch_probe = true;
            }
        }
        if !observation.semantic_eligible
            || observation.confidence == ObservationConfidence::ExactTextFallback
            || observation.evidence_key.is_empty()
        {
            return EpisodeOutcome {
                class: ProgressClass::Unknown,
                decision: ProgressDecision::Continue,
                hint: None,
                request_reflection: None,
                episode_created: false,
            };
        }
        if observation.family == OperationFamily::Wait && observation.wait_key.is_some() {
            if let Some(index) = self.episodes.iter().position(|episode| episode.key == key) {
                let episode = self.episodes.remove(index).expect("existing wait episode");
                self.episodes.push_back(episode);
                return EpisodeOutcome {
                    class: ProgressClass::VerifiedWait,
                    decision: ProgressDecision::Continue,
                    hint: None,
                    request_reflection: None,
                    episode_created: false,
                };
            }
            let known_scope = self
                .episodes
                .iter()
                .any(|episode| episode.key.target_key == key.target_key);
            if self.episodes.len() == MAX_EPISODES {
                self.episodes.pop_front();
            }
            self.episodes.push_back(Episode {
                key: key.clone(),
                samples: VecDeque::from([key.evidence_key]),
                observations: 1,
                hinted: false,
                switch_required: false,
                reflection_requested: false,
                post_switch_probe: false,
                terminal: false,
            });
            return EpisodeOutcome {
                class: if known_scope {
                    ProgressClass::EvidenceChanged
                } else {
                    ProgressClass::VerifiedWait
                },
                decision: ProgressDecision::Continue,
                hint: None,
                request_reflection: None,
                episode_created: true,
            };
        }
        if observation.family == OperationFamily::Validate
            && let Some(current) = observation.validation_score
            && let Some(previous) = self
                .episodes
                .iter()
                .rev()
                .find(|episode| {
                    episode.key.family == OperationFamily::Validate
                        && episode.key.target_key == key.target_key
                        && episode.key.validation_score.is_some()
                })
                .and_then(|episode| episode.key.validation_score)
            && current != previous
        {
            let class = if current < previous {
                ProgressClass::ValidationImproved
            } else {
                ProgressClass::Regressed
            };
            // A changed trusted validation score ends the prior validation
            // episode for this scope. The new score becomes a fresh bounded
            // baseline; it never clears unrelated mutation/read episodes.
            self.episodes.retain(|episode| {
                episode.key.family != OperationFamily::Validate
                    || episode.key.target_key != key.target_key
            });
            if self.episodes.len() == MAX_EPISODES {
                self.episodes.pop_front();
            }
            self.episodes.push_back(Episode {
                key: key.clone(),
                samples: VecDeque::from([key.evidence_key]),
                observations: 1,
                hinted: false,
                switch_required: false,
                reflection_requested: false,
                post_switch_probe: false,
                terminal: false,
            });
            return EpisodeOutcome {
                class,
                decision: ProgressDecision::Continue,
                hint: None,
                request_reflection: None,
                episode_created: true,
            };
        }
        if let Some(index) = self.episodes.iter().position(|episode| episode.key == key) {
            let mut episode = self.episodes.remove(index).expect("existing episode");
            episode.observations = episode.observations.saturating_add(1);
            if episode.samples.len() == MAX_SAMPLES {
                episode.samples.pop_front();
            }
            episode.samples.push_back(key.evidence_key.clone());
            let decision = if episode.terminal || episode.switch_required {
                episode.terminal = true;
                ProgressDecision::TerminalNonRetryable
            } else if episode.observations >= 3 {
                episode.switch_required = true;
                ProgressDecision::SwitchRequired
            } else if episode.observations == 2 && !episode.hinted {
                episode.hinted = true;
                ProgressDecision::Hint
            } else {
                ProgressDecision::Continue
            };
            let request_reflection =
                if decision == ProgressDecision::SwitchRequired && !episode.reflection_requested {
                    episode.reflection_requested = true;
                    Some(SemanticReflectionRequest::from_observation(observation))
                } else {
                    None
                };
            self.episodes.push_back(episode);
            let hint = match decision {
                ProgressDecision::Hint => Some(bounded(
                    &format!(
                        "\n\n[NO PROGRESS] The same {:?} failure at {} returned the same evidence. Inspect the current location or choose a different diagnostic action.",
                        observation.family, observation.target_label
                    ),
                    MAX_HINT_BYTES,
                )),
                ProgressDecision::SwitchRequired => Some(bounded(
                    &format!(
                        "\n\n[SWITCH REQUIRED] Repeated {:?} failure at {} has unchanged evidence despite changed attempts. Use a different diagnostic action before retrying.",
                        observation.family, observation.target_label
                    ),
                    MAX_HINT_BYTES,
                )),
                _ => None,
            };
            return EpisodeOutcome {
                class: ProgressClass::NoProgress,
                decision,
                hint,
                request_reflection,
                episode_created: false,
            };
        }
        let known_scope = self
            .episodes
            .iter()
            .any(|episode| episode.key.same_scope(&key));
        let class = match (observation.family, observation.outcome) {
            (OperationFamily::Mutate, Some(MutationOutcome::Modified)) => {
                ProgressClass::StateChanged
            }
            (OperationFamily::Mutate, Some(MutationOutcome::NoChange)) => ProgressClass::NoProgress,
            (OperationFamily::Read | OperationFamily::Search, _) => ProgressClass::EvidenceChanged,
            _ if known_scope => ProgressClass::EvidenceChanged,
            _ => ProgressClass::Unknown,
        };
        if self.episodes.len() == MAX_EPISODES {
            self.episodes.pop_front();
        }
        self.episodes.push_back(Episode {
            key: key.clone(),
            samples: VecDeque::from([key.evidence_key]),
            observations: 1,
            hinted: false,
            switch_required: false,
            reflection_requested: false,
            post_switch_probe: false,
            terminal: false,
        });
        EpisodeOutcome {
            class,
            decision: ProgressDecision::Continue,
            hint: None,
            request_reflection: None,
            episode_created: true,
        }
    }

    pub(crate) fn terminal_for(
        observation: &ProgressObservation,
        outcome: &EpisodeOutcome,
    ) -> Option<H07Terminal> {
        (outcome.decision == ProgressDecision::TerminalNonRetryable)
            .then(|| H07Terminal::from_observation(observation))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.episodes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_recovery::{OutputRange, OutputStream};
    use serde_json::json;

    fn h07_m7_validation(errors: u64, warnings: u64) -> ProgressObservation {
        let call = call("check", json!({"path": "."}));
        let result = ToolResult {
            output: format!("trusted fixture: {errors} errors, {warnings} warnings"),
            success: true,
            structured_metadata: Some(json!({
                "validation": {
                    "schema": "octos.validation.v1",
                    "adapter": "check",
                    "ran": true,
                    "errors": errors,
                    "warnings": warnings,
                }
            })),
            ..Default::default()
        };
        ObservationFacts::from_result(&call, &result).finish(&result.output, None)
    }

    #[test]
    fn h07_m7_trusted_validation_improvement_and_regression_are_directional() {
        let mut tracker = EpisodeTracker::default();
        let failing = h07_m7_validation(2, 1);
        assert_eq!(tracker.observe(&failing).class, ProgressClass::Unknown);
        assert_eq!(tracker.observe(&failing).class, ProgressClass::NoProgress);

        let improved = h07_m7_validation(1, 1);
        let outcome = tracker.observe(&improved);
        assert_eq!(outcome.class, ProgressClass::ValidationImproved);
        assert!(outcome.episode_created);

        let regressed = h07_m7_validation(3, 0);
        let outcome = tracker.observe(&regressed);
        assert_eq!(outcome.class, ProgressClass::Regressed);
        assert!(outcome.episode_created);
    }

    #[test]
    fn h07_m7_plain_tests_passed_text_is_not_trusted_validation_progress() {
        let call = call("check", json!({"path": "."}));
        let result = ToolResult {
            output: "tests passed".to_owned(),
            success: true,
            ..Default::default()
        };
        let observation =
            ObservationFacts::from_result(&call, &result).finish(&result.output, None);
        assert_eq!(
            observation.confidence,
            ObservationConfidence::ExactTextFallback
        );
        assert!(!observation.semantic_eligible);
        assert_eq!(observation.validation_score, None);
        let outcome = EpisodeTracker::default().observe(&observation);
        assert_eq!(outcome.class, ProgressClass::Unknown);
        assert!(!outcome.episode_created);
    }

    #[test]
    fn h07_m7_unrelated_successful_read_does_not_clear_validation_stall() {
        let mut tracker = EpisodeTracker::default();
        let stalled = h07_m7_validation(2, 1);
        assert_eq!(
            tracker.observe(&stalled).decision,
            ProgressDecision::Continue
        );
        assert_eq!(tracker.observe(&stalled).decision, ProgressDecision::Hint);

        let read = call("read_file", json!({"path": "diagnostic.txt"}));
        let source = OutputSource::File {
            target: "/workspace/diagnostic.txt".into(),
            sha256: format!("sha256:{}", "d".repeat(64)),
        };
        let ranges = [OutputRange {
            stream: OutputStream::File,
            start: 0,
            end: 10,
            lines: Some((1, 1)),
        }];
        let unrelated = ObservationFacts::from_result(
            &read,
            &ToolResult {
                output: "diagnostic".into(),
                success: true,
                ..Default::default()
            },
        )
        .finish_with_source("diagnostic", Some((&source, &ranges, false)));
        assert_eq!(
            tracker.observe(&unrelated).decision,
            ProgressDecision::Continue
        );
        assert_eq!(
            tracker.observe(&stalled).decision,
            ProgressDecision::SwitchRequired
        );
    }

    #[test]
    fn h07_m5_verified_wait_requires_a_live_runtime_handle() {
        let mut tools = ToolRegistry::new();
        tools.mark_spawn_only("async_tool", None);
        let supervisor = tools.supervisor();
        let live = supervisor.register("async_tool", "call-live", Some("session"));
        supervisor.mark_running(&live);
        let call = call("async_tool", json!({}));
        let body = json!({"task_handle": live.clone()}).to_string();
        let fact = verified_wait_fact(&tools, &call, Some(&body)).expect("live handle");
        let observation = ProgressObservation::verified_wait(&call, &fact, &body);
        let mut tracker = EpisodeTracker::default();
        assert_eq!(
            tracker.observe(&observation).class,
            ProgressClass::VerifiedWait
        );
        assert_eq!(
            tracker.observe(&observation).class,
            ProgressClass::VerifiedWait
        );

        supervisor.mark_completed(&live, vec![]);
        assert!(verified_wait_fact(&tools, &call, Some(&body)).is_none());
        let failed = supervisor.register("async_tool", "call-failed", Some("session"));
        supervisor.mark_failed(&failed, "failed".to_string());
        let failed_body = json!({"task_handle": failed}).to_string();
        assert!(verified_wait_fact(&tools, &call, Some(&failed_body)).is_none());
        let parked = supervisor.register("async_tool", "call-parked", Some("session"));
        supervisor.mark_parked(&parked, "reattach".to_string());
        let parked_body = json!({"task_handle": parked}).to_string();
        assert!(verified_wait_fact(&tools, &call, Some(&parked_body)).is_none());
        let missing = json!({"task_handle": "missing"}).to_string();
        assert!(verified_wait_fact(&tools, &call, Some(&missing)).is_none());
    }

    #[test]
    fn h07_m5_live_read_output_changes_evidence_without_stalling_wait() {
        let tools = ToolRegistry::new();
        let supervisor = tools.supervisor();
        let live = supervisor.register("async_tool", "call-live", Some("session"));
        supervisor.mark_running(&live);
        let read = call("read_task_output", json!({"task_handle": live}));
        let fact = verified_wait_fact(&tools, &read, None).expect("live handle");
        let first = ProgressObservation::verified_wait(&read, &fact, "page one");
        let changed = ProgressObservation::verified_wait(&read, &fact, "page two");
        let mut tracker = EpisodeTracker::default();
        assert_eq!(tracker.observe(&first).class, ProgressClass::VerifiedWait);
        assert_eq!(tracker.observe(&first).class, ProgressClass::VerifiedWait);
        assert_eq!(
            tracker.observe(&changed).class,
            ProgressClass::EvidenceChanged
        );
        assert_eq!(tracker.observe(&changed).class, ProgressClass::VerifiedWait);
    }

    #[test]
    fn h07_m5_terminal_message_is_bounded_and_uses_observed_scope() {
        let call = call("diff_edit", json!({"path": "src/main.rs"}));
        let observation = h07_m4_mutation(&call, "modified", true, "confirmed", 'a', 'a');
        let outcome = EpisodeOutcome {
            class: ProgressClass::NoProgress,
            decision: ProgressDecision::TerminalNonRetryable,
            hint: None,
            request_reflection: None,
            episode_created: false,
        };
        let terminal = EpisodeTracker::terminal_for(&observation, &outcome).unwrap();
        assert!(terminal.message.contains("mutation"));
        assert!(terminal.message.contains("main.rs"));
        assert!(terminal.message.len() <= MAX_HINT_BYTES);
    }

    fn h07_m4_mutation(
        call: &ToolCall,
        outcome: &str,
        file_modified: bool,
        final_state: &str,
        final_digest_char: char,
        write_digest_char: char,
    ) -> ProgressObservation {
        ObservationFacts::from_result(
            call,
            &ToolResult {
                output: "long mutation report\nExit code: 0".repeat(8),
                success: true,
                file_modified: file_modified.then(|| "a.rs".into()),
                structured_metadata: Some(json!({
                    "outcome": outcome,
                    "file_modified": file_modified,
                    "final_state": final_state,
                    "write_version": {"content_sha256": format!("sha256:{}", write_digest_char.to_string().repeat(64))},
                    "final_version": {"content_sha256": format!("sha256:{}", final_digest_char.to_string().repeat(64))},
                    "changed_range": {"before": {"start": 1, "count": 200}},
                })),
                ..Default::default()
            },
        )
        .finish("long mutation report\nExit code: 0", None)
    }

    #[test]
    fn h07_m4_final_version_owns_mutation_progress_and_no_change_stalls() {
        let edit = call("edit_file", json!({"path": "a.rs", "old_string": "x"}));
        let first = h07_m4_mutation(&edit, "modified", true, "confirmed", 'b', 'a');
        let same_final_different_write =
            h07_m4_mutation(&edit, "modified", true, "confirmed", 'b', 'c');
        let next_final = h07_m4_mutation(&edit, "modified", true, "confirmed", 'd', 'c');
        assert_eq!(first.evidence_key, same_final_different_write.evidence_key);
        let mut tracker = EpisodeTracker::default();
        assert_eq!(tracker.observe(&first).class, ProgressClass::StateChanged);
        let repeated = tracker.observe(&same_final_different_write);
        assert_eq!(repeated.class, ProgressClass::NoProgress);
        assert_eq!(repeated.decision, ProgressDecision::Hint);
        assert_eq!(
            tracker.observe(&next_final).class,
            ProgressClass::StateChanged
        );

        let no_change = h07_m4_mutation(&edit, "no_change", false, "confirmed", 'd', 'e');
        let mut tracker = EpisodeTracker::default();
        assert_eq!(tracker.observe(&no_change).class, ProgressClass::NoProgress);
        assert_eq!(tracker.observe(&no_change).decision, ProgressDecision::Hint);
    }

    #[test]
    fn h07_m4_unconfirmed_and_conflicting_mutations_stay_unknown() {
        let edit = call("edit_file", json!({"path": "a.rs"}));
        let unconfirmed = h07_m4_mutation(&edit, "modified", true, "unconfirmed", 'b', 'a');
        assert!(!unconfirmed.semantic_eligible);
        assert!(unconfirmed.has_authoritative_progress());
        let mut tracker = EpisodeTracker::default();
        assert_eq!(tracker.observe(&unconfirmed).class, ProgressClass::Unknown);
        assert_eq!(tracker.len(), 0);

        let conflicting = ObservationFacts::from_result(
            &edit,
            &ToolResult {
                success: true,
                structured_metadata: Some(json!({
                    "outcome": "modified", "file_modified": true,
                    "final_state": "confirmed",
                    "final_version": {"content_sha256": format!("sha256:{}", "b".repeat(64))}
                })),
                ..Default::default()
            },
        )
        .finish("claimed success", None);
        assert_eq!(
            conflicting.diagnostic,
            Some(ObservationDiagnostic::ConflictingMutationFields)
        );
        assert!(!conflicting.semantic_eligible);
        assert!(conflicting.has_authoritative_progress());
        assert_eq!(tracker.observe(&conflicting).class, ProgressClass::Unknown);
    }

    #[test]
    fn h07_m4_read_pages_use_source_range_and_version_evidence() {
        let read = call("read_file", json!({"path": "alias/a.rs"}));
        let recall = call("recall", json!({"output_id": "random", "offset": 10}));
        let source = OutputSource::File {
            target: "/workspace/a.rs".into(),
            sha256: format!("sha256:{}", "a".repeat(64)),
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
        let observe = |call: &ToolCall, source: &OutputSource, ranges: &[OutputRange]| {
            ObservationFacts::from_result(
                call,
                &ToolResult {
                    success: true,
                    ..Default::default()
                },
            )
            .finish_with_source("same visible text", Some((source, ranges, false)))
        };
        let first_read = observe(&read, &source, &first);
        let recalled_same = observe(&recall, &source, &first);
        assert_eq!(first_read.target_key, recalled_same.target_key);
        assert_eq!(first_read.evidence_key, recalled_same.evidence_key);
        let mut tracker = EpisodeTracker::default();
        assert_eq!(
            tracker.observe(&first_read).class,
            ProgressClass::EvidenceChanged
        );
        assert_eq!(
            tracker.observe(&recalled_same).decision,
            ProgressDecision::Hint
        );
        assert_eq!(
            tracker.observe(&observe(&recall, &source, &next)).class,
            ProgressClass::EvidenceChanged
        );
        let changed_source = OutputSource::File {
            target: "/workspace/a.rs".into(),
            sha256: format!("sha256:{}", "b".repeat(64)),
        };
        assert_eq!(
            tracker
                .observe(&observe(&read, &changed_source, &first))
                .class,
            ProgressClass::EvidenceChanged
        );
    }

    #[test]
    fn h07_m4_recall_search_uses_bounded_match_coordinates() {
        let search_call = call(
            "recall",
            json!({"output_id": "artifact", "stream": "file", "query": "needle"}),
        );
        let observed = |start: u64, snippet: &str| {
            let output = json!({
                "output_id": "artifact", "stream": "file",
                "searched_range": [0, 4096],
                "matches": [{"start": start, "end": start + 6, "snippet": snippet}],
                "search_complete": true, "artifact_complete": true,
                "next_offset": null, "scan_limit_bytes": 4096,
            })
            .to_string();
            ObservationFacts::from_result(
                &search_call,
                &ToolResult {
                    output: output.clone(),
                    success: true,
                    ..Default::default()
                },
            )
            .finish(&output, None)
        };
        let first = observed(12, "volatile snippet one");
        let same_coordinates = observed(12, "volatile snippet two");
        let new_hit = observed(24, "new hit");
        assert_eq!(first.family, OperationFamily::Search);
        assert_eq!(first.confidence, ObservationConfidence::TrustedAdapter);
        assert_eq!(first.evidence_key, same_coordinates.evidence_key);
        let mut tracker = EpisodeTracker::default();
        assert_eq!(
            tracker.observe(&first).class,
            ProgressClass::EvidenceChanged
        );
        assert_eq!(
            tracker.observe(&same_coordinates).decision,
            ProgressDecision::Hint
        );
        assert_eq!(
            tracker.observe(&new_hit).class,
            ProgressClass::EvidenceChanged
        );
    }

    #[test]
    fn h07_m4_typed_file_modified_false_infers_no_change() {
        let edit = call("edit_file", json!({"path": "a.rs"}));
        let observation = ObservationFacts::from_result(
            &edit,
            &ToolResult {
                success: true,
                structured_metadata: Some(json!({
                    "file_modified": false,
                    "final_state": "confirmed",
                    "final_version": {
                        "content_sha256": format!("sha256:{}", "a".repeat(64))
                    }
                })),
                ..Default::default()
            },
        )
        .finish("unchanged", None);
        assert_eq!(observation.outcome, Some(MutationOutcome::NoChange));
        assert_eq!(
            EpisodeTracker::default().observe(&observation).class,
            ProgressClass::NoProgress
        );
    }

    fn h07_m3_rejection(call: &ToolCall, volatile: u64, expected_line: u64) -> ProgressObservation {
        ObservationFacts::from_result(
            call,
            &ToolResult {
                success: false,
                output: format!("request_id={volatile} duration={volatile}ms"),
                structured_metadata: Some(json!({
                    "error_code": "diff_context_no_match",
                    "file_modified": false,
                    "matcher": "context",
                    "reason": "no_match",
                    "hunk_index": 0,
                    "expected_line": expected_line,
                    "occurrence_count": 1,
                    "current_version": {"content_sha256": format!("sha256:{}", "a".repeat(64))},
                    "searched_context_digest": format!("sha256:{volatile}"),
                    "duration_ms": volatile,
                    "request_id": format!("random-{volatile}"),
                    "candidates": [{
                        "line_range": {"start": 10, "end": 12},
                        "matcher": "context",
                        "score": volatile,
                        "excerpt": format!("volatile-{volatile}")
                    }]
                })),
                ..Default::default()
            },
        )
        .finish(&format!("request_id={volatile}"), None)
    }

    #[test]
    fn h07_m3_typed_episode_ignores_volatile_attempts_and_switches_once() {
        let mut tracker = EpisodeTracker::default();
        let mut observed = Vec::new();
        for n in 0..3 {
            let call = call(
                "diff_edit",
                json!({
                    "path": "/workspace/目标.rs", "diff": format!("attempt {n}"),
                    "limit": n, "request_id": n
                }),
            );
            observed.push(tracker.observe(&h07_m3_rejection(&call, n, 9)));
        }
        assert_eq!(observed[0].decision, ProgressDecision::Continue);
        assert_eq!(observed[1].decision, ProgressDecision::Hint);
        assert_eq!(observed[2].decision, ProgressDecision::SwitchRequired);
        assert!(observed[0].request_reflection.is_none());
        assert!(observed[1].request_reflection.is_none());
        let reflection = observed[2]
            .request_reflection
            .as_ref()
            .expect("switch_required should request one strategy reflection");
        assert_eq!(reflection.category, "mutate/no_progress");
        assert!(reflection.target.contains("目标.rs"));
        assert!(reflection.evidence.contains("diff_context_no_match"));
        assert!(observed[1].hint.as_ref().unwrap().len() <= MAX_HINT_BYTES);
        let fourth = call(
            "diff_edit",
            json!({"path": "/workspace/目标.rs", "diff": "fourth attempt"}),
        );
        let terminal = tracker.observe(&h07_m3_rejection(&fourth, 4, 9));
        assert_eq!(terminal.decision, ProgressDecision::TerminalNonRetryable);
        assert!(terminal.request_reflection.is_none());
        assert!(observed[2].hint.as_ref().unwrap().len() <= MAX_HINT_BYTES);
        assert_eq!(tracker.len(), 1);
        let different = h07_m3_rejection(
            &call("diff_edit", json!({"path": "/workspace/目标.rs"})),
            10,
            10,
        );
        assert_eq!(
            tracker.observe(&different).class,
            ProgressClass::EvidenceChanged
        );
        assert!(tracker.episodes[0].post_switch_probe);
        let repeated = h07_m3_rejection(
            &call(
                "diff_edit",
                json!({"path": "/workspace/目标.rs", "diff": "new"}),
            ),
            11,
            9,
        );
        assert_eq!(
            tracker.observe(&repeated).decision,
            ProgressDecision::TerminalNonRetryable
        );
    }

    #[test]
    fn h07_m3_fallback_and_harness_error_keep_their_confidence_boundary() {
        let call = call("shell", json!({"command": "x"}));
        let first = ObservationFacts::from_error(&call, "tool_execution").finish("id=1", None);
        let second = ObservationFacts::from_error(&call, "tool_execution").finish("id=2", None);
        assert_ne!(first.evidence_key, second.evidence_key);
        let mut tracker = EpisodeTracker::default();
        assert_eq!(tracker.observe(&first).class, ProgressClass::Unknown);
        assert_eq!(
            tracker.observe(&second).decision,
            ProgressDecision::Continue
        );
        assert_eq!(tracker.len(), 0);

        let error = HarnessError::ToolExecution {
            tool_name: "shell".into(),
            message: "provider request id=1".into(),
        };
        let typed = ObservationFacts::from_harness_error(&call, &error, Some("NotFound"))
            .finish("id=1", None);
        let changed_id = ObservationFacts::from_harness_error(&call, &error, Some("NotFound"))
            .finish("id=2", None);
        assert_eq!(typed.evidence_key, changed_id.evidence_key);
        assert_eq!(tracker.observe(&typed).decision, ProgressDecision::Continue);
        assert_eq!(
            tracker.observe(&changed_id).decision,
            ProgressDecision::Hint
        );
        let other_reason =
            ObservationFacts::from_harness_error(&call, &error, Some("PermissionDenied"))
                .finish("id=3", None);
        assert_eq!(
            tracker.observe(&other_reason).class,
            ProgressClass::EvidenceChanged
        );
    }

    #[test]
    fn h07_m3_expected_actual_change_is_new_evidence_at_same_location() {
        let call = call(
            "edit_file",
            json!({"path": "a.rs", "old_string": "attempt"}),
        );
        let observed = |expected: &str, actual: &str| {
            ObservationFacts::from_result(
                &call,
                &ToolResult {
                    success: false,
                    structured_metadata: Some(json!({
                        "error_code": "edit_no_match",
                        "file_modified": false,
                        "matcher": "exact",
                        "reason": "no_match",
                        "occurrence_count": 0,
                        "current_version": {"content_sha256": format!("sha256:{}", "a".repeat(64))},
                        "expected": expected,
                        "actual": actual,
                    })),
                    ..Default::default()
                },
            )
            .finish("visible", None)
        };
        let mut tracker = EpisodeTracker::default();
        tracker.observe(&observed("left", "right"));
        assert_eq!(
            tracker.observe(&observed("left", "changed")).class,
            ProgressClass::EvidenceChanged
        );
        assert_eq!(tracker.len(), 2);
    }

    #[test]
    fn h07_m3_candidate_ranking_does_not_change_stable_locations() {
        let tool_call = call("edit_file", json!({"path": "a.rs"}));
        let candidate = |line: u64, score: u64| {
            json!({
                "line_range": {"start": line, "end": line + 1},
                "matcher": "exact", "score": score,
                "excerpt": format!("request-{score}")
            })
        };
        let observed = |candidates: Vec<Value>| {
            ObservationFacts::from_result(
                &tool_call,
                &ToolResult {
                    success: false,
                    structured_metadata: Some(json!({
                        "error_code": "edit_ambiguous", "file_modified": false,
                        "matcher": "exact", "reason": "multiple_matches",
                        "occurrence_count": 2, "candidates": candidates,
                        "current_version": {"content_sha256": format!("sha256:{}", "a".repeat(64))}
                    })),
                    ..Default::default()
                },
            )
            .finish("different visible output", None)
        };
        let first = observed(vec![candidate(10, 1), candidate(20, 2)]);
        let reordered = observed(vec![candidate(20, 9), candidate(10, 8)]);
        assert_eq!(first.evidence_key, reordered.evidence_key);
        assert_eq!(
            ProgressObservation::base(&call("edit_file", json!({"path": "a.rs"}))).target_key,
            ProgressObservation::base(&call("diff_edit", json!({"path": "a.rs"}))).target_key
        );
    }

    #[test]
    fn h07_m3_lru_eviction_full_digest_and_very_long_run_are_bounded() {
        let mut tracker = EpisodeTracker::default();
        let mut seed = h07_m3_rejection(&call("diff_edit", json!({"path": "a.rs"})), 0, 1);
        seed.target_key = format!("sha256:{}", "a".repeat(64));
        tracker.observe(&seed);
        let mut collision = seed.clone();
        collision.target_key = format!("sha256:{}", "a".repeat(63) + "b");
        assert_eq!(
            tracker.observe(&collision).decision,
            ProgressDecision::Continue
        );
        assert_eq!(tracker.len(), 2);
        for n in 0..50_000u32 {
            let mut observation = seed.clone();
            observation.target_key = format!("sha256:{n:064x}");
            tracker.observe(&observation);
        }
        assert_eq!(tracker.len(), MAX_EPISODES);
        assert_eq!(tracker.observe(&seed).decision, ProgressDecision::Continue);
        assert!(
            tracker
                .episodes
                .iter()
                .all(|episode| episode.samples.len() <= MAX_SAMPLES)
        );
        let mut saturating = EpisodeTracker::default();
        saturating.observe(&seed);
        saturating.episodes[0].observations = u8::MAX;
        assert_eq!(
            saturating.observe(&seed).decision,
            ProgressDecision::SwitchRequired
        );
        assert_eq!(saturating.episodes[0].observations, u8::MAX);
    }

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
        assert!(!observation.semantic_eligible);
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
        assert_eq!(observed.confidence, ObservationConfidence::TrustedAdapter);
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
