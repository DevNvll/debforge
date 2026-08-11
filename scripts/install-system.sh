#!/usr/bin/bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd -- "$script_dir/.." && pwd)
packaging_dir="$project_dir/packaging"

cd -- "$packaging_dir"
makepkg --cleanbuild --clean --force --noconfirm -p PKGBUILD.local
package_path=$(makepkg --packagelist -p PKGBUILD.local)

if [[ ! -f "$package_path" ]]; then
    printf 'The Debforge package was not created: %s\n' "$package_path" >&2
    exit 1
fi

pkexec /usr/bin/pacman -U --needed --noconfirm -- "$package_path"
/usr/bin/debtap-rs register-handler

printf 'Installed Debforge and registered the .deb file handler.\n'
