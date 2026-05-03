//! Bash tool execution with timeout, output truncation, and proper child
//! cleanup. The agent never invokes this directly — the code writer loop
//! emits a `CommandProposal`, waits for the user to approve, and only then
//! calls into here.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct BashOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

pub fn resolve_timeout(requested: Option<u64>) -> Duration {
    let secs = requested
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Run `command` via `/bin/sh -lc` (or `cmd /C` on Windows) inside
/// `workspace_root`. Captures stdout/stderr, enforces a timeout, kills the
/// process on timeout, and truncates oversized output. Errors during spawn
/// are surfaced as a non-zero outcome rather than a panic.
pub async fn run(
    workspace_root: Option<&Path>,
    command: &str,
    timeout: Duration,
) -> BashOutcome {
    let child = match spawn_shell(workspace_root, command) {
        Ok(c) => c,
        Err(err) => {
            return BashOutcome {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to spawn shell: {err}"),
                truncated: false,
                timed_out: false,
            };
        }
    };
    run_with_timeout(child, timeout).await
}

#[cfg(not(target_os = "windows"))]
fn spawn_shell(workspace_root: Option<&Path>, command: &str) -> std::io::Result<Child> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-lc").arg(command);
    if let Some(root) = workspace_root {
        cmd.current_dir(root);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
}

#[cfg(target_os = "windows")]
fn spawn_shell(workspace_root: Option<&Path>, command: &str) -> std::io::Result<Child> {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    if let Some(root) = workspace_root {
        cmd.current_dir(root);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
}

async fn run_with_timeout(mut child: Child, timeout: Duration) -> BashOutcome {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = stdout.map(|mut h| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = h.read_to_end(&mut buf).await;
            buf
        })
    });
    let stderr_task = stderr.map(|mut h| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = h.read_to_end(&mut buf).await;
            buf
        })
    });

    let timed_out;
    let exit_code;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            timed_out = false;
            exit_code = status.code();
        }
        Ok(Err(err)) => {
            timed_out = false;
            return BashOutcome {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("wait failed: {err}"),
                truncated: false,
                timed_out,
            };
        }
        Err(_) => {
            let _ = child.kill().await;
            timed_out = true;
            exit_code = None;
        }
    }

    let stdout_bytes = match stdout_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };
    let stderr_bytes = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };

    let (stdout_str, stdout_t) = clip(stdout_bytes);
    let (stderr_str, stderr_t) = clip(stderr_bytes);

    let mut stderr = stderr_str;
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "[timed out after {}s; child killed]",
            timeout.as_secs()
        ));
    }

    BashOutcome {
        exit_code,
        stdout: stdout_str,
        stderr,
        truncated: stdout_t || stderr_t,
        timed_out,
    }
}

fn clip(bytes: Vec<u8>) -> (String, bool) {
    let total = bytes.len();
    if total <= MAX_OUTPUT_BYTES {
        return (String::from_utf8_lossy(&bytes).into_owned(), false);
    }
    let truncated = &bytes[..MAX_OUTPUT_BYTES];
    let mut s = String::from_utf8_lossy(truncated).into_owned();
    s.push_str(&format!(
        "\n… [truncated; {total} bytes total, only first {MAX_OUTPUT_BYTES} shown]"
    ));
    (s, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_captures_stdout_and_exit_zero() {
        let outcome = run(None, "echo hello", Duration::from_secs(5)).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout.contains("hello"));
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn run_captures_stderr_and_nonzero_exit() {
        let outcome = run(
            None,
            "echo bad >&2; exit 7",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome.exit_code, Some(7));
        assert!(outcome.stderr.contains("bad"));
    }

    #[tokio::test]
    async fn run_kills_child_on_timeout() {
        let outcome = run(None, "sleep 5", Duration::from_millis(200)).await;
        assert!(outcome.timed_out, "expected timeout flag");
        assert!(outcome.stderr.contains("timed out"));
    }

    #[test]
    fn resolve_timeout_caps_at_max() {
        assert_eq!(
            resolve_timeout(Some(MAX_TIMEOUT_SECS * 10)),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        assert_eq!(
            resolve_timeout(None),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
        assert_eq!(resolve_timeout(Some(30)), Duration::from_secs(30));
    }
}
