#!/usr/bin/env bash
# Publish the channel manifest for one version: hash every tarball the
# version prefix holds, as served, and repoint the stable pointer.
#
#   .gitea/scripts/publish-manifest.sh 0.5.0
#
# The release-manifest job calls this once every build leg has landed;
# the manifest workflow (workflow_dispatch) calls it to recover a
# release whose legs cannot be re-run, or after a platform published
# outside the matrix (a Mac running the rhema.rs repo's
# scripts/publish-binary.sh).
set -euo pipefail

version="${1:?usage: publish-manifest.sh VERSION}"
endpoint="https://s3.rhema.rs"
base="https://get.rhema.rs"

mkdir -p dist
# Whatever the legs uploaded, plus any platform built outside this
# matrix: the prefix is the set, not this file.
names=$(aws s3 ls "s3://get/$version/" --endpoint-url "$endpoint" |
  awk '{print $4}' | sed -n 's/^rhema-\(.*\)\.tar\.gz$/\1/p' | sort)
for required in linux-amd64 linux-arm64 linux-riscv64 darwin-arm64; do
  echo "$names" | grep -qx "$required" ||
    { echo "no rhema-$required.tar.gz under $version" >&2; exit 1; }
done
for name in $names; do
  curl -fsSL "$base/$version/rhema-$name.tar.gz" -o "dist/rhema-$name.tar.gz"
done
(cd dist && sha256sum rhema-*.tar.gz > checksums.txt)
# One binary per line, on the same line as its name: the installer
# parses the manifest with line-wise sed, so reformatting this through
# json.dumps breaks every install.
{
  printf '{\n  "stable": "%s",\n  "binaries": {\n' "$version"
  first=1
  for name in $names; do
    if [ "$first" -eq 1 ]; then first=0; else printf ',\n'; fi
    sha="$(sha256sum "dist/rhema-$name.tar.gz" | awk '{print $1}')"
    printf '    "rhema-%s": {"url": "%s/%s/rhema-%s.tar.gz", "sha256": "%s"}' \
      "$name" "$base" "$version" "$name" "$sha"
  done
  printf '\n  }\n}\n'
} > dist/manifest.json
aws s3 cp dist/checksums.txt "s3://get/$version/" --endpoint-url "$endpoint"
aws s3 cp dist/manifest.json "s3://get/manifest.json" --endpoint-url "$endpoint"
