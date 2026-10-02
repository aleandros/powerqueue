//! Agent session runtime: the provider abstraction ([`agent`]) and its
//! implementations ([`claude`], [`codex`], [`gemini`]), prompt +
//! launch script generation, Claude Code hook settings, transcript tailing
//! for token usage, liveness probing and resource sampling.

pub mod agent;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod hooks;
pub mod launcher;
pub mod monitor;
pub mod transcript;
pub mod trust;

pub use agent::{AgentCli, AgentLaunch, AuthStatus, ClaudeCli, CodexCli, GeminiCli, LaunchContext, agent_for};
pub use hooks::{HookOutcome, interpret_hook};
pub use launcher::{LaunchPlan, Launcher, build_prompt, hook_settings};
pub use monitor::{SessionProbe, probe_session, sample_resources};
pub use transcript::{TranscriptReader, TranscriptState, transcript_path_for};
