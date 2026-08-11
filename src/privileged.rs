use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::digest;
use crate::error::{AppError, Context, Result};

const STAGING_PARENT: &str = "/var/tmp";
const SYSTEM_SHA256SUM: &str = "/usr/bin/sha256sum";
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const O_NOFOLLOW: i32 = 0o400000;
const O_CLOEXEC: i32 = 0o2000000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub expected_sha256: String,
    pub package: PathBuf,
}

pub struct StagedPackage {
    root: PathBuf,
    path: PathBuf,
}

impl StagedPackage {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedPackage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn run<I, S>(arguments: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let request = parse_request(arguments)?;
    let caller_uid = verify_execution_context()?;
    let staged = stage_verified_package(
        &request.package,
        &request.expected_sha256,
        caller_uid,
        Path::new(STAGING_PARENT),
    )?;
    validate_and_install(staged.path())
}

pub fn parse_request<I, S>(arguments: I) -> Result<InstallRequest>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    if arguments.len() != 4 || arguments[0] != "--sha256" || arguments[2] != "--" {
        return Err(AppError::new(
            "Usage: debforge-helper --sha256 DIGEST -- PACKAGE",
        ));
    }
    let digest_text = arguments[1]
        .to_str()
        .ok_or_else(|| AppError::new("The expected SHA-256 value is not valid UTF-8."))?;
    let expected_sha256 = digest::normalize_sha256(digest_text)?;
    let package = PathBuf::from(&arguments[3]);
    if !package.is_absolute() {
        return Err(AppError::new("The package path must be absolute."));
    }
    Ok(InstallRequest {
        expected_sha256,
        package,
    })
}

fn verify_execution_context() -> Result<u32> {
    let status = fs::read_to_string("/proc/self/status")
        .context("Cannot read the process identity from /proc/self/status")?;
    let effective_uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|value| value.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| AppError::new("Cannot determine the effective user ID."))?;
    if effective_uid != 0 {
        return Err(AppError::new(
            "The secure Debforge helper must run as root through Polkit.",
        ));
    }

    let caller_uid = env::var("PKEXEC_UID")
        .context("PKEXEC_UID is absent. Start the helper through pkexec.")?
        .parse::<u32>()
        .context("PKEXEC_UID is not a valid user ID")?;
    if caller_uid == 0 {
        return Err(AppError::new(
            "The Debforge helper requires a non-root desktop caller.",
        ));
    }
    Ok(caller_uid)
}

pub fn stage_verified_package(
    source_path: &Path,
    expected_sha256: &str,
    caller_uid: u32,
    staging_parent: &Path,
) -> Result<StagedPackage> {
    let expected_sha256 = digest::normalize_sha256(expected_sha256)?;
    let mut source_options = OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(O_NOFOLLOW | O_CLOEXEC);
    let mut source = source_options.open(source_path).context(format!(
        "Cannot securely open converted package {}",
        source_path.display()
    ))?;
    let source_metadata = source.metadata().context(format!(
        "Cannot inspect converted package {}",
        source_path.display()
    ))?;
    if !source_metadata.is_file() {
        return Err(AppError::new(
            "The converted package is not a regular file.",
        ));
    }
    if source_metadata.uid() != caller_uid {
        return Err(AppError::new(
            "The converted package is not owned by the authorizing user.",
        ));
    }
    if source_metadata.len() == 0 || source_metadata.len() > MAX_PACKAGE_BYTES {
        return Err(AppError::new(
            "The converted package size is outside the supported range.",
        ));
    }

    let root = create_private_staging(staging_parent)?;
    let mut guard = StagingGuard::new(root.clone());
    let staged_path = root.join("package.pkg.tar.zst");
    let mut staged = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_CLOEXEC)
        .open(&staged_path)
        .context(format!(
            "Cannot create secure package copy {}",
            staged_path.display()
        ))?;
    let copied = io::copy(&mut source, &mut staged).context("Cannot copy the converted package")?;
    if copied != source_metadata.len() {
        return Err(AppError::new(
            "The converted package changed while the secure copy was created.",
        ));
    }
    staged
        .flush()
        .context("Cannot flush the secure package copy")?;
    staged
        .sync_all()
        .context("Cannot synchronize the secure package copy")?;
    drop(staged);

    let actual_sha256 = digest::sha256_file_with_tool(&staged_path, Path::new(SYSTEM_SHA256SUM))?;
    if actual_sha256 != expected_sha256 {
        return Err(AppError::new(
            "The converted package changed after the installation review.",
        ));
    }

    guard.disarm();
    Ok(StagedPackage {
        root,
        path: staged_path,
    })
}

fn create_private_staging(parent: &Path) -> Result<PathBuf> {
    let parent_metadata = fs::metadata(parent).context(format!(
        "Cannot inspect staging parent {}",
        parent.display()
    ))?;
    if !parent_metadata.is_dir() {
        return Err(AppError::new(format!(
            "The staging parent is not a directory: {}",
            parent.display()
        )));
    }

    for _ in 0..32 {
        let nonce = random_nonce()?;
        let candidate = parent.join(format!("debforge-install-{nonce}"));
        match DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::new(format!(
                    "Cannot create secure staging directory {}: {error}",
                    candidate.display()
                )));
            }
        }
    }
    Err(AppError::new(
        "Cannot allocate a unique secure staging directory.",
    ))
}

fn random_nonce() -> Result<String> {
    let mut random = File::open("/dev/urandom").context("Cannot open /dev/urandom")?;
    let mut bytes = [0_u8; 16];
    random
        .read_exact(&mut bytes)
        .context("Cannot read a secure random value")?;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(output)
}

fn validate_and_install(package: &Path) -> Result<()> {
    let pacman = Path::new("/usr/bin/pacman");
    if !pacman.is_file() {
        return Err(AppError::new("Pacman is not installed at /usr/bin/pacman."));
    }

    let validation = Command::new(pacman)
        .args([OsStr::new("-Qp"), OsStr::new("--")])
        .arg(package)
        .stdin(Stdio::null())
        .status()
        .context("Cannot start Pacman package validation")?;
    if !validation.success() {
        return Err(AppError::new(format!(
            "Pacman rejected the secure package copy (status {validation})."
        )));
    }

    let installation = Command::new(pacman)
        .args([
            OsStr::new("-U"),
            OsStr::new("--needed"),
            OsStr::new("--noconfirm"),
            OsStr::new("--"),
        ])
        .arg(package)
        .status()
        .context("Cannot start the Pacman transaction")?;
    if installation.success() {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "Pacman could not install the package (status {installation})."
        )))
    }
}

struct StagingGuard {
    path: PathBuf,
    armed: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    use super::{parse_request, stage_verified_package};
    use crate::digest;

    fn test_root(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("debforge-helper-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("test root");
        path
    }

    #[test]
    fn parses_only_the_fixed_helper_interface() {
        let digest = "a".repeat(64);
        let request = parse_request([
            OsString::from("--sha256"),
            OsString::from(digest.clone()),
            OsString::from("--"),
            OsString::from("/tmp/package.pkg.tar.zst"),
        ])
        .expect("request");
        assert_eq!(request.expected_sha256, digest);
        assert!(parse_request([OsString::from("/tmp/package")]).is_err());
        assert!(
            parse_request([
                OsString::from("--sha256"),
                OsString::from("bad"),
                OsString::from("--"),
                OsString::from("/tmp/package"),
            ])
            .is_err()
        );
    }

    #[test]
    fn stages_one_verified_regular_file_and_rejects_a_link() {
        let root = test_root("stage");
        let source = root.join("source.pkg.tar.zst");
        fs::write(&source, b"verified package bytes").expect("source");
        let digest = digest::sha256_file(&source).expect("digest");
        let uid = fs::metadata(&source).expect("metadata").uid();
        {
            let staged = stage_verified_package(&source, &digest, uid, &root).expect("stage");
            assert_eq!(
                fs::read(staged.path()).expect("staged bytes"),
                b"verified package bytes"
            );
        }

        let link = root.join("link.pkg.tar.zst");
        symlink(&source, &link).expect("link");
        assert!(stage_verified_package(&link, &digest, uid, &root).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
