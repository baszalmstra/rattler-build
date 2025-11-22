//! Module for different script execution runners (host, sandbox, docker)

mod host;
mod sandbox;
mod docker;

pub use host::HostRunner;
pub use sandbox::SandboxRunner;
pub use docker::{DockerRunner, DockerConfiguration, DockerArguments};

use crate::script::SandboxConfiguration;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Trait for different script execution runners
///
/// A runner is responsible for executing a command in a specific environment
/// (e.g., on the host, in a sandbox, or in a docker container).
#[async_trait]
pub trait Runner {
    /// Run a command in the specific execution environment
    ///
    /// # Arguments
    ///
    /// * `args` - The command and its arguments to execute
    /// * `cwd` - The working directory for the command
    /// * `replacements` - String replacements to apply to output (for path masking)
    ///
    /// # Returns
    ///
    /// Returns the output of the command execution
    async fn run_command(
        &self,
        args: &[&str],
        cwd: &Path,
        replacements: &HashMap<String, String>,
    ) -> Result<std::process::Output, std::io::Error>;
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
