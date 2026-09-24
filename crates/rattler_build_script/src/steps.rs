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
//! in its environment reaches other steps.
//!
//! Values activation leaves alone are passed on exactly as they were. On
//! Unix the dump (`env -0`) keeps every byte of the values activation sets.
//! cmd.exe dumps one `NAME=VALUE` line per variable (`SET`), so a value that
//! activation sets or changes to contain a line break is cut at it, and its
//! further lines can show up as variables of their own.
//!
//! The [`StepGraph`] of the steps decides when each step starts; it is
//! validated before anything is written or activated. A step that declares
//! neither inputs nor outputs is a barrier: it starts once every step before
//! it has finished, and no later step starts before it has finished. Between
//! barriers, a declared step starts as soon as the producers of its inputs
//! and the steps it `depends_on` have succeeded, with at most as many steps
//! running as the machine has cores. A declared input that no step produces
//! must exist when its step starts. Right before a step starts, its declared
//! outputs are cleared without following symbolic links, so they have to be
//! created by the step itself and exist when it succeeds: in the work
//! directory whatever an earlier build left there is removed, while in the
//! host and build prefixes, which hold installed packages, an existing
//! output fails the step unless it is an empty directory. Declared outputs
//! may not claim the files rattler-build writes in the work directory, and
//! a build with declared steps fails before anything runs when its work
//! directory is, contains, or lies inside the host or build prefix. As
//! clearing a directory tree output would also remove a step's working
//! directory at or inside it, a step whose `cwd` (or the work directory,
//! when it has none) is or lies inside one of its own tree outputs fails
//! the build before anything runs as well; it may run in the parent of the
//! tree instead.
//!
//! The first step that fails stops further steps from starting. Steps that
//! are already running are waited for, not killed, and the first failure is
//! the error returned. Dropping the future of a running build does not kill
//! step processes that are still running, just as it does not kill the
//! process of a `build.script`.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{FileType, Metadata};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;

use futures::stream::{FuturesUnordered, StreamExt};
use indexmap::IndexMap;
use rattler_shell::shell::{Shell, ShellEnum};

use crate::{
    InterpreterError, PlannedOutput, StepGraph, StepOutputKind, StepPath, StepRoot,
    execution::{
        BuildScriptSection, ExecutionArgs, run_process_with_replacements, script_generation_error,
        write_activation_script, write_native_wrapper, write_wrapper_script,
    },
    runner::resolve_process_env,
    shell_dialect::{ShellDialect, quote_arg, shell_dialect, write_shell_script},
};

/// Directory in the work directory that holds one artifact directory per step.
const STEP_ARTIFACTS_DIR: &str = "conda_build_steps";

/// Files in the work directory that rattler-build writes for a build with
/// steps, on any platform: the build log, the activation scripts and the
/// replay wrappers.
const ENGINE_FILES: [&str; 5] = [
    "conda_build.log",
    "build_env.sh",
    "build_env.bat",
    "conda_build.sh",
    "conda_build.bat",
];

/// Variable names and values of a process environment, as the OS stores them.
type ProcessEnv = IndexMap<OsString, OsString>;

/// Runs every section of `exec_args` as an independent build step.
///
/// The step graph of the sections is validated first; an invalid graph fails
/// before any script is written or activation runs. Activation then runs
/// once, in the work directory, and the environment it exports is captured.
/// Each section runs in its own process started from that environment, when
/// the step graph lets it start (see the module documentation); its `env`
/// applies to that section only and it runs in its `cwd` resolved against
/// the work directory, or in the work directory when it has none. The first
/// failing section stops further sections from starting and is named in the
/// returned error once the sections still running have finished.
///
/// The scripts run are the ones [`create_steps_script`] writes.
pub async fn run_steps(exec_args: ExecutionArgs) -> Result<(), InterpreterError> {
    let graph = step_graph(&exec_args)?;
    let dialect = shell_dialect(exec_args.context.runtime().process_platform());
    let scripts = write_step_scripts(&exec_args, dialect.as_ref(), &graph).await?;
    let launcher = Launcher::new(&exec_args, dialect.as_ref());

    let process_env = resolve_process_env(
        exec_args.env_isolation,
        &exec_args.env_vars,
        &exec_args.secrets,
        exec_args.context.runtime(),
    );
    let activated_env = capture_activated_env(&launcher, &scripts.activation, &process_env).await?;

    StepRunner {
        launcher: &launcher,
        graph: &graph,
        step_scripts: &scripts.steps,
        activated_env: &activated_env,
    }
    .run_all()
    .await
}

/// Writes the scripts of a build with steps without running them.
///
/// The step graph of the sections is validated first; an invalid graph
/// fails before any script is written. Next to the activation script
/// `build_env.<ext>`, every step gets its own wrapper
/// `conda_build_steps/step_<index>/conda_build.<ext>`, together with its
/// interpreter scripts. A step wrapper never activates: it runs its step in
/// the environment it is started in, which must already be activated.
///
/// The work directory's `conda_build.<ext>` replays all steps, however it is
/// started: on Windows it first restarts itself in the build's architecture
/// when that differs from the architecture of rattler-build. It then
/// activates once, unless the environment is already activated, and starts
/// every step wrapper as a separate process of the native shell in that
/// architecture, so each step sees only the exported activated environment.
/// Unlike [`run_steps`], the replay runs one step at a time, in the
/// topological order of the step graph, so it keeps the order of the steps
/// but not the overlap of steps that run concurrently in a build. It does
/// not check declared inputs and outputs. The first failing step stops it
/// with that step's status, even when activation turned `set -e` off.
pub async fn create_steps_script(exec_args: ExecutionArgs) -> Result<(), std::io::Error> {
    let graph = step_graph(&exec_args).map_err(script_generation_error)?;
    let dialect = shell_dialect(exec_args.context.runtime().process_platform());
    let scripts = write_step_scripts(&exec_args, dialect.as_ref(), &graph)
        .await
        .map_err(script_generation_error)?;

    tracing::info!("Build script created at {}", scripts.replay.display());
    Ok(())
}

/// Validates the step declarations of the sections of `args` and plans
/// their order, with path identities matched the way the file system of the
/// platform the steps run on compares them, and `host` and `build` paths
/// naming the same files when the build shares one prefix.
///
/// As a step's outputs are cleared before it starts, declared outputs must
/// also leave alone the files rattler-build writes in the work directory
/// itself, and with declared steps the work directory may not be, contain,
/// or lie inside a prefix, where clearing a `work` output would remove
/// installed files. For the same reason, no step may run in, or inside,
/// one of its own directory tree outputs.
fn step_graph(args: &ExecutionArgs) -> Result<StepGraph, InterpreterError> {
    let invalid = |message: String| {
        InterpreterError::ExecutionFailed(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid build steps: {message}"),
        ))
    };
    let platform = args.context.runtime().process_platform();
    let graph = StepGraph::new(
        args.sections.iter().map(|section| &section.graph),
        platform,
        args.context.layout(),
    )
    .map_err(|err| invalid(err.to_string()))?;

    // The graph identifies paths case-insensitively on Windows and macOS,
    // whose file systems usually compare names that way.
    let case_insensitive = platform.is_windows() || platform.is_osx();
    let names = |name: &str, reserved: &str| {
        if case_insensitive {
            name.to_lowercase() == reserved
        } else {
            name == reserved
        }
    };
    for (step, section) in args.sections.iter().enumerate() {
        for output in graph.outputs(step) {
            let path = output.path();
            let top_level = path.as_str().split('/').next().unwrap_or_default();
            let reserved = path.root() == StepRoot::Work
                && (names(top_level, STEP_ARTIFACTS_DIR)
                    || ENGINE_FILES.iter().any(|&file| names(path.as_str(), file)));
            if reserved {
                return Err(invalid(format!(
                    "{} declares the output `{path}`, which rattler-build writes itself; \
                     declare another path",
                    step_name(section, step)
                )));
            }
        }
    }

    if (0..graph.len()).any(|step| !graph.is_barrier(step)) {
        let work_dir = canonical_root(&args.work_dir)?;
        let prefixes = [
            ("host prefix", args.context.host().path()),
            ("build prefix", args.context.build().path()),
        ];
        for (name, prefix) in prefixes {
            let prefix_dir = canonical_root(prefix)?;
            if nested(&work_dir, &prefix_dir, case_insensitive)
                || nested(&prefix_dir, &work_dir, case_insensitive)
            {
                return Err(invalid(format!(
                    "the work directory {} and the {name} {} are the same directory or one \
                     contains the other, so clearing the declared outputs of a step could \
                     remove installed files; use separate directories for declared build steps",
                    args.work_dir.display(),
                    prefix.display()
                )));
            }
        }
    }

    // Clearing a tree output right before its step starts would remove the
    // directory the step is about to run in when that is the tree or lies
    // inside it, and the step would fail only after the steps before it
    // ran. Both sides are compared where they physically are, as far as
    // they exist, so links and other names of the same directory count too.
    // The tree is not created in advance instead: a step that creates
    // nothing would then seem to have produced its output.
    let windows = platform.is_windows();
    for (step, section) in args.sections.iter().enumerate() {
        let mut trees = graph
            .outputs(step)
            .iter()
            .filter(|output| matches!(output.kind(), StepOutputKind::Tree))
            .peekable();
        if trees.peek().is_none() {
            continue;
        }
        let cwd = match &section.cwd {
            Some(cwd) => args.work_dir.join(cwd),
            None => args.work_dir.clone(),
        };
        let physical_cwd = canonical_root(&lexical_path(&cwd, windows))?;
        for output in trees {
            let path = output.path();
            let physical_output = output_location(&path.resolve(root_dir(args, path.root())))?;
            if nested(&physical_output, &physical_cwd, case_insensitive) {
                return Err(invalid(format!(
                    "{} runs in {}, which is or lies inside its declared output `{path}`; \
                     outputs are cleared right before their step starts, which would remove \
                     the directory the step runs in, so run it outside the output, for \
                     example in its parent directory",
                    step_name(section, step),
                    cwd.display()
                )));
            }
        }
    }
    Ok(graph)
}

/// Returns the directory of `root` in the build of `args`.
fn root_dir(args: &ExecutionArgs, root: StepRoot) -> &Path {
    match root {
        StepRoot::Work => args.work_dir.as_path(),
        StepRoot::Host => args.context.host().path(),
        StepRoot::Build => args.context.build().path(),
    }
}

/// Returns `path` with its `.` and `..` components resolved by name, the
/// way the step shells change into a `cwd`. With `windows` set, a name
/// also loses what Windows ignores when it opens it: an alternate data
/// stream suffix from the first `:` on, and trailing dots and spaces.
fn lexical_path(path: &Path, windows: bool) -> PathBuf {
    let mut normalized = PathBuf::new();
    // The number of trailing names in `normalized` that `..` can remove.
    let mut names = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir if names > 0 => {
                normalized.pop();
                names -= 1;
            }
            Component::ParentDir if normalized.has_root() => {}
            Component::ParentDir => normalized.push(component),
            Component::Normal(name) => {
                let name = match name.to_str() {
                    Some(text) if windows => OsStr::new(
                        text.split(':')
                            .next()
                            .unwrap_or_default()
                            .trim_end_matches(['.', ' ']),
                    ),
                    _ => name,
                };
                if !name.is_empty() {
                    normalized.push(name);
                    names += 1;
                }
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(Component::CurDir);
    }
    normalized
}

/// Returns where the entry that clearing an output at `path` removes
/// physically is, as far as it exists. A symbolic link at `path` itself is
/// not followed, as clearing removes the link and not what it points to.
fn output_location(path: &Path) -> io::Result<PathBuf> {
    match (
        fs_err::symlink_metadata(path),
        path.parent(),
        path.file_name(),
    ) {
        (Ok(metadata), Some(parent), Some(name)) if metadata.file_type().is_symlink() => {
            Ok(canonical_root(parent)?.join(name))
        }
        _ => canonical_root(path),
    }
}

/// Returns `path` with every symbolic link resolved, as far as it exists:
/// the part below its deepest existing ancestor is appended as given. A
/// relative path is resolved against the current directory.
fn canonical_root(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match fs_err::canonicalize(existing) {
            Ok(mut canonical) => {
                canonical.extend(missing.iter().rev());
                return Ok(canonical);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(io::Error::new(
                        err.kind(),
                        format!("cannot resolve {}: {err}", path.display()),
                    ));
                };
                missing.push(name);
                existing = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
            }
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("cannot resolve {}: {err}", path.display()),
                ));
            }
        }
    }
}

/// Whether `inner` is `outer` or lies inside it, comparing the names of
/// their components case-insensitively if `case_insensitive` is set.
fn nested(outer: &Path, inner: &Path, case_insensitive: bool) -> bool {
    let mut inner = inner.components();
    outer.components().all(|component| {
        inner.next().is_some_and(|other| {
            if case_insensitive {
                component.as_os_str().to_string_lossy().to_lowercase()
                    == other.as_os_str().to_string_lossy().to_lowercase()
            } else {
                component == other
            }
        })
    })
}

/// Names step `step`, whose section is `section`, in the log: its label,
/// with its id if it has one.
fn step_name(section: &BuildScriptSection, step: usize) -> String {
    let label = section
        .label
        .clone()
        .unwrap_or_else(|| format!("step {step}"));
    match &section.graph.id {
        Some(id) => format!("{label} (`{id}`)"),
        None => label,
    }
}

/// The generated scripts of a build with steps.
struct StepScripts {
    /// The combined activation script `build_env.<ext>`.
    activation: PathBuf,
    /// The wrapper of every step, by step index.
    steps: Vec<PathBuf>,
    /// The wrapper replaying all steps, `conda_build.<ext>`.
    replay: PathBuf,
}

/// Writes the activation script, the step wrappers and the replay wrapper
/// described by [`create_steps_script`].
async fn write_step_scripts(
    args: &ExecutionArgs,
    dialect: &dyn ShellDialect,
    graph: &StepGraph,
) -> Result<StepScripts, InterpreterError> {
    let activation = write_activation_script(args, dialect).await?;

    let steps_dir = args.work_dir.join(STEP_ARTIFACTS_DIR);
    let mut steps = Vec::with_capacity(args.sections.len());
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
        steps.push(step_script);
    }

    let replay_fragments = graph
        .topological_order()
        .iter()
        .map(|&step| dialect.child_script_command(&steps[step], &args.context))
        .collect::<Vec<_>>();
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

/// Runs the steps of one build from their activated environment, in the
/// order their [`StepGraph`] allows.
struct StepRunner<'a> {
    launcher: &'a Launcher<'a>,
    graph: &'a StepGraph,
    /// The wrapper of every step, by step index.
    step_scripts: &'a [PathBuf],
    activated_env: &'a ProcessEnv,
}

/// Why a build step did not succeed.
enum StepFailure<'a> {
    /// A declared input that no step produces did not exist when the step
    /// was about to start.
    MissingInput(&'a StepPath),
    /// The step process could not be run, or a declared path could not be
    /// inspected.
    Error(InterpreterError),
    /// The step process exited unsuccessfully.
    Status(ExitStatus),
    /// What was at a declared output before the step started could not be
    /// cleared.
    OccupiedOutput(&'a PlannedOutput, io::Error),
    /// The step process succeeded without creating a declared output.
    MissingOutput(&'a PlannedOutput),
}

impl<'a> StepRunner<'a> {
    /// Runs every step once all its dependencies have succeeded, lowest step
    /// index first among the steps that can start, with at most as many
    /// steps running as the machine has cores.
    ///
    /// After the first failure no further step starts; the steps that are
    /// running are waited for, and the first failure is returned.
    async fn run_all(&self) -> Result<(), InterpreterError> {
        let graph = self.graph;
        let step_count = graph.len();
        let max_running = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let mut unfinished_dependencies = (0..step_count)
            .map(|step| graph.dependencies(step).len())
            .collect::<Vec<_>>();
        let mut ready = (0..step_count)
            .filter(|&step| unfinished_dependencies[step] == 0)
            .map(Reverse)
            .collect::<BinaryHeap<_>>();
        let mut running = FuturesUnordered::new();
        let mut succeeded = 0;
        let mut first_error = None;

        loop {
            if first_error.is_none() {
                while running.len() < max_running
                    && let Some(Reverse(step)) = ready.pop()
                {
                    running.push(async move { (step, self.run_step(step).await) });
                }
            }
            let Some((step, result)) = running.next().await else {
                break;
            };
            match result {
                Ok(()) => {
                    succeeded += 1;
                    for &dependent in graph.dependents(step) {
                        unfinished_dependencies[dependent] -= 1;
                        if unfinished_dependencies[dependent] == 0 {
                            ready.push(Reverse(dependent));
                        }
                    }
                }
                Err(failure) if first_error.is_none() => {
                    first_error = Some(self.report(step, failure));
                    if !running.is_empty() {
                        tracing::info!(
                            "Waiting for the {} build step(s) still running to finish",
                            running.len()
                        );
                    }
                }
                Err(failure) => {
                    tracing::error!(
                        "{} after an earlier build step failed",
                        self.describe(step, &failure)
                    );
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None if succeeded == step_count => Ok(()),
            None => Err(InterpreterError::ExecutionFailed(io::Error::other(
                format!(
                    "{} of {step_count} build steps never became ready to run",
                    step_count - succeeded
                ),
            ))),
        }
    }

    /// Runs step `step`, whose dependencies have all succeeded: checks that
    /// its declared inputs that no step produces exist, clears its declared
    /// outputs, runs its wrapper, and checks that it created its declared
    /// outputs.
    async fn run_step(&self, step: usize) -> Result<(), StepFailure<'a>> {
        let graph = self.graph;
        for input in graph.source_inputs(step) {
            let metadata = entry_metadata(&self.resolve(input), false)
                .await
                .map_err(|err| StepFailure::Error(err.into()))?;
            if metadata.is_none() {
                return Err(StepFailure::MissingInput(input));
            }
        }

        // Only what this step creates may satisfy its outputs. The graph
        // rejects declared inputs and other declared outputs at or below
        // them, so clearing them touches nothing another step declares.
        for output in graph.outputs(step) {
            clear_output(self.root_dir(output.path().root()), output)
                .await
                .map_err(|err| StepFailure::OccupiedOutput(output, err))?;
        }

        // Declared steps can overlap, so mark where each one starts and ends
        // in the log; barriers run alone and log as before.
        let declared = !graph.is_barrier(step);
        if declared {
            tracing::info!("Starting build {}", self.step_name(step));
        }
        let status = self
            .launcher
            .run(&self.step_scripts[step], self.activated_env)
            .await
            .map_err(StepFailure::Error)?;
        if !status.success() {
            return Err(StepFailure::Status(status));
        }

        for output in graph.outputs(step) {
            let path = self.resolve(output.path());
            let present = match output.kind() {
                StepOutputKind::File => entry_metadata(&path, false)
                    .await
                    .map(|metadata| metadata.is_some_and(|metadata| !metadata.is_dir())),
                StepOutputKind::Tree => entry_metadata(&path, true)
                    .await
                    .map(|metadata| metadata.is_some_and(|metadata| metadata.is_dir())),
            }
            .map_err(|err| StepFailure::Error(err.into()))?;
            if !present {
                return Err(StepFailure::MissingOutput(output));
            }
        }
        if declared {
            tracing::info!("Finished build {}", self.step_name(step));
        }
        Ok(())
    }

    /// Returns where `path` is in the file system of this build.
    fn resolve(&self, path: &StepPath) -> PathBuf {
        path.resolve(self.root_dir(path.root()))
    }

    /// Returns the directory of `root` in this build.
    fn root_dir(&self, root: StepRoot) -> &Path {
        root_dir(self.launcher.args, root)
    }

    /// Names step `step` in the log: its label, with its id if it has one.
    fn step_name(&self, step: usize) -> String {
        step_name(&self.launcher.args.sections[step], step)
    }

    /// Describes the failure of step `step` in one line.
    fn describe(&self, step: usize, failure: &StepFailure<'_>) -> String {
        let name = self.step_name(step);
        match failure {
            StepFailure::MissingInput(input) => format!(
                "Build {name} cannot start: its declared input `{input}` does not exist at {}, \
                 and no step declares it as an output",
                self.resolve(input).display()
            ),
            StepFailure::Error(err) => format!("Build {name} failed: {err}"),
            StepFailure::Status(status) => format!(
                "Build {name} failed with status {}",
                status.code().unwrap_or(1)
            ),
            StepFailure::OccupiedOutput(output, err) => format!(
                "Build {name} cannot start: its declared output {} `{}` at {} cannot be \
                 cleared: {err}",
                output_kind_name(output.kind()),
                output.path(),
                self.resolve(output.path()).display()
            ),
            StepFailure::MissingOutput(output) => format!(
                "Build {name} succeeded but did not create its declared output {} `{}` at {}",
                output_kind_name(output.kind()),
                output.path(),
                self.resolve(output.path()).display()
            ),
        }
    }

    /// Logs the failure of step `step` and returns it as the error of the
    /// build.
    fn report(&self, step: usize, failure: StepFailure<'_>) -> InterpreterError {
        match failure {
            StepFailure::Status(status) => self.launcher.failed(
                &format!("Build {}", self.step_name(step)),
                status,
                Some(&self.step_scripts[step]),
            ),
            StepFailure::Error(err) => {
                tracing::error!("Build {} failed: {err}", self.step_name(step));
                err
            }
            StepFailure::MissingInput(_)
            | StepFailure::OccupiedOutput(..)
            | StepFailure::MissingOutput(_) => {
                let message = self.describe(step, &failure);
                tracing::error!("{message}");
                InterpreterError::ExecutionFailed(io::Error::other(message))
            }
        }
    }
}

/// Returns the metadata of the file system entry at `path`, following a
/// final symbolic link if `follow_symlink` is set, or `None` if there is no
/// such entry.
async fn entry_metadata(path: &Path, follow_symlink: bool) -> io::Result<Option<Metadata>> {
    let metadata = if follow_symlink {
        tokio::fs::metadata(path).await
    } else {
        tokio::fs::symlink_metadata(path).await
    };
    match metadata {
        Ok(metadata) => Ok(Some(metadata)),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("cannot inspect {}: {err}", path.display()),
        )),
    }
}

/// Names an output kind in diagnostics.
fn output_kind_name(kind: StepOutputKind) -> &'static str {
    match kind {
        StepOutputKind::File => "file",
        StepOutputKind::Tree => "directory tree",
    }
}

/// Clears `output`, a declared output below `root_dir`, before its step
/// starts, so that only what the step creates can satisfy it, without
/// following any symbolic link.
///
/// A link between `root_dir` and the output is refused, as clearing through
/// it would reach outside the declared path. So is a directory at a file
/// output, since declaring it as a file does not claim what is inside.
///
/// In the work directory, what an earlier build left at the output is
/// removed: a link itself, a file, or the whole directory of a tree output.
/// The host and build prefixes hold installed packages, so nothing is
/// removed there but an empty directory at a tree output; anything else at
/// an output in a prefix is refused.
async fn clear_output(root_dir: &Path, output: &PlannedOutput) -> io::Result<()> {
    let in_prefix = match output.path().root() {
        StepRoot::Work => false,
        StepRoot::Host | StepRoot::Build => true,
    };
    let occupied = || {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "it already exists, and nothing is removed from the host and build prefixes, as \
             they hold installed packages: declare a path that no package and no other step \
             creates",
        )
    };

    let mut path = root_dir.to_path_buf();
    let mut components = output.path().relative_path().components().peekable();
    while let Some(component) = components.next() {
        path.push(component);
        let Some(metadata) = entry_metadata(&path, false).await? else {
            return Ok(());
        };
        let file_type = metadata.file_type();
        if components.peek().is_some() {
            if file_type.is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is a symbolic link, and outputs are not cleared through links",
                        path.display()
                    ),
                ));
            }
            continue;
        }

        if file_type.is_dir() {
            return match output.kind() {
                StepOutputKind::File => Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    "it is a directory; declare the output with `kind: tree` to own the directory",
                )),
                StepOutputKind::Tree if in_prefix => match tokio::fs::remove_dir(&path).await {
                    Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => Err(occupied()),
                    removed => removed,
                },
                StepOutputKind::Tree => tokio::fs::remove_dir_all(&path).await,
            };
        }
        return if in_prefix {
            Err(occupied())
        } else if file_type.is_symlink() {
            remove_symlink(&path, file_type).await
        } else {
            tokio::fs::remove_file(&path).await
        };
    }
    Ok(())
}

/// Removes the symbolic link at `path`, not what it points to. Windows
/// removes a link to a directory, or a junction, as a directory.
#[cfg(windows)]
async fn remove_symlink(path: &Path, file_type: FileType) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    if file_type.is_symlink_dir() {
        tokio::fs::remove_dir(path).await
    } else {
        tokio::fs::remove_file(path).await
    }
}

/// Removes the symbolic link at `path`, not what it points to.
#[cfg(not(windows))]
async fn remove_symlink(path: &Path, _file_type: FileType) -> io::Result<()> {
    tokio::fs::remove_file(path).await
}
