//! Claude Code session runtime: prompt + launch script generation, hook
//! settings, transcript tailing for token usage, liveness probing and
//! resource sampling.

pub mod hooks;
pub mod launcher;
pub mod monitor;
pub mod transcript;
pub mod trust;

pub use hooks::{HookOutcome, interpret_hook};
pub use launcher::{LaunchPlan, Launcher, build_prompt, hook_settings};
pub use monitor::{SessionProbe, probe_session, sample_resources};
pub use transcript::{TranscriptReader, transcript_path_for};
