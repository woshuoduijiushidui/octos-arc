//! Shared local-provider freshness checks for built-in file mutations.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde_json::json;

use super::{ToolContext, ToolResult};
use crate::file_state_cache::{
    FileMetadataHint, FileMutationClaim, FileTarget, FileVersion, MutationClaimError,
};
use crate::model_read_receipts::ReceiptRevokeReason;

pub(crate) const STALE_MUTATION_CODE: &str = "stale_file_version";

#[derive(Clone, Copy)]
pub(crate) enum ExpectedVersionPolicy {
    /// An enabled ledger must contain a prior stable observation.
    RequireWhenTracked,
    /// Current patch context can authorize the edit when no version exists.
    Optional,
    /// [`Optional`] plus byte-identical candidates return without writing.
    OptionalNoChange,
    /// [`RequireWhenTracked`] plus byte-identical candidates return without writing.
    RequireWhenTrackedNoChange,
    /// [`OptionalNoChange`] bound to an independently authorized descriptor epoch.
    OptionalNoChangeAtEpoch(super::read_window::ViewEpoch),
    /// [`RequireWhenTrackedNoChange`] bound to an authorized descriptor epoch.
    RequireWhenTrackedNoChangeAtEpoch(super::read_window::ViewEpoch),
}

impl ExpectedVersionPolicy {
    pub(crate) fn no_change(
        require_when_tracked: bool,
        expected_epoch: Option<super::read_window::ViewEpoch>,
    ) -> Self {
        match (require_when_tracked, expected_epoch) {
            (false, None) => Self::OptionalNoChange,
            (true, None) => Self::RequireWhenTrackedNoChange,
            (false, Some(epoch)) => Self::OptionalNoChangeAtEpoch(epoch),
            (true, Some(epoch)) => Self::RequireWhenTrackedNoChangeAtEpoch(epoch),
        }
    }

    fn require_when_tracked(self) -> bool {
        matches!(
            self,
            Self::RequireWhenTracked
                | Self::RequireWhenTrackedNoChange
                | Self::RequireWhenTrackedNoChangeAtEpoch(_)
        )
    }

    fn detect_no_change(self) -> bool {
        matches!(
            self,
            Self::OptionalNoChange
                | Self::RequireWhenTrackedNoChange
                | Self::OptionalNoChangeAtEpoch(_)
                | Self::RequireWhenTrackedNoChangeAtEpoch(_)
        )
    }

    fn expected_epoch(self) -> Option<super::read_window::ViewEpoch> {
        match self {
            Self::OptionalNoChangeAtEpoch(epoch)
            | Self::RequireWhenTrackedNoChangeAtEpoch(epoch) => Some(epoch),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct GuardedRewrite<T> {
    pub value: T,
    pub before: Vec<u8>,
    pub written: Vec<u8>,
    pub changed: bool,
}

#[derive(Debug)]
pub(crate) struct StaleMutation {
    reason: &'static str,
    expected: Option<VersionSummary>,
    current: Option<VersionSummary>,
}

#[derive(Debug)]
pub(crate) enum GuardedMutationError {
    Stale(StaleMutation),
    Rejected(Box<MutationRejection>),
    Io {
        error: std::io::Error,
        may_have_modified: bool,
    },
}

pub(crate) enum MutationTransformError {
    Rejected(Box<MutationRejection>),
    StaleContext,
}

#[derive(Debug)]
pub(crate) struct MutationRejection {
    output: String,
    output_document: Option<crate::output_recovery::OutputDocument>,
    structured_metadata: Option<serde_json::Value>,
}

impl MutationRejection {
    pub(crate) fn new(result: ToolResult) -> Box<Self> {
        Box::new(Self {
            output: result.output,
            output_document: result.output_document,
            structured_metadata: result.structured_metadata,
        })
    }

    fn into_tool_result(self: Box<Self>) -> ToolResult {
        ToolResult {
            output: self.output,
            output_document: self.output_document,
            success: false,
            structured_metadata: self.structured_metadata,
            ..Default::default()
        }
    }
}

impl From<String> for MutationTransformError {
    fn from(message: String) -> Self {
        Self::Rejected(Box::new(MutationRejection {
            output: message,
            output_document: None,
            structured_metadata: None,
        }))
    }
}

impl GuardedMutationError {
    pub(crate) fn into_tool_result(self, tool: &str, display_path: &str) -> ToolResult {
        match self {
            Self::Stale(stale) => stale_result(stale, tool, display_path),
            Self::Rejected(rejection) => rejection.into_tool_result(),
            Self::Io {
                error,
                may_have_modified,
            } => {
                let mut result = super::file_io_error(error, display_path);
                if may_have_modified {
                    result.file_modified = Some(PathBuf::from(display_path));
                }
                result
            }
        }
    }
}

fn stale_result(stale: StaleMutation, tool: &str, display_path: &str) -> ToolResult {
    metrics::counter!(
        "octos_stale_mutations_total",
        "tool" => tool.to_string(),
        "reason" => stale.reason.to_string(),
    )
    .increment(1);
    let shown_path = if Path::new(display_path).is_absolute() {
        Path::new(display_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<file>")
            .to_string()
    } else {
        octos_core::truncated_utf8(display_path, 200, "...")
    };
    let expected_text = stale
        .expected
        .as_ref()
        .map(VersionSummary::display)
        .unwrap_or_else(|| "not observed".to_string());
    let current_text = stale
        .current
        .as_ref()
        .map(VersionSummary::display)
        .unwrap_or_else(|| "unavailable".to_string());
    ToolResult {
        output: format!(
            "[{STALE_MUTATION_CODE}] {tool} refused for {shown_path}: the file changed or \
             another mutation consumed the observed version (reason: {}). Expected \
             {expected_text}; current {current_text}. Re-read it with read_file, review the \
             current content, then retry with a new edit.",
            stale.reason,
        ),
        success: false,
        structured_metadata: Some(json!({
            "error_code": STALE_MUTATION_CODE,
            "reason": stale.reason,
            "expected_version": stale.expected.as_ref().map(VersionSummary::json),
            "current_version": stale.current.as_ref().map(VersionSummary::json),
            "remedy": "reread_review_retry",
        })),
        ..Default::default()
    }
}

#[derive(Clone, Debug)]
struct VersionSummary {
    content_sha256: String,
    size: u64,
}

impl VersionSummary {
    fn from_version(version: &FileVersion) -> Self {
        Self {
            content_sha256: version.content_sha256().to_string(),
            size: version.size(),
        }
    }

    fn display(&self) -> String {
        let digest = self
            .content_sha256
            .strip_prefix("sha256:")
            .unwrap_or(&self.content_sha256);
        let short = &digest[..digest.len().min(12)];
        format!("sha256:{short}... ({} bytes)", self.size)
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "content_sha256": self.content_sha256,
            "size": self.size,
        })
    }
}

struct MutationAuthorization {
    target: FileTarget,
    expected: Option<FileVersion>,
    claim: Option<FileMutationClaim>,
    ledger: Option<Arc<crate::file_state_cache::FileStateCache>>,
    blocked_reason: Option<&'static str>,
    enforce_expected: bool,
}

impl MutationAuthorization {
    fn prepare(
        ctx: &ToolContext,
        workspace_root: &Path,
        path: &Path,
        policy: ExpectedVersionPolicy,
        explicit_expected: Option<FileVersion>,
    ) -> Result<Self, GuardedMutationError> {
        let target = FileTarget::for_local_workspace(workspace_root, path).map_err(|error| {
            GuardedMutationError::Io {
                error,
                may_have_modified: false,
            }
        })?;
        if let Some(expected) = explicit_expected {
            return Ok(Self {
                target,
                expected: Some(expected),
                claim: None,
                ledger: ctx.file_state_cache.clone(),
                blocked_reason: None,
                enforce_expected: true,
            });
        }

        let Some(ledger) = ctx.file_state_cache.clone() else {
            return Ok(Self {
                target,
                expected: None,
                claim: None,
                ledger: None,
                blocked_reason: None,
                enforce_expected: false,
            });
        };
        match ledger.claim_mutation(&target) {
            Ok(claim) => Ok(Self {
                target,
                expected: Some(claim.expected().clone()),
                claim: Some(claim),
                ledger: Some(ledger),
                blocked_reason: None,
                enforce_expected: policy.require_when_tracked(),
            }),
            Err(MutationClaimError::MissingExpectedVersion) => Ok(Self {
                target,
                expected: None,
                claim: None,
                ledger: Some(ledger),
                blocked_reason: policy
                    .require_when_tracked()
                    .then_some("missing_expected_version"),
                enforce_expected: false,
            }),
            Err(MutationClaimError::AlreadyClaimed) => {
                let expected = ledger.peek(&target);
                Ok(Self {
                    target,
                    expected,
                    claim: None,
                    ledger: Some(ledger),
                    blocked_reason: Some("concurrent_mutation"),
                    enforce_expected: false,
                })
            }
        }
    }

    fn stale(
        mut self,
        reason: &'static str,
        current: Option<&FileVersion>,
    ) -> GuardedMutationError {
        if let Some(claim) = self.claim.take() {
            claim.invalidate();
        } else if let Some(ledger) = self.ledger.as_ref() {
            ledger.invalidate_target(&self.target);
        }
        GuardedMutationError::Stale(StaleMutation {
            reason,
            expected: self.expected.as_ref().map(VersionSummary::from_version),
            current: current.map(VersionSummary::from_version),
        })
    }

    fn success(mut self, ctx: &ToolContext) {
        if let Some(claim) = self.claim.take() {
            claim.invalidate();
        }
        complete_target_mutation(ctx, &self.target);
    }
}

pub(crate) fn complete_mutation(ctx: &ToolContext, workspace_root: &Path, path: &Path) {
    if let Ok(target) = FileTarget::for_local_workspace(workspace_root, path) {
        complete_target_mutation(ctx, &target);
    } else if let Some(ledger) = ctx.file_state_cache.as_ref() {
        ledger.invalidate_path(path);
    }
}

fn complete_target_mutation(ctx: &ToolContext, target: &FileTarget) {
    if let Some(ledger) = ctx.file_state_cache.as_ref() {
        ledger.invalidate_target(target);
    }
    if let Some(receipts) = ctx.model_read_receipts.as_ref() {
        receipts.revoke_target(target, ReceiptRevokeReason::Mutation);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MutationLockKey {
    workspace_root: PathBuf,
    target: PathBuf,
}

fn target_lock(workspace_root: &Path, path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<MutationLockKey, Weak<Mutex<()>>>>> = OnceLock::new();
    let key = MutationLockKey {
        workspace_root: dunce::canonicalize(workspace_root)
            .unwrap_or_else(|_| workspace_root.to_path_buf()),
        target: dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    };
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

pub(crate) async fn observe_existing(
    path: &Path,
    workspace_root: &Path,
) -> Result<(Vec<u8>, FileVersion), GuardedMutationError> {
    let path = path.to_path_buf();
    let workspace_root = workspace_root.to_path_buf();
    tokio::task::spawn_blocking(
        move || -> Result<(Vec<u8>, FileVersion), GuardedMutationError> {
            for _ in 0..2 {
                let mut file = open_local(&path, &workspace_root, false).map_err(|error| {
                    GuardedMutationError::Io {
                        error,
                        may_have_modified: false,
                    }
                })?;
                match read_current(&mut file, &path, &workspace_root) {
                    Ok(observation) => return Ok(observation),
                    Err(ObservationError::Concurrent) => continue,
                    Err(ObservationError::Io(error)) => {
                        return Err(GuardedMutationError::Io {
                            error,
                            may_have_modified: false,
                        });
                    }
                }
            }
            Err(GuardedMutationError::Stale(StaleMutation {
                reason: "concurrent_change",
                expected: None,
                current: None,
            }))
        },
    )
    .await
    .unwrap_or_else(|error| {
        Err(GuardedMutationError::Io {
            error: std::io::Error::other(error),
            may_have_modified: false,
        })
    })
}

pub(crate) async fn rewrite_existing<T, F, E>(
    ctx: &ToolContext,
    workspace_root: &Path,
    path: &Path,
    policy: ExpectedVersionPolicy,
    explicit_expected: Option<FileVersion>,
    opened_file: Option<File>,
    transform: F,
) -> Result<GuardedRewrite<T>, GuardedMutationError>
where
    T: Send + 'static,
    F: FnOnce(&[u8]) -> Result<(Vec<u8>, T), E> + Send + 'static,
    E: Into<MutationTransformError> + Send + 'static,
{
    let authorization =
        MutationAuthorization::prepare(ctx, workspace_root, path, policy, explicit_expected)?;
    let expected = authorization.expected.clone();
    let blocked_reason = authorization.blocked_reason;
    let enforce_expected = authorization.enforce_expected;
    let detect_no_change = policy.detect_no_change();
    let expected_epoch = policy.expected_epoch();
    let path = path.to_path_buf();
    let workspace_root = workspace_root.to_path_buf();
    let lock = target_lock(&workspace_root, &path);
    let outcome =
        tokio::task::spawn_blocking(move || -> Result<RewriteOutcome<T>, RewriteFailure> {
            let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
            let mut file = match opened_file {
                Some(file) => file,
                None => open_local(&path, &workspace_root, true)?,
            };
            let (bytes, current) = match read_current(&mut file, &path, &workspace_root) {
                Ok(observation) => observation,
                Err(ObservationError::Concurrent) => {
                    return Ok(RewriteOutcome::Stale {
                        reason: "concurrent_change",
                        current: None,
                    });
                }
                Err(ObservationError::Io(error)) => {
                    return Err(RewriteFailure::IoBeforeWrite(error));
                }
            };
            if let Some(expected_epoch) = expected_epoch {
                let found_epoch = file
                    .metadata()
                    .ok()
                    .and_then(|metadata| super::read_window::ViewEpoch::from_metadata(&metadata));
                if found_epoch != Some(expected_epoch) {
                    return Ok(RewriteOutcome::Stale {
                        reason: "version_mismatch",
                        current: Some(Box::new(current)),
                    });
                }
            }
            if let Some(reason) = blocked_reason {
                return Ok(RewriteOutcome::Stale {
                    reason,
                    current: Some(Box::new(current)),
                });
            }
            if enforce_expected
                && expected
                    .as_ref()
                    .is_some_and(|expected| expected != &current)
            {
                return Ok(RewriteOutcome::Stale {
                    reason: "version_mismatch",
                    current: Some(Box::new(current)),
                });
            }
            let (new_bytes, value) = match transform(&bytes).map_err(Into::into) {
                Ok(transformed) => transformed,
                Err(MutationTransformError::Rejected(rejection)) => {
                    let (_, after_rejection) = match read_current(&mut file, &path, &workspace_root)
                    {
                        Ok(observation) => observation,
                        Err(ObservationError::Concurrent) => {
                            return Ok(RewriteOutcome::Stale {
                                reason: "concurrent_change",
                                current: None,
                            });
                        }
                        Err(ObservationError::Io(error)) => {
                            return Err(RewriteFailure::IoBeforeWrite(error));
                        }
                    };
                    if after_rejection != current {
                        return Ok(RewriteOutcome::Stale {
                            reason: "concurrent_change",
                            current: Some(Box::new(after_rejection)),
                        });
                    }
                    return Err(RewriteFailure::Rejected(rejection));
                }
                Err(MutationTransformError::StaleContext) => {
                    return Ok(RewriteOutcome::Stale {
                        reason: "context_changed",
                        current: Some(Box::new(current)),
                    });
                }
            };

            let (_, before_write) = match read_current(&mut file, &path, &workspace_root) {
                Ok(observation) => observation,
                Err(ObservationError::Concurrent) => {
                    return Ok(RewriteOutcome::Stale {
                        reason: "concurrent_change",
                        current: None,
                    });
                }
                Err(ObservationError::Io(error)) => return Err(error.into()),
            };
            if before_write != current {
                return Ok(RewriteOutcome::Stale {
                    reason: "concurrent_change",
                    current: Some(Box::new(before_write)),
                });
            }
            if detect_no_change && new_bytes == bytes {
                return Ok(RewriteOutcome::Unchanged { value, bytes });
            }

            file.seek(SeekFrom::Start(0))
                .map_err(RewriteFailure::IoBeforeWrite)?;
            file.set_len(0).map_err(RewriteFailure::IoAfterWrite)?;
            file.write_all(&new_bytes)
                .map_err(RewriteFailure::IoAfterWrite)?;
            file.flush().map_err(RewriteFailure::IoAfterWrite)?;
            if !descriptor_still_at_path(&file, &path) {
                return Err(RewriteFailure::IoAfterWrite(std::io::Error::other(
                    "target changed after write",
                )));
            }
            let (written, _) =
                read_current(&mut file, &path, &workspace_root).map_err(|error| {
                    let error = match error {
                        ObservationError::Concurrent => {
                            std::io::Error::other("file changed while verifying completed write")
                        }
                        ObservationError::Io(error) => error,
                    };
                    RewriteFailure::IoAfterWrite(error)
                })?;
            if written != new_bytes {
                return Err(RewriteFailure::IoAfterWrite(std::io::Error::other(
                    "completed write could not be verified",
                )));
            }
            Ok(RewriteOutcome::Written {
                value,
                before: bytes,
                written,
            })
        })
        .await
        .unwrap_or_else(|error| Err(RewriteFailure::IoBeforeWrite(std::io::Error::other(error))));

    match outcome {
        Ok(RewriteOutcome::Written {
            value,
            before,
            written,
        }) => {
            authorization.success(ctx);
            Ok(GuardedRewrite {
                value,
                before,
                written,
                changed: true,
            })
        }
        Ok(RewriteOutcome::Unchanged { value, bytes }) => {
            drop(authorization);
            Ok(GuardedRewrite {
                value,
                before: bytes.clone(),
                written: bytes,
                changed: false,
            })
        }
        Ok(RewriteOutcome::Stale { reason, current }) => {
            Err(authorization.stale(reason, current.as_deref()))
        }
        Err(RewriteFailure::Rejected(message)) => Err(GuardedMutationError::Rejected(message)),
        Err(RewriteFailure::IoBeforeWrite(error))
            if authorization.expected.is_some() && is_target_replaced_error(&error) =>
        {
            Err(authorization.stale("target_replaced", None))
        }
        Err(RewriteFailure::IoBeforeWrite(error)) => Err(GuardedMutationError::Io {
            error,
            may_have_modified: false,
        }),
        Err(RewriteFailure::IoAfterWrite(error)) => {
            authorization.success(ctx);
            Err(GuardedMutationError::Io {
                error,
                may_have_modified: true,
            })
        }
    }
}

pub(crate) async fn create_new(
    ctx: &ToolContext,
    workspace_root: &Path,
    path: &Path,
    content: Vec<u8>,
    confined_rel: Option<PathBuf>,
) -> Result<FileTarget, GuardedMutationError> {
    let target = FileTarget::for_local_workspace(workspace_root, path).map_err(|error| {
        GuardedMutationError::Io {
            error,
            may_have_modified: false,
        }
    })?;
    if let Some(expected) = ctx
        .file_state_cache
        .as_ref()
        .and_then(|ledger| ledger.peek(&target))
    {
        if let Some(ledger) = ctx.file_state_cache.as_ref() {
            ledger.invalidate_target(&target);
        }
        return Err(GuardedMutationError::Stale(StaleMutation {
            reason: "target_disappeared",
            expected: Some(VersionSummary::from_version(&expected)),
            current: None,
        }));
    }
    let path = path.to_path_buf();
    let workspace_root = workspace_root.to_path_buf();
    let lock = target_lock(&workspace_root, &path);
    let outcome = tokio::task::spawn_blocking(move || {
        let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
        let opened = if let Some(rel) = confined_rel {
            super::write_grant::open_confined(
                &workspace_root,
                &rel,
                super::write_grant::ConfinedLeaf::CreateNew,
            )
        } else if let Some(rel) = super::write_grant::workspace_relative(&workspace_root, &path) {
            super::write_grant::open_confined(
                &workspace_root,
                &rel,
                super::write_grant::ConfinedLeaf::CreateNew,
            )
        } else {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            #[cfg(not(unix))]
            if path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_symlink())
            {
                return Err(CreateFailure::Stale(None));
            }
            options.open(&path)
        };
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let current = observe_path_once(&path, &workspace_root)
                    .ok()
                    .map(|(_, version)| Box::new(version));
                return Err(CreateFailure::Stale(current));
            }
            Err(error) => return Err(CreateFailure::Io(error, false)),
        };
        file.write_all(&content)
            .map_err(|error| CreateFailure::Io(error, true))?;
        file.flush()
            .map_err(|error| CreateFailure::Io(error, true))?;
        if !descriptor_still_at_path(&file, &path) {
            let current = observe_path_once(&path, &workspace_root)
                .ok()
                .map(|(_, version)| Box::new(version));
            return Err(CreateFailure::Stale(current));
        }
        FileTarget::for_local_workspace(&workspace_root, &path)
            .map_err(|error| CreateFailure::Io(error, true))
    })
    .await
    .unwrap_or_else(|error| Err(CreateFailure::Io(std::io::Error::other(error), false)));

    match outcome {
        Ok(created_target) => {
            complete_target_mutation(ctx, &created_target);
            Ok(created_target)
        }
        Err(CreateFailure::Stale(current)) => {
            if let Some(ledger) = ctx.file_state_cache.as_ref() {
                ledger.invalidate_target(&target);
            }
            Err(GuardedMutationError::Stale(StaleMutation {
                reason: "target_appeared",
                expected: None,
                current: current.as_deref().map(VersionSummary::from_version),
            }))
        }
        Err(CreateFailure::Io(error, may_have_modified)) => {
            if may_have_modified {
                complete_target_mutation(ctx, &target);
            }
            Err(GuardedMutationError::Io {
                error,
                may_have_modified,
            })
        }
    }
}

pub(crate) async fn remove_existing(
    ctx: &ToolContext,
    workspace_root: &Path,
    path: &Path,
    expected: FileVersion,
) -> Result<FileTarget, GuardedMutationError> {
    let authorization = MutationAuthorization::prepare(
        ctx,
        workspace_root,
        path,
        ExpectedVersionPolicy::Optional,
        Some(expected),
    )?;
    let expected = authorization
        .expected
        .clone()
        .expect("explicit expected version is retained");
    let path = path.to_path_buf();
    let workspace_root = workspace_root.to_path_buf();
    let lock = target_lock(&workspace_root, &path);
    let outcome = tokio::task::spawn_blocking(move || {
        let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
        let mut file = open_local(&path, &workspace_root, false).map_err(RemoveFailure::Io)?;
        let (_, current) = match read_current(&mut file, &path, &workspace_root) {
            Ok(observation) => observation,
            Err(ObservationError::Concurrent) => {
                return Err(RemoveFailure::Stale(None));
            }
            Err(ObservationError::Io(error)) => return Err(RemoveFailure::Io(error)),
        };
        if current != expected {
            return Err(RemoveFailure::Stale(Some(Box::new(current))));
        }
        let (_, before_remove) = match read_current(&mut file, &path, &workspace_root) {
            Ok(observation) => observation,
            Err(ObservationError::Concurrent) => {
                return Err(RemoveFailure::Stale(None));
            }
            Err(ObservationError::Io(error)) => return Err(RemoveFailure::Io(error)),
        };
        if before_remove != current || !descriptor_still_at_path(&file, &path) {
            return Err(RemoveFailure::Stale(Some(Box::new(before_remove))));
        }
        std::fs::remove_file(&path).map_err(RemoveFailure::Io)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|error| Err(RemoveFailure::Io(std::io::Error::other(error))));

    match outcome {
        Ok(()) => {
            let target = authorization.target.clone();
            authorization.success(ctx);
            Ok(target)
        }
        Err(RemoveFailure::Stale(current)) => {
            Err(authorization.stale("version_mismatch", current.as_deref()))
        }
        Err(RemoveFailure::Io(error)) if is_target_replaced_error(&error) => {
            Err(authorization.stale("target_replaced", None))
        }
        Err(RemoveFailure::Io(error)) => Err(GuardedMutationError::Io {
            error,
            may_have_modified: false,
        }),
    }
}

enum RewriteOutcome<T> {
    Written {
        value: T,
        before: Vec<u8>,
        written: Vec<u8>,
    },
    Unchanged {
        value: T,
        bytes: Vec<u8>,
    },
    Stale {
        reason: &'static str,
        current: Option<Box<FileVersion>>,
    },
}

enum CreateFailure {
    Stale(Option<Box<FileVersion>>),
    Io(std::io::Error, bool),
}

enum RemoveFailure {
    Stale(Option<Box<FileVersion>>),
    Io(std::io::Error),
}

enum RewriteFailure {
    Rejected(Box<MutationRejection>),
    IoBeforeWrite(std::io::Error),
    IoAfterWrite(std::io::Error),
}

impl From<std::io::Error> for RewriteFailure {
    fn from(error: std::io::Error) -> Self {
        Self::IoBeforeWrite(error)
    }
}

enum ObservationError {
    Concurrent,
    Io(std::io::Error),
}

impl From<std::io::Error> for ObservationError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn open_no_follow(path: &Path, writable: bool) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_symlink())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "symlink rejected",
        ));
    }
    options.open(path)
}

fn open_local(path: &Path, workspace_root: &Path, writable: bool) -> std::io::Result<File> {
    if let Some(rel) = super::write_grant::workspace_relative(workspace_root, path) {
        let mode = if writable {
            super::write_grant::ConfinedLeaf::OpenExistingRw
        } else {
            super::write_grant::ConfinedLeaf::OpenExistingRo
        };
        super::write_grant::open_confined(workspace_root, &rel, mode)
    } else {
        open_no_follow(path, writable)
    }
}

fn is_target_replaced_error(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::NotFound || super::is_symlink_error(error) {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ENOTDIR)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn read_current(
    file: &mut File,
    path: &Path,
    workspace_root: &Path,
) -> Result<(Vec<u8>, FileVersion), ObservationError> {
    let before = file.metadata()?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let path_after = match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() => metadata,
        Ok(_) => return Err(ObservationError::Concurrent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ObservationError::Concurrent);
        }
        Err(error) => return Err(ObservationError::Io(error)),
    };
    let before_hint = FileMetadataHint::from_metadata(&before);
    let after_hint = FileMetadataHint::from_metadata(&after);
    let path_hint = FileMetadataHint::from_metadata(&path_after);
    if before_hint != after_hint
        || after_hint != path_hint
        || after_hint.size() != bytes.len() as u64
    {
        return Err(ObservationError::Concurrent);
    }
    let target = FileTarget::for_local_workspace(workspace_root, path)?;
    Ok((
        bytes.clone(),
        FileVersion::from_bytes(target, None, &bytes, after_hint),
    ))
}

fn observe_path_once(
    path: &Path,
    workspace_root: &Path,
) -> Result<(Vec<u8>, FileVersion), ObservationError> {
    let mut file = open_local(path, workspace_root, false)?;
    read_current(&mut file, path, workspace_root)
}

fn descriptor_still_at_path(file: &File, path: &Path) -> bool {
    let Ok(descriptor) = file.metadata() else {
        return false;
    };
    let Ok(path_metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    !path_metadata.file_type().is_symlink()
        && FileMetadataHint::from_metadata(&descriptor)
            == FileMetadataHint::from_metadata(&path_metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_change_policy_returns_without_touching_the_file() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("target.txt");
        std::fs::write(&path, "same\n").unwrap();
        let before = std::fs::metadata(&path).unwrap();

        let rewrite = rewrite_existing(
            &ToolContext::zero(),
            workspace.path(),
            &path,
            ExpectedVersionPolicy::OptionalNoChange,
            None,
            None,
            move |_| Ok::<_, String>((b"same\n".to_vec(), ())),
        )
        .await
        .unwrap();

        assert!(!rewrite.changed);
        assert_eq!(rewrite.before, b"same\n");
        assert_eq!(rewrite.written, b"same\n");
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(before.ino(), after.ino());
            assert_eq!(before.ctime(), after.ctime());
            assert_eq!(before.ctime_nsec(), after.ctime_nsec());
        }
    }

    #[tokio::test]
    async fn concurrent_creates_never_overwrite_the_winning_file() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("target.txt");
        let context = ToolContext::zero();

        let (left, right) = tokio::join!(
            create_new(&context, workspace.path(), &path, b"left\n".to_vec(), None,),
            create_new(&context, workspace.path(), &path, b"right\n".to_vec(), None,),
        );
        let results = [left, right];

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let stale = results
            .into_iter()
            .find_map(Result::err)
            .expect("one create must lose the race")
            .into_tool_result("write_file", "target.txt");
        assert!(!stale.success);
        assert_eq!(
            stale.structured_metadata.as_ref().unwrap()["error_code"],
            STALE_MUTATION_CODE
        );
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content == "left\n" || content == "right\n");
    }

    #[test]
    fn io_failure_after_a_possible_partial_write_reports_the_modified_path() {
        let result = GuardedMutationError::Io {
            error: std::io::Error::other("synthetic partial write"),
            may_have_modified: true,
        }
        .into_tool_result("edit_file", "target.txt");

        assert!(!result.success);
        assert_eq!(
            result.file_modified.as_deref(),
            Some(Path::new("target.txt"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_change_policy_still_checks_the_authorized_epoch() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("target.txt");
        let replacement = workspace.path().join("replacement.txt");
        std::fs::write(&path, "same\n").unwrap();
        let expected_epoch =
            super::super::read_window::ViewEpoch::from_metadata(&std::fs::metadata(&path).unwrap())
                .unwrap();
        std::fs::write(&replacement, "same\n").unwrap();
        std::fs::rename(replacement, &path).unwrap();

        let error = rewrite_existing(
            &ToolContext::zero(),
            workspace.path(),
            &path,
            ExpectedVersionPolicy::OptionalNoChangeAtEpoch(expected_epoch),
            None,
            None,
            move |_| Ok::<_, String>((b"same\n".to_vec(), ())),
        )
        .await
        .expect_err("a replacement inode must invalidate the authorized view");
        let result = error.into_tool_result("write_file", "target.txt");

        assert!(!result.success);
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            json!("stale_file_version")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "same\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_between_match_and_truncate_returns_typed_stale() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("target.txt");
        let replacement = workspace.path().join("replacement.txt");
        std::fs::write(&path, "observed\n").unwrap();
        std::fs::write(&replacement, "external\n").unwrap();
        let (_, expected) = observe_existing(&path, workspace.path()).await.unwrap();
        let path_for_swap = path.clone();
        let replacement_for_swap = replacement.clone();

        let error = rewrite_existing(
            &ToolContext::zero(),
            workspace.path(),
            &path,
            ExpectedVersionPolicy::Optional,
            Some(expected),
            None,
            move |_| -> Result<_, String> {
                std::fs::rename(replacement_for_swap, path_for_swap).unwrap();
                Ok((b"tool write\n".to_vec(), ()))
            },
        )
        .await
        .expect_err("replacement must invalidate the write");
        let result = error.into_tool_result("edit_file", "target.txt");

        assert!(!result.success);
        assert!(result.output.contains("[stale_file_version]"));
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            json!("stale_file_version")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "external\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_during_typed_rejection_returns_stale_not_old_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("target.txt");
        let replacement = workspace.path().join("replacement.txt");
        std::fs::write(&path, "observed\n").unwrap();
        std::fs::write(&replacement, "external\n").unwrap();
        let path_for_swap = path.clone();
        let replacement_for_swap = replacement.clone();

        let error = rewrite_existing(
            &ToolContext::zero(),
            workspace.path(),
            &path,
            ExpectedVersionPolicy::Optional,
            None,
            None,
            move |_| -> Result<(Vec<u8>, ()), MutationTransformError> {
                std::fs::rename(replacement_for_swap, path_for_swap).unwrap();
                Err(MutationTransformError::Rejected(MutationRejection::new(
                    ToolResult {
                        output: "[edit_no_match] stale candidate".into(),
                        success: false,
                        structured_metadata: Some(json!({
                            "error_code": "edit_no_match",
                        })),
                        ..Default::default()
                    },
                )))
            },
        )
        .await
        .expect_err("a changed file must hide rejection evidence");
        let result = error.into_tool_result("edit_file", "target.txt");

        assert!(result.output.contains("[stale_file_version]"));
        assert_eq!(
            result.structured_metadata.as_ref().unwrap()["error_code"],
            json!("stale_file_version")
        );
        assert!(!result.output.contains("stale candidate"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "external\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_swap_between_match_and_truncate_never_writes_through_link() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = workspace.path().join("target.txt");
        let outside_path = outside.path().join("outside.txt");
        std::fs::write(&path, "observed\n").unwrap();
        std::fs::write(&outside_path, "outside\n").unwrap();
        let (_, expected) = observe_existing(&path, workspace.path()).await.unwrap();
        let path_for_swap = path.clone();
        let outside_for_swap = outside_path.clone();

        let error = rewrite_existing(
            &ToolContext::zero(),
            workspace.path(),
            &path,
            ExpectedVersionPolicy::Optional,
            Some(expected),
            None,
            move |_| -> Result<_, String> {
                std::fs::remove_file(&path_for_swap).unwrap();
                symlink(outside_for_swap, path_for_swap).unwrap();
                Ok((b"tool write\n".to_vec(), ()))
            },
        )
        .await
        .expect_err("symlink replacement must invalidate the write");
        let result = error.into_tool_result("edit_file", "target.txt");

        assert!(result.output.contains("[stale_file_version]"));
        assert_eq!(std::fs::read_to_string(outside_path).unwrap(), "outside\n");
    }
}
