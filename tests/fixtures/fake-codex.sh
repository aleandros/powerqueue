#!/usr/bin/env bash
# A stand-in for the `codex` binary used by powerqueue's end-to-end tests.
#
# It mimics the observable behaviour powerqueue depends on (verified against
# codex-cli 0.159.2, see docs/reference/providers-research.md):
#   * accepts `[resume <thread>] -C <dir> -m <model> [-a ..] [-s ..] [-c k=v]...
#     [--add-dir <dir>]... [flags] "<prompt>"`
#   * writes a rollout under $CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<thread>.jsonl
#     (session_meta with the physical cwd, turn_context with the model,
#     token_count with last_token_usage and a rate_limits snapshot,
#     task_complete); a resume appends to the same file
#   * runs the `-c notify=[...]` program with the agent-turn-complete payload
#     as its last argument
#
# Behaviour is selected with FAKE_CODEX_MODE:
#   complete   (default) use some tokens, `powerqueue task complete`, notify DONE, exit 0
#   ratelimit  log a usage-limit error event in the rollout and wait
#   crash      first run exits 1 after some usage; the resumed run completes
# FAKE_CODEX_STATE_DIR (default: $TMPDIR) stores run counters.

set -u
exec python3 - "$@" <<'PY'
import datetime, json, os, subprocess, sys, time, uuid, glob

args = sys.argv[1:]
mode = os.environ.get("FAKE_CODEX_MODE", "complete")
state_dir = os.environ.get("FAKE_CODEX_STATE_DIR") or os.environ.get("TMPDIR", "/tmp")
os.makedirs(state_dir, exist_ok=True)
home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")

thread = None
cd = os.getcwd()
model = "gpt-6-astra"
notify = None
prompt = ""
i = 0
if args[:1] == ["resume"]:
    thread = args[1]
    i = 2
while i < len(args):
    a = args[i]
    if a in ("-C", "--cd"):
        cd = args[i + 1]; i += 2
    elif a in ("-m", "--model"):
        model = args[i + 1]; i += 2
    elif a in ("-c", "--config"):
        key, _, value = args[i + 1].partition("=")
        if key == "notify":
            notify = json.loads(value)  # TOML string arrays of plain paths are valid JSON
        i += 2
    elif a in ("-a", "-s", "--add-dir", "-p", "--profile"):
        i += 2
    elif a.startswith("-"):
        i += 1
    else:
        prompt = a; i += 1

os.chdir(cd)
cwd = os.path.realpath(os.getcwd())
resumed = thread is not None
thread = thread or str(uuid.uuid4())
print(f"fake-codex: thread={thread} resume={resumed} model={model} mode={mode}", file=sys.stderr)
print(f"fake-codex: prompt bytes={len(prompt)}", file=sys.stderr)

def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"

existing = glob.glob(os.path.join(home, "sessions", "*", "*", "*", f"rollout-*-{thread}.jsonl"))
if existing:
    rollout = existing[0]
else:
    day = os.path.join(home, "sessions", time.strftime("%Y/%m/%d"))
    os.makedirs(day, exist_ok=True)
    rollout = os.path.join(day, f"rollout-{time.strftime('%Y-%m-%dT%H-%M-%S')}-{thread}.jsonl")

ordinal = sum(1 for _ in open(rollout)) if os.path.exists(rollout) else 0

def emit(kind, payload):
    global ordinal
    with open(rollout, "a") as f:
        f.write(json.dumps({"timestamp": now(), "ordinal": ordinal, "type": kind, "payload": payload}) + "\n")
    ordinal += 1

def tokens(inp, cached, out):
    usage = {"input_tokens": inp, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
             "output_tokens": out, "reasoning_output_tokens": 0, "total_tokens": inp + out}
    emit("event_msg", {"type": "token_count",
                       "info": {"total_token_usage": usage, "last_token_usage": usage, "model_context_window": 258400},
                       "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 17.0, "window_minutes": 10080,
                                       "resets_at": int(time.time()) + 86400}, "secondary": None,
                                       "plan_type": "prolite", "rate_limit_reached_type": None}})

def run_notify(message):
    if not notify:
        return
    payload = {"type": "agent-turn-complete", "thread-id": thread, "turn-id": str(uuid.uuid4()), "cwd": cwd,
               "client": "codex-tui", "input-messages": [prompt[:200]], "last-assistant-message": message}
    subprocess.run(notify + [json.dumps(payload)], check=False)

if not resumed:
    emit("session_meta", {"id": thread, "session_id": thread, "cwd": cwd, "originator": "codex-tui",
                          "cli_version": "0.159.2", "source": "cli", "timestamp": now()})
turn = str(uuid.uuid4())
emit("event_msg", {"type": "task_started", "turn_id": turn, "started_at": int(time.time())})
emit("turn_context", {"turn_id": turn, "cwd": cwd, "approval_policy": "never", "model": model})

counter = os.path.join(state_dir, f"codex-runs-{thread}")
runs = int(open(counter).read().strip() or 0) + 1 if os.path.exists(counter) else 1
open(counter, "w").write(str(runs))

if mode == "ratelimit":
    emit("event_msg", {"type": "error", "message": "You’ve hit your usage limit. Try again at 9:00 PM."})
    time.sleep(float(os.environ.get("FAKE_CODEX_IDLE_SECS", "120")))
    sys.exit(0)

if mode == "crash" and runs == 1:
    tokens(5000, 3000, 300)
    print("fake-codex: simulating crash", file=sys.stderr)
    sys.exit(1)

tokens(18699, 12032, 500)
with open("FAKE_CODEX_TOUCHED.txt", "a") as f:
    f.write(f"fake change {time.time()}\n")
subprocess.run("git add -A && git -c user.name=fake -c user.email=fake@example.com commit -qm 'fake-codex: work on task'",
               shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
tokens(20000, 15000, 900)
task_id = os.environ.get("POWERQUEUE_TASK_ID")
pq = os.environ.get("POWERQUEUE_BIN", "powerqueue")
if task_id:
    subprocess.run([pq, "task", "complete", task_id, "--summary", "fake-codex finished"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
final = "All done.\n[[POWERQUEUE:DONE]] fake-codex finished"
emit("event_msg", {"type": "task_complete", "turn_id": turn, "last_agent_message": final})
run_notify(final)
time.sleep(1)
sys.exit(0)
PY
