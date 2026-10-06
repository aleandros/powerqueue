# Claude Code facts powerqueue relies on

Verified against Claude Code 2.1.287 (October 2026) and the official docs
(https://code.claude.com/docs/en/cli-reference, /hooks).

## Launching a session

```
claude --session-id <uuid> --model <fable|opus|sonnet|haiku> \
       --permission-mode <acceptEdits|bypassPermissions|...> \
       --settings <path/to/settings.json> --name <display-name> \
       [--effort high] [--fallback-model sonnet,haiku] [--add-dir ...] \
       "<initial prompt>"
```

* `--session-id` must be a UUID. Reuse it with `claude --resume <uuid>` to continue after a crash (the transcript is kept on disk).
* `--resume <uuid> "<prompt>"` resumes *and* sends a new message.
* `--settings` accepts a file path or inline JSON; keys override `settings.json` for that session only. Hooks can be passed this way.
* `--permission-prompts none` (print mode) denies prompts instead of blocking. In interactive mode (what we use, so the user can attach) a permission prompt blocks until answered; a `Notification` hook with `notification_type: permission_prompt` tells us.
* `--max-budget-usd` is print-mode only; not usable for interactive sessions.
* `--bare` disables CLAUDE.md/hooks/skills discovery — do **not** use it for task sessions (we rely on hooks and the repo's CLAUDE.md).
* Model aliases: `fable`, `opus`, `sonnet`, `haiku`. Full ids look like `claude-fable-5-1`, `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-4-5-20251001`.
* `claude auth status` prints JSON (`authMethod`, `configDirectory`); exit 0 if logged in.
* Exit codes: 0 on `/exit`; non-zero on crash. Rate limits do **not** exit the process; they surface as `StopFailure` hooks with `error_type: rate_limit`.

## Hooks

Settings shape (file passed via `--settings`):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "/abs/powerqueue hook --task <id> --session <sid> --event SessionStart", "timeout": 10 }] }],
    "Stop":         [{ "hooks": [{ "type": "command", "command": "... --event Stop", "timeout": 10 }] }],
    "StopFailure":  [{ "hooks": [{ "type": "command", "command": "... --event StopFailure", "timeout": 10 }] }],
    "SessionEnd":   [{ "hooks": [{ "type": "command", "command": "... --event SessionEnd", "timeout": 10 }] }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "... --event Notification", "timeout": 10 }] }],
    "PreCompact":   [{ "hooks": [{ "type": "command", "command": "... --event PreCompact", "timeout": 10, "async": true }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "... --event UserPromptSubmit", "timeout": 10 }] }],
    "PostToolUse": [{ "hooks": [{ "type": "command", "command": "... --event PostToolUse", "timeout": 10 }] }],
    "PostToolUseFailure": [{ "hooks": [{ "type": "command", "command": "... --event PostToolUseFailure", "timeout": 10 }] }]
  }
}
```

State-changing hooks run synchronously to preserve ordering; only `PreCompact`
runs asynchronously. Tool completion and prompt submission clear stale attention
when the task and session are still live.

Hook input arrives on **stdin** as JSON. Common fields: `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `permission_mode`. Event-specific:

| event | extra fields |
|-------|--------------|
| `SessionStart` | `source` (`startup\|resume\|clear\|compact\|fork`), `model` |
| `Stop` | `last_assistant_message`, `stop_hook_active` |
| `StopFailure` | `error_type` (`rate_limit\|overloaded\|authentication_failed\|server_error\|...`), `error_message` |
| `SessionEnd` | `reason` (`clear\|resume\|logout\|prompt_input_exit\|other`) |
| `Notification` | `message`, `notification_type` (`permission_prompt\|idle_prompt\|elicitation_dialog\|...`) |

Exit code 0 = ok. Exit 2 blocks (for `Stop` it forces Claude to continue — we never do that from `powerqueue hook`). Hooks must be fast; ours only writes a row to SQLite.

A `Stop` hook may return JSON `{"decision":"block","reason":"..."}` to keep Claude going; we do not use it.

## Transcripts

`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` (honour `CLAUDE_CONFIG_DIR` for the `~/.claude` part). The cwd is encoded by replacing every character that is not `[A-Za-z0-9]` with `-` (`/Users/me/code/app` → `-Users-me-code-app`). `SessionStart` hook input gives the exact `transcript_path`; prefer it.

One line per JSON object. Relevant shape for `type: "assistant"`:

```json
{"type":"assistant","uuid":"...","parentUuid":"...","sessionId":"...","timestamp":"2026-10-01T23:15:26.413Z",
 "requestId":"req_011...","cwd":"/path","version":"2.1.287","gitBranch":"main",
 "message":{"id":"msg_011...","model":"claude-fable-5-1","role":"assistant",
   "content":[{"type":"text","text":"..."}],
   "usage":{"input_tokens":2,"output_tokens":2793,"cache_creation_input_tokens":18834,"cache_read_input_tokens":25148}}}
```

**One API response is written as several lines** (one per content block, e.g. thinking + text + tool_use) that share `message.id`, `requestId` and the same `usage`. Count usage once per `message.id`. `input_tokens` is often a placeholder (0/1/2); cache and output fields are reliable. Lines with `isSidechain: true` belong to subagents and still count. Other line types: `user` (incl. `tool_result` blocks), `system`, `summary`, `attachment`, `file-history-snapshot` — ignore for usage.

## Usage limits

Subscriptions have a rolling 5-hour window and a weekly cap (fixed reset time per account, visible in `/usage` inside Claude Code). There is no public API for the remaining allowance; powerqueue therefore paces on its own measured weighted tokens, lets the user calibrate with `budget set-observed <percent>` and `budget set-reset <time>`, and treats `StopFailure{rate_limit}` as a hard signal.
