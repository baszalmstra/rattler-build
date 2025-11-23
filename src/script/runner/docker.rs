//! Docker runner - executes commands inside a Docker container

use clap::Parser;
use comfy_table::Table;
use serde::{Deserialize, Serialize};

/// CLI argument parser for Docker runner
#[derive(Debug, Parser, Clone, Default)]
pub struct DockerArguments {
    /// Enable the Docker runner
    #[clap(
        long,
        action,
        help_heading = "Docker runner arguments",
        conflicts_with = "sandbox"
    )]
    pub docker: bool,

    /// Docker image to use for build execution (required if --docker is enabled)
    #[clap(
        long,
        help_heading = "Docker runner arguments",
        required_if_eq("docker", "true")
    )]
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

        // Docker runner is not supported on Windows
        #[cfg(windows)]
        {
            eprintln!("Error: Docker runner is not supported on Windows");
            eprintln!("Windows cannot reliably build packages for Linux targets");
            std::process::exit(1);
        }

        // docker_image is guaranteed to be Some() due to required_if_eq
        let image = args
            .docker_image
            .expect("docker_image is required when docker is enabled");

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
    mounts: Vec<super::VolumeMount>,
}

impl DockerRunner {
    /// Create a new Docker runner with the given configuration and volume mounts
    pub fn new(config: DockerConfiguration, mounts: Vec<super::VolumeMount>) -> Self {
        Self { config, mounts }
    }

    /// Display the Docker configuration as a table
    fn display_table(&self) {
        let mut table = Table::new();
        table
            .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
            .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);

        // Add header
        table.add_row(vec![
            "",
            &format!("{} Docker Build Environment", console::Emoji("🐳", "")),
        ]);

        // Add separator
        table.add_row(vec!["", ""]);

        // Add image
        table.add_row(vec![
            "Image:",
            &console::style(&self.config.image).cyan().bold().to_string(),
        ]);

        // Add network status
        let network_status = if self.config.allow_network {
            console::style("Enabled").green().to_string()
        } else {
            console::style("Isolated (--network=none)")
                .dim()
                .to_string()
        };
        table.add_row(vec!["Network:", &network_status]);

        // Add empty row for spacing
        table.add_row(vec!["", ""]);

        // Add volume mounts header
        table.add_row(vec!["Volume Mounts:", ""]);

        // Add each mount
        for mount in &self.mounts {
            let path_display = if let Some(label) = &mount.label {
                console::style(label).cyan().to_string()
            } else {
                mount.path.display().to_string()
            };

            let access = match mount.access_mode {
                super::VolumeAccessMode::ReadOnly => {
                    console::style("(read-only)").dim().to_string()
                }
                super::VolumeAccessMode::ReadWrite => {
                    console::style("(read-write)").dim().to_string()
                }
            };

            table.add_row(vec![
                &format!("  {}", console::Emoji("•", "-")),
                &format!("{} {}", path_display, access),
            ]);
        }

        // Add empty row for spacing
        table.add_row(vec!["", ""]);

        tracing::info!("\n{}", table);
    }
}

impl super::Runner for DockerRunner {
    fn build_command(
        &self,
        command_args: &[&str],
        work_dir: &std::path::Path,
    ) -> Result<tokio::process::Command, std::io::Error> {
        // Display the Docker configuration table
        self.display_table();

        // Check if docker command exists
        if which::which("docker").is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Docker is not available. Please install Docker to use the docker runner.",
            ));
        }

        let mut command = tokio::process::Command::new("docker");

        // Build docker run arguments
        command.arg("run");
        command.arg("--rm"); // Remove container after execution

        // Run with same user permissions to avoid permission conflicts in mounted volumes
        #[cfg(unix)]
        {
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            command.arg("--user");
            command.arg(format!("{}:{}", uid, gid));
        }

        // Network configuration
        if !self.config.allow_network {
            command.arg("--network=none");
        }

        // Mount necessary directories with appropriate access modes
        // We mount at the same paths to ensure scripts work without modification
        for mount in &self.mounts {
            let path_str = mount.path.to_string_lossy();
            command.arg("-v");
            let mount_spec = match mount.access_mode {
                super::VolumeAccessMode::ReadOnly => format!("{}:{}:ro", path_str, path_str),
                super::VolumeAccessMode::ReadWrite => format!("{}:{}", path_str, path_str),
            };
            command.arg(mount_spec);
        }

        // Set working directory in container to match host
        command.arg("-w");
        command.arg(work_dir.to_string_lossy().to_string());

        // Specify the image
        command.arg(&self.config.image);

        // Add the command to execute
        command.args(command_args);

        Ok(command)
    }
}
