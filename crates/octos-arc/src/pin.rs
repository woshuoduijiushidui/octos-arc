use std::path::Path;

use eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};

use crate::workspace::file_digest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRelease {
    pub version: String,
    pub source_commit: String,
    pub binary_sha256: String,
    pub target: String,
    pub archive_sha256: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct Lock {
    schema_version: u32,
    /// The downstream repository that published `runtime_release`. It is the
    /// authority for the release URL: a manifest may not name one repository
    /// and download the release from another.
    #[serde(default)]
    repository: Option<String>,
    runtime_release: Option<RuntimeRelease>,
}

pub struct BinaryIdentity<'identity> {
    pub executable: &'identity Path,
    pub source_commit: &'identity str,
    pub target: &'identity str,
    pub dirty: bool,
}

fn hexadecimal(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The bytes GitHub allows in an owner or repository path segment.
fn segment_is_valid(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// `owner/repository` and nothing else. The value is interpolated into the
/// release URL, so `.`/`..` and extra segments are refused here rather than
/// left to the URL checks to notice.
fn repository_is_well_formed(value: &str) -> bool {
    let segments: Vec<&str> = value.split('/').collect();
    segments.len() == 2 && segments.iter().all(|segment| segment_is_valid(segment))
}

pub fn verify(path: &Path, identity: &BinaryIdentity<'_>) -> Result<RuntimeRelease> {
    let lock: Lock = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(lock.schema_version == 1, "Unsupported runtime lock version");
    let release = lock
        .runtime_release
        .ok_or_else(|| eyre!("Source-only baseline: no fixed runtime release has been built"))?;
    ensure!(
        !identity.dirty,
        "Refusing a binary built from tracked uncommitted changes"
    );
    ensure!(
        hexadecimal(&release.source_commit, 40) && release.source_commit == identity.source_commit,
        "Runtime source commit mismatch"
    );
    ensure!(
        release.target == identity.target,
        "Runtime build target mismatch"
    );
    ensure!(
        !release.version.trim().is_empty()
            && !release.version.to_ascii_lowercase().contains("latest"),
        "Runtime version must be fixed"
    );
    let repository = lock.repository.as_deref().ok_or_else(|| {
        eyre!("Runtime lock must name the downstream repository that published the release")
    })?;
    ensure!(
        repository_is_well_formed(repository),
        "Runtime repository must be an owner/repository pair"
    );
    let prefix = format!(
        "https://github.com/{repository}/releases/download/{}/",
        release.version
    );
    ensure!(
        release.url.starts_with(&prefix)
            && !release.url.contains(['?', '#'])
            && !release.url.contains("/../"),
        "Runtime URL must identify this downstream versioned release"
    );
    ensure!(
        hexadecimal(&release.archive_sha256, 64),
        "Missing archive checksum"
    );
    ensure!(
        hexadecimal(&release.binary_sha256, 64)
            && file_digest(identity.executable)? == release.binary_sha256,
        "Runtime binary checksum mismatch"
    );
    Ok(release)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// A runtime lock plus the executable it pins, so each test only has to
    /// name the downstream repository and the release URL it claims.
    struct Fixture {
        _temp: tempfile::TempDir,
        lock: std::path::PathBuf,
        binary: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let binary = temp.path().join("octos");
            std::fs::write(&binary, "test executable bytes").unwrap();
            let lock = temp.path().join("lock.json");
            Self {
                _temp: temp,
                lock,
                binary,
            }
        }

        fn identity(&self) -> BinaryIdentity<'_> {
            BinaryIdentity {
                executable: &self.binary,
                source_commit: "1111111111111111111111111111111111111111",
                target: "test",
                dirty: false,
            }
        }

        /// The manifest of a release published by `repository` at `url`.
        fn manifest(&self, repository: Option<&str>, url: &str) -> Value {
            let mut value = json!({
                "schema_version": 1,
                "runtime_release": {
                    "version": "arc-v1",
                    "source_commit": "1111111111111111111111111111111111111111",
                    "target": "test",
                    "binary_sha256": file_digest(&self.binary).unwrap(),
                    "archive_sha256": "a".repeat(64),
                    "url": url,
                }
            });
            if let Some(repository) = repository {
                value["repository"] = json!(repository);
            }
            value
        }

        fn write(&self, value: &Value) {
            std::fs::write(&self.lock, serde_json::to_vec(value).unwrap()).unwrap();
        }
    }

    #[test]
    fn rejects_source_only_and_wrong_binaries() {
        let fixture = Fixture::new();
        let identity = fixture.identity();
        std::fs::write(
            &fixture.lock,
            r#"{"schema_version":1,"runtime_release":null}"#,
        )
        .unwrap();
        assert!(
            verify(&fixture.lock, &identity)
                .unwrap_err()
                .to_string()
                .contains("Source-only")
        );
        let mut value = fixture.manifest(
            Some("octos-org/octos-arc"),
            "https://github.com/octos-org/octos-arc/releases/download/arc-v1/runtime.tar.gz",
        );
        fixture.write(&value);
        assert!(verify(&fixture.lock, &identity).is_ok());
        value["runtime_release"]["url"] =
            json!("https://github.com/octos-org/octos/releases/latest/download/runtime.tar.gz");
        fixture.write(&value);
        assert!(verify(&fixture.lock, &identity).is_err());
        value["runtime_release"]["url"] =
            json!("https://github.com/octos-org/octos-arc/releases/download/arc-v1/runtime.tar.gz");
        fixture.write(&value);
        std::fs::write(&fixture.binary, "other binary").unwrap();
        assert!(
            verify(&fixture.lock, &identity)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
    }

    /// The acceptance check for a release bump: the shipped manifest must
    /// verify against the binary it was published with. That binary is a Linux
    /// release artifact, so this is opt-in rather than part of the default run.
    ///
    /// `OCTOS_ARC_RELEASED_BINARY=<path to the extracted octos> cargo test -p
    /// octos-arc should_verify_the_shipped_manifest -- --ignored`
    #[test]
    #[ignore = "needs the released binary via OCTOS_ARC_RELEASED_BINARY"]
    fn should_verify_the_shipped_manifest_against_the_released_binary() {
        let binary = std::env::var("OCTOS_ARC_RELEASED_BINARY")
            .expect("OCTOS_ARC_RELEASED_BINARY must point at the released executable");
        let lock =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../arc-runtime-lock.json");
        let value: Value = serde_json::from_slice(&std::fs::read(&lock).unwrap()).unwrap();
        let release = &value["runtime_release"];
        let identity = BinaryIdentity {
            executable: std::path::Path::new(&binary),
            source_commit: release["source_commit"].as_str().unwrap(),
            target: release["target"].as_str().unwrap(),
            dirty: false,
        };
        let verified = verify(&lock, &identity).unwrap();
        assert_eq!(verified.version, release["version"].as_str().unwrap());
        assert_eq!(verified.url, release["url"].as_str().unwrap());
    }

    /// The fork publishes its own releases, so a manifest that names the fork
    /// and points at the fork's release must verify.
    #[test]
    fn should_accept_release_url_from_the_repository_named_in_the_lock() {
        let fixture = Fixture::new();
        fixture.write(&fixture.manifest(
            Some("woshuoduijiushidui/octos-arc"),
            "https://github.com/woshuoduijiushidui/octos-arc/releases/download/arc-v1/runtime.tar.gz",
        ));
        assert!(verify(&fixture.lock, &fixture.identity()).is_ok());
    }

    /// A manifest may not name one downstream repository while downloading from
    /// another: that is how a fork silently inherits upstream's binary.
    #[test]
    fn should_reject_release_url_from_a_foreign_repository() {
        let fixture = Fixture::new();
        fixture.write(&fixture.manifest(
            Some("woshuoduijiushidui/octos-arc"),
            "https://github.com/octos-org/octos-arc/releases/download/arc-v1/runtime.tar.gz",
        ));
        let error = verify(&fixture.lock, &fixture.identity())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Runtime URL"), "{error}");
    }

    #[test]
    fn should_reject_release_that_does_not_name_its_repository() {
        let fixture = Fixture::new();
        fixture.write(&fixture.manifest(
            None,
            "https://github.com/octos-org/octos-arc/releases/download/arc-v1/runtime.tar.gz",
        ));
        let error = verify(&fixture.lock, &fixture.identity())
            .unwrap_err()
            .to_string();
        assert!(error.contains("repository"), "{error}");
    }

    /// The repository is interpolated into the release URL, so anything that is
    /// not a plain `owner/repository` pair must be refused outright.
    #[test]
    fn should_reject_repository_that_is_not_an_owner_repository_pair() {
        for repository in [
            "",
            "octos-org",
            "a/b/c",
            "octos-org/",
            "/octos-arc",
            "octos-org/octos-arc?x=1",
            "octos-org/octos-arc#fragment",
            "../evil/x",
            "octos-org/..",
            "octos org/octos-arc",
        ] {
            let fixture = Fixture::new();
            fixture.write(&fixture.manifest(
                Some(repository),
                &format!("https://github.com/{repository}/releases/download/arc-v1/runtime.tar.gz"),
            ));
            let error = verify(&fixture.lock, &fixture.identity())
                .expect_err(&format!("{repository:?} must not be accepted"))
                .to_string();
            assert!(error.contains("repository"), "{repository:?}: {error}");
        }
    }
}
