# debtap-rs architecture

## Purpose

`debtap-rs` converts one Debian binary package into one Arch Linux package.
It replaces the slow shell control flow in Debtap with a small Rust process.
It uses the system archive tools for their mature format and compression support.

The first release has these targets:

- Convert the 350 MB ChatGPT Debian package in less than 90 seconds on this system.
- Keep peak memory use below 128 MB, apart from system archive-tool buffers.
- Do not download or update a package database during conversion.
- Do not run code from the Debian package during conversion.
- Produce a package that `pacman -Qp` and `pacman -Qip` can read.
- Remove its work directory after success, failure, or a normal Rust error.

## Pipeline

The converter has five stages.

1. **Read**
   - Validate the `!<arch>` Debian archive signature.
   - Extract the `control.tar` and `data.tar` members with `ar`.
   - Parse the Deb822 `control` file in Rust.

2. **Resolve**
   - Read available Arch package names once with `pacman -Slq`.
   - Parse all Debian dependency alternatives in one pass.
   - Apply a built-in map for names that differ between Debian and Arch.
   - Select an installed or available Arch alternative.

3. **Normalize**
   - Extract the payload once with `bsdtar`.
   - Merge `/bin`, `/sbin`, and `/lib*` into the Arch `/usr` layout.
   - Merge Debian multiarch library directories into the Arch library layout.
   - Preserve modes, symbolic links, hard links, and extended archive metadata.

4. **Describe**
   - Generate `.PKGINFO` directly from the parsed control data.
   - Generate `.MTREE` with SHA-256 file data unless the user disables it.
   - Use the installed Pacman hooks for standard cache updates.
   - Never copy Debian `apt`, repository, `dpkg`, or service-control code into the default install script.

5. **Write**
   - Stream one `bsdtar` process into one parallel `zstd` process.
   - Use Zstandard level 3 by default.
   - Write to a partial file and rename it only after both processes succeed.

## Why this is faster

Debtap scans Debian package-file indexes that are larger than 1 GB many times.
It then starts `pkgfile` once for many individual files. It also builds a temporary
package for a full Namcap shared-library scan. The cost grows with the number of
dependencies and payload files.

`debtap-rs` uses a constant number of process starts for normal conversion. It
reads the local Arch package-name set once. Dependency resolution is then an
in-memory set lookup. It does not inspect every ELF file. Extraction and output
compression remain disk-bound and run through libarchive and parallel Zstandard.

## Module boundaries

- `cli`: command-line compatibility and option validation.
- `control`: Deb822 parsing and Debian version and architecture conversion.
- `dependency`: relation parsing, alias mapping, and alternative selection.
- `workspace`: private work directories and cleanup.
- `archive`: Debian member extraction, payload extraction, MTREE generation, and package output.
- `transform`: filesystem layout changes and installed-size calculation.
- `scripts`: safe hook generation and explicit raw-script translation.
- `package`: Arch metadata and PKGBUILD generation.
- `process`: checked child-process execution and pipelines.

No module can install a package. Installation remains a separate, authorized
`pacman -U` step in the desktop handler.

## Dependency policy

The Rust binary has no third-party Rust dependencies. This keeps the build
reproducible with restricted network access. It requires these system commands:

- `ar`
- `bsdtar`
- `zstd`

It uses `pacman` when it is available. If `pacman` is not available, dependency
resolution uses the built-in mapping and keeps valid same-name dependencies.

## Maintainer-script policy

The default `safe` policy generates hooks only from payload types that need a
cache refresh. Examples are desktop files, MIME data, icon themes, GLib schemas,
systemd units, and tmpfiles rules.

The `none` policy writes no `.INSTALL` file.

The `raw` policy is an explicit compatibility mode. It wraps the original Debian
scripts in Arch install functions. The command prints a warning because raw
scripts can call Debian-only commands or change repository settings.

## Failure model

- A malformed control file stops conversion before payload extraction.
- An unsafe or unsupported archive entry makes `bsdtar` fail the conversion.
- A path-merge conflict stops conversion and reports both paths.
- An unknown dependency name is kept after safe name normalization and produces a warning.
- Existing output is not replaced unless `--force` is present.
- Partial output always uses a separate file name.

## Compatibility

The CLI accepts Debtap's common conversion options: `-o`, `-q`, `-Q`, `-w`,
`-p`, `-P`, `-u`, `-v`, and `-h`. Interactive metadata editing is intentionally
removed. Metadata is deterministic and can be changed with explicit options or
by building the generated PKGBUILD.
