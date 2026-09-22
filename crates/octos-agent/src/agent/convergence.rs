//! Soft convergence checkpoints for long-running interactive turns.
//!
//! Unlike a hard iteration budget, these checkpoints never end the turn.
//! They periodically give the model a tools-disabled round in which it must
//! synthesize evidence, notice repeated work, and choose a smaller next step.

use std::time::{Duration, Instant};

use octos_core::TokenUsage;

const DEFAULT_LLM_CALL_INTERVAL: u32 = 20;
const DEFAULT_ACTIVE_TOKEN_INTERVAL: u64 = 100_000;
const DEFAULT_ELAPSED_INTERVAL_SECS: u64 = 300;
const MAX_PENDING_REASONS: usize = 8;
const MAX_REFLECTION_GOAL_BYTES: usize = 800;
const MAX_RELATED_REASON_BYTES: usize = 320;

/// Typed tail envelope for the transient reflection. It is re-injected as a
/// USER-role context event, never as a System row: Anthropic hoists every
/// System row into the `system` field and OpenAI-compatible caches key on the
/// exact serialized prefix, so a System row whose text changes at each
/// checkpoint would rewrite the stable prefix inside the epoch (ADR "Stable
/// versus volatile prompt content"). `authority="background"` marks the block
/// as working memory rather than a user instruction.
const CHECKPOINT_CONTEXT_OPEN: &str =
    "<context_event kind=\"convergence_checkpoint\" authority=\"background\">";
const CHECKPOINT_CONTEXT_CLOSE: &str = "</context_event>";
const CHECKPOINT_CONTEXT_PREFIX: &str = "[internal convergence checkpoint]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CheckpointReason {
    LlmCalls {
        calls: u32,
    },
    ActiveTokens {
        tokens: u64,
    },
    Elapsed {
        elapsed: Duration,
    },
    FileChurn {
        path: String,
        edits: usize,
        escalation: bool,
    },
    PeerPolling {
        tool_name: String,
    },
    VerifiedWait {
        tool_name: String,
    },
    /// H07 strategy reflection after an episode reaches `switch_required`.
    /// The payload is deliberately typed and bounded; raw tool output never
    /// enters the private reflection request.
    SemanticNoProgress {
        category: String,
        target: String,
        evidence: String,
    },
    Combined(Vec<CheckpointReason>),
}

impl CheckpointReason {
    fn describe(&self) -> String {
        match self {
            Self::LlmCalls { calls } => {
                format!("{calls} LLM action calls completed without a convergence checkpoint")
            }
            Self::ActiveTokens { tokens } => {
                format!("{tokens} active input/output tokens accumulated")
            }
            Self::Elapsed { elapsed } => {
                format!("{} elapsed", format_elapsed(*elapsed))
            }
            Self::PeerPolling { tool_name } => {
                format!("{tool_name} returned the same asynchronous peer snapshot three times")
            }
            Self::VerifiedWait { tool_name } => {
                format!("{tool_name} returned unchanged output for the same live task three times")
            }
            Self::SemanticNoProgress {
                category,
                target,
                evidence,
            } => format!("H07 {category} episode at {target} reached switch_required ({evidence})"),
            Self::Combined(reasons) => reasons
                .iter()
                .map(Self::describe)
                .collect::<Vec<_>>()
                .join("; "),
            Self::FileChurn {
                path,
                edits,
                escalation,
            } => {
                let escalation_note = if *escalation {
                    "; provider/model escalation requested"
                } else {
                    ""
                };
                format!("{path} was modified {edits} times{escalation_note}")
            }
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Self::SemanticNoProgress { .. } => 0,
            Self::FileChurn { .. } => 1,
            Self::VerifiedWait { .. } => 2,
            Self::PeerPolling { .. } => 3,
            Self::LlmCalls { .. } | Self::ActiveTokens { .. } | Self::Elapsed { .. } => 4,
            Self::Combined(_) => 5,
        }
    }

    pub(super) fn contains_semantic_no_progress(&self) -> bool {
        matches!(self, Self::SemanticNoProgress { .. })
            || matches!(self, Self::Combined(reasons) if reasons.iter().any(Self::contains_semantic_no_progress))
    }

    fn contains_peer_polling(&self) -> bool {
        matches!(self, Self::PeerPolling { .. })
            || matches!(self, Self::Combined(reasons) if reasons.iter().any(Self::contains_peer_polling))
    }

    fn contains_verified_wait(&self) -> bool {
        matches!(self, Self::VerifiedWait { .. })
            || matches!(self, Self::Combined(reasons) if reasons.iter().any(Self::contains_verified_wait))
    }

    fn contains_escalated_file_churn(&self) -> bool {
        matches!(
            self,
            Self::FileChurn {
                escalation: true,
                ..
            }
        ) || matches!(self, Self::Combined(reasons) if reasons.iter().any(Self::contains_escalated_file_churn))
    }

    fn into_reasons(self) -> Vec<Self> {
        match self {
            Self::Combined(reasons) => reasons,
            reason => vec![reason],
        }
    }

    fn from_reasons(mut reasons: Vec<Self>) -> Option<Self> {
        reasons.sort_by_key(Self::priority);
        reasons.dedup();
        reasons.truncate(MAX_PENDING_REASONS);
        match reasons.len() {
            0 => None,
            1 => reasons.pop(),
            _ => Some(Self::Combined(reasons)),
        }
    }
}

/// Per-turn state for recurring, non-terminal reflection checkpoints.
pub(super) struct ConvergenceController {
    started_at: Instant,
    last_checkpoint_at: Instant,
    llm_call_interval: u32,
    active_token_interval: u64,
    elapsed_interval: Duration,
    /// Action calls COMPLETED so far in this turn, recorded by the loop after
    /// each successful action response. Reflection calls are never recorded,
    /// so they cannot accelerate the next checkpoint.
    completed_action_calls: u32,
    /// `completed_action_calls` at the last checkpoint (0 at turn start).
    action_calls_at_checkpoint: u32,
    /// Action-call active tokens observed at the last checkpoint.
    active_tokens_at_checkpoint: u64,
    /// Active tokens consumed by the reflection calls themselves. The turn
    /// records them as real spend; the thresholds must not.
    reflection_active_tokens: u64,
    pending_reason: Option<CheckpointReason>,
    latest_reflection: Option<String>,
    checkpoints: u32,
}

impl ConvergenceController {
    pub(super) fn from_env(started_at: Instant) -> Self {
        let llm_call_interval = env_u64(
            "OCTOS_CONVERGENCE_LLM_CALLS",
            u64::from(DEFAULT_LLM_CALL_INTERVAL),
            2,
            10_000,
        ) as u32;
        let active_token_interval = env_u64(
            "OCTOS_CONVERGENCE_ACTIVE_TOKENS",
            DEFAULT_ACTIVE_TOKEN_INTERVAL,
            1_000,
            100_000_000,
        );
        let elapsed_interval = Duration::from_secs(env_u64(
            "OCTOS_CONVERGENCE_SECS",
            DEFAULT_ELAPSED_INTERVAL_SECS,
            10,
            86_400,
        ));
        Self::new(
            started_at,
            llm_call_interval,
            active_token_interval,
            elapsed_interval,
        )
    }

    pub(super) fn new(
        started_at: Instant,
        llm_call_interval: u32,
        active_token_interval: u64,
        elapsed_interval: Duration,
    ) -> Self {
        Self {
            started_at,
            last_checkpoint_at: started_at,
            llm_call_interval,
            active_token_interval,
            elapsed_interval,
            completed_action_calls: 0,
            action_calls_at_checkpoint: 0,
            active_tokens_at_checkpoint: 0,
            reflection_active_tokens: 0,
            pending_reason: None,
            latest_reflection: None,
            checkpoints: 0,
        }
    }

    /// Task mode only enables H07-forced reflection. It intentionally has no
    /// periodic call/token/time schedule of its own.
    pub(super) fn semantic_only(started_at: Instant) -> Self {
        Self::new(started_at, u32::MAX, u64::MAX, Duration::MAX)
    }

    /// Queue an early checkpoint without overwriting another typed trigger.
    /// Multiple causes observed before the next loop boundary are merged into
    /// one tools-disabled request, with typed H07 evidence kept first.
    pub(super) fn force(&mut self, reason: CheckpointReason) {
        let mut reasons = self
            .pending_reason
            .take()
            .map(CheckpointReason::into_reasons)
            .unwrap_or_default();
        reasons.extend(reason.into_reasons());
        self.pending_reason = CheckpointReason::from_reasons(reasons);
    }

    pub(super) fn semantic_reflection_pending(&self) -> bool {
        self.pending_reason
            .as_ref()
            .is_some_and(CheckpointReason::contains_semantic_no_progress)
    }

    /// Record one COMPLETED action call. Call this after a successful action
    /// response, never for a checkpoint's reflection call.
    pub(super) fn record_action_call(&mut self) {
        self.completed_action_calls = self.completed_action_calls.saturating_add(1);
    }

    /// Called at the top of a loop iteration, BEFORE its action call. The call
    /// axis therefore counts COMPLETED action calls (via
    /// [`Self::record_action_call`]), never the pre-call iteration index, so
    /// an N-call checkpoint fires only once N action calls have finished and
    /// the reflection runs before action call N+1.
    pub(super) fn due(&mut self, usage: &TokenUsage) -> Option<CheckpointReason> {
        let mut reasons = self
            .pending_reason
            .take()
            .map(CheckpointReason::into_reasons)
            .unwrap_or_default();

        // Thresholds count completed ACTION calls and ACTION tokens only: a
        // checkpoint's own reflection call is real spend for the turn but is
        // never recorded as an action call and its tokens are subtracted.
        let calls_since_checkpoint = self
            .completed_action_calls
            .saturating_sub(self.action_calls_at_checkpoint);
        let action_tokens = self.action_active_tokens(usage);
        if calls_since_checkpoint >= self.llm_call_interval {
            reasons.push(CheckpointReason::LlmCalls {
                calls: calls_since_checkpoint,
            });
        } else if action_tokens.saturating_sub(self.active_tokens_at_checkpoint)
            >= self.active_token_interval
        {
            reasons.push(CheckpointReason::ActiveTokens {
                tokens: action_tokens,
            });
        } else if self.last_checkpoint_at.elapsed() >= self.elapsed_interval {
            reasons.push(CheckpointReason::Elapsed {
                elapsed: self.started_at.elapsed(),
            });
        }
        CheckpointReason::from_reasons(reasons)
    }

    pub(super) fn prompt(reason: &CheckpointReason, user_goal: &str) -> String {
        if reason.contains_semantic_no_progress() {
            return Self::semantic_prompt(reason, user_goal);
        }
        let escalation_instruction = if reason.contains_peer_polling() {
            "\nThis is asynchronous peer polling, not proof the tool cannot change. Identify whether peers are still running, awaiting input, completed, or failed from the actual tool evidence. Do not busy-wait or invent a completed result: choose independent work, address a peer's input request, or use an available bounded wait before gathering again. Only claim completion after seeing the required result."
        } else if reason.contains_verified_wait() {
            "\nThis is a runtime-confirmed live task wait, not proof the task failed or cannot change. Do not busy-wait, invent a completed result, or start a replacement task from this observation alone: choose independent work or use an available bounded wait before reading the same handle again. Only claim completion after observing its actual completed or failed state."
        } else if reason.contains_escalated_file_churn() {
            "\nThis is a repeated churn breach. If you cannot identify a substantially different, root-cause-driven approach, the next action must be to pause and ask the user for direction instead of editing again."
        } else {
            ""
        };
        format!(
            "[CONVERGENCE CHECKPOINT — internal working step, not a user-facing final answer]\n\
             Trigger: {}. Tools are disabled for this call. Briefly synthesize:\n\
             1. the user's actual goal and what is already complete;\n\
             2. concrete evidence gathered and changes made;\n\
             3. failures, uncertainty, or repeated work (especially repeated edits);\n\
             4. whether the current approach is converging;\n\
             5. the smallest 1–3 next actions, or COMPLETE if the goal is satisfied.\n\
             Be concise and factual. Do not address the user and do not call tools.{}",
            reason.describe(),
            escalation_instruction,
        )
    }

    fn semantic_prompt(reason: &CheckpointReason, user_goal: &str) -> String {
        let goal = bounded(user_goal.trim(), MAX_REFLECTION_GOAL_BYTES);
        let evidence = match reason {
            CheckpointReason::SemanticNoProgress {
                category,
                target,
                evidence,
            } => format!(
                "Episode category: {category}\nTarget: {target}\nBounded evidence: {evidence}"
            ),
            CheckpointReason::Combined(reasons) => reasons
                .iter()
                .map(|item| match item {
                    CheckpointReason::SemanticNoProgress {
                        category,
                        target,
                        evidence,
                    } => format!(
                        "Episode category: {category}\nTarget: {target}\nBounded evidence: {evidence}"
                    ),
                    other => format!(
                        "Related trigger: {}",
                        bounded(&other.describe(), MAX_RELATED_REASON_BYTES)
                    ),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => format!("Related trigger: {}", other.describe()),
        };
        format!(
            "[H07 STRATEGY REFLECTION — private working step]\n\
             User goal: {goal}\n\
             {evidence}\n\
             Tools are disabled. Choose one substantially different, verifiable next action. \
             State what fresh observation would demonstrate progress. Be concise; do not address \
             the user, claim completion, or repeat tool output."
        )
    }

    /// Record a completed checkpoint and re-arm the periodic thresholds: the
    /// next call-based checkpoint is due after `llm_call_interval` further
    /// COMPLETED action calls.
    ///
    /// `reflection_usage` is the reflection call's own usage when that call
    /// succeeded; its tokens are excluded from the action counters (the call
    /// itself is never recorded as an action call). `None` means the
    /// checkpoint failed open and the iteration proceeds to its normal action
    /// call, which is recorded when it completes.
    pub(super) fn complete(
        &mut self,
        usage: &TokenUsage,
        reflection_usage: Option<&TokenUsage>,
        reflection: String,
    ) {
        self.checkpoints = self.checkpoints.saturating_add(1);
        self.action_calls_at_checkpoint = self.completed_action_calls;
        if let Some(reflection_usage) = reflection_usage {
            self.reflection_active_tokens = self
                .reflection_active_tokens
                .saturating_add(active_tokens(reflection_usage));
        }
        self.active_tokens_at_checkpoint = self.action_active_tokens(usage);
        self.last_checkpoint_at = Instant::now();
        let reflection = reflection.trim();
        if !reflection.is_empty() {
            self.latest_reflection = Some(reflection.to_string());
        }
    }

    /// Turn-cumulative active tokens minus what the reflection calls consumed.
    fn action_active_tokens(&self, usage: &TokenUsage) -> u64 {
        active_tokens(usage).saturating_sub(self.reflection_active_tokens)
    }

    /// The latest reflection as a typed User-role tail envelope (see
    /// [`CHECKPOINT_CONTEXT_OPEN`]); `None` before the first checkpoint.
    pub(super) fn context_message(&self) -> Option<String> {
        self.latest_reflection.as_ref().map(|reflection| {
            format!(
                "{CHECKPOINT_CONTEXT_OPEN}\n{CHECKPOINT_CONTEXT_PREFIX}\n{reflection}\n\
                 Use this internal reflection to guide the next action. Verify it against the \n\
                 conversation and tool results; it is not a user message.\n\
                 {CHECKPOINT_CONTEXT_CLOSE}"
            )
        })
    }

    pub(super) fn checkpoints(&self) -> u32 {
        self.checkpoints
    }
}

/// True for the transient User-role reflection envelope produced by
/// [`ConvergenceController::context_message`], so the loop can strip it before
/// prompt repair/compaction and re-inject only the latest one.
pub(super) fn is_checkpoint_context(message: &str) -> bool {
    message.starts_with(CHECKPOINT_CONTEXT_OPEN)
}

pub(super) fn active_tokens(usage: &TokenUsage) -> u64 {
    u64::from(usage.input_tokens) + u64::from(usage.output_tokens)
}

pub(super) fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(min, max))
        .unwrap_or(default)
}

fn bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_controller(llm_calls: u32, active_tokens: u64) -> ConvergenceController {
        ConvergenceController::new(
            Instant::now(),
            llm_calls,
            active_tokens,
            Duration::from_secs(3600),
        )
    }

    fn record_action_calls(controller: &mut ConvergenceController, count: u32) {
        for _ in 0..count {
            controller.record_action_call();
        }
    }

    #[test]
    fn call_threshold_rearms_instead_of_stopping_the_turn() {
        // GAP-3: the call axis counts COMPLETED action calls, not the pre-call
        // iteration index (which fired one action early), and the reflection
        // call never counts: with N=3 the first checkpoint follows the 3rd
        // completed action call and the next one follows the 6th.
        let mut controller = new_controller(3, 10_000);
        let usage = TokenUsage::default();

        record_action_calls(&mut controller, 2);
        assert!(controller.due(&usage).is_none());
        controller.record_action_call();
        assert!(matches!(
            controller.due(&usage),
            Some(CheckpointReason::LlmCalls { calls: 3 })
        ));
        controller.complete(&usage, Some(&usage), "try one smaller edit".into());
        assert!(controller.due(&usage).is_none());
        record_action_calls(&mut controller, 2);
        assert!(controller.due(&usage).is_none());
        controller.record_action_call();
        assert!(controller.due(&usage).is_some());
        assert_eq!(controller.checkpoints(), 1);
    }

    #[test]
    fn should_fire_call_checkpoint_only_after_n_completed_action_calls() {
        // `due` runs BEFORE an iteration's action call: with two completed
        // action calls an N=3 checkpoint must not fire; it fires once the 3rd
        // has completed, i.e. before the 4th action call.
        let mut controller = new_controller(3, 10_000);
        let usage = TokenUsage::default();
        assert!(controller.due(&usage).is_none());
        controller.record_action_call();
        assert!(controller.due(&usage).is_none());
        controller.record_action_call();
        assert!(
            controller.due(&usage).is_none(),
            "only two action calls have completed"
        );
        controller.record_action_call();
        let reason = controller
            .due(&usage)
            .expect("three completed action calls are due");
        assert_eq!(reason, CheckpointReason::LlmCalls { calls: 3 });
        assert_eq!(
            reason.describe(),
            "3 LLM action calls completed without a convergence checkpoint"
        );
    }

    #[test]
    fn should_require_full_action_call_increment_when_a_checkpoint_reflection_ran() {
        let mut controller = new_controller(3, 10_000);
        let usage = TokenUsage::default();
        record_action_calls(&mut controller, 3);
        assert!(controller.due(&usage).is_some());

        // The reflection call ran; it is not an action call.
        controller.complete(&usage, Some(&usage), "reflect".into());
        assert!(controller.due(&usage).is_none());

        // The next three COMPLETED action calls re-arm the checkpoint; the
        // reflection must not be counted as one of them.
        record_action_calls(&mut controller, 2);
        assert!(
            controller.due(&usage).is_none(),
            "the reflection call must not count toward the next call threshold"
        );
        controller.record_action_call();
        assert!(matches!(
            controller.due(&usage),
            Some(CheckpointReason::LlmCalls { calls: 3 })
        ));
    }

    #[test]
    fn should_not_count_reflection_tokens_toward_the_next_token_checkpoint() {
        let mut controller = new_controller(100, 1_000);
        let action = TokenUsage {
            input_tokens: 900,
            output_tokens: 100,
            ..Default::default()
        };
        assert!(matches!(
            controller.due(&action),
            Some(CheckpointReason::ActiveTokens { tokens: 1_000 })
        ));

        // The reflection call itself is expensive (an uncached prefix); the
        // turn records it, the thresholds must not.
        let reflection = TokenUsage {
            input_tokens: 50_000,
            output_tokens: 500,
            ..Default::default()
        };
        let total = TokenUsage {
            input_tokens: 50_900,
            output_tokens: 600,
            ..Default::default()
        };
        controller.complete(&total, Some(&reflection), "reflect".into());

        assert!(controller.due(&total).is_none());
        let almost = TokenUsage {
            input_tokens: 51_899,
            output_tokens: 600,
            ..Default::default()
        };
        assert!(controller.due(&almost).is_none());
        let enough = TokenUsage {
            input_tokens: 51_900,
            output_tokens: 600,
            ..Default::default()
        };
        assert!(matches!(
            controller.due(&enough),
            Some(CheckpointReason::ActiveTokens { tokens: 2_000 })
        ));
    }

    #[test]
    fn should_rearm_call_checkpoint_after_n_completed_actions_when_checkpoint_fails_open() {
        let mut controller = new_controller(3, 10_000);
        let usage = TokenUsage::default();
        record_action_calls(&mut controller, 3);
        assert!(controller.due(&usage).is_some());

        // The reflection provider failed; the iteration proceeds to its action
        // call, which counts once it completes like any other.
        controller.complete(&usage, None, "Checkpoint failed".into());
        record_action_calls(&mut controller, 2);
        assert!(controller.due(&usage).is_none());
        controller.record_action_call();
        assert!(matches!(
            controller.due(&usage),
            Some(CheckpointReason::LlmCalls { calls: 3 })
        ));
    }

    #[test]
    fn should_wrap_reflection_in_typed_context_event_envelope_when_reinjected() {
        let mut controller = new_controller(3, 10_000);
        assert!(controller.context_message().is_none());

        controller.complete(
            &TokenUsage::default(),
            Some(&TokenUsage::default()),
            "  next: one bounded edit  ".into(),
        );

        let context = controller.context_message().expect("reflection stored");
        assert!(context.starts_with(
            "<context_event kind=\"convergence_checkpoint\" authority=\"background\">"
        ));
        assert!(context.contains("next: one bounded edit"));
        assert!(context.ends_with("</context_event>"));
        assert!(is_checkpoint_context(&context));
        assert!(!is_checkpoint_context(
            "<context_event kind=\"other\">x</context_event>"
        ));
        assert!(!is_checkpoint_context("[internal convergence checkpoint]"));
    }

    #[test]
    fn token_threshold_counts_active_io_but_not_cache_traffic() {
        let mut controller = new_controller(100, 1_000);
        let usage = TokenUsage {
            input_tokens: 400,
            output_tokens: 100,
            cache_read_tokens: 50_000,
            cache_write_tokens: 10_000,
            ..Default::default()
        };
        assert!(controller.due(&usage).is_none());

        let usage = TokenUsage {
            input_tokens: 800,
            output_tokens: 250,
            ..Default::default()
        };
        assert!(matches!(
            controller.due(&usage),
            Some(CheckpointReason::ActiveTokens { tokens: 1_050 })
        ));
    }

    #[test]
    fn forced_file_churn_takes_priority() {
        let mut controller = new_controller(3, 1_000);
        controller.force(CheckpointReason::FileChurn {
            path: "app.css".into(),
            edits: 5,
            escalation: false,
        });
        assert!(matches!(
            controller.due(&TokenUsage::default()),
            Some(CheckpointReason::FileChurn { path, .. }) if path == "app.css"
        ));
    }

    #[test]
    fn h07_m6_merges_typed_and_periodic_triggers_into_one_checkpoint() {
        let mut controller = new_controller(3, 10_000);
        controller.force(CheckpointReason::PeerPolling {
            tool_name: "peer_gather".into(),
        });
        controller.force(CheckpointReason::FileChurn {
            path: "src/app.rs".into(),
            edits: 5,
            escalation: false,
        });
        controller.force(CheckpointReason::SemanticNoProgress {
            category: "mutate/no_progress".into(),
            target: "app.rs".into(),
            evidence: "status=Failed; evidence=sha256:abc".into(),
        });
        record_action_calls(&mut controller, 3);

        let reason = controller
            .due(&TokenUsage::default())
            .expect("all due causes should merge");
        let CheckpointReason::Combined(reasons) = &reason else {
            panic!("expected one combined checkpoint, got {reason:?}");
        };
        assert_eq!(reasons.len(), 4);
        assert!(matches!(
            reasons.first(),
            Some(CheckpointReason::SemanticNoProgress { .. })
        ));
        assert!(
            reasons
                .iter()
                .any(|item| matches!(item, CheckpointReason::LlmCalls { calls: 3 }))
        );

        let prompt = ConvergenceController::prompt(&reason, "Fix the parser without looping");
        assert!(prompt.contains("User goal: Fix the parser without looping"));
        assert!(prompt.contains("Episode category: mutate/no_progress"));
        assert!(prompt.contains("Bounded evidence: status=Failed"));
        assert!(prompt.contains("Related trigger: peer_gather"));
        assert!(prompt.contains("different, verifiable next action"));
    }

    #[test]
    fn h07_m6_task_controller_has_no_periodic_schedule() {
        let mut controller = ConvergenceController::semantic_only(Instant::now());
        record_action_calls(&mut controller, 100);
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        assert!(controller.due(&usage).is_none());
        controller.force(CheckpointReason::SemanticNoProgress {
            category: "validate/no_progress".into(),
            target: "contract".into(),
            evidence: "status=Failed; evidence=sha256:def".into(),
        });
        assert!(matches!(
            controller.due(&usage),
            Some(CheckpointReason::SemanticNoProgress { .. })
        ));
    }

    #[test]
    fn elapsed_format_is_compact_for_ui_status() {
        assert_eq!(format_elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m05s");
    }
}
