//! Module for different script execution runners (host, sandbox, docker)

mod docker;
mod host;
mod sandbox;

pub use docker::{DockerArguments, DockerConfiguration, DockerRunner};
pub use host::HostRunner;
pub use sandbox::SandboxRunner;

use crate::script::SandboxConfiguration;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::script::normalize_crlf;

/// Trait for different script execution runners
///
/// A runner is responsible for building a command configured for a specific execution
/// environment (e.g., on the host, in a sandbox, or in a docker container).
pub trait Runner {
    /// Build a command ready for execution in the specific environment
    ///
    /// # Arguments
    ///
    /// * `args` - The command and its arguments to execute
    /// * `cwd` - The working directory for the command
    ///
    /// # Returns
    ///
    /// Returns a configured `tokio::process::Command` ready to be spawned
    fn build_command(&self, args: &[&str], cwd: &Path) -> tokio::process::Command;
}

/// Execute a command with output streaming and string replacements
///
/// This function handles all the common execution logic:
/// - Opens/creates the build log file
/// - Spawns the command and captures stdout/stderr
/// - Applies string replacements to output lines (for path masking, secret redaction)
/// - Writes output to both the log file and tracing
/// - Waits for command completion and returns the output
async fn execute_with_replacements(
    mut command: tokio::process::Command,
    cwd: &Path,
    replacements: &HashMap<String, String>,
) -> Result<std::process::Output, std::io::Error> {
    // Create or open the build log file
    let log_file_path = cwd.join("conda_build.log");
    let mut log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .await?;

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

/// Configuration for selecting and configuring a runner
#[derive(Debug, Clone)]
pub enum RunnerConfiguration {
    /// Execute directly on the host system
    Host,
    /// Execute in a sandboxed environment
    Sandbox(SandboxConfiguration),
    /// Execute in a Docker container
    Docker(DockerConfiguration, Vec<PathBuf>), // config + additional mounts
}

impl RunnerConfiguration {
    /// Create a runner instance based on this configuration
    pub fn create_runner(&self) -> Box<dyn Runner + Send + Sync> {
        match self {
            RunnerConfiguration::Host => Box::new(HostRunner),
            RunnerConfiguration::Sandbox(config) => Box::new(SandboxRunner::new(config.clone())),
            RunnerConfiguration::Docker(config, mounts) => {
                Box::new(DockerRunner::new(config.clone()).with_mounts(mounts.clone()))
            }
        }
    }

    /// Run a command in the configured execution environment
    ///
    /// This is the main entry point for executing commands through runners.
    /// It creates the appropriate runner, builds the command, and executes it
    /// with output streaming and string replacements.
    ///
    /// # Arguments
    ///
    /// * `args` - The command and its arguments to execute
    /// * `cwd` - The working directory for the command
    /// * `replacements` - String replacements to apply to output (for path masking, secret redaction)
    ///
    /// # Returns
    ///
    /// Returns the output of the command execution
    pub async fn run_command(
        &self,
        args: &[&str],
        cwd: &Path,
        replacements: &HashMap<String, String>,
    ) -> Result<std::process::Output, std::io::Error> {
        let runner = self.create_runner();
        let command = runner.build_command(args, cwd);
        execute_with_replacements(command, cwd, replacements).await
    }

    /// Check if this is a host runner
    pub fn is_host(&self) -> bool {
        matches!(self, RunnerConfiguration::Host)
    }

    /// Check if this is a sandbox runner
    pub fn is_sandbox(&self) -> bool {
        matches!(self, RunnerConfiguration::Sandbox(_))
    }

    /// Check if this is a docker runner
    pub fn is_docker(&self) -> bool {
        matches!(self, RunnerConfiguration::Docker(_, _))
    }
}
