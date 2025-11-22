//! Host runner - executes commands directly on the host system

use super::{Runner, RunnerContext};
use std::process::Stdio;

/// Runner that executes commands directly on the host system
pub struct HostRunner;

impl Runner for HostRunner {
    fn build_command(
        &self,
        context: &RunnerContext,
    ) -> Result<tokio::process::Command, std::io::Error> {
        let mut command = tokio::process::Command::new(context.command_args[0]);

        command
            .current_dir(context.work_dir)
            // when using `pixi global install bash` the current work dir
            // causes some strange issues that are fixed when setting the `PWD`
            .env("PWD", context.work_dir)
            .envs(context.env_vars)
            .args(&context.command_args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        Ok(command)
    }
}
