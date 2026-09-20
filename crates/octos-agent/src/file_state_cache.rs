//! Task-local file version ledger with LRU eviction.
//!
//! # Invariants
//!
//! - LRU ordering is maintained per `get` / `record` — the most recently accessed
//!   entry moves to the back.
//! - `record` evicts until both `max_entries` and `max_total_bytes` are respected.
//! - A key contains both the workspace owner and canonical provider target.
//! - A version is derived from bytes and metadata captured by one stable read.
//! - The ledger never decides whether a model has seen file contents.
//! - `invalidate_path` drops every workspace-owned version for a target.
//! - `clear` drops every entry and resets `total_size_bytes` to 0.
//! - `clone_for_subagent` produces an independent snapshot so parent and
//!   delegate agents cannot race.
//!
//! The cache is intentionally wrapped in internal mutability (a single
//! `Mutex`) so tools can consult it through an `Arc<FileStateCache>` without
//! coordinating on a `&mut` handle. A single lock keeps the state machine
//! small and easy to reason about; the critical sections are short.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
#[cfg(not(unix))]
use std::time::SystemTime;

use sha2::{Digest, Sha256};

/// Default maximum number of file versions retained by Octos.
pub const DEFAULT_MAX_ENTRIES: usize = 100;

/// Default maximum sum of observed file sizes retained by Octos.
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 25 * 1024 * 1024;

/// Provider-scoped identity of a file.
///
/// Local files use the canonical workspace root as `workspace_id` and the
/// canonical absolute path as `target_key`. Other providers may supply their
/// own stable owner and target identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileTarget {
    workspace_id: String,
    target_key: PathBuf,
}

impl FileTarget {
    /// Create an identity for a provider-owned target.
    pub fn new(workspace_id: impl Into<String>, target_key: impl Into<PathBuf>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            target_key: target_key.into(),
        }
    }

    /// Canonicalize a local workspace and target into one stable identity.
    pub fn for_local_workspace(workspace_root: &Path, target: &Path) -> io::Result<Self> {
        let workspace_root = dunce::canonicalize(workspace_root)?;
        // The leaf may have been deleted after it was observed or may be the
        // destination of a create. Resolve the nearest existing ancestor so
        // both states retain the same canonical provider key.
        let target_key = octos_core::canonicalize_lossy(target);
        Ok(Self::new(
            format!("local:{}", workspace_root.display()),
            target_key,
        ))
    }

    /// Stable owner of the target.
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// Canonical provider-specific target key.
    pub fn target_key(&self) -> &Path {
        &self.target_key
    }
}

/// Cheap metadata used to reject stale observations before comparing content.
///
/// Metadata is only a hint. Equality never substitutes for the SHA-256 digest
/// in [`FileVersion`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMetadataHint {
    size: u64,
    mtime_ns: Option<i128>,
    ctime_ns: Option<i128>,
    device: Option<u64>,
    inode: Option<u64>,
}

impl FileMetadataHint {
    /// Create a metadata hint. These fields are never sufficient for equality
    /// without a content digest or trusted provider revision.
    pub fn new(
        size: u64,
        mtime_ns: Option<i128>,
        ctime_ns: Option<i128>,
        device: Option<u64>,
        inode: Option<u64>,
    ) -> Self {
        Self {
            size,
            mtime_ns,
            ctime_ns,
            device,
            inode,
        }
    }

    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let (mtime_ns, ctime_ns, device, inode) = metadata_identity(metadata);
        Self::new(metadata.len(), mtime_ns, ctime_ns, device, inode)
    }

    /// Observed file size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Observed modification time as nanoseconds from the Unix epoch.
    pub fn mtime_ns(&self) -> Option<i128> {
        self.mtime_ns
    }

    /// Observed inode change time as nanoseconds from the Unix epoch.
    pub fn ctime_ns(&self) -> Option<i128> {
        self.ctime_ns
    }

    /// Observed device identifier when the platform exposes it.
    pub fn device(&self) -> Option<u64> {
        self.device
    }

    /// Observed inode identifier when the platform exposes it.
    pub fn inode(&self) -> Option<u64> {
        self.inode
    }
}

#[cfg(unix)]
fn metadata_identity(
    metadata: &std::fs::Metadata,
) -> (Option<i128>, Option<i128>, Option<u64>, Option<u64>) {
    use std::os::unix::fs::MetadataExt;

    let mtime_ns = i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec());
    let ctime_ns = i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
    (
        Some(mtime_ns),
        Some(ctime_ns),
        Some(metadata.dev()),
        Some(metadata.ino()),
    )
}

#[cfg(not(unix))]
fn metadata_identity(
    metadata: &std::fs::Metadata,
) -> (Option<i128>, Option<i128>, Option<u64>, Option<u64>) {
    let mtime_ns = metadata.modified().ok().and_then(system_time_nanos);
    (mtime_ns, None, None, None)
}

#[cfg(not(unix))]
fn system_time_nanos(time: SystemTime) -> Option<i128> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).ok(),
        Err(error) => i128::try_from(error.duration().as_nanos())
            .ok()
            .map(|nanos| -nanos),
    }
}

/// Strong identity of bytes observed for one provider-scoped target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileVersion {
    target: FileTarget,
    provider_version: Option<String>,
    content_sha256: String,
    size: u64,
    metadata_hint: FileMetadataHint,
}

impl FileVersion {
    /// Build a strong version from raw provider bytes.
    pub fn from_bytes(
        target: FileTarget,
        provider_version: Option<String>,
        bytes: &[u8],
        metadata_hint: FileMetadataHint,
    ) -> Self {
        let started_at = Instant::now();
        let content_sha256 = Self::sha256(bytes);
        metrics::counter!("octos_file_version_hash_bytes_total").increment(bytes.len() as u64);
        metrics::histogram!("octos_file_version_hash_duration_seconds")
            .record(started_at.elapsed().as_secs_f64());
        Self {
            target,
            provider_version,
            content_sha256,
            size: bytes.len() as u64,
            metadata_hint,
        }
    }

    /// Return a prefixed lowercase SHA-256 digest.
    pub fn sha256(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    /// Provider-scoped target this version belongs to.
    pub fn target(&self) -> &FileTarget {
        &self.target
    }

    /// Opaque provider revision when one was supplied.
    pub fn provider_version(&self) -> Option<&str> {
        self.provider_version.as_deref()
    }

    /// SHA-256 digest of the raw bytes read from the provider.
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    /// Number of raw bytes in this version.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Metadata hint captured with the same stable observation.
    pub fn metadata_hint(&self) -> &FileMetadataHint {
        &self.metadata_hint
    }
}

#[derive(Debug)]
struct Inner {
    entries: HashMap<FileTarget, FileVersion>,
    /// Order of keys from least recently used (front) to most recently used
    /// (back). On every `get`/`record` we bump the touched key to the back.
    order: VecDeque<FileTarget>,
    total_size_bytes: usize,
    /// Targets whose observed version is currently being consumed by a
    /// mutation. A second mutation cannot reuse the same observation.
    mutation_claims: HashSet<FileTarget>,
}

impl Inner {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_size_bytes: 0,
            mutation_claims: HashSet::new(),
        }
    }

    fn bump_to_back(&mut self, target: &FileTarget) {
        if let Some(pos) = self.order.iter().position(|candidate| candidate == target) {
            if let Some(key) = self.order.remove(pos) {
                self.order.push_back(key);
            }
        }
    }
}

/// Why an observed version could not be reserved for a mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationClaimError {
    /// The task has not observed this target, or that observation was invalidated.
    MissingExpectedVersion,
    /// Another mutation is already consuming the same observed version.
    AlreadyClaimed,
}

/// Exclusive, task-local claim on one observed file version.
///
/// Dropping a claim releases it without changing the recorded observation.
/// [`Self::invalidate`] consumes both the claim and the observation after a
/// successful write or a proven stale comparison.
#[derive(Debug)]
pub(crate) struct FileMutationClaim {
    ledger: Arc<FileStateCache>,
    target: FileTarget,
    expected: FileVersion,
    invalidated: bool,
}

impl FileMutationClaim {
    pub(crate) fn expected(&self) -> &FileVersion {
        &self.expected
    }

    pub(crate) fn invalidate(mut self) {
        self.ledger.finish_mutation_claim(&self.target, true);
        self.invalidated = true;
    }
}

impl Drop for FileMutationClaim {
    fn drop(&mut self) {
        if !self.invalidated {
            self.ledger.finish_mutation_claim(&self.target, false);
        }
    }
}

/// LRU ledger of strong file versions.
///
/// Cloning (or [`FileStateCache::clone_for_subagent`]) yields a **deep copy**
/// so parent and subagent cannot race each other's entries.
#[derive(Debug)]
pub struct FileStateCache {
    max_entries: usize,
    max_total_bytes: usize,
    inner: Mutex<Inner>,
}

impl Default for FileStateCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FileStateCache {
    /// Create a cache with default capacity (100 entries / 25 MB).
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Return a builder for tuning `max_entries` / `max_total_bytes`.
    pub fn builder() -> FileStateCacheBuilder {
        FileStateCacheBuilder::default()
    }

    /// Maximum number of cached entries.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Maximum total cached bytes across all entries.
    pub fn max_total_bytes(&self) -> usize {
        self.max_total_bytes
    }

    /// Total bytes currently cached.
    pub fn total_size_bytes(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.total_size_bytes
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.len()
    }

    /// Whether the cache currently has no recorded entries.
    pub fn is_empty(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.is_empty()
    }

    /// Return the latest observed version for `target`.
    ///
    /// This is historical disk state, not proof that any model saw the bytes.
    pub fn get(&self, target: &FileTarget) -> Option<FileVersion> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stored = inner.entries.get(target)?;
        let entry = stored.clone();
        inner.bump_to_back(target);
        Some(entry)
    }

    /// Look up `target` without updating LRU order.
    pub fn peek(&self, target: &FileTarget) -> Option<FileVersion> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.get(target).cloned()
    }

    /// Reserve the current observation for one mutation.
    ///
    /// This is intentionally task-local: it prevents two built-in mutations
    /// from both committing against one observed version. The caller must
    /// still compare [`FileMutationClaim::expected`] with bytes read from the
    /// descriptor it will actually mutate.
    pub(crate) fn claim_mutation(
        self: &Arc<Self>,
        target: &FileTarget,
    ) -> Result<FileMutationClaim, MutationClaimError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(expected) = inner.entries.get(target).cloned() else {
            return Err(MutationClaimError::MissingExpectedVersion);
        };
        if !inner.mutation_claims.insert(target.clone()) {
            return Err(MutationClaimError::AlreadyClaimed);
        }
        Ok(FileMutationClaim {
            ledger: self.clone(),
            target: target.clone(),
            expected,
            invalidated: false,
        })
    }

    /// Record a stable observation, bumping it to the most-recent LRU slot and
    /// evicting stale entries until both caps hold.
    pub fn record(&self, version: FileVersion) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = version.target.clone();

        if let Some(old) = inner.entries.remove(&key) {
            inner.total_size_bytes = inner.total_size_bytes.saturating_sub(version_size(&old));
            if let Some(pos) = inner.order.iter().position(|p| p == &key) {
                inner.order.remove(pos);
            }
        }

        inner.total_size_bytes = inner
            .total_size_bytes
            .saturating_add(version_size(&version));
        inner.entries.insert(key.clone(), version);
        inner.order.push_back(key);

        // Evict until within caps.
        while (inner.entries.len() > self.max_entries
            || inner.total_size_bytes > self.max_total_bytes)
            && !inner.order.is_empty()
        {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(dropped) = inner.entries.remove(&oldest) {
                inner.total_size_bytes = inner
                    .total_size_bytes
                    .saturating_sub(version_size(&dropped));
            }
        }
    }

    /// Drop every version for a canonical target across workspace owners.
    pub fn invalidate_path(&self, path: &Path) {
        let target_key = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let targets = inner
            .entries
            .keys()
            .filter(|target| target.target_key == target_key)
            .cloned()
            .collect::<Vec<_>>();
        for target in targets {
            if let Some(dropped) = inner.entries.remove(&target) {
                inner.total_size_bytes = inner
                    .total_size_bytes
                    .saturating_sub(version_size(&dropped));
            }
            if let Some(pos) = inner
                .order
                .iter()
                .position(|candidate| candidate == &target)
            {
                inner.order.remove(pos);
            }
        }
    }

    /// Drop one exact provider-scoped target.
    pub(crate) fn invalidate_target(&self, target: &FileTarget) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::remove_target(&mut inner, target);
    }

    /// Clear every recorded version. Losing ledger state only causes later
    /// reads to establish a fresh version.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.clear();
        inner.order.clear();
        inner.total_size_bytes = 0;
    }

    /// Return a deep-copied version ledger for a subagent.
    ///
    /// The child's writes/invalidations do not race the parent. The caps are
    /// copied verbatim. Use this at spawn/delegate boundaries.
    pub fn clone_for_subagent(&self) -> Self {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = Inner {
            entries: inner.entries.clone(),
            order: inner.order.clone(),
            total_size_bytes: inner.total_size_bytes,
            mutation_claims: HashSet::new(),
        };
        Self {
            max_entries: self.max_entries,
            max_total_bytes: self.max_total_bytes,
            inner: Mutex::new(snapshot),
        }
    }

    fn finish_mutation_claim(&self, target: &FileTarget, invalidate: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.mutation_claims.remove(target);
        if invalidate {
            Self::remove_target(&mut inner, target);
        }
    }

    fn remove_target(inner: &mut Inner, target: &FileTarget) {
        if let Some(dropped) = inner.entries.remove(target) {
            inner.total_size_bytes = inner
                .total_size_bytes
                .saturating_sub(version_size(&dropped));
        }
        if let Some(pos) = inner.order.iter().position(|candidate| candidate == target) {
            inner.order.remove(pos);
        }
    }
}

fn version_size(version: &FileVersion) -> usize {
    usize::try_from(version.size).unwrap_or(usize::MAX)
}

impl Clone for FileStateCache {
    fn clone(&self) -> Self {
        self.clone_for_subagent()
    }
}

/// Builder for [`FileStateCache`].
#[derive(Debug, Clone)]
pub struct FileStateCacheBuilder {
    max_entries: usize,
    max_total_bytes: usize,
}

impl Default for FileStateCacheBuilder {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

impl FileStateCacheBuilder {
    /// Maximum number of cached entries (must be >= 1).
    pub fn max_entries(mut self, value: usize) -> Self {
        self.max_entries = value.max(1);
        self
    }

    /// Maximum total cached bytes (must be >= 1).
    pub fn max_total_bytes(mut self, value: usize) -> Self {
        self.max_total_bytes = value.max(1);
        self
    }

    /// Construct the cache.
    pub fn build(self) -> FileStateCache {
        FileStateCache {
            max_entries: self.max_entries,
            max_total_bytes: self.max_total_bytes,
            inner: Mutex::new(Inner::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(workspace: &str, path: &str) -> FileTarget {
        FileTarget::new(workspace, PathBuf::from(path))
    }

    fn version(workspace: &str, path: &str, bytes: &[u8]) -> FileVersion {
        FileVersion::from_bytes(
            target(workspace, path),
            None,
            bytes,
            FileMetadataHint::new(bytes.len() as u64, None, None, None, None),
        )
    }

    #[test]
    fn should_record_and_get_strong_version() {
        let ledger = FileStateCache::new();
        let version = version("workspace", "/tmp/a.txt", b"content");
        let target = version.target().clone();
        ledger.record(version.clone());

        assert_eq!(ledger.get(&target), Some(version));
    }

    #[test]
    fn should_distinguish_content_with_identical_metadata_hints() {
        let target = target("workspace", "/tmp/same-metadata.txt");
        let hint = FileMetadataHint::new(4, Some(1), Some(2), Some(3), Some(4));
        let old = FileVersion::from_bytes(target.clone(), None, b"AAAA", hint.clone());
        let current = FileVersion::from_bytes(target, None, b"BBBB", hint);

        assert_eq!(old.metadata_hint(), current.metadata_hint());
        assert_eq!(old.size(), current.size());
        assert_ne!(old.content_sha256(), current.content_sha256());
    }

    #[test]
    fn should_evict_lru_when_max_entries_exceeded() {
        let ledger = FileStateCache::builder().max_entries(2).build();
        let a = target("workspace", "/a");
        let b = target("workspace", "/b");
        let c = target("workspace", "/c");

        ledger.record(version("workspace", "/a", b"aaaaaaaaaa"));
        ledger.record(version("workspace", "/b", b"bbbbbbbbbb"));
        ledger.record(version("workspace", "/c", b"cccccccccc"));

        assert_eq!(ledger.len(), 2);
        assert!(ledger.peek(&a).is_none(), "oldest entry must be evicted");
        assert!(ledger.peek(&b).is_some());
        assert!(ledger.peek(&c).is_some());
    }

    #[test]
    fn should_evict_lru_when_max_bytes_exceeded() {
        let ledger = FileStateCache::builder()
            .max_entries(100)
            .max_total_bytes(30)
            .build();
        let a = target("workspace", "/a");
        let b = target("workspace", "/b");
        let c = target("workspace", "/c");

        ledger.record(version("workspace", "/a", &[b'a'; 20]));
        ledger.record(version("workspace", "/b", &[b'b'; 10]));
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.total_size_bytes(), 30);

        ledger.record(version("workspace", "/c", &[b'c'; 20]));
        assert!(ledger.peek(&a).is_none());
        assert!(ledger.peek(&b).is_some());
        assert!(ledger.peek(&c).is_some());
        assert_eq!(ledger.total_size_bytes(), 30);
    }

    #[test]
    fn should_bump_lru_position_on_hit() {
        let ledger = FileStateCache::builder().max_entries(2).build();
        let a = target("workspace", "/a");
        let b = target("workspace", "/b");
        let c = target("workspace", "/c");

        ledger.record(version("workspace", "/a", b"a"));
        ledger.record(version("workspace", "/b", b"b"));

        assert!(ledger.get(&a).is_some());
        ledger.record(version("workspace", "/c", b"c"));

        assert!(ledger.peek(&a).is_some(), "/a was touched");
        assert!(ledger.peek(&b).is_none(), "/b was evicted");
        assert!(ledger.peek(&c).is_some());
    }

    #[test]
    fn should_replace_version_for_same_target() {
        let ledger = FileStateCache::new();
        let target = target("workspace", "/a");
        ledger.record(version("workspace", "/a", &[b'a'; 100]));
        ledger.record(version("workspace", "/a", &[b'b'; 25]));

        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.total_size_bytes(), 25);
        assert_eq!(ledger.peek(&target).unwrap().size(), 25);
    }

    #[test]
    fn should_isolate_same_target_across_workspaces() {
        let ledger = FileStateCache::new();
        let a = target("workspace-a", "/same");
        let b = target("workspace-b", "/same");
        ledger.record(version("workspace-a", "/same", b"aaaa"));
        ledger.record(version("workspace-b", "/same", b"bbbb"));

        assert_ne!(
            ledger.get(&a).unwrap().content_sha256(),
            ledger.get(&b).unwrap().content_sha256()
        );
        assert_eq!(ledger.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn local_target_canonicalizes_ancestor_symlink_alias() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let real_dir = root.path().join("real");
        let alias_dir = root.path().join("alias");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("same.txt"), "content").unwrap();
        symlink(&real_dir, &alias_dir).unwrap();

        let direct =
            FileTarget::for_local_workspace(root.path(), &real_dir.join("same.txt")).unwrap();
        let aliased =
            FileTarget::for_local_workspace(root.path(), &alias_dir.join("same.txt")).unwrap();

        assert_eq!(direct, aliased);
    }

    #[test]
    fn should_invalidate_target_path_across_workspace_owners() {
        let ledger = FileStateCache::new();
        let a = target("workspace-a", "/same");
        let b = target("workspace-b", "/same");
        let other = target("workspace-a", "/other");
        ledger.record(version("workspace-a", "/same", b"aaaa"));
        ledger.record(version("workspace-b", "/same", b"bbbb"));
        ledger.record(version("workspace-a", "/other", b"other"));

        ledger.invalidate_path(Path::new("/same"));

        assert!(ledger.peek(&a).is_none());
        assert!(ledger.peek(&b).is_none());
        assert!(ledger.peek(&other).is_some());
    }

    #[test]
    fn should_clone_for_subagent_produces_independent_copy() {
        let parent = FileStateCache::new();
        let c = target("workspace", "/c");
        parent.record(version("workspace", "/a", b"aaaaaaaaaa"));
        parent.record(version("workspace", "/b", &[b'b'; 20]));

        let child = parent.clone_for_subagent();
        assert_eq!(child.len(), 2);
        assert_eq!(child.total_size_bytes(), 30);

        child.invalidate_path(Path::new("/a"));
        assert_eq!(child.len(), 1);
        assert_eq!(parent.len(), 2);

        parent.record(version("workspace", "/c", b"ccccc"));
        assert!(parent.peek(&c).is_some());
        assert!(child.peek(&c).is_none());
    }

    #[test]
    fn should_clear_drops_all_entries() {
        let ledger = FileStateCache::new();
        let a = target("workspace", "/a");
        ledger.record(version("workspace", "/a", b"a"));
        ledger.record(version("workspace", "/b", b"bb"));
        ledger.record(version("workspace", "/c", b"ccc"));
        assert_eq!(ledger.len(), 3);
        assert_eq!(ledger.total_size_bytes(), 6);

        ledger.clear();

        assert!(ledger.is_empty());
        assert_eq!(ledger.total_size_bytes(), 0);
        assert!(ledger.peek(&a).is_none());
    }

    #[test]
    fn poisoned_lock_only_loses_optimization_not_ledger_availability() {
        let ledger = FileStateCache::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ledger.inner.lock().unwrap();
            panic!("poison test");
        }));

        let version = version("workspace", "/after-poison", b"body");
        let target = version.target().clone();
        ledger.record(version.clone());

        assert_eq!(ledger.get(&target), Some(version));
    }

    #[test]
    fn builder_exposes_configured_caps() {
        let cache = FileStateCache::builder()
            .max_entries(10)
            .max_total_bytes(4096)
            .build();
        assert_eq!(cache.max_entries(), 10);
        assert_eq!(cache.max_total_bytes(), 4096);
    }

    #[test]
    fn sha256_is_stable_for_same_input() {
        let a = FileVersion::sha256(b"hello");
        let b = FileVersion::sha256(b"hello");
        assert_eq!(a, b);
        let c = FileVersion::sha256(b"hello\n");
        assert_ne!(a, c);
        assert_eq!(
            a,
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
