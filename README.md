# Debforge

Debforge is a fast Debian-to-Arch package converter and reviewed desktop
installer written in Rust. The compatible converter command is `debtap-rs`.
The system package also provides the shorter `debforge` command.

The converter removes the main Debtap performance problem. It does not scan
large Debian and Ubuntu `Contents` files once for each dependency. It reads the
local Arch package catalog one time and resolves all dependencies in memory.
It also streams the final archive into parallel Zstandard compression at a fast
default level.

## Important limit

Conversion is a package repack. It is not an ABI port. A converted application
can still depend on Debian filesystem rules, library versions, service tools, or
maintainer behavior. Review warnings before you install the result.

## Requirements

- Rust 1.85 or newer for a source build
- `ar`
- `bsdtar`
- `zstd`
- `pacman` for dependency selection and output validation
- `sha256sum` for source and installation integrity

There are no third-party Rust dependencies.

## Build

```sh
cargo build --release --locked --offline
```

The output binaries are `target/release/debtap-rs` and
`target/release/debforge-helper`.

For secure desktop integration, build and install the local Arch package:

```sh
./scripts/install-system.sh
```

This installs the converter, root-owned helper, Polkit policy, desktop file,
and icon. It also makes Debforge the default `.deb` application for the current
user.

To install only the converter for the current user:

```sh
./scripts/install-user.sh
```

The user-only installation cannot install packages until the secure system
helper is installed.

## Use

```sh
debtap-rs application.deb
debtap-rs --output /path/to/output application.deb
debtap-rs --compression-level 7 application.deb
```

To convert, review, authorize, and install a package:

```sh
debforge install application.deb
```

The review shows Pacman metadata, the planned transaction, file count, native
package name conflicts, installed versions, and all conversion warnings. The
root helper copies the reviewed package into a root-only directory and checks
its SHA-256 value again before Pacman reads it.

To restore the file association later:

```sh
debforge register-handler
```

The default Zstandard level is 3. A higher value makes a smaller package and
uses more CPU time.

Common Debtap options are accepted:

```text
-o, --output DIR
-q, --quiet
-Q, --Quiet
-w, --wipeout
-s, --pseudo
-p, --pkgbuild
-P, --Pkgbuild
-u, --update
-v, --version
-h, --help
```

Additional options are:

```text
--compression-level N
--scripts safe|none|raw
--packager TEXT
--source-date-epoch N
--no-mtree
--keep-work
--force
```

## Script safety

The default `--scripts safe` mode does not copy Debian maintainer scripts into
the Arch package. The installed Pacman hooks refresh desktop, MIME, icon, GLib,
linker, systemd, sysusers, and tmpfiles data.

This rule stops foreign root code such as these operations:

- APT repository and signing-key installation
- `dpkg` database changes
- `update-alternatives`
- service enablement
- AppArmor activation
- arbitrary network access

`--scripts none` also omits maintainer scripts and reports one short warning.

`--scripts raw` wraps the original scripts in Arch install functions. This mode
is unsafe and is only for manual review. The command prints a warning.

Debian `triggers` data is never executed. Safe mode records its omission in the
generated package so that the installation review can show it.

## Compatibility inspection

Normal conversion scans ELF files without starting one process per file. It
checks CPU architecture, ELF class, byte order, interpreter paths, required
shared-library names, Debian multiarch runtime paths, setuid and setgid modes,
and unsupported special files. A definite architecture mismatch or special
file stops conversion. Missing runtime items become installation warnings.

The scan does not prove complete ABI compatibility. A vendor package can still
require newer GLIBC or GLIBCXX symbol versions than the system provides.

## Installation receipts

Successful desktop installations write a private receipt below
`$XDG_STATE_HOME/debforge/receipts`, or `~/.local/state/debforge/receipts`.
Each receipt records source and converted-package hashes, the package version,
the Debforge version, and installation time.

## Output validation

Every normal conversion validates the result with `pacman -Qp` when Pacman is
available. You can run more checks with:

```sh
testpkg output.pkg.tar.zst
pacman -Qip output.pkg.tar.zst
pacman -Qlp output.pkg.tar.zst
bsdtar -tf output.pkg.tar.zst
```

## Tests

```sh
cargo test --locked --offline
cargo clippy --all-targets --locked --offline -- -D warnings
./scripts/benchmark.sh /path/to/application.deb
```

The benchmark performs a real conversion, validates the result, reports elapsed
time and output size, and removes its temporary output.

On this system, the 349,937,362-byte ChatGPT Debian package converted in 12.63
seconds. Pacman and `testpkg` accepted the generated package.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the pipeline, safety boundaries,
compatibility rules, and performance targets.

## Input format references

- [Debian binary package format](https://manpages.debian.org/testing/dpkg-dev/deb.5.en.html)
- [Debian binary control format](https://manpages.debian.org/unstable/dpkg-dev/deb-control.5.en.html)

## License

GPL-2.0-or-later
