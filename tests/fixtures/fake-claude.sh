#!/usr/bin/env bash
# A stand-in for the `claude` binary used by powerqueue's end-to-end tests.
#
# It mimics the observable behaviour powerqueue depends on:
#   * accepts the flags the launcher passes (--session-id/--resume, --model,
#     --settings <file>, --permission-mode, --name, extra args, final prompt)
#   * fires the command hooks declared in the settings file with realistic
#     JSON payloads on stdin (SessionStart, Stop, StopFailure, SessionEnd)
#   * appends assistant lines with `message.usage` to the transcript JSONL
#     under $CLAUDE_CONFIG_DIR/projects/<encoded cwd>/<session>.jsonl
#
# Behaviour is selected with FAKE_CLAUDE_MODE:
#   complete   (default) start, use some tokens, complete the task, exit 0
#   crash-once first run exits 1 after some usage; the resumed run completes
#   idle       end the turn without a completion marker and wait
#   ratelimit  report a StopFailure{rate_limit} then exit 0
# FAKE_CLAUDE_STATE_DIR (default: $TMPDIR) stores run counters.

set -u
mode="${FAKE_CLAUDE_MODE:-complete}"
state_dir="${FAKE_CLAUDE_STATE_DIR:-${TMPDIR:-/tmp}}"
mkdir -p "$state_dir"

# ---------------------------------------------------------------- headless
# `powerqueue tune` runs `claude -p ... --output-format json` with the prompt
# on stdin and the draft directory as cwd. FAKE_TUNE_MODE selects what the
# "agent" does to the drafts:
#   edit     (default) add an override to PRIORITY.md and set
#            scheduler.max_concurrent = 3 in config.toml (appended; the test
#            configs have no [scheduler] table)
#   priority only edit PRIORITY.md
#   noop     change nothing
#   invalid  write an unparsable rule and an invalid config value
#   fail     exit 1 with a message on stderr
#   hang     sleep (for the timeout path)
# The prompt is saved to $FAKE_CLAUDE_STATE_DIR/tune-prompt.md and the
# argv to tune-argv.txt; `powerqueue priority check --file PRIORITY.md` is run
# through PATH and its exit code saved to tune-check-exit.txt, so tests can
# verify that the session sees the right binary and home.
headless=0
for arg in "$@"; do
  case "$arg" in -p|--print) headless=1 ;; esac
done
if [ "$headless" = 1 ]; then
  tune_mode="${FAKE_TUNE_MODE:-edit}"
  printf '%s\n' "$@" > "$state_dir/tune-argv.txt"
  cat > "$state_dir/tune-prompt.md"
  echo "fake-claude: headless mode=$tune_mode cwd=$(pwd) prompt bytes=$(wc -c < "$state_dir/tune-prompt.md")" >&2
  if command -v powerqueue >/dev/null 2>&1; then
    powerqueue priority check --file PRIORITY.md >/dev/null 2>&1
    echo "$?" > "$state_dir/tune-check-exit.txt"
    powerqueue config validate --file config.toml >/dev/null 2>&1
    echo "$?" > "$state_dir/tune-validate-exit.txt"
  fi
  case "$tune_mode" in
    edit)
      printf '\n## Overrides\n- FAKE-1: critical\n' >> PRIORITY.md
      printf '\n[scheduler]\nmax_concurrent = 3\n' >> config.toml
      ;;
    priority)
      printf '\n## Overrides\n- FAKE-1: critical\n' >> PRIORITY.md
      ;;
    noop) ;;
    invalid)
      printf '\n## Critical\n- bogus: nonsense\n' >> PRIORITY.md
      printf '\n[scheduler]\nmax_concurrent = 0\n' >> config.toml
      ;;
    fail)
      echo "fake-claude: simulated failure" >&2
      exit 1
      ;;
    hang)
      sleep "${FAKE_CLAUDE_IDLE_SECS:-120}"
      ;;
  esac
  printf '{"type":"result","subtype":"success","is_error":false,"duration_ms":1500,"num_turns":2,"result":"fake-tune (%s): made FAKE-1 critical.\\nSimulation now ranks it first.","session_id":"fake-%s","total_cost_usd":0.01}\n' "$tune_mode" "$$"
  exit 0
fi

session=""
resume=0
settings=""
model="sonnet"
prompt=""
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session="$2"; shift 2 ;;
    --resume) session="$2"; resume=1; shift 2 ;;
    --settings) settings="$2"; shift 2 ;;
    --model) model="$2"; shift 2 ;;
    --permission-mode|--name|--effort|--fallback-model|--allowedTools|--append-system-prompt|--output-format) shift 2 ;;
    --*) shift ;;
    *) prompt="$1"; shift ;;
  esac
done
[ -n "$session" ] || { echo "fake-claude: no session id" >&2; exit 2; }

cwd="$(pwd)"
encoded="$(printf '%s' "$cwd" | sed 's/[^A-Za-z0-9]/-/g')"
claude_home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
transcript_dir="$claude_home/projects/$encoded"
mkdir -p "$transcript_dir"
transcript="$transcript_dir/$session.jsonl"
echo "fake-claude: session=$session resume=$resume model=$model mode=$mode" >&2
echo "fake-claude: prompt bytes=${#prompt}" >&2

hook_cmd() { # $1 = event name -> prints the command string or nothing
  python3 - "$settings" "$1" <<'PY'
import json, sys
path, event = sys.argv[1], sys.argv[2]
try:
    cfg = json.load(open(path))
except Exception:
    sys.exit(0)
for entry in cfg.get("hooks", {}).get(event, []):
    for h in entry.get("hooks", []):
        if h.get("type") == "command":
            print(h["command"])
PY
}

fire() { # $1 = event, $2 = extra json fields (object body without braces)
  local cmd
  cmd="$(hook_cmd "$1")"
  [ -n "$cmd" ] || return 0
  printf '{"session_id":"%s","transcript_path":"%s","cwd":"%s","hook_event_name":"%s","permission_mode":"acceptEdits"%s}' \
    "$session" "$transcript" "$cwd" "$1" "${2:+,$2}" | sh -c "$cmd" || true
}

usage_line() { # $1 = msg id, $2 = output tokens, $3 = text
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
  local model_id
  case "$model" in
    fable) model_id="claude-fable-5-1" ;;
    opus) model_id="claude-opus-5-5" ;;
    haiku) model_id="claude-haiku-4-5-20251001" ;;
    *) model_id="claude-sonnet-5-5" ;;
  esac
  # Two lines sharing the same message id, like real transcripts (thinking + text).
  for block in '{"type":"thinking","thinking":"..."}' "{\"type\":\"text\",\"text\":\"$3\"}"; do
    printf '{"type":"assistant","uuid":"%s-%s","sessionId":"%s","timestamp":"%s","cwd":"%s","requestId":"req_%s","message":{"id":"%s","model":"%s","role":"assistant","content":[%s],"usage":{"input_tokens":2,"output_tokens":%s,"cache_creation_input_tokens":1200,"cache_read_input_tokens":8000}}}\n' \
      "$1" "$RANDOM" "$session" "$ts" "$cwd" "$1" "$1" "$model_id" "$block" "$2" >> "$transcript"
  done
}

source_kind="startup"; [ "$resume" = 1 ] && source_kind="resume"
fire SessionStart "\"source\":\"$source_kind\",\"model\":\"$model\""

task_id="${POWERQUEUE_TASK_ID:-}"
counter="$state_dir/runs-$session"
runs=$(( $(cat "$counter" 2>/dev/null || echo 0) + 1 ))
echo "$runs" > "$counter"

case "$mode" in
  crash-once)
    if [ "$runs" -eq 1 ]; then
      usage_line "msg_${session:0:8}_a" 300 "Working on it"
      echo "fake-claude: simulating crash" >&2
      exit 1
    fi
    ;;
  idle|attention-reply)
    usage_line "msg_${session:0:8}_idle" 50 "Should I continue?"
    fire Stop "\"last_assistant_message\":\"I made some changes. Should I continue?\",\"stop_hook_active\":false"
    if [ "$mode" = attention-reply ]; then
      IFS= read -r reply
      fire UserPromptSubmit '"prompt":"continue"'
      usage_line "msg_${session:0:8}_reply" 25 "Continuing after your answer"
    fi
    sleep "${FAKE_CLAUDE_IDLE_SECS:-120}"
    fire SessionEnd "\"reason\":\"other\""
    exit 0
    ;;
  ratelimit)
    fire StopFailure "\"error_type\":\"rate_limit\",\"error_message\":\"Rate limit exceeded\""
    sleep 1
    fire SessionEnd "\"reason\":\"other\""
    exit 0
    ;;
esac

# complete (and second run of crash-once)
usage_line "msg_${session:0:8}_1" 500 "Starting"
echo "fake change $(date +%s)" >> FAKE_CLAUDE_TOUCHED.txt
git add -A >/dev/null 2>&1 && git -c user.name=fake -c user.email=fake@example.com commit -qm "fake-claude: work on task" >/dev/null 2>&1 || true
usage_line "msg_${session:0:8}_2" 900 "Done"
if [ -n "$task_id" ] && command -v "${POWERQUEUE_BIN:-powerqueue}" >/dev/null 2>&1; then
  "${POWERQUEUE_BIN:-powerqueue}" task complete "$task_id" --summary "fake-claude finished" >/dev/null 2>&1 || true
fi
fire Stop "\"last_assistant_message\":\"All done.\\n[[POWERQUEUE:DONE]] fake-claude finished\",\"stop_hook_active\":false"
fire SessionEnd "\"reason\":\"prompt_input_exit\""
exit 0
