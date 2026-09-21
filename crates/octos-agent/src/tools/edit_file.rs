//! Edit file tool for making precise text replacements.

use std::ops::Range;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::write_grant::WritePathGrant;
use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

const MAX_RENDERED_CANDIDATES: usize = 3;
const CANDIDATE_EXCERPT_BYTES: usize = 512;
const EDIT_REJECTION_OUTPUT_BYTES: usize = 3072;
const MAX_REPLACE_ALL_MATCHES: usize = 1_000;
const MAX_REPORTED_REPLACEMENT_LOCATIONS: usize = 20;
const MAX_REPLACE_ALL_RESULT_BYTES: usize = 10_000_000;

#[derive(Debug)]
struct AppliedEdit {
    matcher: &'static str,
    content: String,
    replacement_count: usize,
    replacement_locations: Vec<super::replacer::CandidateLineRange>,
    replace_all: bool,
}

#[derive(Debug, Clone)]
struct ReplacementOccurrence {
    range: Range<usize>,
    matcher: &'static str,
}

#[derive(Debug)]
struct ReplaceAllScan {
    occurrences: Vec<ReplacementOccurrence>,
    count: usize,
    matcher: &'static str,
}

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    Crlf,
}

fn find_replace_all_matches(content: &str, old_string: &str) -> ReplaceAllScan {
    if !content.contains("\r\n") && !old_string.contains("\r\n") {
        let mut occurrences = Vec::new();
        let mut count = 0usize;
        for (start, matched) in content.match_indices(old_string) {
            count += 1;
            if occurrences.len() < MAX_REPLACE_ALL_MATCHES {
                occurrences.push(ReplacementOccurrence {
                    range: start..start + matched.len(),
                    matcher: "exact",
                });
            }
        }
        return ReplaceAllScan {
            occurrences,
            count,
            matcher: if count == 0 { "none" } else { "exact" },
        };
    }

    let (normalized, boundaries) = normalize_crlf_with_boundaries(content);
    let normalized_old = old_string.replace("\r\n", "\n");
    let mut occurrences = Vec::new();
    let mut count = 0usize;
    let mut exact_count = 0usize;
    let mut equivalent_count = 0usize;

    for (start, matched) in normalized.match_indices(&normalized_old) {
        count += 1;
        let range = boundaries[start]..boundaries[start + matched.len()];
        let matcher = if &content[range.clone()] == old_string {
            exact_count += 1;
            "exact"
        } else {
            equivalent_count += 1;
            "line_ending_equivalent"
        };
        if occurrences.len() < MAX_REPLACE_ALL_MATCHES {
            occurrences.push(ReplacementOccurrence { range, matcher });
        }
    }

    let matcher = match (exact_count > 0, equivalent_count > 0) {
        (true, false) => "exact",
        (false, true) => "line_ending_equivalent",
        (true, true) => "exact_and_line_ending_equivalent",
        (false, false) => "none",
    };
    ReplaceAllScan {
        occurrences,
        count,
        matcher,
    }
}

fn normalize_crlf_with_boundaries(content: &str) -> (String, Vec<usize>) {
    let bytes = content.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut boundaries = Vec::with_capacity(bytes.len() + 1);
    boundaries.push(0);
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
        boundaries.push(index);
    }
    (
        String::from_utf8(normalized).expect("normalizing CRLF preserves UTF-8"),
        boundaries,
    )
}

fn line_range(content: &str, range: &Range<usize>) -> super::replacer::CandidateLineRange {
    let start = content.as_bytes()[..range.start]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    let last = range.end.saturating_sub(1).max(range.start);
    let end = content.as_bytes()[..last]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    super::replacer::CandidateLineRange { start, end }
}

fn replace_all_candidates(
    content: &str,
    occurrences: &[ReplacementOccurrence],
) -> Vec<super::replacer::ReplacementCandidate> {
    occurrences
        .iter()
        .take(MAX_RENDERED_CANDIDATES)
        .map(|occurrence| super::replacer::ReplacementCandidate {
            range: occurrence.range.clone(),
            lines: line_range(content, &occurrence.range),
            matcher: occurrence.matcher,
            score: None,
        })
        .collect()
}

fn line_ending_near(content: &str, range: &Range<usize>) -> LineEnding {
    let bytes = content.as_bytes();
    if let Some(offset) = bytes[range.start..].iter().position(|byte| *byte == b'\n') {
        let newline = range.start + offset;
        return if newline > 0 && bytes[newline - 1] == b'\r' {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };
    }
    if let Some(newline) = bytes[..range.start].iter().rposition(|byte| *byte == b'\n') {
        return if newline > 0 && bytes[newline - 1] == b'\r' {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };
    }
    LineEnding::Lf
}

fn replacement_for_line_ending(normalized_new: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => normalized_new.to_string(),
        LineEnding::Crlf => normalized_new.replace('\n', "\r\n"),
    }
}

fn replacement_result_len(
    content: &str,
    occurrences: &[ReplacementOccurrence],
    normalized_new: &str,
) -> usize {
    let newline_count = normalized_new
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    occurrences.iter().fold(content.len(), |size, occurrence| {
        let replacement_len = normalized_new.len()
            + usize::from(matches!(
                line_ending_near(content, &occurrence.range),
                LineEnding::Crlf
            )) * newline_count;
        size.saturating_sub(occurrence.range.len())
            .saturating_add(replacement_len)
    })
}

fn apply_replace_all(
    content: &str,
    occurrences: &[ReplacementOccurrence],
    normalized_new: &str,
) -> String {
    let mut result = content.to_string();
    for occurrence in occurrences.iter().rev() {
        let replacement = replacement_for_line_ending(
            normalized_new,
            line_ending_near(content, &occurrence.range),
        );
        result.replace_range(occurrence.range.clone(), &replacement);
    }
    result
}

fn replacement_locations(
    content: &str,
    occurrences: &[ReplacementOccurrence],
) -> Vec<super::replacer::CandidateLineRange> {
    occurrences
        .iter()
        .take(MAX_REPORTED_REPLACEMENT_LOCATIONS)
        .map(|occurrence| line_range(content, &occurrence.range))
        .collect()
}

fn replacement_locations_json(
    locations: &[super::replacer::CandidateLineRange],
) -> serde_json::Value {
    locations
        .iter()
        .map(|location| {
            json!({
                "line_range": {
                    "start": location.start,
                    "end": location.end,
                }
            })
        })
        .collect()
}

fn replacement_lines_summary(
    locations: &[super::replacer::CandidateLineRange],
    total: usize,
) -> String {
    let mut summary = locations
        .iter()
        .map(|location| {
            if location.start == location.end {
                location.start.to_string()
            } else {
                format!("{}-{}", location.start, location.end)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    if total > locations.len() {
        summary.push_str(&format!(",+{} more", total - locations.len()));
    }
    truncate_to_budget(&summary, 256)
}

/// Tool for editing files via string replacement.
pub struct EditFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
    /// Whether writes are permitted.
    file_access: FileAccessMode,
    /// #1976 — optional per-path write fence. Under `create_only` EVERY edit
    /// is refused (allowlisted paths may only be created); otherwise edits
    /// follow the allowlist. `None` = pre-#1976 behaviour.
    write_grant: Option<WritePathGrant>,
    local_edit_enabled: bool,
}

impl EditFileTool {
    /// Create a new edit file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            write_grant: None,
            local_edit_enabled: false,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    /// Set the effective file access mode.
    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }

    /// #1976 — bind a per-path write fence (see
    /// [`WriteFileTool::with_write_grant`](super::WriteFileTool::with_write_grant)).
    pub fn with_write_grant(mut self, write_grant: WritePathGrant) -> Self {
        self.write_grant = Some(write_grant);
        self
    }

    /// Enable typed, bounded recovery evidence for rejected edits.
    pub fn with_local_edit_enabled(mut self, enabled: bool) -> Self {
        self.local_edit_enabled = enabled;
        self
    }
}

fn displayed_path(path: &str) -> String {
    truncate_to_budget(&metadata_path(path), 96)
}

fn metadata_path(path: &str) -> String {
    if Path::new(path).is_absolute() {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<file>")
            .to_string()
    } else {
        path.to_string()
    }
}

fn truncate_to_budget(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    const SUFFIX: &str = "...";
    let mut end = budget.saturating_sub(SUFFIX.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], SUFFIX)
}

fn candidate_excerpt(content: &str, range: &Range<usize>) -> String {
    const MARKER_BYTES: usize = 6;
    if range.len() + MARKER_BYTES >= CANDIDATE_EXCERPT_BYTES {
        let matched = &content[range.clone()];
        let head_budget = (CANDIDATE_EXCERPT_BYTES - MARKER_BYTES) / 2;
        let tail_budget = CANDIDATE_EXCERPT_BYTES - MARKER_BYTES - head_budget;
        let mut head_end = head_budget.min(matched.len());
        while head_end > 0 && !matched.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = matched.len().saturating_sub(tail_budget);
        while tail_start < matched.len() && !matched.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let raw = format!("{}[...]{}", &matched[..head_end], &matched[tail_start..]);
        return truncate_to_budget(
            &crate::sanitize::sanitize_tool_output(&raw),
            CANDIDATE_EXCERPT_BYTES,
        );
    }

    let context_budget = CANDIDATE_EXCERPT_BYTES - range.len();
    let mut start = range.start.saturating_sub(context_budget / 2);
    while start < range.start && !content.is_char_boundary(start) {
        start += 1;
    }
    let mut end =
        (range.end + context_budget.saturating_sub(range.start - start)).min(content.len());
    while end > range.end && !content.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = if start > 0 { "..." } else { "" };
    let suffix = if end < content.len() { "..." } else { "" };
    let raw = format!("{prefix}{}{suffix}", &content[start..end]);
    let safe = crate::sanitize::sanitize_tool_output(&raw);
    truncate_to_budget(&safe, CANDIDATE_EXCERPT_BYTES)
}

struct EditRejection<'a> {
    code: &'static str,
    path: &'a str,
    current_bytes: &'a [u8],
    content: Option<&'a str>,
    old_string: &'a str,
    matcher: &'static str,
    occurrence_count: usize,
    candidates: &'a [super::replacer::ReplacementCandidate],
    reason: &'static str,
}

fn typed_edit_rejection(
    rejection: EditRejection<'_>,
) -> super::mutation_guard::MutationTransformError {
    let current_digest = crate::file_state_cache::FileVersion::sha256(rejection.current_bytes);
    let searched_old_digest =
        crate::file_state_cache::FileVersion::sha256(rejection.old_string.as_bytes());
    let digest = current_digest
        .strip_prefix("sha256:")
        .unwrap_or(&current_digest);
    let short_version = format!("sha256:{}...", &digest[..digest.len().min(12)]);
    let metadata_path = metadata_path(rejection.path);
    let shown_path = displayed_path(rejection.path);
    let remedy = match rejection.reason {
        "replace_all_match_limit" | "replace_all_result_limit" => {
            "use_structured_generator_or_explicit_script"
        }
        _ => "retry_with_current_exact_text",
    };
    let limit = match rejection.reason {
        "replace_all_match_limit" => format!(" limit={MAX_REPLACE_ALL_MATCHES}"),
        "replace_all_result_limit" => format!(" limit_bytes={MAX_REPLACE_ALL_RESULT_BYTES}"),
        _ => String::new(),
    };
    let mut output = format!(
        "[{}] path={shown_path} count={} \
         current={short_version}{limit} remedy={remedy}",
        rejection.code, rejection.occurrence_count,
    );
    let mut rendered_candidates = Vec::new();

    if let Some(content) = rejection.content {
        for candidate in rejection.candidates.iter().take(MAX_RENDERED_CANDIDATES) {
            let excerpt = candidate_excerpt(content, &candidate.range);
            let score_text = candidate
                .score
                .map(|score| format!(" score={score:.3}"))
                .unwrap_or_default();
            let summary = format!(
                "\nc{} suggestion=true lines={}-{} matcher={}{}",
                rendered_candidates.len() + 1,
                candidate.lines.start,
                candidate.lines.end,
                candidate.matcher,
                score_text
            );
            if output.len() + summary.len() > EDIT_REJECTION_OUTPUT_BYTES {
                break;
            }
            output.push_str(&summary);
            rendered_candidates.push((candidate, excerpt));
        }
    }

    let mut candidate_metadata = Vec::with_capacity(rendered_candidates.len());
    for (index, (candidate, excerpt)) in rendered_candidates.into_iter().enumerate() {
        let body = format!("\ncandidate {} excerpt:\n{}", index + 1, excerpt);
        if output.len() + body.len() <= EDIT_REJECTION_OUTPUT_BYTES {
            output.push_str(&body);
        }
        candidate_metadata.push(json!({
            "byte_range": {
                "start": candidate.range.start,
                "end": candidate.range.end,
            },
            "line_range": {
                "start": candidate.lines.start,
                "end": candidate.lines.end,
            },
            "matcher": candidate.matcher,
            "score": candidate.score,
            "suggestion": true,
            "excerpt": excerpt,
        }));
    }

    let output_document = crate::output_recovery::OutputDocument::unavailable(output.clone());
    super::mutation_guard::MutationTransformError::Rejected(
        super::mutation_guard::MutationRejection::new(ToolResult {
            output,
            output_document: Some(output_document),
            success: false,
            structured_metadata: Some(json!({
                "error_code": rejection.code,
                "path": metadata_path,
                "current_version": {
                    "content_sha256": current_digest,
                    "size": rejection.current_bytes.len(),
                },
                "searched_old_digest": searched_old_digest,
                "reason": rejection.reason,
                "matcher": rejection.matcher,
                "occurrence_count": rejection.occurrence_count,
                "candidates": candidate_metadata,
                "remedy": remedy,
                "replace_all_limits": {
                    "matches": MAX_REPLACE_ALL_MATCHES,
                    "result_bytes": MAX_REPLACE_ALL_RESULT_BYTES,
                },
                "file_modified": false,
            })),
            ..Default::default()
        }),
    )
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct EditFileInput {
    /// #1767: `filePath` is the industry-convention alias.
    #[serde(alias = "filePath")]
    path: String,
    #[serde(alias = "oldString")]
    old_string: String,
    #[serde(alias = "newString")]
    new_string: String,
    #[serde(default, alias = "replaceAll")]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing a string with a new string. An exact match of old_string is preferred; when none exists, whitespace-, indentation- and escape-tolerant fuzzy matching is tried as a fallback. The old_string must identify a single location."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // edit_file rewrites a file in place — same race hazard as
        // write_file. Serialize the whole batch. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (alias: filePath)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The string to find and replace. An exact match is preferred; minor whitespace/indentation/escape differences are tolerated as a fallback. Must identify a unique location. (alias: oldString)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The string to replace it with (alias: newString)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        });
        crate::local_edit::tool_input_schema(self.name(), schema, self.local_edit_enabled)
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
        let local_edit_enabled =
            self.local_edit_enabled || super::registry::local_edit_execution_enabled();
        if !local_edit_enabled
            && args.as_object().is_some_and(|args| {
                args.contains_key("replace_all") || args.contains_key("replaceAll")
            })
        {
            return Err(eyre::Report::new(super::ToolInputError::new(
                "Invalid arguments for tool 'edit_file':\n\
                 - replace_all: unknown parameter\n\
                 Fix the arguments and call the tool again.",
            )));
        }
        let schema = crate::local_edit::tool_input_schema(
            self.name(),
            self.input_schema(),
            local_edit_enabled,
        );
        let input: EditFileInput = super::args::parse_tool_args(self.name(), &schema, args)?;

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "edit_file is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Phase 2-C of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, use it as the single
        // source of truth for base_dir + path classification. Same
        // write policy as `write_file` — `InWorkspace` and
        // `InGrantedDir` allowed; `InSharedZone` and `OutOfScope`
        // refused. The shared helper canonicalizes the candidate before
        // classification so ancestor symlinks can't smuggle an edit
        // out of the workspace.
        let path = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_write(scope, &input.path) {
                Ok(p) => p,
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
                Ok(p) => p,
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

        // #1976 — a fenced edit still obtains its descriptor through the
        // component-wise confined walk. M4 hands that descriptor to the shared
        // mutation guard, which reads, re-matches, verifies, and rewrites the
        // same object.
        let opened_file = if let Some(grant) = &self.write_grant {
            let rel = match grant.check_edit(&workspace_root, &path, &input.path, self.name()) {
                Ok(rel) => rel,
                Err(denied) => {
                    return Ok(ToolResult {
                        output: denied,
                        success: false,
                        ..Default::default()
                    });
                }
            };
            match super::write_grant::confined_open_rdwr(workspace_root.clone(), rel).await {
                Ok((file, _)) => Some(file),
                Err(e) => {
                    return Ok(ToolResult {
                        output: grant.map_confined_error(
                            &e,
                            &workspace_root,
                            &input.path,
                            self.name(),
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        } else {
            None
        };

        if input.old_string.is_empty() && !local_edit_enabled {
            return Ok(ToolResult {
                output: "old_string must not be empty".to_string(),
                success: false,
                ..Default::default()
            });
        }

        let old_string = input.old_string.clone();
        let new_string = input.new_string.clone();
        let replace_all = input.replace_all;
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
            opened_file,
            move |bytes| -> Result<_, super::mutation_guard::MutationTransformError> {
                if old_string.is_empty() {
                    return Err(typed_edit_rejection(EditRejection {
                        code: "invalid_edit_input",
                        path: &display_path,
                        current_bytes: bytes,
                        content: None,
                        old_string: &old_string,
                        matcher: "none",
                        occurrence_count: 0,
                        candidates: &[],
                        reason: "empty_old_string",
                    }));
                }
                let content = match std::str::from_utf8(bytes) {
                    Ok(content) => content,
                    Err(_) if local_edit_enabled => {
                        return Err(typed_edit_rejection(EditRejection {
                            code: "invalid_edit_input",
                            path: &display_path,
                            current_bytes: bytes,
                            content: None,
                            old_string: &old_string,
                            matcher: "none",
                            occurrence_count: 0,
                            candidates: &[],
                            reason: "invalid_utf8",
                        }));
                    }
                    Err(_) => {
                        return Err("File is not valid UTF-8 and cannot be edited"
                            .to_string()
                            .into());
                    }
                };
                if local_edit_enabled && old_string == new_string {
                    return Ok((
                        bytes.to_vec(),
                        AppliedEdit {
                            matcher: "identical_input",
                            content: String::new(),
                            replacement_count: 0,
                            replacement_locations: Vec::new(),
                            replace_all,
                        },
                    ));
                }
                if replace_all {
                    let scan = find_replace_all_matches(content, &old_string);
                    if scan.count == 0 {
                        let evidence =
                            super::replacer::find_replacement_evidence(content, &old_string);
                        return Err(typed_edit_rejection(EditRejection {
                            code: "edit_no_match",
                            path: &display_path,
                            current_bytes: bytes,
                            content: Some(content),
                            old_string: &old_string,
                            matcher: "exact_or_line_ending_equivalent",
                            occurrence_count: 0,
                            candidates: &evidence.candidates,
                            reason: "replace_all_no_exact_match",
                        }));
                    }
                    let candidates = replace_all_candidates(content, &scan.occurrences);
                    if scan.count > MAX_REPLACE_ALL_MATCHES {
                        return Err(typed_edit_rejection(EditRejection {
                            code: "invalid_edit_input",
                            path: &display_path,
                            current_bytes: bytes,
                            content: Some(content),
                            old_string: &old_string,
                            matcher: scan.matcher,
                            occurrence_count: scan.count,
                            candidates: &candidates,
                            reason: "replace_all_match_limit",
                        }));
                    }
                    let normalized_new = new_string.replace("\r\n", "\n");
                    if replacement_result_len(content, &scan.occurrences, &normalized_new)
                        > MAX_REPLACE_ALL_RESULT_BYTES
                    {
                        return Err(typed_edit_rejection(EditRejection {
                            code: "invalid_edit_input",
                            path: &display_path,
                            current_bytes: bytes,
                            content: Some(content),
                            old_string: &old_string,
                            matcher: scan.matcher,
                            occurrence_count: scan.count,
                            candidates: &candidates,
                            reason: "replace_all_result_limit",
                        }));
                    }
                    let locations = replacement_locations(content, &scan.occurrences);
                    let new_content =
                        apply_replace_all(content, &scan.occurrences, &normalized_new);
                    return Ok((
                        new_content.as_bytes().to_vec(),
                        AppliedEdit {
                            matcher: scan.matcher,
                            content: new_content,
                            replacement_count: scan.count,
                            replacement_locations: locations,
                            replace_all: true,
                        },
                    ));
                }
                let evidence = super::replacer::find_replacement_evidence(content, &old_string);
                let (range, replacer_name) = match evidence.outcome.clone() {
                    super::replacer::ChainOutcome::Match { range, replacer } => (range, replacer),
                    super::replacer::ChainOutcome::Ambiguous { count, replacer }
                        if local_edit_enabled =>
                    {
                        return Err(typed_edit_rejection(EditRejection {
                            code: "edit_ambiguous",
                            path: &display_path,
                            current_bytes: bytes,
                            content: Some(content),
                            old_string: &old_string,
                            matcher: replacer,
                            occurrence_count: count,
                            candidates: &evidence.candidates,
                            reason: "ambiguous_current_matches",
                        }));
                    }
                    super::replacer::ChainOutcome::Ambiguous { count, replacer } => {
                        return Err(format!(
                            "Found {count} occurrences of the string (via {replacer} \
                                 replacer). Please provide more context to make the match unique.",
                        )
                        .into());
                    }
                    super::replacer::ChainOutcome::NoMatch if local_edit_enabled => {
                        return Err(typed_edit_rejection(EditRejection {
                            code: "edit_no_match",
                            path: &display_path,
                            current_bytes: bytes,
                            content: Some(content),
                            old_string: &old_string,
                            matcher: "none",
                            occurrence_count: 0,
                            candidates: &evidence.candidates,
                            reason: "no_safe_match",
                        }));
                    }
                    super::replacer::ChainOutcome::NoMatch => {
                        return Err(format!(
                            "String not found in file. No exact match, and no fuzzy match via \
                                 the line-trimmed, whitespace-normalized, indentation-flexible, \
                                 escape-normalized or block-anchor replacers.\n\nSearched for:\n\
                                 ```\n{old_string}\n```"
                        )
                        .into());
                    }
                };
                let guard_needle = if replacer_name == "escape_normalized" {
                    super::replacer::unescape_find(&old_string)
                } else {
                    old_string.clone()
                };
                let splice_new = if replacer_name == "escape_normalized" {
                    super::replacer::unescape_find(&new_string)
                } else {
                    new_string
                };
                let matched_text = &content[range.clone()];
                if super::replacer::is_disproportionate_match(matched_text, &guard_needle) {
                    if local_edit_enabled {
                        return Err(typed_edit_rejection(EditRejection {
                            code: "edit_no_match",
                            path: &display_path,
                            current_bytes: bytes,
                            content: Some(content),
                            old_string: &old_string,
                            matcher: replacer_name,
                            occurrence_count: 0,
                            candidates: &evidence.candidates,
                            reason: "disproportionate_fuzzy_span",
                        }));
                    }
                    return Err(format!(
                        "Fuzzy match rejected as disproportionate: the {replacer_name} replacer \
                         matched {} lines / {} bytes for an old_string of {} lines / {} bytes. \
                         Provide more context so the match is precise.",
                        matched_text.lines().count(),
                        matched_text.len(),
                        guard_needle.lines().count(),
                        guard_needle.len(),
                    )
                    .into());
                }
                let mut new_content =
                    String::with_capacity(content.len() - range.len() + splice_new.len());
                new_content.push_str(&content[..range.start]);
                new_content.push_str(&splice_new);
                new_content.push_str(&content[range.end..]);
                let location = line_range(content, &range);
                Ok((
                    new_content.as_bytes().to_vec(),
                    AppliedEdit {
                        matcher: replacer_name,
                        content: new_content,
                        replacement_count: 1,
                        replacement_locations: vec![location],
                        replace_all: false,
                    },
                ))
            },
        )
        .await;
        let guarded = match guarded {
            Ok(rewrite) => rewrite,
            Err(error) => return Ok(error.into_tool_result(self.name(), &input.path)),
        };
        let AppliedEdit {
            matcher: replacer_name,
            content: new_content,
            replacement_count,
            replacement_locations,
            replace_all,
        } = guarded.value;
        let locations_truncated = replacement_count.saturating_sub(replacement_locations.len());
        if !guarded.changed {
            let mut metadata = super::mutation_report::no_change_metadata(
                self.name(),
                &input.path,
                &guarded.before,
            );
            super::mutation_report::insert(&mut metadata, "matcher", json!(replacer_name));
            super::mutation_report::insert(
                &mut metadata,
                "replacement_count",
                json!(replacement_count),
            );
            super::mutation_report::insert(&mut metadata, "replace_all", json!(replace_all));
            super::mutation_report::insert(
                &mut metadata,
                "replacement_locations",
                replacement_locations_json(&replacement_locations),
            );
            super::mutation_report::insert(
                &mut metadata,
                "replacement_locations_truncated",
                json!(locations_truncated),
            );
            return Ok(ToolResult {
                output: format!(
                    "[no_change] path={} matcher={} current={}",
                    displayed_path(&input.path),
                    replacer_name,
                    super::mutation_report::short_bytes_version(&guarded.before),
                ),
                success: true,
                structured_metadata: Some(metadata),
                ..Default::default()
            });
        }

        if replacer_name != "exact" {
            tracing::info!(
                replacer = replacer_name,
                path = %input.path,
                "edit_file fuzzy match succeeded"
            );
        }

        // #1976 SECURITY ROUND 2 (codex): under a per-path write fence the
        // post-write processors that re-resolve the LEXICAL path are SKIPPED —
        // a fenced edit is a leaf-file operation, not a project mutation.
        // Formatting would run an external tool on a lexical filename, and
        // `snapshot_workspace_change` lexically derives a repo root and
        // unconditionally creates dirs/`.git`/objects/commits at NON-granted
        // sibling paths (reopening the ancestor-swap window the confined edit
        // just closed). `write_grant` Some at this point means the edit went
        // through the confined path above. Cache invalidation stays (pure
        // in-memory, path-keyed).
        let fence_active = self.write_grant.is_some();

        // #1774: opt-in post-edit formatting. Runs BEFORE cache invalidation
        // and the git snapshot so both observe the final on-disk content.
        // Best-effort by contract — a formatter failure never fails the edit.
        // Never runs under a fence (see above).
        let formatting = if local_edit_enabled {
            Some(
                super::mutation_report::FormattingRun::execute(
                    &path,
                    ctx.format_after_edit,
                    !fence_active,
                )
                .await,
            )
        } else {
            None
        };
        let format_note = if !local_edit_enabled && ctx.format_after_edit && !fence_active {
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
            super::mutation_report::insert(&mut report.metadata, "matcher", json!(replacer_name));
            super::mutation_report::insert(
                &mut report.metadata,
                "replacement_count",
                json!(replacement_count),
            );
            super::mutation_report::insert(&mut report.metadata, "replace_all", json!(replace_all));
            super::mutation_report::insert(
                &mut report.metadata,
                "replacement_locations",
                replacement_locations_json(&replacement_locations),
            );
            super::mutation_report::insert(
                &mut report.metadata,
                "replacement_locations_truncated",
                json!(locations_truncated),
            );
        }

        // Invalidate every recorded workspace-owned version for this path.
        super::mutation_guard::complete_mutation(ctx, &workspace_root, &path);

        if !fence_active {
            if let Err(error) =
                crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "edit_file")
            {
                warn!(
                    path = %input.path,
                    error = %error,
                    "workspace git snapshot failed after edit_file"
                );
            }
        }

        // Report which replacer produced the match. The exact-match wording
        // is kept identical to the historical output for compatibility.
        let output = if let Some(report) = report.as_ref() {
            if replace_all {
                format!(
                    "Edited {}: matcher={}, replacements={}, lines={}, final={}, formatter={}",
                    displayed_path(&input.path),
                    replacer_name,
                    replacement_count,
                    replacement_lines_summary(&replacement_locations, replacement_count),
                    report.final_label,
                    report.formatter_label,
                )
            } else {
                format!(
                    "Edited {}: matcher={}, replacements=1, final={}, formatter={}",
                    displayed_path(&input.path),
                    replacer_name,
                    report.final_label,
                    report.formatter_label,
                )
            }
        } else if replacer_name == "exact" {
            format!("Successfully edited {}", input.path)
        } else {
            format!(
                "Successfully edited {} (fuzzy match via {replacer_name} replacer)",
                input.path
            )
        };

        Ok(ToolResult {
            output: format!("{output}{}", format_note.unwrap_or_default()),
            success: true,
            file_modified: Some(path),
            structured_metadata: report.map(|report| report.metadata),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn edit_file_tool_is_exclusive() {
        // edit_file rewrites the target file; parallel read_file would race
        // on in-flight content, so it must serialize (M8.8).
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[tokio::test]
    async fn test_edit_file_basic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "code.rs",
                "old_string": "println!(\"hello\")",
                "new_string": "println!(\"world\")"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[tokio::test]
    async fn test_edit_file_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "some content").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "file.txt",
                "old_string": "nonexistent string",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("String not found"));
    }

    #[tokio::test]
    async fn test_edit_file_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "foo bar foo baz foo").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "dup.txt",
                "old_string": "foo",
                "new_string": "qux"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("3 occurrences"));
    }

    #[tokio::test]
    async fn local_edit_ambiguity_returns_bounded_current_unicode_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let original = "开头\r\n目标 α\r\n中间\r\n目标 α\r\n更多\r\n目标 α\r\n末尾\r\n目标 α\r\n";
        std::fs::write(dir.path().join("重复.txt"), original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "重复.txt",
                "old_string": "目标 α",
                "new_string": "替换"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.file_modified.is_none());
        assert!(result.output.starts_with("[edit_ambiguous]"));
        assert!(result.output.len() <= EDIT_REJECTION_OUTPUT_BYTES);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "edit_ambiguous");
        assert_eq!(metadata["matcher"], "exact");
        assert_eq!(metadata["occurrence_count"], 4);
        assert_eq!(
            metadata["current_version"]["content_sha256"],
            crate::file_state_cache::FileVersion::sha256(original.as_bytes())
        );
        assert_eq!(
            metadata["searched_old_digest"],
            crate::file_state_cache::FileVersion::sha256("目标 α".as_bytes())
        );
        let candidates = metadata["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), MAX_RENDERED_CANDIDATES);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate["line_range"]["start"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![2, 4, 6]
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate["suggestion"] == true)
        );
        assert!(
            result
                .output_document
                .as_ref()
                .is_some_and(|document| document.file_read.is_none()),
            "candidate output must not be eligible for a full-file read receipt"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("重复.txt")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn local_edit_no_match_returns_optional_current_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let original = "begin\ncompletely different middle here\nfinish\n";
        std::fs::write(dir.path().join("candidate.txt"), original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "candidate.txt",
                "old_string": "begin\nzzzz\nfinish",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.starts_with("[edit_no_match]"));
        assert!(result.output.contains("suggestion=true"));
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "edit_no_match");
        assert_eq!(metadata["matcher"], "none");
        assert_eq!(metadata["occurrence_count"], 0);
        assert_eq!(metadata["candidates"][0]["matcher"], "block_anchor");
        assert!(
            metadata["candidates"][0]["score"]
                .as_f64()
                .is_some_and(|score| score < 0.65)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("candidate.txt")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn local_edit_no_match_without_candidate_stays_typed_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let original = "alpha\nbeta\n";
        std::fs::write(dir.path().join("none.txt"), original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "none.txt",
                "old_string": "one\ntwo\nthree",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.starts_with("[edit_no_match]"));
        assert!(!result.output.contains("suggestion=true"));
        assert!(
            result.structured_metadata.as_ref().unwrap()["candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("none.txt")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn local_edit_keeps_fuzzy_writes_but_types_multistage_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let unique = "fn one() {\n    launch();\n}\n";
        std::fs::write(dir.path().join("unique.rs"), unique).unwrap();
        let tool = EditFileTool::new(dir.path()).with_local_edit_enabled(true);

        let applied = tool
            .execute(&serde_json::json!({
                "path": "unique.rs",
                "old_string": "fn one() {\nlaunch();\n}",
                "new_string": "fn one() {\n    stop();\n}"
            }))
            .await
            .unwrap();
        assert!(applied.success, "{}", applied.output);
        assert!(applied.output.contains("line_trimmed"));

        let repeated = "fn a() {\n    launch();\n}\nfn b() {\n  launch();\n}\n";
        std::fs::write(dir.path().join("repeated.rs"), repeated).unwrap();
        let rejected = tool
            .execute(&serde_json::json!({
                "path": "repeated.rs",
                "old_string": "launch(); ",
                "new_string": "stop();"
            }))
            .await
            .unwrap();

        assert!(!rejected.success);
        let metadata = rejected.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "edit_ambiguous");
        assert_eq!(metadata["matcher"], "line_trimmed");
        assert_eq!(metadata["occurrence_count"], 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("repeated.rs")).unwrap(),
            repeated
        );
    }

    #[tokio::test]
    async fn local_edit_candidate_excerpt_bounds_long_unicode_line_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let nested = "long-directory-name-".repeat(8);
        std::fs::create_dir(dir.path().join(&nested)).unwrap();
        let relative = format!("{nested}/long.txt");
        let long_line = format!("prefix {} target suffix", "界".repeat(2000));
        let original = format!("{long_line}\n{long_line}\n");
        std::fs::write(dir.path().join(&relative), &original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": relative,
                "old_string": "target",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.len() <= EDIT_REJECTION_OUTPUT_BYTES);
        let candidates = result.structured_metadata.as_ref().unwrap()["candidates"]
            .as_array()
            .unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| {
            candidate["excerpt"]
                .as_str()
                .is_some_and(|excerpt| excerpt.len() <= CANDIDATE_EXCERPT_BYTES)
        }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&relative)).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn local_edit_rejection_does_not_expose_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.txt");
        std::fs::write(&path, "same\nsame\n").unwrap();
        let absolute = path.display().to_string();

        let result = EditFileTool::new(dir.path())
            .with_filesystem_scope(FilesystemScope::Host)
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": absolute,
                "old_string": "same",
                "new_string": "changed"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(!result.output.contains(&dir.path().display().to_string()));
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["path"],
            "private.txt"
        );
    }

    #[tokio::test]
    async fn local_edit_candidate_evidence_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "a".repeat(64);
        let original = format!("{secret} target\n{secret} target\n");
        std::fs::write(dir.path().join("secret.txt"), &original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "secret.txt",
                "old_string": "target",
                "new_string": "changed"
            }))
            .await
            .unwrap();

        assert!(!result.output.contains(&secret));
        assert!(result.output.contains("[hex-redacted]"));
        let metadata = result.structured_metadata.unwrap();
        assert!(
            metadata["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|candidate| !candidate["excerpt"].as_str().unwrap().contains(&secret))
        );
    }

    #[tokio::test]
    async fn test_edit_file_multiline_replacement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("multi.txt"), "line1\nline2\nline3\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "multi.txt",
                "old_string": "line2\nline3",
                "new_string": "replaced2\nreplaced3"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("multi.txt")).unwrap();
        assert!(content.contains("replaced2\nreplaced3"));
    }

    #[tokio::test]
    async fn test_edit_file_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({
                "path": "nope.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_edit_file_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({
                "path": "../../etc/passwd",
                "old_string": "root",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside working directory"));
    }

    #[test]
    fn test_tool_metadata() {
        let tool = EditFileTool::new("/tmp");
        assert_eq!(tool.name(), "edit_file");
        assert!(tool.tags().contains(&"fs"));
    }

    // -----------------------------------------------------------------------
    // Phase 2-C: SessionScope integration tests for EditFileTool.
    // -----------------------------------------------------------------------

    use octos_core::SessionScope;

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "edit-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn edit_file_uses_scope_workspace_as_base_dir_for_relative_paths() {
        // Relative edit path anchors at `scope.workspace()`, not the
        // legacy `base_dir`. Pre-create the target file there.
        let scope_dir = tempfile::tempdir().unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        std::fs::write(scope_dir.path().join("doc.md"), "before\n").unwrap();
        std::fs::write(legacy_dir.path().join("doc.md"), "decoy\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(legacy_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "doc.md",
                    "old_string": "before",
                    "new_string": "after",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);

        // Only the scope-dir copy is mutated; the legacy decoy is
        // untouched. (Edit even refused to look at the legacy file.)
        assert_eq!(
            std::fs::read_to_string(scope_dir.path().join("doc.md")).unwrap(),
            "after\n",
        );
        assert_eq!(
            std::fs::read_to_string(legacy_dir.path().join("doc.md")).unwrap(),
            "decoy\n",
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_out_of_scope_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("target.txt");
        std::fs::write(&outside_file, "untouched\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": outside_file.to_string_lossy(),
                    "old_string": "untouched",
                    "new_string": "owned",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
        // File MUST remain unchanged.
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "untouched\n"
        );
    }

    #[tokio::test]
    async fn edit_file_allows_in_workspace_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        std::fs::write(scope_dir.path().join("inside.txt"), "alpha\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "inside.txt",
                    "old_string": "alpha",
                    "new_string": "beta",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert_eq!(
            std::fs::read_to_string(scope_dir.path().join("inside.txt")).unwrap(),
            "beta\n",
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_write_to_shared_zone() {
        // Multi-tenant shared zones are read-only for session workers
        // — symmetric with `write_file_refuses_write_to_shared_zone`.
        let data_dir = tempfile::tempdir().unwrap();
        let data = data_dir.path().to_path_buf();
        std::fs::create_dir_all(data.join("research/topic")).unwrap();
        std::fs::create_dir_all(data.join("users/web-1/workspace")).unwrap();
        let shared_file = data.join("research/topic/notes.md");
        std::fs::write(&shared_file, "untouched\n").unwrap();

        let scope = SessionScope::multi_tenant_with_default_zones(
            data.clone(),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let tool = EditFileTool::new(scope.workspace());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": shared_file.to_string_lossy(),
                    "old_string": "untouched",
                    "new_string": "owned",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("shared zone"),
            "expected shared-zone rejection, got: {}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(&shared_file).unwrap(),
            "untouched\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_refuses_ancestor_symlink_escape() {
        // Symmetric with the write_file ancestor-symlink test. Even
        // with a real target file pre-staged at the symlink target,
        // the scoped resolver must refuse before O_NOFOLLOW would
        // (correctly) bail on the symlink itself.
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        std::fs::write(outside_dir.path().join("target.txt"), "secret\n").unwrap();
        let link_path = scope_dir.path().join("link");
        symlink(outside_dir.path(), &link_path).unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "link/target.txt",
                    "old_string": "secret",
                    "new_string": "owned",
                }),
            )
            .await
            .unwrap();
        assert!(
            !result.success,
            "ancestor-symlink escape MUST be refused, got: {}",
            result.output
        );
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
        // Real file at the symlink target must remain unchanged.
        assert_eq!(
            std::fs::read_to_string(outside_dir.path().join("target.txt")).unwrap(),
            "secret\n",
        );
    }

    #[tokio::test]
    async fn edit_file_falls_back_to_legacy_when_no_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "x\n").unwrap();
        let tool = EditFileTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let ok = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "ok.txt",
                    "old_string": "x",
                    "new_string": "y",
                }),
            )
            .await
            .unwrap();
        assert!(ok.success);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ok.txt")).unwrap(),
            "y\n"
        );

        let bad = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "../escape.txt",
                    "old_string": "a",
                    "new_string": "b",
                }),
            )
            .await
            .unwrap();
        assert!(!bad.success);
        assert!(bad.output.contains("outside working directory"));
    }

    // -----------------------------------------------------------------------
    // #1771: cascading fuzzy replacer chain.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_match_via_line_trimmed_when_indentation_differs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    if ready {\n        launch();\n    }\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // LLM lost the indentation (flush left) but every line's trimmed
        // content is right — the line_trimmed replacer must recover it.
        let result = tool
            .execute(&serde_json::json!({
                "path": "code.rs",
                "old_string": "if ready {\nlaunch();\n}",
                "new_string": "if ready {\n        abort();\n    }"
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "line-trimmed fuzzy match should succeed: {}",
            result.output
        );
        assert!(
            result.output.contains("line_trimmed"),
            "success output must report which replacer matched: {}",
            result.output
        );
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("abort();"));
        assert!(!content.contains("launch();"));
    }

    #[tokio::test]
    async fn should_match_via_whitespace_normalized_when_internal_runs_differ() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("w.rs"), "let x  =  compute( a, b );\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "w.rs",
                "old_string": "let x = compute( a, b );",
                "new_string": "let x = compute(a, b, c);"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("whitespace_normalized"));
        let content = std::fs::read_to_string(dir.path().join("w.rs")).unwrap();
        assert_eq!(content, "let x = compute(a, b, c);\n");
    }

    #[tokio::test]
    async fn should_match_via_indentation_flexible_when_blank_boundary_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("i.rs"),
            "fn wrapper() {\n    step_one();\n    step_two();\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // Needle copied with stray blank lines around the block and a
        // uniformly deeper indent — only the dedent matcher recovers it.
        let result = tool
            .execute(&serde_json::json!({
                "path": "i.rs",
                "old_string": "\n        step_one();\n        step_two();\n\n",
                "new_string": "    merged_steps();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("indentation_flexible"));
        let content = std::fs::read_to_string(dir.path().join("i.rs")).unwrap();
        assert_eq!(content, "fn wrapper() {\n    merged_steps();\n}\n");
    }

    #[tokio::test]
    async fn should_match_via_escape_normalized_when_newline_double_escaped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e.rs"), "alpha {\n    beta();\n}\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        // old_string carries a literal backslash-n instead of a newline.
        let result = tool
            .execute(&serde_json::json!({
                "path": "e.rs",
                "old_string": "alpha {\\n    beta();",
                "new_string": "alpha {\n    gamma();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("escape_normalized"));
        let content = std::fs::read_to_string(dir.path().join("e.rs")).unwrap();
        assert_eq!(content, "alpha {\n    gamma();\n}\n");
    }

    #[tokio::test]
    async fn should_unescape_new_string_when_match_is_escape_normalized() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e2.rs"), "alpha {\n    beta();\n}\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        // #1771 review: realistic double-escaping afflicts BOTH strings of
        // the call. The new_string must be unescaped with the same rules
        // that made old_string match, or literal backslash-n text is
        // written into the file as code.
        let result = tool
            .execute(&serde_json::json!({
                "path": "e2.rs",
                "old_string": "alpha {\\n    beta();",
                "new_string": "alpha {\\n    gamma();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("escape_normalized"));
        let content = std::fs::read_to_string(dir.path().join("e2.rs")).unwrap();
        assert_eq!(content, "alpha {\n    gamma();\n}\n");
    }

    #[tokio::test]
    async fn should_not_reject_multiline_escape_normalized_match_as_disproportionate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.rs"),
            "l1();\nl2();\nl3();\nl4();\nl5();\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // #1771 review: the escaped old_string is ONE physical line but
        // unescapes to five real lines. The disproportionate guard must
        // measure against the unescaped needle — with the raw old_string
        // the line cap is max(1+3, 2) = 4 and every legitimate >= 5-line
        // escape-normalized match was falsely rejected.
        let result = tool
            .execute(&serde_json::json!({
                "path": "m.rs",
                "old_string": "l1();\\nl2();\\nl3();\\nl4();\\nl5();",
                "new_string": "one();\\ntwo();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("escape_normalized"));
        let content = std::fs::read_to_string(dir.path().join("m.rs")).unwrap();
        assert_eq!(content, "one();\ntwo();\n");
    }

    #[tokio::test]
    async fn should_match_via_block_anchor_when_middle_line_drifted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "fn compute() {\n    let total = base + extra;\n    total * 2\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // Middle line remembered slightly wrong (offset vs extra) — first
        // and last lines anchor the block, similarity carries the middle.
        let result = tool
            .execute(&serde_json::json!({
                "path": "b.rs",
                "old_string": "fn compute() {\n    let total = base + offset;\n    total * 2\n}",
                "new_string": "fn compute() {\n    base * 3\n}"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("block_anchor"));
        let content = std::fs::read_to_string(dir.path().join("b.rs")).unwrap();
        assert_eq!(content, "fn compute() {\n    base * 3\n}\n");
    }

    #[tokio::test]
    async fn should_refuse_block_anchor_span_far_exceeding_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let original = "fn f() {\n    a();\n    b();\n    c();\n    d();\n    e();\n    g();\n}\n";
        std::fs::write(dir.path().join("g.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // Anchors would bracket 8 lines for a 3-line old_string. The block
        // similarity score counts the six undescribed middle lines against
        // the match, so block_anchor refuses outright (NoMatch) before the
        // disproportionate guard is even consulted.
        let result = tool
            .execute(&serde_json::json!({
                "path": "g.rs",
                "old_string": "fn f() {\n    a();\n}",
                "new_string": "fn f() {}"
            }))
            .await
            .unwrap();

        assert!(
            !result.success,
            "runaway span must be refused: {}",
            result.output
        );
        assert!(
            result.output.contains("String not found"),
            "expected NoMatch refusal, got: {}",
            result.output
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("g.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_reject_disproportionate_byte_growth_on_block_anchor_match() {
        let dir = tempfile::tempdir().unwrap();
        let indent = " ".repeat(60);
        let original = format!("{indent}fn deep() {{\n{indent}    b_call();\n{indent}}}\n");
        std::fs::write(dir.path().join("d.rs"), &original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // Line counts line up (3 vs 3) and the drifted middle clears the
        // similarity bar, but the 60-space indentation makes the span more
        // than 4x the old_string's bytes — the guard must still refuse
        // block_anchor matches on byte growth.
        let result = tool
            .execute(&serde_json::json!({
                "path": "d.rs",
                "old_string": "fn deep() {\n    a_call();\n}",
                "new_string": "fn deep() {}"
            }))
            .await
            .unwrap();

        assert!(
            !result.success,
            "byte blowup must be refused: {}",
            result.output
        );
        assert!(result.output.contains("disproportionate"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("d.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_reject_disproportionate_char_growth() {
        let dir = tempfile::tempdir().unwrap();
        let indent = " ".repeat(60);
        let original = format!("{indent}x();\n{indent}y();\n{indent}z();\n");
        std::fs::write(dir.path().join("c.rs"), &original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // line_trimmed would match, but the span is 194 bytes for a
        // 14-byte old_string (> 4x) — the guard must refuse.
        let result = tool
            .execute(&serde_json::json!({
                "path": "c.rs",
                "old_string": "x();\ny();\nz();",
                "new_string": "w();"
            }))
            .await
            .unwrap();

        assert!(!result.success, "guard must reject: {}", result.output);
        assert!(result.output.contains("disproportionate"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_fail_with_count_when_fuzzy_match_is_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let original = "fn a() {\n    if ready {\n        launch();\n    }\n}\nfn b() {\n  if ready {\n    launch();\n  }\n}\n";
        std::fs::write(dir.path().join("amb.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // No exact occurrence, but the trimmed block exists at two
        // different indentation levels — must fail with the count, not
        // silently pick one or fall through to a fuzzier stage.
        let result = tool
            .execute(&serde_json::json!({
                "path": "amb.rs",
                "old_string": "if ready {\nlaunch();\n}",
                "new_string": "abort();"
            }))
            .await
            .unwrap();

        assert!(!result.success, "{}", result.output);
        assert!(
            result.output.contains("2 occurrences"),
            "expected the ambiguity count, got: {}",
            result.output
        );
        assert!(result.output.contains("line_trimmed"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("amb.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn should_reject_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "content").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "f.txt",
                "old_string": "",
                "new_string": "x"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("must not be empty"));
    }

    #[tokio::test]
    async fn local_edit_empty_old_string_is_typed_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "content").unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "f.txt",
                "old_string": "",
                "new_string": "x"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            "invalid_edit_input"
        );
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["reason"],
            "empty_old_string"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "content");
    }

    #[tokio::test]
    async fn should_keep_exact_success_output_stable() {
        // Exact matches keep the historical wording — no replacer chatter.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.txt"), "hello world\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "s.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "Successfully edited s.txt");
    }

    #[tokio::test]
    async fn local_edit_no_change_has_no_write_or_post_processors() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("sites/demo");
        std::fs::create_dir_all(&site).unwrap();
        let path = site.join("index.html");
        std::fs::write(&path, "<h1>same</h1>\n").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let target =
            crate::file_state_cache::FileTarget::for_local_workspace(dir.path(), &path).unwrap();
        let ledger = Arc::new(crate::file_state_cache::FileStateCache::new());
        let version = crate::file_state_cache::FileVersion::from_bytes(
            target.clone(),
            None,
            b"<h1>same</h1>\n",
            crate::file_state_cache::FileMetadataHint::from_metadata(&before),
        );
        ledger.record(version.clone());
        let receipts = Arc::new(
            crate::model_read_receipts::ModelReadReceiptStore::for_owner(
                crate::model_read_receipts::ReadReceiptOwner::new(
                    target.workspace_id(),
                    "task",
                    "session",
                    "branch",
                )
                .unwrap(),
            ),
        );
        let read_args = serde_json::json!({"path": "sites/demo/index.html"});
        receipts.stage(
            "call_read",
            &read_args,
            version,
            crate::model_read_receipts::FileView::Full,
            crate::model_read_receipts::FileView::Full,
            "visible",
        );
        let mut assistant = octos_core::Message::assistant("");
        assistant.tool_calls = Some(vec![octos_core::ToolCall {
            id: "call_read".into(),
            name: "read_file".into(),
            arguments: read_args,
            metadata: None,
        }]);
        let tool_output = octos_core::Message {
            role: octos_core::MessageRole::Tool,
            content: "visible".into(),
            media: Vec::new(),
            tool_calls: None,
            tool_call_id: Some("call_read".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let pending = receipts.prepare_dispatch(&[assistant, tool_output], "policy-v1");
        receipts.activate(pending);
        assert_eq!(receipts.active_len(), 1);
        let mut ctx = ToolContext::zero();
        ctx.file_state_cache = Some(ledger.clone());
        ctx.model_read_receipts = Some(receipts.clone());
        ctx.format_after_edit = true;

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "sites/demo/index.html",
                    "old_string": "same",
                    "new_string": "same"
                }),
            )
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.file_modified.is_none());
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["outcome"], "no_change");
        assert_eq!(metadata["file_modified"], false);
        assert_eq!(metadata["formatter"]["status"], "not_run");
        assert!(ledger.peek(&target).is_some());
        assert_eq!(receipts.active_len(), 1);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(!site.join(".git").exists());
    }

    #[tokio::test]
    async fn local_edit_identical_arguments_are_no_change_without_matching() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "current\n").unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "file.txt",
                "old_string": "absent",
                "new_string": "absent",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.file_modified.is_none());
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["outcome"], "no_change");
        assert_eq!(metadata["matcher"], "identical_input");
        assert_eq!(metadata["replace_all"], true);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "current\n");
    }

    #[tokio::test]
    async fn local_edit_replace_all_replaces_every_exact_non_overlapping_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("all.txt");
        std::fs::write(&path, "aaaa\n").unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "all.txt",
                "old_string": "aa",
                "new_string": "猫",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "猫猫\n");
        assert!(result.output.contains("replacements=2"));
        assert!(result.output.contains("lines=1,1"));
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["matcher"], "exact");
        assert_eq!(metadata["replace_all"], true);
        assert_eq!(metadata["replacement_count"], 2);
        assert_eq!(
            metadata["replacement_locations"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            metadata["replacement_locations"][0]["line_range"]["start"],
            1
        );
        assert_eq!(
            metadata["replacement_locations"][1]["line_range"]["start"],
            1
        );
    }

    #[test]
    fn replace_all_match_collection_never_selects_overlapping_ranges() {
        let scan = find_replace_all_matches("aaa", "aa");

        assert_eq!(scan.count, 1);
        assert_eq!(scan.occurrences[0].range, 0..2);
        assert_eq!(apply_replace_all("aaa", &scan.occurrences, "b"), "ba");
    }

    #[tokio::test]
    async fn local_edit_replace_all_false_preserves_single_match_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("default.txt");
        let original = "same\nsame\n";
        std::fs::write(&path, original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "default.txt",
                "old_string": "same",
                "new_string": "changed",
                "replace_all": false
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            "edit_ambiguous"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_replace_all_never_uses_fuzzy_matchers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fuzzy.rs");
        let original = "fn one() {\n    launch();\n}\n";
        std::fs::write(&path, original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "fuzzy.rs",
                "old_string": "fn one() {\nlaunch();\n}",
                "new_string": "fn one() {\n    stop();\n}",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "edit_no_match");
        assert_eq!(metadata["reason"], "replace_all_no_exact_match");
        assert_eq!(metadata["candidates"][0]["matcher"], "line_trimmed");
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_replace_all_handles_crlf_and_unicode_without_normalizing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.txt");
        std::fs::write(&path, "目标\r\nkeep\r\n目标\r\n").unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "unicode.txt",
                "old_string": "目标\n",
                "new_string": "结果\n",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "结果\r\nkeep\r\n结果\r\n".as_bytes()
        );
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["matcher"], "line_ending_equivalent");
        assert_eq!(metadata["replacement_count"], 2);
        assert_eq!(
            metadata["replacement_locations"][0]["line_range"]["start"],
            1
        );
        assert_eq!(
            metadata["replacement_locations"][1]["line_range"]["start"],
            3
        );
    }

    #[tokio::test]
    async fn local_edit_replace_all_truncates_locations_but_keeps_total_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.txt");
        std::fs::write(
            &path,
            "hit\n".repeat(MAX_REPORTED_REPLACEMENT_LOCATIONS + 5),
        )
        .unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "many.txt",
                "old_string": "hit",
                "new_string": "done",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(
            metadata["replacement_count"],
            MAX_REPORTED_REPLACEMENT_LOCATIONS + 5
        );
        assert_eq!(
            metadata["replacement_locations"].as_array().unwrap().len(),
            MAX_REPORTED_REPLACEMENT_LOCATIONS
        );
        assert_eq!(metadata["replacement_locations_truncated"], 5);
    }

    #[tokio::test]
    async fn local_edit_replace_all_rejects_the_match_limit_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too-many.txt");
        let original = "x".repeat(MAX_REPLACE_ALL_MATCHES + 1);
        std::fs::write(&path, &original).unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "too-many.txt",
                "old_string": "x",
                "new_string": "y",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "invalid_edit_input");
        assert_eq!(metadata["reason"], "replace_all_match_limit");
        assert_eq!(metadata["occurrence_count"], MAX_REPLACE_ALL_MATCHES + 1);
        assert!(
            result
                .output
                .contains("use_structured_generator_or_explicit_script")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn local_edit_replace_all_rejects_an_oversized_result_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too-large.txt");
        std::fs::write(&path, "x\n").unwrap();
        let replacement = "y".repeat(MAX_REPLACE_ALL_RESULT_BYTES + 1);

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "too-large.txt",
                "old_string": "x",
                "new_string": replacement,
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["error_code"], "invalid_edit_input");
        assert_eq!(metadata["reason"], "replace_all_result_limit");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "x\n");
    }

    #[tokio::test]
    async fn local_edit_success_reports_final_actual_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.txt"), "hello world\n").unwrap();

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute(&serde_json::json!({
                "path": "s.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert!(result.file_modified.is_some());
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["outcome"], "modified");
        assert_eq!(metadata["matcher"], "exact");
        assert_eq!(metadata["replacement_count"], 1);
        assert_eq!(metadata["final_state"], "confirmed");
        assert!(metadata["before_version"].is_object());
        assert!(metadata["write_version"].is_object());
        assert!(metadata["final_version"].is_object());
        assert_eq!(metadata["changed_range"]["before"]["start"], 1);
        assert_eq!(metadata["changed_range"]["before"]["count"], 1);
        assert_eq!(metadata["changed_range"]["after"]["count"], 1);
        assert_eq!(metadata["formatter"]["status"], "disabled");
        assert_eq!(metadata["diff_preview"][0]["op"], "update");
        assert!(
            metadata["diff_preview"][0]["diff"]
                .as_str()
                .unwrap()
                .contains("goodbye")
        );
        assert!(result.output.contains("final=sha256:"));
    }

    #[tokio::test]
    async fn should_splice_fuzzy_match_at_located_span_not_first_substring() {
        // The fuzzy-matched span's exact text ("    a();\n    b();") ALSO
        // occurs earlier in the file, but mid-line (inside a string-ish
        // context), where it is not a valid line window. A string replacen
        // of the matched text would corrupt the earlier occurrence; the
        // byte-range splice must edit the located window only.
        let dir = tempfile::tempdir().unwrap();
        let original = "code(    a();\n    b();x)\nfn late() {\n    a();\n    b();\n}\n";
        std::fs::write(dir.path().join("span.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "span.rs",
                // Flush-left needle: line_trimmed matches only the window
                // inside fn late().
                "old_string": "a();\nb();",
                "new_string": "    c();"
            }))
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        let content = std::fs::read_to_string(dir.path().join("span.rs")).unwrap();
        assert_eq!(
            content,
            "code(    a();\n    b();x)\nfn late() {\n    c();\n}\n"
        );
    }

    // -----------------------------------------------------------------------
    // #1767: industry-convention parameter aliases.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_accept_camel_case_aliases_for_edit_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alias.txt"), "hello world\n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "filePath": "alias.txt",
                "oldString": "hello",
                "newString": "goodbye"
            }))
            .await
            .unwrap();

        assert!(result.success, "aliases must work: {}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("alias.txt")).unwrap(),
            "goodbye world\n"
        );
    }

    #[test]
    fn schema_advertises_canonical_names_only() {
        let tool = EditFileTool::new("/tmp");
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("old_string"));
        assert!(props.contains_key("new_string"));
        assert!(!props.contains_key("replace_all"));
        assert!(!props.contains_key("filePath"));
        assert!(!props.contains_key("oldString"));
        assert!(!props.contains_key("newString"));
        assert!(!props.contains_key("replaceAll"));

        let enabled = EditFileTool::new("/tmp").with_local_edit_enabled(true);
        let enabled_schema = enabled.input_schema();
        assert_eq!(
            enabled_schema["properties"]["replace_all"]["type"],
            "boolean"
        );
        assert_eq!(
            enabled_schema["properties"]["replace_all"]["default"],
            false
        );
    }

    #[tokio::test]
    async fn local_edit_replace_all_rejects_wrong_type_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "same\nsame\n").unwrap();
        let tool = EditFileTool::new(dir.path()).with_local_edit_enabled(true);

        let wrong_type = tool
            .execute(&serde_json::json!({
                "path": "file.txt",
                "old_string": "same",
                "new_string": "changed",
                "replace_all": "yes"
            }))
            .await
            .err()
            .unwrap()
            .to_string();
        let unknown = tool
            .execute(&serde_json::json!({
                "path": "file.txt",
                "old_string": "same",
                "new_string": "changed",
                "replace_everything": true
            }))
            .await
            .err()
            .unwrap()
            .to_string();

        assert!(wrong_type.contains("replace_all: expected boolean"));
        assert!(unknown.contains("replace_everything: unknown parameter"));
    }

    #[tokio::test]
    async fn should_edit_file_tool_invalidate_cache_after_edit() {
        use crate::file_state_cache::{FileMetadataHint, FileStateCache, FileTarget, FileVersion};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        std::fs::write(&file_path, "fn foo() {}\n").unwrap();

        let cache = Arc::new(FileStateCache::new());
        let target = FileTarget::for_local_workspace(dir.path(), &file_path).unwrap();
        cache.record(FileVersion::from_bytes(
            target.clone(),
            None,
            b"fn foo() {}\n",
            FileMetadataHint::from_metadata(&std::fs::metadata(&file_path).unwrap()),
        ));
        assert_eq!(cache.len(), 1);

        let mut ctx = ToolContext::zero();
        ctx.file_state_cache = Some(cache.clone());

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "code.rs",
                    "old_string": "fn foo() {}",
                    "new_string": "fn bar() {}"
                }),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(cache.peek(&target).is_none());
    }

    #[tokio::test]
    async fn current_unique_context_can_apply_and_revokes_the_old_receipt() {
        use crate::model_read_receipts::{ModelReadReceiptStore, ReadReceiptOwner};
        use crate::tools::read_file::ReadFileTool;
        use octos_core::{Message, MessageRole, ToolCall};

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("notes.txt");
        std::fs::write(&file_path, "before\nchange me\nafter\n").unwrap();
        let workspace_id = format!(
            "local:{}",
            std::fs::canonicalize(dir.path()).unwrap().display()
        );
        let ledger = Arc::new(crate::file_state_cache::FileStateCache::new());
        let receipts = Arc::new(ModelReadReceiptStore::for_owner(
            ReadReceiptOwner::new(workspace_id, "task", "session", "branch").unwrap(),
        ));
        let mut context = ToolContext::zero();
        context.tool_id = "call_read".to_string();
        context.file_state_cache = Some(ledger);
        context.model_read_receipts = Some(receipts.clone());
        let read_args = serde_json::json!({"path": "notes.txt"});
        let read = ReadFileTool::new(dir.path())
            .execute_with_context(&context, &read_args)
            .await
            .unwrap();
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_read".to_string(),
            name: "read_file".to_string(),
            arguments: read_args,
            metadata: None,
        }]);
        let tool_output = Message {
            role: MessageRole::Tool,
            content: read.output,
            media: Vec::new(),
            tool_calls: None,
            tool_call_id: Some("call_read".to_string()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let pending = receipts.prepare_dispatch(&[assistant, tool_output], "policy-v1");
        receipts.activate(pending);
        assert_eq!(receipts.active_len(), 1);

        std::fs::write(&file_path, "external header\nbefore\nchange me\nafter\n").unwrap();
        let result = EditFileTool::new(dir.path())
            .execute_with_context(
                &context,
                &serde_json::json!({
                    "path": "notes.txt",
                    "old_string": "change me",
                    "new_string": "changed",
                }),
            )
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "external header\nbefore\nchanged\nafter\n"
        );
        assert_eq!(receipts.active_len(), 0);
    }

    // -----------------------------------------------------------------------
    // #1774: post-edit formatting integration.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_not_run_formatter_when_format_after_edit_disabled() {
        // OFF by default: even for a .rs file, the on-disk bytes must be
        // exactly what the edit produced and no formatting note may appear.
        let dir = tempfile::tempdir().unwrap();
        let ugly = "fn main(){let x=1;println!(\"{}\",x);}\n";
        std::fs::write(dir.path().join("code.rs"), ugly).unwrap();

        let tool = EditFileTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(!ctx.format_after_edit, "formatting must be OFF by default");

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "code.rs",
                    "old_string": "let x=1",
                    "new_string": "let x=2",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(!result.output.contains("reformatted"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("code.rs")).unwrap(),
            ugly.replace("let x=1", "let x=2"),
            "disabled formatting must leave the written bytes untouched"
        );
    }

    #[tokio::test]
    async fn should_return_formatted_content_when_format_after_edit_enabled() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main(){let x=1;println!(\"{}\",x);}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "code.rs",
                    "old_string": "let x=1",
                    "new_string": "let x=2",
                }),
            )
            .await
            .unwrap();
        assert!(result.success, "edit must succeed: {}", result.output);
        // The tool result must state the reformat and echo the REAL on-disk
        // content so the LLM's mental copy is not stale.
        assert!(
            result.output.contains("reformatted"),
            "output must state the file was reformatted: {}",
            result.output
        );
        assert!(
            result.output.contains("fn main() {"),
            "output must echo the formatted content: {}",
            result.output
        );
        let on_disk = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(
            on_disk.contains("fn main() {"),
            "file must be rustfmt-formatted on disk: {on_disk}"
        );
        assert!(
            on_disk.contains("let x = 2;"),
            "edit must survive: {on_disk}"
        );
    }

    #[tokio::test]
    async fn local_edit_formatter_metadata_uses_final_disk_content() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main(){let x=1;println!(\"{}\",x);}\n").unwrap();
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "code.rs",
                    "old_string": "let x=1",
                    "new_string": "let x=2",
                }),
            )
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        let metadata = result.structured_metadata.as_ref().unwrap();
        let status = metadata["formatter"]["status"].as_str().unwrap();
        if status == "timed_out" {
            eprintln!("skipping final formatter assertions: rustfmt timed out");
            return;
        }
        assert_eq!(status, "formatted");
        assert_eq!(metadata["formatter"]["changed"], true);
        assert_eq!(metadata["formatter_expanded_change"], true);
        assert_ne!(metadata["write_version"], metadata["final_version"]);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(
            metadata["final_version"]["content_sha256"],
            crate::file_state_cache::FileVersion::sha256(&on_disk)
        );
        assert!(result.output.contains("formatter=formatted"));
    }

    #[tokio::test]
    async fn local_edit_replace_all_reports_formatter_final_content() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch.rs");
        std::fs::write(&path, "fn main(){let x=1;let y=1;println!(\"{}\",x+y);}\n").unwrap();
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "batch.rs",
                    "old_string": "=1",
                    "new_string": "=2",
                    "replace_all": true,
                }),
            )
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        let metadata = result.structured_metadata.as_ref().unwrap();
        assert_eq!(metadata["replacement_count"], 2);
        let status = metadata["formatter"]["status"].as_str().unwrap();
        if status == "timed_out" {
            eprintln!("skipping final formatter assertions: rustfmt timed out");
            return;
        }
        assert_eq!(status, "formatted");
        assert_eq!(metadata["formatter"]["changed"], true);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(
            metadata["final_version"]["content_sha256"],
            crate::file_state_cache::FileVersion::sha256(&on_disk)
        );
        assert!(
            std::str::from_utf8(&on_disk)
                .unwrap()
                .contains("let x = 2;")
        );
        assert!(
            std::str::from_utf8(&on_disk)
                .unwrap()
                .contains("let y = 2;")
        );
    }

    #[tokio::test]
    async fn local_edit_formatter_failure_keeps_modified_result() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.rs");
        std::fs::write(&path, "fn main( { let a=1 \n").unwrap();
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = EditFileTool::new(dir.path())
            .with_local_edit_enabled(true)
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "broken.rs",
                    "old_string": "let a=1",
                    "new_string": "let a=2",
                }),
            )
            .await
            .unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(result.file_modified.as_deref(), Some(path.as_path()));
        let metadata = result.structured_metadata.as_ref().unwrap();
        let status = metadata["formatter"]["status"].as_str().unwrap();
        assert!(matches!(status, "failed" | "timed_out"));
        assert_eq!(metadata["final_state"], "confirmed");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "fn main( { let a=2 \n"
        );
    }

    #[tokio::test]
    async fn should_keep_edit_success_when_formatter_fails() {
        if !crate::format::binary_on_path("rustfmt") {
            eprintln!("skipping: rustfmt not on PATH");
            return;
        }
        // Syntactically broken Rust: the edit applies (plain string
        // replacement) but rustfmt exits non-zero. The edit must STAND and
        // the result must stay success=true with a formatting-failed note.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.rs"), "fn main( { let a=1 \n").unwrap();

        let tool = EditFileTool::new(dir.path());
        let mut ctx = ToolContext::zero();
        ctx.format_after_edit = true;

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "broken.rs",
                    "old_string": "let a=1",
                    "new_string": "let a=2",
                }),
            )
            .await
            .unwrap();
        assert!(
            result.success,
            "formatter failure must NOT fail the edit: {}",
            result.output
        );

        // This asserts rustfmt FAILED (non-zero on broken syntax), which needs
        // rustfmt to actually run. Post-edit formatting is best-effort behind a
        // hard 5s `format::FORMAT_TIMEOUT`, and a loaded runner can exceed that
        // just spawning it — `check-windows` did on run 30763300877. A timeout
        // is a legitimate outcome (`FormatOutcome::TimedOut`), and it reports
        // "timed out" rather than "failed", so the note assertion is only
        // meaningful when the formatter got to run and exit. The edit-preserved
        // check below holds either way and still runs.
        let formatter_ran = !result.output.contains("timed out");
        if !formatter_ran {
            eprintln!("skipping formatter-failure assertion: rustfmt exceeded FORMAT_TIMEOUT");
        } else {
            assert!(
                result.output.contains("failed"),
                "output must note the formatter failure: {}",
                result.output
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("broken.rs")).unwrap(),
            "fn main( { let a=2 \n",
            "the edit must be kept exactly as written when formatting fails"
        );
    }

    // -----------------------------------------------------------------------
    // #1976: per-path write-grant enforcement.
    // -----------------------------------------------------------------------

    fn fenced_tool(dir: &std::path::Path, patterns: &[&str], create_only: bool) -> EditFileTool {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        EditFileTool::new(dir).with_write_grant(
            crate::tools::write_grant::WritePathGrant::new(&owned, create_only)
                .expect("test grant compiles"),
        )
    }

    #[tokio::test]
    async fn write_grant_create_only_refuses_edit_even_on_allowlisted_path() {
        // Acceptance (#1976): under create_only, edit_file is refused on ANY
        // path — allowlisted or not ("created, never modified").
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exemplar.card"), "v1\n").unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], true);

        let result = tool
            .execute(&serde_json::json!({
                "path": "exemplar.card",
                "old_string": "v1",
                "new_string": "v2",
            }))
            .await
            .unwrap();
        assert!(!result.success, "create_only must refuse edits");
        assert!(
            result
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER),
            "typed refusal: {}",
            result.output
        );
        assert!(result.output.contains("create-only"), "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("exemplar.card")).unwrap(),
            "v1\n",
            "refused edit must leave the file untouched"
        );
    }

    #[tokio::test]
    async fn write_grant_edit_follows_allowlist_without_create_only() {
        // Without create_only: edits inside the allowlist pass, outside are
        // refused with the typed `[denied]` class.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exemplar.card"), "v1\n").unwrap();
        std::fs::write(dir.path().join("app.md"), "keep\n").unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], false);

        let ok = tool
            .execute(&serde_json::json!({
                "path": "exemplar.card",
                "old_string": "v1",
                "new_string": "v2",
            }))
            .await
            .unwrap();
        assert!(ok.success, "allowlisted edit must pass: {}", ok.output);

        let denied = tool
            .execute(&serde_json::json!({
                "path": "app.md",
                "old_string": "keep",
                "new_string": "gone",
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(
            denied
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER),
            "typed refusal: {}",
            denied.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.md")).unwrap(),
            "keep\n",
            "refused edit must leave the file untouched"
        );
    }
}
