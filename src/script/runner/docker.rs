//! Docker runner - executes commands inside a Docker container

use super::Runner;
use async_trait::async_trait;
use crate::script::normalize_crlf;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

/// CLI argument parser for Docker runner
#[derive(Debug, Parser, Clone, Default)]
pub struct DockerArguments {
    /// Enable the Docker runner
    #[clap(long, action, help_heading = "Docker runner arguments")]
    pub docker: bool,

    /// Docker image to use for build execution (required if --docker is enabled)
    #[clap(long, help_heading = "Docker runner arguments")]
    pub docker_image: Option<String>,

    /// Allow network access during Docker build (default: false if docker is enabled)
    #[clap(long, action, help_heading = "Docker runner arguments")]
    pub docker_allow_network: bool,
}

impl From<DockerArguments> for Option<DockerConfiguration> {
    fn from(args: DockerArguments) -> Self {
        if !args.docker {
            return None;
        }

        // Require docker_image if docker is enabled
        let image = args.docker_image.unwrap_or_else(|| {
            eprintln!("Error: --docker-image is required when --docker is enabled");
            std::process::exit(1);
        });

        Some(DockerConfiguration {
            image,
            allow_network: args.docker_allow_network,
        })
    }
}

/// Configuration for the Docker runner
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DockerConfiguration {
    /// The Docker image to use for execution
    pub image: String,
    /// Whether to allow network access during execution
    #[serde(default)]
    pub allow_network: bool,
}

impl Display for DockerConfiguration {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{} Docker Configuration", console::Emoji("🐳", " "))?;
        writeln!(f, "Image: {}", self.image)?;
        writeln!(
            f,
            "Network Access: {}",
            if self.allow_network {
                console::Emoji("✅", " ")
            } else {
                console::Emoji("❌", " ")
            }
        )?;
        Ok(())
    }
}

impl DockerConfiguration {
    /// Create a new Docker configuration with the given image
    pub fn new(image: String) -> Self {
        Self {
            image,
            allow_network: false,
        }
    }

    /// Set whether to allow network access
    pub fn with_network(mut self, allow_network: bool) -> Self {
        self.allow_network = allow_network;
        self
    }
}

/// Runner that executes commands inside a Docker container
pub struct DockerRunner {
    config: DockerConfiguration,
    /// Additional paths to mount (beyond cwd)
    additional_mounts: Vec<PathBuf>,
}

impl DockerRunner {
    /// Create a new Docker runner with the given configuration
    pub fn new(config: DockerConfiguration) -> Self {
        Self {
            config,
            additional_mounts: Vec::new(),
        }
    }

    /// Add additional paths to mount in the container
    pub fn with_mounts(mut self, mounts: Vec<PathBuf>) -> Self {
        self.additional_mounts = mounts;
        self
    }

    /// Check if Docker is available
    async fn check_docker_available() -> Result<(), std::io::Error> {
        let output = tokio::process::Command::new("docker")
            .arg("--version")
            .output()
            .await;

        match output {
            Ok(output) if output.status.success() => Ok(()),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Docker command failed",
            )),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Docker is not available. Please install Docker to use the docker runner.",
            )),
        }
    }

    /// Collect all unique parent directories to mount
    fn collect_mount_paths(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut paths = vec![cwd.to_path_buf()];
        paths.extend(self.additional_mounts.clone());

        // Deduplicate and collect parent directories
        let mut unique_paths = std::collections::HashSet::new();
        for path in paths {
            // Try to canonicalize the path, but if it fails (e.g., path doesn't exist yet),
            // just use the path as-is
            let canonical = path.canonicalize().unwrap_or(path.clone());

            // Add the path itself
            unique_paths.insert(canonical.clone());

            // Also add parent directories to ensure they exist
            if let Some(parent) = canonical.parent() {
                unique_paths.insert(parent.to_path_buf());
            }
        }

        unique_paths.into_iter().collect()
    }
}

#[async_trait]
impl Runner for DockerRunner {
    async fn run_command(
        &self,
        args: &[&str],
        cwd: &Path,
        replacements: &HashMap<String, String>,
    ) -> Result<std::process::Output, std::io::Error> {
        // Check if Docker is available
        Self::check_docker_available().await?;

        tracing::info!("{}", self.config);

        // Create or open the build log file
        let log_file_path = cwd.join("conda_build.log");
        let mut log_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file_path)
            .await?;

        let mut command = tokio::process::Command::new("docker");

        // Build docker run arguments
        command.arg("run");
        command.arg("--rm"); // Remove container after execution

        // Network configuration
        if !self.config.allow_network {
            command.arg("--network=none");
        }

        // Mount necessary directories
        // We mount at the same paths to ensure scripts work without modification
        let mount_paths = self.collect_mount_paths(cwd);
        for path in mount_paths {
            let path_str = path.to_string_lossy();
            command.arg("-v");
            command.arg(format!("{}:{}", path_str, path_str));
        }

        // Set working directory in container to match host
        command.arg("-w");
        command.arg(cwd.to_string_lossy().to_string());

        // Pass environment variables from the host
        // Note: Docker already passes some env vars, but we don't pass all of them
        // The script will set up its own environment through the activation scripts

        // Specify the image
        command.arg(&self.config.image);

        // Add the command to execute
        command.args(args);

        command
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
