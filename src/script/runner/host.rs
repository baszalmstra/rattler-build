//! Host runner - executes commands directly on the host system

/// Runner that executes commands directly on the host system
pub struct HostRunner;

impl HostRunner {
    /// Build a base command for host execution
    ///
    /// Returns a tokio::process::Command with the command and args set.
    /// The caller should add environment variables, working directory, etc.
    pub fn build_command(
        &self,
        command_args: &[&str],
    ) -> Result<tokio::process::Command, std::io::Error> {
        let mut command = tokio::process::Command::new(command_args[0]);
        command.args(&command_args[1..]);
        Ok(command)
    }
}
