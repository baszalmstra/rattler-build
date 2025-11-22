//! Sandbox runner - executes commands using rattler-sandbox

use super::Runner;
use async_trait::async_trait;
use crate::script::{normalize_crlf, SandboxConfiguration};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

/// Find the rattler-sandbox executable in PATH
fn find_rattler_sandbox() -> Option<PathBuf> {
    which::which("rattler-sandbox").ok()
}

/// Runner that executes commands in a sandboxed environment using rattler-sandbox
pub struct SandboxRunner {
    config: SandboxConfiguration,
}

impl SandboxRunner {
    /// Create a new sandbox runner with the given configuration
    pub fn new(config: SandboxConfiguration) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Runner for SandboxRunner {
    async fn run_command(
        &self,
        args: &[&str],
        cwd: &Path,
        replacements: &HashMap<String, String>,
    ) -> Result<std::process::Output, std::io::Error> {
        tracing::info!("{}", self.config);

        // Create or open the build log file
        let log_file_path = cwd.join("conda_build.log");
        let mut log_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file_path)
            .await?;

        // Try to find rattler-sandbox executable
        let sandbox_exe = find_rattler_sandbox().ok_or_else(|| {
            tracing::error!("rattler-sandbox executable not found in PATH");
            tracing::error!("Please install it by running: pixi global install rattler-sandbox");
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "rattler-sandbox executable not found. Please install it with: pixi global install rattler-sandbox",
            )
        })?;

        let mut command = tokio::process::Command::new(sandbox_exe);

        // Add sandbox configuration arguments
        let sandbox_args = self.config.with_cwd(cwd).to_args();
        command.args(&sandbox_args);

        // Add the actual command to execute (as positional arguments)
        command.arg(args[0]);
        command.args(&args[1..]);

        command
            .current_dir(cwd)
            // when using `pixi global install bash` the current work dir
            // causes some strange issues that are fixed when setting the `PWD`
            .env("PWD", cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;

        let stdout = child.stdout.take().expect("Failed to take stdout");
        let stderr = child.stderr.take().expect("Failed to take stderr");

        let stdout_wrapped = normalize_crlf(stdout);
        let stderr_wrapped = normalize_crlf(stderr);

        use tokio::io::AsyncBufReadExt;
        let mut stdout_lines = tokio::io::BufReader::new(stdout_wrapped).lines();
        let mut stderr_lines = tokio::io::BufReader::new(stderr_wrapped).lines();

        let mut stdout_log = String::new();
        let mut stderr_log = String::new();
        let mut closed = (false, false);

        loop {
            let (line, is_stderr) = tokio::select! {
                line = stdout_lines.next_line() => (line, false),
                line = stderr_lines.next_line() => (line, true),
                else => break,
            };

            match line {
                Ok(Some(line)) => {
                    let filtered_line = replacements
                        .iter()
                        .fold(line, |acc, (from, to)| acc.replace(from, to));

                    if is_stderr {
                        stderr_log.push_str(&filtered_line);
                        stderr_log.push('\n');
                    } else {
                        stdout_log.push_str(&filtered_line);
                        stdout_log.push('\n');
                    }

                    // Write to log file
                    if let Err(e) = log_file.write_all(filtered_line.as_bytes()).await {
                        tracing::warn!("Failed to write to build log: {:?}", e);
                    }
                    if let Err(e) = log_file.write_all(b"\n").await {
                        tracing::warn!("Failed to write newline to build log: {:?}", e);
                    }

                    tracing::info!("{}", filtered_line);
                }
                Ok(None) if !is_stderr => closed.0 = true,
                Ok(None) if is_stderr => closed.1 = true,
                Ok(None) => unreachable!(),
                Err(e) => {
                    tracing::warn!("Error reading output: {:?}", e);
                    break;
                }
            };
            // make sure we close the loop when both stdout and stderr are closed
            if closed == (true, true) {
                break;
            }
        }

        let status = child.wait().await?;

        // Flush and close the log file
        if let Err(e) = log_file.flush().await {
            tracing::warn!("Failed to flush build log: {:?}", e);
        }

        Ok(std::process::Output {
            status,
            stdout: stdout_log.into_bytes(),
            stderr: stderr_log.into_bytes(),
        })
    }
}
