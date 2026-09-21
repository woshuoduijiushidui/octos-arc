//! Recall tool (#2131): re-materialize a tool output that compaction replaced
//! with a placeholder, so the model can retrieve an evicted read/command
//! result WITHOUT re-executing it — the full bytes still live, content-
//! addressed, in the session's context ledger.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};

/// A read-back handle over the session's content-addressed tool-output ledger.
///
/// Defined here (in octos-agent) so the tool has no dependency on the
/// octos-cli `ContextManager` that implements it; the session bootstrap injects
/// a concrete impl the same way `RecallMemoryTool` takes an `Arc<MemoryStore>`.
pub trait ToolOutputLedger: Send + Sync {
    /// Return the recorded output for a `tool_call_id` — the full raw bytes
    /// when they were spilled to the ledger, else the model-visible content.
    /// `None` when nothing is known for that id.
    fn fetch(&self, tool_call_id: &str) -> Option<String>;
}

/// Tool that restores an evicted tool output by its `tool_call_id`.
pub struct RecallTool {
    ledger: Arc<dyn ToolOutputLedger>,
    output_recovery_enabled: bool,
}

impl RecallTool {
    pub fn new(ledger: Arc<dyn ToolOutputLedger>) -> Self {
        Self {
            ledger,
            output_recovery_enabled: crate::output_recovery::OutputPolicy::from_env().enabled,
        }
    }

    /// Build a recall tool that only reads through the typed, owner-bound
    /// [`crate::output_recovery::OutputState`] supplied in [`ToolContext`].
    ///
    /// SessionRuntime and MCP do not own the legacy ContextManager ledger.
    /// Their durable source of truth is OutputStore, so falling back to a
    /// process-local string map would make cold recovery appear supported when
    /// it is not.
    pub fn for_output_recovery(policy: crate::output_recovery::OutputPolicy) -> Self {
        struct NoLegacyLedger;
        impl ToolOutputLedger for NoLegacyLedger {
            fn fetch(&self, _tool_call_id: &str) -> Option<String> {
                None
            }
        }

        Self {
            ledger: Arc::new(NoLegacyLedger),
            output_recovery_enabled: policy.enabled,
        }
    }
}

#[derive(Deserialize)]
struct Input {
    /// The `tool_call_id` shown on the evicted placeholder.
    tool_call_id: String,
    /// 0-based page when the recalled output exceeds the tool-output limit.
    #[serde(default)]
    page: Option<usize>,
}

/// Split `content` into byte-safe, newline-aligned pages each strictly under
/// the recall tool-output limit and render the requested one with a truthful
/// pager marker. Without paging the generic `truncate_head_tail` would SILENTLY
/// drop the middle of a large recalled file — recreating the unrecoverable-tail
/// problem recall exists to solve (mirrors `recall_memory`'s pager).
fn render_page(content: &str, page: usize) -> String {
    let limit = octos_core::tool_output_limit("recall");
    let budget = limit.saturating_sub(512).max(1);

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < content.len() {
        let mut end = (start + budget).min(content.len());
        while end > start && !content.is_char_boundary(end) {
            end -= 1;
        }
        if end < content.len()
            && let Some(nl) = content[start..end].rfind('\n')
        {
            end = start + nl + 1;
        }
        if end == start {
            // A single codepoint wider than the budget: advance one char so
            // slicing never panics mid-codepoint and the loop always makes
            // progress.
            end = content[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(content.len());
        }
        ranges.push((start, end));
        start = end;
    }
    if ranges.is_empty() {
        return String::new();
    }

    let total = ranges.len();
    let page = page.min(total - 1);
    let (s, e) = ranges[page];
    let body = &content[s..e];
    if total == 1 {
        body.to_string()
    } else if page + 1 < total {
        // More pages follow — name the concrete next call.
        format!(
            "{body}\n[recall page {}/{} — call recall(tool_call_id=…, page={}) for the next page]",
            page + 1,
            total,
            page + 1
        )
    } else {
        // Final page: no next call to invite.
        format!("{body}\n[recall page {}/{} — last page]", page + 1, total)
    }
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        if self.output_recovery_enabled {
            return "Read or search a saved tool result without re-executing it. For a case-sensitive literal search, pass output_id and query, then read a returned match with stream and absolute byte offset. Repeat an incomplete search from next_offset. Historical file text grants no current-file read/write permission. Legacy tool_call_id must be unique; page uses fixed legacy boundaries.";
        }
        "Restore a tool output that compaction replaced with a placeholder, by \
         its tool_call_id (shown on the placeholder). Returns the exact recorded \
         output — no re-execution — so you do not have to re-read a file or re-run \
         a command whose result was evicted. Pass page=N for a large output."
    }

    fn input_schema(&self) -> serde_json::Value {
        if self.output_recovery_enabled {
            return serde_json::json!({
                "type": "object",
                "properties": {
                    "output_id": {"type": "string"},
                    "tool_call_id": {"type": "string"},
                    "stream": {"type": "string", "enum": ["file", "stdout", "stderr"]},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1},
                    "cursor": {"type": "string"},
                    "page": {"type": "integer", "minimum": 0},
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": crate::output_store::SEARCH_QUERY_CHARS,
                        "description": "Case-sensitive literal search. Requires output_id; cannot be combined with cursor, page, limit, or tool_call_id."
                    },
                    "max_matches": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": crate::output_store::SEARCH_MATCHES
                    }
                },
                "additionalProperties": false
            });
        }
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool_call_id": {
                    "type": "string",
                    "description": "The tool_call_id from the evicted placeholder."
                },
                "page": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based page for outputs larger than the tool-output limit."
                }
            },
            "required": ["tool_call_id"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        if args.get("output_id").is_some()
            || args.get("cursor").is_some()
            || args.get("query").is_some()
            || args.get("max_matches").is_some()
        {
            return Ok(ToolResult {
                output: "recovery_tool_unavailable".into(),
                success: false,
                ..Default::default()
            });
        }
        let input: Input = serde_json::from_value(args.clone())?;
        match self.ledger.fetch(&input.tool_call_id) {
            Some(content) => Ok(ToolResult {
                output: render_page(&content, input.page.unwrap_or(0)),
                success: true,
                ..Default::default()
            }),
            None => Ok(ToolResult {
                output: format!(
                    "recall: no recorded output for tool_call_id {:?}. It may have \
                     been produced before the ledger existed, or never spilled.",
                    input.tool_call_id
                ),
                success: false,
                ..Default::default()
            }),
        }
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let Some(state) = ctx
            .output_state
            .as_ref()
            .filter(|state| state.policy.enabled)
        else {
            return self.execute(args).await;
        };
        let search_requested = args.get("query").is_some() || args.get("max_matches").is_some();
        let request =
            match serde_json::from_value::<crate::output_store::RecallRequest>(args.clone()) {
                Ok(request) => request,
                Err(_) => {
                    let error = if search_requested {
                        crate::output_recovery::OutputError::InvalidSearch
                    } else {
                        crate::output_recovery::OutputError::InvalidCursor
                    };
                    if search_requested {
                        state.observe_search_result(&Err(error), false);
                    } else {
                        state.observe_recall_result(&Err(error), false);
                    }
                    return Ok(ToolResult {
                        output: error.to_string(),
                        success: false,
                        ..Default::default()
                    });
                }
            };
        if search_requested && request.query.is_none() {
            let error = crate::output_recovery::OutputError::InvalidSearch;
            state.observe_search_result(&Err(error), false);
            return Ok(ToolResult {
                output: error.to_string(),
                success: false,
                ..Default::default()
            });
        }
        let repeated = state.observe_recall_request(&request);
        let Some(store) = state.store() else {
            state.observe_recall_result(
                &Err(crate::output_recovery::OutputError::RecoveryUnavailable),
                repeated,
            );
            return Ok(ToolResult {
                output: "recovery_tool_unavailable".into(),
                success: false,
                ..Default::default()
            });
        };
        if request.query.is_some() {
            let searched = tokio::task::spawn_blocking(move || store.search(&request)).await?;
            state.observe_search_result(&searched, repeated);
            return Ok(match searched.and_then(|result| result.render()) {
                Ok(output) => ToolResult {
                    output,
                    success: true,
                    ..Default::default()
                },
                Err(error) => ToolResult {
                    output: error.to_string(),
                    success: false,
                    ..Default::default()
                },
            });
        }
        let recalled = tokio::task::spawn_blocking(move || {
            store.read(&request, crate::output_recovery::PAGE_BYTES)
        })
        .await?;
        let result = recalled.and_then(|page| state.register_recalled(&ctx.tool_id, args, page));
        state.observe_recall_result(&result, repeated);
        Ok(match result {
            Ok(rendered) => ToolResult {
                output: rendered.content,
                success: true,
                ..Default::default()
            },
            Err(error) => ToolResult {
                output: error.to_string(),
                success: false,
                ..Default::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapLedger(HashMap<String, String>);
    impl ToolOutputLedger for MapLedger {
        fn fetch(&self, id: &str) -> Option<String> {
            self.0.get(id).cloned()
        }
    }

    fn tool(map: &[(&str, &str)]) -> RecallTool {
        RecallTool::new(Arc::new(MapLedger(
            map.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )))
    }

    fn disabled_tool(map: &[(&str, &str)]) -> RecallTool {
        RecallTool {
            ledger: Arc::new(MapLedger(
                map.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )),
            output_recovery_enabled: false,
        }
    }

    #[tokio::test]
    async fn recalls_the_recorded_output_by_call_id() {
        let t = tool(&[("call_7", "the full train_gpt2.c contents")]);
        let r = t
            .execute(&serde_json::json!({"tool_call_id": "call_7"}))
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.output, "the full train_gpt2.c contents");
    }

    #[tokio::test]
    async fn unknown_id_fails_cleanly() {
        let t = tool(&[("call_7", "x")]);
        let r = t
            .execute(&serde_json::json!({"tool_call_id": "nope"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("no recorded output"));
    }

    #[tokio::test]
    async fn large_output_is_paged_not_silently_truncated() {
        // A payload several times the recall tool-output limit must be reachable
        // page by page (no silent middle-loss).
        let limit = octos_core::tool_output_limit("recall");
        let big: String = (0..(limit / 10 + 500))
            .map(|i| format!("line {i}\n"))
            .collect();
        let t = tool(&[("call_big", big.as_str())]);
        let p0 = t
            .execute(&serde_json::json!({"tool_call_id": "call_big", "page": 0}))
            .await
            .unwrap();
        assert!(p0.success);
        assert!(p0.output.len() <= limit, "each page stays under the budget");
        assert!(p0.output.contains("recall page 1/"), "pager marker present");
        // A later page returns different content (the tail is reachable).
        let p1 = t
            .execute(&serde_json::json!({"tool_call_id": "call_big", "page": 1}))
            .await
            .unwrap();
        assert_ne!(
            p0.output, p1.output,
            "page 2 is different content, not a re-truncation"
        );
        assert!(
            p0.output.contains("for the next page"),
            "non-final pages invite the next"
        );
        // The final page (a high index clamps to the last) must NOT invite a
        // non-existent next page (#2131 review item 3).
        let last = t
            .execute(&serde_json::json!({"tool_call_id": "call_big", "page": 9_999}))
            .await
            .unwrap();
        assert!(last.output.contains("last page"), "{}", last.output);
        assert!(
            !last.output.contains("for the next page"),
            "{}",
            last.output
        );
    }

    #[test]
    fn h03_m7_search_schema_is_bounded_and_absent_when_recovery_is_off() {
        let enabled =
            RecallTool::for_output_recovery(crate::output_recovery::OutputPolicy { enabled: true });
        let schema = enabled.input_schema();
        assert_eq!(
            schema["properties"]["query"]["maxLength"],
            crate::output_store::SEARCH_QUERY_CHARS
        );
        assert_eq!(
            schema["properties"]["max_matches"]["maximum"],
            crate::output_store::SEARCH_MATCHES
        );
        let declaration = format!("{}{}", enabled.description(), schema);
        assert!(octos_llm::context::estimate_tokens(&declaration) < 300);

        let disabled = disabled_tool(&[]);
        assert!(disabled.input_schema()["properties"].get("query").is_none());
    }

    #[tokio::test]
    async fn h03_m7_search_is_unavailable_when_output_recovery_is_off() {
        let result = disabled_tool(&[("call", "needle")])
            .execute(&serde_json::json!({
                "tool_call_id": "call",
                "query": "needle",
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.output, "recovery_tool_unavailable");
    }

    #[tokio::test]
    async fn h03_m7_null_query_and_orphan_match_limit_fail_as_search_requests() {
        use crate::model_read_receipts::ReadReceiptOwner;
        use crate::output_recovery::{OutputPolicy, OutputState};

        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(OutputState::new(
            OutputPolicy { enabled: true },
            ReadReceiptOwner::new("workspace", "task", "session", "branch").unwrap(),
        ));
        state.enable_store(dir.path()).unwrap();
        let mut context = ToolContext::zero();
        context.tool_id = "search-call".into();
        context.output_id = uuid::Uuid::new_v4().to_string();
        context.output_state = Some(state);
        let tool = RecallTool::for_output_recovery(OutputPolicy { enabled: true });

        for args in [
            serde_json::json!({"output_id": uuid::Uuid::new_v4(), "query": null}),
            serde_json::json!({"output_id": uuid::Uuid::new_v4(), "max_matches": 2}),
        ] {
            let result = tool.execute_with_context(&context, &args).await.unwrap();
            assert!(!result.success);
            assert_eq!(result.output, "invalid_search");
        }
    }
}
