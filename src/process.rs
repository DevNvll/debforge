use crate::error::{AppError, Context, Result};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, ExitStatus, Output, Stdio};
use std::thread::{self, JoinHandle};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const MAX_PIPELINE_DIAGNOSTIC_BYTES: usize = 1024 * 1024;

/// Find an executable in `PATH`.
///
/// A name that contains a path separator is checked as a path and is not
/// searched for in `PATH`.
pub fn find_tool(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
    if name.is_empty() {
        return None;
    }

    let path = Path::new(name);
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable_file(path).then(|| path.to_path_buf());
    }

    let search_path = env::var_os("PATH")?;
    env::split_paths(&search_path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Return true when an executable can be found in `PATH`.
pub fn command_exists(name: impl AsRef<OsStr>) -> bool {
    find_tool(name).is_some()
}

/// An alias for [`command_exists`] that reads well at call sites.
pub fn tool_exists(name: impl AsRef<OsStr>) -> bool {
    command_exists(name)
}

/// Find a required system tool or return an actionable error.
pub fn require_tool(name: impl AsRef<OsStr>) -> Result<PathBuf> {
    let name = name.as_ref();
    find_tool(name).ok_or_else(|| {
        AppError::new(format!(
            "required command '{}' was not found in PATH",
            name.to_string_lossy()
        ))
    })
}

/// Find all required system tools.
pub fn require_tools(names: &[&str]) -> Result<Vec<PathBuf>> {
    names.iter().map(require_tool).collect()
}

/// Run a command, capture its output, and require a successful exit status.
pub fn run_checked_output(command: &mut Command) -> Result<Output> {
    let description = describe_command(command);
    let output = command
        .output()
        .context(format!("failed to start {description}"))?;

    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failure(&description, &output))
    }
}

/// Run a command and require a successful exit status.
pub fn run_checked(command: &mut Command) -> Result<()> {
    run_checked_output(command).map(|_| ())
}

/// Run a command and return its captured standard output.
pub fn capture_stdout(command: &mut Command) -> Result<Vec<u8>> {
    run_checked_output(command).map(|output| output.stdout)
}

/// Run a command and return UTF-8 standard output.
pub fn capture_text(command: &mut Command) -> Result<String> {
    let description = describe_command(command);
    let bytes = capture_stdout(command)?;
    String::from_utf8(bytes).context(format!(
        "{description} wrote non-UTF-8 data to standard output"
    ))
}

/// Run `bsdtar` and stream its output through parallel `zstd` compression.
///
/// The output path is created with `create_new`. Thus, this function never
/// replaces an existing file. The partial file is removed if either child
/// process fails.
pub fn pipe_bsdtar_to_zstd(
    bsdtar_arguments: &[OsString],
    output_path: &Path,
    compression_level: i32,
) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let output = options.open(output_path).context(format!(
        "could not create package output {}",
        output_path.display()
    ))?;

    if let Err(error) = pipe_bsdtar_to_zstd_file(bsdtar_arguments, output, compression_level) {
        let _ = fs::remove_file(output_path);
        return Err(error);
    }

    Ok(())
}

/// The file-based form of [`pipe_bsdtar_to_zstd`].
///
/// The caller owns output-path cleanup. The file must be new or truncated and
/// positioned at byte zero.
pub fn pipe_bsdtar_to_zstd_file(
    bsdtar_arguments: &[OsString],
    output: File,
    compression_level: i32,
) -> Result<()> {
    validate_zstd_level(compression_level)?;

    let bsdtar_path = require_tool("bsdtar")?;
    let zstd_path = require_tool("zstd")?;

    let mut bsdtar_command = Command::new(&bsdtar_path);
    bsdtar_command
        .args(bsdtar_arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C");

    let mut bsdtar = bsdtar_command
        .spawn()
        .context("failed to start bsdtar for package creation")?;
    let bsdtar_stdout = bsdtar
        .stdout
        .take()
        .ok_or_else(|| AppError::new("could not connect bsdtar standard output to zstd"))?;
    let bsdtar_stderr = bsdtar
        .stderr
        .take()
        .ok_or_else(|| AppError::new("could not capture bsdtar standard error"))?;
    let bsdtar_diagnostics = drain_stderr(bsdtar_stderr);

    let zstd_output = output
        .try_clone()
        .context("could not duplicate the package output file handle")?;
    let mut zstd_command = Command::new(&zstd_path);
    if compression_level > 19 {
        zstd_command.arg("--ultra");
    }
    zstd_command
        .arg(format!("-{compression_level}"))
        .args(["--threads=0", "--quiet", "--stdout", "--"])
        .stdin(Stdio::from(bsdtar_stdout))
        .stdout(Stdio::from(zstd_output))
        .stderr(Stdio::piped())
        .env("LC_ALL", "C");

    let mut zstd = match zstd_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = bsdtar.kill();
            let _ = bsdtar.wait();
            let diagnostics = join_diagnostics(bsdtar_diagnostics);
            let detail = diagnostic_text(&diagnostics);
            return Err(AppError::new(format!(
                "failed to start zstd: {error}{}",
                suffix_detail(&detail)
            )));
        }
    };

    let zstd_stderr = zstd
        .stderr
        .take()
        .ok_or_else(|| AppError::new("could not capture zstd standard error"))?;
    let zstd_diagnostics = drain_stderr(zstd_stderr);

    // Both children can make progress while these waits run. Their error
    // streams are drained in separate threads, which prevents a full error
    // pipe from blocking the archive pipeline.
    let bsdtar_status = bsdtar.wait();
    let zstd_status = zstd.wait();
    let bsdtar_error = join_diagnostics(bsdtar_diagnostics);
    let zstd_error = join_diagnostics(zstd_diagnostics);

    let bsdtar_status = bsdtar_status.context("could not wait for bsdtar")?;
    let zstd_status = zstd_status.context("could not wait for zstd")?;

    if !bsdtar_status.success() || !zstd_status.success() {
        return Err(pipeline_failure(
            bsdtar_status,
            zstd_status,
            &bsdtar_error,
            &zstd_error,
        ));
    }

    output
        .sync_all()
        .context("could not synchronize the compressed package output")?;
    Ok(())
}

fn validate_zstd_level(level: i32) -> Result<()> {
    if (1..=22).contains(&level) {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "invalid zstd compression level {level}; expected a value from 1 through 22"
        )))
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn describe_command(command: &Command) -> String {
    let mut description = quote_argument(command.get_program());
    for argument in command.get_args() {
        description.push(' ');
        description.push_str(&quote_argument(argument));
    }
    description
}

fn quote_argument(argument: &OsStr) -> String {
    let text = argument.to_string_lossy();
    if text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._/+,:=@%-".contains(&byte))
    {
        text.into_owned()
    } else {
        format!("{text:?}")
    }
}

fn command_failure(description: &str, output: &Output) -> AppError {
    let stderr = diagnostic_text(&output.stderr);
    let stdout = diagnostic_text(&output.stdout);
    let detail = if stderr.is_empty() { stdout } else { stderr };
    AppError::new(format!(
        "{description} failed with {}{}",
        status_description(output.status),
        suffix_detail(&detail)
    ))
}

fn pipeline_failure(
    bsdtar_status: ExitStatus,
    zstd_status: ExitStatus,
    bsdtar_error: &[u8],
    zstd_error: &[u8],
) -> AppError {
    let mut message = format!(
        "archive pipeline failed (bsdtar: {}; zstd: {})",
        status_description(bsdtar_status),
        status_description(zstd_status)
    );
    let bsdtar_detail = diagnostic_text(bsdtar_error);
    let zstd_detail = diagnostic_text(zstd_error);
    if !bsdtar_detail.is_empty() {
        message.push_str("; bsdtar: ");
        message.push_str(&bsdtar_detail);
    }
    if !zstd_detail.is_empty() {
        message.push_str("; zstd: ");
        message.push_str(&zstd_detail);
    }
    AppError::new(message)
}

fn status_description(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "termination by signal".to_string(),
        |code| format!("exit status {code}"),
    )
}

fn diagnostic_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().replace('\n', "; ")
}

fn suffix_detail(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

fn drain_stderr(stderr: ChildStderr) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || read_limited(stderr, MAX_PIPELINE_DIAGNOSTIC_BYTES))
}

fn read_limited(mut input: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;

    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        if remaining > 0 {
            captured.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }

    if truncated {
        captured.extend_from_slice(b"\n[diagnostic output was truncated]");
    }
    Ok(captured)
}

fn join_diagnostics(handle: JoinHandle<io::Result<Vec<u8>>>) -> Vec<u8> {
    match handle.join() {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => format!("could not read diagnostic output: {error}").into_bytes(),
        Err(_) => b"diagnostic reader thread stopped unexpectedly".to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_absolute_executable_is_found() {
        assert!(find_tool("/bin/sh").is_some());
    }

    #[test]
    fn an_empty_tool_name_is_not_found() {
        assert!(find_tool("").is_none());
    }

    #[test]
    fn checked_output_reports_a_nonzero_exit() {
        let error =
            run_checked_output(Command::new("/bin/sh").args(["-c", "printf problem >&2; exit 7"]))
                .expect_err("the command must fail");
        let message = error.to_string();
        assert!(message.contains("exit status 7"));
        assert!(message.contains("problem"));
    }

    #[test]
    fn zstd_level_validation_has_clear_bounds() {
        assert!(validate_zstd_level(1).is_ok());
        assert!(validate_zstd_level(22).is_ok());
        assert!(validate_zstd_level(0).is_err());
        assert!(validate_zstd_level(23).is_err());
    }
}
