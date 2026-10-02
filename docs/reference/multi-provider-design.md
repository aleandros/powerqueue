# Multi-provider budgets: design and work split

Status: implementation in progress (2026-10-01). Companion:
[providers-research.md](providers-research.md) for the verified CLI facts.

## Goal

Run tasks on more than one coding agent, each with its own subscription
budget: Claude Code (`claude`), OpenAI Codex CLI (`codex`) and Google's
Antigravity CLI (`gemini`, binary `agy`, the CLI that serves Google AI
Pro/Ultra since June 2026). A task is scheduled on the best model any
enabled provider can afford right now, honouring per-criticality preference
lists (`critical: fable | gpt-6.1-sol`) and a provider fallback order. Where a
provider can report its remaining allowance, the daemon reads it instead of
only estimating from transcripts.

## Decisions

### Model identity: string-backed, provider inferred

`ModelTier` stays the type name everywhere but becomes a string newtype
(`Clone`, not `Copy`): the canonical alias the CLI accepts (`fable`, `opus`,
`sonnet`, `haiku`, `gpt-6.1-sol`, `gpt-6-astra`, `gemini-3-pro`, ...).
Reasons: model catalogs churn monthly (Codex went from `gpt-5-codex` to the
GPT-6 family in weeks); users must be able to add a model in config without
a release. Serde stays a plain string, so existing SQLite rows, `commands`
JSON and `RateLimitState` keys keep deserialising.

- `Provider { Claude, Codex, Gemini }` with `as_str`, `Display`, `FromStr`
  (accepts `antigravity` and `agy` as aliases of `gemini`), `ALL`.
- `ModelTier::new(&str)` lowercases and trims; `ModelTier::provider()` is
  inferred by name: `fable|mythos|opus|sonnet|haiku|claude-*` → Claude;
  `gpt-*|o1*|o3*|o4*|codex*` → Codex; `gemini-*` → Gemini; otherwise an
  explicit `provider:name` form (`codex:custom-slug`) sets it, and bare
  unknown names are rejected by `FromStr` with the alias rules in the error.
- `ModelTier::fable()/opus()/sonnet()/haiku()` constructors replace the old
  variants. `from_model_id(id)` maps Claude ids as before and passes other
  ids through; `from_model_id_for(provider, id)` falls back to the
  provider's default model for unknown ids.
- Capability order is configuration, not code: `ModelBudget.rank` (lower is
  more capable; defaults 10/20/30/40 for the shipped models, 50 for user
  additions). `downgrade()` becomes `BudgetConfig::downgrade(&model)` (next
  higher rank within the same provider) and `downgrade_chain(&model)`.
- `default_weight()` moves to the shipped `ModelBudget.weight` defaults.

### Config

```toml
[budget]                         # shared knobs
default_model = "sonnet"
low_model = "sonnet"
safety_margin = 0.05
endgame_fraction = 0.8
provider_order = ["claude", "codex", "gemini"]
probe_interval_mins = 15         # how often usage probes run

[budget.providers.claude]
enabled = true
period_hours = 168
period_anchor = "2026-10-05T14:00:00Z"   # optional; probes can learn it
window_hours = 5                 # 0 disables the window
period_weighted_tokens = 80000000
window_weighted_tokens = 12000000
rate_limit_cooldown_mins = 30
[budget.providers.claude.models.fable]
rank = 10
share = 0.25
min_criticality = "critical"
relax_after_fraction = 0.5
weight = 5.0
enabled = true
# opus (rank 20), sonnet (30), haiku (40) as before

[budget.providers.codex]
enabled = false                  # init asks; doctor checks `codex login status`
period_hours = 168
window_hours = 5                 # Plus/Business; Pro-family plans have no 5h window: probes set window used = 0
period_weighted_tokens = 60000000
window_weighted_tokens = 8000000
rate_limit_cooldown_mins = 30
[budget.providers.codex.models."gpt-6.1-sol"]   # rank 10, share 0.4, critical, weight 2.0
[budget.providers.codex.models."gpt-6-astra"]   # rank 20, share 0.4, high,     weight 1.5
[budget.providers.codex.models."gpt-6-luna"]    # rank 30, share 0.2, low,      weight 0.5

[budget.providers.gemini]
enabled = false
period_hours = 168
window_hours = 5
period_weighted_tokens = 40000000
window_weighted_tokens = 6000000
rate_limit_cooldown_mins = 30
[budget.providers.gemini.models."gemini-3-pro"]    # rank 10, share 0.7, high, weight 1.0
[budget.providers.gemini.models."gemini-3-flash"]  # rank 20, share 0.3, low,  weight 0.2

[claude]      # unchanged
[codex]
binary = "codex"
approval = "workspace-write"     # workspace-write | approve-for-me | yolo | on-request
reasoning_effort = "high"        # optional → -c model_reasoning_effort=…
extra_args = []
env = {}
trust_workspace = true           # -c 'projects."<worktree>".trust_level="trusted"'
[gemini]
binary = "agy"
mode = "skip-permissions"        # skip-permissions | accept-edits | plan
effort = "high"                  # optional → --effort
extra_args = []
env = {}
```

Backward compatibility: a config with the old flat `[budget]` keys and
`[budget.models.<tier>]` loads unchanged — `Config::from_toml` moves those
keys under `budget.providers.claude` (pure `migrate_legacy_budget(&mut
toml::Table) -> Vec<String>`), `validate` reports each moved key as a
deprecation note, and conflicting old+new values are an error. `config set
budget.models.fable.share 0.3` keeps working through the same migration.
`.powerqueue.toml` repo overrides gain `[codex]` and `[gemini]` tables with
the same subset as `[claude]`.

### Budget engine

- `PeriodClock::from_provider(&ProviderBudget, now)`; `window_hours = 0`
  means "no window" (`window_fraction()` is 0, window checks pass).
- `Ledger` gains `provider`; `Ledgers { by_provider: BTreeMap<Provider,
  Ledger> }` with `load(store, cfg, now)` for enabled providers, `get`,
  `get_mut`, `for_model`, `ordered(&provider_order)`, `earliest_period_end`.
- Calibration per provider in kv `budget.calibration.<provider>` (store
  migration v2 renames the legacy key to `budget.calibration.claude`).
  `RateLimitState` keys are already model aliases, so `budget.rate_limits`
  stays; add `mark_provider`/`clear_provider`.
- **Observed usage** (new `budget/probe.rs`): `ObservedUsage { window_used:
  Option<f64>, window_resets_at: Option<DateTime<Utc>>, period_used:
  Option<f64>, period_resets_at: Option<DateTime<Utc>>, blocked: bool,
  observed_at }`, stored in kv `budget.observed.<provider>`. The ledger uses
  it two ways: `period_used` becomes the calibration (same mechanism as
  `budget set-observed`), and `period_resets_at` overrides the configured
  anchor for that period (so users never have to type it). `blocked` or
  `window_used >= 1` puts the provider on cooldown until the matching
  `resets_at`.
- Probes (`trait UsageProbe { fn provider(&self) -> Provider; fn probe(&self)
  -> Result<Option<ObservedUsage>> }`):
  - Codex: spawn `<binary> -s read-only -a never app-server`, send
    `initialize`/`initialized`/`account/rateLimits/read` (see research), read
    until the response with `id` 1, kill the child. `windowDurationMins <=
    600` → window bucket, else period bucket; `usedPercent/100`; `resetsAt`
    epoch seconds; `ordinaryUsageAllowed == false` → `blocked`.
  - Claude: no network call. The per-task `settings.json` gets a `statusLine`
    command `powerqueue hook --task <id> --session <sid> --event StatusLine`;
    `powerqueue hook` stores the `rate_limits` object from stdin in kv
    `budget.observed.claude` (latest wins, not a `hook_events` row). The
    probe just reads that key.
  - Gemini: run `<binary> -p "/usage" --output-format json --print-timeout
    20s`, parse `command.data.groups[].buckets[]` with ids `gemini-5h` /
    `gemini-weekly` (`remaining_fraction`); unknown shape → `Ok(None)` with
    a debug log, never an error that stops the daemon.
  The daemon runs each enabled provider's probe every
  `budget.probe_interval_mins` (and once at start); `budget show` prints the
  observed values and their age; `doctor` warns when a probe keeps failing.
- `Policy::decide(task, prediction, preferred: &[ModelTier]) -> Decision`:
  candidates = enabled providers in `provider_order` × their enabled models
  by rank; preference list from the hard override (`task.model_override`,
  never crosses providers), else the rules (`## Models` lists), else
  criticality defaults; each preferred model is tried through its downgrade
  chain; if nothing in the list is eligible and it was not a hard override,
  the first eligible candidate in provider order wins. `retry_at` is the
  earliest hint across providers.

### Session layer

`src/session/agent.rs` defines the provider abstraction; `claude.rs`,
`codex.rs`, `gemini.rs` implement it. Hook payloads are normalised to the
Claude shape at the `powerqueue hook --provider <p>` boundary so
`hooks.rs::interpret_hook`, `transitions.rs` and `hook.rs::apply_markers`
stay untouched.

```rust
pub struct LaunchContext<'a> { cfg, task, session_id: Uuid, model: &'a ModelTier, attempt, resume: Option<&'a str> /* provider session id */, task_dir, prompt_path, worktree, self_bin }
pub struct AgentLaunch { files: Vec<(PathBuf, String, u32)>, env: Vec<(String, String)>, argv: Vec<String>, transcript_path: Option<PathBuf>, poll_transcript_for_completion: bool }
pub trait AgentCli: Send + Sync {
    fn provider(&self) -> Provider;
    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch>;
    fn pre_launch(&self, cfg: &Config, repo: &Path, worktree: &Path) -> Result<()>;
    fn discover_session(&self, cfg: &Config, worktree: &Path, started_after: DateTime<Utc>) -> Result<Option<(String, PathBuf)>>; // provider session id + transcript path when the CLI generates its own id
    fn parse_transcript_line(&self, line: &str, session_id: Uuid, task_id: TaskId, launched_model: &ModelTier) -> Option<UsageRecord>;
    fn normalize_hook(&self, event: &str, payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)>;
    fn rate_limit_signatures(&self) -> &'static [&'static str];
    fn auth_status(&self, binary: &str) -> Result<AuthStatus>;
    fn allowed_modes(&self) -> &'static [&'static str];
    fn probe(&self, cfg: &Config, store: &Store) -> Result<Option<ObservedUsage>>;
}
pub fn agent_for(p: Provider) -> &'static dyn AgentCli;
```

- `sessions` table gains `agent_session_id TEXT` (nullable; schema v2) for
  CLIs that generate their own ids (Codex thread uuid, agy conversation id).
  `discover_session` runs every tick for live sessions without one.
- **Claude**: existing behaviour moved into `claude.rs`, plus the
  `statusLine` entry in `settings.json`. Treat the pane text `Usage limit
  reached · continuing automatically` as *waiting* (no stale/crash) until
  the observed reset.
- **Codex**: argv `codex -C <worktree> -m <model> <approval flags> -c
  'notify=["<self_bin>","hook","--provider","codex","--task","<id>","--session","<sid>","--event","Notify"]'
  [-c model_reasoning_effort=<e>] [-c 'projects."<worktree>".trust_level="trusted"']
  --add-dir <data_dir> --add-dir <state_dir> <extra_args> "<prompt>"`;
  resume = `codex resume <agent_session_id> "<attempt prompt>"`. Approval
  map: `yolo` → `--dangerously-bypass-approvals-and-sandbox`;
  `workspace-write` → `-a never -s workspace-write`; `approve-for-me` →
  `--approve-for-me`; `on-request` → `-a on-request -s workspace-write`.
  `discover_session`: newest `~/.codex/sessions/*/*/*/rollout-*.jsonl`
  (honour `$CODEX_HOME`) whose `session_meta.cwd` equals the worktree and
  whose timestamp is after launch. Usage: `event_msg/token_count` →
  `info.last_token_usage` (`input_tokens` − `cached_input_tokens` as input,
  `cached_input_tokens` as cache read, `cache_write_input_tokens` as cache
  write, `output_tokens` as output), message id `<thread>-<ordinal>`; model
  from the latest `turn_context.model`, else the launched model. Hook
  `Notify` with `type == agent-turn-complete` → `Stop` with
  `last_assistant_message = last-assistant-message`; `event_msg/error` whose
  message matches a rate-limit signature → synthesised `StopFailure` with
  `error_type = rate_limit`. Signatures: `hit your usage limit`, `usage limit
  reached`, `rate limit exceeded`, `Quota exceeded`, `out of credits`.
  `auth_status`: `codex login status` exit code + output.
- **Gemini (agy)**: argv `agy -i "<prompt>" --model <slug> <mode flag>
  [--effort <e>] --add-dir <data_dir> --add-dir <state_dir> <extra_args>` run
  with cwd = worktree; resume `agy --conversation <id> -i "<attempt
  prompt>"`. Completion: write `<worktree>/.agents/hooks.json` with a `Stop`
  hook calling `powerqueue hook --provider gemini … --event Stop` (and add
  `.agents/` to the repo's `.git/info/exclude` once, in `pre_launch`); the
  hook reads the last `PLANNER_RESPONSE` from `transcript_path` into
  `last_assistant_message`. Set `poll_transcript_for_completion = true` as
  a belt-and-braces path: the daemon scans the transcript for the DONE /
  BLOCKED markers. Usage records: agy transcripts carry no token counts, so
  record nothing and rely on the probe; the estimator treats a provider
  with no samples as "unknown cost" (use the configured average, never
  block). `discover_session`: `~/.gemini/antigravity-cli/cache/last_conversations.json[<worktree>]`.
  Everything here is from community reports; the provider is documented as
  experimental and `doctor` says so.
- The daemon's `launch.sh`, `prompt.md`, `env`, tmux window, CPU/RSS
  sampling, idle/stale/crash handling and cleanup are provider-neutral and
  unchanged. The prompt mentions the completion command and marker only;
  it does not name the agent.

### UX

- `budget show` groups by provider: period/window/observed usage age,
  anchor source (config / observed / default), rate-limit cooldowns, then the
  model table; one "what would run now" table at the end. JSON:
  `{"providers": {"claude": {...}}, "next": {...}}`.
- `budget set-reset|set-observed|clear-limits [--provider <p>]`
  (default `claude`); `budget probe [--provider <p>]` runs the probes now and
  prints what came back.
- Dashboard: one gauge block per enabled provider (header line with
  period/window and cooldown), compact per-provider summary in the header,
  `next <model>` hint.
- `task model <task> <model>` / `add --model` accept any provider's model
  (warn when its provider is disabled). PRIORITY.md `## Models` accepts
  alternatives: `critical: fable | gpt-6.1-sol`; overrides accept
  `KEY: model = fable | gpt-6.1-sol`.
- `init` asks "Also run tasks on Codex (needs `codex login`)?" and "...on
  Google Antigravity (`agy`)?" when the binaries are on PATH, enabling the
  provider and probing once to seed the ledger. `init --reconfigure` covers
  the same.
- `doctor`: per enabled provider — binary present and version, logged in,
  shares ≤ 1, at least one enabled model, probe freshness, anchor known;
  the `gemini` provider gets an "experimental" note.

## Work split

1. **Foundation** (lands first): `domain.rs`, `config.rs`, `store`
   migration v2, `budget/period.rs`, `budget/ledger.rs` (`Ledgers`,
   calibration keys), `budget/probe.rs` (types + `UsageProbe` + kv helpers,
   no real probes), `session/agent.rs` (trait, `agent_for`, `ClaudeCli`
   wrapping today's code, `CodexCli`/`GeminiCli` stubs that `bail!`),
   `cli/mod.rs` (`Provider` as `ValueEnum`, `--provider` flags, help text)
   and every mechanical fix so the tree builds and tests pass with
   Claude-only behaviour.
2. **Budget engine + scheduler**: `budget/policy.rs`, probe scheduling in
   `scheduler/daemon.rs`, per-provider cooldowns in `transitions.rs`, the
   Codex and Claude probes, `hook.rs` `--provider` + `StatusLine` handling,
   `budget` CLI changes.
3. **Session / provider CLIs**: `session/{claude,codex,gemini}.rs`,
   `launcher.rs`, `transcript.rs`, `discover_session` in the daemon tick,
   fake fixtures `tests/fixtures/fake-codex.sh` and `fake-agy.sh`, e2e tests.
4. **UX**: dashboard, `budget show`, `doctor`, `priority/rules.rs` lists,
   `init` provider questions, README/docs/site.

Branches 2–4 start from the foundation commit and code against the
signatures above; conflicts are expected only in `cli/mod.rs` and README.
