# The `job` macro: gob is the backend, jq reads its JSON. The session
# tag is the ownership record; the stop file is the notification
# record. No helper process sits between the model and gob.
local verb="${1:-help}"
if [ $# -gt 0 ]; then shift; fi
local session="${RHEMA_SESSION:-}"
local session_file="${RHEMA_JOBS_SESSION:-}"
if [ -z "$session" ] && [ -n "$session_file" ] && [ -r "$session_file" ]; then
  session="$(cat "$session_file" 2>/dev/null)"
fi
session="${session%"${session##*[![:space:]]}"}"
local stopped_file="${TMPDIR:-/tmp}/rhema-jobs-${session}-stopped"

case "$verb" in
  run)
    if [ -z "$session" ]; then
      echo "job: not wired in this shell (no RHEMA_SESSION)" >&2
      return 4
    fi
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    command -v jq >/dev/null 2>&1 || { echo "job: jq is required for the job macro" >&2; return 1; }
    # A single word is already the line the model meant, quotes and all.
    # Several words are the shell's own split, so re-quote each one
    # before `bash -c` reparses the line.
    local line
    if [ $# -eq 1 ]; then
      line="$1"
    else
      line="$(printf '%q ' "$@")"
      line="${line% }"
    fi
    if [ -z "$line" ]; then
      echo "job: run needs a command" >&2
      return 2
    fi
    local listing
    listing="$(gob list --json --all 2>/dev/null)" || { echo "job: cannot list gob jobs" >&2; return 1; }
    # gob dedupes an identical running command by reusing it and
    # rewriting its description, which would move the job to this
    # session. Refuse first and name the owner.
    local found
    found="$(printf '%s' "$listing" | jq -r --arg cmd "$line" --arg cwd "$PWD" '
      first(.[]
      | select(.status | startswith("running"))
      | select((.workdir // $cwd) == $cwd)
      | select((if ((.command|length) == 3 and .command[0] == "bash" and .command[1] == "-c") then .command[2] else (.command|join(" ")) end) == $cmd)
      | [.id, (.description // "")] | @tsv) // empty')"
    if [ -n "$found" ]; then
      local id="${found%%	*}"
      local desc="${found#*	}"
      local owner=""
      case "$desc" in
        \[*\]) owner="${desc#\[}"; owner="${owner%%\]*}" ;;
      esac
      [ "${#owner}" -le 32 ] || owner=""
      if [ -n "$owner" ] && [ "$owner" != "$session" ]; then
        echo "already running as $id (started by session $owner); not starting a second one"
      else
        echo "already running as $id"
      fi
      echo "  job logs $id"
      echo "  job stop $id"
      return 0
    fi
    local desc="[$session] $line"
    local out
    if ! out="$(gob add --description "$desc" -- bash -c "$line" 2>&1)"; then
      echo "job: the backend refused it: $(printf '%s' "$out" | tail -n 1)" >&2
      return 1
    fi
    local id
    id="$(printf '%s\n' "$out" | awk '
      $1 == "Added" && $2 == "job" { print $3; exit }
      $1 == "Job" && $3 == "already" { print $2; exit }')"
    if [ -z "$id" ]; then
      id="$(gob list --json --all 2>/dev/null | jq -r --arg d "$desc" 'first(.[] | select(.description == $d) | .id) // empty')"
    fi
    if [ -z "$id" ]; then
      echo "started \`$line\` (id unknown; \`job ls\`)"
      return 0
    fi
    echo "started $id: $line"
    echo "  job logs $id"
    echo "  job stop $id"
    ;;
  ls|list)
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    command -v jq >/dev/null 2>&1 || { echo "job: jq is required for the job macro" >&2; return 1; }
    local scope=session json=0 filter="$session"
    while [ $# -gt 0 ]; do
      case "$1" in
        --all|-a) scope=machine ;;
        --dir) scope=directory ;;
        --json) json=1 ;;
        --session) shift; filter="${1:-}" ;;
        *) echo "job: ls does not take \`$1\`" >&2; return 2 ;;
      esac
      shift
    done
    local allflag=""
    [ "$scope" = directory ] || allflag="--all"
    local listing
    listing="$(gob list --json $allflag 2>/dev/null)" || { echo "job: cannot list gob jobs" >&2; return 1; }
    local rows
    rows="$(printf '%s' "$listing" | jq -c --arg scope "$scope" --arg tag "$filter" '
      [ .[]
        | select(
            if $scope == "session" then
              ((.description // "") | startswith("[" + $tag + "]"))
            else true end)
        | {
            id: .id,
            status: .status,
            exit_code: (.exit_code // null),
            command: (if ((.command|length) == 3 and .command[0] == "bash" and .command[1] == "-c") then .command[2] else (.command|join(" ")) end),
            session: (if ((.description // "") | test("^\\[[^]]{1,32}\\]")) then ((.description | capture("^\\[(?<s>[^]]+)\\]")).s) else null end),
            started_at: (.started_at // null)
          } ]')" || { echo "job: cannot read the listing" >&2; return 1; }
    if [ "$json" = 1 ]; then
      printf '%s\n' "$rows"
      return 0
    fi
    if [ "$rows" = "[]" ]; then
      echo "no jobs"
      return 0
    fi
    printf '%s' "$rows" | jq -r --arg scope "$scope" '
      .[]
      | "\(.id) \(.status)\(if (.status | startswith("running")) then "" elif .exit_code != null then " (\(.exit_code))" else "" end)\(if (.session != null and $scope != "session") then " [\(.session)]" else "" end): \(.command)"'
    ;;
  logs)
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    gob logs "$@"
    ;;
  stop)
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    local code=0
    gob stop "$@" || code=$?
    if [ "$code" = 0 ] && [ -n "$session" ]; then
      local id
      for id in "$@"; do printf '%s\n' "$id" >> "$stopped_file"; done
    fi
    return "$code"
    ;;
  wait)
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    gob await "$@"
    ;;
  rm|remove)
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    local code=0
    gob remove "$@" || code=$?
    if [ "$code" = 0 ] && [ -n "$session" ]; then
      local id
      for id in "$@"; do printf '%s\n' "$id" >> "$stopped_file"; done
    fi
    return "$code"
    ;;
  shutdown)
    if [ -z "$session" ]; then
      echo "job: not wired in this shell (no RHEMA_SESSION)" >&2
      return 4
    fi
    command -v gob >/dev/null 2>&1 || { echo "job: cannot find gob on PATH" >&2; return 5; }
    command -v jq >/dev/null 2>&1 || { echo "job: jq is required for the job macro" >&2; return 1; }
    local ids
    ids="$(gob list --json --all 2>/dev/null | jq -r --arg tag "$session" '
      .[] | select((.status | startswith("running")) and ((.description // "") | startswith("[" + $tag + "]"))) | .id')" || { echo "job: cannot list gob jobs" >&2; return 1; }
    local count=0 failed=0 id
    while IFS= read -r id; do
      [ -n "$id" ] || continue
      if gob stop "$id" >/dev/null 2>&1; then
        count=$((count + 1))
        printf '%s\n' "$id" >> "$stopped_file"
      else
        failed=$((failed + 1))
      fi
    done <<< "$ids"
    echo "stopped $count job(s)"
    if [ "$failed" -gt 0 ]; then
      echo "job: $failed job(s) would not stop" >&2
      return 1
    fi
    ;;
  help|--help|-h|"")
    cat <<'USAGE'
job: background shells for this session

    job run <command>          start a shell line in the background
    job ls [--dir] [--all]     this session's jobs (--dir: this directory's,
                               --all: the machine's)
    job logs <id>              a job's output so far
    job logs                   every job's, here
    job stop <id>              stop it
    job wait <id>              block until it finishes
    job rm <id>                forget a stopped job
    job shutdown               stop every job this session started
USAGE
    ;;
  *)
    echo "job: unknown verb \`$verb\`" >&2
    return 2
    ;;
esac
