#!/usr/bin/env bash
# Refresh vendor/sunset from an official crates.io release and reapply the
# RATPUTER patches (every vendor-patches/sunset/*.patch, in name order). The
# existing vendor is restored if the firmware build fails.
set -euo pipefail

crate="sunset"
repo="https://github.com/mkj/sunset"
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
vendor="$root/vendor/sunset"
patch_dir="$root/vendor-patches/sunset"

usage() {
    echo "Usage: $0 <version>" >&2
    echo "Example: $0 0.6.0" >&2
    exit 2
}

[[ $# -eq 1 && $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] || usage
version=$1

for command in curl python3 tar sha256sum git; do
    command -v "$command" >/dev/null || {
        echo "error: required command not found: $command" >&2
        exit 1
    }
done

[[ -d "$patch_dir" ]] || {
    echo "error: missing patch directory: $patch_dir" >&2
    exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/ratputer-sunset.XXXXXX")
backup=
lock_backup=
restore_vendor() {
    status=$?
    if [[ -n ${backup:-} && -d $backup ]]; then
        echo "Restoring the previous vendor after a failed validation..." >&2
        rm -rf "$vendor"
        mv "$backup" "$vendor"
    fi
    if [[ -n ${lock_backup:-} && -f $lock_backup ]]; then
        cp "$lock_backup" "$root/Cargo.lock"
    fi
    rm -rf "$work"
    exit "$status"
}
trap restore_vendor EXIT

metadata="$work/metadata.json"
archive="$work/$crate-$version.crate"
echo "Fetching $crate $version metadata..."
curl --fail --location --silent --show-error \
    --user-agent "ratputer-rs-vendor-updater (https://github.com/urioTV/ratputer-rs)" \
    "https://crates.io/api/v1/crates/$crate/$version" -o "$metadata"

readarray -t release < <(python3 - "$metadata" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    version = json.load(stream)["version"]
print(version["checksum"])
print(version.get("yanked", False))
PY
)
checksum=${release[0]}
[[ ${release[1]} == False ]] || {
    echo "error: $crate $version is yanked" >&2
    exit 1
}

curl --fail --location --silent --show-error \
    --user-agent "ratputer-rs-vendor-updater (https://github.com/urioTV/ratputer-rs)" \
    "https://crates.io/api/v1/crates/$crate/$version/download" -o "$archive"
echo "$checksum  $archive" | sha256sum --check --status || {
    echo "error: crates.io checksum mismatch" >&2
    exit 1
}

tar -xzf "$archive" -C "$work"
staged="$work/$crate-$version"
[[ -f "$staged/Cargo.toml" ]] || {
    echo "error: downloaded crate has an unexpected layout" >&2
    exit 1
}

# Repository-only files: CI scripts, design notes, and a rust-toolchain.toml
# that must not be mistaken for this project's pinned Xtensa toolchain.
rm -rf "$staged/.github" "$staged/testing" "$staged/docs" "$staged/rust-toolchain.toml"

# The crate's [profile.*] tables are ignored for path dependencies and only
# produce cargo warnings; the workspace root defines the profiles.
python3 - "$staged/Cargo.toml" <<'PY'
from pathlib import Path
import sys

manifest = Path(sys.argv[1])
out = []
skipping = False
for line in manifest.read_text().splitlines(keepends=True):
    stripped = line.strip()
    if stripped.startswith("[profile"):
        skipping = True
        continue
    if skipping and stripped.startswith("["):
        skipping = False
    if not skipping:
        out.append(line)
manifest.write_text("".join(out))
PY

shopt -s nullglob
for patch in "$patch_dir"/*.patch; do
    echo "Applying $(basename "$patch")..."
    git -C "$staged" apply --check "$patch"
    git -C "$staged" apply "$patch"
done

commit=$(python3 - "$staged/.cargo_vcs_info.json" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        print(json.load(stream)["git"]["sha1"])
except (FileNotFoundError, KeyError, TypeError, json.JSONDecodeError):
    print("unknown")
PY
)
# PATCHES.md is project documentation, not part of the upstream crate.
if [[ -f "$vendor/PATCHES.md" ]]; then
    cp "$vendor/PATCHES.md" "$staged/PATCHES.md"
fi

cat >"$staged/UPSTREAM.toml" <<EOF
# Source provenance for the vendored sunset crate.
# Update this file only through tools/update-sunset.sh.
repository = "$repo"
crate = "$crate"
version = "$version"
tag = "$crate-$version"
commit = "$commit"
crate_sha256 = "$checksum"
EOF

# Replace only after download, verification and patching succeeded. Keep the
# previous tree until the project has compiled successfully.
backup="$work/previous-vendor"
lock_backup="$work/Cargo.lock"
cp "$root/Cargo.lock" "$lock_backup"
if [[ -d "$vendor" ]]; then
    mv "$vendor" "$backup"
fi
mv "$staged" "$vendor"

cd "$root"
echo "Validating the firmware..."
if [[ -n ${IN_NIX_SHELL:-} ]]; then
    cargo check --release
    build
else
    nix develop -c cargo check --release
    nix develop -c build
fi

rm -rf "$backup"
backup=
lock_backup=
trap - EXIT
rm -rf "$work"

echo "$crate $version is vendored, patched, and build-verified."
echo "Review vendor/sunset, Cargo.lock, and the patch applicability before committing."
