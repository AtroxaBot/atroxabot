#!/bin/sh
# Writes and signs a release's manifest, which the app believes an update by
# (crates/core/src/update.rs):
#
#   UPDATE_SIGNING_KEY=<the private key, PEM> sh packaging/manifest.sh <folder of the release's files> <version>
#
# It writes <folder>/update.manifest: the Ed25519 signature in hex on the
# first line, then the JSON it signs, with the size and SHA-256 of each file
# the app updates itself from. The key must be the one whose public half is
# in packaging/update-key.pub, which is compiled into the app: a release
# signed with any other is refused by every installed copy.
set -eu

dir=$1
version=$2
here=$(dirname "$0")

if [ -z "${UPDATE_SIGNING_KEY:-}" ]; then
    echo "UPDATE_SIGNING_KEY is not set: the release cannot be signed, and no app would take it." >&2
    exit 1
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf '%s\n' "$UPDATE_SIGNING_KEY" > "$work/key.pem"

public=$(openssl pkey -in "$work/key.pem" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \r\n')
if [ "$public" != "$(tr -d ' \r\n' < "$here/update-key.pub")" ]; then
    echo "UPDATE_SIGNING_KEY is not the key in packaging/update-key.pub: no app would take this release." >&2
    exit 1
fi

files=""
for name in atroxabot-windows-x86_64.exe atroxabot-linux-x86_64.tar.gz atroxabot-macos-arm64.tar.gz; do
    if [ ! -f "$dir/$name" ]; then
        echo "The release has no $name." >&2
        exit 1
    fi
    size=$(wc -c < "$dir/$name" | tr -d ' ')
    hash=$(sha256sum "$dir/$name" | cut -c 1-64)
    files="$files${files:+,}\"$name\":{\"size\":$size,\"sha256\":\"$hash\"}"
done
printf '{"version":"%s","files":{%s}}\n' "$version" "$files" > "$work/manifest.json"
openssl pkeyutl -sign -inkey "$work/key.pem" -rawin -in "$work/manifest.json" -out "$work/manifest.sig"
{
    od -An -tx1 "$work/manifest.sig" | tr -d ' \r\n'
    echo
    cat "$work/manifest.json"
} > "$dir/update.manifest"
echo "Signed version $version: $dir/update.manifest"
