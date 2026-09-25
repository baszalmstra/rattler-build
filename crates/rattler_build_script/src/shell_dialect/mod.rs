//! Platform-native shell dialect support.
//!
//! This module selects the [`ShellDialect`] for a platform and provides helpers
//! for writing shell-specific scripts. Specialized interpreters are described by
//! `crate::interpreter` and are emitted as commands inside the native wrapper.

mod bash;
mod cmd_exe;

use std::path::Path;

use indexmap::IndexMap;
use rattler_conda_types::Platform;
use rattler_shell::shell::{Shell, ShellEnum};

use crate::ExecutionContext;

/// A process invocation with owned arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

impl CommandSpec {
    pub(crate) fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Defines the platform-native shell syntax used for wrapper execution.
pub(crate) trait ShellDialect: Send + Sync {
    /// Returns the shell syntax used for the generated native wrapper script.
    fn shell(&self) -> ShellEnum;

    /// The recipe interpreter name for this wrapper shell (`bash`/`cmd`), used
    /// as the default when no interpreter is specified.
    fn default_interpreter(&self) -> &'static str;

    /// Returns the shell preamble inserted at the top of `conda_build.*`. It
    /// sources `activation_script_path` unless the environment is already
    /// activated; without an activation script the preamble never activates.
    fn preamble(&self, activation_script_path: Option<&Path>) -> String;

    /// Returns the process invocation used to execute the generated native
    /// wrapper script from a process started in `start_dir`. Its program is
    /// the executable of [`Self::shell`], which callers may replace with the
    /// resolved path of that executable.
    fn command_to_run_script(
        &self,
        build_script_path: &Path,
        start_dir: &Path,
        context: &ExecutionContext,
    ) -> CommandSpec;

    /// Returns the replacement template used when streaming process output.
    fn replacements_template(&self) -> &'static str;

    /// Returns whether this shell dialect supports rattler-sandbox execution.
    fn supports_sandbox(&self) -> bool {
        true
    }

    /// Returns a native-shell command that invokes a section script file, if
    /// native sections must be run indirectly. `cmd.exe` uses this so
    /// `exit /b` exits only the called section script, not the whole wrapper.
    fn native_section_script_command(&self, _script_path: &Path) -> Option<Vec<String>> {
        None
    }

    /// Returns the preamble of the wrapper `script_path` that replays the
    /// steps of a build (see [`Self::preamble`]). It makes the wrapper run in
    /// the architecture [`Self::command_to_run_script`] runs wrappers in,
    /// however the wrapper is started, before it activates with
    /// `activation_script_path` unless the environment is already activated.
    fn replay_preamble(
        &self,
        _script_path: &Path,
        activation_script_path: &Path,
        _context: &ExecutionContext,
    ) -> String {
        self.preamble(Some(activation_script_path))
    }

    /// Returns wrapper lines running the native script `script_path` in a
    /// new process of this shell, which inherits only the exported
    /// environment and runs in the architecture
    /// [`Self::command_to_run_script`] runs wrappers in. When that process
    /// fails, the wrapper exits with its status, whatever shell error options
    /// (such as `set -e`) activation left in effect.
    fn child_script_command(&self, script_path: &Path, context: &ExecutionContext) -> String;

    /// Returns wrapper lines removing the files at `paths` that exist. When
    /// a file cannot be removed, the wrapper exits with status 1, whatever
    /// shell error options activation left in effect.
    fn remove_files(&self, paths: &[&Path]) -> String;

    /// Returns wrapper lines checking the step manifest at `manifest` that a
    /// replayed step has just written. With `recorded`, the manifest must
    /// exist with exactly the bytes of the file at `recorded`; without, it
    /// must be missing or empty. Otherwise the wrapper writes `message` to
    /// stderr and exits with status 1, whatever shell error options
    /// activation left in effect.
    fn check_step_manifest(
        &self,
        manifest: &Path,
        recorded: Option<&Path>,
        message: &str,
    ) -> String;

    /// Wraps a non-empty section body in an isolated shell scope so its
    /// step-local `env` and shell state don't leak into later sections and a
    /// failure aborts the wrapper. `env` is emitted via [`Shell::set_env_var`]
    /// for consistent quoting, so its values keep the variable references
    /// the shell expands. `literal_env` is emitted after it via
    /// [`Self::set_literal_env_var`], so its variables take precedence and
    /// hold their values exactly as given. The scope primitive is
    /// shell-specific.
    fn scope_section(
        &self,
        label: Option<&str>,
        env: &IndexMap<String, String>,
        literal_env: &[(&str, &str)],
        cwd: Option<&Path>,
        body: &str,
    ) -> Result<String, std::io::Error>;

    /// Appends to `out` the wrapper line setting the variable `name` to
    /// `value` exactly as given. Unlike [`Shell::set_env_var`], the shell
    /// expands and interprets nothing in `value`, so it may be any path,
    /// whatever characters the shell would otherwise read as variable
    /// references, command substitutions, or operators. Fails when `name`
    /// is not a valid variable name or `value` cannot be written on a line
    /// of this shell.
    fn set_literal_env_var(
        &self,
        out: &mut String,
        name: &str,
        value: &str,
    ) -> Result<(), std::io::Error>;

    /// Returns human-readable reproduction instructions shown when execution fails.
    fn debug_info(&self, work_dir: &Path, context: &ExecutionContext) -> String;
}

/// Selects the native wrapper shell for the given platform: `cmd.exe` on
/// Windows, `bash` elsewhere. The script runs on the host, so callers pass the
/// runtime platform (which equals the host).
pub(crate) fn shell_dialect(platform: Platform) -> Box<dyn ShellDialect> {
    if platform.is_windows() {
        Box::new(cmd_exe::CmdExeDialect)
    } else {
        Box::new(bash::BashDialect)
    }
}

pub(crate) fn write_shell_script(
    shell: ShellEnum,
    script: &str,
) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    shell.write_script(&mut bytes, script)?;
    Ok(bytes)
}

/// Validate an env assignment before emitting it into a shell wrapper.
pub(crate) fn validate_env_assignment(key: &str, value: &str) -> Result<(), std::io::Error> {
    let mut chars = key.chars();
    let valid_key = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    if !valid_key {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid environment variable name '{key}'; expected [A-Za-z_][A-Za-z0-9_]*"),
        ));
    }
    if value.contains(['\n', '\r']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "environment variable '{key}' contains a newline, which cannot be represented safely in build scripts"
            ),
        ));
    }
    Ok(())
}

/// Quotes a single command argument for the given shell when it contains shell
/// metacharacters, whitespace, or is empty. `rattler_shell::Shell::run_command`
/// joins arguments with spaces without quoting, so a resolved interpreter,
/// script path, or `cwd` containing characters like spaces or `&` would
/// otherwise be split or interpreted by the shell. For cmd batch files, literal
/// `%` characters are also escaped to avoid environment-variable expansion.
pub(crate) fn quote_arg(shell: &ShellEnum, arg: &str) -> String {
    fn posix_needs_quotes(arg: &str) -> bool {
        arg.is_empty()
            || arg.chars().any(|c| {
                !(c.is_ascii_alphanumeric()
                    || matches!(c, '/' | '.' | '-' | '_' | ':' | '+' | '=' | '@' | '%'))
            })
    }

    fn cmd_needs_quotes(arg: &str) -> bool {
        arg.is_empty()
            || arg.chars().any(|c| {
                c.is_whitespace()
                    || matches!(c, '&' | '|' | '<' | '>' | '(' | ')' | '^' | ';' | ',' | '=')
            })
    }

    match shell {
        ShellEnum::CmdExe(_) => {
            let escaped = arg.replace('%', "%%");
            if cmd_needs_quotes(&escaped) {
                format!("\"{escaped}\"")
            } else {
                escaped
            }
        }
        _ if posix_needs_quotes(arg) => format!("'{}'", arg.replace('\'', r"'\''")),
        _ => arg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{quote_arg, shell_dialect};
    use crate::{
        ExecutionContext, RuntimeEnv,
        windows_machine::{WindowsMachine, windows_machine_transition},
    };
    use indexmap::IndexMap;
    use rattler_conda_types::Platform;
    use rattler_shell::shell::{self, Shell};

    /// bash scopes a section in a bare subshell, emits the label comment, and
    /// quotes env via `set_env_var` (shlex). No errorlevel guard — `set -e`
    /// from the preamble handles failure.
    #[test]
    fn bash_scope_section_subshell_env_and_label() {
        let dialect = shell_dialect(Platform::Linux64);
        let mut env = IndexMap::new();
        env.insert("FOO".to_string(), "a b".to_string());
        let out = dialect
            .scope_section(Some("uses: configure"), &env, &[], None, "echo hi")
            .unwrap();
        insta::assert_snapshot!(out, @r###"
# === uses: configure ===
(
export FOO='a b'
echo hi
)
"###);
    }

    /// No label and empty env => just `( body )`.
    #[test]
    fn bash_scope_section_minimal() {
        let dialect = shell_dialect(Platform::Linux64);
        let out = dialect
            .scope_section(None, &IndexMap::new(), &[], None, "echo hi")
            .unwrap();
        insta::assert_snapshot!(out, @r###"
(
echo hi
)
"###);
    }

    /// cmd scopes env via `setlocal`/`endlocal`, cwd via `pushd`/`popd`, and
    /// appends an errorlevel guard (required even for the last section).
    #[test]
    fn cmd_scope_section_setlocal_env_and_guard() {
        let dialect = shell_dialect(Platform::Win64);
        let mut env = IndexMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let out = dialect
            .scope_section(Some("step 1"), &env, &[], None, "echo hi")
            .unwrap();
        insta::assert_snapshot!(out, @r###"
@rem === step 1 ===
setlocal
@SET "FOO=bar"
pushd "." || exit /b 1
echo hi
set "RB_SECTION_ERRORLEVEL=%errorlevel%"
popd
if %RB_SECTION_ERRORLEVEL% equ 0 if %errorlevel% neq 0 set "RB_SECTION_ERRORLEVEL=%errorlevel%"
endlocal & if %RB_SECTION_ERRORLEVEL% neq 0 exit /b %RB_SECTION_ERRORLEVEL%
"###);
    }

    #[test]
    fn cmd_scope_section_pushd_uses_cwd() {
        let dialect = shell_dialect(Platform::Win64);
        let out = dialect
            .scope_section(
                Some("step 1"),
                &IndexMap::new(),
                &[],
                Some(std::path::Path::new(r"C:\some&dir")),
                "echo hi",
            )
            .unwrap();

        insta::assert_snapshot!(out, @r###"
@rem === step 1 ===
setlocal
pushd "C:\some&dir" || exit /b 1
echo hi
set "RB_SECTION_ERRORLEVEL=%errorlevel%"
popd
if %RB_SECTION_ERRORLEVEL% equ 0 if %errorlevel% neq 0 set "RB_SECTION_ERRORLEVEL=%errorlevel%"
endlocal & if %RB_SECTION_ERRORLEVEL% neq 0 exit /b %RB_SECTION_ERRORLEVEL%
"###);
    }

    #[test]
    fn scope_section_rejects_invalid_env_names() {
        let dialect = shell_dialect(Platform::Linux64);
        let mut env = IndexMap::new();
        env.insert("BAD-NAME".to_string(), "value".to_string());

        let err = dialect
            .scope_section(None, &env, &[], None, "echo hi")
            .expect_err("invalid env name should fail");

        assert!(
            err.to_string()
                .contains("invalid environment variable name")
        );
    }

    #[test]
    fn cmd_scope_section_rejects_newline_env_values() {
        let dialect = shell_dialect(Platform::Win64);
        let mut env = IndexMap::new();
        env.insert("FOO".to_string(), "safe\necho injected".to_string());

        let err = dialect
            .scope_section(None, &env, &[], None, "echo hi")
            .expect_err("newline env value should fail");

        assert!(err.to_string().contains("contains a newline"));
    }

    /// In the native shell of this machine, a literal variable of a section
    /// holds exactly the given value, whatever characters the shell would
    /// otherwise expand or interpret, and takes precedence over a section
    /// variable of the same name, while section variables still expand the
    /// references in their values. On Windows this holds with delayed
    /// expansion disabled and enabled.
    #[test]
    fn scope_section_sets_literal_env_verbatim() {
        let dialect = shell_dialect(Platform::current());
        let shell = dialect.shell();
        let dir = tempfile::tempdir().unwrap();
        let literal = "C:\\a b\\pct %OS% x\\bang!OS!y\\amp&c^d|e<f>(g)'q'~$HOME`echo pwned`$(echo x)\\steps.json";
        let reference = if cfg!(windows) {
            "%RB_TEST_BASE%-x"
        } else {
            "$RB_TEST_BASE-x"
        };
        let mut env = IndexMap::new();
        env.insert("RB_TEST_BASE".to_string(), "base".to_string());
        env.insert("RB_TEST_USER".to_string(), reference.to_string());
        env.insert("RB_TEST_LITERAL".to_string(), "from env".to_string());

        let literal_out = dir.path().join("literal.txt");
        let user_out = dir.path().join("user.txt");
        let quote = |path: &std::path::Path| quote_arg(&shell, &path.to_string_lossy());
        let body = if cfg!(windows) {
            format!(
                "@set RB_TEST_LITERAL> {}\n@set RB_TEST_USER> {}\n",
                quote(&literal_out),
                quote(&user_out)
            )
        } else {
            format!(
                "printf 'RB_TEST_LITERAL=%s\\n' \"$RB_TEST_LITERAL\" > {}\n\
                 printf 'RB_TEST_USER=%s\\n' \"$RB_TEST_USER\" > {}\n",
                quote(&literal_out),
                quote(&user_out)
            )
        };
        let script = dialect
            .scope_section(None, &env, &[("RB_TEST_LITERAL", literal)], None, &body)
            .unwrap();
        let path = dir.path().join(format!("literal.{}", shell.extension()));
        fs_err::write(
            &path,
            super::write_shell_script(shell.clone(), &format!("{script}\n")).unwrap(),
        )
        .unwrap();

        let runs: Vec<std::process::Command> = if cfg!(windows) {
            ["/v:off", "/v:on"]
                .into_iter()
                .map(|expansion| {
                    let mut command = std::process::Command::new("cmd.exe");
                    command.args(["/d", expansion, "/c"]).arg(&path);
                    command
                })
                .collect()
        } else {
            let mut command = std::process::Command::new("bash");
            command.arg(&path);
            vec![command]
        };
        for mut command in runs {
            let output = command.output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{command:?}: {stderr}");
            let read = |path: &std::path::Path| {
                fs_err::read_to_string(path).unwrap().trim_end().to_string()
            };
            assert_eq!(
                read(&literal_out),
                format!("RB_TEST_LITERAL={literal}"),
                "{command:?}"
            );
            assert_eq!(read(&user_out), "RB_TEST_USER=base-x", "{command:?}");
        }
    }

    /// A double quote would end the quoted cmd assignment and let the rest
    /// of the value run as commands, so a literal value with one is
    /// rejected rather than written.
    #[test]
    fn cmd_literal_env_rejects_double_quotes() {
        let mut out = String::new();
        let err = shell_dialect(Platform::Win64)
            .set_literal_env_var(&mut out, "P", "a\" & echo injected & \"b")
            .expect_err("a double quote cannot be set literally in cmd");
        assert!(err.to_string().contains("double quote"), "{err}");
        assert!(out.is_empty(), "{out}");
    }

    #[test]
    fn shell_dialect_follows_the_platform() {
        // Independent of the host this test runs on.
        assert_eq!(shell_dialect(Platform::Win64).shell().extension(), "bat");
        assert_eq!(shell_dialect(Platform::Linux64).shell().extension(), "sh");
        assert_eq!(shell_dialect(Platform::OsxArm64).shell().extension(), "sh");

        assert_eq!(shell_dialect(Platform::Win64).default_interpreter(), "cmd");
        assert_eq!(
            shell_dialect(Platform::Linux64).default_interpreter(),
            "bash"
        );
    }

    #[test]
    fn cmd_switches_between_supported_windows_architectures() {
        let script = std::path::Path::new("work/conda_build.bat");
        let work_dir = std::path::Path::new("work");
        let dialect = shell_dialect(Platform::Win64);

        let x64_to_arm = ExecutionContext::shared(
            RuntimeEnv::for_test(Platform::Win64),
            "prefix",
            Platform::WinArm64,
            Platform::WinArm64,
        );
        let arm_command = dialect.command_to_run_script(script, work_dir, &x64_to_arm);
        assert_eq!(arm_command.program, "cmd.exe");
        assert_eq!(arm_command.args[..3], ["/d", "/v:on", "/c"]);
        assert!(arm_command.args[3].contains("/machine arm64"));
        assert!(arm_command.args[3].contains("conda_build.bat"));
        assert!(arm_command.args[3].contains("exit /b !ERRORLEVEL!"));

        let spaced_script = std::path::Path::new("work/conda build.bat");
        assert!(
            dialect
                .command_to_run_script(spaced_script, work_dir, &x64_to_arm)
                .args[3]
                .contains(r#"cmd.exe /d /c "conda build.bat""#)
        );

        let arm_to_x64 = ExecutionContext::shared(
            RuntimeEnv::for_test(Platform::WinArm64),
            "prefix",
            Platform::Win64,
            Platform::Win64,
        );
        let x64_command = dialect.command_to_run_script(script, work_dir, &arm_to_x64);
        assert!(x64_command.args[3].contains("/machine amd64"));

        let x64_to_x86 = ExecutionContext::shared(
            RuntimeEnv::for_test(Platform::Win64),
            "prefix",
            Platform::Win32,
            Platform::Win32,
        );
        let x86_command = dialect.command_to_run_script(script, work_dir, &x64_to_x86);
        assert!(x86_command.args[3].contains("/machine x86"));
        assert!(
            x86_command.args[3].contains(r"%SystemRoot%\SysWOW64\cmd.exe"),
            "x86 must launch the SysWOW64 command interpreter: {}",
            x86_command.args[3]
        );

        let same_arch = ExecutionContext::shared(
            RuntimeEnv::for_test(Platform::Win64),
            "prefix",
            Platform::Win64,
            Platform::Win64,
        );
        assert_eq!(
            dialect
                .command_to_run_script(script, work_dir, &same_arch)
                .args,
            ["/d", "/c", "work/conda_build.bat"]
        );
        assert_eq!(
            windows_machine_transition(Platform::Win64, Platform::Win32),
            Some(WindowsMachine::X86)
        );
        assert_eq!(
            windows_machine_transition(Platform::Win32, Platform::Win64),
            None
        );
        assert_eq!(
            windows_machine_transition(Platform::Win32, Platform::WinArm64),
            None
        );
        assert_eq!(WindowsMachine::X86.wow64_processor_architecture(), None);
        assert_eq!(
            WindowsMachine::Amd64.wow64_processor_architecture(),
            Some("AMD64")
        );
        assert_eq!(
            WindowsMachine::Arm64.wow64_processor_architecture(),
            Some("ARM64")
        );
        assert_eq!(
            windows_machine_transition(Platform::Win32, Platform::Win32),
            None
        );
        assert_eq!(
            windows_machine_transition(Platform::Linux64, Platform::Win32),
            None
        );
    }

    #[test]
    fn bash_preamble_enables_tracing_after_activation() {
        let preamble =
            shell_dialect(Platform::Linux64).preamble(Some(std::path::Path::new("build_env.sh")));
        let activation = preamble
            .find("source")
            .expect("preamble sources activation");
        let trace = preamble.find("set -x").expect("preamble enables tracing");
        // `set -x` must come after activation so the sourced environment setup
        // (which may expand secrets) is not traced (#2264).
        assert!(
            trace > activation,
            "set -x must follow activation, got:\n{preamble}"
        );
    }

    #[test]
    fn quotes_only_when_needed() {
        let bash = shell::Bash::default().into();
        // No whitespace: left untouched (flags must not be quoted).
        assert_eq!(quote_arg(&bash, "-NoLogo"), "-NoLogo");
        assert_eq!(quote_arg(&bash, "/usr/bin/python"), "/usr/bin/python");
        // Whitespace and metacharacters are single-quoted for posix shells.
        assert_eq!(
            quote_arg(&bash, "/opt/my tools/node"),
            "'/opt/my tools/node'"
        );
        assert_eq!(quote_arg(&bash, "/tmp/a&b"), "'/tmp/a&b'");
        // Embedded single quote is escaped.
        assert_eq!(quote_arg(&bash, "a'b c"), "'a'\\''b c'");
    }

    #[test]
    fn quotes_for_cmd_with_double_quotes() {
        let cmd = shell::CmdExe.into();
        assert_eq!(quote_arg(&cmd, "/d"), "/d");
        assert_eq!(
            quote_arg(&cmd, r"C:\Program Files\nodejs\node.exe"),
            "\"C:\\Program Files\\nodejs\\node.exe\""
        );
        assert_eq!(quote_arg(&cmd, r"C:\tmp\a&b"), "\"C:\\tmp\\a&b\"");
        assert_eq!(quote_arg(&cmd, r"C:\tmp\a;b"), "\"C:\\tmp\\a;b\"");
        assert_eq!(quote_arg(&cmd, r"C:\tmp\a,b"), "\"C:\\tmp\\a,b\"");
        assert_eq!(quote_arg(&cmd, r"C:\tmp\a=b"), "\"C:\\tmp\\a=b\"");
        assert_eq!(
            quote_arg(&cmd, r"C:\tmp\%NO_SUCH_VAR%\script.bat"),
            r"C:\tmp\%%NO_SUCH_VAR%%\script.bat"
        );
        assert_eq!(
            quote_arg(&cmd, r"C:\tmp\%NO_SUCH_VAR% dir\script.bat"),
            r#""C:\tmp\%%NO_SUCH_VAR%% dir\script.bat""#
        );
    }

    /// The replay guards run in the native shell of this machine: a step
    /// manifest has to match its recorded copy byte for byte, a trailing
    /// newline and NUL bytes included, or be missing or empty when none was
    /// recorded.
    /// Otherwise the script stops with status 1 and prints the message as
    /// written, even with characters the shell would otherwise interpret,
    /// and paths with such characters are compared all the same. Removing
    /// declaration files succeeds whether or not they exist.
    #[test]
    fn replay_guards_compare_manifests_byte_for_byte() {
        let dialect = shell_dialect(Platform::current());
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("steps (x86) & 100%.json");
        let inputs = dir.path().join("inputs.json");
        let recorded = dir.path().join("recorded (x86) & 100%.json");
        let message = "Build step `g` (x86) & 100% <changed> declared different steps";
        let recorded_bytes = b"{\"version\": 1}\n";
        fs_err::write(&recorded, recorded_bytes).unwrap();

        let run = |recorded: Option<&std::path::Path>, written: Option<&[u8]>| {
            match written {
                Some(bytes) => fs_err::write(&manifest, bytes).unwrap(),
                None if manifest.exists() => fs_err::remove_file(&manifest).unwrap(),
                None => {}
            }
            fs_err::write(&inputs, b"{}").unwrap();
            let script = format!(
                "{}{}",
                dialect.remove_files(&[inputs.as_path()]),
                dialect.check_step_manifest(&manifest, recorded, message)
            );
            let path = dir
                .path()
                .join(format!("guard.{}", dialect.shell().extension()));
            fs_err::write(
                &path,
                super::write_shell_script(dialect.shell(), &script).unwrap(),
            )
            .unwrap();
            let output = if cfg!(windows) {
                std::process::Command::new("cmd.exe")
                    .args(["/d", "/c"])
                    .arg(&path)
                    .output()
            } else {
                std::process::Command::new("bash").arg(&path).output()
            }
            .unwrap();
            assert!(!inputs.exists(), "the input report was not removed");
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            (output.status.code(), stderr)
        };

        let recorded = Some(recorded.as_path());
        let passes = [
            ("identical manifest", recorded, Some(&recorded_bytes[..])),
            ("nothing recorded, no manifest", None, None),
            ("nothing recorded, empty manifest", None, Some(&b""[..])),
        ];
        for (case, recorded, written) in passes {
            let (status, stderr) = run(recorded, written);
            assert_eq!(status, Some(0), "{case}: {stderr}");
            assert!(!stderr.contains("declared different"), "{case}: {stderr}");
        }

        let stops = [
            (
                "trailing newline missing",
                recorded,
                Some(&b"{\"version\": 1}"[..]),
            ),
            (
                "NUL byte inserted",
                recorded,
                Some(&b"{\"version\": 1}\0\n"[..]),
            ),
            ("manifest missing", recorded, None),
            ("nothing recorded, manifest written", None, Some(&b"{}"[..])),
        ];
        for (case, recorded, written) in stops {
            let (status, stderr) = run(recorded, written);
            assert_eq!(status, Some(1), "{case}: {stderr}");
            assert!(stderr.contains(message), "{case}: {stderr}");
        }
    }
}
