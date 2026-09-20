//! Explicit ownership boundary for file-version and model-visible read state.

use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::file_state_cache::{FileStateCache, FileTarget};
use crate::model_read_receipts::{ModelReadReceiptStore, ReadReceiptOwner};

/// Task-local file state that may be shared by model branches.
///
/// The ledger contains disk observations only. Calling [`Self::for_branch`]
/// mints a new receipt store, so cloning this value cannot accidentally share
/// model-visible evidence between parent, child, or sibling agents.
#[derive(Clone, Debug)]
pub struct TaskFileState {
    workspace_id: String,
    ledger: Arc<FileStateCache>,
}

impl TaskFileState {
    /// Create an empty ledger bound to one canonical local workspace.
    pub fn for_local_workspace(workspace_root: &Path) -> io::Result<Self> {
        Self::with_ledger_for_local_workspace(workspace_root, Arc::new(FileStateCache::new()))
    }

    /// Bind an existing task ledger to a canonical local workspace.
    ///
    /// This is used when a child works in a separate worktree while retaining
    /// the parent's task-local version service. The workspace identity still
    /// follows the file provider's canonical path rules.
    pub fn with_ledger_for_local_workspace(
        workspace_root: &Path,
        ledger: Arc<FileStateCache>,
    ) -> io::Result<Self> {
        let workspace = FileTarget::for_local_workspace(workspace_root, workspace_root)?;
        Ok(Self {
            workspace_id: workspace.workspace_id().to_string(),
            ledger,
        })
    }

    /// Rebind this ledger to the workspace used by a child agent.
    pub fn for_workspace(&self, workspace_root: &Path) -> io::Result<Self> {
        Self::with_ledger_for_local_workspace(workspace_root, self.ledger.clone())
    }

    /// Create model-visible state for one complete branch owner.
    pub fn for_branch(
        &self,
        task_id: impl Into<String>,
        logical_session_id: impl Into<String>,
        model_branch_id: impl Into<String>,
    ) -> Option<ModelBranchFileState> {
        let owner = ReadReceiptOwner::new(
            self.workspace_id.clone(),
            task_id,
            logical_session_id,
            model_branch_id,
        )?;
        Some(ModelBranchFileState {
            task: self.clone(),
            receipts: Arc::new(ModelReadReceiptStore::for_owner(owner)),
        })
    }

    /// Canonical provider owner used by local file versions.
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// Shared task-local version ledger.
    pub fn ledger(&self) -> &Arc<FileStateCache> {
        &self.ledger
    }
}

/// Complete file state for one model-visible history branch.
#[derive(Clone, Debug)]
pub struct ModelBranchFileState {
    task: TaskFileState,
    receipts: Arc<ModelReadReceiptStore>,
}

impl ModelBranchFileState {
    /// Task-local state whose ledger may be shared with other branches.
    pub fn task_state(&self) -> &TaskFileState {
        &self.task
    }

    /// Shared task-local version ledger.
    pub fn ledger(&self) -> &Arc<FileStateCache> {
        self.task.ledger()
    }

    /// Receipt store private to this model branch.
    pub fn receipts(&self) -> &Arc<ModelReadReceiptStore> {
        &self.receipts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_share_only_the_version_ledger() {
        let workspace = tempfile::tempdir().unwrap();
        let task = TaskFileState::for_local_workspace(workspace.path()).unwrap();
        let parent = task.for_branch("task", "session", "root").unwrap();
        let child = task
            .for_branch("task-child", "session-child", "child")
            .unwrap();

        assert!(Arc::ptr_eq(parent.ledger(), child.ledger()));
        assert!(!Arc::ptr_eq(parent.receipts(), child.receipts()));
        assert_ne!(parent.receipts().owner(), child.receipts().owner());
    }

    #[test]
    fn incomplete_branch_owner_disables_receipts() {
        let workspace = tempfile::tempdir().unwrap();
        let task = TaskFileState::for_local_workspace(workspace.path()).unwrap();

        assert!(task.for_branch("", "session", "root").is_none());
        assert!(task.for_branch("task", "", "root").is_none());
        assert!(task.for_branch("task", "session", "").is_none());
    }

    #[test]
    fn workspace_rebind_keeps_ledger_but_changes_owner() {
        let parent_workspace = tempfile::tempdir().unwrap();
        let child_workspace = tempfile::tempdir().unwrap();
        let parent = TaskFileState::for_local_workspace(parent_workspace.path()).unwrap();
        let child = parent.for_workspace(child_workspace.path()).unwrap();

        assert!(Arc::ptr_eq(parent.ledger(), child.ledger()));
        assert_ne!(parent.workspace_id(), child.workspace_id());
    }
}
