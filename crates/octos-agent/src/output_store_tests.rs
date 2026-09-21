use super::*;
use crate::output_recovery::{ExecutionStatus, OBSERVATION_ENTRIES, OutputPolicy, OutputState};

fn owner(branch: &str) -> ReadReceiptOwner {
    ReadReceiptOwner::new("workspace", "task", "session", branch).unwrap()
}

fn state(dir: &Path, branch: &str) -> OutputState {
    let state = OutputState::new(OutputPolicy { enabled: true }, owner(branch));
    state.enable_store(dir).unwrap();
    state
}

fn document(text: &str, start: u64) -> OutputDocument {
    OutputDocument {
        source: OutputSource::File {
            target: "/fixture".into(),
            sha256: digest(text.as_bytes()),
        },
        parts: vec![OutputPart {
            stream: OutputStream::File,
            text: text.into(),
            start,
            first_line: None,
            total: Some(start + text.len() as u64),
        }],
        capture: CaptureState::Complete,
        execution: ExecutionStatus::NotApplicable,
        transformed: false,
        loss_reason: None,
        file_read: None,
    }
}

fn save(state: &OutputState, text: &str, call: &str, start: u64) -> RenderedOutput {
    state
        .register(
            uuid::Uuid::new_v4().to_string(),
            call,
            &serde_json::json!({}),
            document(text, start),
            true,
            PAGE_BYTES,
        )
        .unwrap()
}

fn request(id: &str) -> RecallRequest {
    RecallRequest {
        output_id: Some(id.into()),
        ..Default::default()
    }
}

fn header(page: &RenderedOutput) -> serde_json::Value {
    serde_json::from_str(page.content.split_once('\n').unwrap().0).unwrap()
}

fn restore(store: &OutputStore, id: &str) -> String {
    let mut request = request(id);
    let mut text = String::new();
    let mut previous_end = None;
    for _ in 0..10_000 {
        let page = store.read(&request, PAGE_BYTES).unwrap();
        assert!(page.rendered.content.len() <= PAGE_BYTES);
        assert!(page.rendered.view.historical);
        let json = header(&page.rendered);
        let body = page.rendered.content.split_once('\n').unwrap().1;
        if let Some(range) = page.rendered.view.visible_ranges.first() {
            assert_eq!(range.end - range.start, body.len() as u64);
            if let Some(end) = previous_end {
                assert_eq!(range.start, end);
            }
            previous_end = Some(range.end);
        }
        text.push_str(body);
        if let Some(cursor) = json["recall"]["cursor"].as_str() {
            assert!(!body.is_empty());
            request = RecallRequest {
                cursor: Some(cursor.into()),
                ..Default::default()
            };
        } else {
            assert!(matches!(
                page.rendered.view.continuation,
                crate::output_recovery::Continuation::Eof
                    | crate::output_recovery::Continuation::SelectionEnd
                    | crate::output_recovery::Continuation::Unavailable
            ));
            return text;
        }
    }
    panic!("recovery did not terminate");
}

#[test]
fn h03_m2_hot_and_cold_unicode_ranges_reconstruct_exact_hash() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let text = format!("\u{feff}{}末尾无换行", "line 中文 🦀\r\n".repeat(12_000));
    let saved = save(&state, &text, "call_same", 91);
    assert!(saved.view.recoverable);
    let id = saved.view.output_id;
    let store = state.store().unwrap();
    let historical = store.read(&request(&id), PAGE_BYTES).unwrap();
    assert!(historical.rendered.view.historical);
    assert!(historical.document.file_read.is_none());
    assert_eq!(
        digest(restore(&store, &id).as_bytes()),
        digest(text.as_bytes())
    );
    let files = store.directory.files().unwrap();
    let cold_owner = store.owner.clone();
    drop(store);
    drop(state);
    let cold = OutputStore::open(dir.path(), cold_owner).unwrap();
    assert_eq!(restore(&cold, &id), text);
    assert_eq!(cold.directory.files().unwrap().len(), files.len());
}

#[test]
fn h03_m2_repeated_cursor_is_stable_and_never_writes_an_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let saved = save(&state, &"some output\n".repeat(10_000), "call", 0);
    let store = state.store().unwrap();
    let first = store
        .read(&request(&saved.view.output_id), PAGE_BYTES)
        .unwrap();
    let req = RecallRequest {
        cursor: Some(
            header(&first.rendered)["recall"]["cursor"]
                .as_str()
                .unwrap()
                .into(),
        ),
        ..Default::default()
    };
    let before = store.directory.read("index.json", CATALOG_BYTES).unwrap();
    let expected = store.read(&req, 4096).unwrap().rendered;
    for _ in 0..20 {
        assert_eq!(
            store.read(&req, 4096).unwrap().rendered.content,
            expected.content
        );
    }
    assert_eq!(
        before,
        store.directory.read("index.json", CATALOG_BYTES).unwrap()
    );
    assert_eq!(store.catalog().unwrap().records.len(), 1);
}

#[test]
fn h03_m2_duplicate_call_ids_and_fixed_legacy_pages_do_not_skip_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let text = (0..30_000)
        .map(|i| format!("line {i}\n"))
        .collect::<String>();
    let saved = save(&state, &text, "call_reused", 0);
    let store = state.store().unwrap();
    let old = RecallRequest {
        tool_call_id: Some("call_reused".into()),
        page: Some(1),
        ..Default::default()
    };
    let small = store.read(&old, 2048).unwrap().rendered;
    let large = store.read(&old, PAGE_BYTES).unwrap().rendered;
    assert_eq!(
        small.view.visible_ranges[0].start,
        large.view.visible_ranges[0].start
    );
    assert!(header(&small)["recall"]["cursor"].is_string());
    let mut terminal = small;
    while let Some(cursor) = header(&terminal)["recall"]["cursor"].as_str() {
        terminal = store
            .read(
                &RecallRequest {
                    cursor: Some(cursor.into()),
                    ..Default::default()
                },
                2048,
            )
            .unwrap()
            .rendered;
    }
    assert_eq!(
        header(&terminal)["recall"]["offset"],
        terminal.view.visible_ranges[0].end
    );
    assert_eq!(
        header(&terminal)["recall"]["output_id"],
        saved.view.output_id
    );
    let mut req = old.clone();
    req.page = Some(u64::MAX);
    assert_eq!(
        store.read(&req, PAGE_BYTES).unwrap_err(),
        OutputError::OutOfRange
    );
    save(&state, &text, "call_reused", 0);
    assert_eq!(
        store.read(&old, PAGE_BYTES).unwrap_err(),
        OutputError::AmbiguousCallId
    );
    assert_eq!(restore(&store, &saved.view.output_id), text);
}

#[test]
fn h03_m2_owner_task_branch_and_workspace_must_all_match() {
    let dir = tempfile::tempdir().unwrap();
    let original = state(dir.path(), "root");
    let saved = save(&original, "owned body", "call", 0);
    for other in [
        owner("child"),
        ReadReceiptOwner::new("workspace", "other-task", "session", "root").unwrap(),
        ReadReceiptOwner::new("workspace", "task", "other-session", "root").unwrap(),
        ReadReceiptOwner::new("other-workspace", "task", "session", "root").unwrap(),
    ] {
        let store = OutputStore::open(dir.path(), other).unwrap();
        assert_eq!(
            store
                .read(&request(&saved.view.output_id), PAGE_BYTES)
                .unwrap_err(),
            OutputError::OwnerMismatch
        );
    }
}

#[test]
fn h03_m2_invalid_cursor_eof_empty_and_argument_validation() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let saved = save(&state, "文🦀tail", "call", 0);
    let store = state.store().unwrap();
    for req in [
        RecallRequest {
            cursor: Some("broken".into()),
            ..Default::default()
        },
        RecallRequest {
            offset: Some(u64::MAX),
            limit: Some(1),
            ..request(&saved.view.output_id)
        },
        RecallRequest {
            offset: Some(1),
            ..request(&saved.view.output_id)
        },
        RecallRequest {
            limit: Some(1),
            ..request(&saved.view.output_id)
        },
        RecallRequest {
            tool_call_id: Some("call".into()),
            ..request(&saved.view.output_id)
        },
        RecallRequest {
            limit: Some(0),
            ..request(&saved.view.output_id)
        },
        RecallRequest {
            page: Some(1),
            ..request(&saved.view.output_id)
        },
    ] {
        assert!(store.read(&req, PAGE_BYTES).is_err(), "{req:?}");
    }
    let end = "文🦀tail".len() as u64;
    let eof = store
        .read(
            &RecallRequest {
                offset: Some(end),
                ..request(&saved.view.output_id)
            },
            PAGE_BYTES,
        )
        .unwrap();
    assert!(eof.rendered.view.visible_ranges.is_empty());
    assert_eq!(
        eof.rendered.view.continuation,
        crate::output_recovery::Continuation::Eof
    );
    let empty = save(&state, "", "empty", 0);
    assert_eq!(restore(&store, &empty.view.output_id), "");
    for value in [
        serde_json::json!({"output_id": saved.view.output_id, "offset": -1}),
        serde_json::json!({"output_id": saved.view.output_id, "start_line": 1}),
        serde_json::json!({"output_id": saved.view.output_id, "offset": 1.5}),
    ] {
        assert!(serde_json::from_value::<RecallRequest>(value).is_err());
    }
}

#[test]
fn h03_m2_corrupt_missing_and_unknown_schema_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let saved = save(&state, &"payload line\n".repeat(15_000), "call", 0);
    let store = state.store().unwrap();
    let mut catalog = store.catalog().unwrap();
    let record = &mut catalog.records[0];
    let mut manifest = store.manifest(record).unwrap();
    let blob = format!("{}.text", manifest.parts[0].sha256);
    let original = store.directory.read(&blob, OUTPUT_BYTES as u64).unwrap();
    let mut broken = original.clone();
    broken[BLOCK_BYTES + 12] ^= 1;
    store.directory.atomic(&blob, &broken).unwrap();
    // An untouched first block is still independently verifiable.
    assert!(
        store
            .read(&request(&saved.view.output_id), PAGE_BYTES)
            .is_ok()
    );
    assert_eq!(
        store
            .read(
                &RecallRequest {
                    offset: Some(BLOCK_BYTES as u64),
                    ..request(&saved.view.output_id)
                },
                PAGE_BYTES
            )
            .unwrap_err(),
        OutputError::Corrupt
    );
    assert_eq!(
        store.status(&saved.view.output_id).unwrap_err(),
        OutputError::Corrupt
    );
    store.directory.remove(&blob);
    assert_eq!(
        store.status(&saved.view.output_id).unwrap_err(),
        OutputError::Missing
    );
    store.directory.atomic(&blob, &original).unwrap();
    manifest.version = 99;
    let bytes = json(&manifest).unwrap();
    record.manifest = digest(&bytes);
    record.metadata_bytes = bytes.len() as u64;
    store
        .directory
        .atomic(&format!("{}.manifest", record.manifest), &bytes)
        .unwrap();
    store.commit(&catalog).unwrap();
    assert_eq!(
        store
            .read(&request(&saved.view.output_id), PAGE_BYTES)
            .unwrap_err(),
        OutputError::UnsupportedSchema
    );
}

#[test]
fn h03_m2_running_is_pending_then_finalized_and_restart_is_partial() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let mut doc = document("some partial token", 0);
    doc.capture = CaptureState::Running;
    doc.execution = ExecutionStatus::Running;
    doc.parts[0].total = None;
    let id = uuid::Uuid::new_v4().to_string();
    state
        .register(
            id.clone(),
            "call",
            &serde_json::json!({}),
            doc.clone(),
            true,
            PAGE_BYTES,
        )
        .unwrap();
    let store = state.store().unwrap();
    let page = store.read(&request(&id), PAGE_BYTES).unwrap();
    assert_eq!(
        page.rendered.view.continuation,
        crate::output_recovery::Continuation::Pending
    );
    assert!(page.rendered.view.visible_ranges.is_empty());
    assert!(!header(&page.rendered)["recall"]["cursor"].is_string());
    doc.capture = CaptureState::Complete;
    doc.execution = ExecutionStatus::NotApplicable;
    doc.parts[0].total = Some(doc.parts[0].text.len() as u64);
    state
        .register(
            id.clone(),
            "call",
            &serde_json::json!({}),
            doc.clone(),
            true,
            PAGE_BYTES,
        )
        .unwrap();
    assert_eq!(restore(&store, &id), doc.parts[0].text);
    let interrupted = uuid::Uuid::new_v4().to_string();
    doc.capture = CaptureState::Running;
    state
        .register(
            interrupted.clone(),
            "call2",
            &serde_json::json!({}),
            doc,
            true,
            PAGE_BYTES,
        )
        .unwrap();
    drop(store);
    drop(state);
    let cold = OutputStore::open(dir.path(), owner("root")).unwrap();
    let status = cold.status(&interrupted).unwrap();
    assert_eq!(status.capture, CaptureState::Partial);
    assert_eq!(status.execution, ExecutionStatus::Unknown);
    assert_eq!(status.loss_reason.as_deref(), Some("capture_interrupted"));
}

#[test]
fn h03_m2_unpublished_files_never_create_a_readable_complete_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path(), owner("root")).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    store
        .directory
        .atomic("unpublished.text", b"partial")
        .unwrap();
    store
        .directory
        .atomic("unpublished.manifest", b"{\"complete\":true}")
        .unwrap();
    let mut half = store.directory.file("crash.tmp", true, true).unwrap();
    half.write_all(b"half written").unwrap();
    drop(half);
    drop(store);
    let cold = OutputStore::open(dir.path(), owner("root")).unwrap();
    assert_eq!(cold.status(&id).unwrap_err(), OutputError::Missing);
    assert!(
        cold.directory
            .files()
            .unwrap()
            .iter()
            .all(|(name, _)| !name.starts_with("unpublished") && name != "crash.tmp")
    );
}

#[test]
fn h03_m2_long_secrets_across_blocks_and_pages_use_one_safe_view() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let secret = format!("sk-{}", "Zp_".repeat(40_000));
    let text = format!("{}{}\nend marker", "safe line\n".repeat(6553), secret);
    let saved = save(&state, &text, "call", 0);
    assert!(saved.view.transformed);
    let store = state.store().unwrap();
    let safe = restore(&store, &saved.view.output_id);
    assert_eq!(safe, crate::sanitize::sanitize_tool_output(&text));
    assert!(!safe.contains("Zp_Zp_"));
    for (name, _) in store.directory.files().unwrap() {
        if name.ends_with(".text") {
            let bytes = store.directory.read(&name, OUTPUT_BYTES as u64).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("Zp_Zp_"));
        }
    }
}

#[test]
fn h03_m2_per_stream_capacity_keeps_prefix_and_real_failure_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let stdout = "stdout line\n".repeat(STREAM_BYTES / 12 + 200);
    let doc = OutputDocument {
        source: OutputSource::Command {
            run_id: String::new(),
        },
        parts: vec![
            OutputPart {
                stream: OutputStream::Stdout,
                text: stdout.clone(),
                start: 0,
                first_line: None,
                total: Some(stdout.len() as u64),
            },
            OutputPart {
                stream: OutputStream::Stderr,
                text: "unique failure".into(),
                start: 0,
                first_line: None,
                total: Some(14),
            },
        ],
        capture: CaptureState::Complete,
        execution: ExecutionStatus::Exited {
            code: Some(7),
            signal: None,
        },
        transformed: false,
        loss_reason: None,
        file_read: None,
    };
    let id = uuid::Uuid::new_v4().to_string();
    let saved = state
        .register(
            id.clone(),
            "call",
            &serde_json::json!({}),
            doc,
            false,
            PAGE_BYTES,
        )
        .unwrap();
    assert!(saved.view.recoverable);
    assert_eq!(saved.view.capture, CaptureState::Partial);
    assert_eq!(saved.view.loss_reason.as_deref(), Some("storage_limit"));
    assert_eq!(saved.view.stored_ranges[0].end, STREAM_BYTES as u64);
    let store = state.store().unwrap();
    let error = store
        .read(
            &RecallRequest {
                stream: Some(OutputStream::Stderr),
                ..request(&id)
            },
            PAGE_BYTES,
        )
        .unwrap();
    assert!(error.rendered.content.contains("unique failure"));
    assert!(!error.rendered.view.success);
    assert_eq!(
        error.rendered.view.execution,
        ExecutionStatus::Exited {
            code: Some(7),
            signal: None
        }
    );
    assert_eq!(
        store
            .read(
                &RecallRequest {
                    offset: Some(STREAM_BYTES as u64 + 1),
                    ..request(&id)
                },
                PAGE_BYTES
            )
            .unwrap_err(),
        OutputError::OutOfRange
    );
}

#[test]
fn h03_m2_active_leases_prevent_cleanup_and_idle_records_expire() {
    let dir = tempfile::tempdir().unwrap();
    let first = state(dir.path(), "first");
    let saved = save(&first, "owned payload", "call", 0);
    let second = state(dir.path(), "second");
    let store = second.store().unwrap();
    let mut catalog = store.catalog().unwrap();
    catalog.records[0].updated = 0;
    store.commit(&catalog).unwrap();
    store.clean(&mut catalog, true).unwrap();
    assert_eq!(catalog.records.len(), 1);
    assert!(first.store().unwrap().status(&saved.view.output_id).is_ok());
    drop(first);
    store.clean(&mut catalog, false).unwrap();
    assert!(catalog.records.is_empty());
    assert_eq!(
        store.status(&saved.view.output_id).unwrap_err(),
        OutputError::Missing
    );
}

#[cfg(unix)]
#[test]
fn h03_m2_symlink_ancestors_leaf_and_hardlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join("context_ledgers")).unwrap();
    assert_eq!(
        OutputStore::open(dir.path(), owner("root")).unwrap_err(),
        OutputError::StorageFailed
    );
    std::fs::remove_file(dir.path().join("context_ledgers")).unwrap();
    let state = state(dir.path(), "root");
    let saved = save(&state, "owned body", "call", 0);
    let store = state.store().unwrap();
    let manifest = store
        .manifest(&store.catalog().unwrap().records[0])
        .unwrap();
    let blob = format!("{}.text", manifest.parts[0].sha256);
    store.directory.remove(&blob);
    let other = outside.path().join("other.txt");
    std::fs::write(&other, "owned body").unwrap();
    symlink(&other, store.directory.path.join(&blob)).unwrap();
    assert!(
        store
            .read(&request(&saved.view.output_id), PAGE_BYTES)
            .is_err()
    );
    std::fs::remove_file(store.directory.path.join(&blob)).unwrap();
    std::fs::hard_link(&other, store.directory.path.join(&blob)).unwrap();
    assert_eq!(
        store
            .read(&request(&saved.view.output_id), PAGE_BYTES)
            .unwrap_err(),
        OutputError::Corrupt
    );
}

#[test]
fn h03_m2_session_payload_and_manifest_limits_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let text = "normal log line\n".repeat(OUTPUT_BYTES / 16);
    assert_eq!(text.len(), OUTPUT_BYTES);
    for i in 0..4 {
        let saved = save(&state, &text, &format!("call{i}"), 0);
        assert!(saved.view.recoverable, "{:?}", saved.view.loss_reason);
        assert_eq!(saved.view.stored_bytes, OUTPUT_BYTES as u64);
    }
    let full = save(&state, "cannot fit", "full", 0);
    assert!(!full.view.recoverable);
    assert_eq!(full.view.availability, Availability::StoreFailed);
    assert_eq!(full.view.loss_reason.as_deref(), Some("storage_limit"));
    let store = state.store().unwrap();
    assert_eq!(store.catalog().unwrap().records.len(), 4);
    assert!(
        store
            .directory
            .files()
            .unwrap()
            .iter()
            .filter(|(name, _)| name.ends_with(".manifest"))
            .all(|(_, bytes)| *bytes <= MANIFEST_BYTES as u64)
    );
}

#[test]
fn h03_m2_session_index_and_running_capture_limits_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    for i in 0..SESSION_ENTRIES {
        assert!(
            save(&state, "payload", &format!("call{i}"), 0)
                .view
                .recoverable
        );
    }
    assert!(!save(&state, "payload", "over-limit", 0).view.recoverable);
    assert_eq!(
        state.store().unwrap().catalog().unwrap().records.len(),
        SESSION_ENTRIES
    );
    let other_dir = tempfile::tempdir().unwrap();
    let other = super::tests::state(other_dir.path(), "root");
    let mut doc = document("pending", 0);
    doc.capture = CaptureState::Running;
    for i in 0..9 {
        let result = other
            .register(
                uuid::Uuid::new_v4().to_string(),
                &format!("call{i}"),
                &serde_json::json!({}),
                doc.clone(),
                true,
                PAGE_BYTES,
            )
            .unwrap();
        assert_eq!(result.view.recoverable, i < 8);
    }
}

#[cfg(unix)]
#[test]
fn h03_m2_publish_failure_never_marks_payload_recoverable_and_retry_is_explicit() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let store = state.store().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "unchanged").unwrap();
    symlink(outside.path(), store.directory.path.join("index.json")).unwrap();
    let saved = save(&state, "payload", "call", 0);
    assert!(!saved.view.recoverable);
    assert_eq!(saved.view.availability, Availability::StoreFailed);
    assert_eq!(saved.view.loss_reason.as_deref(), Some("storage_failed"));
    assert_eq!(
        std::fs::read_to_string(outside.path()).unwrap(),
        "unchanged"
    );
    store.directory.remove("index.json");
    assert_eq!(
        store.status(&saved.view.output_id).unwrap_err(),
        OutputError::Missing
    );
}

#[test]
fn h03_m2_duplicate_output_id_cannot_change_immutable_content_or_cursor_version() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let saved = save(&state, &"payload line\n".repeat(1000), "call", 0);
    let store = state.store().unwrap();
    let before = store.directory.read("index.json", CATALOG_BYTES).unwrap();
    let exact = store
        .save(&saved.view, &document(&"payload line\n".repeat(1000), 0))
        .unwrap();
    assert_eq!(exact.output_id, saved.view.output_id);
    assert_eq!(
        store.directory.read("index.json", CATALOG_BYTES).unwrap(),
        before
    );
    assert_eq!(
        store
            .save(&saved.view, &document(&"changed line\n".repeat(1000), 0))
            .unwrap_err(),
        OutputError::StaleSource
    );
    let page = store
        .read(&request(&saved.view.output_id), PAGE_BYTES)
        .unwrap();
    let token = header(&page.rendered)["recall"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut cursor: Cursor =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&token).unwrap()).unwrap();
    cursor.revision = "wrong-version".into();
    let invalid = RecallRequest {
        cursor: Some(URL_SAFE_NO_PAD.encode(json(&cursor).unwrap())),
        ..Default::default()
    };
    assert_eq!(
        store.read(&invalid, PAGE_BYTES).unwrap_err(),
        OutputError::StaleSource
    );
    let mixed = RecallRequest {
        cursor: Some(token),
        offset: Some(0),
        ..Default::default()
    };
    assert_eq!(
        store.read(&mixed, PAGE_BYTES).unwrap_err(),
        OutputError::InvalidCursor
    );
}

fn restore_stream(store: &OutputStore, id: &str, stream: OutputStream) -> String {
    let mut request = RecallRequest {
        output_id: Some(id.into()),
        stream: Some(stream),
        ..Default::default()
    };
    let mut text = String::new();
    loop {
        let page = store.read(&request, PAGE_BYTES).unwrap();
        let range = page.rendered.view.visible_ranges[0].clone();
        let length = (range.end - range.start) as usize;
        text.push_str(&page.document.parts[0].text[..length]);
        let page_header = header(&page.rendered);
        let Some(cursor) = page_header["recall"]["cursor"].as_str() else {
            return text;
        };
        request = RecallRequest {
            cursor: Some(cursor.into()),
            ..Default::default()
        };
    }
}

#[test]
fn h03_m4_command_preview_marks_head_tail_gap_and_recall_restores_each_stream() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let stdout = format!("HEAD\n{}TAIL\n", "middle\n".repeat(40_000));
    let stderr = "unique failure on stderr\n";
    let document = OutputDocument {
        source: OutputSource::Command {
            run_id: String::new(),
        },
        parts: vec![
            OutputPart {
                stream: OutputStream::Stdout,
                text: stdout.clone(),
                start: 0,
                first_line: None,
                total: Some(stdout.len() as u64),
            },
            OutputPart {
                stream: OutputStream::Stderr,
                text: stderr.into(),
                start: 0,
                first_line: None,
                total: Some(stderr.len() as u64),
            },
        ],
        capture: CaptureState::Complete,
        execution: ExecutionStatus::Exited {
            code: Some(7),
            signal: None,
        },
        transformed: false,
        loss_reason: None,
        file_read: None,
    };
    let saved = state
        .register(
            uuid::Uuid::new_v4().to_string(),
            "command-call",
            &serde_json::json!({"command": "fixture"}),
            document,
            false,
            PAGE_BYTES,
        )
        .unwrap();

    assert!(saved.content.contains("HEAD"));
    assert!(saved.content.contains("TAIL"));
    assert!(
        saved
            .content
            .contains("omitted from this preview; use recall")
    );
    assert!(saved.content.contains("unique failure on stderr"));
    assert_eq!(
        saved.view.execution,
        ExecutionStatus::Exited {
            code: Some(7),
            signal: None,
        }
    );
    let stdout_ranges: Vec<_> = saved
        .view
        .visible_ranges
        .iter()
        .filter(|range| range.stream == OutputStream::Stdout)
        .collect();
    assert_eq!(stdout_ranges.len(), 2);
    assert!(stdout_ranges[0].end < stdout_ranges[1].start);
    assert!(matches!(
        &saved.view.continuation,
        crate::output_recovery::Continuation::Next { positions }
            if positions.contains(&(OutputStream::Stdout, stdout_ranges[0].end))
    ));
    let initial_header = header(&saved);
    assert_eq!(
        initial_header["recall"]["offset"].as_u64(),
        Some(stdout_ranges[0].end)
    );

    let store = state.store().unwrap();
    assert_eq!(
        restore_stream(&store, &saved.view.output_id, OutputStream::Stdout),
        stdout
    );
    assert_eq!(
        restore_stream(&store, &saved.view.output_id, OutputStream::Stderr),
        stderr
    );
}

#[cfg(unix)]
#[test]
fn h03_m4_command_store_failure_preserves_output_and_exit_status_without_false_reference() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let store = state.store().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    symlink(outside.path(), store.directory.path.join("index.json")).unwrap();
    let document = OutputDocument {
        source: OutputSource::Command {
            run_id: String::new(),
        },
        parts: vec![OutputPart {
            stream: OutputStream::Stdout,
            text: "captured before store failure\n".into(),
            start: 0,
            first_line: None,
            total: Some(30),
        }],
        capture: CaptureState::Complete,
        execution: ExecutionStatus::Exited {
            code: Some(9),
            signal: None,
        },
        transformed: false,
        loss_reason: None,
        file_read: None,
    };
    let saved = state
        .register(
            uuid::Uuid::new_v4().to_string(),
            "failed-store-command",
            &serde_json::json!({"command": "fixture"}),
            document,
            false,
            PAGE_BYTES,
        )
        .unwrap();

    assert!(!saved.view.recoverable);
    assert_eq!(saved.view.availability, Availability::StoreFailed);
    assert_eq!(saved.view.loss_reason.as_deref(), Some("storage_failed"));
    assert_eq!(
        saved.view.execution,
        ExecutionStatus::Exited {
            code: Some(9),
            signal: None,
        }
    );
    assert!(saved.content.contains("captured before store failure"));
}

#[test]
fn h03_m6_observes_bytes_recall_progress_and_repetition_without_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let marker = "M6_SECRET_PATH_AND_ARGUMENT";
    let text = format!("{marker}\n{}", "saved output\n".repeat(2_000));
    let output_id = uuid::Uuid::new_v4().to_string();
    let saved = state
        .register(
            output_id.clone(),
            "observed-call",
            &serde_json::json!({"path": marker, "command": marker}),
            document(&text, 0),
            true,
            PAGE_BYTES,
        )
        .unwrap();
    let request = request(&saved.view.output_id);
    let store = state.store().unwrap();

    for repeated in [false, true] {
        assert_eq!(state.observe_recall_request(&request), repeated);
        let recalled = store.read(&request, PAGE_BYTES).unwrap();
        let rendered = state.register_recalled(
            "recall-call",
            &serde_json::json!({"output_id": output_id}),
            recalled,
        );
        state.observe_recall_result(&rendered, repeated);
        assert!(rendered.is_ok());
    }

    let observations = state.observations();
    let capture = observations
        .iter()
        .find(|event| event.operation == "capture" && event.outcome == "success")
        .unwrap();
    assert_eq!(capture.captured_bytes, text.len() as u64);
    assert!(!capture.unknown_total);
    let saved_event = observations
        .iter()
        .find(|event| event.operation == "save" && event.outcome == "success")
        .unwrap();
    assert_eq!(saved_event.stored_bytes, text.len() as u64);
    assert_eq!(saved_event.known_omitted_bytes, 0);
    assert_eq!(saved_event.artifact, Some("available"));
    let initial_render = observations
        .iter()
        .find(|event| event.operation == "render" && event.layer == "initial")
        .unwrap();
    assert_eq!(initial_render.reason, "output_budget");
    assert!(initial_render.visible_bytes < initial_render.captured_bytes);
    let recall_results: Vec<_> = observations
        .iter()
        .filter(|event| event.operation == "recall" && event.layer == "result")
        .collect();
    assert_eq!(recall_results.len(), 2);
    assert_eq!(recall_results[0].reason, "strict_progress");
    assert!(!recall_results[0].repeated);
    assert!(recall_results[1].repeated);
    assert!(recall_results.iter().all(|event| {
        event
            .range
            .is_some_and(|(start, end)| end > start && event.visible_bytes == end - start)
    }));

    let diagnostic = format!("{observations:?}");
    assert!(!diagnostic.contains(marker));
    assert!(!diagnostic.contains(&saved.view.output_id));
    assert!(!diagnostic.contains("observed-call"));
}

#[test]
fn h03_m6_counts_one_terminal_command_and_bounds_observation_memory() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let command = || OutputDocument {
        source: OutputSource::Command {
            run_id: String::new(),
        },
        parts: vec![OutputPart {
            stream: OutputStream::Stdout,
            text: "bounded command output".into(),
            start: 0,
            first_line: None,
            total: Some(22),
        }],
        capture: CaptureState::Complete,
        execution: ExecutionStatus::TimedOut,
        transformed: false,
        loss_reason: None,
        file_read: None,
    };
    let command_id = uuid::Uuid::new_v4().to_string();
    for _ in 0..2 {
        state
            .register(
                command_id.clone(),
                "command-call",
                &serde_json::json!({"command": "not-recorded"}),
                command(),
                false,
                PAGE_BYTES,
            )
            .unwrap();
    }
    let command_events: Vec<_> = state
        .observations()
        .into_iter()
        .filter(|event| event.operation == "command")
        .collect();
    assert_eq!(command_events.len(), 1);
    assert_eq!(command_events[0].termination, Some("timed_out"));
    assert_eq!(command_events[0].artifact, Some("available"));

    state.observe_recall_result(&Err(OutputError::StaleSource), false);
    assert!(
        state
            .observations()
            .iter()
            .any(|event| event.operation == "recall" && event.reason == "stale_source")
    );
    for _ in 0..(OBSERVATION_ENTRIES * 2) {
        state.observe_recall_result(&Err(OutputError::Missing), false);
    }
    let observations = state.observations();
    assert_eq!(observations.len(), OBSERVATION_ENTRIES);
    assert!(
        observations
            .iter()
            .all(|event| event.operation == "recall" && event.reason == "missing")
    );
}

#[test]
fn h03_m6_transformed_coordinates_report_unknown_instead_of_false_omission() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), "root");
    let secret = format!("sk-{}", "observabilitysecret".repeat(20));
    let saved = save(
        &state,
        &format!("before {secret} after"),
        "sanitized-call",
        0,
    );
    assert!(saved.view.transformed);

    let observations = state.observations();
    for event in observations
        .iter()
        .filter(|event| matches!(event.operation, "save" | "render"))
    {
        assert!(event.unknown_total);
        assert_eq!(event.known_omitted_bytes, 0);
    }
    assert!(
        observations
            .iter()
            .any(|event| event.operation == "render" && event.reason == "source_transformed")
    );
    assert!(!format!("{observations:?}").contains(&secret));
}
