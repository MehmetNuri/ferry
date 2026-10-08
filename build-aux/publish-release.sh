#!/bin/sh
# Usage: build-aux/publish-release.sh VERSION [RUN_ID]
# Publishes the files of a passed Release run under your own GitHub account.
set -eu
version=$1
run=${2:-$(gh run list --workflow release.yml --branch "v$version" --status success --limit 1 \
    --json databaseId --jq '.[0].databaseId')}
[ -n "$run" ] || { echo "no successful Release run for v$version" >&2; exit 1; }
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
gh run download "$run" --name "release-$version" --dir "$dir"
(cd "$dir" && sha256sum -c SHA256SUMS)
gh release create "v$version" "$dir"/* --verify-tag \
    --title "Ferry $version" --notes-file "build-aux/release-notes/$version.md"
