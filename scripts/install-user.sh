#!/usr/bin/bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd -- "$script_dir/.." && pwd)
install_dir="${HOME}/.local/bin"

cd -- "$project_dir"
cargo build --release --locked --offline
/usr/bin/install -Dm0755 \
    "$project_dir/target/release/debtap-rs" \
    "$install_dir/debtap-rs"

printf 'Installed %s\n' "$install_dir/debtap-rs"

if [[ -x /usr/lib/debforge/debforge-helper ]]; then
    applications_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    icons_dir="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
    /usr/bin/install -Dm0644 \
        "$project_dir/assets/io.github.devnvll.Debforge.desktop" \
        "$applications_dir/io.github.devnvll.Debforge.desktop"
    /usr/bin/install -Dm0644 \
        "$project_dir/assets/io.github.devnvll.Debforge.svg" \
        "$icons_dir/io.github.devnvll.Debforge.svg"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$applications_dir"
    fi
    "$install_dir/debtap-rs" register-handler
else
    printf 'The converter is ready. Run scripts/install-system.sh for secure desktop installation.\n'
fi
