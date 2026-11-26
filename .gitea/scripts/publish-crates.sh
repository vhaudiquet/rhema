#!/usr/bin/env bash
# Publish the library closure to crates.io, in dependency order.
#
#   .gitea/scripts/publish-crates.sh --check    # offline: the list is complete
#                                               # and topologically ordered
#   .gitea/scripts/publish-crates.sh 0.2.3      # publish whatever is missing
#
# CRATES is hand-ordered because the release also skips crates that never
# reach crates.io (rhema-code waits for its crossterm patches to land
# upstream), so the list, not the workspace, says what ships. --check runs
# in CI on every push: a crate that joins the dependency chain without
# joining the list fails there, not halfway through a tag release.
#
# A versioned dev-dependency constrains the order as well: cargo strips the
# path when it packages a crate and resolves the version against crates.io,
# so the dependency must already be published. A path-only dev-dependency
# carries no version and adds no such requirement.
#
# CARGO_REGISTRY_TOKEN must be allowed to publish every crate in the
# list. A token restricted to specific crates answers 403 for the first
# name it does not carry, and a brand-new name is the usual culprit:
# tokens cannot be edited, so a crate joining the list needs a new one.
set -euo pipefail

CRATES=(
  rhema-core
  rhema-skills
  rhema-config
  rhema-session-store
  rhema-session-store-file
  rhema-session-store-sqlite
  rhema-submissions
  rhema-provider
  rhema-model-catalog
  rhema-prompt
  rhema-bash
  rhema-sandbox
  rhema-sandbox-local
  rhema-sandbox-http
  rhema-sandbox-ssh
  rhema-sandbox-docker
  rhema-sandbox-lxd
  rhema-sandbox-ecrou
  rhema-transcript
  rhema-host-agent
  rhema-rsd
  rhema-memory
  rhema-identity
  rhema-cloud-agent
  rhema-provider-anthropic
  rhema-provider-chatgpt
  rhema-provider-openai-compatible
  rhema-provider-opencode
  rhema-provider-copilot
  rhema-provider-google
  rhema-provider-llama-cpp
  rhema-provider-mistral
  rhema-provider-openrouter
  rhema-provider-registry
  rhema-acp
  rhema-websearch
  rhema-websearch-opencode
  rhema-websearch-brave
  rhema-websearch-tavily
  rhema-websearch-exa
  rhema-websearch-registry
  rhema-decision
  rhema-decision-opencode
  rhema-decision-jev
  rhema-decision-clef
  rhema-decision-openrouter
  rhema-decision-registry
  rhema-subagents
)

index_of() {
  local i
  for i in "${!CRATES[@]}"; do
    if [ "${CRATES[$i]}" = "$1" ]; then
      echo "$i"
      return 0
    fi
  done
  return 1
}

# The crate's in-tree dependency lines, as "<name> <kind>", where kind is
# versioned, path-only, dev-versioned, or dev-path-only.
# Only `path = ` entries are in-tree.
manifest_edges() {
  awk '
    /^\[/ { section = $0; next }
    section ~ /dependencies\]$/ && /path *=/ {
      name = $0
      sub(/[[:space:]]*=.*/, "", name)
      req = ""
      if (match($0, /version *= *"[^"]+"/)) {
        req = substr($0, RSTART, RLENGTH)
        sub(/^version *= *"/, "", req)
        sub(/"$/, "", req)
      }
      kind = (req != "" ? "versioned@" req : "path-only")
      if (section ~ /dev-dependencies/) kind = "dev-" kind
      print name, kind
    }
  ' "$1"
}

# An in-tree dependency's version requirement is typed by hand: every
# crate's own version inherits `version.workspace = true` from the root
# manifest, but the `version = "0.3.0"` on the dependency lines does not.
# A bump that misses one string is invisible to every build (the path
# wins silently), and at `cargo publish` the requirement resolves against
# a previous release of the dependency instead of the one that ships.
# Exact sync with the workspace version is the repo invariant; this
# checks it over the whole tree, not only the publish list, so the
# miss fails the push, not the tag release.
versioned_requirements_match() {
  local crate dep kind req stripped tag_version bad=0
  tag_version="$(awk -F'"' '/^version =/{print $2; exit}' Cargo.toml)"
  while read -r crate dep kind; do
    req="${kind#*@}"
    stripped="${req#^}"
    stripped="${stripped#[>=<~]}"
    if [ "$stripped" != "$tag_version" ]; then
      echo "$crate requires $dep at \"$req\" but the workspace version is $tag_version" >&2
      bad=1
    fi
  done < <(
    for manifest in crates/*/Cargo.toml; do
      crate="${manifest#crates/}"
      crate="${crate%/Cargo.toml}"
      manifest_edges "$manifest" |
        while read -r dep kind; do
          case "$kind" in
            versioned@* | dev-versioned@*) echo "$crate $dep $kind" ;;
          esac
        done
    done
  )
  return "$bad"
}

check() {
  local bad=0 crate manifest at dep dep_at kind
  for crate in "${CRATES[@]}"; do
    manifest="crates/$crate/Cargo.toml"
    if [ ! -f "$manifest" ]; then
      echo "publish list names $crate, which has no $manifest" >&2
      bad=1
      continue
    fi
    if grep -qE '^publish *= *false' "$manifest"; then
      echo "publish list names $crate, which sets publish = false" >&2
      bad=1
      continue
    fi
    at=$(index_of "$crate")
    while read -r dep kind; do
      case "$kind" in
        versioned@* | dev-versioned@*) needs_registry=yes ;;
        *) needs_registry=no ;;
      esac
      if [ "$needs_registry" = yes ]; then
        if ! dep_at=$(index_of "$dep"); then
          echo "$crate depends on $dep, which the publish list does not carry" >&2
          bad=1
        elif [ "$dep_at" -ge "$at" ]; then
          echo "$crate is listed before $dep; move $dep earlier" >&2
          bad=1
        fi
      elif [ "$kind" = path-only ]; then
        echo "$crate depends on $dep by path only; crates.io needs a version" >&2
        bad=1
      fi
    done < <(manifest_edges "$manifest")
  done
  versioned_requirements_match || bad=1
  return "$bad"
}

# Idempotent and rate-limit aware. crates.io caps NEW crates per rolling
# window, and a first release publishes a dozen at once, so: skip any
# crate whose exact version is already on the registry, tolerate the
# version landing between check and publish, and wait a rate-limited
# crate's named window out and retry it — eight refusals give up the
# crate, failing the run without blocking later ones, and re-running
# resumes where the last run stopped.
# Parses a rate-limit refusal out of cargo publish's output and sleeps
# the window it names out: the body says "Please try again after Thu,
# 19 Nov 2026 21:12:40 GMT". Returns nonzero when the failure is a
# different one, so the caller only waits refusals, not real errors.
wait_rate_limit() {
  local when secs
  when=$(printf '%s\n' "$1" \
    | sed -n 's/.*try again after \(.*GMT\).*/\1/Ip' | head -1)
  [ -n "$when" ] || return 1
  if ! secs=$(date -d "$when" +%s 2>/dev/null); then
    secs=$(( $(date +%s) + 600 )) # unparsed clock: one window's guess
  fi
  secs=$(( secs - $(date +%s) + 15 ))
  ((secs < 30)) && secs=30
  echo "rate-limited, sleeping ${secs}s for the window $when" >&2
  sleep "$secs"
}

publish() {
  local version="${1#v}" crate ok=yes out refusals
  check || {
    echo "the publish list is incomplete; nothing was uploaded" >&2
    return 1
  }
  for crate in "${CRATES[@]}"; do
    if curl -fsSL -A "rhema-release (git.vhaudiquet.fr/vhaudiquet/rhema)" \
      "https://crates.io/api/v1/crates/$crate/$version" >/dev/null 2>&1; then
      echo "$crate v$version is already on crates.io, skipping"
      continue
    fi
    echo "publishing $crate v$version ..."
    refusals=0
    until out=$(cargo publish -p "$crate" 2>&1); do
      if echo "$out" | grep -qi "already exists\|already uploaded"; then
        echo "$crate v$version landed between check and publish, counting as done"
        out=
        break
      elif wait_rate_limit "$out"; then
        if ((++refusals > 8)); then
          printf '%s\n' "$out" >&2
          ok=no
          break
        fi
        echo "retrying $crate v$version after the window"
      else
        printf '%s\n' "$out" >&2
        ok=no
        break
      fi
    done
    [ -n "$out" ] && echo "$out"
    sleep 5
  done
  [ "$ok" = yes ] || {
    echo "some crates did not publish; re-run this job to resume" >&2
    return 1
  }
}

case "${1:---check}" in
  --check)
    check
    ;;
  --help | -h)
    sed -n '2,9p' "$0"
    ;;
  *)
    publish "$1"
    ;;
esac
