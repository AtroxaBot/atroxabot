#!/bin/sh
# Makes a version: sets it in Cargo.toml (the one place the version lives),
# commits that and tags the commit.
#
#   sh packaging/release.sh 1.2.3
#
# Nothing is pushed. Pushing the tag is what publishes: the release pipeline
# (.github/workflows/ci.yml) then builds every platform, signs the release
# and puts it on GitHub, where the app's "Check for updates" finds it.
set -eu

version=${1:-}
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
    echo "Give the version as three numbers: sh packaging/release.sh 1.2.3" >&2
    exit 1
fi
cd "$(dirname "$0")/.."
if [ -n "$(git status --porcelain)" ]; then
    echo "There are changes that are not committed. Commit or stash them first." >&2
    exit 1
fi
if git rev-parse -q --verify "refs/tags/v$version" > /dev/null; then
    echo "Version $version is tagged already." >&2
    exit 1
fi

# The first `version = "…"` is the workspace's; every crate takes it from there.
awk -v version="$version" '
    !done && /^version = "/ { sub(/"[^"]*"/, "\"" version "\""); done = 1 }
    { print }
' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml
# Cargo.lock names the workspace's crates with their version.
cargo update --workspace --quiet

git commit --quiet -m "Version $version" Cargo.toml Cargo.lock
git tag "v$version"
echo "Version $version is committed and tagged. To publish it:"
echo "    git push origin HEAD v$version"
