//! Host runner - executes commands directly on the host system

use super::Runner;

/// Runner that executes commands directly on the host system
pub struct HostRunner;

impl Runner for HostRunner {
    fn build_command(
        &self,
        command_args: &[&str],
        work_dir: &std::path::Path,
    ) -> Result<tokio::process::Command, std::io::Error> {
        let mut command = tokio::process::Command::new(command_args[0]);
        command.args(&command_args[1..]);
        command.current_dir(work_dir);
        Ok(command)
    }
}
