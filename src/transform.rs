use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Component, Path, PathBuf};

use crate::error::{AppError, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformReport {
    pub installed_size: u64,
    pub backup_paths: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn normalize_payload(
    payload_root: &Path,
    architecture: &str,
    control_dir: &Path,
) -> Result<TransformReport> {
    normalize_layout(payload_root, architecture)?;
    let mut warnings = remove_debian_only_files(payload_root)?;
    add_owned_compatibility_files(payload_root, &mut warnings)?;
    validate_and_rewrite_links(payload_root, architecture)?;
    let (backup_paths, backup_warnings) =
        read_backup_paths(control_dir, payload_root, architecture)?;
    warnings.extend(backup_warnings);
    let installed_size = calculate_installed_size(payload_root)?;

    Ok(TransformReport {
        installed_size,
        backup_paths,
        warnings,
    })
}

pub fn normalize_layout(root: &Path, architecture: &str) -> Result<()> {
    for (source, destination) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/bin"),
        ("usr/sbin", "usr/bin"),
        ("usr/games", "usr/bin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib"),
        ("usr/lib64", "usr/lib"),
    ] {
        merge_path(&root.join(source), &root.join(destination))?;
    }

    let lib32_destination = if architecture == "x86_64" {
        "usr/lib32"
    } else {
        "usr/lib"
    };
    merge_path(&root.join("lib32"), &root.join(lib32_destination))?;

    for triplet in multiarch_triplets(architecture) {
        merge_path(&root.join("usr/lib").join(triplet), &root.join("usr/lib"))?;
    }

    Ok(())
}

fn multiarch_triplets(architecture: &str) -> &'static [&'static str] {
    match architecture {
        "x86_64" => &["x86_64-linux-gnu"],
        "i686" => &["i386-linux-gnu", "i686-linux-gnu"],
        "aarch64" => &["aarch64-linux-gnu"],
        "armv7h" => &["arm-linux-gnueabihf"],
        "arm" => &["arm-linux-gnueabi"],
        "ppc64le" => &["powerpc64le-linux-gnu"],
        "ppc64" => &["powerpc64-linux-gnu"],
        "riscv64" => &["riscv64-linux-gnu"],
        "s390x" => &["s390x-linux-gnu"],
        _ => &[],
    }
}

fn merge_path(source: &Path, destination: &Path) -> Result<()> {
    let source_metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::new(format!(
                "Cannot inspect {}: {error}",
                source.display()
            )));
        }
    };

    let destination_metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(AppError::new(format!(
                "Cannot inspect {}: {error}",
                destination.display()
            )));
        }
    };

    let Some(destination_metadata) = destination_metadata else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).context(format!("Cannot create {}", parent.display()))?;
        }
        fs::rename(source, destination).context(format!(
            "Cannot move {} to {}",
            source.display(),
            destination.display()
        ))?;
        return Ok(());
    };

    if source_metadata.is_dir() && destination_metadata.is_dir() {
        let mut entries = fs::read_dir(source)
            .context(format!("Cannot read {}", source.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context(format!("Cannot list {}", source.display()))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            merge_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
        fs::remove_dir(source).context(format!("Cannot remove {}", source.display()))?;
        return Ok(());
    }

    if source_metadata.is_file()
        && destination_metadata.is_file()
        && files_equal(source, destination)?
    {
        fs::remove_file(source).context(format!("Cannot remove {}", source.display()))?;
        return Ok(());
    }

    if source_metadata.file_type().is_symlink() && destination_metadata.file_type().is_symlink() {
        let source_target =
            fs::read_link(source).context(format!("Cannot read link {}", source.display()))?;
        let destination_target = fs::read_link(destination)
            .context(format!("Cannot read link {}", destination.display()))?;
        if source_target == destination_target {
            fs::remove_file(source).context(format!("Cannot remove {}", source.display()))?;
            return Ok(());
        }
    }

    Err(AppError::new(format!(
        "Path rewrite collision between {} and {}",
        source.display(),
        destination.display()
    )))
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata = fs::metadata(left).context(format!("Cannot read {}", left.display()))?;
    let right_metadata = fs::metadata(right).context(format!("Cannot read {}", right.display()))?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }

    let mut left_reader =
        BufReader::new(File::open(left).context(format!("Cannot open {}", left.display()))?);
    let mut right_reader =
        BufReader::new(File::open(right).context(format!("Cannot open {}", right.display()))?);
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];

    loop {
        let left_count = left_reader
            .read(&mut left_buffer)
            .context(format!("Cannot read {}", left.display()))?;
        let right_count = right_reader
            .read(&mut right_buffer)
            .context(format!("Cannot read {}", right.display()))?;
        if left_count != right_count || left_buffer[..left_count] != right_buffer[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn remove_debian_only_files(root: &Path) -> Result<Vec<String>> {
    let lintian = root.join("usr/share/lintian");
    if fs::symlink_metadata(&lintian).is_ok() {
        fs::remove_dir_all(&lintian).context(format!("Cannot remove {}", lintian.display()))?;
        Ok(vec![
            "Removed Debian-only Lintian package metadata.".to_string(),
        ])
    } else {
        Ok(Vec::new())
    }
}

fn add_owned_compatibility_files(root: &Path, warnings: &mut Vec<String>) -> Result<()> {
    let opencode_target = root.join("opt/OpenCode/ai.opencode.desktop");
    let opencode_link = root.join("usr/bin/ai.opencode.desktop");
    if opencode_target.is_file() && fs::symlink_metadata(&opencode_link).is_err() {
        let parent = opencode_link
            .parent()
            .ok_or_else(|| AppError::new("The OpenCode link has no parent directory."))?;
        fs::create_dir_all(parent).context(format!("Cannot create {}", parent.display()))?;
        symlink("../../opt/OpenCode/ai.opencode.desktop", &opencode_link)
            .context(format!("Cannot create {}", opencode_link.display()))?;
        warnings.push("Added the OpenCode command link as an owned package file.".to_string());
    }
    Ok(())
}

fn validate_and_rewrite_links(root: &Path, architecture: &str) -> Result<()> {
    for path in collect_paths(root)? {
        let metadata =
            fs::symlink_metadata(&path).context(format!("Cannot inspect {}", path.display()))?;
        if !metadata.file_type().is_symlink() {
            continue;
        }

        let target =
            fs::read_link(&path).context(format!("Cannot read link {}", path.display()))?;
        if target.is_absolute() {
            let rewritten = rewrite_absolute_target(&target, architecture)?;
            if rewritten != target {
                fs::remove_file(&path).context(format!("Cannot replace {}", path.display()))?;
                symlink(&rewritten, &path).context(format!("Cannot create {}", path.display()))?;
            }
        } else {
            let relative_parent = path
                .parent()
                .and_then(|parent| parent.strip_prefix(root).ok())
                .ok_or_else(|| AppError::new("A symbolic link is outside the payload root."))?;
            normalize_relative(&relative_parent.join(&target)).map_err(|error| {
                AppError::new(format!("Unsafe link {}: {error}", path.display()))
            })?;
        }
    }
    Ok(())
}

fn rewrite_absolute_target(target: &Path, architecture: &str) -> Result<PathBuf> {
    let relative = target.strip_prefix("/").map_err(|_| {
        AppError::new(format!(
            "Invalid absolute link target: {}",
            target.display()
        ))
    })?;
    let normalized = normalize_relative(relative)?;
    let rewritten = rewrite_relative_path(&normalized, architecture);
    Ok(Path::new("/").join(rewritten))
}

fn normalize_relative(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AppError::new(format!(
                        "Path escapes the package root: {}",
                        path.display()
                    )));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::new(format!(
                    "Path is not relative: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

fn rewrite_relative_path(path: &Path, architecture: &str) -> PathBuf {
    let text = path.to_string_lossy();
    for (source, destination) in [
        ("usr/sbin/", "usr/bin/"),
        ("usr/games/", "usr/bin/"),
        ("usr/lib64/", "usr/lib/"),
        ("bin/", "usr/bin/"),
        ("sbin/", "usr/bin/"),
        ("lib64/", "usr/lib/"),
        ("lib/", "usr/lib/"),
    ] {
        if let Some(rest) = text.strip_prefix(source) {
            return PathBuf::from(destination).join(rest);
        }
    }
    if let Some(rest) = text.strip_prefix("lib32/") {
        let destination = if architecture == "x86_64" {
            "usr/lib32"
        } else {
            "usr/lib"
        };
        return PathBuf::from(destination).join(rest);
    }
    path.to_path_buf()
}

pub fn read_backup_paths(
    control_dir: &Path,
    payload_root: &Path,
    architecture: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let conffiles = control_dir.join("conffiles");
    let content = match fs::read_to_string(&conffiles) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => {
            return Err(AppError::new(format!(
                "Cannot read {}: {error}",
                conffiles.display()
            )));
        }
    };

    let mut backups = Vec::new();
    let mut warnings = Vec::new();
    for line in content.lines() {
        let value = line.trim();
        if value.is_empty() {
            continue;
        }
        let relative = value.strip_prefix('/').unwrap_or(value);
        let normalized = normalize_relative(Path::new(relative))?;
        let rewritten = rewrite_relative_path(&normalized, architecture);
        if fs::symlink_metadata(payload_root.join(&rewritten)).is_ok() {
            backups.push(rewritten.to_string_lossy().into_owned());
        } else {
            warnings.push(format!(
                "Configuration file '{}' is not in the payload and was not added as a backup.",
                rewritten.display()
            ));
        }
    }
    backups.sort();
    backups.dedup();
    Ok((backups, warnings))
}

pub fn calculate_installed_size(root: &Path) -> Result<u64> {
    let mut seen_hard_links = HashSet::new();
    let mut size = 0_u64;
    for path in collect_paths(root)? {
        if is_package_metadata(root, &path) {
            continue;
        }
        let metadata =
            fs::symlink_metadata(&path).context(format!("Cannot inspect {}", path.display()))?;
        if metadata.is_file() {
            let identity = (metadata.dev(), metadata.ino());
            if metadata.nlink() <= 1 || seen_hard_links.insert(identity) {
                size = size.checked_add(metadata.len()).ok_or_else(|| {
                    AppError::new("The installed-size value is larger than u64 can store.")
                })?;
            }
        }
    }
    Ok(size)
}

fn is_package_metadata(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative.components().count() == 1
        && matches!(
            relative.to_str(),
            Some(".PKGINFO" | ".BUILDINFO" | ".MTREE" | ".INSTALL")
        )
}

fn collect_paths(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        let mut entries = fs::read_dir(directory)
            .context(format!("Cannot read {}", directory.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context(format!("Cannot list {}", directory.display()))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .context(format!("Cannot inspect {}", path.display()))?;
            paths.push(path.clone());
            if metadata.is_dir() {
                visit(&path, paths)?;
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(root, &mut paths)?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{calculate_installed_size, normalize_layout, read_backup_paths};

    fn fixture(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "debtap-rs-transform-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("fixture directory");
        path
    }

    #[test]
    fn merges_usr_paths_and_hidden_files() {
        let root = fixture("merge");
        fs::create_dir_all(root.join("bin")).expect("bin");
        fs::create_dir_all(root.join("usr/bin")).expect("usr bin");
        fs::write(root.join("bin/.hidden"), b"data").expect("file");
        fs::write(root.join("usr/bin/tool"), b"tool").expect("file");

        normalize_layout(&root, "x86_64").expect("normalization");
        assert_eq!(
            fs::read(root.join("usr/bin/.hidden")).expect("read"),
            b"data"
        );
        assert!(!root.join("bin").exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_different_files_at_one_output_path() {
        let root = fixture("collision");
        fs::create_dir_all(root.join("bin")).expect("bin");
        fs::create_dir_all(root.join("usr/bin")).expect("usr bin");
        fs::write(root.join("bin/tool"), b"one").expect("file");
        fs::write(root.join("usr/bin/tool"), b"two").expect("file");

        let error = normalize_layout(&root, "x86_64").expect_err("collision expected");
        assert!(error.to_string().contains("collision"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn counts_hard_link_content_once() {
        let root = fixture("hardlink");
        fs::write(root.join("one"), b"12345").expect("file");
        fs::hard_link(root.join("one"), root.join("two")).expect("hard link");
        symlink("one", root.join("link")).expect("symbolic link");

        assert_eq!(calculate_installed_size(&root).expect("size"), 5);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn maps_and_checks_configuration_files() {
        let root = fixture("backup-root");
        let control = fixture("backup-control");
        fs::create_dir_all(root.join("usr/lib")).expect("lib");
        fs::write(root.join("usr/lib/app.conf"), b"config").expect("config");
        fs::write(control.join("conffiles"), "/lib/app.conf\n/missing\n").expect("conffiles");

        let (backups, warnings) = read_backup_paths(&control, &root, "x86_64").expect("backups");
        assert_eq!(backups, vec!["usr/lib/app.conf"]);
        assert_eq!(warnings.len(), 1);
        fs::remove_dir_all(root).expect("cleanup");
        fs::remove_dir_all(control).expect("cleanup");
    }
}
