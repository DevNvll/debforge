use crate::error::{AppError, Context, Result};
use crate::process;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

pub const DEBIAN_AR_MAGIC: &[u8; 8] = b"!<arch>\n";
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The three required members extracted from a Debian binary package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebArchive {
    pub debian_binary: PathBuf,
    pub control_member: PathBuf,
    pub data_member: PathBuf,
    pub control_member_name: OsString,
    pub data_member_name: OsString,
}

/// Options for Arch package creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackageOptions {
    pub zstd_level: i32,
    pub source_date_epoch: Option<i64>,
    pub overwrite: bool,
    pub validate_with_pacman: bool,
}

impl Default for PackageOptions {
    fn default() -> Self {
        Self {
            zstd_level: DEFAULT_ZSTD_LEVEL,
            source_date_epoch: None,
            overwrite: false,
            validate_with_pacman: false,
        }
    }
}

/// State returned by optional `pacman -Qp` validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationStatus {
    Validated,
    PacmanUnavailable,
}

/// Validate the outer Debian `ar` signature.
pub fn validate_debian_magic(input: &Path) -> Result<()> {
    let mut file =
        File::open(input).context(format!("could not open Debian package {}", input.display()))?;
    let mut magic = [0_u8; DEBIAN_AR_MAGIC.len()];
    file.read_exact(&mut magic).context(format!(
        "could not read the Debian archive signature from {}",
        input.display()
    ))?;
    if &magic != DEBIAN_AR_MAGIC {
        return Err(AppError::new(format!(
            "{} is not a Debian ar archive: invalid !<arch> signature",
            input.display()
        )));
    }
    Ok(())
}

/// Extract and validate the required members from a Debian package.
///
/// Extra outer members, such as `_gpgorigin`, are accepted. There must be one
/// `debian-binary`, one `control.tar*`, and one `data.tar*` member.
pub fn extract_deb_members(input: &Path, members_dir: &Path) -> Result<DebArchive> {
    validate_debian_magic(input)?;
    ensure_real_directory(members_dir)?;

    let ar = process::require_tool("ar")?;
    let input = fs::canonicalize(input).context(format!(
        "could not resolve Debian package {}",
        input.display()
    ))?;
    let listing = process::run_checked_output(
        Command::new(&ar)
            .arg("t")
            .arg("--")
            .arg(&input)
            .stdin(Stdio::null()),
    )?;
    let names = parse_ar_member_listing(&listing.stdout)?;

    let debian_binary_name = unique_member(
        &names,
        |name| member_bytes(name) == b"debian-binary",
        "debian-binary",
    )?;
    let control_name = unique_member(
        &names,
        |name| member_bytes(name).starts_with(b"control.tar"),
        "control.tar*",
    )?;
    let data_name = unique_member(
        &names,
        |name| member_bytes(name).starts_with(b"data.tar"),
        "data.tar*",
    )?;

    let debian_binary = members_dir.join("debian-binary");
    let control_member = members_dir.join("control.archive");
    let data_member = members_dir.join("data.archive");
    let mut created = CreatedFiles::default();

    extract_ar_member(&ar, &input, &debian_binary_name, &debian_binary)?;
    created.push(debian_binary.clone());
    extract_ar_member(&ar, &input, &control_name, &control_member)?;
    created.push(control_member.clone());
    extract_ar_member(&ar, &input, &data_name, &data_member)?;
    created.push(data_member.clone());

    validate_debian_binary(&debian_binary)?;
    created.disarm();

    Ok(DebArchive {
        debian_binary,
        control_member,
        data_member,
        control_member_name: control_name,
        data_member_name: data_name,
    })
}

/// Compatibility alias for [`extract_deb_members`].
pub fn extract_members(input: &Path, members_dir: &Path) -> Result<DebArchive> {
    extract_deb_members(input, members_dir)
}

/// Require the exact Debian package format marker `2.0\n`.
pub fn validate_debian_binary(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path).context(format!("could not inspect {}", path.display()))?;
    if metadata.len() > 16 {
        return Err(AppError::new(format!(
            "{} does not contain the Debian package format version 2.0",
            path.display()
        )));
    }
    let contents = fs::read(path).context(format!("could not read {}", path.display()))?;
    if contents != b"2.0\n" {
        return Err(AppError::new(format!(
            "{} does not contain the Debian package format version 2.0",
            path.display()
        )));
    }
    Ok(())
}

/// Extract a Debian control tar member and return its required control file.
pub fn extract_control(member: &Path, control_dir: &Path) -> Result<PathBuf> {
    extract_tar_member(member, control_dir, "control")?;
    let control = control_dir.join("control");
    let metadata = fs::symlink_metadata(&control).context(format!(
        "the Debian control archive has no control file at {}",
        control.display()
    ))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::new(format!(
            "the Debian control entry is not a regular file: {}",
            control.display()
        )));
    }
    Ok(control)
}

/// Extract the Debian payload tar member into a package filesystem root.
pub fn extract_payload(member: &Path, payload_dir: &Path) -> Result<()> {
    extract_tar_member(member, payload_dir, "payload")
}

/// Return a byte-sorted list of all package paths.
///
/// Paths are relative to `root` and have the `./` prefix used by Arch package
/// metadata. Directory symbolic links are included but are never entered.
pub fn collect_sorted_file_list(root: &Path) -> Result<Vec<PathBuf>> {
    validate_real_directory(root, "package root")?;
    let mut paths = Vec::new();
    walk_without_following_links(root, Path::new(""), &mut paths)?;
    sort_paths(&mut paths);
    Ok(paths)
}

/// Return the sorted package file list as NUL-delimited bytes.
pub fn sorted_nul_file_list(root: &Path) -> Result<Vec<u8>> {
    paths_to_nul_bytes(&collect_sorted_file_list(root)?)
}

/// Write a sorted NUL-delimited package file list and return its entry count.
pub fn write_sorted_nul_file_list(root: &Path, output: &Path) -> Result<usize> {
    let paths = collect_sorted_file_list(root)?;
    let bytes = paths_to_nul_bytes(&paths)?;
    let mut file = new_file(output, 0o600)?;
    file.write_all(&bytes)
        .context(format!("could not write file list {}", output.display()))?;
    file.sync_all().context(format!(
        "could not synchronize file list {}",
        output.display()
    ))?;
    Ok(paths.len())
}

/// Generate the standard gzip-compressed `.MTREE` file in a package root.
pub fn generate_mtree(root: &Path) -> Result<PathBuf> {
    generate_mtree_with_epoch(root, None)
}

/// Generate `.MTREE`, with an optional fixed timestamp for its entries.
pub fn generate_mtree_with_epoch(root: &Path, source_date_epoch: Option<i64>) -> Result<PathBuf> {
    validate_epoch(source_date_epoch)?;
    validate_real_directory(root, "package root")?;
    let root = fs::canonicalize(root)
        .context(format!("could not resolve package root {}", root.display()))?;
    let bsdtar = process::require_tool("bsdtar")?;

    let mut paths = collect_sorted_file_list(&root)?;
    paths.retain(|path| path != Path::new("./.MTREE"));

    let temporary_parent = root.parent().unwrap_or(&root);
    let (list_path, mut list_file) =
        create_unique_file(temporary_parent, ".debtap-rs-mtree-list", 0o600)?;
    let mut list_guard = FileGuard::new(list_path.clone());
    list_file
        .write_all(&paths_to_nul_bytes(&paths)?)
        .context("could not write the temporary MTREE file list")?;
    list_file
        .sync_all()
        .context("could not synchronize the temporary MTREE file list")?;
    drop(list_file);

    let (raw_path, raw_file) = create_unique_file(temporary_parent, ".debtap-rs-mtree-raw", 0o600)?;
    let mut raw_guard = FileGuard::new(raw_path.clone());
    let mut command = Command::new(&bsdtar);
    command
        .arg("-cnf")
        .arg("-")
        .arg("--format=mtree")
        .arg("--options=!all,use-set,type,uid,gid,mode,time,size,sha256,link")
        .args([
            "--uid", "0", "--gid", "0", "--uname", "root", "--gname", "root",
        ])
        .arg("--null")
        .arg("--files-from")
        .arg(&list_path)
        .arg("--exclude")
        .arg(".MTREE")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(raw_file))
        .env("LC_ALL", "C");
    if let Some(epoch) = source_date_epoch {
        command.arg("--mtime").arg(format!("@{epoch}"));
    }
    process::run_checked(&mut command)?;

    let mtree = root.join(".MTREE");
    let (compressed_path, mut compressed) =
        create_unique_file(&root, ".MTREE.debtap-rs-part", 0o600)?;
    let mut compressed_guard = FileGuard::new(compressed_path.clone());
    let mut raw = OpenOptions::new()
        .read(true)
        .open(&raw_path)
        .context("could not reopen generated MTREE data")?;
    raw.seek(SeekFrom::Start(0))
        .context("could not seek generated MTREE data")?;
    write_gzip_stored(&mut raw, &mut compressed)?;
    compressed
        .sync_all()
        .context("could not synchronize compressed MTREE data")?;
    drop(compressed);
    set_file_mode(&compressed_path, 0o644)?;
    fs::rename(&compressed_path, &mtree).context(format!(
        "could not install generated MTREE file at {}",
        mtree.display()
    ))?;
    compressed_guard.disarm();

    // These removals are explicit so successful conversions do not wait for
    // scope cleanup. The guards still cover all error paths.
    fs::remove_file(&raw_path).context("could not remove temporary MTREE data")?;
    raw_guard.disarm();
    fs::remove_file(&list_path).context("could not remove temporary MTREE file list")?;
    list_guard.disarm();
    Ok(mtree)
}

/// Create a root-owned restricted-pax `.pkg.tar.zst` without replacement.
pub fn create_package(root: &Path, output: &Path, zstd_level: i32) -> Result<()> {
    create_package_with_options(
        root,
        output,
        PackageOptions {
            zstd_level,
            ..PackageOptions::default()
        },
    )
}

/// Create an Arch package with explicit timestamp and publication options.
pub fn create_package_with_options(
    root: &Path,
    output: &Path,
    options: PackageOptions,
) -> Result<()> {
    validate_epoch(options.source_date_epoch)?;
    validate_package_suffix(output)?;
    validate_real_directory(root, "package root")?;
    let root = fs::canonicalize(root)
        .context(format!("could not resolve package root {}", root.display()))?;

    let output_parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_parent).context(format!(
        "could not create output directory {}",
        output_parent.display()
    ))?;
    let output_parent = fs::canonicalize(output_parent).context(format!(
        "could not resolve output directory {}",
        output_parent.display()
    ))?;
    if output_parent.starts_with(&root) {
        return Err(AppError::new(
            "the package output directory must be outside the package root",
        ));
    }
    let file_name = output.file_name().ok_or_else(|| {
        AppError::new(format!(
            "package output has no file name: {}",
            output.display()
        ))
    })?;
    let final_output = output_parent.join(file_name);
    if !options.overwrite && fs::symlink_metadata(&final_output).is_ok() {
        return Err(AppError::new(format!(
            "package output already exists: {}",
            final_output.display()
        )));
    }

    let paths = collect_sorted_file_list(&root)?;
    if paths.is_empty() {
        return Err(AppError::new("cannot create a package from an empty root"));
    }
    let archive_paths = paths
        .iter()
        .map(|path| {
            path.strip_prefix(Path::new("."))
                .unwrap_or(path)
                .to_path_buf()
        })
        .collect::<Vec<_>>();

    let (list_path, mut list_file) =
        create_unique_file(&output_parent, ".debtap-rs-package-list", 0o600)?;
    let mut list_guard = FileGuard::new(list_path.clone());
    list_file
        .write_all(&paths_to_nul_bytes(&archive_paths)?)
        .context("could not write the package file list")?;
    list_file
        .sync_all()
        .context("could not synchronize the package file list")?;
    drop(list_file);

    let (partial_path, partial_file) =
        create_unique_file(&output_parent, ".debtap-rs-package-part", 0o600)?;
    let mut partial_guard = FileGuard::new(partial_path.clone());

    let mut arguments = vec![
        OsString::from("-cnf"),
        OsString::from("-"),
        OsString::from("--no-fflags"),
        OsString::from("--no-read-sparse"),
        OsString::from("--uid"),
        OsString::from("0"),
        OsString::from("--gid"),
        OsString::from("0"),
        OsString::from("--uname"),
        OsString::from("root"),
        OsString::from("--gname"),
        OsString::from("root"),
        OsString::from("--no-recursion"),
        OsString::from("-C"),
        root.as_os_str().to_owned(),
        OsString::from("--null"),
        OsString::from("--files-from"),
        list_path.as_os_str().to_owned(),
    ];
    if let Some(epoch) = options.source_date_epoch {
        arguments.push(OsString::from("--mtime"));
        arguments.push(OsString::from(format!("@{epoch}")));
    }

    process::pipe_bsdtar_to_zstd_file(&arguments, partial_file, options.zstd_level)?;
    set_file_mode(&partial_path, 0o644)?;

    if options.validate_with_pacman {
        try_validate_package(&partial_path)?;
    }

    publish_file(&partial_path, &final_output, options.overwrite)?;
    partial_guard.disarm();
    synchronize_directory(&output_parent)?;

    fs::remove_file(&list_path).context("could not remove the temporary package file list")?;
    list_guard.disarm();
    Ok(())
}

/// Validate an Arch package with `pacman -Qp` when pacman is installed.
pub fn validate_package(package: &Path) -> Result<()> {
    try_validate_package(package).map(|_| ())
}

/// Validate a package and report whether pacman was available.
pub fn try_validate_package(package: &Path) -> Result<ValidationStatus> {
    let metadata = fs::metadata(package)
        .context(format!("could not inspect package {}", package.display()))?;
    if !metadata.is_file() {
        return Err(AppError::new(format!(
            "package output is not a regular file: {}",
            package.display()
        )));
    }

    let Some(pacman) = process::find_tool("pacman") else {
        return Ok(ValidationStatus::PacmanUnavailable);
    };
    let package = fs::canonicalize(package)
        .context(format!("could not resolve package {}", package.display()))?;
    process::run_checked(
        Command::new(pacman)
            .arg("-Qp")
            .arg("--")
            .arg(package)
            .stdin(Stdio::null()),
    )?;
    Ok(ValidationStatus::Validated)
}

fn extract_ar_member(
    ar: &Path,
    input: &Path,
    member_name: &OsStr,
    destination: &Path,
) -> Result<()> {
    let output = new_file(destination, 0o600)?;
    let result = process::run_checked(
        Command::new(ar)
            .arg("p")
            .arg("--")
            .arg(input)
            .arg(member_name)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().context(format!(
                "could not duplicate output handle {}",
                destination.display()
            ))?)),
    );
    if let Err(error) = result {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    output.sync_all().context(format!(
        "could not synchronize extracted member {}",
        destination.display()
    ))?;
    Ok(())
}

fn extract_tar_member(member: &Path, destination: &Path, description: &str) -> Result<()> {
    ensure_empty_real_directory(destination)?;
    let bsdtar = process::require_tool("bsdtar")?;
    let member = fs::canonicalize(member).context(format!(
        "could not resolve {description} archive {}",
        member.display()
    ))?;
    let destination = fs::canonicalize(destination).context(format!(
        "could not resolve {description} destination {}",
        destination.display()
    ))?;

    process::run_checked(
        Command::new(bsdtar)
            .arg("-xpf")
            .arg(member)
            .arg("--no-same-owner")
            .arg("--no-fflags")
            .arg("-C")
            .arg(destination)
            .stdin(Stdio::null()),
    )
}

fn parse_ar_member_listing(output: &[u8]) -> Result<Vec<OsString>> {
    let mut names = Vec::new();
    for raw_line in output.split(|byte| *byte == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        let mut line = raw_line;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.last() == Some(&b'/') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line.contains(&0) {
            return Err(AppError::new("ar returned an invalid member name"));
        }
        names.push(os_string_from_bytes(line));
    }
    Ok(names)
}

fn unique_member<F>(names: &[OsString], predicate: F, description: &str) -> Result<OsString>
where
    F: Fn(&OsStr) -> bool,
{
    let matches: Vec<&OsString> = names
        .iter()
        .filter(|name| predicate(name.as_os_str()))
        .collect();
    match matches.as_slice() {
        [name] => Ok((*name).clone()),
        [] => Err(AppError::new(format!(
            "Debian archive has no {description} member"
        ))),
        _ => Err(AppError::new(format!(
            "Debian archive has more than one {description} member"
        ))),
    }
}

fn walk_without_following_links(
    root: &Path,
    relative_directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<()> {
    let directory = root.join(relative_directory);
    let entries = fs::read_dir(&directory).context(format!(
        "could not read package directory {}",
        directory.display()
    ))?;
    for entry in entries {
        let entry = entry.context(format!(
            "could not read an entry in package directory {}",
            directory.display()
        ))?;
        let relative = relative_directory.join(entry.file_name());
        output.push(Path::new(".").join(&relative));
        let file_type = entry.file_type().context(format!(
            "could not inspect package path {}",
            entry.path().display()
        ))?;
        if file_type.is_dir() {
            walk_without_following_links(root, &relative, output)?;
        }
    }
    Ok(())
}

fn sort_paths(paths: &mut [PathBuf]) {
    #[cfg(unix)]
    paths.sort_by(|left, right| {
        left.as_os_str()
            .as_bytes()
            .cmp(right.as_os_str().as_bytes())
    });

    #[cfg(not(unix))]
    paths.sort();
}

fn paths_to_nul_bytes(paths: &[PathBuf]) -> Result<Vec<u8>> {
    let total_path_bytes = paths
        .iter()
        .map(|path| os_str_bytes(path.as_os_str()).map_or(0, <[u8]>::len))
        .sum::<usize>();
    let mut output = Vec::with_capacity(total_path_bytes.saturating_add(paths.len()));
    for path in paths {
        let bytes = os_str_bytes(path.as_os_str()).ok_or_else(|| {
            AppError::new(format!(
                "package path is not valid text on this platform: {}",
                path.display()
            ))
        })?;
        if bytes.contains(&0) {
            return Err(AppError::new("a package path contains a NUL byte"));
        }
        output.extend_from_slice(bytes);
        output.push(0);
    }
    Ok(output)
}

fn write_gzip_stored(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    // RFC 1952 header: deflate, no optional fields, timestamp zero, Unix OS.
    output
        .write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3])
        .context("could not write the gzip MTREE header")?;

    let mut crc = 0xffff_ffff_u32;
    let mut input_size = 0_u32;
    let mut buffer = [0_u8; u16::MAX as usize];
    loop {
        let count = input
            .read(&mut buffer)
            .context("could not read generated MTREE data")?;
        if count == 0 {
            break;
        }
        // A zero BFINAL bit and BTYPE=00 form one uncompressed DEFLATE block.
        output
            .write_all(&[0])
            .context("could not write a gzip MTREE block")?;
        let length = u16::try_from(count)
            .map_err(|_| AppError::new("internal MTREE gzip block is too large"))?;
        output
            .write_all(&length.to_le_bytes())
            .context("could not write a gzip MTREE block length")?;
        output
            .write_all(&(!length).to_le_bytes())
            .context("could not write a gzip MTREE block length check")?;
        output
            .write_all(&buffer[..count])
            .context("could not write gzip MTREE data")?;
        crc = crc32_update(crc, &buffer[..count]);
        input_size = input_size.wrapping_add(count as u32);
    }

    // Finish with an empty final uncompressed block.
    output
        .write_all(&[1, 0, 0, 0xff, 0xff])
        .context("could not finish the gzip MTREE stream")?;
    output
        .write_all(&(!crc).to_le_bytes())
        .context("could not write the gzip MTREE checksum")?;
    output
        .write_all(&input_size.to_le_bytes())
        .context("could not write the gzip MTREE size")?;
    Ok(())
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc
}

fn publish_file(partial: &Path, output: &Path, overwrite: bool) -> Result<()> {
    if overwrite {
        fs::rename(partial, output)
            .context(format!("could not publish package at {}", output.display()))
    } else {
        fs::hard_link(partial, output).context(format!(
            "could not publish package without replacing {}",
            output.display()
        ))?;
        fs::remove_file(partial).context(format!(
            "package was published, but its partial link could not be removed: {}",
            partial.display()
        ))
    }
}

fn validate_package_suffix(output: &Path) -> Result<()> {
    let Some(name) = output.file_name() else {
        return Err(AppError::new(format!(
            "package output has no file name: {}",
            output.display()
        )));
    };
    if !member_bytes(name).ends_with(b".pkg.tar.zst") {
        return Err(AppError::new(format!(
            "package output must end in .pkg.tar.zst: {}",
            output.display()
        )));
    }
    Ok(())
}

fn validate_epoch(epoch: Option<i64>) -> Result<()> {
    if epoch.is_some_and(|value| value < 0) {
        Err(AppError::new(
            "SOURCE_DATE_EPOCH cannot be a negative number",
        ))
    } else {
        Ok(())
    }
}

fn ensure_empty_real_directory(path: &Path) -> Result<()> {
    ensure_real_directory(path)?;
    let mut entries =
        fs::read_dir(path).context(format!("could not read directory {}", path.display()))?;
    if entries.next().is_some() {
        return Err(AppError::new(format!(
            "extraction destination is not empty: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder
            .create(path)
            .context(format!("could not create directory {}", path.display()))?;
    }
    validate_real_directory(path, "directory")
}

fn validate_real_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context(format!(
        "could not inspect {description} {}",
        path.display()
    ))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::new(format!(
            "{description} is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn new_file(path: &Path, mode: u32) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(mode);
    #[cfg(not(unix))]
    let _ = mode;
    options
        .open(path)
        .context(format!("could not create file {}", path.display()))
}

fn create_unique_file(directory: &Path, prefix: &str, mode: u32) -> Result<(PathBuf, File)> {
    for _ in 0..128 {
        let counter = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let path = directory.join(format!(
            "{prefix}-{}-{nanos:032x}-{counter:016x}",
            std::process::id()
        ));
        match new_file(&path, mode) {
            Ok(file) => return Ok((path, file)),
            Err(error) if fs::symlink_metadata(&path).is_ok_and(|_| true) => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(AppError::new(format!(
        "could not create a unique temporary file in {}",
        directory.display()
    )))
}

fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .context(format!("could not set file mode on {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn synchronize_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .context(format!(
            "could not synchronize directory {}",
            path.display()
        ))
}

fn member_bytes(value: &OsStr) -> &[u8] {
    #[cfg(unix)]
    {
        value.as_bytes()
    }

    #[cfg(not(unix))]
    {
        value.to_str().unwrap_or("").as_bytes()
    }
}

fn os_str_bytes(value: &OsStr) -> Option<&[u8]> {
    #[cfg(unix)]
    {
        Some(value.as_bytes())
    }

    #[cfg(not(unix))]
    {
        value.to_str().map(str::as_bytes)
    }
}

fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        OsString::from_vec(bytes.to_vec())
    }

    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

#[derive(Default)]
struct CreatedFiles {
    paths: Vec<PathBuf>,
    armed: bool,
}

impl CreatedFiles {
    fn push(&mut self, path: PathBuf) {
        self.armed = true;
        self.paths.push(path);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreatedFiles {
    fn drop(&mut self) {
        if self.armed {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

struct FileGuard {
    path: PathBuf,
    armed: bool,
}

impl FileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
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
                let path = std::env::temp_dir().join(format!(
                    "debtap-rs-archive-test-{}-{counter:016x}",
                    std::process::id()
                ));
                let mut builder = DirBuilder::new();
                #[cfg(unix)]
                builder.mode(0o700);
                if builder.create(&path).is_ok() {
                    return Self(path);
                }
            }
            panic!("could not create an archive test directory");
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn invalid_debian_magic_is_rejected() {
        let test = TestDirectory::create();
        let input = test.0.join("bad.deb");
        fs::write(&input, b"not an ar file").expect("write fixture");
        assert!(validate_debian_magic(&input).is_err());
    }

    #[test]
    fn file_list_is_sorted_and_does_not_enter_symlinks() {
        let test = TestDirectory::create();
        let root = test.0.join("root");
        let outside = test.0.join("outside");
        fs::create_dir(&root).expect("root");
        fs::create_dir(&outside).expect("outside");
        fs::write(outside.join("hidden"), b"hidden").expect("outside file");
        fs::write(root.join("z"), b"z").expect("z file");
        fs::create_dir(root.join("a")).expect("a directory");
        fs::write(root.join("a/file"), b"a").expect("a file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("directory symlink");

        let paths = collect_sorted_file_list(&root).expect("file list");
        let rendered: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(rendered[0], "./a");
        assert_eq!(rendered[1], "./a/file");
        #[cfg(unix)]
        {
            assert!(rendered.contains(&"./link".to_string()));
            assert!(!rendered.iter().any(|path| path.contains("hidden")));
        }
        assert_eq!(rendered.last().map(String::as_str), Some("./z"));
    }

    #[test]
    fn stored_gzip_has_a_valid_crc_and_round_trips_with_gzip() {
        if process::find_tool("gzip").is_none() {
            return;
        }
        let test = TestDirectory::create();
        let compressed_path = test.0.join("data.gz");
        let expected = b"#mtree\n./file type=file uid=0 gid=0 mode=644\n";
        let mut input = &expected[..];
        let mut output = File::create(&compressed_path).expect("compressed file");
        write_gzip_stored(&mut input, &mut output).expect("write gzip");
        drop(output);

        let gzip = process::require_tool("gzip").expect("gzip path");
        let decoded = process::capture_stdout(
            Command::new(gzip)
                .arg("-cd")
                .arg(&compressed_path)
                .stdin(Stdio::null()),
        )
        .expect("decode gzip");
        assert_eq!(decoded, expected);
    }

    #[test]
    fn package_suffix_is_required() {
        assert!(validate_package_suffix(Path::new("a.pkg.tar.zst")).is_ok());
        assert!(validate_package_suffix(Path::new("a.tar.zst")).is_err());
    }

    #[test]
    fn generated_package_is_readable_by_archive_tools() {
        if process::find_tool("bsdtar").is_none() || process::find_tool("zstd").is_none() {
            return;
        }
        let test = TestDirectory::create();
        let root = test.0.join("root");
        let destination = test.0.join("sample-1-1-any.pkg.tar.zst");
        fs::create_dir(&root).expect("package root");
        fs::create_dir_all(root.join("usr/share/sample")).expect("payload directory");
        fs::write(root.join("usr/share/sample/data"), b"payload\n").expect("payload file");
        fs::write(
            root.join(".PKGINFO"),
            b"pkgname = sample\npkgbase = sample\nxdata = pkgtype=pkg\npkgver = 1-1\npkgdesc = sample\nurl = https://example.invalid\nbuilddate = 0\npackager = debtap-rs\nsize = 8\narch = any\n",
        )
        .expect("PKGINFO");

        generate_mtree_with_epoch(&root, Some(0)).expect("generate MTREE");
        create_package_with_options(
            &root,
            &destination,
            PackageOptions {
                source_date_epoch: Some(0),
                validate_with_pacman: true,
                ..PackageOptions::default()
            },
        )
        .expect("create package");

        let bsdtar = process::require_tool("bsdtar").expect("bsdtar");
        let listing = process::capture_text(
            Command::new(bsdtar)
                .arg("-tf")
                .arg(&destination)
                .stdin(Stdio::null()),
        )
        .expect("list package");
        assert!(listing.lines().any(|line| line == ".PKGINFO"));
        assert!(listing.lines().any(|line| line == "usr/share/sample/data"));
    }
}
