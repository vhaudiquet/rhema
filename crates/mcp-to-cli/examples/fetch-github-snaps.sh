#!/bin/sh
# Fetch github-mcp-server's real tool definitions (the `__toolsnaps__`
# the upstream test suite snapshots) into a scratch directory, for the
# catalog example to build a full command tree from. Not a test
# fixture: the full set lives out of the tree, the trimmed fixtures in
# tests/fixtures/ are the committed matrix.
#
#   crates/mcp-to-cli/examples/fetch-github-snaps.sh /tmp/snaps
set -eu
dir=${1:?usage: fetch-github-snaps.sh <target-dir>}
mkdir -p "$dir"
base=https://raw.githubusercontent.com/github/github-mcp-server/main/pkg/github/__toolsnaps__
curl -sfL "https://api.github.com/repos/github/github-mcp-server/contents/pkg/github/__toolsnaps__" |
    jq -r '.[].name' |
    xargs -P 8 -I{} curl -sfL "$base/{}" -o "$dir/{}"
echo "$(ls "$dir" | wc -l | tr -d ' ') tool definitions in $dir"
