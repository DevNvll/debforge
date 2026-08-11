#!/usr/bin/bash

set -euo pipefail

if (( $# < 1 || $# > 2 )); then
    printf 'Usage: %s package.deb [debtap-rs-binary]\n' "$0" >&2
    exit 2
fi

input=$(/usr/bin/realpath -- "$1")
if [[ ! -f "$input" ]]; then
    printf 'Input does not exist: %s\n' "$input" >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd -- "$script_dir/.." && pwd)
binary=${2:-"$project_dir/target/release/debtap-rs"}

if [[ ! -x "$binary" ]]; then
    printf 'Build the release binary first: cargo build --release --locked --offline\n' >&2
    exit 2
fi

cache_root="${XDG_CACHE_HOME:-$HOME/.cache}"
/usr/bin/mkdir -p -- "$cache_root"
test_dir=$(/usr/bin/mktemp -d --tmpdir="$cache_root" debtap-rs-benchmark.XXXXXX)

cleanup() {
    case "$test_dir" in
        "$cache_root"/debtap-rs-benchmark.*)
            /usr/bin/rm -rf -- "$test_dir"
            ;;
    esac
}
trap cleanup EXIT INT TERM

start_ns=$(/usr/bin/date +%s%N)
SOURCE_DATE_EPOCH=1700000000 "$binary" \
    --output "$test_dir" \
    --scripts safe \
    "$input"
end_ns=$(/usr/bin/date +%s%N)

package=$(/usr/bin/find "$test_dir" -maxdepth 1 -type f -name '*.pkg.tar.zst' -print -quit)
if [[ -z "$package" ]]; then
    printf 'The benchmark did not create a package.\n' >&2
    exit 1
fi

/usr/bin/pacman -Qp -- "$package" >/dev/null
elapsed_ms=$(((end_ns - start_ns) / 1000000))
output_bytes=$(/usr/bin/stat -c '%s' "$package")

printf 'input_bytes=%s\n' "$(/usr/bin/stat -c '%s' "$input")"
printf 'output_bytes=%s\n' "$output_bytes"
printf 'elapsed_ms=%s\n' "$elapsed_ms"
