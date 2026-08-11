# Debforge

Debforge is a fast Debian-to-Arch package converter written in Rust. Its
command-line program is `debtap-rs`. It is a clean rewrite of the main Debtap
conversion workflow.

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

There are no third-party Rust dependencies.

## Build

```sh
cargo build --release --locked --offline
```

The output binary is `target/release/debtap-rs`.

To install it for the current user:

```sh
./scripts/install-user.sh
```

## Use

```sh
debtap-rs application.deb
debtap-rs --output /path/to/output application.deb
debtap-rs --compression-level 7 application.deb
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
