//! One bounded routing prompt, separate from implementation sessions.
use anyhow::{Context, Result, bail};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Run Claude Code with no tools, repository settings, hooks or session persistence.
/// Fails on timeout, oversized output, process errors or malformed output; never exposes stderr.
pub async fn analyze(
    cfg: &crate::analysis::ClaudeCodeConfig,
    input: &crate::analysis::AnalysisInput,
    timeout_secs: u64,
) -> Result<serde_json::Value> {
    let dir = tempfile::tempdir().context("create isolated routing directory")?;
    let mut command = tokio::process::Command::from(super::host_command(&cfg.binary)?);
    command.env_remove("CLAUDECODE").current_dir(dir.path()).args([
        "-p", "--safe-mode", "--output-format", "json", "--model", &cfg.model,
        "--tools", "", "--disallowedTools", "mcp__*", "--strict-mcp-config", "--mcp-config", "{\"mcpServers\":{}}",
        "--setting-sources", "", "--settings", "{\"disableAllHooks\":true}", "--disable-slash-commands",
        "--no-session-persistence", "--max-turns", "1", "--permission-mode", "dontAsk",
        "--system-prompt", &format!("{} Return only JSON with exactly: project (registered ID or null), classification (bug|feature|question|maintenance|unknown), confidence (0..1), reasoning (string), plan (array of at most 10 short steps). Include a preliminary plan. Return null for project if uncertain.", crate::analysis::client::INSTRUCTIONS),
    ]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start Claude Code repository analysis")?;
    #[cfg(unix)]
    let group = ProcessGroup(child.id().and_then(|id| i32::try_from(id).ok()).and_then(rustix::process::Pid::from_raw));
    let mut stdin = child.stdin.take().context("open Claude Code routing input")?;
    let stdout = child.stdout.take().context("open Claude Code routing output")?;
    let payload = serde_json::to_vec(input)?;
    let request = async {
        let write = async {
            stdin.write_all(&payload).await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let read = async {
            let mut output = Vec::new();
            stdout.take(128 * 1024 + 1).read_to_end(&mut output).await?;
            Ok::<_, std::io::Error>(output)
        };
        let ((), output) = tokio::try_join!(write, read).context("read/write Claude Code routing request")?;
        if output.len() > 128 * 1024 {
            bail!("Claude Code routing response exceeds 128 KiB");
        }
        if !child.wait().await.context("wait for Claude Code routing request")?.success() {
            bail!("Claude Code routing request failed; check its installation and login");
        }
        let raw: serde_json::Value = serde_json::from_slice(&output).context("decode Claude Code routing response")?;
        if raw["is_error"].as_bool() == Some(true) {
            bail!("Claude Code routing request did not complete");
        }
        Ok(raw)
    };
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(timeout_secs), request).await {
        result
    } else {
        #[cfg(unix)]
        drop(group);
        let _ = child.kill().await;
        bail!("Claude Code repository analysis timed out");
    }
}

// Drop also runs when the worker is cancelled or response validation fails.
// Wrappers share this owned group; none may outlive a completed request.
#[cfg(unix)]
struct ProcessGroup(Option<rustix::process::Pid>);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}
