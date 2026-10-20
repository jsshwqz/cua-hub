//! Shell command execution with a hard timeout.

use std::time::Duration;

use cua_core::{Result, ShellKind};
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 4000;

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }

    /// Compact form fed back to the model.
    pub fn to_note(&self) -> String {
        let mut s = String::new();
        if self.timed_out {
            s.push_str("timed out after 120s\n");
        } else {
            s.push_str(&format!("exit code: {:?}\n", self.code));
        }
        if !self.stdout.trim().is_empty() {
            s.push_str("stdout:\n");
            s.push_str(&truncate(&self.stdout));
            s.push('\n');
        }
        if !self.stderr.trim().is_empty() {
            s.push_str("stderr:\n");
            s.push_str(&truncate(&self.stderr));
        }
        s
    }
}

pub async fn run_command(command: &str, shell: ShellKind) -> Result<CommandOutput> {
    let mut cmd = build(command, shell);
    cmd.kill_on_drop(true);
    cmd.stdin(std::process::Stdio::null());

    match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => Ok(CommandOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            timed_out: false,
        }),
        Ok(Err(e)) => Err(cua_core::Error::Io(e)),
        Err(_) => Ok(CommandOutput {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        }),
    }
}

fn build(command: &str, shell: ShellKind) -> Command {
    let shell = match shell {
        ShellKind::Auto => {
            if cfg!(windows) {
                ShellKind::PowerShell
            } else {
                ShellKind::Bash
            }
        }
        s => s,
    };

    let mut cmd = match shell {
        ShellKind::PowerShell => {
            let mut c = Command::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ]);
            c
        }
        ShellKind::Cmd => {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        }
        ShellKind::Bash | ShellKind::Auto => {
            if cfg!(windows) {
                // Git Bash is the most common bash on Windows dev machines.
                let mut c = Command::new("bash");
                c.args(["-lc", command]);
                c
            } else {
                let mut c = Command::new("/bin/sh");
                c.args(["-lc", command]);
                c
            }
        }
    };
    cmd.env("TERM", "dumb");
    cmd
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_OUTPUT {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_OUTPUT).collect();
    out.push_str("\n...(truncated)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echoes_stdout() {
        let cmd = if cfg!(windows) {
            "Write-Output hi"
        } else {
            "echo hi"
        };
        let out = run_command(cmd, ShellKind::Auto).await.unwrap();
        assert!(out.success(), "{out:?}");
        assert!(out.stdout.contains("hi"));
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let out = run_command("exit 3", ShellKind::Auto).await.unwrap();
        assert!(!out.success());
        assert_eq!(out.code, Some(3));
    }

    #[test]
    fn truncates_long_output() {
        let long = "x".repeat(MAX_OUTPUT + 10);
        assert!(truncate(&long).contains("truncated"));
    }
}
