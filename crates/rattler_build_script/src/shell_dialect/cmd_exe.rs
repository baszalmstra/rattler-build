use std::fmt::Write as _;
use std::path::Path;

use indexmap::IndexMap;
use rattler_shell::shell::{self, Shell};

use super::{CommandSpec, ShellDialect};
use crate::{
    ExecutionContext, PrefixLayout,
    windows_machine::{WindowsMachine, windows_machine_transition},
};

pub(crate) struct CmdExeDialect;

/// Argument with which a replay wrapper restarts itself in the build's
/// architecture; the restarted wrapper skips the restart.
const IN_BUILD_MACHINE_ARG: &str = "--rattler-build-in-build-machine";

impl ShellDialect for CmdExeDialect {
    fn shell(&self) -> shell::ShellEnum {
        shell::CmdExe.into()
    }

    fn default_interpreter(&self) -> &'static str {
        "cmd"
    }

    fn preamble(&self, activation_script_path: Option<&Path>) -> String {
        let activation = activation_script_path
            .map(|path| {
                format!(
                    r#"IF "%CONDA_BUILD%" == "" (
    @rem special behavior from conda-build for Windows
    call "{}"
)
@rem re-enable echo because the activation scripts might have messed with it
@echo on
"#,
                    path.to_string_lossy()
                )
            })
            .unwrap_or_default();
        format!(
            r#"
@chcp 65001 > nul
@echo on
{activation}"#
        )
    }

    fn command_to_run_script(
        &self,
        build_script_path: &Path,
        start_dir: &Path,
        context: &ExecutionContext,
    ) -> CommandSpec {
        if let Some(machine) = machine_transition(context) {
            // `start` waits for the child (see `start_in_machine`), but the
            // outer `cmd /c` would still return the status of the `start`
            // command itself, hence the explicit delayed `ERRORLEVEL`
            // expansion and `exit /b` after the child finishes.
            //
            // The whole `start` line is one argument of the outer `cmd /c`, and
            // the argument quoting of the process launch escapes embedded
            // double quotes in a way cmd does not understand. The script is
            // therefore named relative to `start_dir`, the directory the outer
            // process starts in: generated scripts live below it, so the
            // relative path consists of generated names that need no quotes.
            // Quote it when necessary anyway so a path with whitespace remains
            // a single argument of the child.
            let script_path = build_script_path
                .strip_prefix(start_dir)
                .unwrap_or(build_script_path)
                .to_string_lossy();
            let script_path = super::quote_arg(&self.shell(), &script_path);
            let command = format!(
                "{} & exit /b !ERRORLEVEL!",
                start_in_machine(machine, &script_path)
            );
            CommandSpec::new(
                "cmd.exe",
                [
                    "/d".to_string(),
                    "/v:on".to_string(),
                    "/c".to_string(),
                    command,
                ],
            )
        } else {
            CommandSpec::new(
                "cmd.exe",
                [
                    "/d".to_string(),
                    "/c".to_string(),
                    build_script_path.to_string_lossy().into_owned(),
                ],
            )
        }
    }

    /// When the build needs another architecture than this process, the
    /// replay wrapper first restarts itself in a command processor of that
    /// architecture, the way [`Self::command_to_run_script`] starts wrappers,
    /// and exits with its status. The restarted wrapper gets an argument
    /// that makes it skip the restart, so it activates and runs the steps
    /// itself, and restarts at most once however it was started. `goto`
    /// keeps the restart out of a parenthesized block, whose `%errorlevel%`
    /// would expand before `start` runs.
    fn replay_preamble(
        &self,
        script_path: &Path,
        activation_script_path: &Path,
        context: &ExecutionContext,
    ) -> String {
        let preamble = self.preamble(Some(activation_script_path));
        let Some(machine) = machine_transition(context) else {
            return preamble;
        };
        let restart = start_in_machine(
            machine,
            &format!("{} {IN_BUILD_MACHINE_ARG}", call_script(script_path)),
        );
        format!(
            "@rem Run in the architecture of the build, like rattler-build does\n\
             @if \"%~1\" == \"{IN_BUILD_MACHINE_ARG}\" goto in_build_machine\n\
             @{restart}\n\
             @exit /b %errorlevel%\n\
             :in_build_machine\n\
             {preamble}"
        )
    }

    fn replacements_template(&self) -> &'static str {
        "%((var))%"
    }

    fn supports_sandbox(&self) -> bool {
        false
    }

    fn native_section_script_command(&self, script_path: &Path) -> Option<Vec<String>> {
        Some(child_command(script_path))
    }

    /// A build that needs another architecture than this process starts the
    /// child like [`Self::command_to_run_script`] does, so it does not depend
    /// on the architecture of the wrapper. The status is checked on its own
    /// line, where `%errorlevel%` expands after the child has exited.
    fn child_script_command(&self, script_path: &Path, context: &ExecutionContext) -> String {
        let command = match machine_transition(context) {
            Some(machine) => start_in_machine(machine, &call_script(script_path)),
            None => {
                let shell = self.shell();
                child_command(script_path)
                    .iter()
                    .map(|arg| super::quote_arg(&shell, arg))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        format!("@{command}\n@if %errorlevel% neq 0 exit /b %errorlevel%\n")
    }

    fn remove_files(&self, paths: &[&Path]) -> String {
        let mut lines = String::new();
        for path in paths {
            let quoted = quoted_path(path);
            let failed = echo_error(&format!("cannot remove {}", path.display()));
            let _ = writeln!(lines, "@if exist {quoted} del /f /q {quoted}");
            let _ = writeln!(lines, "@if exist {quoted} ({failed} & exit /b 1)");
        }
        lines
    }

    /// `fc /b` compares the files byte by byte and fails when they differ
    /// or one is missing. It is started from the system directory, as the
    /// activated `PATH` may not include it. A missing manifest has no size
    /// to compare, hence the `exist` check first.
    fn check_step_manifest(
        &self,
        manifest: &Path,
        recorded: Option<&Path>,
        message: &str,
    ) -> String {
        let manifest = quoted_path(manifest);
        let failed = format!("({} & exit /b 1)", echo_error(message));
        match recorded {
            None => format!(
                "@if exist {manifest} for %%F in ({manifest}) do @if %%~zF gtr 0 {failed}\n"
            ),
            Some(recorded) => format!(
                "@\"%SystemRoot%\\System32\\fc.exe\" /b {manifest} {} > nul 2>&1 || {failed}\n",
                quoted_path(recorded)
            ),
        }
    }

    /// `setlocal`/`endlocal` scope environment changes, while `pushd`/`popd`
    /// restore the working directory after successful sections. The saved
    /// errorlevel keeps `popd`/`endlocal` from masking a failing body.
    fn scope_section(
        &self,
        label: Option<&str>,
        env: &IndexMap<String, String>,
        literal_env: &[(&str, &str)],
        cwd: Option<&Path>,
        body: &str,
    ) -> Result<String, std::io::Error> {
        let shell = shell::CmdExe;
        let mut out = String::new();
        if let Some(label) = label {
            let _ = writeln!(out, "@rem === {label} ===");
        }
        out.push_str("setlocal\n");
        for (key, value) in env {
            super::validate_env_assignment(key, value)?;
            shell
                .set_env_var(&mut out, key, value)
                .map_err(std::io::Error::other)?;
        }
        for (name, value) in literal_env {
            self.set_literal_env_var(&mut out, name, value)?;
        }
        let cwd = cwd
            .map(|cwd| super::quote_arg(&self.shell(), &cwd.to_string_lossy()))
            .unwrap_or_else(|| ".".to_string());
        // `pushd` can misparse an unquoted path containing forward slashes as
        // command switches, even when the path has no spaces.
        let cwd = if cwd.starts_with('"') {
            cwd
        } else {
            format!("\"{cwd}\"")
        };
        // Use command chaining instead of inspecting `%errorlevel%`: a successful
        // `pushd` does not reliably clear an error left by environment activation.
        let _ = writeln!(out, "pushd {cwd} || exit /b 1");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("set \"RB_SECTION_ERRORLEVEL=%errorlevel%\"\n");
        out.push_str("popd\n");
        out.push_str(
            "if %RB_SECTION_ERRORLEVEL% equ 0 if %errorlevel% neq 0 set \"RB_SECTION_ERRORLEVEL=%errorlevel%\"\n",
        );
        out.push_str("endlocal & if %RB_SECTION_ERRORLEVEL% neq 0 exit /b %RB_SECTION_ERRORLEVEL%");
        Ok(out)
    }

    /// Inside `@SET "name=value"` the operators `&`, `|`, `<`, `>`, `(`,
    /// `)` and `^` are literal, and `%` is written as `%%` so it is not
    /// expanded. `!` is literal too unless delayed expansion is enabled,
    /// which the registry or `cmd /v:on` can do for the whole wrapper. When
    /// `value` has a `!`, a second line sets it again with `^` and `!`
    /// escaped, only when delayed expansion is enabled: then `"!!"`
    /// expands to `""`. A double quote would end the quoted assignment, so a
    /// value with one, which no Windows path has, is rejected.
    fn set_literal_env_var(
        &self,
        out: &mut String,
        name: &str,
        value: &str,
    ) -> Result<(), std::io::Error> {
        super::validate_env_assignment(name, value)?;
        if value.contains('"') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "environment variable '{name}' contains a double quote, which cannot be set literally in a batch file"
                ),
            ));
        }
        let value = value.replace('%', "%%");
        let _ = writeln!(out, "@SET \"{name}={value}\"");
        if value.contains('!') {
            let delayed = value.replace('^', "^^").replace('!', "^!");
            let _ = writeln!(out, "@if \"!!\"==\"\" @SET \"{name}={delayed}\"");
        }
        Ok(())
    }

    /// Returns reproduction instructions for the failed cmd wrapper script.
    fn debug_info(&self, work_dir: &Path, context: &ExecutionContext) -> String {
        let mut output = String::new();

        output.push_str("\nScript execution failed.\n\n");
        output.push_str(&format!("  Work directory: {}\n", work_dir.display()));
        output.push_str(&format!("  Prefix: {}\n", context.host().path().display()));

        if context.layout() == PrefixLayout::Separate {
            output.push_str(&format!(
                "  Build prefix: {}\n",
                context.build().path().display()
            ));
        } else {
            output.push_str("  Build prefix: None\n");
        }

        let command =
            self.command_to_run_script(&work_dir.join("conda_build.bat"), work_dir, context);
        output.push_str("\nTo run the script manually, use the following command:\n");
        output.push_str(&format!(
            "  cd {:?} && {} {}\n\n",
            work_dir,
            command.program,
            command.args.join(" ")
        ));
        output.push_str("To run commands interactively in the build environment:\n");
        output.push_str(&format!("  cd {:?} && call build_env.bat", work_dir));

        output
    }
}

/// The architecture a Windows build runs its wrappers in when it differs
/// from the architecture of this process.
fn machine_transition(context: &ExecutionContext) -> Option<WindowsMachine> {
    windows_machine_transition(
        context.runtime().process_platform(),
        context.build().platform(),
    )
}

/// Returns the `start` command running `command` with `cmd /c` in a new
/// command processor of `machine` and waiting for it, so that the errorlevel
/// after it is the status of `command`.
///
/// `start /machine` selects the architecture of the child `cmd.exe`, and
/// `/wait` makes `start` return only after the child finished.
fn start_in_machine(machine: WindowsMachine, command: &str) -> String {
    // `/machine x86` does not redirect an explicit `cmd.exe` lookup from
    // System32. Launch the x86 command interpreter from SysWOW64 directly.
    // SystemRoot is conventionally an unspaced system path, so keep it
    // unquoted to avoid `start` treating it as a title. The other
    // architectures use `cmd.exe`, whose image selection is handled by
    // `/machine`.
    let child_cmd = match machine {
        WindowsMachine::X86 => r"%SystemRoot%\SysWOW64\cmd.exe",
        WindowsMachine::Amd64 | WindowsMachine::Arm64 => "cmd.exe",
    };
    format!(
        "start /b /wait /machine {} {child_cmd} /d /c {command}",
        machine.start_argument(),
    )
}

/// Returns `call <script_path>` with the path quoted for a batch file line;
/// see [`child_command`] for why `call` is needed.
fn call_script(script_path: &Path) -> String {
    format!(
        "call {}",
        super::quote_arg(&shell::CmdExe.into(), &script_path.to_string_lossy())
    )
}

/// Runs `script_path` in a child command processor. `call` keeps a quoted
/// path intact, since `cmd /c` otherwise strips the outer quotes of a line
/// containing special characters.
fn child_command(script_path: &Path) -> Vec<String> {
    // Activated build environments can replace PATH entirely. Resolve the
    // command processor before entering the wrapper so nested native
    // scripts do not depend on `cmd.exe` remaining discoverable.
    let command_processor = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    vec![
        command_processor,
        "/d".to_string(),
        "/c".to_string(),
        "call".to_string(),
        script_path.to_string_lossy().into_owned(),
    ]
}

/// Returns `path` in double quotes for a batch file line, with `%` escaped
/// so it is not expanded as a variable.
fn quoted_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('%', "%%"))
}

/// Returns an `echo` command writing `message` to stderr from a batch file
/// line, also inside a parenthesized block: the characters `cmd.exe` would
/// read as operators are escaped, and `%` so it is not expanded.
fn echo_error(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for c in message.chars() {
        match c {
            '^' | '&' | '|' | '<' | '>' | '(' | ')' => {
                escaped.push('^');
                escaped.push(c);
            }
            '%' => escaped.push_str("%%"),
            '\r' | '\n' => escaped.push(' '),
            _ => escaped.push(c),
        }
    }
    format!("1>&2 echo {escaped}")
}
