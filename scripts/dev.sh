#!/usr/bin/env bash
# Run the claw and the web UI together against scratch state — the quick
# debug loop. Nothing here touches your real rhema config or sessions:
# the daemon gets a throwaway config under $RHEMA_DEV_DIR (default
# /tmp/rhema-dev), persisted across runs so sessions survive a restart
# of the script; wipe the directory for a clean slate.
#
# Usage:
#   scripts/dev.sh            # real provider (CLAW_DEV_PROVIDER, key from
#                             # the provider's env var; default google)
#   scripts/dev.sh --mock     # zero credentials: canned streamed replies
#
# The UI proxies /v1 to the claw, so one URL matters:
#   http://localhost:3000
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="${RHEMA_DEV_DIR:-/tmp/rhema-dev}"
PROVIDER="${CLAW_DEV_PROVIDER:-google}"

MOCK=0
for arg in "$@"; do
  case "$arg" in
    --mock) MOCK=1 ;;
    *) echo "unknown argument: $arg (supported: --mock)" >&2; exit 2 ;;
  esac
done

mkdir -p "$SCRATCH/workspaces"

CONFIG="$SCRATCH/claw.yaml"
if [ "$MOCK" = 1 ]; then
  cat > "$CONFIG" <<EOF
# Scratch debug config (scripts/dev.sh --mock): no model is called.
db: $SCRATCH/claw.db
workspace_root: $SCRATCH/workspaces
listen: 127.0.0.1:7799
poll_interval_secs: 60
EOF
  FEATURES=(--features mock)
  RUN_FLAGS=(--mock)
else
  case "$PROVIDER" in
    google) KEY_VAR=GOOGLE_API_KEY ;;
    anthropic) KEY_VAR=ANTHROPIC_API_KEY ;;
    openai) KEY_VAR=OPENAI_API_KEY ;;
    openrouter) KEY_VAR=OPENROUTER_API_KEY ;;
    llama-cpp) KEY_VAR="" ;;
    *) echo "CLAW_DEV_PROVIDER must be google, anthropic, openai, openrouter, or llama-cpp" >&2; exit 2 ;;
  esac
  if [ -n "$KEY_VAR" ] && [ -z "${!KEY_VAR:-}" ]; then
    echo "set $KEY_VAR first (or run scripts/dev.sh --mock for zero credentials)" >&2
    exit 1
  fi
  cat > "$CONFIG" <<EOF
# Scratch debug config (scripts/dev.sh): throwaway db and workspaces.
provider: $PROVIDER
db: $SCRATCH/claw.db
workspace_root: $SCRATCH/workspaces
listen: 127.0.0.1:7799
poll_interval_secs: 60
EOF
  FEATURES=()
  RUN_FLAGS=()
fi

echo "[dev] claw config: $CONFIG"
cargo build -p rhema-claw "${FEATURES[@]}"

if [ ! -d "$ROOT/web/claw/node_modules" ]; then
  echo "[dev] installing web/claw dependencies"
  (cd "$ROOT/web/claw" && npm install)
fi

# exec so the trap's kill reaches the servers, not a wrapper.
"$ROOT/target/debug/rhema-claw" --config "$CONFIG" "${RUN_FLAGS[@]}" &
CLAW_PID=$!
(cd "$ROOT/web/claw" && exec ./node_modules/.bin/nuxt dev) &
UI_PID=$!

cleanup() {
  kill "$CLAW_PID" "$UI_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "[dev] claw:  http://127.0.0.1:7799  (config: $CONFIG)"
echo "[dev] ui:    http://localhost:3000   (ctrl-c stops both)"
wait "$CLAW_PID"
