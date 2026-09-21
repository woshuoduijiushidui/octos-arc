//! Diff-based file editing tool using unified diff format.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool for editing files via unified diff format with fuzzy matching.
pub struct DiffEditTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    file_access: FileAccessMode,
    local_edit_enabled: bool,
}

impl DiffEditTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            local_edit_enabled: false,
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
}

#[derive(Debug, Deserialize)]
struct DiffEditInput {
    path: String,
    diff: String,
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
            move |bytes| -> Result<_, String> {
                let content = std::str::from_utf8(bytes)
                    .map_err(|_| "File is not valid UTF-8 and cannot be edited".to_string())?;
                let new_content = apply_hunks(content, &hunks)
                    .map_err(|error| format!("Failed to apply diff: {error}"))?;
                Ok((new_content.as_bytes().to_vec(), new_content))
            },
        )
        .await;
        let guarded = match guarded {
            Ok(rewrite) => rewrite,
            Err(error) => return Ok(error.into_tool_result(self.name(), &input.path)),
        };
        let new_content = guarded.value;
        if !guarded.changed {
            let mut metadata = super::mutation_report::no_change_metadata(
                self.name(),
                &input.path,
                &guarded.before,
            );
            super::mutation_report::insert(&mut metadata, "matcher", json!("diff_hunks"));
            super::mutation_report::insert(&mut metadata, "hunk_count", json!(hunk_count));
            return Ok(ToolResult {
                output: format!(
                    "[no_change] path={} matcher=diff_hunks hunks={} current={}",
                    super::mutation_report::safe_path(&input.path),
                    hunk_count,
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
                "Applied {} hunk(s) to {}: final={}, formatter={}",
                hunk_count,
                super::mutation_report::safe_path(&input.path),
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

const FUZZY_RANGE: i64 = 3;

fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String> {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    // Apply hunks in reverse order so line numbers stay valid
    let mut sorted_hunks: Vec<(usize, &Hunk)> = hunks.iter().enumerate().collect();
    sorted_hunks.sort_by_key(|entry| std::cmp::Reverse(entry.1.old_start));

    // Check for overlapping hunks (sorted descending by old_start)
    for window in sorted_hunks.windows(2) {
        let (_, later_hunk) = window[0]; // higher line number
        let (_, earlier_hunk) = window[1]; // lower line number
        let earlier_end = earlier_hunk.old_start + pattern_lines(&earlier_hunk.lines).len();
        if earlier_end > later_hunk.old_start {
            eyre::bail!(
                "overlapping hunks at lines {} and {}",
                earlier_hunk.old_start,
                later_hunk.old_start
            );
        }
    }

    for (idx, hunk) in sorted_hunks {
        let context_lines = pattern_lines(&hunk.lines);

        if context_lines.is_empty() {
            eyre::bail!("hunk {} has no context or remove lines", idx + 1);
        }

        // Try exact position first, then fuzzy search
        let target = hunk.old_start.saturating_sub(1); // 1-indexed to 0-indexed
        let match_pos = find_match(&lines, &context_lines, target)?;

        // Apply the hunk at match_pos: replace the matched pattern block with
        // the replacement block.
        let remove_count = context_lines.len();
        let new_lines = replacement_lines(&hunk.lines);

        // Replace the matched region
        let end = (match_pos + remove_count).min(lines.len());
        lines.splice(match_pos..end, new_lines);
    }

    // Preserve trailing newline if original had one
    let mut result = lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

fn find_match(lines: &[String], pattern: &[&str], target: usize) -> Result<usize> {
    let start = target.saturating_sub(FUZZY_RANGE as usize);
    let end = target
        .saturating_add(FUZZY_RANGE as usize)
        .min(lines.len().saturating_sub(pattern.len()));
    let matches = (start..=end)
        .filter(|position| matches_at(lines, pattern, *position))
        .collect::<Vec<_>>();
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
