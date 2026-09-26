use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use wasmtime::{Engine, component::Component};

use crate::{ComponentDigest, sha256_hex};

const MANIFEST_LIMIT_BYTES: u64 = 64 * 1024;
const ARTIFACT_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct PrivateArtifactCache {
    root: Arc<PathBuf>,
}

impl PrivateArtifactCache {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CacheOpenError> {
        let path = path.into();
        create_private_directory(&path).map_err(CacheOpenError::Io)?;
        let metadata = fs::symlink_metadata(&path).map_err(CacheOpenError::Io)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(CacheOpenError::NotPrivateDirectory);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.mode() & 0o022 != 0 || metadata.uid() != effective_uid() {
                return Err(CacheOpenError::NotPrivateDirectory);
            }
        }
        Ok(Self {
            root: Arc::new(path),
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_ref()
    }
}

#[derive(Debug)]
pub enum CacheOpenError {
    Io(io::Error),
    NotPrivateDirectory,
}

impl std::fmt::Display for CacheOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "cannot open artifact cache: {error}"),
            Self::NotPrivateDirectory => formatter.write_str(
                "artifact cache must be a real effective-UID-owned directory not writable by group or other users",
            ),
        }
    }
}

impl std::error::Error for CacheOpenError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheRejection {
    ManifestTooLarge,
    ManifestUnreadable(String),
    ManifestInvalid(String),
    ComponentDigest,
    ExecutorBuildIdentity,
    WasmtimeVersion,
    Target,
    EngineCompatibility,
    ConfigIdentity,
    ArtifactLength,
    ArtifactDigest,
    ArtifactUnreadable(String),
    UntrustedOwnership(String),
    Deserialization(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDisposition {
    MemoryHit,
    DiskHit,
    CompiledAndStored,
    CompiledWithoutCache,
    RejectedAndCompiled(CacheRejection),
    CompiledButNotStored(String),
}

#[derive(Clone, Serialize)]
pub(crate) struct CacheIdentity {
    pub(crate) executor_build_identity: String,
    pub(crate) wasmtime_version: &'static str,
    pub(crate) target: &'static str,
    pub(crate) engine_compatibility: String,
    pub(crate) config_identity: String,
}

impl CacheIdentity {
    fn key(&self) -> String {
        let encoded =
            serde_json::to_vec(self).expect("serializing cache identity fields cannot fail");
        sha256_hex(&encoded)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArtifactManifest {
    component_digest: String,
    executor_build_identity: String,
    wasmtime_version: String,
    target: String,
    engine_compatibility: String,
    config_identity: String,
    artifact_length: u64,
    artifact_digest: String,
}

pub(crate) enum CacheLookup {
    Miss,
    Hit(Component),
    Rejected(CacheRejection),
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecuredEntryKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct SecurityMetadata {
    kind: SecuredEntryKind,
    uid: u32,
    mode: u32,
}

#[cfg(unix)]
trait SecurityMetadataOracle {
    fn metadata(&self, path: &Path) -> Result<SecurityMetadata, String>;
}

#[cfg(unix)]
struct FilesystemMetadata;

#[cfg(unix)]
impl SecurityMetadataOracle for FilesystemMetadata {
    fn metadata(&self, path: &Path) -> Result<SecurityMetadata, String> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            SecuredEntryKind::Symlink
        } else if metadata.is_dir() {
            SecuredEntryKind::Directory
        } else if metadata.is_file() {
            SecuredEntryKind::RegularFile
        } else {
            SecuredEntryKind::Other
        };
        Ok(SecurityMetadata {
            kind,
            uid: metadata.uid(),
            mode: metadata.mode(),
        })
    }
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum OwnershipViolation {
    OutsideRoot,
    Metadata {
        path: PathBuf,
        error: String,
    },
    WrongKind {
        path: PathBuf,
        expected: SecuredEntryKind,
        actual: SecuredEntryKind,
    },
    WrongOwner {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },
    WritableByOther {
        path: PathBuf,
    },
}

#[cfg(unix)]
impl std::fmt::Display for OwnershipViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoot => formatter.write_str("cache entry is outside the cache root"),
            Self::Metadata { path, error } => {
                write!(formatter, "cannot inspect {}: {error}", path.display())
            }
            Self::WrongKind {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "{} has kind {actual:?}; expected {expected:?}",
                path.display(),
            ),
            Self::WrongOwner {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "{} is owned by UID {actual}; expected effective UID {expected}",
                path.display(),
            ),
            Self::WritableByOther { path } => write!(
                formatter,
                "{} is writable by group or other users",
                path.display(),
            ),
        }
    }
}

#[cfg(unix)]
fn validate_owned_cache_entry_with(
    root: &Path,
    entry: &Path,
    effective_uid: u32,
    oracle: &impl SecurityMetadataOracle,
) -> Result<(), OwnershipViolation> {
    let relative = entry
        .strip_prefix(root)
        .map_err(|_| OwnershipViolation::OutsideRoot)?;
    validate_secured_entry(root, SecuredEntryKind::Directory, effective_uid, oracle)?;
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(OwnershipViolation::OutsideRoot);
        };
        current.push(component);
        let expected = if components.peek().is_some() {
            SecuredEntryKind::Directory
        } else {
            SecuredEntryKind::RegularFile
        };
        validate_secured_entry(&current, expected, effective_uid, oracle)?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_secured_entry(
    path: &Path,
    expected: SecuredEntryKind,
    effective_uid: u32,
    oracle: &impl SecurityMetadataOracle,
) -> Result<(), OwnershipViolation> {
    let metadata = oracle
        .metadata(path)
        .map_err(|error| OwnershipViolation::Metadata {
            path: path.to_path_buf(),
            error,
        })?;
    if metadata.kind != expected {
        return Err(OwnershipViolation::WrongKind {
            path: path.to_path_buf(),
            expected,
            actual: metadata.kind,
        });
    }
    if metadata.uid != effective_uid {
        return Err(OwnershipViolation::WrongOwner {
            path: path.to_path_buf(),
            expected: effective_uid,
            actual: metadata.uid,
        });
    }
    if metadata.mode & 0o022 != 0 {
        return Err(OwnershipViolation::WritableByOther {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and reads process credentials.
    unsafe { libc::geteuid() }
}

pub(crate) fn load(
    cache: &PrivateArtifactCache,
    engine: &Engine,
    component_digest: ComponentDigest,
    identity: &CacheIdentity,
) -> CacheLookup {
    let manifest_path = manifest_path(cache, component_digest, identity);
    let manifest_bytes = match read_regular_bounded(&manifest_path, MANIFEST_LIMIT_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return CacheLookup::Miss,
        Err(ReadError::TooLarge) => {
            return CacheLookup::Rejected(CacheRejection::ManifestTooLarge);
        }
        Err(ReadError::Io(error)) => {
            return CacheLookup::Rejected(CacheRejection::ManifestUnreadable(error));
        }
    };
    let manifest: ArtifactManifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            return CacheLookup::Rejected(CacheRejection::ManifestInvalid(error.to_string()));
        }
    };
    if manifest.component_digest != component_digest.to_string() {
        return CacheLookup::Rejected(CacheRejection::ComponentDigest);
    }
    if manifest.executor_build_identity != identity.executor_build_identity {
        return CacheLookup::Rejected(CacheRejection::ExecutorBuildIdentity);
    }
    if manifest.wasmtime_version != identity.wasmtime_version {
        return CacheLookup::Rejected(CacheRejection::WasmtimeVersion);
    }
    if manifest.target != identity.target {
        return CacheLookup::Rejected(CacheRejection::Target);
    }
    if manifest.engine_compatibility != identity.engine_compatibility {
        return CacheLookup::Rejected(CacheRejection::EngineCompatibility);
    }
    if manifest.config_identity != identity.config_identity {
        return CacheLookup::Rejected(CacheRejection::ConfigIdentity);
    }
    if manifest.artifact_length > ARTIFACT_LIMIT_BYTES {
        return CacheLookup::Rejected(CacheRejection::ArtifactLength);
    }
    if !is_sha256(&manifest.artifact_digest) {
        return CacheLookup::Rejected(CacheRejection::ArtifactDigest);
    }
    let artifact_path = cache
        .root()
        .join("objects")
        .join(format!("{}.cwasm", manifest.artifact_digest));
    let artifact = match read_regular_bounded(&artifact_path, manifest.artifact_length) {
        Ok(Some(bytes)) if bytes.len() as u64 == manifest.artifact_length => bytes,
        Ok(Some(_)) | Err(ReadError::TooLarge) => {
            return CacheLookup::Rejected(CacheRejection::ArtifactLength);
        }
        Ok(None) => {
            return CacheLookup::Rejected(CacheRejection::ArtifactUnreadable(
                "artifact is missing".to_owned(),
            ));
        }
        Err(ReadError::Io(error)) => {
            return CacheLookup::Rejected(CacheRejection::ArtifactUnreadable(error));
        }
    };
    if sha256_hex(&artifact) != manifest.artifact_digest {
        return CacheLookup::Rejected(CacheRejection::ArtifactDigest);
    }
    #[cfg(unix)]
    {
        let oracle = FilesystemMetadata;
        for path in [&manifest_path, &artifact_path] {
            if let Err(error) =
                validate_owned_cache_entry_with(cache.root(), path, effective_uid(), &oracle)
            {
                return CacheLookup::Rejected(CacheRejection::UntrustedOwnership(
                    error.to_string(),
                ));
            }
        }
    }

    // The cache root, internal ancestors, manifest and artifact are effective-UID-owned and
    // non-writable by other users, and every manifest field plus the exact artifact bytes has
    // been verified before crossing Wasmtime's unsafe AOT boundary.
    let component = unsafe { Component::deserialize(engine, &artifact) };
    match component {
        Ok(component) => CacheLookup::Hit(component),
        Err(error) => CacheLookup::Rejected(CacheRejection::Deserialization(error.to_string())),
    }
}

pub(crate) fn store(
    cache: &PrivateArtifactCache,
    component: &Component,
    component_digest: ComponentDigest,
    identity: &CacheIdentity,
) -> Result<(), String> {
    let artifact = component.serialize().map_err(|error| error.to_string())?;
    if artifact.len() as u64 > ARTIFACT_LIMIT_BYTES {
        return Err("serialized component exceeds the artifact cache limit".to_owned());
    }
    let artifact_digest = sha256_hex(&artifact);
    let artifact_length = artifact.len() as u64;
    let object_directory = cache.root().join("objects");
    create_private_directory(&object_directory).map_err(|error| error.to_string())?;
    let artifact_path = object_directory.join(format!("{artifact_digest}.cwasm"));
    create_immutable(&artifact_path, &artifact)?;

    let manifest = ArtifactManifest {
        component_digest: component_digest.to_string(),
        executor_build_identity: identity.executor_build_identity.clone(),
        wasmtime_version: identity.wasmtime_version.to_owned(),
        target: identity.target.to_owned(),
        engine_compatibility: identity.engine_compatibility.clone(),
        config_identity: identity.config_identity.clone(),
        artifact_length,
        artifact_digest,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
    let path = manifest_path(cache, component_digest, identity);
    let directory = path
        .parent()
        .expect("manifest paths always have a parent directory");
    create_private_directory(directory).map_err(|error| error.to_string())?;
    create_immutable(&path, &manifest_bytes)
}

fn manifest_path(
    cache: &PrivateArtifactCache,
    component_digest: ComponentDigest,
    identity: &CacheIdentity,
) -> PathBuf {
    cache
        .root()
        .join("manifests")
        .join(component_digest.to_string())
        .join(format!("{}.json", identity.key()))
}

fn create_immutable(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "cache entry path has no parent directory".to_owned())?;
    let (temporary_path, mut file) = loop {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = path
            .file_name()
            .ok_or_else(|| "cache entry path has no file name".to_owned())?
            .to_string_lossy();
        let temporary_path = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&temporary_path) {
            Ok(file) => break (temporary_path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    };

    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(error.to_string());
    }
    drop(file);

    match publish_no_replace(&temporary_path, path) {
        Ok(()) => sync_directory(parent).map_err(|error| error.to_string()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary_path);
            verify_immutable(path, bytes)?;
            sync_directory(parent).map_err(|error| error.to_string())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            Err(error.to_string())
        }
    }
}

fn verify_immutable(path: &Path, bytes: &[u8]) -> Result<(), String> {
    match read_regular_bounded(path, bytes.len() as u64) {
        Ok(Some(existing)) if existing == bytes => Ok(()),
        Ok(Some(_)) | Err(ReadError::TooLarge) => {
            Err("immutable cache entry already exists with different bytes".to_owned())
        }
        Ok(None) => Err("immutable cache entry disappeared during publication".to_owned()),
        Err(ReadError::Io(error)) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn publish_no_replace(from: &Path, to: &Path) -> Result<(), io::Error> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let c_from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "temporary path contains NUL"))?;
    let c_to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cache path contains NUL"))?;
    // Both paths are in the same private cache directory. RENAME_NOREPLACE gives publication
    // a single atomic winner without ever exposing the temporary file at the final name.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(code) if code == libc::ENOSYS || code == libc::EINVAL
    ) {
        return publish_with_hard_link(from, to);
    }
    Err(error)
}

#[cfg(target_os = "macos")]
fn publish_no_replace(from: &Path, to: &Path) -> Result<(), io::Error> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "temporary path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cache path contains NUL"))?;
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn publish_no_replace(from: &Path, to: &Path) -> Result<(), io::Error> {
    publish_with_hard_link(from, to)
}

#[cfg(not(target_os = "macos"))]
fn publish_with_hard_link(from: &Path, to: &Path) -> Result<(), io::Error> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

fn sync_directory(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

enum ReadError {
    TooLarge,
    Io(String),
}

fn read_regular_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, ReadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReadError::Io(error.to_string())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| ReadError::Io(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ReadError::Io("entry is not a regular file".to_owned()));
    }
    if metadata.len() > limit {
        return Err(ReadError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ReadError::Io(error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(ReadError::TooLarge);
    }
    Ok(Some(bytes))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    use super::*;

    fn identity() -> CacheIdentity {
        CacheIdentity {
            executor_build_identity: "executor-a".to_owned(),
            wasmtime_version: "48.0.1",
            target: "x86_64-unknown-linux-gnu",
            engine_compatibility: "compat-a".to_owned(),
            config_identity: "config-a".to_owned(),
        }
    }

    #[cfg(unix)]
    struct FakeMetadata {
        entries: HashMap<PathBuf, SecurityMetadata>,
    }

    #[cfg(unix)]
    impl SecurityMetadataOracle for FakeMetadata {
        fn metadata(&self, path: &Path) -> Result<SecurityMetadata, String> {
            self.entries
                .get(path)
                .copied()
                .ok_or_else(|| "missing fake metadata".to_owned())
        }
    }

    #[cfg(unix)]
    fn owned_manifest_tree(uid: u32) -> (PathBuf, PathBuf, FakeMetadata) {
        let root = PathBuf::from("/cache");
        let manifest_directory = root.join("manifests");
        let digest_directory = manifest_directory.join("digest");
        let manifest = digest_directory.join("identity.json");
        let entries = [
            (
                root.clone(),
                SecurityMetadata {
                    kind: SecuredEntryKind::Directory,
                    uid,
                    mode: 0o700,
                },
            ),
            (
                manifest_directory,
                SecurityMetadata {
                    kind: SecuredEntryKind::Directory,
                    uid,
                    mode: 0o700,
                },
            ),
            (
                digest_directory,
                SecurityMetadata {
                    kind: SecuredEntryKind::Directory,
                    uid,
                    mode: 0o700,
                },
            ),
            (
                manifest.clone(),
                SecurityMetadata {
                    kind: SecuredEntryKind::RegularFile,
                    uid,
                    mode: 0o600,
                },
            ),
        ]
        .into_iter()
        .collect();
        (root, manifest, FakeMetadata { entries })
    }

    #[cfg(unix)]
    #[test]
    fn ownership_policy_checks_root_ancestors_and_entry() {
        let effective_uid = 42;
        let (root, manifest, trusted) = owned_manifest_tree(effective_uid);
        assert_eq!(
            validate_owned_cache_entry_with(&root, &manifest, effective_uid, &trusted),
            Ok(())
        );

        for path in [
            root.clone(),
            root.join("manifests"),
            root.join("manifests/digest"),
            manifest.clone(),
        ] {
            let (_, _, mut untrusted) = owned_manifest_tree(effective_uid);
            untrusted.entries.get_mut(&path).unwrap().uid = 7;
            assert_eq!(
                validate_owned_cache_entry_with(&root, &manifest, effective_uid, &untrusted,),
                Err(OwnershipViolation::WrongOwner {
                    path,
                    expected: effective_uid,
                    actual: 7,
                })
            );
        }
    }

    #[test]
    fn cache_key_covers_every_compatibility_field() {
        let base = identity();
        let mut variants = Vec::new();

        let mut changed = base.clone();
        changed.executor_build_identity = "executor-b".to_owned();
        variants.push(changed);
        let mut changed = base.clone();
        changed.wasmtime_version = "48.0.2";
        variants.push(changed);
        let mut changed = base.clone();
        changed.target = "aarch64-unknown-linux-gnu";
        variants.push(changed);
        let mut changed = base.clone();
        changed.engine_compatibility = "compat-b".to_owned();
        variants.push(changed);
        let mut changed = base.clone();
        changed.config_identity = "config-b".to_owned();
        variants.push(changed);

        let keys = std::iter::once(base.key())
            .chain(variants.iter().map(CacheIdentity::key))
            .collect::<HashSet<_>>();
        assert_eq!(keys.len(), variants.len() + 1);
    }

    #[test]
    fn concurrent_immutable_writers_publish_one_complete_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = Arc::new(directory.path().join("entry"));
        let bytes = Arc::new(vec![0x5a; 64 * 1024]);
        let barrier = Arc::new(Barrier::new(8));
        let writers = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let bytes = Arc::clone(&bytes);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    create_immutable(&path, &bytes)
                })
            })
            .collect::<Vec<_>>();

        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        assert_eq!(fs::read(path.as_ref()).unwrap(), *bytes);
        let residue = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(residue, 0);
    }

    #[test]
    fn truncated_crash_temp_is_never_published_as_the_final_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("entry");
        let residue = directory.path().join(".entry.tmp-crash");
        fs::write(&residue, b"partial").unwrap();

        create_immutable(&path, b"complete immutable bytes").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"complete immutable bytes");
        assert_eq!(fs::read(residue).unwrap(), b"partial");
    }

    #[test]
    fn truncated_final_entry_is_rejected_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("entry");
        fs::write(&path, b"partial").unwrap();

        let error = create_immutable(&path, b"complete immutable bytes").unwrap_err();

        assert!(error.contains("different bytes"));
        assert_eq!(fs::read(path).unwrap(), b"partial");
    }
}
