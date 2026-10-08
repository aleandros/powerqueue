//! Agent session runtime: the provider abstraction ([`agent`]) and its
//! implementations ([`claude`], [`codex`], [`gemini`]), the `binary`
//! command template ([`binary`]), prompt + launch script generation, Claude
//! Code hook settings, the container shim and inbox ([`inbox`]), transcript
//! tailing for token usage, liveness probing and resource sampling.

pub mod agent;
pub mod binary;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod hooks;
pub mod inbox;
pub mod launcher;
pub mod monitor;
pub mod transcript;
pub mod trust;

pub use agent::{AgentCli, AgentLaunch, AuthStatus, ClaudeCli, CodexCli, GeminiCli, LaunchContext, agent_for};
pub use binary::{BINARY_PLACEHOLDERS, BinaryTemplate, BinaryVars, host_command, program_of, which_program};
pub use hooks::{HookOutcome, interpret_hook};
pub use launcher::{
    LaunchPlan, Launcher, PROMPT_PLACEHOLDERS, PromptContext, RenderedPrompt, build_prompt, hook_settings, prompt_variables,
    render_prompt, render_template,
};
pub use monitor::{SessionProbe, probe_session, sample_resources};
pub use transcript::{TranscriptReader, TranscriptState, transcript_path_for};
