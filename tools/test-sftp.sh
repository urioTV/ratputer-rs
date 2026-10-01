#!/usr/bin/env bash
# Host-side tests of the actual SFTP parser and patched FAT writer, followed by
# independent fsck validation. Only newly created temporary images are touched.
# Run: nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#dosfstools -c bash tools/test-sftp.sh
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Run outside the firmware's .cargo config (Xtensa and build-std).
cd "$work"
cargo build --manifest-path "$root/tools/sftp-test/Cargo.toml" --target x86_64-unknown-linux-gnu
binary="$root/tools/sftp-test/target/x86_64-unknown-linux-gnu/debug/ratputer-sftp-test"
for spec in '12 4M 4' '16 32M 4' '32 128M 1' '32 4G 64'; do
    read -r fat size sectors <<< "$spec"
    image="$work/fat$fat-$sectors.img"
    truncate -s "$size" "$image"
    mkfs.fat -F "$fat" -s "$sectors" "$image"
    "$binary" "$image"
    fsck.fat -n "$image"
done
