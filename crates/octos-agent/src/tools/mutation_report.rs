use std::path::Path;

use serde_json::{Value, json};
use similar::TextDiff;

use super::mutation_guard;
use crate::file_state_cache::FileVersion;
use crate::format::FormatOutcome;

const DIFF_PREVIEW_BYTES: usize = 2048;

#[derive(Debug)]
pub(crate) enum FormattingRun {
    Disabled,
    SkippedWriteFence,
    Attempted(FormatOutcome),
}

impl FormattingRun {
    pub(crate) async fn execute(path: &Path, requested: bool, allowed: bool) -> Self {
        if !allowed {
            return Self::SkippedWriteFence;
        }
        if !requested {
            return Self::Disabled;
        }
        Self::Attempted(crate::format::format_file(path).await)
    }

    fn metadata(&self, formatter_changed: Option<bool>) -> Value {
        let (status, formatter, detail) = match self {
            Self::Disabled => ("disabled", None, None),
            Self::SkippedWriteFence => ("skipped_write_fence", None, None),
            Self::Attempted(FormatOutcome::NoFormatter) => ("not_configured", None, None),
            Self::Attempted(FormatOutcome::MissingBinary { formatter }) => {
                ("missing_binary", Some(*formatter), None)
            }
            Self::Attempted(FormatOutcome::Formatted { formatter }) => {
                ("formatted", Some(*formatter), None)
            }
            Self::Attempted(FormatOutcome::Failed { formatter, detail }) => {
                ("failed", Some(*formatter), Some(bounded_text(detail, 300)))
            }
            Self::Attempted(FormatOutcome::TimedOut { formatter }) => {
                ("timed_out", Some(*formatter), None)
            }
        };
        json!({
            "status": status,
            "formatter": formatter,
            "changed": formatter_changed,
            "detail": detail,
        })
    }

    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::SkippedWriteFence => "skipped_write_fence",
            Self::Attempted(FormatOutcome::NoFormatter) => "not_configured",
            Self::Attempted(FormatOutcome::MissingBinary { .. }) => "missing_binary",
            Self::Attempted(FormatOutcome::Formatted { .. }) => "formatted",
            Self::Attempted(FormatOutcome::Failed { .. }) => "failed",
            Self::Attempted(FormatOutcome::TimedOut { .. }) => "timed_out",
        }
    }
}

pub(crate) struct MutationReport {
    pub metadata: Value,
    pub final_label: String,
    pub formatter_label: &'static str,
    pub final_matches_written: Option<bool>,
}

impl MutationReport {
    pub(crate) async fn collect(
        tool: &str,
        path: &Path,
        workspace_root: &Path,
        display_path: &str,
        before: Option<&[u8]>,
        written: &[u8],
        formatting: &FormattingRun,
    ) -> Self {
        let (final_bytes, final_version, final_label) =
            if matches!(formatting, FormattingRun::SkippedWriteFence) {
                (
                    Some(written.to_vec()),
                    Some(version_json(written)),
                    short_bytes_version(written),
                )
            } else {
                match mutation_guard::observe_existing(path, workspace_root).await {
                    Ok((bytes, version)) => (
                        Some(bytes),
                        Some(file_version_json(&version)),
                        short_version(&version),
                    ),
                    Err(_) => (None, None, "unconfirmed".to_string()),
                }
            };
        let formatter_changed = match formatting {
            FormattingRun::Attempted(_) => final_bytes.as_deref().map(|bytes| bytes != written),
            FormattingRun::Disabled | FormattingRun::SkippedWriteFence => Some(false),
        };
        let final_matches_written = final_bytes
            .as_deref()
            .map(|final_bytes| final_bytes == written);
        let final_confirmed = match formatting {
            FormattingRun::SkippedWriteFence => true,
            FormattingRun::Disabled
            | FormattingRun::Attempted(FormatOutcome::NoFormatter)
            | FormattingRun::Attempted(FormatOutcome::MissingBinary { .. }) => {
                final_matches_written == Some(true)
            }
            FormattingRun::Attempted(
                FormatOutcome::Formatted { .. }
                | FormatOutcome::Failed { .. }
                | FormatOutcome::TimedOut { .. },
            ) => final_bytes.is_some(),
        };
        let baseline = before.unwrap_or_default();
        let tool_range = Some(changed_envelope(baseline, written));
        let final_range = final_confirmed
            .then(|| {
                final_bytes
                    .as_deref()
                    .map(|final_bytes| changed_envelope(baseline, final_bytes))
            })
            .flatten();
        let formatter_expanded_change = match (&tool_range, &final_range, formatter_changed) {
            (Some(tool_range), Some(final_range), Some(true)) => {
                changed_line_count(final_range) > changed_line_count(tool_range)
            }
            _ => false,
        };
        let final_label = if final_confirmed {
            final_label
        } else {
            "unconfirmed".to_string()
        };
        let op = if before.is_some() { "update" } else { "add" };
        let diff_preview = match (final_confirmed, before, final_bytes.as_deref()) {
            (true, Some(before), Some(final_bytes)) => {
                vec![json!({
                    "op": op,
                    "path": safe_path(display_path),
                    "diff": unified_preview(before, final_bytes, display_path),
                })]
            }
            (true, None, Some(final_bytes)) => vec![json!({
                "op": op,
                "path": safe_path(display_path),
                "diff": unified_preview(&[], final_bytes, display_path),
            })],
            _ => Vec::new(),
        };
        let metadata = json!({
            "outcome": "modified",
            "codex_tool": tool,
            "path": safe_path(display_path),
            "before_version": before.map(version_json),
            "write_version": version_json(written),
            "final_version": final_version,
            "final_state": if final_confirmed { "confirmed" } else { "unconfirmed" },
            "tool_changed_range": tool_range,
            "changed_range": final_range,
            "diff_preview": diff_preview,
            "modified_paths": [safe_path(display_path)],
            "formatter": formatting.metadata(formatter_changed),
            "formatter_expanded_change": formatter_expanded_change,
            "file_modified": true,
        });
        Self {
            metadata,
            final_label,
            formatter_label: formatting.status(),
            final_matches_written,
        }
    }
}

pub(crate) fn no_change_metadata(tool: &str, display_path: &str, bytes: &[u8]) -> Value {
    json!({
        "outcome": "no_change",
        "codex_tool": tool,
        "path": safe_path(display_path),
        "before_version": version_json(bytes),
        "write_version": null,
        "final_version": version_json(bytes),
        "final_state": "confirmed",
        "tool_changed_range": null,
        "changed_range": null,
        "diff_preview": [],
        "modified_paths": [],
        "formatter": {
            "status": "not_run",
            "formatter": null,
            "changed": false,
            "detail": null,
        },
        "formatter_expanded_change": false,
        "file_modified": false,
    })
}

pub(crate) fn insert(metadata: &mut Value, key: &'static str, value: Value) {
    if let Some(object) = metadata.as_object_mut() {
        object.insert(key.to_string(), value);
    }
}

pub(crate) fn short_bytes_version(bytes: &[u8]) -> String {
    let digest = FileVersion::sha256(bytes);
    let digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
    format!("sha256:{}...", &digest[..digest.len().min(12)])
}

pub(crate) fn safe_path(path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<file>")
            .to_string()
    } else {
        path.to_string_lossy().to_string()
    }
}

fn version_json(bytes: &[u8]) -> Value {
    json!({
        "content_sha256": FileVersion::sha256(bytes),
        "size": bytes.len(),
    })
}

fn file_version_json(version: &FileVersion) -> Value {
    json!({
        "content_sha256": version.content_sha256(),
        "size": version.size(),
    })
}

fn short_version(version: &FileVersion) -> String {
    let digest = version
        .content_sha256()
        .strip_prefix("sha256:")
        .unwrap_or(version.content_sha256());
    format!("sha256:{}...", &digest[..digest.len().min(12)])
}

fn changed_envelope(before: &[u8], after: &[u8]) -> Value {
    let before = String::from_utf8_lossy(before);
    let after = String::from_utf8_lossy(after);
    let before_lines: Vec<_> = before.split_inclusive('\n').collect();
    let after_lines: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = before_lines
        .iter()
        .zip(after_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before_lines[prefix..]
        .iter()
        .rev()
        .zip(after_lines[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    json!({
        "before": {
            "start": prefix + 1,
            "count": before_lines.len().saturating_sub(prefix + suffix),
        },
        "after": {
            "start": prefix + 1,
            "count": after_lines.len().saturating_sub(prefix + suffix),
        },
    })
}

fn changed_line_count(range: &Value) -> u64 {
    range["before"]["count"]
        .as_u64()
        .unwrap_or(0)
        .max(range["after"]["count"].as_u64().unwrap_or(0))
}

fn unified_preview(before: &[u8], after: &[u8], path: &str) -> String {
    let before = String::from_utf8_lossy(before);
    let after = String::from_utf8_lossy(after);
    let diff = TextDiff::from_lines(before.as_ref(), after.as_ref())
        .unified_diff()
        .context_radius(2)
        .header(
            &format!("a/{}", safe_path(path)),
            &format!("b/{}", safe_path(path)),
        )
        .to_string();
    bounded_text(&diff, DIFF_PREVIEW_BYTES)
}

fn bounded_text(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    const SUFFIX: &str = "\n... [diff preview truncated]";
    let mut end = budget.saturating_sub(SUFFIX.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_envelope_reports_before_and_after_line_counts() {
        let range = changed_envelope(b"same\nold\nend\n", b"same\nnew\nextra\nend\n");
        assert_eq!(range["before"]["start"], 2);
        assert_eq!(range["before"]["count"], 1);
        assert_eq!(range["after"]["start"], 2);
        assert_eq!(range["after"]["count"], 2);
    }

    #[test]
    fn unified_preview_is_bounded_and_utf8_safe() {
        let before = format!("{}\n", "旧".repeat(3000));
        let after = format!("{}\n", "新".repeat(3000));
        let preview = unified_preview(before.as_bytes(), after.as_bytes(), "unicode.txt");
        assert!(preview.len() <= DIFF_PREVIEW_BYTES);
        assert!(preview.ends_with("... [diff preview truncated]"));
    }

    #[test]
    fn no_change_metadata_has_no_modified_path_or_diff() {
        let metadata = no_change_metadata("edit_file", "same.txt", b"same\n");
        assert_eq!(metadata["outcome"], "no_change");
        assert_eq!(metadata["file_modified"], false);
        assert!(metadata["modified_paths"].as_array().unwrap().is_empty());
        assert!(metadata["diff_preview"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn final_report_never_claims_stale_written_bytes_after_disk_changes() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("file.rs");
        let before = b"fn value() -> u8 { 1 }\n";
        let written = b"fn value() -> u8 { 2 }\n";
        let current = b"fn value() -> u8 { 3 }\n";
        std::fs::write(&path, current).unwrap();

        let unformatted = MutationReport::collect(
            "edit_file",
            &path,
            workspace.path(),
            "file.rs",
            Some(before),
            written,
            &FormattingRun::Disabled,
        )
        .await;
        assert_eq!(unformatted.metadata["final_state"], "unconfirmed");
        assert!(unformatted.metadata["changed_range"].is_null());
        assert!(
            unformatted.metadata["diff_preview"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let formatted = MutationReport::collect(
            "edit_file",
            &path,
            workspace.path(),
            "file.rs",
            Some(before),
            written,
            &FormattingRun::Attempted(FormatOutcome::Formatted {
                formatter: "rustfmt",
            }),
        )
        .await;
        assert_eq!(formatted.metadata["final_state"], "confirmed");
        assert_eq!(
            formatted.metadata["final_version"]["content_sha256"],
            FileVersion::sha256(current)
        );
        assert!(
            formatted.metadata["diff_preview"][0]["diff"]
                .as_str()
                .unwrap()
                .contains("+fn value() -> u8 { 3 }")
        );
        assert!(
            !formatted.metadata["diff_preview"][0]["diff"]
                .as_str()
                .unwrap()
                .contains("+fn value() -> u8 { 2 }")
        );
    }
}
