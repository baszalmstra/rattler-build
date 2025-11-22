//! Docker runner - executes commands inside a Docker container

use super::Runner;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Stdio;

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

impl Runner for DockerRunner {
    fn build_command(&self, args: &[&str], cwd: &Path) -> tokio::process::Command {
        tracing::info!("{}", self.config);

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

        command
    }
}
