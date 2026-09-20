use std::sync::Arc;

use octos_agent::{
    FileMetadataHint, FileStateCache, FileTarget, FileVersion, ReadFileTool, Tool,
    tools::ToolContext,
};

fn context_with_ledger(ledger: Arc<FileStateCache>) -> ToolContext {
    let mut context = ToolContext::zero();
    context.tool_id = "h02-m1-read".to_owned();
    context.file_state_cache = Some(ledger);
    context
}

#[tokio::test]
async fn repeated_reads_return_body_while_m1_dedup_is_disabled() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("stable.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let ledger = Arc::new(FileStateCache::new());
    let context = context_with_ledger(ledger.clone());
    let tool = ReadFileTool::new(workspace.path());

    let first = tool
        .execute_with_context(&context, &serde_json::json!({"path": "stable.txt"}))
        .await
        .unwrap();
    let target = FileTarget::for_local_workspace(workspace.path(), &path).unwrap();
    let first_version = ledger.get(&target).expect("first version recorded");
    let second = tool
        .execute_with_context(&context, &serde_json::json!({"path": "stable.txt"}))
        .await
        .unwrap();
    let second_version = ledger.get(&target).expect("second version recorded");

    assert!(first.success && second.success);
    assert!(first.output.contains("alpha"));
    assert!(second.output.contains("alpha"));
    assert!(!second.output.contains("[FILE_UNCHANGED]"));
    assert_eq!(first_version, second_version);
    assert_eq!(
        second_version.content_sha256(),
        FileVersion::sha256(b"alpha\nbeta\n")
    );
}

#[tokio::test]
async fn same_size_rewrite_with_restored_mtime_changes_strong_version() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("changed.txt");
    std::fs::write(&path, "AAAA\n").unwrap();
    let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let ledger = Arc::new(FileStateCache::new());
    let context = context_with_ledger(ledger.clone());
    let tool = ReadFileTool::new(workspace.path());
    let target = FileTarget::for_local_workspace(workspace.path(), &path).unwrap();

    let first = tool
        .execute_with_context(&context, &serde_json::json!({"path": "changed.txt"}))
        .await
        .unwrap();
    assert!(first.output.contains("AAAA"));
    let first_version = ledger.get(&target).expect("first version recorded");

    std::fs::write(&path, "BBBB\n").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let second = tool
        .execute_with_context(&context, &serde_json::json!({"path": "changed.txt"}))
        .await
        .unwrap();
    assert!(second.output.contains("BBBB"));
    assert!(!second.output.contains("[FILE_UNCHANGED]"));
    let second_version = ledger.get(&target).expect("new version recorded");

    assert_eq!(first_version.size(), second_version.size());
    assert_eq!(
        first_version.metadata_hint().mtime_ns(),
        second_version.metadata_hint().mtime_ns(),
        "fixture restores the original mtime"
    );
    assert_ne!(
        first_version.content_sha256(),
        second_version.content_sha256()
    );
}

#[test]
fn same_target_path_is_isolated_by_workspace_owner() {
    let ledger = FileStateCache::new();
    let path = std::path::PathBuf::from("/shared/name.txt");
    let metadata = FileMetadataHint::new(4, None, None, None, None);
    let target_a = FileTarget::new("workspace-a", path.clone());
    let target_b = FileTarget::new("workspace-b", path);

    ledger.record(FileVersion::from_bytes(
        target_a.clone(),
        None,
        b"aaaa",
        metadata.clone(),
    ));
    ledger.record(FileVersion::from_bytes(
        target_b.clone(),
        None,
        b"bbbb",
        metadata,
    ));

    assert_eq!(ledger.len(), 2);
    assert_ne!(
        ledger.get(&target_a).unwrap().content_sha256(),
        ledger.get(&target_b).unwrap().content_sha256()
    );
}
