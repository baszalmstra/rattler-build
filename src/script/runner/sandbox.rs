//! Sandbox runner - executes commands using rattler-sandbox

use super::{Runner, RunnerContext};
use crate::script::SandboxConfiguration;
use std::path::PathBuf;
use std::process::Stdio;

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
        context: &RunnerContext,
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
        let sandbox_args = self.config.with_cwd(context.work_dir).to_args();
        command.args(&sandbox_args);

        // Add the actual command to execute (as positional arguments)
        command.arg(context.command_args[0]);
        command.args(&context.command_args[1..]);

        command
            .current_dir(context.work_dir)
            // when using `pixi global install bash` the current work dir
            // causes some strange issues that are fixed when setting the `PWD`
            .env("PWD", context.work_dir)
            .envs(context.env_vars)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        Ok(command)
    }
}
