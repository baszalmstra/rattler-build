//! Sandbox runner - executes commands using rattler-sandbox

use super::Runner;
use crate::script::SandboxConfiguration;
use std::path::{Path, PathBuf};

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

impl Runner for SandboxRunner {
    fn build_command(
        &self,
        command_args: &[&str],
        work_dir: &Path,
    ) -> Result<tokio::process::Command, std::io::Error> {
        tracing::info!("{}", self.config);

        // Find rattler-sandbox executable
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
        let sandbox_args = self.config.with_cwd(work_dir).to_args();
        command.args(&sandbox_args);

        // Add the actual command to execute (as positional arguments)
        command.arg(command_args[0]);
        command.args(&command_args[1..]);

        Ok(command)
    }
}
