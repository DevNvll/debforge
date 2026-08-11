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

The converter has six stages.

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

4. **Inspect**
   - Parse ELF headers and dynamic tables in Rust.
   - Reject a definite CPU architecture mismatch.
   - Reject device nodes, sockets, and other unsupported special files.
   - Report missing interpreters, missing shared libraries, privileged modes,
     and Debian multiarch runtime paths.

5. **Describe**
   - Generate `.PKGINFO` directly from the parsed control data.
   - Generate `.MTREE` with SHA-256 file data unless the user disables it.
   - Use the installed Pacman hooks for standard cache updates.
   - Never copy Debian `apt`, repository, `dpkg`, or service-control code into the default install script.

6. **Write**
   - Stream one `bsdtar` process into one parallel `zstd` process.
   - Use Zstandard level 3 by default.
   - Write to a partial file and rename it only after both processes succeed.

## Why this is faster

Debtap scans Debian package-file indexes that are larger than 1 GB many times.
It then starts `pkgfile` once for many individual files. It also builds a temporary
package for a full Namcap shared-library scan. The cost grows with the number of
dependencies and payload files.

`debtap-rs` uses a constant number of process starts for normal conversion. It
reads the local Arch package-name set once. Dependency resolution and ELF
inspection are then in-memory operations. Extraction and output compression
remain disk-bound and run through libarchive and parallel Zstandard.

## Module boundaries

- `cli`: command-line compatibility and option validation.
- `control`: Deb822 parsing and Debian version and architecture conversion.
- `dependency`: relation parsing, alias mapping, and alternative selection.
- `compatibility`: ELF and special-file inspection.
- `workspace`: private work directories and cleanup.
- `archive`: Debian member extraction, payload extraction, MTREE generation, and package output.
- `transform`: filesystem layout changes and installed-size calculation.
- `scripts`: safe hook generation and explicit raw-script translation.
- `package`: Arch metadata and PKGBUILD generation.
- `process`: checked child-process execution and pipelines.
- `installer`: unprivileged review, authorization, and receipts.
- `privileged`: fixed root helper interface, secure staging, and Pacman execution.

The converter cannot install a package. The installer is a separate
unprivileged flow. It creates a private conversion, shows a transaction review,
and sends only the reviewed package path and digest to a small Polkit helper.
The helper copies the same open file into a root-only directory, checks the
digest again, validates it, and starts `pacman -U`.

## Dependency policy

The Rust binary has no third-party Rust dependencies. This keeps the build
reproducible with restricted network access. It requires these system commands:

- `ar`
- `bsdtar`
- `zstd`

It uses `pacman` when it is available. If `pacman` is not available, dependency
resolution uses the built-in mapping and keeps valid same-name dependencies.

## Maintainer-script policy

The default `safe` policy omits Debian maintainer code. Standard Pacman
transaction hooks handle desktop files, MIME data, icon themes, GLib schemas,
systemd units, and tmpfiles rules when their owning Arch packages provide those
hooks. Debforge records omitted scripts and Debian triggers as review warnings.

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
- A special payload file or definite ELF architecture mismatch stops conversion.
- The root helper refuses links, wrong owners, changed hashes, relative paths,
  unexpected arguments, and packages larger than its fixed safety limit.

## Compatibility

The CLI accepts Debtap's common conversion options: `-o`, `-q`, `-Q`, `-w`,
`-p`, `-P`, `-u`, `-v`, and `-h`. Interactive metadata editing is intentionally
removed. Metadata is deterministic and can be changed with explicit options or
by building the generated PKGBUILD.
