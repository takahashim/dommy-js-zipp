#!/bin/sh
# Apply the zipp-rb P0 embed patches to the vendored zipp fork.
#
# The fork is pinned at upstream commit 0b4d26e6 (v0.0.23-2). The patches under
# patches/ add the embed surface a Ruby binding needs; they are maintained as a
# patch series rather than committed into the submodule so a fresh clone can
# reproduce the fork from the pinned upstream revision:
#
#   git submodule update --init
#   scripts/apply-patches.sh
#
# Re-running is safe: already-applied patches are skipped.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
fork="$root/vendor/zipp"

if [ ! -d "$fork/crates/zipp-vm" ]; then
    echo "vendor/zipp is missing; run: git submodule update --init" >&2
    exit 1
fi

for patch in "$root"/patches/*.patch; do
    [ -e "$patch" ] || continue
    if git -C "$fork" apply --reverse --check "$patch" 2>/dev/null; then
        echo "skip (already applied): $(basename "$patch")"
        continue
    fi
    echo "apply: $(basename "$patch")"
    git -C "$fork" apply "$patch"
done

echo "done."
