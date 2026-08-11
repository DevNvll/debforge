#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SOURCE_DATE_EPOCH: &str = "1700000000";
const OUTPUT_NAME: &str = "fixture-app-1.2.3%2D4-1-x86_64.pkg.tar.zst";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let base = std::env::temp_dir();
        for _ in 0..128 {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the system clock must be after 1970")
                .as_nanos();
            let path = base.join(format!(
                "debtap-rs-conversion-test-{}-{nanos:032x}-{counter:016x}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory {}: {error}", path.display()),
            }
        }
        panic!("cannot allocate a unique integration-test directory");
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn converts_a_deb_with_mapped_dependencies_and_safe_metadata() {
    let temporary = TestDirectory::new();
    let fixture = build_debian_fixture(&temporary.path);
    let first_output = temporary.path.join("output-one");
    let second_output = temporary.path.join("output-two");
    fs::create_dir(&first_output).expect("create first output directory");
    fs::create_dir(&second_output).expect("create second output directory");

    let first_run = convert_fixture(&fixture, &first_output, temporary.path.join("cache-one"));
    assert!(
        text(&first_run.stderr).contains("Safe mode omitted foreign Debian maintainer code"),
        "safe-script warning was absent:\n{}",
        text(&first_run.stderr)
    );
    assert!(
        text(&first_run.stderr).contains("Debian package-manager commands were not copied"),
        "package-manager warning was absent:\n{}",
        text(&first_run.stderr)
    );

    let first_package = first_output.join(OUTPUT_NAME);
    assert!(
        first_package.is_file(),
        "converter did not create {}\nstdout:\n{}\nstderr:\n{}",
        first_package.display(),
        text(&first_run.stdout),
        text(&first_run.stderr)
    );

    let listing = command_text(Command::new("bsdtar").arg("-tf").arg(&first_package));
    let entries = normalized_entries(&listing);
    assert!(entries.contains(".PKGINFO"));
    assert!(entries.contains(".MTREE"));
    assert!(entries.contains("usr/bin/fixture-app"));
    assert!(entries.contains("etc/fixture-app.conf"));
    assert!(
        !entries.contains(".INSTALL"),
        "safe mode copied a Debian maintainer script into the package"
    );

    let pkginfo = command_text(
        Command::new("bsdtar")
            .arg("-xOf")
            .arg(&first_package)
            .arg("./.PKGINFO"),
    );
    for expected in [
        "pkgname = fixture-app",
        "pkgbase = fixture-source",
        "pkgver = 2:1.2.3%2D4-1",
        "pkgdesc = Small integration fixture",
        "url = https://example.invalid/fixture",
        "builddate = 1700000000",
        "packager = Integration Test <test@example.invalid>",
        "arch = x86_64",
        "license = MIT",
        "backup = etc/fixture-app.conf",
        "depend = glibc",
        "depend = gtk3",
        "depend = libsecret",
        "optdepend = libappindicator: recommended by the Debian package",
        "xdata = debian-package=fixture-app",
        "xdata = debian-version=2:1.2.3-4",
        "xdata = debian-architecture=amd64",
    ] {
        assert!(
            pkginfo.lines().any(|line| line == expected),
            "missing metadata line '{expected}':\n{pkginfo}"
        );
    }
    assert!(
        !pkginfo
            .lines()
            .any(|line| line.starts_with("depend = libc6")),
        "the Debian libc package name was not mapped:\n{pkginfo}"
    );
    assert!(
        !pkginfo.lines().any(|line| line.contains(">= 2.35")),
        "a Debian-only version limit was retained:\n{pkginfo}"
    );
    assert_fixed_archive_times(
        &first_package,
        &temporary.path.join("extracted-first-package"),
    );

    validate_with_pacman_when_available(&first_package);
    validate_with_testpkg_when_available(&first_package);

    let second_run = convert_fixture(&fixture, &second_output, temporary.path.join("cache-two"));
    let second_package = second_output.join(OUTPUT_NAME);
    assert!(
        second_package.is_file(),
        "second conversion failed\nstdout:\n{}\nstderr:\n{}",
        text(&second_run.stdout),
        text(&second_run.stderr)
    );
    let second_pkginfo = command_text(
        Command::new("bsdtar")
            .arg("-xOf")
            .arg(&second_package)
            .arg(".PKGINFO"),
    );
    assert_eq!(
        pkginfo, second_pkginfo,
        "a fixed source date epoch did not produce stable package metadata"
    );
    assert_eq!(
        fs::read(&first_package).expect("read first converted package"),
        fs::read(&second_package).expect("read second converted package"),
        "a fixed source date epoch did not produce the same package bytes"
    );
    assert_fixed_archive_times(
        &second_package,
        &temporary.path.join("extracted-second-package"),
    );
}

fn build_debian_fixture(root: &Path) -> PathBuf {
    let build = root.join("fixture-build");
    let control_root = build.join("control-root");
    let payload_root = build.join("payload-root");
    fs::create_dir_all(&control_root).expect("create control root");
    fs::create_dir_all(payload_root.join("bin")).expect("create payload bin directory");
    fs::create_dir_all(payload_root.join("etc")).expect("create payload configuration directory");

    fs::write(build.join("debian-binary"), "2.0\n").expect("write debian-binary");
    fs::write(
        control_root.join("control"),
        concat!(
            "Package: fixture-app\n",
            "Source: fixture-source (2:1.2.3-4)\n",
            "Version: 2:1.2.3-4\n",
            "Architecture: amd64\n",
            "Depends: libgtk-3-0, libc6 (>= 2.30),\n",
            " libc6 (>= 2.35), libsecret-1-0\n",
            "Recommends: libappindicator3-1\n",
            "Installed-Size: 1\n",
            "Maintainer: Fixture Maintainer <fixture@example.invalid>\n",
            "Homepage: https://example.invalid/fixture\n",
            "License: MIT\n",
            "Description: Small integration fixture\n",
            " This folded long description proves that the real CLI reads Deb822 continuations.\n",
            " .\n",
            " It also has a second paragraph.\n",
        ),
    )
    .expect("write control file");
    fs::write(control_root.join("conffiles"), "/etc/fixture-app.conf\n").expect("write conffiles");
    fs::write(
        control_root.join("postinst"),
        "#!/bin/sh\nset -e\napt-get update\nmkdir -p /etc/apt/sources.list.d\nupdate-alternatives --install /usr/bin/fixture-app fixture-app /opt/fixture-app 1\n",
    )
    .expect("write unsafe maintainer script");
    make_executable(&control_root.join("postinst"));

    fs::write(
        payload_root.join("bin/fixture-app"),
        "#!/bin/sh\nprintf 'fixture-app\\n'\n",
    )
    .expect("write fixture executable");
    make_executable(&payload_root.join("bin/fixture-app"));
    fs::write(
        payload_root.join("etc/fixture-app.conf"),
        "fixture_enabled=true\n",
    )
    .expect("write fixture configuration");

    run_success(
        Command::new("bsdtar")
            .arg("-czf")
            .arg(build.join("control.tar.gz"))
            .arg("-C")
            .arg(&control_root)
            .arg("."),
    );
    run_success(
        Command::new("bsdtar")
            .arg("-czf")
            .arg(build.join("data.tar.gz"))
            .arg("-C")
            .arg(&payload_root)
            .arg("."),
    );

    let package = build.join("fixture-app.deb");
    run_success(
        Command::new("ar")
            .current_dir(&build)
            .arg("rcs")
            .arg(&package)
            .args(["debian-binary", "control.tar.gz", "data.tar.gz"]),
    );
    package
}

fn convert_fixture(input: &Path, output: &Path, cache: PathBuf) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_debtap-rs"));
    command
        .arg("--quiet")
        .arg("--scripts=safe")
        .arg("--compression-level=1")
        .arg("--source-date-epoch")
        .arg(SOURCE_DATE_EPOCH)
        .arg("--packager")
        .arg("Integration Test <test@example.invalid>")
        .arg("--output")
        .arg(output)
        .arg(input)
        .env("XDG_CACHE_HOME", cache)
        .env("LC_ALL", "C")
        .env_remove("SOURCE_DATE_EPOCH");
    run_success(&mut command)
}

fn validate_with_pacman_when_available(package: &Path) {
    match Command::new("pacman")
        .arg("-Qip")
        .arg("--")
        .arg(package)
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "pacman rejected the generated package\nstdout:\n{}\nstderr:\n{}",
            text(&output.stdout),
            text(&output.stderr)
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // The bsdtar listing and extraction above are the fallback package
            // validation on systems that do not have pacman.
        }
        Err(error) => panic!("cannot start pacman package validation: {error}"),
    }
}

fn validate_with_testpkg_when_available(package: &Path) {
    match Command::new("testpkg").arg(package).output() {
        Ok(output) => assert!(
            output.status.success(),
            "testpkg rejected the generated package\nstdout:\n{}\nstderr:\n{}",
            text(&output.stdout),
            text(&output.stderr)
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot start testpkg package validation: {error}"),
    }
}

fn assert_fixed_archive_times(package: &Path, extraction_root: &Path) {
    fs::create_dir(extraction_root).expect("create timestamp extraction directory");
    run_success(
        Command::new("bsdtar")
            .arg("-xpf")
            .arg(package)
            .arg("-C")
            .arg(extraction_root),
    );
    assert_tree_time(extraction_root, extraction_root);
}

fn assert_tree_time(root: &Path, directory: &Path) {
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
    {
        let entry = entry.expect("read extracted package entry");
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
        let modified = metadata
            .modified()
            .unwrap_or_else(|error| panic!("cannot read the time of {}: {error}", path.display()))
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|error| panic!("invalid time for {}: {error}", path.display()))
            .as_secs();
        assert_eq!(
            modified,
            SOURCE_DATE_EPOCH.parse::<u64>().expect("valid test epoch"),
            "archive entry {} does not use the fixed source date epoch",
            path.strip_prefix(root).unwrap_or(&path).display()
        );
        if metadata.is_dir() {
            assert_tree_time(root, &path);
        }
    }
}

fn normalized_entries(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_prefix("./").unwrap_or(line))
        .map(|line| line.trim_end_matches('/').to_owned())
        .collect()
}

fn make_executable(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot make {} executable: {error}", path.display()));
}

fn command_text(command: &mut Command) -> String {
    let output = run_success(command);
    String::from_utf8(output.stdout).expect("command output must be UTF-8")
}

fn run_success(command: &mut Command) -> Output {
    let description = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot start {description}: {error}"));
    assert!(
        output.status.success(),
        "command failed: {description}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        text(&output.stdout),
        text(&output.stderr)
    );
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
