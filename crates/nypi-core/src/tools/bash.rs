//! `bash`: run a shell command in the working directory with merged stdout/stderr, tail
//! truncation, a full-output spill file, optional timeout, and process-group kill on abort.
//! `run_process` is shared with command tools.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_tail};

pub struct BashTool {
    pub cwd: PathBuf,
    /// The command runs under this (decision D62).
    pub sandbox: crate::sandbox::Sandbox,
}

/// A year: beyond this a timeout is not a limit, and `Duration` cannot hold what a model may send.
const MAX_TIMEOUT_SECONDS: f64 = 365.0 * 86_400.0;

/// How the command ended.
pub(crate) enum Outcome {
    Exited(Option<i32>),
    TimedOut(u64),
    Aborted,
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: format!(
                "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.{}",
                DEFAULT_MAX_BYTES / 1024,
                self.sandbox.note()
            ),
            parameters: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let command = args["command"].as_str().unwrap_or_default().to_string();
        // A timeout the clock cannot hold (the model can send any number) is no timeout.
        let timeout = args["timeout"].as_f64().filter(|t| *t > 0.0 && *t <= MAX_TIMEOUT_SECONDS);
        let mut cmd = self.sandbox.command("bash");
        cmd.arg("-c").arg(&command).current_dir(&self.cwd);
        let result = run_process(cmd, None, timeout, &ctx).await?;
        let mut output = ToolOutput::text(result.text).with_details(json!({
            "command": command,
            "exitCode": result.exit_code,
            "fullOutputPath": result.full_output_path,
        }));
        output.is_error = result.is_error;
        Ok(output)
    }
}

/// What a finished process produced, formatted for the model.
pub(crate) struct ProcessResult {
    pub text: String,
    pub is_error: bool,
    pub exit_code: Option<i32>,
    pub full_output_path: Option<PathBuf>,
}

/// What a finished process produced, before formatting (Pi's `BashResult`).
pub(crate) struct Captured {
    /// Merged stdout and stderr, the tail kept when it is long.
    pub output: String,
    pub outcome: Outcome,
    pub truncated: bool,
    /// The whole output, when `output` is only its tail.
    pub full_output_path: Option<PathBuf>,
    /// Lines in the whole output, and how many `output` keeps.
    total_lines: usize,
    kept_lines: usize,
}

impl Captured {
    /// The process's exit code; None when it was killed or cannot tell.
    pub fn exit_code(&self) -> Option<i32> {
        match self.outcome {
            Outcome::Exited(code) => code,
            _ => None,
        }
    }
}

/// Run a prepared command with stdout and stderr merged, tail truncation with a full-output
/// spill file, an optional timeout, and a process-group kill on timeout or `cancel`. `stdin`
/// is written and closed; without it stdin is empty. `progress` sees the output so far, twice
/// a second. With `clean`, the output is cleaned first (`clean_output`), spill file included.
pub(crate) async fn capture(
    mut cmd: std::process::Command,
    stdin: Option<String>,
    timeout: Option<f64>,
    cancel: &tokio_util::sync::CancellationToken,
    clean: bool,
    mut progress: impl FnMut(&str),
) -> anyhow::Result<Captured> {
    let (reader, writer) = std::io::pipe().context("Cannot create output pipe")?;
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(writer.try_clone()?)
        .stderr(writer)
        // Own process group, so abort and timeout can kill the whole tree.
        .process_group(0);
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Cannot start {program}"))?;
    let pid = child.id();
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = pipe.write_all(input.as_bytes()).await;
        });
    }

    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader_task = {
        let buffer = buffer.clone();
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buffer.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        })
    };

    let deadline = async {
        match timeout {
            Some(seconds) => tokio::time::sleep(Duration::from_secs_f64(seconds)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.tick().await;
    let outcome = loop {
        tokio::select! {
            status = child.wait() => break Outcome::Exited(status.ok().and_then(|s| s.code())),
            _ = &mut deadline => break Outcome::TimedOut(timeout.unwrap_or_default() as u64),
            _ = cancel.cancelled() => break Outcome::Aborted,
            _ = ticker.tick() => progress(&String::from_utf8_lossy(&buffer.lock().unwrap())),
        }
    };
    if !matches!(outcome, Outcome::Exited(_)) {
        kill_group(pid);
        let _ = child.wait().await;
    }
    // Background processes may keep the pipe open; do not wait on them forever.
    let _ = tokio::time::timeout(Duration::from_millis(200), reader_task).await;

    let mut raw = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
    if clean {
        raw = clean_output(&raw);
    }
    let full = raw.trim_end_matches('\n');
    let truncation = truncate_tail(full, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut full_output_path = None;
    if truncation.truncated {
        let file = tempfile::Builder::new().prefix("nypi-bash-").suffix(".log").tempfile()?;
        let (_, path) = file.keep()?;
        std::fs::write(&path, full)?;
        full_output_path = Some(path);
    }
    Ok(Captured {
        output: truncation.text,
        outcome,
        truncated: truncation.truncated,
        full_output_path,
        total_lines: truncation.total_lines,
        kept_lines: truncation.kept_lines,
    })
}

/// Run a prepared command (see `capture`), partial output streamed to `ctx`, and format what it
/// produced for the model.
pub(crate) async fn run_process(
    cmd: std::process::Command,
    stdin: Option<String>,
    timeout: Option<f64>,
    ctx: &ToolContext,
) -> anyhow::Result<ProcessResult> {
    let progress =
        |text: &str| ctx.update(ToolOutput::text(truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).text));
    let captured = capture(cmd, stdin, timeout, &ctx.cancel, false, progress).await?;
    let mut text = if captured.output.is_empty() { "(no output)".to_string() } else { captured.output.clone() };
    if let Some(path) = &captured.full_output_path {
        let first = captured.total_lines - captured.kept_lines + 1;
        text.push_str(&format!(
            "\n\n[Showing lines {first}-{} of {} ({} limit). Full output: {}]",
            captured.total_lines,
            captured.total_lines,
            format_size(DEFAULT_MAX_BYTES),
            path.display()
        ));
    }

    let (is_error, exit_code) = match captured.outcome {
        Outcome::Exited(Some(0)) => (false, Some(0)),
        Outcome::Exited(code) => {
            text.push_str(&format!(
                "\n\nCommand exited with code {}",
                code.map_or("unknown".into(), |c| c.to_string())
            ));
            (true, code)
        }
        Outcome::TimedOut(seconds) => {
            text.push_str(&format!("\n\nCommand timed out after {seconds} seconds"));
            (true, None)
        }
        Outcome::Aborted => {
            text.push_str("\n\nCommand aborted");
            (true, None)
        }
    };
    Ok(ProcessResult { text, is_error, exit_code, full_output_path: captured.full_output_path })
}

/// Output as Pi stores the user's own commands (`bash-executor.ts`): terminal escape sequences
/// stripped, binary control characters dropped, and carriage returns removed.
pub(crate) fn clean_output(text: &str) -> String {
    static ESCAPES: OnceLock<regex::Regex> = OnceLock::new();
    // Pi's `ansiRegex` (from ansi-regex): OSC sequences up to their terminator, and CSI and
    // related sequences with their parameters and final byte.
    let escapes = ESCAPES.get_or_init(|| {
        let st = r"(?:\x07|\x1B\\|\x{9C})";
        let csi = r"(?:(?:\x1B\[|\x{9B})[<=>]|[\x1B\x{9B}][\[\]()#;?]*)(?:\d{1,4}(?:[;:]\d{0,4})*)?";
        let fin = r"[\dA-PR-TZcf-nq-uy=><~]";
        regex::Regex::new(&format!(r"(?:\x1B\][\s\S]*?{st})|{csi}{fin}")).expect("escape pattern compiles")
    });
    let text = if text.contains(['\x1B', '\u{9B}']) { escapes.replace_all(text, "") } else { text.into() };
    text.chars()
        .filter(|c| !matches!(c, '\x00'..='\x08' | '\x0B' | '\x0C' | '\x0D'..='\x1F' | '\u{FFF9}'..='\u{FFFB}'))
        .collect()
}

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: plain syscall; a negative pid targets the process group created by process_group(0).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_command_output_is_cleaned_as_pi_cleans_it() {
        let raw = "\x1B[1;31merror\x1B[0m: bad\r\n\x1B]0;title\x07done\x01\x02\tok\n\u{FFFA}end";
        assert_eq!(clean_output(raw), "error: bad\ndone\tok\nend");
        assert_eq!(clean_output("plain text\n"), "plain text\n");
    }
}
