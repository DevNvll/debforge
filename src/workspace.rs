use crate::error::{AppError, Context, Result};
use std::env;
use std::fs::{self, DirBuilder};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

#[cfg(all(unix, not(target_os = "linux")))]
use std::fs::OpenOptions;
#[cfg(all(unix, not(target_os = "linux")))]
use std::os::unix::fs::OpenOptionsExt;

pub const CACHE_DIRECTORY_NAME: &str = "debtap-rs";
pub const DEFAULT_STALE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A private conversion workspace.
///
/// The directory is removed on drop unless it was created with `keep` or was
/// preserved later with [`Workspace::preserve`].
#[derive(Debug)]
pub struct Workspace {
    path: PathBuf,
    members: PathBuf,
    control: PathBuf,
    payload: PathBuf,
    output: PathBuf,
    keep: bool,
}

impl Workspace {
    /// Create a workspace in the user's normal cache directory.
    pub fn create(keep: bool) -> Result<Self> {
        let cache_home = cache_home()?;
        Self::create_in(&cache_home, keep)
    }

    /// An alias for [`Workspace::create`].
    pub fn new(keep: bool) -> Result<Self> {
        Self::create(keep)
    }

    /// Create a workspace below a specified cache root.
    ///
    /// This function is public to support controlled build environments and
    /// tests. It creates `<cache_root>/debtap-rs/work-*`.
    pub fn create_in(cache_root: &Path, keep: bool) -> Result<Self> {
        let application_root = prepare_application_root(cache_root)?;
        // Stale cleanup is useful, but it must not prevent a new conversion.
        // The public cleanup function reports cleanup errors to callers that
        // need them.
        let _ = cleanup_in_application_root(&application_root, DEFAULT_STALE_AGE);

        let path = create_unique_workspace(&application_root)?;
        let mut guard = DirectoryGuard::new(path.clone());

        let members = path.join("members");
        let control = path.join("control");
        let payload = path.join("root");
        let output = path.join("output");
        for directory in [&members, &control, &payload, &output] {
            create_private_directory(directory).context(format!(
                "could not create workspace directory {}",
                directory.display()
            ))?;
        }

        guard.disarm();
        Ok(Self {
            path,
            members,
            control,
            payload,
            output,
            keep,
        })
    }

    /// Return the workspace base directory.
    pub fn root(&self) -> &Path {
        &self.path
    }

    /// Return the workspace base directory.
    pub fn path(&self) -> &Path {
        self.root()
    }

    /// Return the directory for members extracted from the outer `ar` file.
    pub fn members_dir(&self) -> &Path {
        &self.members
    }

    /// Return the extracted Debian control directory.
    pub fn control_dir(&self) -> &Path {
        &self.control
    }

    /// Return the extracted package filesystem root.
    pub fn payload_dir(&self) -> &Path {
        &self.payload
    }

    /// An alias for [`Workspace::payload_dir`].
    pub fn root_dir(&self) -> &Path {
        self.payload_dir()
    }

    /// Return the directory for temporary and final output files.
    pub fn output_dir(&self) -> &Path {
        &self.output
    }

    /// Return true when the workspace will remain after drop.
    pub fn is_kept(&self) -> bool {
        self.keep
    }

    /// Change whether the workspace will remain after drop.
    pub fn set_keep(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// Keep the workspace and return its path.
    pub fn preserve(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Resolve the cache root from `XDG_CACHE_HOME`, or from `HOME/.cache`.
pub fn cache_home() -> Result<PathBuf> {
    if let Some(value) = env::var_os("XDG_CACHE_HOME") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path);
        }
    }

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            AppError::new("could not find an absolute XDG_CACHE_HOME or HOME directory")
        })?;
    Ok(home.join(".cache"))
}

/// Remove old workspaces below a specified cache root.
///
/// Only immediate child directories with the exact `debtap-rs` workspace name
/// pattern and the current user ID are eligible. Symbolic links are ignored.
pub fn cleanup_stale_workspaces(cache_root: &Path, older_than: Duration) -> Result<usize> {
    let application_root = cache_root.join(CACHE_DIRECTORY_NAME);
    let metadata = match fs::symlink_metadata(&application_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(AppError::new(format!(
                "could not inspect cache directory {}: {error}",
                application_root.display()
            )));
        }
    };
    validate_application_root(&application_root, &metadata)?;
    cleanup_in_application_root(&application_root, older_than)
}

/// Remove old workspaces from the user's normal cache directory.
pub fn cleanup_default_stale_workspaces(older_than: Duration) -> Result<usize> {
    cleanup_stale_workspaces(&cache_home()?, older_than)
}

/// Compatibility alias for [`cleanup_stale_workspaces`].
pub fn cleanup_stale_owned_work(cache_root: &Path, older_than: Duration) -> Result<usize> {
    cleanup_stale_workspaces(cache_root, older_than)
}

fn prepare_application_root(cache_root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(cache_root).context(format!(
        "could not create cache root {}",
        cache_root.display()
    ))?;

    let application_root = cache_root.join(CACHE_DIRECTORY_NAME);
    match create_private_directory(&application_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(AppError::new(format!(
                "could not create private cache directory {}: {error}",
                application_root.display()
            )));
        }
    }

    let metadata = fs::symlink_metadata(&application_root).context(format!(
        "could not inspect cache directory {}",
        application_root.display()
    ))?;
    validate_application_root(&application_root, &metadata)?;

    #[cfg(unix)]
    fs::set_permissions(&application_root, fs::Permissions::from_mode(0o700)).context(format!(
        "could not make cache directory private: {}",
        application_root.display()
    ))?;

    Ok(application_root)
}

fn validate_application_root(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::new(format!(
            "cache path is not a real directory: {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    {
        let current_uid = current_user_id()?;
        if metadata.uid() != current_uid {
            return Err(AppError::new(format!(
                "cache directory {} is not owned by the current user",
                path.display()
            )));
        }
    }

    Ok(())
}

fn create_unique_workspace(application_root: &Path) -> Result<PathBuf> {
    for _ in 0..128 {
        let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let name = format!("work-{}-{nanos:032x}-{counter:016x}", std::process::id());
        let candidate = application_root.join(name);
        match create_private_directory(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::new(format!(
                    "could not create workspace {}: {error}",
                    candidate.display()
                )));
            }
        }
    }

    Err(AppError::new(
        "could not allocate a unique debtap-rs workspace after 128 attempts",
    ))
}

fn cleanup_in_application_root(application_root: &Path, older_than: Duration) -> Result<usize> {
    #[cfg(unix)]
    let current_uid = current_user_id()?;
    let now = SystemTime::now();
    let mut removed = 0_usize;
    let mut first_error = None;

    let entries = fs::read_dir(application_root).context(format!(
        "could not read cache directory {}",
        application_root.display()
    ))?;
    for entry_result in entries {
        let entry = match entry_result {
            Ok(entry) => entry,
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    AppError::new(format!("could not read a cache entry: {error}"))
                });
                continue;
            }
        };

        let name = entry.file_name();
        let Some(pid) = workspace_pid(&name) else {
            continue;
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    AppError::new(format!("could not inspect {}: {error}", path.display()))
                });
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        #[cfg(unix)]
        if metadata.uid() != current_uid {
            continue;
        }

        let age = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or(Duration::ZERO);
        if age < older_than || process_is_running(pid) {
            continue;
        }

        match fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    AppError::new(format!(
                        "could not remove stale workspace {}: {error}",
                        path.display()
                    ))
                });
            }
        }
    }

    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(removed)
    }
}

fn workspace_pid(name: &std::ffi::OsStr) -> Option<u32> {
    let name = name.to_str()?;
    let mut parts = name.split('-');
    if parts.next()? != "work" {
        return None;
    }
    let pid_text = parts.next()?;
    let time_text = parts.next()?;
    let counter_text = parts.next()?;
    if parts.next().is_some()
        || pid_text.is_empty()
        || time_text.len() != 32
        || counter_text.len() != 16
        || !time_text.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !counter_text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    pid_text.parse().ok()
}

fn process_is_running(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(unix)]
fn current_user_id() -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        fs::metadata("/proc/self")
            .map(|metadata| metadata.uid())
            .context("could not determine the current user ID from /proc/self")
    }

    #[cfg(not(target_os = "linux"))]
    {
        current_user_id_from_probe()
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn current_user_id_from_probe() -> Result<u32> {
    for _ in 0..128 {
        let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            ".debtap-rs-owner-{}-{counter:016x}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&path) {
            Ok(file) => {
                let result = file
                    .metadata()
                    .map(|metadata| metadata.uid())
                    .context("could not read the owner of a temporary ownership probe");
                drop(file);
                let _ = fs::remove_file(path);
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::new(format!(
                    "could not create a temporary ownership probe: {error}"
                )));
            }
        }
    }
    Err(AppError::new(
        "could not create a unique temporary ownership probe",
    ))
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

struct DirectoryGuard {
    path: PathBuf,
    armed: bool,
}

impl DirectoryGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Self {
            for _ in 0..128 {
                let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = env::temp_dir().join(format!(
                    "debtap-rs-workspace-test-{}-{counter:016x}",
                    std::process::id()
                ));
                if create_private_directory(&path).is_ok() {
                    return Self(path);
                }
            }
            panic!("could not create a test directory");
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn workspace_has_all_private_directories_and_cleans_up() {
        let test_root = TestDirectory::create();
        let workspace = Workspace::create_in(&test_root.0, false).expect("create workspace");
        let workspace_path = workspace.root().to_path_buf();

        for path in [
            workspace.root(),
            workspace.members_dir(),
            workspace.control_dir(),
            workspace.payload_dir(),
            workspace.output_dir(),
        ] {
            assert!(path.is_dir());
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(path)
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        drop(workspace);
        assert!(!workspace_path.exists());
    }

    #[test]
    fn kept_workspace_remains() {
        let test_root = TestDirectory::create();
        let workspace = Workspace::create_in(&test_root.0, true).expect("create workspace");
        let path = workspace.root().to_path_buf();
        drop(workspace);
        assert!(path.is_dir());
    }

    #[test]
    fn cleanup_removes_only_exact_stale_workspace_names() {
        let test_root = TestDirectory::create();
        let application_root = prepare_application_root(&test_root.0).expect("application root");
        let stale = application_root
            .join("work-4294967295-00000000000000000000000000000000-0000000000000000");
        create_private_directory(&stale).expect("stale directory");
        let unrelated = application_root.join("work-not-a-debtap-workspace");
        create_private_directory(&unrelated).expect("unrelated directory");

        let removed =
            cleanup_stale_workspaces(&test_root.0, Duration::ZERO).expect("clean stale workspaces");
        assert_eq!(removed, 1);
        assert!(!stale.exists());
        assert!(unrelated.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_does_not_follow_a_workspace_shaped_symlink() {
        use std::os::unix::fs::symlink;

        let test_root = TestDirectory::create();
        let application_root = prepare_application_root(&test_root.0).expect("application root");
        let target = test_root.0.join("target");
        create_private_directory(&target).expect("target directory");
        let link = application_root
            .join("work-4294967295-00000000000000000000000000000000-0000000000000000");
        symlink(&target, &link).expect("workspace-shaped symlink");

        let removed =
            cleanup_stale_workspaces(&test_root.0, Duration::ZERO).expect("clean stale workspaces");
        assert_eq!(removed, 0);
        assert!(target.is_dir());
        assert!(link.symlink_metadata().is_ok());
    }
}
