//! Bounded safe-text artifacts in the context ledger. The catalog is the commit record.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model_read_receipts::ReadReceiptOwner;
use crate::output_recovery::{
    Availability, CaptureState, OutputDocument, OutputError, OutputPart, OutputRange, OutputSource,
    OutputStream, OutputView, PAGE_BYTES, RenderedOutput, digest, render,
};

const SCHEMA: u32 = 1;
pub const BLOCK_BYTES: usize = 64 * 1024;
pub const OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub const STREAM_BYTES: usize = 8 * 1024 * 1024;
pub const SESSION_BYTES: u64 = 64 * 1024 * 1024;
pub const SESSION_ENTRIES: usize = 256;
pub const MANIFEST_BYTES: usize = 16 * 1024;
const CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const GLOBAL_BYTES: u64 = 256 * 1024 * 1024;
const GLOBAL_ENTRIES: usize = 4096;
const TTL: u64 = 24 * 60 * 60;
const LEGACY_BODY_BYTES: usize = 50_000 - 512;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecallRequest {
    pub output_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub stream: Option<OutputStream>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
    pub cursor: Option<String>,
    pub page: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    v: u32,
    id: String,
    stream: OutputStream,
    position: u64,
    upper: u64,
    revision: String,
}

#[derive(Clone, Debug)]
pub struct RecalledOutput {
    pub rendered: RenderedOutput,
    pub document: OutputDocument,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredPart {
    stream: OutputStream,
    start: u64,
    bytes: u64,
    sha256: String,
    blocks: Vec<String>,
    legacy_starts: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    version: u32,
    input_digest: String,
    view: OutputView,
    parts: Vec<StoredPart>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    id: String,
    owner: String,
    session: String,
    running: bool,
    call_id: String,
    manifest: String,
    payload_bytes: u64,
    metadata_bytes: u64,
    updated: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Catalog {
    version: u32,
    records: Vec<Record>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: SCHEMA,
            records: Vec::new(),
        }
    }
}

/// File descriptors anchor every read, rename and unlink below a trusted host directory.
#[derive(Debug)]
struct Directory {
    file: File,
    #[cfg(test)]
    path: PathBuf,
}

impl Directory {
    #[cfg(unix)]
    fn open(data_dir: &Path) -> Result<Self, OutputError> {
        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        let root = std::fs::canonicalize(data_dir).map_err(|_| OutputError::StorageFailed)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut fd = openat(rustix::fs::CWD, &root, flags, Mode::empty())
            .map_err(|_| OutputError::StorageFailed)?;
        #[cfg(test)]
        let mut path = root.clone();
        for name in ["context_ledgers", "tool-output", "recovery-v1"] {
            match mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(_) => return Err(OutputError::StorageFailed),
            }
            fd = openat(&fd, name, flags, Mode::empty()).map_err(|_| OutputError::StorageFailed)?;
            #[cfg(test)]
            path.push(name);
        }
        Ok(Self {
            file: File::from(fd),
            #[cfg(test)]
            path,
        })
    }

    #[cfg(not(unix))]
    fn open(_: &Path) -> Result<Self, OutputError> {
        Err(OutputError::RecoveryUnavailable)
    }

    #[cfg(unix)]
    fn file(&self, name: &str, create: bool, exclusive: bool) -> Result<File, OutputError> {
        use rustix::fs::{Mode, OFlags, openat};
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        {
            return Err(OutputError::Corrupt);
        }
        let mut flags = OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        flags |= if create {
            OFlags::RDWR | OFlags::CREATE
        } else {
            OFlags::RDONLY
        };
        if exclusive {
            flags |= OFlags::EXCL;
        }
        let fd = openat(&self.file, name, flags, Mode::from_raw_mode(0o600)).map_err(|e| {
            if e == rustix::io::Errno::NOENT {
                OutputError::Missing
            } else {
                OutputError::StorageFailed
            }
        })?;
        let file = File::from(fd);
        let metadata = file.metadata().map_err(|_| OutputError::StorageFailed)?;
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(OutputError::Corrupt);
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    fn file(&self, _: &str, _: bool, _: bool) -> Result<File, OutputError> {
        Err(OutputError::RecoveryUnavailable)
    }

    fn read(&self, name: &str, max: u64) -> Result<Vec<u8>, OutputError> {
        let file = self.file(name, false, false)?;
        if file.metadata().map_err(|_| OutputError::Corrupt)?.len() > max {
            return Err(OutputError::Corrupt);
        }
        let mut bytes = Vec::new();
        file.take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| OutputError::Corrupt)?;
        if bytes.len() as u64 > max {
            return Err(OutputError::Corrupt);
        }
        Ok(bytes)
    }

    fn atomic(&self, name: &str, bytes: &[u8]) -> Result<(), OutputError> {
        let tmp = format!("{}.tmp", uuid::Uuid::new_v4());
        let result = (|| {
            let mut file = self.file(&tmp, true, true)?;
            file.write_all(bytes)
                .map_err(|_| OutputError::StorageFailed)?;
            file.sync_all().map_err(|_| OutputError::StorageFailed)?;
            #[cfg(unix)]
            rustix::fs::renameat(&self.file, tmp.as_str(), &self.file, name)
                .map_err(|_| OutputError::StorageFailed)?;
            #[cfg(not(unix))]
            return Err(OutputError::RecoveryUnavailable);
            self.file.sync_all().map_err(|_| OutputError::StorageFailed)
        })();
        if result.is_err() {
            self.remove(&tmp);
        }
        result
    }

    fn remove(&self, name: &str) {
        #[cfg(unix)]
        {
            let _ = rustix::fs::unlinkat(&self.file, name, rustix::fs::AtFlags::empty());
        }
    }

    fn files(&self) -> Result<Vec<(String, u64)>, OutputError> {
        #[cfg(not(unix))]
        return Err(OutputError::RecoveryUnavailable);
        #[cfg(unix)]
        let mut directory =
            rustix::fs::Dir::read_from(&self.file).map_err(|_| OutputError::StorageFailed)?;
        let mut files = Vec::new();
        #[cfg(unix)]
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(|_| OutputError::StorageFailed)?;
            let name = entry.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            if files.len() >= GLOBAL_ENTRIES * 8 {
                return Err(OutputError::StorageLimit);
            }
            let name = std::str::from_utf8(name)
                .map_err(|_| OutputError::Corrupt)?
                .to_owned();
            let file = self.file(&name, false, false)?;
            files.push((
                name,
                file.metadata().map_err(|_| OutputError::Corrupt)?.len(),
            ));
        }
        Ok(files)
    }
}

#[derive(Debug)]
pub struct OutputStore {
    directory: Directory,
    owner: ReadReceiptOwner,
    owner_key: String,
    session_key: String,
    /// Shared lease pins this owner's records across threads and processes.
    _lease: File,
    lock: Mutex<File>,
}

struct CatalogLock<'a>(&'a File);
impl Drop for CatalogLock<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.0);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>, OutputError> {
    serde_json::to_vec(value).map_err(|_| OutputError::Corrupt)
}

fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id)
        .is_ok_and(|parsed| parsed.to_string() == id || parsed.simple().to_string() == id)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl OutputStore {
    /// The host must supply the original owner. A model-supplied ID never grants ownership.
    pub fn open(data_dir: &Path, owner: ReadReceiptOwner) -> Result<Arc<Self>, OutputError> {
        if [
            owner.workspace_id(),
            owner.task_id(),
            owner.logical_session_id(),
            owner.model_branch_id(),
        ]
        .iter()
        .any(|s| s.trim().is_empty())
        {
            return Err(OutputError::OwnerMismatch);
        }
        let directory = Directory::open(data_dir)?;
        let lock = directory.file("catalog.lock", true, false)?;
        lock.lock_exclusive()
            .map_err(|_| OutputError::StorageFailed)?;
        let guard = CatalogLock(&lock);
        let owner_key = digest(&json(&owner)?);
        let session_key = digest(&json(&(
            owner.workspace_id(),
            owner.task_id(),
            owner.logical_session_id(),
        ))?);
        let lease = directory.file(&format!("{owner_key}.lease"), true, false)?;
        let cold = lease.try_lock_exclusive().is_ok();
        FileExt::lock_shared(&lease).map_err(|_| OutputError::StorageFailed)?;
        drop(guard);
        let store = Arc::new(Self {
            directory,
            owner,
            owner_key,
            session_key,
            _lease: lease,
            lock: Mutex::new(lock),
        });
        {
            let lock = store.lock.lock().map_err(|_| OutputError::StorageFailed)?;
            lock.lock_exclusive()
                .map_err(|_| OutputError::StorageFailed)?;
            let _guard = CatalogLock(&lock);
            let mut catalog = store.catalog()?;
            if cold {
                let before = catalog.records.len();
                catalog.records.retain(|r| {
                    r.owner != store.owner_key || now().saturating_sub(r.updated) < TTL
                });
                let mut changed = before != catalog.records.len();
                for record in catalog
                    .records
                    .iter_mut()
                    .filter(|r| r.owner == store.owner_key)
                {
                    let mut manifest = store.manifest(record)?;
                    if manifest.view.capture == CaptureState::Running {
                        manifest.view.capture = CaptureState::Partial;
                        manifest.view.execution = crate::output_recovery::ExecutionStatus::Unknown;
                        manifest.view.loss_reason = Some("capture_interrupted".into());
                        let bytes = json(&manifest)?;
                        record.manifest = digest(&bytes);
                        record.metadata_bytes = bytes.len() as u64;
                        record.running = false;
                        store
                            .directory
                            .atomic(&format!("{}.manifest", record.manifest), &bytes)?;
                        changed = true;
                    }
                }
                if changed {
                    store.commit(&catalog)?;
                }
            }
            store.clean(&mut catalog, false)?;
        }
        Ok(store)
    }

    fn catalog(&self) -> Result<Catalog, OutputError> {
        let bytes = match self.directory.read("index.json", CATALOG_BYTES) {
            Ok(bytes) => bytes,
            Err(OutputError::Missing) => return Ok(Catalog::default()),
            Err(e) => return Err(e),
        };
        let catalog: Catalog = serde_json::from_slice(&bytes).map_err(|_| OutputError::Corrupt)?;
        if catalog.version != SCHEMA {
            return Err(OutputError::UnsupportedSchema);
        }
        if catalog.records.len() > GLOBAL_ENTRIES {
            return Err(OutputError::StorageLimit);
        }
        let mut ids = HashSet::new();
        for record in &catalog.records {
            if !valid_id(&record.id)
                || !ids.insert(&record.id)
                || !valid_digest(&record.owner)
                || !valid_digest(&record.session)
                || !valid_digest(&record.manifest)
                || record.metadata_bytes > MANIFEST_BYTES as u64
                || record.payload_bytes > OUTPUT_BYTES as u64
            {
                return Err(OutputError::Corrupt);
            }
        }
        Ok(catalog)
    }

    fn commit(&self, catalog: &Catalog) -> Result<(), OutputError> {
        let bytes = json(catalog)?;
        if bytes.len() as u64 > CATALOG_BYTES {
            return Err(OutputError::StorageLimit);
        }
        self.directory.atomic("index.json", &bytes)
    }

    fn manifest(&self, record: &Record) -> Result<Manifest, OutputError> {
        let bytes = self.directory.read(
            &format!("{}.manifest", record.manifest),
            MANIFEST_BYTES as u64,
        )?;
        if digest(&bytes) != record.manifest {
            return Err(OutputError::Corrupt);
        }
        if bytes.len() as u64 != record.metadata_bytes {
            return Err(OutputError::Corrupt);
        }
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|_| OutputError::Corrupt)?;
        if manifest.version != SCHEMA || manifest.view.schema_version != 1 {
            return Err(OutputError::UnsupportedSchema);
        }
        if manifest.view.owner != self.owner || record.owner != self.owner_key {
            return Err(OutputError::OwnerMismatch);
        }
        let session = digest(&json(&(
            manifest.view.owner.workspace_id(),
            manifest.view.owner.task_id(),
            manifest.view.owner.logical_session_id(),
        ))?);
        if manifest.view.output_id != record.id || manifest.view.call_id != record.call_id {
            return Err(OutputError::Corrupt);
        }
        if record.session != session
            || record.running != (manifest.view.capture == CaptureState::Running)
            || manifest.view.stored_bytes != record.payload_bytes
            || manifest.view.stored_sha256.as_deref() != Some(&digest(&json(&manifest.parts)?))
        {
            return Err(OutputError::Corrupt);
        }
        if manifest.parts.is_empty() || manifest.parts.len() > 3 {
            return Err(OutputError::Corrupt);
        }
        let mut streams = HashSet::new();
        let mut payload_bytes = 0u64;
        for part in &manifest.parts {
            payload_bytes = payload_bytes
                .checked_add(part.bytes)
                .ok_or(OutputError::Corrupt)?;
            if !streams.insert(format!("{:?}", part.stream))
                || !valid_digest(&part.sha256)
                || part.bytes > OUTPUT_BYTES as u64
                || part.start.checked_add(part.bytes).is_none()
                || part.blocks.len() != (part.bytes as usize).div_ceil(BLOCK_BYTES)
                || part.legacy_starts.first() != Some(&0)
                || part.legacy_starts.windows(2).any(|w| w[0] >= w[1])
                || part.legacy_starts.last().is_some_and(|p| *p > part.bytes)
            {
                return Err(OutputError::Corrupt);
            }
        }
        if payload_bytes != record.payload_bytes {
            return Err(OutputError::Corrupt);
        }
        Ok(manifest)
    }

    /// Only unleased owners can be evicted. Publication and cleanup share the process lock.
    fn clean(&self, catalog: &mut Catalog, pressure: bool) -> Result<(), OutputError> {
        let mut idle = HashSet::new();
        for owner in catalog
            .records
            .iter()
            .map(|r| &r.owner)
            .collect::<HashSet<_>>()
        {
            if owner == &self.owner_key {
                continue;
            }
            let lease = self
                .directory
                .file(&format!("{owner}.lease"), true, false)?;
            if lease.try_lock_exclusive().is_ok() {
                idle.insert(owner.clone());
            }
        }
        let before = catalog.records.len();
        let oldest = pressure
            .then(|| {
                catalog
                    .records
                    .iter()
                    .filter(|r| idle.contains(&r.owner))
                    .min_by_key(|r| r.updated)
                    .map(|r| r.id.clone())
            })
            .flatten();
        catalog.records.retain(|r| {
            !idle.contains(&r.owner)
                || (Some(&r.id) != oldest.as_ref() && now().saturating_sub(r.updated) < TTL)
        });
        if before != catalog.records.len() {
            self.commit(catalog)?;
        }

        // Orphans are never served. Under the catalog lock no publication is in progress.
        let mut keep: HashSet<String> = ["catalog.lock", "index.json"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        keep.insert(format!("{}.lease", self.owner_key));
        for record in &catalog.records {
            keep.insert(format!("{}.lease", record.owner));
            keep.insert(format!("{}.manifest", record.manifest));
            let bytes = self.directory.read(
                &format!("{}.manifest", record.manifest),
                MANIFEST_BYTES as u64,
            )?;
            if digest(&bytes) != record.manifest {
                return Err(OutputError::Corrupt);
            }
            let manifest: Manifest =
                serde_json::from_slice(&bytes).map_err(|_| OutputError::Corrupt)?;
            for part in manifest.parts {
                keep.insert(format!("{}.text", part.sha256));
            }
        }
        for (name, _) in self.directory.files()? {
            if !keep.contains(&name) && !name.ends_with(".lease") {
                self.directory.remove(&name);
            }
            if name.ends_with(".lease") && !keep.contains(&name) {
                let lease = self.directory.file(&name, false, false)?;
                if lease.try_lock_exclusive().is_ok() {
                    self.directory.remove(&name);
                }
            }
        }
        Ok(())
    }

    /// Input has already been sanitized as one complete view by OutputState.
    /// Running raw snapshots are withheld: unbounded regex matches may change earlier bytes.
    pub(crate) fn save(
        &self,
        view: &OutputView,
        document: &OutputDocument,
    ) -> Result<OutputView, OutputError> {
        if view.owner != self.owner {
            return Err(OutputError::OwnerMismatch);
        }
        if !valid_id(&view.output_id) {
            return Err(OutputError::SourceIncomplete);
        }
        if matches!(view.source, OutputSource::Unspecified) {
            return Err(OutputError::SourceIncomplete);
        }
        let lock = self.lock.lock().map_err(|_| OutputError::StorageFailed)?;
        lock.lock_exclusive()
            .map_err(|_| OutputError::StorageFailed)?;
        let _guard = CatalogLock(&lock);
        let mut catalog = self.catalog()?;
        let input_digest = digest(&json(&(
            &view.source,
            &view.capture,
            &view.execution,
            view.transformed,
            document
                .parts
                .iter()
                .map(|p| (p.stream, p.start, p.total, digest(p.text.as_bytes())))
                .collect::<Vec<_>>(),
        ))?);
        if let Some(record) = catalog.records.iter().find(|r| r.id == view.output_id) {
            let previous = self.manifest(record)?;
            if previous.view.capture != CaptureState::Running {
                if previous.input_digest != input_digest || previous.view.call_id != view.call_id {
                    return Err(OutputError::StaleSource);
                }
                return Ok(previous.view);
            }
            if previous.view.source != view.source || previous.view.call_id != view.call_id {
                return Err(OutputError::StaleSource);
            }
        }
        self.clean(&mut catalog, false)?;
        let prior = catalog.records.iter().find(|r| r.id == view.output_id);
        let owner_records: Vec<_> = catalog
            .records
            .iter()
            .filter(|r| r.session == self.session_key && r.id != view.output_id)
            .collect();
        if owner_records.len() >= SESSION_ENTRIES
            || (catalog.records.len() >= GLOBAL_ENTRIES && prior.is_none())
        {
            return Err(OutputError::StorageLimit);
        }
        let owner_bytes: u64 = owner_records.iter().map(|r| r.payload_bytes).sum();
        let owner_metadata: u64 = owner_records.iter().map(|r| r.metadata_bytes).sum();
        if view.capture == CaptureState::Running {
            let active = owner_records.iter().filter(|r| r.running).count();
            if active >= 8 {
                return Err(OutputError::StorageLimit);
            }
        }
        let mut remaining = OUTPUT_BYTES.min(SESSION_BYTES.saturating_sub(owner_bytes) as usize);
        let mut parts = Vec::new();
        let mut payloads = Vec::new();
        let mut stored = view.clone();
        stored.stored_ranges.clear();
        stored.stored_bytes = 0;
        let running = view.capture == CaptureState::Running;
        let mut limited = false;
        for part in &document.parts {
            if part.stream == OutputStream::Display {
                continue;
            }
            let stream_limit = if matches!(part.stream, OutputStream::Stdout | OutputStream::Stderr)
            {
                STREAM_BYTES
            } else {
                OUTPUT_BYTES
            };
            let mut end = if running {
                0
            } else {
                part.text.len().min(remaining).min(stream_limit)
            };
            while !part.text.is_char_boundary(end) {
                end -= 1;
            }
            limited |= !running && end < part.text.len();
            let text = &part.text[..end];
            remaining -= end;
            let sha256 = digest(text.as_bytes());
            let blocks = text
                .as_bytes()
                .chunks(BLOCK_BYTES)
                .map(|b| URL_SAFE_NO_PAD.encode(Sha256::digest(b)))
                .collect();
            let mut legacy_starts = vec![0];
            let mut position = 0;
            while text.len() - position > LEGACY_BODY_BYTES {
                let mut next = position + LEGACY_BODY_BYTES;
                while !text.is_char_boundary(next) {
                    next -= 1;
                }
                if let Some(nl) = text[position..next].rfind('\n') {
                    next = position + nl + 1;
                }
                legacy_starts.push(next as u64);
                position = next;
            }
            stored.stored_ranges.push(OutputRange {
                stream: part.stream,
                start: part.start,
                end: part.start + end as u64,
                lines: None,
            });
            stored.stored_bytes += end as u64;
            parts.push(StoredPart {
                stream: part.stream,
                start: part.start,
                bytes: end as u64,
                sha256: sha256.clone(),
                blocks,
                legacy_starts,
            });
            payloads.push((sha256, text.as_bytes()));
        }
        if parts.is_empty() {
            return Err(OutputError::SourceIncomplete);
        }
        if limited {
            stored.capture = CaptureState::Partial;
            stored.loss_reason = Some("storage_limit".into());
            if stored.stored_bytes == 0 {
                return Err(OutputError::StorageLimit);
            }
        }
        if running {
            stored.loss_reason = Some("safe_text_pending_finalization".into());
        }
        stored.availability = Availability::Available;
        stored.recoverable = true;
        stored.stored_sha256 = Some(digest(&json(&parts)?));
        let manifest = Manifest {
            version: SCHEMA,
            input_digest,
            view: stored.clone(),
            parts,
        };
        let metadata = json(&manifest)?;
        if metadata.len() > MANIFEST_BYTES || owner_metadata + metadata.len() as u64 > CATALOG_BYTES
        {
            return Err(OutputError::StorageLimit);
        }
        // Reserve payload, manifest, catalog and all temporary bytes before writing.
        let reservation = stored.stored_bytes + metadata.len() as u64 * 2 + CATALOG_BYTES;
        let disk_bytes = |store: &Self| -> Result<u64, OutputError> {
            Ok(store.directory.files()?.iter().map(|(_, n)| *n).sum())
        };
        while disk_bytes(self)?.saturating_add(reservation) > GLOBAL_BYTES {
            let before = catalog.records.len();
            self.clean(&mut catalog, true)?;
            if before == catalog.records.len() {
                return Err(OutputError::StorageLimit);
            }
        }
        for (sha, bytes) in payloads {
            let name = format!("{sha}.text");
            match self.directory.file(&name, false, false) {
                Ok(mut file) => {
                    let mut hasher = Sha256::new();
                    let mut block = [0; BLOCK_BYTES];
                    loop {
                        let n = file.read(&mut block).map_err(|_| OutputError::Corrupt)?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&block[..n]);
                    }
                    if format!("{:x}", hasher.finalize()) != sha {
                        return Err(OutputError::Corrupt);
                    }
                }
                Err(OutputError::Missing) => self.directory.atomic(&name, bytes)?,
                Err(e) => return Err(e),
            }
        }
        let manifest_id = digest(&metadata);
        self.directory
            .atomic(&format!("{manifest_id}.manifest"), &metadata)?;
        catalog.records.retain(|r| r.id != view.output_id);
        catalog.records.push(Record {
            id: view.output_id.clone(),
            owner: self.owner_key.clone(),
            call_id: view.call_id.clone(),
            session: self.session_key.clone(),
            running,
            manifest: manifest_id,
            payload_bytes: stored.stored_bytes,
            metadata_bytes: metadata.len() as u64,
            updated: now(),
        });
        self.commit(&catalog)?;
        Ok(stored)
    }

    fn resolve(
        &self,
        request: &RecallRequest,
    ) -> Result<(Record, Manifest, Option<Cursor>), OutputError> {
        if request.output_id.is_some() && request.tool_call_id.is_some()
            || request.cursor.is_some()
                && (request.offset.is_some()
                    || request.limit.is_some()
                    || request.page.is_some()
                    || request.tool_call_id.is_some()
                    || request.stream.is_some())
            || request.page.is_some()
                && (request.output_id.is_some()
                    || request.offset.is_some()
                    || request.limit.is_some())
            || request.limit == Some(0)
        {
            return Err(OutputError::InvalidCursor);
        }
        let cursor = request
            .cursor
            .as_ref()
            .map(|s| {
                if s.len() > 2048 {
                    return Err(OutputError::InvalidCursor);
                }
                let bytes = URL_SAFE_NO_PAD
                    .decode(s)
                    .map_err(|_| OutputError::InvalidCursor)?;
                let c: Cursor =
                    serde_json::from_slice(&bytes).map_err(|_| OutputError::InvalidCursor)?;
                if c.v != SCHEMA
                    || c.position > c.upper
                    || request.output_id.as_ref().is_some_and(|id| id != &c.id)
                {
                    return Err(OutputError::InvalidCursor);
                }
                Ok(c)
            })
            .transpose()?;
        let lock = self.lock.lock().map_err(|_| OutputError::StorageFailed)?;
        FileExt::lock_shared(&*lock).map_err(|_| OutputError::StorageFailed)?;
        let _guard = CatalogLock(&lock);
        let catalog = self.catalog()?;
        let id = cursor
            .as_ref()
            .map(|c| &c.id)
            .or(request.output_id.as_ref());
        let record = if let Some(id) = id {
            if !valid_id(id) {
                return Err(OutputError::InvalidCursor);
            }
            let record = catalog
                .records
                .iter()
                .find(|r| &r.id == id)
                .ok_or(OutputError::Missing)?;
            if record.owner != self.owner_key {
                return Err(OutputError::OwnerMismatch);
            }
            record
        } else if let Some(call_id) = &request.tool_call_id {
            let normalized = crate::agent::normalize_tool_call_id(call_id);
            let mut matches = catalog
                .records
                .iter()
                .filter(|r| r.owner == self.owner_key && r.call_id == normalized);
            let record = matches.next().ok_or(OutputError::SourceIncomplete)?;
            if matches.next().is_some() {
                return Err(OutputError::AmbiguousCallId);
            }
            record
        } else {
            return Err(OutputError::InvalidCursor);
        };
        let manifest = self.manifest(record)?;
        if cursor
            .as_ref()
            .is_some_and(|c| c.revision != record.manifest)
        {
            return Err(OutputError::StaleSource);
        }
        Ok((record.clone(), manifest, cursor))
    }

    pub fn status(&self, output_id: &str) -> Result<OutputView, OutputError> {
        let (_, manifest, _) = self.resolve(&RecallRequest {
            output_id: Some(output_id.into()),
            ..Default::default()
        })?;
        for part in &manifest.parts {
            if part.bytes == 0 {
                let file = self
                    .directory
                    .file(&format!("{}.text", part.sha256), false, false)?;
                if file.metadata().map_err(|_| OutputError::Corrupt)?.len() != 0 {
                    return Err(OutputError::Corrupt);
                }
            }
            for index in 0..part.blocks.len() {
                self.block(part, index)?;
            }
        }
        Ok(manifest.view)
    }

    fn block(&self, part: &StoredPart, index: usize) -> Result<Vec<u8>, OutputError> {
        let mut file = self
            .directory
            .file(&format!("{}.text", part.sha256), false, false)?;
        if file.metadata().map_err(|_| OutputError::Corrupt)?.len() != part.bytes {
            return Err(OutputError::Corrupt);
        }
        let base = index.checked_mul(BLOCK_BYTES).ok_or(OutputError::Corrupt)?;
        if base >= part.bytes as usize || index >= part.blocks.len() {
            return Err(OutputError::Corrupt);
        }
        let length = (part.bytes as usize - base).min(BLOCK_BYTES);
        let mut block = vec![0; length];
        file.seek(SeekFrom::Start(base as u64))
            .map_err(|_| OutputError::Corrupt)?;
        file.read_exact(&mut block)
            .map_err(|_| OutputError::Corrupt)?;
        if URL_SAFE_NO_PAD.encode(Sha256::digest(&block)) != part.blocks[index] {
            return Err(OutputError::Corrupt);
        }
        Ok(block)
    }

    fn char_boundary(&self, part: &StoredPart, position: u64) -> Result<bool, OutputError> {
        if position == 0 || position == part.bytes {
            return Ok(true);
        }
        if position > part.bytes {
            return Err(OutputError::OutOfRange);
        }
        let index = position as usize / BLOCK_BYTES;
        let block = self.block(part, index)?;
        Ok(block[position as usize % BLOCK_BYTES] & 0b1100_0000 != 0b1000_0000)
    }

    /// Verifies only touched blocks, with one 64 KiB block and a bounded page.
    fn range(&self, part: &StoredPart, start: u64, end: u64) -> Result<String, OutputError> {
        let mut result = Vec::with_capacity((end - start) as usize);
        if start == end {
            return Ok(String::new());
        }
        let first = start as usize / BLOCK_BYTES;
        let last = (end as usize - 1) / BLOCK_BYTES;
        for index in first..=last {
            let base = index * BLOCK_BYTES;
            let block = self.block(part, index)?;
            let from = start.saturating_sub(base as u64) as usize;
            let to = ((end - base as u64) as usize).min(block.len());
            result.extend_from_slice(&block[from..to]);
        }
        String::from_utf8(result).map_err(|_| OutputError::Corrupt)
    }

    pub fn read(
        &self,
        request: &RecallRequest,
        budget: usize,
    ) -> Result<RecalledOutput, OutputError> {
        let (record, manifest, cursor) = self.resolve(request)?;
        let stream = cursor
            .as_ref()
            .map(|c| c.stream)
            .or(request.stream)
            .unwrap_or(manifest.parts[0].stream);
        let part = manifest
            .parts
            .iter()
            .find(|p| p.stream == stream)
            .ok_or(OutputError::OutOfRange)?;
        let (position, upper) = if let Some(cursor) = cursor {
            (cursor.position, cursor.upper)
        } else if let Some(page) = request.page {
            let index = usize::try_from(page).map_err(|_| OutputError::OutOfRange)?;
            let start = *part
                .legacy_starts
                .get(index)
                .ok_or(OutputError::OutOfRange)?;
            let end = part
                .legacy_starts
                .get(index + 1)
                .copied()
                .unwrap_or(part.bytes);
            (part.start + start, part.start + end)
        } else {
            let start = request.offset.unwrap_or(part.start);
            let upper = match request.limit {
                Some(limit) => start.checked_add(limit).ok_or(OutputError::OutOfRange)?,
                None => part.start + part.bytes,
            };
            (start, upper)
        };
        if position < part.start || position > upper || upper > part.start + part.bytes {
            return Err(OutputError::OutOfRange);
        }
        if !self.char_boundary(part, position - part.start)?
            || !self.char_boundary(part, upper - part.start)?
        {
            return Err(OutputError::OutOfRange);
        }
        let mut page_end = (position - part.start + PAGE_BYTES as u64).min(upper - part.start);
        while !self.char_boundary(part, page_end)? {
            page_end -= 1;
        }
        let text = self.range(part, position - part.start, page_end)?;
        let mut document = OutputDocument {
            source: manifest.view.source.clone(),
            parts: vec![OutputPart {
                stream,
                text,
                start: position,
                first_line: None,
                total: manifest
                    .view
                    .source_totals
                    .iter()
                    .find(|(s, _)| *s == stream)
                    .and_then(|(_, total)| *total),
            }],
            capture: manifest.view.capture.clone(),
            execution: manifest.view.execution.clone(),
            transformed: manifest.view.transformed,
            loss_reason: manifest.view.loss_reason.clone(),
            file_read: None,
        };
        let mut view = manifest.view.clone();
        // render() must know the rest of the saved selection even when only one page was read.
        view.recovery_boundary = Some((stream, upper, record.manifest));
        view.historical = true;
        if upper < part.start + part.bytes {
            document.parts[0].total = None;
        }
        let rendered = render(&document, &view, budget)?;
        Ok(RecalledOutput { rendered, document })
    }
}

pub(crate) fn continuation_cursor(
    view: &OutputView,
    stream: OutputStream,
    position: u64,
) -> Option<String> {
    let (_, upper, revision) = view.recovery_boundary.as_ref()?;
    Some(
        URL_SAFE_NO_PAD.encode(
            json(&Cursor {
                v: SCHEMA,
                id: view.output_id.clone(),
                stream,
                position,
                upper: *upper,
                revision: revision.clone(),
            })
            .ok()?,
        ),
    )
}

#[cfg(test)]
#[path = "output_store_tests.rs"]
mod tests;
