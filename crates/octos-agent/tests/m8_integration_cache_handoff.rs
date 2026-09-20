//! Compatibility checks for the M8 cache/compaction hand-off after H02 M1.
//!
//! `FileStateCache` now stores disk versions only. Clearing or losing it may
//! cost another hash, but it cannot suppress file contents.

use std::sync::Arc;

use octos_agent::{
    ApiMicroCompactionConfig, FileMetadataHint, FileStateCache, FileTarget, FileVersion,
    MicroCompactionPolicy, TieredCompactionRunner,
    compaction::{CompactionOutcome, CompactionPhase},
    compaction_tiered::FullCompactor,
    tools::{ReadFileTool, Tool, ToolContext},
};
use octos_core::Message;

#[test]
fn tier3_compatibility_helper_can_clear_file_version_ledger() {
    let ledger = Arc::new(FileStateCache::new());
    ledger.record(FileVersion::from_bytes(
        FileTarget::new("workspace", "/repo/stale.rs"),
        None,
        b"content",
        FileMetadataHint::new(7, None, None, None, None),
    ));

    struct AlwaysCompact;
    impl FullCompactor for AlwaysCompact {
        fn needs_compaction(&self, _messages: &[Message]) -> Option<u32> {
            Some(0)
        }

        fn compact(
            &self,
            _messages: &mut Vec<Message>,
            _phase: CompactionPhase,
        ) -> CompactionOutcome {
            CompactionOutcome::default()
        }
    }

    let runner = TieredCompactionRunner::new(
        MicroCompactionPolicy::default(),
        ApiMicroCompactionConfig::default(),
        Box::new(AlwaysCompact),
    );
    let report = runner.run_tier3_and_invalidate_cache(
        &mut Vec::new(),
        CompactionPhase::OnDemand,
        Some(&ledger),
    );

    assert!(report.is_some());
    assert!(ledger.is_empty());
}

#[tokio::test]
async fn read_file_returns_body_and_rebuilds_version_after_ledger_clear() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("boundary.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let ledger = Arc::new(FileStateCache::new());
    let tool = ReadFileTool::new(workspace.path());
    let mut context = ToolContext::zero();
    context.tool_id = "h02-m1".to_owned();
    context.file_state_cache = Some(ledger.clone());

    let first = tool
        .execute_with_context(&context, &serde_json::json!({"path": "boundary.txt"}))
        .await
        .unwrap();
    assert!(first.output.contains("alpha"));
    assert_eq!(ledger.len(), 1);

    ledger.clear();
    let second = tool
        .execute_with_context(&context, &serde_json::json!({"path": "boundary.txt"}))
        .await
        .unwrap();

    assert!(second.output.contains("alpha"));
    assert!(!second.output.contains("[FILE_UNCHANGED]"));
    assert_eq!(ledger.len(), 1);
}
