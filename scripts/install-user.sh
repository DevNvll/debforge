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
