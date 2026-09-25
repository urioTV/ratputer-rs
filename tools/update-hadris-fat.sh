#!/usr/bin/env bash
# Refresh vendor/hadris-fat from an official crates.io release and reapply the
# RATPUTER patches. The existing vendor is restored if the firmware build fails.
set -euo pipefail

crate="hadris-fat"
repo="https://github.com/hxyulin/hadris"
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
vendor="$root/vendor/hadris-fat"
patch_dir="$root/vendor-patches/hadris-fat"

usage() {
    echo "Usage: $0 <version>" >&2
    echo "Example: $0 2.5.0" >&2
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

for patch in \
    0001-bounded-directory-window.patch \
    0002-resumable-append-cursor.patch \
    0003-resumable-read-cursor.patch; do
    [[ -f "$patch_dir/$patch" ]] || {
        echo "error: missing patch: $patch_dir/$patch" >&2
        exit 1
    }
done

work=$(mktemp -d "${TMPDIR:-/tmp}/ratputer-hadris.XXXXXX")
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

# Tests and examples account for most of the crate size and are not compiled as
# part of the firmware. Remove their Cargo targets and in-lib integration-test
# includes so cargo fmt/check do not follow paths which are intentionally absent.
rm -rf "$staged/tests" "$staged/examples"
python3 - "$staged/Cargo.toml" "$staged/src/lib.rs" <<'PY'
from pathlib import Path
import sys

manifest = Path(sys.argv[1])
lines = manifest.read_text().splitlines(keepends=True)
out = []
skipping = False
for line in lines:
    stripped = line.strip()
    if stripped in ("[[example]]", "[[test]]"):
        skipping = True
        continue
    if skipping and stripped.startswith("["):
        skipping = False
    if not skipping:
        out.append(line)
manifest.write_text("".join(out))

lib = Path(sys.argv[2])
text = lib.read_text()
marker = "pub use error::{Error, Result};"
pos = text.find(marker)
if pos < 0:
    raise SystemExit("error: cannot find the expected lib.rs test-module boundary")
lib.write_text(text[: pos + len(marker)] + "\n")
PY

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
# Source provenance for the vendored hadris-fat crate.
# Update this file only through tools/update-hadris-fat.sh.
repository = "$repo"
crate = "$crate"
version = "$version"
tag = "v$version"
commit = "$commit"
crate_sha256 = "$checksum"
EOF

# Replace only after download, verification and patching succeeded. Keep the
# previous tree until the project has compiled successfully.
backup="$work/previous-vendor"
lock_backup="$work/Cargo.lock"
cp "$root/Cargo.lock" "$lock_backup"
mv "$vendor" "$backup"
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
echo "Review vendor/hadris-fat, Cargo.lock, and the patch applicability before committing."
