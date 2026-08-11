# Security policy

## Supported versions

Security fixes apply to the current release line.

| Version | Supported |
| --- | --- |
| 0.2.x | Yes |
| 0.1.x | No |

## Report a vulnerability

Use the **Report a vulnerability** button on the GitHub Security page for this
repository. This method creates a private report for the project owner.

Do not open a public issue for a vulnerability that is not fixed.

Include this information when it is available:

- The affected Debforge version.
- The Arch Linux and Pacman versions.
- Steps that reproduce the problem.
- The expected result and the actual result.
- A small test package or proof of concept that does not contain private data.
- Your proposed fix, if you have one.

## Security boundary

Treat each Debian package as untrusted input. Debforge converts package content,
shows a Pacman review, and asks for authorization before installation. The root
helper accepts one fixed command form. It checks the package owner and SHA-256
value, copies the same open file into a root-only directory, validates the copy,
and then starts Pacman.

The default script policy does not copy Debian maintainer scripts. It does not
make all third-party package content safe. Review the package source and the
Debforge warnings before you authorize installation.
