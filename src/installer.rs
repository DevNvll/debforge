use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::InstallOptions;
use crate::digest;
use crate::error::{AppError, Context, Result};
use crate::process;
use crate::workspace::Workspace;

pub const DESKTOP_FILE_ID: &str = "io.github.devnvll.Debforge.desktop";
pub const PRIVILEGED_HELPER: &str = "/usr/lib/debforge/debforge-helper";
const DEB_MIME_TYPES: [&str; 2] = ["application/vnd.debian.binary-package", "application/x-deb"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReceipt {
    pub package_name: String,
    pub package_version: String,
    pub source_path: PathBuf,
    pub source_sha256: String,
    pub converted_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PackageAnnotations {
    source_sha256: Option<String>,
    warnings: Vec<String>,
}

pub fn install(options: &InstallOptions) -> Result<()> {
    let input = fs::canonicalize(&options.input).context(format!(
        "Cannot resolve Debian package {}",
        options.input.display()
    ))?;
    if !input.is_file() {
        return Err(AppError::new(format!(
            "The selected Debian package is not a regular file: {}",
            input.display()
        )));
    }

    verify_helper()?;
    process::require_tools(&["pacman", "pkexec", "bsdtar", "sha256sum"])?;
    let source_sha256 = digest::sha256_file(&input)?;
    let workspace = Workspace::create(false)?;
    convert_for_install(&input, workspace.output_dir())?;
    let package = find_converted_package(workspace.output_dir())?;
    let converted_sha256 = digest::sha256_file(&package)?;

    let (package_name, package_version) = query_package_identity(&package)?;
    let annotations = read_annotations(&package)?;
    if annotations.source_sha256.as_deref() != Some(source_sha256.as_str()) {
        return Err(AppError::new(
            "The converted package does not contain the expected Debian source digest.",
        ));
    }

    show_review(
        &input,
        &package,
        &package_name,
        &package_version,
        &source_sha256,
        &annotations,
    )?;
    if !options.assume_yes && !confirm_installation()? {
        println!("Installation canceled. No system files were changed.");
        return Ok(());
    }

    run_privileged_install(&package, &converted_sha256)?;
    verify_installed(&package_name, &package_version)?;
    let receipt = InstallReceipt {
        package_name,
        package_version,
        source_path: input,
        source_sha256,
        converted_sha256,
    };
    match write_receipt(&receipt) {
        Ok(receipt_path) => println!("Installation receipt: {}", receipt_path.display()),
        Err(error) => {
            eprintln!("Warning: The package was installed, but its receipt failed: {error}")
        }
    }
    notify_success(&receipt);
    Ok(())
}

pub fn register_handler() -> Result<()> {
    let xdg_mime = process::require_tool("xdg-mime")?;
    for mime_type in DEB_MIME_TYPES {
        process::run_checked(
            Command::new(&xdg_mime)
                .args(["default", DESKTOP_FILE_ID, mime_type])
                .stdin(Stdio::null()),
        )?;
        let selected = process::capture_text(
            Command::new(&xdg_mime)
                .args(["query", "default", mime_type])
                .stdin(Stdio::null()),
        )?;
        if selected.trim() != DESKTOP_FILE_ID {
            return Err(AppError::new(format!(
                "The desktop did not select Debforge for MIME type {mime_type}."
            )));
        }
    }
    println!("Debforge is now the default application for .deb files.");
    Ok(())
}

fn verify_helper() -> Result<()> {
    let path = Path::new(PRIVILEGED_HELPER);
    let metadata = fs::symlink_metadata(path).context(format!(
        "The secure installation helper is not installed at {PRIVILEGED_HELPER}. Install the Debforge system package first."
    ))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::new(format!(
            "The secure installation helper is not a regular file: {PRIVILEGED_HELPER}"
        )));
    }
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != 0 || metadata.gid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(AppError::new(
            "The secure installation helper must have root ownership and must not be writable by other users.",
        ));
    }
    Ok(())
}

fn convert_for_install(input: &Path, output: &Path) -> Result<()> {
    let converter = env::current_exe().context("Cannot find the running Debforge executable")?;
    println!("Converting {}...", input.display());
    let status = Command::new(converter)
        .args(["--Quiet", "--scripts", "safe", "--output"])
        .arg(output)
        .arg(input)
        .stdin(Stdio::null())
        .status()
        .context("Cannot start the Debforge converter")?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "Debforge could not convert the selected package (status {status})."
        )))
    }
}

fn find_converted_package(output: &Path) -> Result<PathBuf> {
    let mut packages = fs::read_dir(output)
        .context(format!(
            "Cannot read conversion output {}",
            output.display()
        ))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.contains(".pkg.tar.") && !name.contains(".partial."))
                && path.is_file()
        })
        .collect::<Vec<_>>();
    packages.sort();
    match packages.as_slice() {
        [package] => Ok(package.clone()),
        [] => Err(AppError::new(
            "The conversion did not create an installable Arch package.",
        )),
        _ => Err(AppError::new(
            "The conversion created more than one Arch package. Installation stopped.",
        )),
    }
}

fn query_package_identity(package: &Path) -> Result<(String, String)> {
    let pacman = process::require_tool("pacman")?;
    let identity = process::capture_text(
        Command::new(pacman)
            .args(["-Qp", "--"])
            .arg(package)
            .stdin(Stdio::null()),
    )?;
    let mut fields = identity.split_whitespace();
    let name = fields
        .next()
        .filter(|value| is_safe_package_component(value))
        .ok_or_else(|| AppError::new("Pacman returned an invalid package name."))?;
    let version = fields
        .next()
        .filter(|value| !value.is_empty() && !value.contains(['\n', '\r']))
        .ok_or_else(|| AppError::new("Pacman returned an invalid package version."))?;
    if fields.next().is_some() {
        return Err(AppError::new(
            "Pacman returned an invalid package identity.",
        ));
    }
    Ok((name.to_string(), version.to_string()))
}

fn read_annotations(package: &Path) -> Result<PackageAnnotations> {
    let bsdtar = process::require_tool("bsdtar")?;
    let pkginfo = process::capture_text(
        Command::new(bsdtar)
            .args(["-xOf"])
            .arg(package)
            .arg(".PKGINFO")
            .stdin(Stdio::null()),
    )?;
    parse_annotations(&pkginfo)
}

fn parse_annotations(pkginfo: &str) -> Result<PackageAnnotations> {
    let mut annotations = PackageAnnotations::default();
    for line in pkginfo.lines() {
        let Some(value) = line.strip_prefix("xdata = ") else {
            continue;
        };
        if let Some(digest) = value.strip_prefix("debian-sha256=") {
            annotations.source_sha256 = Some(digest::normalize_sha256(digest)?);
        } else if let Some(warning) = value.strip_prefix("debforge-warning=") {
            if !warning.trim().is_empty()
                && !annotations.warnings.iter().any(|item| item == warning)
            {
                annotations.warnings.push(warning.to_string());
            }
        }
    }
    Ok(annotations)
}

fn show_review(
    input: &Path,
    package: &Path,
    package_name: &str,
    package_version: &str,
    source_sha256: &str,
    annotations: &PackageAnnotations,
) -> Result<()> {
    let pacman = process::require_tool("pacman")?;
    let details = process::capture_text(
        Command::new(&pacman)
            .args(["-Qip", "--"])
            .arg(package)
            .stdin(Stdio::null()),
    )?;
    let files = process::capture_text(
        Command::new(&pacman)
            .args(["-Qlp", "--"])
            .arg(package)
            .stdin(Stdio::null()),
    )?;
    let transaction = process::capture_text(
        Command::new(&pacman)
            .args([
                "-U",
                "--print",
                "--print-format",
                "%n %v [%a] %s bytes",
                "--",
            ])
            .arg(package)
            .stdin(Stdio::null()),
    )?;

    println!("\nDebforge installation review");
    println!("Source       : {}", input.display());
    println!("Source SHA-256: {source_sha256}");
    println!("Package      : {package_name} {package_version}");
    println!("Package files: {}", files.lines().count());
    println!("\nPacman package information:\n{}", details.trim());
    println!("\nPlanned transaction:\n{}", transaction.trim());

    if command_succeeds(Command::new(&pacman).args(["-Si", "--", package_name])) {
        println!(
            "\nWarning: The configured Arch repositories contain a native package named '{package_name}'."
        );
    }
    if let Ok(installed) = process::capture_text(
        Command::new(&pacman)
            .args(["-Q", "--", package_name])
            .stdin(Stdio::null()),
    ) {
        println!("Installed now: {}", installed.trim());
    }
    if !annotations.warnings.is_empty() {
        println!("\nConversion warnings:");
        for warning in &annotations.warnings {
            println!("  - {warning}");
        }
    }
    Ok(())
}

fn command_succeeds(command: &mut Command) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn confirm_installation() -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(AppError::new(
            "Installation needs a terminal confirmation. Use --yes only after you review the package separately.",
        ));
    }
    print!("\nInstall this package? [y/N] ");
    io::stdout()
        .flush()
        .context("Cannot display the confirmation")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("Cannot read the installation confirmation")?;
    Ok(is_affirmative(&answer))
}

fn is_affirmative(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn run_privileged_install(package: &Path, expected_sha256: &str) -> Result<()> {
    let pkexec = process::require_tool("pkexec")?;
    println!("\nRequesting permission for the Pacman transaction...");
    let status = Command::new(pkexec)
        .arg(PRIVILEGED_HELPER)
        .args(["--sha256", expected_sha256, "--"])
        .arg(package)
        .status()
        .context("Cannot start the secure installation helper")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(126) => Err(AppError::new("Authorization was canceled.")),
        Some(127) => Err(AppError::new("Authorization was not available.")),
        _ => Err(AppError::new(format!(
            "The Pacman transaction failed with status {status}."
        ))),
    }
}

fn verify_installed(package_name: &str, expected_version: &str) -> Result<()> {
    let pacman = process::require_tool("pacman")?;
    let installed = process::capture_text(
        Command::new(pacman)
            .args(["-Q", "--", package_name])
            .stdin(Stdio::null()),
    )?;
    let actual_version = installed.split_whitespace().nth(1).unwrap_or_default();
    if actual_version != expected_version {
        return Err(AppError::new(format!(
            "Pacman installed version '{actual_version}', but Debforge expected '{expected_version}'."
        )));
    }
    println!("Installed: {package_name} {expected_version}");
    Ok(())
}

fn write_receipt(receipt: &InstallReceipt) -> Result<PathBuf> {
    let source_path = receipt.source_path.to_string_lossy();
    if source_path.contains(['\n', '\r']) {
        return Err(AppError::new("The Debian source path contains a new line."));
    }
    let state_root = state_home()?.join("debforge").join("receipts");
    fs::create_dir_all(&state_root).context(format!(
        "Cannot create receipt directory {}",
        state_root.display()
    ))?;
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).context(format!(
        "Cannot protect receipt directory {}",
        state_root.display()
    ))?;

    let final_path = state_root.join(format!("{}.receipt", receipt.package_name));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::new(format!("The system clock is before 1970: {error}")))?
        .as_nanos();
    let temporary = state_root.join(format!(
        ".{}.receipt.{}.{nonce}.partial",
        receipt.package_name,
        std::process::id(),
    ));
    let mut temporary_guard = ReceiptGuard::new(temporary.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context(format!("Cannot create receipt {}", temporary.display()))?;
    let installed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::new(format!("The system clock is before 1970: {error}")))?
        .as_secs();
    writeln!(file, "format=1").context("Cannot write receipt")?;
    writeln!(file, "package={}", receipt.package_name).context("Cannot write receipt")?;
    writeln!(file, "version={}", receipt.package_version).context("Cannot write receipt")?;
    writeln!(file, "source={source_path}").context("Cannot write receipt")?;
    writeln!(file, "source_sha256={}", receipt.source_sha256).context("Cannot write receipt")?;
    writeln!(file, "converted_sha256={}", receipt.converted_sha256)
        .context("Cannot write receipt")?;
    writeln!(file, "debforge_version={}", env!("CARGO_PKG_VERSION"))
        .context("Cannot write receipt")?;
    writeln!(file, "installed_at={installed_at}").context("Cannot write receipt")?;
    file.sync_all().context("Cannot synchronize receipt")?;
    fs::rename(&temporary, &final_path).context(format!(
        "Cannot publish installation receipt {}",
        final_path.display()
    ))?;
    temporary_guard.disarm();
    Ok(final_path)
}

struct ReceiptGuard {
    path: PathBuf,
    armed: bool,
}

impl ReceiptGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReceiptGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn state_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        if path.is_absolute() {
            return Ok(path);
        }
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(".local/state"))
        .ok_or_else(|| AppError::new("Cannot find an absolute XDG_STATE_HOME or HOME directory."))
}

fn is_safe_package_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_'))
}

fn notify_success(receipt: &InstallReceipt) {
    let Some(notify_send) = process::find_tool("notify-send") else {
        return;
    };
    let _ = Command::new(notify_send)
        .args([
            "--app-name=Debforge",
            "--icon=io.github.devnvll.Debforge",
            "Package installed",
            &format!("{} {}", receipt.package_name, receipt.package_version),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::{PackageAnnotations, is_affirmative, parse_annotations};

    #[test]
    fn reads_debforge_annotations() {
        let digest = "a".repeat(64);
        let source = format!(
            "pkgname = demo\nxdata = debian-sha256={digest}\nxdata = debforge-warning=Review this.\nxdata = debforge-warning=Review this.\n"
        );
        assert_eq!(
            parse_annotations(&source).expect("annotations"),
            PackageAnnotations {
                source_sha256: Some(digest),
                warnings: vec!["Review this.".to_string()],
            }
        );
    }

    #[test]
    fn confirmation_is_explicit() {
        assert!(is_affirmative("y\n"));
        assert!(is_affirmative("YES"));
        assert!(!is_affirmative(""));
        assert!(!is_affirmative("sure"));
    }
}
