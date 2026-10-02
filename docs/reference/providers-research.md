# Provider research: Claude Code, Codex CLI, Antigravity CLI (2026-10-01)

Working notes behind the multi-provider budget feature. Legend: **DOC** = in
official docs; **SRC** = inferred from the vendor's source/schemas; **COMM** =
community tools/issues only; **VERIFIED** = run on this machine; **UNKNOWN**.

## Headlines

1. **Claude Code**: the supported way to read 5h/7d utilisation from inside a
   session is the status line JSON (`rate_limits.five_hour|seven_day.{used_percentage,resets_at}`) — DOC.
   `GET https://api.anthropic.com/api/oauth/usage` works but is undocumented
   (COMM). Claude Code ≥ 2.1.234 waits in-session and auto-continues after a
   usage-limit reset ("Usage limit reached · continuing automatically at
   3:45pm · esc to cancel") — the daemon must not treat that pane as hung.
2. **Codex CLI**: `codex app-server` (stdio JSON-RPC) → `account/rateLimits/read`
   is DOC and VERIFIED here (codex-cli 0.159.2). Rollout JSONL carries
   `token_count` and `task_complete` events (VERIFIED). Pro plans have no 5h
   window (weekly only); the `prolite` plan on this machine shows a single
   `primary` bucket with `windowDurationMins = 10080`.
3. **Gemini CLI was replaced by Antigravity CLI (`agy`) for consumer accounts
   on 2026-06-18** (DOC). Gemini CLI no longer serves Google AI Pro/Ultra;
   it keeps working with API keys / Vertex / Code Assist Standard+Enterprise
   (requests-per-day quotas). The subscription CLI to integrate is `agy`
   (5-hour windows under a weekly cap). Nothing of it could be verified here
   (`agy` is not installed).

## Claude Code

### Reading remaining usage
- **Status line** (DOC https://code.claude.com/docs/en/statusline.md#rate-limit-usage):
  the `statusLine` command receives JSON on stdin including
  `rate_limits.five_hour` and `rate_limits.seven_day`, each
  `{used_percentage: 0..100, resets_at: <unix epoch seconds>}`. Present only
  for claude.ai Pro/Max and only after the first API response. Zero extra API
  calls. Can be configured in the per-task `settings.json`.
- **`/usage`** interactive only (DOC). Fails open to last-known bars when the
  usage endpoint is rate limited.
- **`GET https://api.anthropic.com/api/oauth/usage`** (COMM: wakamex/ccusage,
  steipete/CodexBar): headers `Authorization: Bearer <accessToken>`,
  `anthropic-beta: oauth-2025-04-20`; token needs `user:profile` scope.
  Response: `{"five_hour":{"utilization":35.0,"resets_at":"2026-02-06T22:00:00+00:00"},"seven_day":{...},"seven_day_sonnet":{...}|null,"seven_day_opus":null,"extra_usage":{...}}`.
  Unversioned, rate limited. Credentials: macOS Keychain service
  `Claude Code-credentials`, else `~/.claude/.credentials.json`
  (`{"claudeAiOauth":{"accessToken":"sk-ant-oat01-…","refreshToken":…,"expiresAt":…,"subscriptionType":"max","rateLimitTier":…}}`).
- Rate-limit headers `anthropic-ratelimit-unified-{5h,7d}-{utilization,reset,status}` exist (COMM) but are not exposed by the CLI.

### Error / throttle signatures (DOC https://code.claude.com/docs/en/errors.md)
```
You've hit your session limit · resets 3:45pm
You've hit your weekly limit · resets Mon 12:00am
You've hit your Opus limit · resets 3:45pm
You've hit your Sonnet limit · resets 3:45pm
You've used 85% of your session limit · resets 3:45pm
Usage limit reached · continuing automatically at 3:45pm · esc to cancel
Usage limit reset · continuing automatically
Your usage limit has reset · press enter to continue
API Error: Server is temporarily limiting requests (not your usage limit)
API Error: Request rejected (429) · this may be a temporary capacity issue.
```
`StopFailure` hook fires with `error` ∈ `rate_limit | overloaded |
authentication_failed | oauth_org_not_allowed | account_on_hold |
billing_error | invalid_request | model_not_found | server_error |
max_output_tokens | cloud_credential_error | unknown`, plus `error_details`
and `last_assistant_message`. Fallback-model chains never trigger on rate
limits.

### Launch flags / hooks (verified unchanged)
`--session-id`, `--resume`, `--model`, `--effort`, `--permission-mode
default|acceptEdits|plan|auto|dontAsk|bypassPermissions|manual`, `--settings`,
`--allowedTools`, `--fallback-model`. Hook events: `SessionStart`, `Setup`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`,
`StopFailure`, `SessionEnd`, `PreCompact/PostCompact`,
`PreModelSwitch/PostModelSwitch`, `WorktreeCreate/Remove`, `ConfigChange`.
Transcripts `~/.claude/projects/<encoded cwd>/<session-id>.jsonl`.

## OpenAI Codex CLI (docs: https://learn.chatgpt.com/docs/)

### Rate limits via app-server (DOC + VERIFIED)
```
codex -s read-only -a never app-server          # stdio, JSON-RPC 2.0, one message per line
→ {"method":"initialize","id":0,"params":{"clientInfo":{"name":"powerqueue","title":"powerqueue","version":"0.1.0"},"capabilities":{"experimentalApi":true}}}
→ {"method":"initialized","params":{}}
→ {"method":"account/rateLimits/read","id":1,"params":{"excludeResetCreditDetails":true}}
← {"id":1,"result":{"ordinaryUsageAllowed":true,"rateLimits":{"limitId":"codex","limitName":null,"normalModelSlug":null,
     "primary":{"usedPercent":17,"windowDurationMins":10080,"resetsAt":1791234430},"secondary":null,
     "credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"individualLimit":null,"spendControlReached":false,
     "planType":"prolite","rateLimitReachedType":null},
   "rateLimitsByLimitId":{"codex":{...}},"rateLimitResetCredits":{"availableCount":2,"credits":null},"accountId":"…","rateLimitUpsell":null}}
→ {"method":"account/read","id":2,"params":{}}
← {"id":2,"result":{"account":{"type":"chatgpt","email":"…","planType":"prolite"},"requiresOpenaiAuth":true,...}}
```
Semantics: `usedPercent` 0–100, `windowDurationMins` (300 = 5h, 10080 =
weekly), `resetsAt` unix seconds (nullable). `ordinaryUsageAllowed == false`
means blocked regardless of percentages. Plus/Business plans have both
`primary` (5h) and `secondary` (weekly); Pro-family plans have one weekly
bucket. Notification `account/rateLimits/updated` exists. The call is fast
and consumes no quota. The app-server also emits unsolicited notifications
(`remoteControl/status/changed`, `account/updated`) — ignore lines without
`id`.

Underlying REST (SRC): `GET https://chatgpt.com/backend-api/wham/usage` with the
bearer token from `$CODEX_HOME/auth.json`; prefer app-server (auth may be in
the keyring). Per-response headers `x-codex-primary-used-percent`,
`x-codex-primary-window-minutes`, `x-codex-primary-reset-at`,
`x-codex-secondary-*`, `x-codex-rate-limit-reached-type`.

### Rollouts (VERIFIED on 0.159.2)
Path `~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<thread-uuid>.jsonl`.
Lines `{"timestamp":"…","ordinal":N,"type":<RolloutItem>,"payload":{...}}`:
- `session_meta`: `payload.id` / `payload.session_id` (thread uuid), `cwd`, `runtime_workspace_roots`, `originator` (`codex-tui`), `cli_version`, `source`, `model_provider`, `base_instructions`.
- `event_msg` with `payload.type` ∈ `task_started` (`turn_id`, `started_at`), `token_count` (`info.total_token_usage` and `info.last_token_usage`, each `{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`, `rate_limits` snapshot or null), `task_complete` (`turn_id`, `last_agent_message`, `started_at`, `completed_at`), `item_completed`, `agent_message`, `user_message`, `error`, `thread_settings_applied`.
- `response_item`, `turn_context` (has `model`), `compacted`, `token_usage_record`.
The layout is not a stable contract (ongoing migration to a SQLite state db,
`codex migrate-rollouts`). Parse defensively.

### CLI (VERIFIED `codex --help`)
`codex [OPTIONS] [PROMPT]`; `-m/--model`; `-s/--sandbox read-only|workspace-write|danger-full-access`;
`-a/--ask-for-approval on-request|never`; `--approve-for-me`;
`--dangerously-bypass-approvals-and-sandbox` (alias `--yolo`);
`--dangerously-bypass-hook-trust`; `-C/--cd DIR`; `--add-dir DIR`;
`-c/--config key=value` (value parsed as TOML, e.g. `-c 'notify=["a","b"]'`);
`-p/--profile`; `--no-alt-screen`; `--no-daemon`. `codex resume [SESSION_ID]
[PROMPT] [--last]`. `codex queue` queues a message for a running session.
`codex login status`, `codex doctor`, `codex debug models` (JSON catalog).
**There is no `--session-id`**: the thread uuid is generated; find it from the
newest rollout whose `session_meta.cwd` equals the worktree.

Model catalog on 2026-10-01 (`codex debug models`, priority order):
`gpt-6.1-sol` (1), `gpt-6-astra` (2), `gpt-6-sol` (3), `gpt-6-luna` (4),
`gpt-5.6-sol` (5), `gpt-5.6-terra` (8), `gpt-5.6-luna` (9), `gpt-5.5` (13).
Reasoning efforts `low|medium|high|xhigh|max|ultra` via
`-c model_reasoning_effort=high`.

### Hooks and notify (DOC)
- `notify = ["cmd", "args"...]` in config (or `-c 'notify=[...]'`): fired after
  each agent turn with the JSON payload appended as the last argv:
  `{"type":"agent-turn-complete","thread-id":"<uuid>","turn-id":"…","cwd":"…","client":"codex-tui","input-messages":[…],"last-assistant-message":"…"}`.
  No trust prompt. This machine's global config already sets `notify` for
  the desktop app; a per-session `-c notify=[...]` override replaces it for
  that session only.
- `hooks.json` (`~/.codex/hooks.json`, `<repo>/.codex/hooks.json`): events
  `SessionStart`, `SessionEnd`, `PreToolUse`, `PostToolUse`, `Stop`
  (`last_assistant_message`), `UserPromptSubmit`, … stdin `{session_id,
  transcript_path, cwd, hook_event_name, model, permission_mode}`. Hooks
  require trust (hash recorded) unless `--dangerously-bypass-hook-trust`.
- Sandbox: `workspace-write` blocks writes outside the worktree (and network)
  — `powerqueue task complete` needs the data/state dirs writable
  (`--add-dir`), or completion must come from the `[[POWERQUEUE:DONE]]`
  marker in the last message.

### Error signatures (SRC)
HTTP body `{"error":{"type":"usage_limit_reached","plan_type":"plus","resets_at":<epoch>,"limit_window_minutes":…}}`.
Displayed (note the curly apostrophe):
```
You’ve hit your usage limit. Upgrade to Pro … or try again later.
You’ve hit your usage limit. Try again at <local time>.
You’ve hit your usage limit for {limit_name}. Switch to another model now, …
Your workspace is out of credits. …
rate limit exceeded: {msg}
Quota exceeded. Check your plan and billing details.
Selected model is at capacity. Please try a different model.
We’re currently experiencing high demand, which may cause temporary errors.
```
In rollouts: `event_msg` with `payload.type == "error"`.

## Google: Antigravity CLI (`agy`) — all COMM/DOC, nothing verified locally

- Install `curl -fsSL https://antigravity.google/cli/install.sh | bash`;
  auth via system keyring / Google sign-in (prints a URL over SSH).
- Quota: Pro/Ultra "quotas that refresh every five hours" under a weekly cap.
- `/usage` headless (COMM, stablyai/orca): `agy -p "/usage" --output-format json --print-timeout 20s`
  → `command.data.groups[]` (e.g. "Gemini Models") `.buckets[]` with ids
  `gemini-5h`, `gemini-weekly`, `3p-5h`, `3p-weekly`, fields
  `remaining_fraction`, `disabled`, window minutes; check `num_turns == 0`.
  Takes ~2.5 s, no quota used. Raw endpoint
  `POST https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota`
  is unreliable; prefer the command.
- Flags (DOC headless page + COMM): `agy [-i "<text>"]` opening prompt then
  interactive, `-p/--print`, `-c/--continue`, `--conversation <uuid>`,
  `--model <slug>` (`agy models`), `--effort low|medium|high`,
  `--mode accept-edits|plan`, `--sandbox`, `--dangerously-skip-permissions`,
  `--add-dir`, `--output-format text|json|stream-json`, `--print-timeout`.
  No `--session-id`; conversation id from
  `~/.gemini/antigravity-cli/cache/last_conversations.json` (workspace path →
  last conversation id).
- Storage: `~/.gemini/antigravity-cli/conversations/<uuid>.db`,
  `brain/<uuid>/.system_generated/logs/transcript.jsonl` (lines
  `{step_index, source, type: USER_INPUT|PLANNER_RESPONSE|RUN_COMMAND|…, status, created_at, content}`;
  no token usage), `history.jsonl`, `cli.log` (quota errors).
- Hooks (COMM): `~/.gemini/antigravity-cli/hooks.json` and
  `<repo>/.agents/hooks.json`; events `PreInvocation`, `PreToolUse`,
  `PostToolUse`, `PostInvocation`, `Stop`; stdin `{session_id,
  transcript_path, cwd, timestamp, hook_event_name}`.
- Errors: stderr `AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. … Resets in …","status":"RESOURCE_EXHAUSTED","error_code":429,"retryable":true}`;
  prose `Individual quota reached. Please upgrade your subscription to increase your limits. Resets in <duration>`.

## Gemini CLI (`gemini`) — API key / Vertex / Code Assist only
Quota = requests per user per day (+ per minute). Programmatic quota:
`POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota` →
`{"buckets":[{"remainingAmount","remainingFraction","resetTime","tokenType":"REQUESTS","modelId"}]}`.
Flags `gemini [query]`, `-p`, `-i`, `-m auto|pro|flash|<id>`,
`--approval-mode default|auto_edit|yolo|plan`, `-r/--resume`. Hooks in
`settings.json` (`SessionStart`, `AfterAgent`, `SessionEnd`, …). Not a
subscription provider any more; out of scope for now.

## Sources
Claude: code.claude.com/docs/en/{cli-reference,statusline,errors,costs,hooks,sessions,authentication,commands,interactive-mode}; github.com/wakamex/ccusage; github.com/steipete/CodexBar.
Codex: learn.chatgpt.com/docs/{app-server,config-file/config-reference,developer-commands,hooks,pricing}; github.com/openai/codex (`codex-rs/app-server-protocol`, `codex-rs/protocol/src/error.rs`, `codex-rs/rollout`, `codex-rs/hooks/src/legacy_notify.rs`); local runs of codex-cli 0.159.2.
Google: developers.googleblog.com (Gemini CLI → Antigravity CLI transition); geminicli.com/docs/resources/quota-and-pricing; antigravity.google/docs/cli/{headless,reference,commands/usage}; github.com/stablyai/orca PR 24073; github.com/google-antigravity/antigravity-cli issue 387.

## Appendix: verified on this machine (2026-10-01, tmux private server)

### Claude Code 2.1.287 status line
Per-task `settings.json` `{"statusLine":{"type":"command","command":"<script>"}}`
ran the script after the first response with this JSON on stdin (abridged):
```json
{"session_id":"936b3978-…","transcript_path":"/Users/edgar/.claude/projects/<encoded cwd>/936b3978-….jsonl","cwd":"…",
 "model":{"id":"claude-sonnet-5-5","display_name":"Sonnet 5.5"},"version":"2.1.287",
 "cost":{"total_cost_usd":0.0419,"total_duration_ms":37888,…},
 "context_window":{"total_input_tokens":34228,"total_output_tokens":4,"current_usage":{"input_tokens":2,"output_tokens":4,"cache_creation_input_tokens":8963,"cache_read_input_tokens":25263},…},
 "rate_limits":{"five_hour":{"used_percentage":75,"resets_at":1790914200},"seven_day":{"used_percentage":89,"resets_at":1790920800}}}
```
Keys present: `session_id, transcript_path, cwd, scratchpad_dir, prompt_id,
effort, session_name, model, workspace, version, output_style, cost,
context_window, exceeds_200k_tokens, prompt_cache, fast_mode, thinking,
rate_limits`. `resets_at` is unix seconds. The script's stdout is shown as
the status line, so it should print something short.

### Codex CLI 0.159.2 interactive launch
Command: `codex -C <dir> -m gpt-6-luna -a never -s read-only -c 'notify=["<script>"]' -c model_reasoning_effort=low '<prompt>'`.
- Showed the **folder trust dialog** first ("Trust this folder? … 1. Trust and
  continue 2. Quit"); the daemon must seed `projects."<worktree>".trust_level
  = "trusted"` (via `-c`) or the session stalls.
- `notify` fired twice with the payload as the single argv:
  1. `{"type":"agent-turn-complete","thread-id":"01a0faa4-a7c0-…","turn-id":"…","cwd":"<dir>","client":"codex-tui","input-messages":["<prompt>"],"last-assistant-message":"OK"}`
  2. a **title-generation side turn** on a *different* thread id whose
     `input-messages[0]` starts with "Generate a concise, single-line task
     title" and whose `last-assistant-message` is `{"title":"…"}`. Ignore
     payloads whose input message starts with that text; the side thread
     has no rollout file.
- Rollout `~/.codex/sessions/2026/10/01/rollout-2026-10-01T21-24-50-<thread-id>.jsonl`
  for the main thread (found by `session_meta.cwd == <dir>`), events:
  `task_started` (`turn_id`, `started_at`, `model_context_window`),
  `turn_context` (`cwd`, `approval_policy`, `sandbox_policy`, **`model`**),
  `token_count` with `info.last_token_usage` = `{"input_tokens":18699,"cached_input_tokens":12032,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":18704}`
  and a snake_case **`rate_limits` snapshot**: `{"limit_id":"codex","primary":{"used_percent":17.0,"window_minutes":10080,"resets_at":1791234430},"secondary":null,"credits":{…},"plan_type":"prolite","rate_limit_reached_type":null}`,
  `task_complete` (`last_agent_message`, `started_at`, `completed_at`, `duration_ms`).
  `input_tokens` includes the cached part (18699 total, 12032 cached).
