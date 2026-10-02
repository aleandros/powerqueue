#!/usr/bin/env bash
# A stand-in for Google's Antigravity CLI (`agy`) used by powerqueue's
# end-to-end tests. unverified: see docs/reference/providers-research.md —
# the shapes below follow community reports, not a real agy run.
#
#   * accepts `[--conversation <id>] --model <slug> [--mode m] [--effort e]
#     [--dangerously-skip-permissions] [--add-dir d]... -i "<prompt>"`
#   * records `<physical cwd> -> <conversation id>` in
#     $POWERQUEUE_AGY_HOME/cache/last_conversations.json
#   * writes brain/<id>/.system_generated/logs/transcript.jsonl with a final
#     PLANNER_RESPONSE carrying the DONE marker
#   * runs the `Stop` hook from <cwd>/.agents/hooks.json with
#     {session_id, transcript_path, cwd, timestamp, hook_event_name} on stdin
#
# FAKE_AGY_MODE:
#   complete   (default) `powerqueue task complete`, transcript, Stop hook, exit 0
#   poll-only  transcript only (no hook, no CLI completion), then wait:
#              completion must come from the daemon polling the transcript

set -u
exec python3 - "$@" <<'PY'
import datetime, json, os, subprocess, sys, time, uuid

args = sys.argv[1:]
mode = os.environ.get("FAKE_AGY_MODE", "complete")

# `agy -p "/usage" --output-format json`: the usage probe (community-reported
# shape, see docs/reference/providers-research.md).
if "-p" in args or "--print" in args:
    reset = int(time.time()) + 3600
    print(json.dumps({"conversation_id": str(uuid.uuid4()), "status": "SUCCESS", "num_turns": 0, "response": "",
                      "command": {"name": "usage", "data": {"groups": [{"name": "Gemini Models", "buckets": [
                          {"id": "gemini-5h", "remaining_fraction": 0.8, "disabled": False, "window_minutes": 300, "reset_time": reset},
                          {"id": "gemini-weekly", "remaining_fraction": 0.6, "disabled": False, "window_minutes": 10080, "reset_time": reset + 86400}]}]}}}),
          flush=True)
    sys.exit(0)

# Never act outside a powerqueue task (the daemon may run this binary with
# other arguments from an arbitrary directory).
if not os.environ.get("POWERQUEUE_TASK_ID"):
    print(f"fake-agy: no POWERQUEUE_TASK_ID in the environment; ignoring args {args}", file=sys.stderr)
    sys.exit(0)
home = os.environ.get("POWERQUEUE_AGY_HOME") or os.path.expanduser("~/.gemini/antigravity-cli")
conversation = None
model = "gemini-3-pro"
prompt = ""
i = 0
while i < len(args):
    a = args[i]
    if a == "--conversation":
        conversation = args[i + 1]; i += 2
    elif a == "--model":
        model = args[i + 1]; i += 2
    elif a in ("--mode", "--effort", "--add-dir", "--output-format", "--print-timeout"):
        i += 2
    elif a == "-i":
        prompt = args[i + 1]; i += 2
    else:
        i += 1

cwd = os.path.realpath(os.getcwd())
conversation = conversation or str(uuid.uuid4())
print(f"fake-agy: conversation={conversation} model={model} mode={mode} prompt bytes={len(prompt)}", file=sys.stderr)

def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"

cache = os.path.join(home, "cache")
os.makedirs(cache, exist_ok=True)
last = os.path.join(cache, "last_conversations.json")
try:
    convs = json.load(open(last))
except Exception:
    convs = {}
convs[cwd] = conversation
json.dump(convs, open(last, "w"))

logs = os.path.join(home, "brain", conversation, ".system_generated", "logs")
os.makedirs(logs, exist_ok=True)
transcript = os.path.join(logs, "transcript.jsonl")
step = sum(1 for _ in open(transcript)) if os.path.exists(transcript) else 0

def emit(kind, source, content):
    global step
    with open(transcript, "a") as f:
        f.write(json.dumps({"step_index": step, "source": source, "type": kind, "status": "DONE",
                            "created_at": now(), "content": content}) + "\n")
    step += 1

emit("USER_INPUT", "USER", prompt[:200])
emit("PLANNER_RESPONSE", "MODEL", "Working on it")
with open("FAKE_AGY_TOUCHED.txt", "a") as f:
    f.write(f"fake change {time.time()}\n")
subprocess.run("git add -A && git -c user.name=fake -c user.email=fake@example.com commit -qm 'fake-agy: work on task'",
               shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
emit("RUN_COMMAND", "MODEL", {"command": "git commit"})

if mode != "poll-only":
    task_id = os.environ.get("POWERQUEUE_TASK_ID")
    pq = os.environ.get("POWERQUEUE_BIN", "powerqueue")
    if task_id:
        subprocess.run([pq, "task", "complete", task_id, "--summary", "fake-agy finished"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)

emit("PLANNER_RESPONSE", "MODEL", "All done.\n[[POWERQUEUE:DONE]] fake-agy finished")

if mode == "poll-only":
    time.sleep(float(os.environ.get("FAKE_AGY_IDLE_SECS", "120")))
    sys.exit(0)

try:
    hooks = json.load(open(os.path.join(cwd, ".agents", "hooks.json")))
except Exception:
    hooks = {}
payload = json.dumps({"session_id": conversation, "transcript_path": transcript, "cwd": cwd,
                      "timestamp": now(), "hook_event_name": "Stop"})
for entry in hooks.get("hooks", {}).get("Stop", []):
    for h in entry.get("hooks", []):
        if h.get("type") == "command":
            subprocess.run(h["command"], shell=True, input=payload.encode(), check=False)
time.sleep(1)
sys.exit(0)
PY
