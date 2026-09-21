//! Diff-based file editing tool using unified diff format.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

const FUZZY_RANGE: usize = 3;
const MAX_GLOBAL_SCAN_BYTES: usize = 10_000_000;
const MAX_GLOBAL_SCAN_LINES: usize = 100_000;
const MAX_GLOBAL_SCAN_COMPARISONS: usize = 1_000_000;
const MAX_DIFF_CANDIDATES: usize = 3;
const DIFF_CANDIDATE_EXCERPT_BYTES: usize = 512;
const DIFF_REJECTION_OUTPUT_BYTES: usize = 3072;

/// Tool for editing files via unified diff format with fuzzy matching.
pub struct DiffEditTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    file_access: FileAccessMode,
    local_edit_enabled: bool,
    strict_match_enabled: bool,
}

impl DiffEditTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            local_edit_enabled: false,
            strict_match_enabled: false,
        }
    }

    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }

    /// Enable typed no-op and final-change reporting.
    pub fn with_local_edit_enabled(mut self, enabled: bool) -> Self {
        self.local_edit_enabled = enabled;
        self
    }

    /// Require exact context lines for automatic writes.
    pub fn with_strict_match_enabled(mut self, enabled: bool) -> Self {
        self.strict_match_enabled = enabled;
        self
    }
}

#[derive(Debug, Deserialize)]
struct DiffEditInput {
    path: String,
    diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkMatch {
    hunk_index: usize,
    expected_line: usize,
    actual_line: usize,
    matcher: &'static str,
}

#[derive(Debug)]
struct AppliedDiff {
    content: String,
    hunk_matches: Vec<HunkMatch>,
}

#[derive(Debug, Clone)]
struct DiffCandidate {
    start: usize,
    end: usize,
    matcher: &'static str,
}

#[derive(Debug)]
struct DiffApplyError {
    code: &'static str,
    reason: &'static str,
    hunk_index: usize,
    expected_line: usize,
    matcher: &'static str,
    occurrence_count: usize,
    candidates: Vec<DiffCandidate>,
    searched_context_digest: String,
    full_file_scanned: bool,
}

fn typed_diff_rejection(
    path: &str,
    current_bytes: &[u8],
    content: &str,
    error: DiffApplyError,
) -> super::mutation_guard::MutationTransformError {
    let current_digest = crate::file_state_cache::FileVersion::sha256(current_bytes);
    let short_version = super::mutation_report::short_bytes_version(current_bytes);
    let shown_path =
        octos_core::truncated_utf8(&super::mutation_report::safe_path(path), 96, "...");
    let remedy = match error.code {
        "diff_context_ambiguous" => "retry_with_more_context",
        "invalid_edit_input" => "fix_diff_hunk",
        _ => "retry_with_current_context",
    };
    let indexed = index_content_lines(content);
    let mut output = format!(
        "[{}] path={} hunk={} expected_line={} count={} current={} remedy={}",
        error.code,
        shown_path,
        error.hunk_index,
        error.expected_line,
        error.occurrence_count,
        short_version,
        remedy,
    );
    let mut candidates = Vec::new();
    for candidate in error.candidates.iter().take(MAX_DIFF_CANDIDATES) {
        let excerpt = candidate_excerpt(content, &indexed, candidate);
        let summary = format!(
            "\nc{} suggestion=true lines={}-{} matcher={}",
            candidates.len() + 1,
            candidate.start + 1,
            candidate.end,
            candidate.matcher,
        );
        if output.len() + summary.len() > DIFF_REJECTION_OUTPUT_BYTES {
            break;
        }
        output.push_str(&summary);
        candidates.push((candidate, excerpt));
    }
    let mut candidate_metadata = Vec::with_capacity(candidates.len());
    for (index, (candidate, excerpt)) in candidates.into_iter().enumerate() {
        let body = format!("\ncandidate {} excerpt:\n{}", index + 1, excerpt);
        if output.len() + body.len() <= DIFF_REJECTION_OUTPUT_BYTES {
            output.push_str(&body);
        }
        candidate_metadata.push(json!({
            "line_range": {
                "start": candidate.start + 1,
                "end": candidate.end,
            },
            "matcher": candidate.matcher,
            "suggestion": true,
            "excerpt": excerpt,
        }));
    }
    octos_core::truncate_utf8(&mut output, DIFF_REJECTION_OUTPUT_BYTES, "");

    let output_document = crate::output_recovery::OutputDocument::unavailable(output.clone());
    super::mutation_guard::MutationTransformError::Rejected(
        super::mutation_guard::MutationRejection::new(ToolResult {
            output,
            output_document: Some(output_document),
            success: false,
            structured_metadata: Some(json!({
                "error_code": error.code,
                "path": super::mutation_report::safe_path(path),
                "current_version": {
                    "content_sha256": current_digest,
                    "size": current_bytes.len(),
                },
                "searched_context_digest": error.searched_context_digest,
                "reason": error.reason,
                "hunk_index": error.hunk_index,
                "expected_line": error.expected_line,
                "matcher": error.matcher,
                "occurrence_count": error.occurrence_count,
                "candidates": candidate_metadata,
                "full_file_scanned": error.full_file_scanned,
                "scan_limits": {
                    "bytes": MAX_GLOBAL_SCAN_BYTES,
                    "lines": MAX_GLOBAL_SCAN_LINES,
                    "comparisons": MAX_GLOBAL_SCAN_COMPARISONS,
                },
                "remedy": remedy,
                "file_modified": false,
            })),
            ..Default::default()
        }),
    )
}

fn hunk_matches_json(matches: &[HunkMatch]) -> serde_json::Value {
    matches
        .iter()
        .map(|matched| {
            json!({
                "hunk_index": matched.hunk_index,
                "expected_line": matched.expected_line,
                "actual_line": matched.actual_line,
                "matcher": matched.matcher,
            })
        })
        .collect()
}

fn hunk_positions_summary(matches: &[HunkMatch]) -> String {
    let mut summary = matches
        .iter()
        .map(|matched| format!("{}->{}", matched.expected_line, matched.actual_line))
        .collect::<Vec<_>>()
        .join(",");
    octos_core::truncate_utf8(&mut summary, 256, "...");
    summary
}

#[async_trait]
impl Tool for DiffEditTool {
    fn name(&self) -> &str {
        "diff_edit"
    }

    fn description(&self) -> &str {
        "Apply a unified diff to a file. Supports fuzzy matching (+-3 lines offset). \
         Use standard unified diff format with @@ -start,count +start,count @@ headers."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // diff_edit writes back to disk after applying the patch — the same
        // race hazard as write_file / edit_file. Serialize. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "diff": {
                    "type": "string",
                    "description": "Unified diff to apply (with @@ hunk headers)"
                }
            },
            "required": ["path", "diff"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // M8.4: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers still exercise the same
        // file-state-cache invalidation logic.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: DiffEditInput =
            serde_json::from_value(args.clone()).wrap_err("invalid diff_edit input")?;

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "diff_edit is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let path = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_write(scope, &input.path) {
                Ok(path) => path,
                Err(reason) => {
                    return Ok(ToolResult {
                        output: format!("{reason}: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
            None => match super::resolve_path_with_scope(
                &self.base_dir,
                &input.path,
                self.filesystem_scope,
            ) {
                Ok(path) => path,
                Err(_) => {
                    return Ok(ToolResult {
                        output: format!("Path outside working directory: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
        };
        let workspace_root = ctx
            .session_scope
            .as_ref()
            .map(|scope| scope.workspace().to_path_buf())
            .unwrap_or_else(|| self.base_dir.clone());

        let hunks = match parse_unified_diff(&input.diff) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Failed to parse diff: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if hunks.is_empty() {
            return Ok(ToolResult {
                output: "No hunks found in diff".to_string(),
                success: false,
                ..Default::default()
            });
        }
        let hunk_count = hunks.len();
        let local_edit_enabled =
            self.local_edit_enabled || super::registry::local_edit_execution_enabled();
        let strict_match_enabled = local_edit_enabled
            && (self.strict_match_enabled || super::registry::local_edit_strict_match_enabled());
        let display_path = input.path.clone();

        let guarded = super::mutation_guard::rewrite_existing(
            ctx,
            &workspace_root,
            &path,
            if local_edit_enabled {
                super::mutation_guard::ExpectedVersionPolicy::OptionalNoChange
            } else {
                super::mutation_guard::ExpectedVersionPolicy::Optional
            },
            None,
            None,
            move |bytes| -> Result<_, super::mutation_guard::MutationTransformError> {
                let content = std::str::from_utf8(bytes)
                    .map_err(|_| "File is not valid UTF-8 and cannot be edited".to_string())?;
                let applied = if local_edit_enabled {
                    match apply_hunks_local(content, &hunks, strict_match_enabled) {
                        Ok(applied) => applied,
                        Err(error) => {
                            return Err(typed_diff_rejection(
                                &display_path,
                                bytes,
                                content,
                                *error,
                            ));
                        }
                    }
                } else {
                    match apply_hunks(content, &hunks) {
                        Ok(content) => AppliedDiff {
                            content,
                            hunk_matches: Vec::new(),
                        },
                        Err(error) => {
                            return Err(format!("Failed to apply diff: {error}").into());
                        }
                    }
                };
                Ok((applied.content.as_bytes().to_vec(), applied))
            },
        )
        .await;
        let guarded = match guarded {
            Ok(rewrite) => rewrite,
            Err(error) => return Ok(error.into_tool_result(self.name(), &input.path)),
        };
        let AppliedDiff {
            content: new_content,
            hunk_matches,
        } = guarded.value;
        let full_file_fallback_count = hunk_matches
            .iter()
            .filter(|matched| matched.matcher == "full_file_line_exact")
            .count();
        if !guarded.changed {
            let mut metadata = super::mutation_report::no_change_metadata(
                self.name(),
                &input.path,
                &guarded.before,
            );
            super::mutation_report::insert(&mut metadata, "matcher", json!("diff_hunks"));
            super::mutation_report::insert(&mut metadata, "hunk_count", json!(hunk_count));
            super::mutation_report::insert(
                &mut metadata,
                "hunk_matches",
                hunk_matches_json(&hunk_matches),
            );
            super::mutation_report::insert(
                &mut metadata,
                "full_file_fallback_count",
                json!(full_file_fallback_count),
            );
            return Ok(ToolResult {
                output: format!(
                    "[no_change] path={} matcher=diff_hunks hunks={} positions={} current={}",
                    super::mutation_report::safe_path(&input.path),
                    hunk_count,
                    hunk_positions_summary(&hunk_matches),
                    super::mutation_report::short_bytes_version(&guarded.before),
                ),
                success: true,
                structured_metadata: Some(metadata),
                ..Default::default()
            });
        }

        // #1774: opt-in post-edit formatting. Runs BEFORE cache invalidation
        // and the git snapshot so both observe the final on-disk content.
        // Best-effort by contract — a formatter failure never fails the edit.
        let formatting = if local_edit_enabled {
            Some(
                super::mutation_report::FormattingRun::execute(&path, ctx.format_after_edit, true)
                    .await,
            )
        } else {
            None
        };
        let format_note = if !local_edit_enabled && ctx.format_after_edit {
            crate::format::post_edit_format_note(&path, &new_content).await
        } else {
            None
        };
        let mut report = if let Some(formatting) = formatting.as_ref() {
            Some(
                super::mutation_report::MutationReport::collect(
                    self.name(),
                    &path,
                    &workspace_root,
                    &input.path,
                    Some(&guarded.before),
                    &guarded.written,
                    formatting,
                )
                .await,
            )
        } else {
            None
        };
        if let Some(report) = report.as_mut() {
            super::mutation_report::insert(&mut report.metadata, "matcher", json!("diff_hunks"));
            super::mutation_report::insert(&mut report.metadata, "hunk_count", json!(hunk_count));
            super::mutation_report::insert(
                &mut report.metadata,
                "hunk_matches",
                hunk_matches_json(&hunk_matches),
            );
            super::mutation_report::insert(
                &mut report.metadata,
                "full_file_fallback_count",
                json!(full_file_fallback_count),
            );
        }

        // Invalidate every recorded workspace-owned version for this path.
        super::mutation_guard::complete_mutation(ctx, &workspace_root, &path);

        if let Err(error) =
            crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "diff_edit")
        {
            warn!(
                path = %input.path,
                error = %error,
                "workspace git snapshot failed after diff_edit"
            );
        }

        let output = if let Some(report) = report.as_ref() {
            format!(
                "Applied {} hunk(s) to {}: positions={}, final={}, formatter={}",
                hunk_count,
                super::mutation_report::safe_path(&input.path),
                hunk_positions_summary(&hunk_matches),
                report.final_label,
                report.formatter_label,
            )
        } else {
            format!(
                "Applied {} hunk(s) to {}{}",
                hunk_count,
                input.path,
                format_note.unwrap_or_default()
            )
        };
        Ok(ToolResult {
            output,
            success: true,
            file_modified: Some(path),
            structured_metadata: report.map(|report| report.metadata),
            ..Default::default()
        })
    }
}

// --- Diff parsing ---

struct Hunk {
    old_start: usize,
    lines: Vec<DiffLine>,
}

/// One classified line of a unified-diff hunk body. Shared with the
/// `apply_patch` tool so both editors parse and apply hunk bodies with the
/// same semantics (#1773).
#[derive(Debug)]
pub(crate) enum DiffLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Context + Remove lines of a hunk body — the block that must match the
/// current file content before the hunk may be applied.
pub(crate) fn pattern_lines(lines: &[DiffLine]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|l| match l {
            DiffLine::Context(s) | DiffLine::Remove(s) => Some(s.as_str()),
            DiffLine::Add(_) => None,
        })
        .collect()
}

/// Context + Add lines of a hunk body — the block that replaces the matched
/// region.
pub(crate) fn replacement_lines(lines: &[DiffLine]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|l| match l {
            DiffLine::Context(s) | DiffLine::Add(s) => Some(s.clone()),
            DiffLine::Remove(_) => None,
        })
        .collect()
}

fn parse_unified_diff(diff: &str) -> Result<Vec<Hunk>> {
    let mut hunks = Vec::new();
    let mut current_hunk: Option<Hunk> = None;

    for line in diff.lines() {
        if line.starts_with("@@") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }
            let old_start = parse_hunk_header(line)?;
            current_hunk = Some(Hunk {
                old_start,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current_hunk.as_mut() {
            if let Some(rest) = line.strip_prefix('-') {
                hunk.lines.push(DiffLine::Remove(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix('+') {
                hunk.lines.push(DiffLine::Add(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix(' ') {
                hunk.lines.push(DiffLine::Context(rest.to_string()));
            } else if !line.starts_with("---")
                && !line.starts_with("+++")
                && !line.starts_with("diff ")
                && !line.starts_with("index ")
            {
                // Treat unmarked lines as context
                hunk.lines.push(DiffLine::Context(line.to_string()));
            }
        }
        // Lines before any hunk header (e.g., --- a/file, +++ b/file) are ignored
    }

    if let Some(h) = current_hunk {
        hunks.push(h);
    }

    Ok(hunks)
}

fn parse_hunk_header(line: &str) -> Result<usize> {
    // @@ -old_start[,old_count] +new_start[,new_count] @@
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        eyre::bail!("invalid hunk header: {}", line);
    }
    let old_part = parts[1]; // e.g., "-10,5"
    let start_str = old_part
        .trim_start_matches('-')
        .split(',')
        .next()
        .unwrap_or("1");
    let start: usize = start_str
        .parse()
        .wrap_err_with(|| format!("invalid line number in hunk header: {line}"))?;
    Ok(start)
}

// --- Hunk application with fuzzy matching ---

fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String> {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut sorted_hunks: Vec<(usize, &Hunk)> = hunks.iter().enumerate().collect();
    sorted_hunks.sort_by_key(|entry| std::cmp::Reverse(entry.1.old_start));

    for window in sorted_hunks.windows(2) {
        let (_, later_hunk) = window[0];
        let (_, earlier_hunk) = window[1];
        let earlier_end = earlier_hunk.old_start + pattern_lines(&earlier_hunk.lines).len();
        if earlier_end > later_hunk.old_start {
            eyre::bail!(
                "overlapping hunks at lines {} and {}",
                earlier_hunk.old_start,
                later_hunk.old_start
            );
        }
    }

    for (index, hunk) in sorted_hunks {
        let pattern = pattern_lines(&hunk.lines);
        if pattern.is_empty() {
            eyre::bail!("hunk {} has no context or remove lines", index + 1);
        }
        let target = hunk.old_start.saturating_sub(1);
        let position = find_match(&lines, &pattern, target)?;
        let replacement = replacement_lines(&hunk.lines);
        let end = (position + pattern.len()).min(lines.len());
        lines.splice(position..end, replacement);
    }

    let mut result = lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

fn apply_hunks_local(
    content: &str,
    hunks: &[Hunk],
    strict_match: bool,
) -> std::result::Result<AppliedDiff, Box<DiffApplyError>> {
    let indexed = index_content_lines(content);
    let lines = indexed
        .iter()
        .map(|line| line.text(content).to_string())
        .collect::<Vec<_>>();

    // Validate declared ranges before doing any matching.
    let mut sorted_hunks: Vec<(usize, &Hunk)> = hunks.iter().enumerate().collect();
    sorted_hunks.sort_by_key(|entry| std::cmp::Reverse(entry.1.old_start));
    for window in sorted_hunks.windows(2) {
        let (_, later_hunk) = window[0];
        let (earlier_index, earlier_hunk) = window[1];
        let earlier_end = earlier_hunk
            .old_start
            .saturating_add(pattern_lines(&earlier_hunk.lines).len());
        if earlier_end > later_hunk.old_start {
            return Err(invalid_hunk_error(
                "overlapping_declared_hunks",
                earlier_index,
                earlier_hunk,
            ));
        }
    }

    // Locate every hunk against the same immutable source. Applying a later
    // hunk must not create or remove candidates for an earlier hunk.
    let mut located = Vec::with_capacity(hunks.len());
    for (index, hunk) in hunks.iter().enumerate() {
        let pattern = pattern_lines(&hunk.lines);
        if pattern.is_empty() {
            return Err(invalid_hunk_error("empty_context", index, hunk));
        }
        let target = hunk.old_start.saturating_sub(1); // 1-indexed to 0-indexed
        let matched = locate_hunk(content, &lines, &pattern, target, index, hunk, strict_match)?;
        located.push((index, hunk, matched));
    }

    let mut by_position = located.iter().collect::<Vec<_>>();
    by_position.sort_by_key(|(_, _, matched)| matched.actual_line);
    for window in by_position.windows(2) {
        let (_, left_hunk, left_match) = window[0];
        let (right_index, right_hunk, right_match) = window[1];
        let left_end = left_match.actual_line - 1 + pattern_lines(&left_hunk.lines).len();
        if left_end > right_match.actual_line - 1 {
            return Err(Box::new(DiffApplyError {
                code: "invalid_edit_input",
                reason: "overlapping_actual_matches",
                hunk_index: *right_index + 1,
                expected_line: right_hunk.old_start,
                matcher: right_match.matcher,
                occurrence_count: 0,
                candidates: vec![
                    DiffCandidate {
                        start: left_match.actual_line - 1,
                        end: left_end,
                        matcher: left_match.matcher,
                    },
                    DiffCandidate {
                        start: right_match.actual_line - 1,
                        end: right_match.actual_line - 1 + pattern_lines(&right_hunk.lines).len(),
                        matcher: right_match.matcher,
                    },
                ],
                searched_context_digest: pattern_digest(&pattern_lines(&right_hunk.lines)),
                full_file_scanned: located
                    .iter()
                    .any(|(_, _, matched)| matched.matcher == "full_file_line_exact"),
            }));
        }
    }

    let mut result = content.to_string();
    let mut reverse = located.iter().collect::<Vec<_>>();
    reverse.sort_by_key(|(_, _, matched)| std::cmp::Reverse(matched.actual_line));
    for (_, hunk, matched) in reverse {
        let position = matched.actual_line - 1;
        let pattern_len = pattern_lines(&hunk.lines).len();
        let start = indexed[position].start;
        let end = indexed[position + pattern_len - 1].end;
        let replacement = render_hunk_replacement(content, &indexed, position, hunk);
        result.replace_range(start..end, &replacement);
    }

    Ok(AppliedDiff {
        content: result,
        hunk_matches: located.into_iter().map(|(_, _, matched)| matched).collect(),
    })
}

fn locate_hunk(
    content: &str,
    lines: &[String],
    pattern: &[&str],
    target: usize,
    index: usize,
    hunk: &Hunk,
    strict_match: bool,
) -> std::result::Result<HunkMatch, Box<DiffApplyError>> {
    let nearby = if strict_match {
        nearby_exact_matches(lines, pattern, target)
    } else {
        nearby_matches(lines, pattern, target)
    };
    match nearby.as_slice() {
        [position] => {
            return Ok(HunkMatch {
                hunk_index: index + 1,
                expected_line: hunk.old_start,
                actual_line: *position + 1,
                matcher: nearby_matcher(lines, pattern, target, *position),
            });
        }
        [] => {}
        _ => {
            let candidates = nearby
                .iter()
                .take(MAX_DIFF_CANDIDATES)
                .map(|position| DiffCandidate {
                    start: *position,
                    end: *position + pattern.len(),
                    matcher: nearby_matcher(lines, pattern, target, *position),
                })
                .collect();
            return Err(Box::new(DiffApplyError {
                code: "diff_context_ambiguous",
                reason: "ambiguous_nearby_matches",
                hunk_index: index + 1,
                expected_line: hunk.old_start,
                matcher: "nearby_window",
                occurrence_count: nearby.len(),
                candidates,
                searched_context_digest: pattern_digest(pattern),
                full_file_scanned: false,
            }));
        }
    }

    if content.len() > MAX_GLOBAL_SCAN_BYTES
        || lines.len() > MAX_GLOBAL_SCAN_LINES
        || global_scan_work(lines.len(), pattern.len()) > MAX_GLOBAL_SCAN_COMPARISONS
    {
        return Err(Box::new(DiffApplyError {
            code: "diff_context_no_match",
            reason: "global_scan_limit",
            hunk_index: index + 1,
            expected_line: hunk.old_start,
            matcher: "full_file_line_exact",
            occurrence_count: 0,
            candidates: expected_location_candidate(lines.len(), target, pattern.len()),
            searched_context_digest: pattern_digest(pattern),
            full_file_scanned: false,
        }));
    }

    let matches = all_exact_matches(lines, pattern);
    match matches.as_slice() {
        [position] => Ok(HunkMatch {
            hunk_index: index + 1,
            expected_line: hunk.old_start,
            actual_line: *position + 1,
            matcher: "full_file_line_exact",
        }),
        [] => {
            let fuzzy = if strict_match {
                nearby_matches(lines, pattern, target)
            } else {
                Vec::new()
            };
            if !fuzzy.is_empty() {
                let candidates = fuzzy
                    .iter()
                    .take(MAX_DIFF_CANDIDATES)
                    .map(|position| DiffCandidate {
                        start: *position,
                        end: *position + pattern.len(),
                        matcher: nearby_matcher(lines, pattern, target, *position),
                    })
                    .collect();
                let matcher = nearby_matcher(lines, pattern, target, fuzzy[0]);
                return Err(Box::new(DiffApplyError {
                    code: "diff_context_no_match",
                    reason: "strict_match_requires_exact",
                    hunk_index: index + 1,
                    expected_line: hunk.old_start,
                    matcher,
                    occurrence_count: fuzzy.len(),
                    candidates,
                    searched_context_digest: pattern_digest(pattern),
                    full_file_scanned: true,
                }));
            }
            Err(Box::new(DiffApplyError {
                code: "diff_context_no_match",
                reason: "no_full_file_match",
                hunk_index: index + 1,
                expected_line: hunk.old_start,
                matcher: "full_file_line_exact",
                occurrence_count: 0,
                candidates: expected_location_candidate(lines.len(), target, pattern.len()),
                searched_context_digest: pattern_digest(pattern),
                full_file_scanned: true,
            }))
        }
        _ => Err(Box::new(DiffApplyError {
            code: "diff_context_ambiguous",
            reason: "ambiguous_full_file_matches",
            hunk_index: index + 1,
            expected_line: hunk.old_start,
            matcher: "full_file_line_exact",
            occurrence_count: matches.len(),
            candidates: matches
                .iter()
                .take(MAX_DIFF_CANDIDATES)
                .map(|position| DiffCandidate {
                    start: *position,
                    end: *position + pattern.len(),
                    matcher: "full_file_line_exact",
                })
                .collect(),
            searched_context_digest: pattern_digest(pattern),
            full_file_scanned: true,
        })),
    }
}

fn invalid_hunk_error(reason: &'static str, index: usize, hunk: &Hunk) -> Box<DiffApplyError> {
    Box::new(DiffApplyError {
        code: "invalid_edit_input",
        reason,
        hunk_index: index + 1,
        expected_line: hunk.old_start,
        matcher: "none",
        occurrence_count: 0,
        candidates: Vec::new(),
        searched_context_digest: pattern_digest(&pattern_lines(&hunk.lines)),
        full_file_scanned: false,
    })
}

fn nearby_matches(lines: &[String], pattern: &[&str], target: usize) -> Vec<usize> {
    let start = target.saturating_sub(FUZZY_RANGE);
    let end = target
        .saturating_add(FUZZY_RANGE)
        .min(lines.len().saturating_sub(pattern.len()));
    if pattern.is_empty() || lines.len() < pattern.len() || start > end {
        return Vec::new();
    }
    (start..=end)
        .filter(|position| matches_at(lines, pattern, *position))
        .collect()
}

fn nearby_exact_matches(lines: &[String], pattern: &[&str], target: usize) -> Vec<usize> {
    let start = target.saturating_sub(FUZZY_RANGE);
    let end = target
        .saturating_add(FUZZY_RANGE)
        .min(lines.len().saturating_sub(pattern.len()));
    if pattern.is_empty() || lines.len() < pattern.len() || start > end {
        return Vec::new();
    }
    (start..=end)
        .filter(|position| matches_at_exact(lines, pattern, *position))
        .collect()
}

fn find_match(lines: &[String], pattern: &[&str], target: usize) -> Result<usize> {
    let matches = nearby_matches(lines, pattern, target);
    match matches.as_slice() {
        [position] => Ok(*position),
        [] => eyre::bail!(
            "could not find matching context at line {} (+-{} lines). Expected: {:?}",
            target + 1,
            FUZZY_RANGE,
            &pattern[..pattern.len().min(3)]
        ),
        _ => eyre::bail!(
            "matching context is ambiguous: found {} locations near line {}",
            matches.len(),
            target + 1
        ),
    }
}

fn nearby_matcher(
    lines: &[String],
    pattern: &[&str],
    target: usize,
    position: usize,
) -> &'static str {
    match (
        position == target,
        matches_at_exact(lines, pattern, position),
    ) {
        (true, true) => "target_line_exact",
        (true, false) => "target_trailing_whitespace",
        (false, true) => "nearby_line_exact",
        (false, false) => "nearby_trailing_whitespace",
    }
}

fn all_exact_matches(lines: &[String], pattern: &[&str]) -> Vec<usize> {
    if pattern.is_empty() || lines.len() < pattern.len() {
        return Vec::new();
    }
    (0..=lines.len() - pattern.len())
        .filter(|position| matches_at_exact(lines, pattern, *position))
        .collect()
}

fn global_scan_work(line_count: usize, pattern_len: usize) -> usize {
    line_count
        .saturating_sub(pattern_len)
        .saturating_add(1)
        .saturating_mul(pattern_len)
}

fn matches_at_exact(lines: &[String], pattern: &[&str], start: usize) -> bool {
    if start + pattern.len() > lines.len() {
        return false;
    }
    pattern
        .iter()
        .enumerate()
        .all(|(index, expected)| lines[start + index] == *expected)
}

fn pattern_digest(pattern: &[&str]) -> String {
    crate::file_state_cache::FileVersion::sha256(pattern.join("\n").as_bytes())
}

fn expected_location_candidate(
    line_count: usize,
    target: usize,
    pattern_len: usize,
) -> Vec<DiffCandidate> {
    if line_count == 0 {
        return Vec::new();
    }
    let focus = target.min(line_count - 1);
    let start = focus.saturating_sub(2);
    let end = focus
        .saturating_add(pattern_len.max(1))
        .saturating_add(2)
        .min(line_count)
        .max(start + 1);
    vec![DiffCandidate {
        start,
        end,
        matcher: "expected_location",
    }]
}

#[derive(Debug, Clone, Copy)]
struct ContentLine {
    start: usize,
    text_end: usize,
    end: usize,
}

impl ContentLine {
    fn text<'a>(&self, content: &'a str) -> &'a str {
        &content[self.start..self.text_end]
    }

    fn ending<'a>(&self, content: &'a str) -> &'a str {
        &content[self.text_end..self.end]
    }
}

fn index_content_lines(content: &str) -> Vec<ContentLine> {
    let bytes = content.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let text_end = if index > start && bytes[index - 1] == b'\r' {
            index - 1
        } else {
            index
        };
        lines.push(ContentLine {
            start,
            text_end,
            end: index + 1,
        });
        start = index + 1;
    }
    if start < content.len() {
        lines.push(ContentLine {
            start,
            text_end: content.len(),
            end: content.len(),
        });
    }
    lines
}

fn preferred_line_ending<'a>(
    content: &'a str,
    lines: &[ContentLine],
    position: usize,
    pattern_len: usize,
) -> &'a str {
    lines[position..position + pattern_len]
        .iter()
        .chain(lines[..position].iter().rev())
        .chain(lines[position + pattern_len..].iter())
        .map(|line| line.ending(content))
        .find(|ending| !ending.is_empty())
        .unwrap_or("\n")
}

fn render_hunk_replacement(
    content: &str,
    lines: &[ContentLine],
    position: usize,
    hunk: &Hunk,
) -> String {
    let pattern_len = pattern_lines(&hunk.lines).len();
    let preferred_ending = preferred_line_ending(content, lines, position, pattern_len);
    let matched_has_final_ending = !lines[position + pattern_len - 1].ending(content).is_empty();
    let mut source = position;
    let mut output_lines = Vec::new();
    for line in &hunk.lines {
        match line {
            DiffLine::Context(_) => {
                output_lines.push((
                    lines[source].text(content).to_string(),
                    lines[source].ending(content),
                ));
                source += 1;
            }
            DiffLine::Remove(_) => source += 1,
            DiffLine::Add(text) => output_lines.push((text.clone(), "")),
        }
    }

    let mut replacement = String::new();
    let output_count = output_lines.len();
    for (index, (text, original_ending)) in output_lines.into_iter().enumerate() {
        replacement.push_str(&text);
        if index + 1 < output_count || matched_has_final_ending {
            replacement.push_str(if original_ending.is_empty() {
                preferred_ending
            } else {
                original_ending
            });
        }
    }
    replacement
}

fn candidate_excerpt(content: &str, lines: &[ContentLine], candidate: &DiffCandidate) -> String {
    if lines.is_empty() || candidate.start >= candidate.end || candidate.start >= lines.len() {
        return String::new();
    }
    let start = candidate.start.saturating_sub(2);
    let end = (candidate.end + 2).min(lines.len());
    let excerpt = &content[lines[start].start..lines[end - 1].end];
    let sanitized = crate::sanitize::sanitize_tool_output(excerpt);
    octos_core::truncated_utf8(&sanitized, DIFF_CANDIDATE_EXCERPT_BYTES, "...")
}

/// Whether `pattern` matches `lines` starting at `start`, comparing with
/// trailing whitespace ignored. Shared with the `apply_patch` tool's
/// sequential hunk matcher (#1773).
pub(crate) fn matches_at(lines: &[String], pattern: &[&str], start: usize) -> bool {
    if start + pattern.len() > lines.len() {
        return false;
    }
    pattern
        .iter()
        .enumerate()
        .all(|(i, p)| lines[start + i].trim_end() == p.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_edit_tool_is_exclusive() {
        // diff_edit writes back after applying the patch — same race hazard
        // as write_file; must serialize (M8.8).
        let dir = tempfile::tempdir().unwrap();
        let tool = DiffEditTool::new(dir.path());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[tokio::test]
    async fn local_edit_identical_diff_is_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        std::fs::write(&path, "same\n").unwrap();
        let before = std::fs::metadata(&path).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "same.txt",
                "diff": "@@ -1 +1 @@\n-same\n+same\n"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.file_modified.is_none());
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["outcome"], "no_change");
        assert_eq!(metadata["hunk_count"], 1);
        assert_eq!(metadata["file_modified"], false);
        assert_eq!(
            before.modified().unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap()
        );
    }

    #[tokio::test]
    async fn local_edit_diff_success_reports_hunks_and_final_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "old\n").unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "file.txt",
                "diff": "@@ -1 +1 @@\n-old\n+new\n"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["outcome"], "modified");
        assert_eq!(metadata["matcher"], "diff_hunks");
        assert_eq!(metadata["hunk_count"], 1);
        assert_eq!(metadata["final_state"], "confirmed");
        assert_eq!(metadata["changed_range"]["before"]["start"], 1);
        assert_eq!(metadata["changed_range"]["after"]["count"], 1);
        assert!(
            metadata["diff_preview"][0]["diff"]
                .as_str()
                .unwrap()
                .contains("+new")
        );
    }

    #[tokio::test]
    async fn should_invalidate_file_version_after_diff_edit() {
        use std::sync::Arc;

        use crate::file_state_cache::{FileMetadataHint, FileStateCache, FileTarget, FileVersion};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "old\n").unwrap();
        let target = FileTarget::for_local_workspace(dir.path(), &path).unwrap();
        let ledger = Arc::new(FileStateCache::new());
        ledger.record(FileVersion::from_bytes(
            target.clone(),
            None,
            b"old\n",
            FileMetadataHint::from_metadata(&std::fs::metadata(&path).unwrap()),
        ));
        let mut context = ToolContext::zero();
        context.file_state_cache = Some(ledger.clone());

        let result = DiffEditTool::new(dir.path())
            .execute_with_context(
                &context,
                &serde_json::json!({
                    "path": "file.txt",
                    "diff": "@@ -1 +1 @@\n-old\n+new\n"
                }),
            )
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(ledger.get(&target).is_none());
    }

    #[test]
    fn test_parse_simple_diff() {
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+line2_modified\n line3";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].lines.len(), 4); // context + remove + add + context
    }

    #[test]
    fn test_apply_simple_replacement() {
        let content = "line1\nline2\nline3\n";
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+line2_new\n line3\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "line1\nline2_new\nline3\n");
    }

    #[test]
    fn test_apply_insertion() {
        let content = "a\nb\n";
        let diff = "@@ -1,2 +1,3 @@\n a\n+inserted\n b\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "a\ninserted\nb\n");
    }

    #[test]
    fn test_apply_deletion() {
        let content = "a\ndelete_me\nb\n";
        let diff = "@@ -1,3 +1,2 @@\n a\n-delete_me\n b\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "a\nb\n");
    }

    #[test]
    fn test_fuzzy_match_offset() {
        // Content has an extra line at the top, so line numbers are off by 1
        let content = "extra\nline1\nline2\nline3\n";
        // Diff says line 1, but actual match is at line 2
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+line2_fuzzy\n line3\n";
        let hunks = parse_unified_diff(diff).unwrap();
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "extra\nline1\nline2_fuzzy\nline3\n");
    }

    #[tokio::test]
    async fn strict_match_rejects_trailing_whitespace_as_a_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        let original = "alpha  \nbeta\n";
        std::fs::write(dir.path().join("baseline.md"), original).unwrap();
        std::fs::write(dir.path().join("strict.md"), original).unwrap();
        let args = |path: &str| {
            serde_json::json!({
                "path": path,
                "diff": "@@ -1,2 +1,2 @@\n-alpha\n+changed\n beta\n",
            })
        };

        let baseline = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&args("baseline.md"))
            .await
            .unwrap();
        assert!(baseline.success, "{}", baseline.output);
        assert_eq!(
            baseline.structured_metadata.as_ref().unwrap()["hunk_matches"][0]["matcher"],
            "target_trailing_whitespace"
        );

        let strict = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .with_strict_match_enabled(true)
            .execute(&args("strict.md"))
            .await
            .unwrap();
        assert!(!strict.success, "{}", strict.output);
        assert!(strict.file_modified.is_none());
        let metadata = strict.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "diff_context_no_match");
        assert_eq!(metadata["reason"], "strict_match_requires_exact");
        assert_eq!(metadata["matcher"], "target_trailing_whitespace");
        assert_eq!(
            metadata["candidates"][0]["matcher"],
            "target_trailing_whitespace"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("strict.md")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn strict_match_accepts_exact_context_with_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strict.txt");
        std::fs::write(&path, b"alpha\r\nbeta\r\n").unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .with_strict_match_enabled(true)
            .execute(&serde_json::json!({
                "path": "strict.txt",
                "diff": "@@ -10,2 +10,2 @@\n alpha\n-beta\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\r\nchanged\r\n");
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["hunk_matches"][0]["matcher"],
            "full_file_line_exact"
        );
    }

    #[tokio::test]
    async fn strict_match_rejects_ambiguous_full_file_exact_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous.txt");
        let original = "p1\np2\np3\np4\ntarget\np6\np7\np8\np9\ntarget\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .with_strict_match_enabled(true)
            .execute(&serde_json::json!({
                "path": "ambiguous.txt",
                "diff": "@@ -20 +20 @@\n-target\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(!result.success, "{}", result.output);
        assert!(result.file_modified.is_none());
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "diff_context_ambiguous");
        assert_eq!(metadata["matcher"], "full_file_line_exact");
        assert_eq!(metadata["occurrence_count"], 2);
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn strict_match_rejects_all_hunks_when_one_is_only_a_fuzzy_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        let original = "one\nmiddle\ntwo  \n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .with_strict_match_enabled(true)
            .execute(&serde_json::json!({
                "path": "multi.txt",
                "diff": concat!(
                    "@@ -1 +1 @@\n",
                    "-one\n",
                    "+ONE\n",
                    "@@ -3 +3 @@\n",
                    "-two\n",
                    "+TWO\n"
                ),
            }))
            .await
            .unwrap();

        assert!(!result.success, "{}", result.output);
        assert!(result.file_modified.is_none());
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "diff_context_no_match");
        assert_eq!(metadata["hunk_index"], 2);
        assert_eq!(metadata["reason"], "strict_match_requires_exact");
        assert_eq!(
            metadata["candidates"][0]["matcher"],
            "target_trailing_whitespace"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_finds_a_unique_hunk_four_lines_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drift.txt");
        std::fs::write(&path, "p1\np2\np3\np4\ntarget\nend\n").unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "drift.txt",
                "diff": "@@ -1 +1 @@\n-target\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "p1\np2\np3\np4\nchanged\nend\n"
        );
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["hunk_matches"][0]["expected_line"], 1);
        assert_eq!(metadata["hunk_matches"][0]["actual_line"], 5);
        assert_eq!(
            metadata["hunk_matches"][0]["matcher"],
            "full_file_line_exact"
        );
    }

    #[tokio::test]
    async fn local_edit_finds_a_unique_hunk_far_from_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("far.txt");
        let mut original = (1..=80)
            .map(|line| format!("padding-{line}\n"))
            .collect::<String>();
        original.push_str("unique target\nend\n");
        std::fs::write(&path, &original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "far.txt",
                "diff": "@@ -1 +1 @@\n-unique target\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("changed\nend\n")
        );
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["hunk_matches"][0]["actual_line"],
            81
        );
    }

    #[tokio::test]
    async fn local_edit_rejects_ambiguous_full_file_matches_with_bounded_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duplicate.txt");
        let original = concat!(
            "p1\np2\np3\np4\ntarget\n",
            "p6\np7\np8\np9\ntarget\n",
            "p11\np12\np13\np14\ntarget\n",
            "p16\np17\np18\np19\ntarget\n",
        );
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "duplicate.txt",
                "diff": "@@ -1 +1 @@\n-target\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.starts_with("[diff_context_ambiguous]"));
        assert!(result.output.len() <= DIFF_REJECTION_OUTPUT_BYTES);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "diff_context_ambiguous");
        assert_eq!(metadata["occurrence_count"], 4);
        assert_eq!(metadata["candidates"].as_array().unwrap().len(), 3);
        assert_eq!(metadata["candidates"][0]["line_range"]["start"], 5);
        assert_eq!(metadata["candidates"][1]["line_range"]["start"], 10);
        assert_eq!(metadata["candidates"][2]["line_range"]["start"], 15);
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_no_match_returns_current_target_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        let original = "one\ntwo\nthree\nfour\nfive\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "missing.txt",
                "diff": "@@ -3 +3 @@\n-absent\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.starts_with("[diff_context_no_match]"));
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "diff_context_no_match");
        assert_eq!(metadata["reason"], "no_full_file_match");
        assert_eq!(metadata["candidates"][0]["matcher"], "expected_location");
        assert!(
            metadata["candidates"][0]["excerpt"]
                .as_str()
                .unwrap()
                .contains("three")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_full_file_fallback_does_not_ignore_markdown_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("README.md");
        let original = "p1\np2\np3\np4\nkeep break  \nend\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "README.md",
                "diff": "@@ -1 +1 @@\n-keep break\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            "diff_context_no_match"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_full_file_fallback_preserves_crlf_and_eof_policy() {
        let dir = tempfile::tempdir().unwrap();
        let crlf_path = dir.path().join("windows.txt");
        std::fs::write(&crlf_path, b"p1\r\np2\r\np3\r\np4\r\ntarget\r\nend\r\n").unwrap();
        let no_eof_path = dir.path().join("no-eof.txt");
        std::fs::write(&no_eof_path, "p1\np2\np3\np4\ntarget").unwrap();
        let tool = DiffEditTool::new(dir.path()).with_local_edit_enabled(true);

        let crlf = tool
            .execute(&serde_json::json!({
                "path": "windows.txt",
                "diff": "@@ -1 +1 @@\n-target\n+changed\n",
            }))
            .await
            .unwrap();
        let no_eof = tool
            .execute(&serde_json::json!({
                "path": "no-eof.txt",
                "diff": "@@ -1 +1 @@\n-target\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(crlf.success, "{}", crlf.output);
        assert!(no_eof.success, "{}", no_eof.output);
        assert_eq!(
            std::fs::read(crlf_path).unwrap(),
            b"p1\r\np2\r\np3\r\np4\r\nchanged\r\nend\r\n"
        );
        assert_eq!(
            std::fs::read_to_string(no_eof_path).unwrap(),
            "p1\np2\np3\np4\nchanged"
        );
    }

    #[tokio::test]
    async fn local_edit_pre_locates_all_hunks_before_reverse_application() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "p1\np2\np3\np4\nearly\np6\np7\np8\np9\nlate\n").unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "multi.txt",
                "diff": concat!(
                    "@@ -1 +1 @@\n",
                    "-early\n",
                    "+EARLY\n",
                    "@@ -10 +10 @@\n",
                    "-late\n",
                    "+early\n",
                ),
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "p1\np2\np3\np4\nEARLY\np6\np7\np8\np9\nearly\n"
        );
        let matches = result.structured_metadata.as_ref().unwrap()["hunk_matches"]
            .as_array()
            .unwrap();
        assert_eq!(matches[0]["actual_line"], 5);
        assert_eq!(matches[1]["actual_line"], 10);
    }

    #[tokio::test]
    async fn local_edit_rejects_hunks_that_resolve_to_the_same_actual_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actual-overlap.txt");
        let original = "p1\np2\np3\np4\nsame\np6\np7\np8\np9\np10\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "actual-overlap.txt",
                "diff": concat!(
                    "@@ -1 +1 @@\n",
                    "-same\n",
                    "+first\n",
                    "@@ -20 +20 @@\n",
                    "-same\n",
                    "+second\n",
                ),
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "invalid_edit_input");
        assert_eq!(metadata["reason"], "overlapping_actual_matches");
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_one_failed_hunk_keeps_the_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.txt");
        let original = "p1\np2\np3\np4\nfirst\nmiddle\nlast\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "atomic.txt",
                "diff": concat!(
                    "@@ -1 +1 @@\n",
                    "-first\n",
                    "+FIRST\n",
                    "@@ -20 +20 @@\n",
                    "-missing\n",
                    "+LAST\n",
                ),
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            "diff_context_no_match"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_rejects_empty_hunk_context_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-context.txt");
        let original = "keep\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "empty-context.txt",
                "diff": "@@ -1,0 +1,1 @@\n+inserted\n",
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "invalid_edit_input");
        assert_eq!(metadata["reason"], "empty_context");
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn local_edit_global_scan_is_bounded() {
        let content = (0..=MAX_GLOBAL_SCAN_LINES)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        let hunks = parse_unified_diff("@@ -1 +1 @@\n-absent\n+changed\n").unwrap();

        let error = apply_hunks_local(&content, &hunks, false).unwrap_err();

        assert_eq!(error.code, "diff_context_no_match");
        assert_eq!(error.reason, "global_scan_limit");
    }

    #[test]
    fn local_edit_global_scan_caps_comparison_work() {
        let content = "a\n".repeat(2_001);
        let hunk = Hunk {
            old_start: 1,
            lines: (0..1_000)
                .map(|_| DiffLine::Remove("b".to_string()))
                .collect(),
        };

        let error = apply_hunks_local(&content, &[hunk], false).unwrap_err();

        assert_eq!(error.code, "diff_context_no_match");
        assert_eq!(error.reason, "global_scan_limit");
    }

    #[tokio::test]
    async fn should_reject_ambiguous_diff_context_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duplicate.txt");
        let original = "same\nkeep\nsame\nkeep\n";
        std::fs::write(&path, original).unwrap();

        let result = DiffEditTool::new(dir.path())
            .execute(&serde_json::json!({
                "path": "duplicate.txt",
                "diff": "@@ -1 +1 @@\n-same\n+changed\n",
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("ambiguous"), "{}", result.output);
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn test_multiple_hunks() {
        let content = "a\nb\nc\nd\ne\nf\n";
        let diff = "@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -5,2 +5,2 @@\n-e\n+E\n f\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        let result = apply_hunks(content, &hunks).unwrap();
        assert_eq!(result, "A\nb\nc\nd\nE\nf\n");
    }

    // -----------------------------------------------------------------------
    // #1774: post-edit formatting integration.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_format_after_diff_edit_when_enabled() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a(){let x=1;}\n").unwrap();

        let tool = DiffEditTool::new(dir.path());
        let mut ctx = super::ToolContext::zero();
        ctx.format_after_edit = true;

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "lib.rs",
                    "diff": "@@ -1,1 +1,1 @@\n-fn a(){let x=1;}\n+fn a(){let x=2;}\n",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "diff must apply: {}", result.output);

        // Post-edit formatting is BEST-EFFORT behind a hard 5s
        // `format::FORMAT_TIMEOUT`, and a loaded runner can blow that just
        // SPAWNING rustfmt — `check-windows` hit exactly this on run
        // 30763300877 with the suite otherwise finishing 2320 tests in 62s. A
        // timeout is a legitimate production outcome (`FormatOutcome::TimedOut`),
        // not a defect, so asserting formatting HAPPENED is only meaningful when
        // the formatter actually got to run. Same idiom as the
        // rustfmt-not-on-PATH skip above: verify what still holds, then stop.
        if result.output.contains("timed out") {
            eprintln!("skipping formatter assertions: rustfmt exceeded FORMAT_TIMEOUT");
            let on_disk = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
            // Accept EITHER spelling. A timeout means the tool stopped WAITING
            // for rustfmt, not that rustfmt stopped running: the process can
            // still finish and write the formatted file before this read. So
            // `timed out` does NOT imply the on-disk text is unformatted, and
            // asserting the unformatted spelling made this test fail whenever
            // it lost that race (`check-windows` on ce1818258 read
            // `let x = 2;` here and panicked). What this branch actually needs
            // to prove is that the EDIT SURVIVED formatting either way.
            assert!(
                on_disk.contains("let x=2") || on_disk.contains("let x = 2"),
                "the edit must survive even when formatting times out: {on_disk}"
            );
            return;
        }

        assert!(
            result.output.contains("reformatted"),
            "output must state the file was reformatted: {}",
            result.output
        );
        let on_disk = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        assert!(
            on_disk.contains("fn a() {"),
            "file must be rustfmt-formatted on disk: {on_disk}"
        );
        assert!(
            on_disk.contains("let x = 2;"),
            "edit must survive: {on_disk}"
        );
    }

    #[test]
    fn test_overlapping_hunks_rejected() {
        let content = "a\nb\nc\nd\ne\n";
        // Two hunks that overlap: first covers lines 1-3, second starts at line 2
        let diff = "@@ -1,3 +1,3 @@\n-a\n+A\n b\n c\n@@ -2,3 +2,3 @@\n-b\n+B\n c\n d\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        let result = apply_hunks(content, &hunks);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("overlapping hunks"),
            "expected overlapping hunks error, got: {err_msg}"
        );
    }
}
