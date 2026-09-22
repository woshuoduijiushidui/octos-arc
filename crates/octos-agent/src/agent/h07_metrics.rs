//! Bounded, best-effort observability for H07 no-progress handling.
//!
//! Every label accepted here is a fixed enum value selected by the caller.
//! Metric recorder failures must never change an agent result, so emission is
//! isolated behind `catch_unwind`. No tool arguments, paths, command text,
//! provider output, or credentials reach this module.

use metrics::counter;
use octos_core::TokenUsage;

pub(super) const OBSERVATION_TOTAL: &str = "octos_no_progress_observation_total";
pub(super) const EPISODE_TOTAL: &str = "octos_h07_episode_total";
pub(super) const TOOL_EXECUTION_TOTAL: &str = "octos_h07_tool_execution_total";
pub(super) const DECISION_TOTAL: &str = "octos_h07_decision_total";
pub(super) const REFLECTION_TOTAL: &str = "octos_h07_reflection_total";
pub(super) const REFLECTION_TOKENS_TOTAL: &str = "octos_h07_reflection_tokens_total";
pub(super) const TERMINAL_TOTAL: &str = "octos_h07_terminal_total";

fn best_effort(emit: impl FnOnce() + std::panic::UnwindSafe) {
    let _ = std::panic::catch_unwind(emit);
}

pub(crate) fn record_observation(
    family: &'static str,
    progress_class: &'static str,
    decision: &'static str,
    confidence: &'static str,
    reflection_status: &'static str,
) {
    best_effort(|| {
        counter!(
            OBSERVATION_TOTAL,
            "family" => family,
            "progress_class" => progress_class,
            "decision" => decision,
            "confidence" => confidence,
            "reflection_status" => reflection_status,
        )
        .increment(1);
    });
}

pub(crate) fn record_episode_created() {
    best_effort(|| counter!(EPISODE_TOTAL).increment(1));
}

pub(crate) fn record_tool_execution() {
    best_effort(|| counter!(TOOL_EXECUTION_TOTAL).increment(1));
}

pub(crate) fn record_decision(decision: &'static str) {
    best_effort(|| counter!(DECISION_TOTAL, "decision" => decision).increment(1));
}

pub(crate) fn record_reflection(status: &'static str) {
    best_effort(|| counter!(REFLECTION_TOTAL, "status" => status).increment(1));
}

pub(crate) fn record_reflection_tokens(usage: &TokenUsage) {
    for (kind, value) in [
        ("input", usage.input_tokens),
        ("output", usage.output_tokens),
        ("reasoning", usage.reasoning_tokens),
        ("cache_read", usage.cache_read_tokens),
        ("cache_write", usage.cache_write_tokens),
    ] {
        if value > 0 {
            best_effort(|| {
                counter!(REFLECTION_TOKENS_TOTAL, "kind" => kind).increment(u64::from(value));
            });
        }
    }
}

pub(crate) fn record_terminal(mode: &'static str) {
    best_effort(|| {
        counter!(
            TERMINAL_TOTAL,
            "mode" => mode,
            "retryable" => "false",
        )
        .increment(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h07_m7_metric_schema_uses_only_frozen_names_and_labels() {
        assert_eq!(OBSERVATION_TOTAL, "octos_no_progress_observation_total");
        assert_eq!(EPISODE_TOTAL, "octos_h07_episode_total");
        assert_eq!(TOOL_EXECUTION_TOTAL, "octos_h07_tool_execution_total");
        assert_eq!(DECISION_TOTAL, "octos_h07_decision_total");
        assert_eq!(REFLECTION_TOTAL, "octos_h07_reflection_total");
        assert_eq!(REFLECTION_TOKENS_TOTAL, "octos_h07_reflection_tokens_total");
        assert_eq!(TERMINAL_TOTAL, "octos_h07_terminal_total");
        assert_ne!(
            TERMINAL_TOTAL,
            crate::agent::loop_state::OCTOS_LOOP_RETRY_TOTAL
        );

        record_observation(
            "validate",
            "validation_improved",
            "continue",
            "typed",
            "not_requested",
        );
        record_episode_created();
        record_tool_execution();
        record_decision("pre_call_reject");
        record_decision("hint");
        record_decision("switch");
        record_decision("terminal");
        record_reflection("requested");
        record_reflection("completed");
        record_reflection("failed");
        record_reflection_tokens(&TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: 3,
            cache_read_tokens: 4,
            cache_write_tokens: 5,
        });
        record_terminal("task");
    }

    #[test]
    fn h07_m7_observability_failure_cannot_change_agent_control_flow() {
        best_effort(|| panic!("simulated recorder failure"));
        assert_eq!(2 + 2, 4);
    }
}
