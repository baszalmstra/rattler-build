//! Module for different script execution runners (host, sandbox, docker)

mod docker;
mod host;
mod sandbox;

pub use docker::{DockerArguments, DockerConfiguration, DockerRunner};
pub use host::HostRunner;
pub use sandbox::SandboxRunner;

use crate::script::SandboxConfiguration;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::script::normalize_crlf;

/// Access mode for volume mounts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeAccessMode {
    /// Read-only access
    ReadOnly,
    /// Read-write access
    ReadWrite,
}

/// A volume mount specification with path and access mode
#[derive(Debug, Clone)]
pub struct VolumeMount {
    /// The path to mount
    pub path: PathBuf,
    /// The access mode for this mount
    pub access_mode: VolumeAccessMode,
}

impl VolumeMount {
    /// Create a new read-only volume mount
    pub fn read_only(path: PathBuf) -> Self {
        Self {
            path,
            access_mode: VolumeAccessMode::ReadOnly,
        }
    }

    /// Create a new read-write volume mount
    pub fn read_write(path: PathBuf) -> Self {
        Self {
            path,
            access_mode: VolumeAccessMode::ReadWrite,
        }
    }
}

/// Context for running a command in a specific execution environment
#[derive(Debug)]
pub struct RunnerContext<'a> {
    /// The command and its arguments to execute
    pub command_args: &'a [&'a str],
    /// The working directory for the command
    pub work_dir: &'a Path,
    /// Environment variables to set for the command
    pub env_vars: &'a IndexMap<String, String>,
    /// Volume mounts with their access modes
    pub mounts: &'a [VolumeMount],
}

/// Trait for different script execution runners
///
/// A runner is responsible for building a command configured for a specific execution
/// environment (e.g., on the host, in a sandbox, or in a docker container).
pub trait Runner {
    /// Build a command ready for execution in the specific environment
    ///
    /// # Arguments
    ///
    /// * `context` - The context containing command args, working directory, and environment variables
    ///
    /// # Returns
    ///
    /// Returns a configured `tokio::process::Command` ready to be spawned,
    /// or an error if the command cannot be built (e.g., required tool not found)
    fn build_command(
        &self,
        context: &RunnerContext,
    ) -> Result<tokio::process::Command, std::io::Error>;
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

/// A prepared runner ready for executing commands
///
/// This holds a runner instance along with any associated configuration (like volume mounts).
/// The interpreter can use this to execute one or more commands in the same execution environment.
pub struct PreparedRunner {
    runner: Box<dyn Runner + Send + Sync>,
    mounts: Vec<VolumeMount>,
}

impl PreparedRunner {
    /// Execute a command in this runner's environment
    ///
    /// # Arguments
    ///
    /// * `command_args` - The command and its arguments to execute
    /// * `work_dir` - The working directory for the command
    /// * `env_vars` - Environment variables to set for the command
    /// * `replacements` - String replacements to apply to output (for path masking, secret redaction)
    ///
    /// # Returns
    ///
    /// Returns the output of the command execution
    pub async fn execute_command(
        &self,
        command_args: &[&str],
        work_dir: &Path,
        env_vars: &IndexMap<String, String>,
        replacements: &HashMap<String, String>,
    ) -> Result<std::process::Output, std::io::Error> {
        let context = RunnerContext {
            command_args,
            work_dir,
            env_vars,
            mounts: &self.mounts,
        };
        let command = self.runner.build_command(&context)?;
        execute_with_replacements(command, work_dir, replacements).await
    }
}

/// Configuration for selecting and configuring a runner
#[derive(Debug, Clone)]
pub enum RunnerConfiguration {
    /// Execute directly on the host system
    Host,
    /// Execute in a sandboxed environment
    Sandbox(SandboxConfiguration),
    /// Execute in a Docker container
    Docker(DockerConfiguration, Vec<VolumeMount>), // config + volume mounts
}

impl RunnerConfiguration {
    /// Prepare a runner from this configuration
    ///
    /// This creates the actual runner instance that can be used to execute commands.
    pub fn prepare_runner(&self) -> PreparedRunner {
        let runner = match self {
            RunnerConfiguration::Host => Box::new(HostRunner) as Box<dyn Runner + Send + Sync>,
            RunnerConfiguration::Sandbox(config) => Box::new(SandboxRunner::new(config.clone())),
            RunnerConfiguration::Docker(config, _) => Box::new(DockerRunner::new(config.clone())),
        };

        let mounts = match self {
            RunnerConfiguration::Docker(_, mounts) => mounts.clone(),
            _ => Vec::new(),
        };

        PreparedRunner { runner, mounts }
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
