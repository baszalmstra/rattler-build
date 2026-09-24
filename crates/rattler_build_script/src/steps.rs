//! Independent execution of build steps.
//!
//! The combined host/build activation runs once in the native wrapper shell,
//! in the work directory. The shell dumps its exported environment right
//! before and right after activation; applying what differs between the two
//! dumps to the original process environment yields the activated
//! environment. Every step then runs as a separate native wrapper process,
//! started in the work directory from that environment plus the step's own
//! `env` and `cwd`. Shell-local activation state (unexported variables,
//! functions, options) does not reach the steps, and nothing a step changes
//! reaches later steps.
//!
//! Values activation leaves alone are passed on exactly as they were. On
//! Unix the dump (`env -0`) keeps every byte of the values activation sets.
//! cmd.exe dumps one `NAME=VALUE` line per variable (`SET`), so a value that
//! activation sets or changes to contain a line break is cut at it, and its
//! further lines can show up as variables of their own.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use indexmap::IndexMap;
use rattler_shell::shell::{Shell, ShellEnum};

use crate::{
    InterpreterError,
    execution::{
        ExecutionArgs, run_process_with_replacements, script_generation_error,
        write_activation_script, write_native_wrapper, write_wrapper_script,
    },
    runner::resolve_process_env,
    shell_dialect::{ShellDialect, quote_arg, shell_dialect, write_shell_script},
};

/// Directory in the work directory that holds one artifact directory per step.
const STEP_ARTIFACTS_DIR: &str = "conda_build_steps";

/// Variable names and values of a process environment, as the OS stores them.
type ProcessEnv = IndexMap<OsString, OsString>;

/// Runs every section of `exec_args` as an independent build step.
///
/// Activation runs once, in the work directory, and the environment it
/// exports is captured. Each section then runs, in order, in its own process
/// started from that environment; its `env` applies to that section only and
/// it runs in its `cwd` resolved against the work directory, or in the work
/// directory when it has none. The first failing section stops execution and
/// is named in the returned error.
///
/// The scripts run are the ones [`create_steps_script`] writes.
pub async fn run_steps(exec_args: ExecutionArgs) -> Result<(), InterpreterError> {
    let dialect = shell_dialect(exec_args.context.runtime().process_platform());
    let scripts = write_step_scripts(&exec_args, dialect.as_ref()).await?;
    let launcher = Launcher::new(&exec_args, dialect.as_ref());

    let process_env = resolve_process_env(
        exec_args.env_isolation,
        &exec_args.env_vars,
        &exec_args.secrets,
        exec_args.context.runtime(),
    );
    let activated_env = capture_activated_env(&launcher, &scripts.activation, &process_env).await?;

    for ((position, section), step_script) in
        exec_args.sections.iter().enumerate().zip(&scripts.steps)
    {
        let status = launcher.run(step_script, &activated_env).await?;
        if !status.success() {
            let label = section
                .label
                .clone()
                .unwrap_or_else(|| format!("step {position}"));
            return Err(launcher.failed(&format!("Build {label}"), status, Some(step_script)));
        }
    }

    Ok(())
}

/// Writes the scripts of a build with steps without running them.
///
/// Next to the activation script `build_env.<ext>`, every step gets its own
/// wrapper `conda_build_steps/step_<index>/conda_build.<ext>`, together with
/// its interpreter scripts. A step wrapper never activates: it runs its step
/// in the environment it is started in, which must already be activated.
///
/// The work directory's `conda_build.<ext>` replays all steps the way
/// [`run_steps`] runs them, however it is started: on Windows it first
/// restarts itself in the build's architecture when that differs from the
/// architecture of rattler-build. It then activates once, unless the
/// environment is already activated, and starts every step wrapper in order
/// as a separate process of the native shell in that architecture, so each
/// step sees only the exported activated environment. The first failing step
/// stops it with that step's status, even when activation turned `set -e`
/// off.
pub async fn create_steps_script(exec_args: ExecutionArgs) -> Result<(), std::io::Error> {
    let dialect = shell_dialect(exec_args.context.runtime().process_platform());
    let scripts = write_step_scripts(&exec_args, dialect.as_ref())
        .await
        .map_err(script_generation_error)?;

    tracing::info!("Build script created at {}", scripts.replay.display());
    Ok(())
}

/// The generated scripts of a build with steps.
struct StepScripts {
    /// The combined activation script `build_env.<ext>`.
    activation: PathBuf,
    /// The wrapper of every step, in order.
    steps: Vec<PathBuf>,
    /// The wrapper replaying all steps, `conda_build.<ext>`.
    replay: PathBuf,
}

/// Writes the activation script, the step wrappers and the replay wrapper
/// described by [`create_steps_script`].
async fn write_step_scripts(
    args: &ExecutionArgs,
    dialect: &dyn ShellDialect,
) -> Result<StepScripts, InterpreterError> {
    let activation = write_activation_script(args, dialect).await?;

    let steps_dir = args.work_dir.join(STEP_ARTIFACTS_DIR);
    let mut steps = Vec::with_capacity(args.sections.len());
    let mut replay_fragments = Vec::with_capacity(args.sections.len());
    for (position, section) in args.sections.iter().enumerate() {
        let step_dir = steps_dir.join(format!("step_{position}"));
        tokio::fs::create_dir_all(&step_dir).await?;
        let step_script = write_wrapper_script(
            args,
            dialect,
            &step_dir,
            None,
            std::slice::from_ref(section),
            Some(&args.work_dir),
        )
        .await?;
        replay_fragments.push(dialect.child_script_command(&step_script, &args.context));
        steps.push(step_script);
    }

    let replay = args
        .work_dir
        .join(format!("conda_build.{}", dialect.shell().extension()));
    let replay_preamble = dialect.replay_preamble(&replay, &activation, &args.context);
    write_native_wrapper(dialect, &replay, &replay_preamble, &replay_fragments).await?;

    Ok(StepScripts {
        activation,
        steps,
        replay,
    })
}

/// Runs activation once in the native wrapper shell, in the work directory,
/// and returns the environment it leaves behind for the steps.
///
/// The shell dumps its exported environment right before and right after
/// activation into files in a temporary directory in the work directory
/// (writable under the sandbox), so the dumps never reach the build log; any
/// other output of the activation is logged as usual. Variables whose value
/// differs between the dumps are set in `process_env`, the environment the
/// shell started with, and variables missing after activation are removed
/// from it. The temporary directory is removed when this function returns.
async fn capture_activated_env(
    launcher: &Launcher<'_>,
    activation_script_path: &Path,
    process_env: &IndexMap<String, String>,
) -> Result<ProcessEnv, InterpreterError> {
    let shell = launcher.dialect.shell();
    let capture_dir = tempfile::Builder::new()
        .prefix("conda_build_env")
        .tempdir_in(&launcher.args.work_dir)?;
    let before_path = capture_dir.path().join("before");
    let after_path = capture_dir.path().join("after");

    let mut print_env = String::new();
    shell
        .print_env(&mut print_env)
        .map_err(std::io::Error::other)?;
    let dump_env_to = |path: &Path| {
        format!(
            "{} > {}\n",
            print_env.trim_end(),
            quote_arg(&shell, &path.to_string_lossy())
        )
    };
    // cmd.exe writes the dump in the console code page: switch to UTF-8
    // before the first dump, as the preamble does before activation.
    let mut capture_script = String::new();
    shell
        .force_utf8(&mut capture_script)
        .map_err(std::io::Error::other)?;
    capture_script.push_str(&dump_env_to(&before_path));
    // The preamble activates exactly like the single build wrapper does.
    capture_script.push_str(&launcher.dialect.preamble(Some(activation_script_path)));
    capture_script.push_str(&dump_env_to(&after_path));

    let capture_script_path = capture_dir
        .path()
        .join(format!("capture_env.{}", shell.extension()));
    tokio::fs::write(
        &capture_script_path,
        write_shell_script(shell.clone(), &capture_script)?,
    )
    .await?;

    let status = launcher.run(&capture_script_path, process_env).await?;
    if !status.success() {
        return Err(launcher.failed("Activation", status, None));
    }

    let before = parse_env_dump(&shell, &read_env_dump(&before_path).await?);
    let after = parse_env_dump(&shell, &read_env_dump(&after_path).await?);
    Ok(apply_activation(process_env, &before, &after))
}

/// Reads an environment dump written by the activation capture.
async fn read_env_dump(path: &Path) -> std::io::Result<Vec<u8>> {
    tokio::fs::read(path).await.map_err(|err| {
        std::io::Error::new(
            err.kind(),
            format!(
                "activation did not write its environment dump {}: {err}",
                path.display()
            ),
        )
    })
}

/// Parses an environment dump written by the shell's `print_env`.
fn parse_env_dump(shell: &ShellEnum, dump: &[u8]) -> ProcessEnv {
    match shell {
        // `SET` prints one `NAME=VALUE` line per variable, with the value
        // verbatim (unlike `Shell::parse_env`, which strips quotes).
        ShellEnum::CmdExe(_) => String::from_utf8_lossy(dump)
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
        // `env -0` terminates every `NAME=VALUE` record with NUL.
        _ => dump
            .split(|&byte| byte == 0)
            .filter_map(|record| {
                let split = record.iter().position(|&byte| byte == b'=')?;
                (split > 0).then(|| (os_string(&record[..split]), os_string(&record[split + 1..])))
            })
            .collect(),
    }
}

/// Converts dumped bytes to an OS string without altering them.
#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes).to_owned()
}

/// Converts dumped bytes to an OS string. Windows only parses cmd.exe dumps,
/// which are UTF-8, so this conversion is not used there in practice.
#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    String::from_utf8_lossy(bytes).into_owned().into()
}

/// Applies what activation changed between the `before` and `after` dumps
/// to `process_env`, the environment the activating shell was started with.
///
/// Only variables whose dumped value differs are taken from the dumps; every
/// other variable keeps its original value. Shell bookkeeping that is the
/// same in both dumps, and further lines of an unchanged multi-line value
/// that a cmd.exe dump shows as variables, therefore never reach the steps.
fn apply_activation(
    process_env: &IndexMap<String, String>,
    before: &ProcessEnv,
    after: &ProcessEnv,
) -> ProcessEnv {
    let mut env: ProcessEnv = process_env
        .iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
    for name in before.keys().filter(|name| !after.contains_key(*name)) {
        remove_var(&mut env, name);
    }
    for (name, value) in after {
        if before.get(name) != Some(value) {
            remove_var(&mut env, name);
            env.insert(name.clone(), value.clone());
        }
    }
    env
}

/// Removes `name` from `env`, case-insensitively on Windows, where the
/// process environment compares variable names that way.
fn remove_var(env: &mut ProcessEnv, name: &OsStr) {
    if cfg!(windows) {
        env.retain(|key, _| !key.eq_ignore_ascii_case(name));
    } else {
        env.shift_remove(name);
    }
}

/// Launches generated native wrapper scripts of one build.
struct Launcher<'a> {
    args: &'a ExecutionArgs,
    dialect: &'a dyn ShellDialect,
    /// The native wrapper shell, resolved before activation.
    program: String,
    replacements: HashMap<String, String>,
}

impl<'a> Launcher<'a> {
    fn new(args: &'a ExecutionArgs, dialect: &'a dyn ShellDialect) -> Self {
        // Resolve the wrapper shell on the runtime `PATH` once, so an
        // activation that changes `PATH` (for example to a prefix with its
        // own, possibly foreign, `bash`) cannot change the shell the steps
        // run in. When it cannot be resolved, the spawn looks it up.
        let shell = dialect.shell().executable().to_string();
        let program = which::which_in(&shell, Some(args.context.runtime().path()), &args.work_dir)
            .ok()
            .and_then(|path| path.into_os_string().into_string().ok())
            .unwrap_or(shell);
        Self {
            args,
            dialect,
            program,
            replacements: args.replacements(dialect.replacements_template()),
        }
    }

    /// Runs the native script `script_path` with exactly `process_env`,
    /// starting in the work directory, and logs its output to the build log.
    ///
    /// The launch goes through the dialect, which keeps the Windows
    /// `/machine` architecture selection and the sandbox policy.
    async fn run<K: AsRef<OsStr>, V: AsRef<OsStr>>(
        &self,
        script_path: &Path,
        process_env: impl IntoIterator<Item = (K, V)>,
    ) -> Result<ExitStatus, InterpreterError> {
        let mut command_spec = self.dialect.command_to_run_script(
            script_path,
            &self.args.work_dir,
            &self.args.context,
        );
        command_spec.program.clone_from(&self.program);
        let sandbox_config = if self.dialect.supports_sandbox() {
            self.args.sandbox_config.as_ref()
        } else {
            None
        };
        let output = run_process_with_replacements(
            &command_spec,
            &self.args.work_dir,
            &self.replacements,
            process_env,
            sandbox_config,
            self.args.context.runtime(),
        )
        .await?;
        Ok(output.status)
    }

    /// Logs and builds the error for a failed activation or step process.
    fn failed(
        &self,
        what: &str,
        status: ExitStatus,
        step_script: Option<&Path>,
    ) -> InterpreterError {
        let status_code = status.code().unwrap_or(1);
        let step_script = step_script
            .map(|path| {
                format!(
                    "\n\n  Step script: {}\n\n\
                     The step script runs only this step and does not activate the build\n\
                     environment: to rerun the step, run it with the native shell in the\n\
                     build environment (see below). The build script reruns all steps.",
                    path.display()
                )
            })
            .unwrap_or_default();
        let debug_info = self
            .dialect
            .debug_info(&self.args.work_dir, &self.args.context);
        tracing::error!("{what} failed with status {status_code}{step_script}");
        tracing::error!("{debug_info}");
        InterpreterError::ExecutionFailed(std::io::Error::other(format!(
            "{what} failed with status {status_code}{step_script}{debug_info}"
        )))
    }
}
