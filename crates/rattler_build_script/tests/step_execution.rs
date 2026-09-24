//! Real-process coverage for `run_steps`, which activates once and starts
//! every build step as an independent process from the captured exported
//! environment, scheduling steps with declared inputs and outputs as a graph
//! between undeclared barrier steps, and for the legacy single-wrapper
//! `run_script`.

#![cfg(feature = "execution")]

use std::path::{Path, PathBuf};

use fs_err as fs;
use indexmap::IndexMap;
use rattler_build_script::{
    BuildScriptSection, EnvironmentIsolation, ExecutionArgs, ExecutionContext, GraphStep,
    InterpreterError, ResolvedScriptContents, RuntimeEnv, StepInput, StepInputKind, StepOutput,
    StepOutputKind, StepRoot, create_steps_script, run_steps,
};
use rattler_conda_types::Platform;

const ADDED: &str = "RB_STEP_ADDED";
const CHANGED: &str = "RB_STEP_CHANGED";
const REMOVED: &str = "RB_STEP_REMOVED";
const CAPTURED_ONLY: &str = "RB_STEP_CAPTURED_ONLY";
const CAPTURED_ONLY_VALUE: &str = "captured-marker-value";
const LOCAL: &str = "RB_STEP_LOCAL";
const LEAK: &str = "RB_STEP_LEAK";
const SECRET: &str = "RB_STEP_SECRET";
const SECRET_VALUE: &str = "rb-secret-value";

/// File the activation hook appends to each time it runs, relative to the
/// directory activation runs in, which must be the work directory.
const ACTIVATION_LOG: &str = "activations.txt";

/// Activation hook contents: records `name` in [`ACTIVATION_LOG`], adds,
/// changes and removes exported variables, and clears the `CONDA_BUILD`
/// marker, which must not make steps activate again. On Unix it also defines
/// shell-local state that only the activated shell itself can see.
#[cfg(windows)]
fn activation_hook(name: &str) -> String {
    format!(
        "@echo {name}>>\"{ACTIVATION_LOG}\"\r\n\
         @set \"{ADDED}=added\"\r\n\
         @set \"{CHANGED}=after\"\r\n\
         @set \"{REMOVED}=\"\r\n\
         @set \"{CAPTURED_ONLY}={CAPTURED_ONLY_VALUE}\"\r\n\
         @set \"CONDA_BUILD=\"\r\n"
    )
}

#[cfg(unix)]
fn activation_hook(name: &str) -> String {
    format!(
        "echo {name} >> {ACTIVATION_LOG}\n\
         export {ADDED}=added\n\
         export {CHANGED}=after\n\
         unset {REMOVED}\n\
         export {CAPTURED_ONLY}={CAPTURED_ONLY_VALUE}\n\
         unset CONDA_BUILD\n\
         RB_HOOK_SHELL_LOCAL=shell-local\n\
         rb_hook_function() {{ :; }}\n"
    )
}

/// Installs `contents` as an activation hook in `prefix/etc/conda/activate.d`.
fn install_activation_hook(prefix: &Path, contents: &str) {
    let extension = if cfg!(windows) { "bat" } else { "sh" };
    let hook_dir = prefix.join("etc").join("conda").join("activate.d");
    fs::create_dir_all(&hook_dir).unwrap();
    fs::write(hook_dir.join(format!("rb_step_test.{extension}")), contents).unwrap();
}

/// Native commands appending `NAME=value` (or `NAME=unset`) lines to `file`,
/// relative to the step's working directory.
#[cfg(windows)]
fn record_variables(file: &str, names: &[&str]) -> String {
    names
        .iter()
        .map(|name| {
            format!(
                "@if defined {name} (>>\"{file}\" echo {name}=%{name}%) else (>>\"{file}\" echo {name}=unset)"
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(unix)]
fn record_variables(file: &str, names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("echo \"{name}=${{{name}-unset}}\" >> '{file}'"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn set_variable(name: &str, value: &str) -> String {
    if cfg!(windows) {
        format!("@set \"{name}={value}\"")
    } else {
        format!("export {name}={value}")
    }
}

fn echo(text: &str) -> String {
    if cfg!(windows) {
        format!("@echo {text}")
    } else {
        format!("echo {text}")
    }
}

fn echo_variable(name: &str) -> String {
    if cfg!(windows) {
        format!("@echo %{name}%")
    } else {
        format!("echo \"${name}\"")
    }
}

fn exit_with(code: i32) -> String {
    if cfg!(windows) {
        format!("@exit /b {code}")
    } else {
        format!("exit {code}")
    }
}

fn step(lines: &[String]) -> BuildScriptSection {
    BuildScriptSection {
        interpreter: None,
        content: ResolvedScriptContents::Inline(lines.join("\n")),
        env: IndexMap::new(),
        cwd: None,
        label: None,
        graph: GraphStep::default(),
    }
}

/// A context whose build and host environments share `prefix`.
fn shared_context(runtime: RuntimeEnv, prefix: &Path) -> ExecutionContext {
    ExecutionContext::shared(runtime, prefix, Platform::current(), Platform::current())
}

fn execution_args(
    context: ExecutionContext,
    work_dir: &Path,
    sections: Vec<BuildScriptSection>,
) -> ExecutionArgs {
    ExecutionArgs {
        sections,
        env_vars: IndexMap::new(),
        secrets: IndexMap::new(),
        context,
        work_dir: work_dir.to_path_buf(),
        sandbox_config: None,
        env_isolation: EnvironmentIsolation::None,
    }
}

/// Creates `<root>/<name>` and returns it.
fn create_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn read_records(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Activation of separate host and build prefixes runs once, in the work
/// directory, for all steps, even though it clears the `CONDA_BUILD` marker.
/// Every step starts from its exported result (including removals of
/// variables the build environment had set), applies only its own `env` and
/// `cwd` (a relative `cwd` resolves against the work directory), and does not
/// see mutations made by earlier steps. Paths contain spaces and, on Unix,
/// shell metacharacters.
#[tokio::test]
async fn run_steps_start_every_step_from_one_captured_activation() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(
        tmp.path(),
        if cfg!(windows) {
            "work dir"
        } else {
            "work dir & it's"
        },
    );
    let nested_dir = create_dir(&work_dir, "nested dir");
    let host_prefix = create_dir(tmp.path(), "host prefix");
    let build_prefix = create_dir(tmp.path(), "build prefix");
    install_activation_hook(&host_prefix, &activation_hook("host"));
    install_activation_hook(&build_prefix, &activation_hook("build"));

    let mut step_with_overrides = step(&[record_variables("step1.txt", &[CHANGED, LOCAL, LEAK])]);
    step_with_overrides.env = IndexMap::from([
        (LOCAL.to_string(), "local".to_string()),
        (CHANGED.to_string(), "step-override".to_string()),
    ]);
    step_with_overrides.cwd = Some(PathBuf::from("nested dir"));

    let mut args = execution_args(
        ExecutionContext::separate(
            RuntimeEnv::current(),
            &build_prefix,
            Platform::current(),
            &host_prefix,
            Platform::current(),
        ),
        &work_dir,
        vec![
            step(&[
                record_variables("step0.txt", &[ADDED, CHANGED, REMOVED]),
                set_variable(LEAK, "leaked"),
                set_variable(CHANGED, "mutated"),
            ]),
            step_with_overrides,
            step(&[record_variables(
                "step2.txt",
                &[ADDED, CHANGED, REMOVED, LOCAL, LEAK],
            )]),
        ],
    );
    args.env_vars = IndexMap::from([
        (CHANGED.to_string(), "before".to_string()),
        (REMOVED.to_string(), "before".to_string()),
    ]);

    run_steps(args).await.unwrap();

    let mut activations = read_records(&work_dir.join(ACTIVATION_LOG));
    activations.sort();
    assert_eq!(activations, ["build", "host"]);

    assert_eq!(
        read_records(&work_dir.join("step0.txt")),
        [
            format!("{ADDED}=added"),
            format!("{CHANGED}=after"),
            format!("{REMOVED}=unset"),
        ]
    );
    assert_eq!(
        read_records(&nested_dir.join("step1.txt")),
        [
            format!("{CHANGED}=step-override"),
            format!("{LOCAL}=local"),
            format!("{LEAK}=unset"),
        ]
    );
    assert!(!work_dir.join("step1.txt").exists());
    assert_eq!(
        read_records(&work_dir.join("step2.txt")),
        [
            format!("{ADDED}=added"),
            format!("{CHANGED}=after"),
            format!("{REMOVED}=unset"),
            format!("{LOCAL}=unset"),
            format!("{LEAK}=unset"),
        ]
    );
}

/// A failing step fails the build and later steps never start.
#[tokio::test]
async fn run_steps_stop_at_first_failing_step() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![
            step(&[echo("ran>first.txt")]),
            step(&[exit_with(3)]),
            step(&[echo("ran>third.txt")]),
        ],
    );

    let result = run_steps(args).await;

    assert!(
        matches!(result, Err(InterpreterError::ExecutionFailed(_))),
        "unexpected result: {result:?}"
    );
    assert!(work_dir.join("first.txt").exists());
    assert!(!work_dir.join("third.txt").exists());
}

/// Steps can read secrets, but the build log only contains step output with
/// secrets masked, never the captured activation environment (the hook
/// exports a value no step prints), including for a step in its own `cwd`.
#[tokio::test]
async fn run_steps_log_masks_secrets_and_omits_captured_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let nested_dir = create_dir(&work_dir, "nested");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, &activation_hook("host"));

    let mut nested_step = step(&[echo("nested-step-output")]);
    nested_step.cwd = Some(nested_dir);

    let mut args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![
            step(&[
                echo("visible-step-output"),
                echo_variable(SECRET),
                record_variables("observed.txt", &[SECRET]),
            ]),
            nested_step,
        ],
    );
    args.secrets = IndexMap::from([(SECRET.to_string(), SECRET_VALUE.to_string())]);

    run_steps(args).await.unwrap();

    assert_eq!(
        read_records(&work_dir.join("observed.txt")),
        [format!("{SECRET}={SECRET_VALUE}")]
    );

    let log = fs::read_to_string(work_dir.join("conda_build.log")).unwrap();
    assert!(log.contains("visible-step-output"), "log:\n{log}");
    assert!(log.contains("nested-step-output"), "log:\n{log}");
    assert!(!log.contains(SECRET_VALUE), "log:\n{log}");
    assert!(!log.contains(CAPTURED_ONLY_VALUE), "log:\n{log}");
}

/// Values reach steps exactly as the build environment or activation holds
/// them, including surrounding and embedded quotes, `=` and non-ASCII text.
#[tokio::test]
async fn run_steps_pass_values_verbatim() {
    const INHERITED: &[(&str, &str)] = &[
        ("RB_STEP_QUOTED", "\"quoted value\""),
        ("RB_STEP_DEFINE", "-DNAME=\"hello world\""),
        ("RB_STEP_UNICODE", "grüße ✓ 日本"),
    ];
    const ACTIVATED: (&str, &str) = ("RB_STEP_HOOK_QUOTED", "\"-DNAME=hello world\"");
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    let (name, value) = ACTIVATED;
    install_activation_hook(
        &prefix,
        &if cfg!(windows) {
            format!("@set \"{name}={value}\"\r\n")
        } else {
            format!("export {name}='{value}'\n")
        },
    );
    let runtime = INHERITED
        .iter()
        .fold(RuntimeEnv::current(), |runtime, (name, value)| {
            runtime.with_var(*name, *value)
        });
    let expected: Vec<(&str, &str)> = INHERITED.iter().copied().chain([ACTIVATED]).collect();
    let names: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();

    let args = execution_args(
        shared_context(runtime, &prefix),
        &work_dir,
        vec![step(&[record_variables("values.txt", &names)])],
    );

    run_steps(args).await.unwrap();

    assert_eq!(
        read_records(&work_dir.join("values.txt")),
        expected
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
    );
}

/// A value containing a newline reaches steps unchanged and never defines an
/// extra variable from its second line.
#[tokio::test]
async fn run_steps_pass_multiline_values_verbatim() {
    const MULTILINE: &str = "RB_STEP_MULTILINE";
    const INJECTED: &str = "RB_STEP_INJECTED";
    let value = format!("first line\n{INJECTED}=injected");
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");

    // cmd cannot echo a value containing a newline, but `set NAME` prints it
    // unchanged.
    let record_multiline = if cfg!(windows) {
        format!("@set {MULTILINE}>>\"multiline.txt\"")
    } else {
        format!("printf '%s\\n' \"{MULTILINE}=${MULTILINE}\" > multiline.txt")
    };
    let args = execution_args(
        shared_context(RuntimeEnv::current().with_var(MULTILINE, &value), &prefix),
        &work_dir,
        vec![step(&[
            record_multiline,
            record_variables("injected.txt", &[INJECTED]),
        ])],
    );

    run_steps(args).await.unwrap();

    assert_eq!(
        fs::read_to_string(work_dir.join("multiline.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        format!("{MULTILINE}={value}\n")
    );
    assert_eq!(
        read_records(&work_dir.join("injected.txt")),
        [format!("{INJECTED}=unset")]
    );
}

/// Runs the generated `conda_build.<ext>` from the work directory in a fresh
/// native shell, as the build failure instructions advise.
fn run_build_script_manually(work_dir: &Path) -> std::process::Output {
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd.exe");
        command.args(["/d", "/c", "conda_build.bat"]);
        command
    } else {
        let mut command = std::process::Command::new("bash");
        command.arg("conda_build.sh");
        command
    };
    command
        .current_dir(work_dir)
        .env_remove("CONDA_BUILD")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// Steps without a `cwd` run in the work directory even when activation
/// changes directory, and the `conda_build.<ext>` written for debugging
/// replays them the same way: one activation, then every step in its own
/// process that does not see earlier steps' changes.
#[tokio::test]
async fn build_script_replays_steps_like_run_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let elsewhere = create_dir(tmp.path(), "elsewhere");
    let prefix = create_dir(tmp.path(), "prefix");
    let change_directory = if cfg!(windows) {
        format!("@cd /d \"{}\"\r\n", elsewhere.display())
    } else {
        format!("cd '{}'\n", elsewhere.display())
    };
    install_activation_hook(
        &prefix,
        &format!("{}{change_directory}", activation_hook("host")),
    );

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![
            step(&[
                record_variables("step0.txt", &[ADDED]),
                set_variable(LEAK, "leaked"),
            ]),
            step(&[record_variables("step1.txt", &[LEAK])]),
        ],
    );
    let outputs = [ACTIVATION_LOG, "step0.txt", "step1.txt"];
    let assert_outputs = |run: &str| {
        assert_eq!(
            read_records(&work_dir.join(ACTIVATION_LOG)),
            ["host"],
            "{run}"
        );
        assert_eq!(
            read_records(&work_dir.join("step0.txt")),
            [format!("{ADDED}=added")],
            "{run}"
        );
        assert_eq!(
            read_records(&work_dir.join("step1.txt")),
            [format!("{LEAK}=unset")],
            "{run}"
        );
        for file in outputs {
            assert!(!elsewhere.join(file).exists(), "{run}: {file} in elsewhere");
        }
    };

    run_steps(args).await.unwrap();
    assert_outputs("run_steps");

    for file in outputs {
        fs::remove_file(work_dir.join(file)).unwrap();
    }
    let output = run_build_script_manually(&work_dir);
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_outputs("build script replay");
}

/// Bash commands recording whether the activation hook's function and
/// non-exported variable are visible.
#[cfg(unix)]
fn record_shell_local_state(file: &str) -> String {
    format!(
        "if declare -F rb_hook_function >/dev/null; then echo function=present; else echo function=absent; fi >> '{file}'\n\
         echo \"shell_local=${{RB_HOOK_SHELL_LOCAL-unset}}\" >> '{file}'"
    )
}

/// Steps inherit only the exported activation environment: Bash functions and
/// non-exported variables defined during activation are not carried over.
#[cfg(unix)]
#[tokio::test]
async fn run_steps_do_not_inherit_shell_local_activation_state() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, &activation_hook("host"));

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![step(&[record_shell_local_state("shell_state.txt")])],
    );

    run_steps(args).await.unwrap();

    assert_eq!(
        read_records(&work_dir.join("shell_state.txt")),
        ["function=absent", "shell_local=unset"]
    );
}

/// A legacy `build.script` keeps running inside the activated wrapper shell,
/// so Bash functions and non-exported variables from activation stay visible.
#[cfg(unix)]
#[tokio::test]
async fn run_script_keeps_shell_local_activation_state() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, &activation_hook("host"));

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![step(&[record_shell_local_state("shell_state.txt")])],
    );

    rattler_build_script::run_script(args).await.unwrap();

    assert_eq!(
        read_records(&work_dir.join("shell_state.txt")),
        ["function=present", "shell_local=shell-local"]
    );
}

/// Steps start `bash` as found before activation, while their commands see
/// the activated `PATH`, even when that `PATH` has no `bash`.
#[cfg(unix)]
#[tokio::test]
async fn run_steps_start_bash_found_before_activation() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    let activated_path = prefix.join("bin");
    install_activation_hook(
        &prefix,
        &format!("export PATH='{}'\n", activated_path.display()),
    );

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![step(&["echo \"PATH=$PATH\" > path.txt".to_string()])],
    );

    run_steps(args).await.unwrap();

    assert_eq!(
        read_records(&work_dir.join("path.txt")),
        [format!("PATH={}", activated_path.display())]
    );
}

/// Activation may export values that are not valid UTF-8, such as Latin-1
/// paths; steps receive the bytes unchanged.
#[cfg(unix)]
#[tokio::test]
async fn run_steps_pass_non_utf8_values_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, "export RB_STEP_BYTES=\"$(printf 'caf\\351')\"\n");

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![step(&[
            "printf '%s' \"$RB_STEP_BYTES\" > bytes.bin".to_string()
        ])],
    );

    run_steps(args).await.unwrap();

    assert_eq!(fs::read(work_dir.join("bytes.bin")).unwrap(), b"caf\xe9");
}

/// Steps stop at the first failing one even when activation turns `set -e`
/// off: `run_steps` and the `conda_build.sh` replay both fail with that
/// step's status after activating once, and never start the next step.
#[cfg(unix)]
#[tokio::test]
async fn steps_stop_at_first_failure_when_activation_disables_errexit() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, &format!("{}set +e\n", activation_hook("host")));

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![
            step(&[echo("ran>step0.txt"), exit_with(3)]),
            step(&[echo("ran>step1.txt")]),
        ],
    );
    let assert_stopped_after_step0 = |run: &str| {
        assert_eq!(
            read_records(&work_dir.join(ACTIVATION_LOG)),
            ["host"],
            "{run}"
        );
        assert_eq!(read_records(&work_dir.join("step0.txt")), ["ran"], "{run}");
        assert!(!work_dir.join("step1.txt").exists(), "{run}: step 1 ran");
    };

    let result = run_steps(args).await;
    assert!(
        matches!(result, Err(InterpreterError::ExecutionFailed(_))),
        "unexpected result: {result:?}"
    );
    assert_stopped_after_step0("run_steps");

    for file in [ACTIVATION_LOG, "step0.txt"] {
        fs::remove_file(work_dir.join(file)).unwrap();
    }
    let output = run_build_script_manually(&work_dir);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_stopped_after_step0("build script replay");
}

/// A 32-bit Windows build from a 64-bit rattler-build runs activation, once,
/// and every step as x86 processes, and so does the `conda_build.bat` replay
/// when started from a native `cmd.exe`.
#[cfg(windows)]
#[tokio::test]
async fn run_steps_run_as_x86_for_win32_builds() {
    if !matches!(Platform::current(), Platform::Win64 | Platform::WinArm64)
        || !x86_machine_launch_available()
    {
        eprintln!("skipping: `start /machine x86` is unavailable on this host");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(
        &prefix,
        &format!(
            "{}{}\r\n",
            activation_hook("host"),
            record_bitness("activation_arch.txt")
        ),
    );

    let args = execution_args(
        ExecutionContext::shared(
            RuntimeEnv::current(),
            &prefix,
            Platform::Win32,
            Platform::Win32,
        ),
        &work_dir,
        vec![
            step(&[
                record_bitness("arch.txt"),
                record_variables("arch.txt", &[ADDED]),
            ]),
            step(&[
                record_bitness("arch.txt"),
                record_variables("arch.txt", &["PROCESSOR_ARCHITECTURE"]),
            ]),
        ],
    );
    let outputs = [ACTIVATION_LOG, "activation_arch.txt", "arch.txt"];
    let assert_outputs = |run: &str| {
        assert_eq!(
            read_records(&work_dir.join(ACTIVATION_LOG)),
            ["host"],
            "{run}"
        );
        assert_eq!(
            read_records(&work_dir.join("activation_arch.txt")),
            ["wow64"],
            "{run}"
        );
        assert_eq!(
            read_records(&work_dir.join("arch.txt")),
            [
                "wow64".to_string(),
                format!("{ADDED}=added"),
                "wow64".to_string(),
                "PROCESSOR_ARCHITECTURE=x86".to_string(),
            ],
            "{run}"
        );
    };

    run_steps(args).await.unwrap();
    assert_outputs("run_steps");

    for file in outputs {
        fs::remove_file(work_dir.join(file)).unwrap();
    }
    let output = run_build_script_manually(&work_dir);
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_outputs("build script replay");
}

/// Native cmd command appending `wow64` to `file` when it runs in a 32-bit
/// process on 64-bit Windows, the only processes that see the `Sysnative`
/// alias of the system directory, and `native` otherwise.
#[cfg(windows)]
fn record_bitness(file: &str) -> String {
    format!(
        r#"@if exist "%SystemRoot%\Sysnative\cmd.exe" (>>"{file}" echo wow64) else (>>"{file}" echo native)"#
    )
}

/// Whether this host can run an x86 `cmd.exe` the way the cmd dialect does.
#[cfg(windows)]
fn x86_machine_launch_available() -> bool {
    std::process::Command::new("cmd.exe")
        .args([
            "/d",
            "/v:on",
            "/c",
            r"start /b /wait /machine x86 %SystemRoot%\SysWOW64\cmd.exe /d /c exit 5 & exit /b !ERRORLEVEL!",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.code() == Some(5))
}

/// Seconds a step waits for a peer it has to run at the same time as. A
/// passing run returns as soon as the peer starts, which the scheduler
/// launches together with it; the tests that wait this long skip on hosts
/// that run steps one at a time, so only a failing run reaches the bound.
const OVERLAP_WAIT_SECONDS: u32 = 5;

/// Seconds a step watches for a peer that must not run at the same time.
/// Every passing run waits this long.
const EXCLUSION_WAIT_SECONDS: u32 = 2;

/// Recorded by [`watch_for`] when the watched step was seen running.
const MET: &str = "met";

/// Recorded by [`watch_for`] when the watched step was not seen running.
const ALONE: &str = "alone";

/// Whether declared steps can run at the same time on this host: no more
/// steps run at once than there is available parallelism.
fn steps_can_overlap() -> bool {
    std::thread::available_parallelism().is_ok_and(|parallelism| parallelism.get() >= 2)
}

/// Native command creating `<name>.started` in the step's working
/// directory, announcing that step `name` runs.
fn mark_started(name: &str) -> String {
    if cfg!(windows) {
        format!("@echo started>\"{name}.started\"")
    } else {
        format!("echo started > '{name}.started'")
    }
}

/// Native commands waiting up to `seconds` for `<peer>.started` to appear in
/// the step's working directory, then recording in `<me>-<peer>.txt` whether
/// it did ([`MET`]) or not ([`ALONE`]).
#[cfg(windows)]
fn watch_for(me: &str, peer: &str, seconds: u32) -> String {
    format!(
        "@set /a RB_POLLS=0\n\
         :wait_{peer}\n\
         @if exist \"{peer}.started\" goto met_{peer}\n\
         @set /a RB_POLLS+=1\n\
         @if %RB_POLLS% gtr {seconds} goto alone_{peer}\n\
         @ping -n 2 127.0.0.1 >nul\n\
         @goto wait_{peer}\n\
         :met_{peer}\n\
         @echo {MET}>\"{me}-{peer}.txt\"\n\
         @goto watched_{peer}\n\
         :alone_{peer}\n\
         @echo {ALONE}>\"{me}-{peer}.txt\"\n\
         :watched_{peer}"
    )
}

#[cfg(unix)]
fn watch_for(me: &str, peer: &str, seconds: u32) -> String {
    let polls = seconds * 10;
    format!(
        "rb_polls=0\n\
         while [ ! -e '{peer}.started' ] && [ \"$rb_polls\" -lt {polls} ]; do\n\
         rb_polls=$((rb_polls + 1))\n\
         sleep 0.1\n\
         done\n\
         if [ -e '{peer}.started' ]; then echo {MET}; else echo {ALONE}; fi > '{me}-{peer}.txt'"
    )
}

/// Native commands announcing step `me`, then watching for `peer` for up to
/// `seconds` (see [`watch_for`]). Two steps meeting each other both record
/// [`MET`] only when they run at the same time, however long either takes
/// to start, so overlap is observed without relying on timing.
fn rendezvous(me: &str, peer: &str, seconds: u32) -> String {
    format!("{}\n{}", mark_started(me), watch_for(me, peer, seconds))
}

/// What step `me` recorded about `peer` with [`watch_for`].
fn observed(work_dir: &Path, me: &str, peer: &str) -> Vec<String> {
    read_records(&work_dir.join(format!("{me}-{peer}.txt")))
}

/// Native command writing `text` as the only line of the file `path`.
fn write_line(path: &Path, text: &str) -> String {
    if cfg!(windows) {
        format!("@echo {text}>\"{}\"", path.display())
    } else {
        format!("echo {text} > '{}'", path.display())
    }
}

/// Native command appending `text` as a line to the file `path`.
fn append_line(path: &Path, text: &str) -> String {
    if cfg!(windows) {
        format!("@echo {text}>>\"{}\"", path.display())
    } else {
        format!("echo {text} >> '{}'", path.display())
    }
}

/// Native command copying the file `from` to `to`.
fn copy_file(from: &Path, to: &Path) -> String {
    if cfg!(windows) {
        format!("@copy /y \"{}\" \"{}\" >nul", from.display(), to.display())
    } else {
        format!("cp '{}' '{}'", from.display(), to.display())
    }
}

/// Native command writing the contents of every file in `dir` whose name
/// matches `pattern` to `to`.
fn concatenate(dir: &Path, pattern: &str, to: &Path) -> String {
    if cfg!(windows) {
        format!(
            "@for %%f in (\"{}\\{pattern}\") do @type \"%%~f\">>\"{}\"",
            dir.display(),
            to.display()
        )
    } else {
        format!("cat '{}'/{pattern} > '{}'", dir.display(), to.display())
    }
}

/// Native command pausing for about `seconds`.
fn pause(seconds: u32) -> String {
    if cfg!(windows) {
        format!("@ping -n {} 127.0.0.1 >nul", seconds + 1)
    } else {
        format!("sleep {seconds}")
    }
}

/// Native command creating the directory `dir` and its missing parents,
/// unless it exists.
fn make_dir(dir: &Path) -> String {
    if cfg!(windows) {
        format!("@if not exist \"{0}\" mkdir \"{0}\"", dir.display())
    } else {
        format!("mkdir -p '{}'", dir.display())
    }
}

/// Makes `link` a link to the directory `target` without needing any
/// privilege: a symbolic link on Unix, a junction on Windows.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let linked = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            linked.status.success(),
            "mklink /J failed:\n{}{}",
            String::from_utf8_lossy(&linked.stdout),
            String::from_utf8_lossy(&linked.stderr)
        );
    }
}

/// Makes `link` a symbolic link to the file `target`, which need not exist.
/// Returns `false` when this user may not create symbolic links, which
/// Windows only allows with developer mode or elevation.
fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(target, link);
    match created {
        Ok(()) => true,
        // ERROR_PRIVILEGE_NOT_HELD
        Err(err) if cfg!(windows) && err.raw_os_error() == Some(1314) => false,
        Err(err) => panic!(
            "cannot link {} to {}: {err}",
            link.display(),
            target.display()
        ),
    }
}

/// Exact file input `path` of `root`.
fn file_input(root: StepRoot, path: &str) -> StepInput {
    StepInput {
        root,
        path: PathBuf::from(path),
        kind: StepInputKind::File,
    }
}

/// Glob input `pattern` of `root`.
fn glob_input(root: StepRoot, pattern: &str) -> StepInput {
    StepInput {
        root,
        path: PathBuf::from(pattern),
        kind: StepInputKind::Glob,
    }
}

/// File output `path` of `root`.
fn file_output(root: StepRoot, path: &str) -> StepOutput {
    StepOutput {
        root,
        path: PathBuf::from(path),
        kind: StepOutputKind::File,
    }
}

/// Output of the whole directory tree `path` of `root`.
fn tree_output(root: StepRoot, path: &str) -> StepOutput {
    StepOutput {
        root,
        path: PathBuf::from(path),
        kind: StepOutputKind::Tree,
    }
}

/// Declaration of a graph step named `id` with `inputs` and `outputs`.
fn graph(id: &str, inputs: Vec<StepInput>, outputs: Vec<StepOutput>) -> GraphStep {
    GraphStep {
        id: Some(id.to_string()),
        inputs: Some(inputs),
        outputs: Some(outputs),
        depends_on: Vec::new(),
    }
}

/// `graph`, additionally ordered after the steps named `ids`.
fn after(graph: GraphStep, ids: &[&str]) -> GraphStep {
    GraphStep {
        depends_on: ids.iter().map(|id| id.to_string()).collect(),
        ..graph
    }
}

/// `section` scheduled as `graph`.
fn declared(section: BuildScriptSection, graph: GraphStep) -> BuildScriptSection {
    BuildScriptSection { graph, ..section }
}

/// The message of the execution failure `result` must be.
fn execution_failure(result: Result<(), InterpreterError>) -> String {
    match result {
        Err(InterpreterError::ExecutionFailed(error)) => error.to_string(),
        other => panic!("expected an execution failure, got {other:?}"),
    }
}

/// A work directory with separate host and build prefixes: the `work`,
/// `host` and `build` roots of declared inputs and outputs.
struct Roots {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    host: PathBuf,
    build: PathBuf,
}

impl Roots {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let work = create_dir(tmp.path(), "work");
        let host = create_dir(tmp.path(), "host prefix");
        let build = create_dir(tmp.path(), "build prefix");
        Self {
            _tmp: tmp,
            work,
            host,
            build,
        }
    }

    /// Context with the host and build prefixes of these roots.
    fn context(&self) -> ExecutionContext {
        ExecutionContext::separate(
            RuntimeEnv::current(),
            &self.build,
            Platform::current(),
            &self.host,
            Platform::current(),
        )
    }

    /// Arguments running `sections` with these roots.
    fn args(&self, sections: Vec<BuildScriptSection>) -> ExecutionArgs {
        execution_args(self.context(), &self.work, sections)
    }
}

/// Fully declared steps without artifact or explicit edges between them
/// run at the same time, even when both declarations are explicitly empty.
/// Steps declaring neither are barriers and never overlap, even with each
/// other.
#[tokio::test]
async fn empty_declarations_overlap_but_missing_declarations_do_not() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();

    let args = roots.args(vec![
        declared(
            step(&[rendezvous("empty-a", "empty-b", OVERLAP_WAIT_SECONDS)]),
            graph("empty-a", vec![], vec![]),
        ),
        declared(
            step(&[rendezvous("empty-b", "empty-a", OVERLAP_WAIT_SECONDS)]),
            graph("empty-b", vec![], vec![]),
        ),
        step(&[rendezvous("legacy-a", "legacy-b", EXCLUSION_WAIT_SECONDS)]),
        step(&[rendezvous("legacy-b", "legacy-a", EXCLUSION_WAIT_SECONDS)]),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "empty-a", "empty-b"), [MET]);
    assert_eq!(observed(&roots.work, "empty-b", "empty-a"), [MET]);
    assert_eq!(observed(&roots.work, "legacy-a", "legacy-b"), [ALONE]);
    assert_eq!(observed(&roots.work, "legacy-b", "legacy-a"), [MET]);
}

/// An undeclared step splits the declared steps around it into epochs: the
/// steps before it overlap with each other and finish before it starts, and
/// the steps after it start only once it has finished, then overlap again.
#[tokio::test]
async fn undeclared_steps_separate_parallel_epochs() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();

    let args = roots.args(vec![
        declared(
            step(&[
                rendezvous("a", "b", OVERLAP_WAIT_SECONDS),
                watch_for("a", "barrier", EXCLUSION_WAIT_SECONDS),
            ]),
            graph("a", vec![], vec![]),
        ),
        declared(
            step(&[
                rendezvous("b", "a", OVERLAP_WAIT_SECONDS),
                watch_for("b", "barrier", EXCLUSION_WAIT_SECONDS),
            ]),
            graph("b", vec![], vec![]),
        ),
        step(&[rendezvous("barrier", "c", EXCLUSION_WAIT_SECONDS)]),
        declared(
            step(&[rendezvous("c", "d", OVERLAP_WAIT_SECONDS)]),
            graph("c", vec![], vec![]),
        ),
        declared(
            step(&[rendezvous("d", "c", OVERLAP_WAIT_SECONDS)]),
            graph("d", vec![], vec![]),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "a", "b"), [MET]);
    assert_eq!(observed(&roots.work, "b", "a"), [MET]);
    assert_eq!(observed(&roots.work, "a", "barrier"), [ALONE]);
    assert_eq!(observed(&roots.work, "b", "barrier"), [ALONE]);
    assert_eq!(observed(&roots.work, "barrier", "c"), [ALONE]);
    assert_eq!(observed(&roots.work, "c", "d"), [MET]);
    assert_eq!(observed(&roots.work, "d", "c"), [MET]);
}

/// A consumer starts only once the producer of its input has succeeded,
/// even when it is listed first and a stale copy of that input already
/// exists.
#[tokio::test]
async fn consumer_waits_for_producer_despite_stale_output() {
    let roots = Roots::new();
    let generated = create_dir(&roots.work, "gen");
    fs::write(generated.join("message.txt"), "stale\n").unwrap();
    let published = create_dir(&roots.host, "share");

    let args = roots.args(vec![
        declared(
            step(&[
                mark_started("consumer"),
                copy_file(
                    &generated.join("message.txt"),
                    &published.join("message.txt"),
                ),
            ]),
            graph(
                "consumer",
                vec![file_input(StepRoot::Work, "gen/message.txt")],
                vec![file_output(StepRoot::Host, "share/message.txt")],
            ),
        ),
        declared(
            step(&[
                watch_for("producer", "consumer", EXCLUSION_WAIT_SECONDS),
                write_line(&generated.join("message.txt"), "fresh"),
            ]),
            graph(
                "producer",
                vec![],
                vec![file_output(StepRoot::Work, "gen/message.txt")],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "producer", "consumer"), [ALONE]);
    assert_eq!(read_records(&published.join("message.txt")), ["fresh"]);
}

/// A tree output owns everything below it: exact and glob inputs inside the
/// tree wait for its producer, even over stale files an earlier build left
/// in the work directory, and a glob input waits for the producers of all
/// the files it matches, across the work, host and build roots.
#[tokio::test]
async fn tree_and_glob_inputs_wait_for_their_producers() {
    let roots = Roots::new();
    let nested = roots.work.join("generated").join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("value.txt"), "stale\n").unwrap();
    let tools = create_dir(&roots.build, "bin");
    let packaged = create_dir(&roots.host, "share").join("packaged.txt");

    let args = roots.args(vec![
        declared(
            step(&[concatenate(&tools, "*.txt", &packaged)]),
            graph(
                "packager",
                vec![glob_input(StepRoot::Build, "bin/*.txt")],
                vec![file_output(StepRoot::Host, "share/packaged.txt")],
            ),
        ),
        declared(
            step(&[
                mark_started("exact-reader"),
                copy_file(&nested.join("value.txt"), &tools.join("exact.txt")),
            ]),
            graph(
                "exact-reader",
                vec![file_input(StepRoot::Work, "generated/nested/value.txt")],
                vec![file_output(StepRoot::Build, "bin/exact.txt")],
            ),
        ),
        declared(
            step(&[concatenate(&nested, "*.txt", &tools.join("glob.txt"))]),
            graph(
                "glob-reader",
                vec![glob_input(StepRoot::Work, "generated/**/*.txt")],
                vec![file_output(StepRoot::Build, "bin/glob.txt")],
            ),
        ),
        declared(
            step(&[
                watch_for("generator", "exact-reader", EXCLUSION_WAIT_SECONDS),
                make_dir(&nested),
                write_line(&nested.join("value.txt"), "fresh"),
            ]),
            graph(
                "generator",
                vec![],
                vec![tree_output(StepRoot::Work, "generated")],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "generator", "exact-reader"), [ALONE]);
    assert_eq!(read_records(&tools.join("exact.txt")), ["fresh"]);
    assert_eq!(read_records(&tools.join("glob.txt")), ["fresh"]);
    assert_eq!(read_records(&packaged), ["fresh", "fresh"]);
}

/// Artifacts are identified by root and path: steps writing the same
/// relative path in the work directory and in the host prefix neither
/// conflict nor wait for each other, and a consumer of both gets each.
#[tokio::test]
async fn equal_paths_under_different_roots_are_different_artifacts() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();
    let relative = Path::new("share").join("out.txt");
    let work_file = roots.work.join(&relative);
    let host_file = roots.host.join(&relative);
    create_dir(&roots.work, "share");
    create_dir(&roots.host, "share");

    let args = roots.args(vec![
        declared(
            step(&[
                rendezvous("work-writer", "host-writer", OVERLAP_WAIT_SECONDS),
                write_line(&work_file, "work"),
            ]),
            graph(
                "work-writer",
                vec![],
                vec![file_output(StepRoot::Work, "share/out.txt")],
            ),
        ),
        declared(
            step(&[
                rendezvous("host-writer", "work-writer", OVERLAP_WAIT_SECONDS),
                write_line(&host_file, "host"),
            ]),
            graph(
                "host-writer",
                vec![],
                vec![file_output(StepRoot::Host, "share/out.txt")],
            ),
        ),
        declared(
            step(&[
                copy_file(&work_file, &roots.build.join("from-work.txt")),
                copy_file(&host_file, &roots.build.join("from-host.txt")),
            ]),
            graph(
                "reader",
                vec![
                    file_input(StepRoot::Work, "share/out.txt"),
                    file_input(StepRoot::Host, "share/out.txt"),
                ],
                vec![
                    file_output(StepRoot::Build, "from-work.txt"),
                    file_output(StepRoot::Build, "from-host.txt"),
                ],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "work-writer", "host-writer"), [MET]);
    assert_eq!(observed(&roots.work, "host-writer", "work-writer"), [MET]);
    assert_eq!(read_records(&roots.build.join("from-work.txt")), ["work"]);
    assert_eq!(read_records(&roots.build.join("from-host.txt")), ["host"]);
}

/// `depends_on` orders a step after a named step without any artifact
/// between them, whatever their order in the list, and leaves other steps
/// free to overlap with either.
#[tokio::test]
async fn explicit_dependencies_order_steps_without_artifacts() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();

    let args = roots.args(vec![
        declared(
            step(&[mark_started("late")]),
            after(graph("late", vec![], vec![]), &["early"]),
        ),
        declared(
            step(&[
                rendezvous("early", "sibling", OVERLAP_WAIT_SECONDS),
                watch_for("early", "late", EXCLUSION_WAIT_SECONDS),
            ]),
            graph("early", vec![], vec![]),
        ),
        declared(
            step(&[rendezvous("sibling", "early", OVERLAP_WAIT_SECONDS)]),
            graph("sibling", vec![], vec![]),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "early", "sibling"), [MET]);
    assert_eq!(observed(&roots.work, "sibling", "early"), [MET]);
    assert_eq!(observed(&roots.work, "early", "late"), [ALONE]);
    assert!(roots.work.join("late.started").exists(), "late never ran");
}

/// When a step fails, steps already running finish before the build fails,
/// with the first failure, and nothing starts afterwards: neither consumers
/// of the failed step's outputs nor later barriers.
#[tokio::test]
async fn failing_step_lets_running_steps_finish_and_starts_nothing_new() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();

    let args = roots.args(vec![
        declared(
            step(&[
                rendezvous("failing", "running", OVERLAP_WAIT_SECONDS),
                write_line(Path::new("failing.txt"), "partial"),
                exit_with(3),
            ]),
            graph(
                "failing",
                vec![],
                vec![file_output(StepRoot::Work, "failing.txt")],
            ),
        ),
        declared(
            step(&[
                rendezvous("running", "failing", OVERLAP_WAIT_SECONDS),
                pause(1),
                mark_started("running-finishing"),
                exit_with(4),
            ]),
            graph("running", vec![], vec![]),
        ),
        declared(
            step(&[mark_started("consumer")]),
            graph(
                "consumer",
                vec![file_input(StepRoot::Work, "failing.txt")],
                vec![],
            ),
        ),
        step(&[mark_started("barrier")]),
    ]);

    let message = execution_failure(run_steps(args).await);

    assert!(
        message.contains("failing")
            && message.contains("status 3")
            && !message.contains("status 4"),
        "the error does not report the first failure:\n{message}"
    );
    assert!(
        roots.work.join("running-finishing.started").exists(),
        "the build failed before the running step finished"
    );
    assert!(!roots.work.join("consumer.started").exists());
    assert!(!roots.work.join("barrier.started").exists());
}

/// An exact input that no step produces may be created by an earlier
/// undeclared step, and a glob input may match nothing at all.
#[tokio::test]
async fn source_inputs_may_come_from_earlier_barriers() {
    let roots = Roots::new();

    let args = roots.args(vec![
        step(&[write_line(Path::new("seed.txt"), "seeded")]),
        declared(
            step(&[copy_file(Path::new("seed.txt"), Path::new("copy.txt"))]),
            graph(
                "reader",
                vec![file_input(StepRoot::Work, "seed.txt")],
                vec![file_output(StepRoot::Work, "copy.txt")],
            ),
        ),
        declared(
            step(&[mark_started("globber")]),
            graph(
                "globber",
                vec![glob_input(StepRoot::Work, "absent/*.dat")],
                vec![],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(read_records(&roots.work.join("copy.txt")), ["seeded"]);
    assert!(
        roots.work.join("globber.started").exists(),
        "globber never ran"
    );
}

/// An exact input that no step produces and that does not exist when its
/// step is due fails the build, naming the input, without starting the step.
#[tokio::test]
async fn missing_source_input_fails_before_its_step_starts() {
    let roots = Roots::new();

    let args = roots.args(vec![declared(
        step(&[mark_started("needs-source")]),
        graph(
            "needs-source",
            vec![file_input(StepRoot::Work, "missing-source.txt")],
            vec![],
        ),
    )]);

    let message = execution_failure(run_steps(args).await);

    assert!(message.contains("missing-source.txt"), "{message}");
    assert!(!roots.work.join("needs-source.started").exists());
}

/// A step that succeeds without creating a declared output fails the build,
/// naming the output, even when a file from an earlier build is at its path,
/// and consumers of that output never start.
#[tokio::test]
async fn missing_declared_output_fails_the_build() {
    let roots = Roots::new();
    fs::write(roots.work.join("never-written.txt"), "stale\n").unwrap();

    let args = roots.args(vec![
        declared(
            step(&[mark_started("lazy")]),
            graph(
                "lazy",
                vec![],
                vec![file_output(StepRoot::Work, "never-written.txt")],
            ),
        ),
        declared(
            step(&[mark_started("consumer")]),
            graph(
                "consumer",
                vec![file_input(StepRoot::Work, "never-written.txt")],
                vec![],
            ),
        ),
    ]);

    let message = execution_failure(run_steps(args).await);

    assert!(message.contains("never-written.txt"), "{message}");
    assert!(roots.work.join("lazy.started").exists(), "lazy never ran");
    assert!(!roots.work.join("consumer.started").exists());
}

/// Step graphs that cannot be scheduled, each with the texts its error must
/// contain: the steps and paths involved.
fn invalid_graphs() -> Vec<(&'static str, Vec<GraphStep>, Vec<&'static str>)> {
    let mut cases = vec![
        (
            "artifact cycle",
            vec![
                graph(
                    "cycle-a",
                    vec![file_input(StepRoot::Work, "a.txt")],
                    vec![file_output(StepRoot::Work, "b.txt")],
                ),
                graph(
                    "cycle-b",
                    vec![file_input(StepRoot::Work, "b.txt")],
                    vec![file_output(StepRoot::Work, "a.txt")],
                ),
            ],
            vec!["cycle-a", "cycle-b"],
        ),
        (
            "explicit cycle",
            vec![
                after(graph("loop-a", vec![], vec![]), &["loop-b"]),
                after(graph("loop-b", vec![], vec![]), &["loop-a"]),
            ],
            vec!["loop-a", "loop-b"],
        ),
        (
            "dependency on a step after a barrier",
            vec![
                after(graph("before-barrier", vec![], vec![]), &["after-barrier"]),
                GraphStep::default(),
                graph("after-barrier", vec![], vec![]),
            ],
            vec!["before-barrier", "after-barrier"],
        ),
        (
            "unknown dependency",
            vec![after(graph("orphan", vec![], vec![]), &["nowhere"])],
            vec!["orphan", "nowhere"],
        ),
        (
            "duplicate id",
            vec![graph("twin", vec![], vec![]), graph("twin", vec![], vec![])],
            vec!["twin"],
        ),
        (
            "duplicate output",
            vec![
                graph(
                    "writer-a",
                    vec![],
                    vec![file_output(StepRoot::Host, "share/out.txt")],
                ),
                graph(
                    "writer-b",
                    vec![],
                    vec![file_output(StepRoot::Host, "share/out.txt")],
                ),
            ],
            vec!["writer-a", "writer-b", "share/out.txt"],
        ),
        (
            "file inside an output tree",
            vec![
                graph(
                    "tree-owner",
                    vec![],
                    vec![tree_output(StepRoot::Build, "lib")],
                ),
                graph(
                    "file-owner",
                    vec![],
                    vec![file_output(StepRoot::Build, "lib/nested/inner.txt")],
                ),
            ],
            vec!["tree-owner", "file-owner", "lib/nested/inner.txt"],
        ),
        (
            "nested output trees",
            vec![
                graph(
                    "outer-tree",
                    vec![],
                    vec![tree_output(StepRoot::Work, "out")],
                ),
                graph(
                    "inner-tree",
                    vec![],
                    vec![tree_output(StepRoot::Work, "out/sub")],
                ),
            ],
            vec!["outer-tree", "inner-tree"],
        ),
        (
            "partial declaration",
            vec![GraphStep {
                id: Some("half-declared".to_string()),
                inputs: Some(Vec::new()),
                outputs: None,
                depends_on: Vec::new(),
            }],
            vec!["half-declared"],
        ),
        (
            "path with a parent component",
            vec![graph(
                "parent-output",
                vec![],
                vec![file_output(StepRoot::Work, "gen/../out.txt")],
            )],
            vec!["parent-output"],
        ),
        (
            "absolute path",
            vec![graph(
                "rooted-input",
                vec![file_input(StepRoot::Host, "/etc/hosts")],
                vec![],
            )],
            vec!["rooted-input"],
        ),
        (
            "drive path",
            vec![graph(
                "drive-output",
                vec![],
                vec![file_output(StepRoot::Build, "C:/outside.txt")],
            )],
            vec!["drive-output"],
        ),
        (
            "drive inside a path",
            vec![graph(
                "embedded-drive-output",
                vec![],
                vec![tree_output(StepRoot::Work, "scratch/C:/outside")],
            )],
            vec!["embedded-drive-output"],
        ),
        (
            "file stream",
            vec![graph(
                "stream-input",
                vec![file_input(StepRoot::Work, "share/out.txt:stream")],
                vec![],
            )],
            vec!["stream-input"],
        ),
        (
            "malformed glob",
            vec![graph(
                "bad-glob",
                vec![glob_input(StepRoot::Work, "src/[a-")],
                vec![],
            )],
            vec!["bad-glob"],
        ),
    ];
    if cfg!(any(windows, target_os = "macos")) {
        cases.push((
            "outputs differing only in case and separators",
            vec![
                graph(
                    "upper-writer",
                    vec![],
                    vec![file_output(StepRoot::Work, "Gen/Out.TXT")],
                ),
                graph(
                    "lower-writer",
                    vec![],
                    vec![file_output(StepRoot::Work, r"gen\out.txt")],
                ),
            ],
            vec!["upper-writer", "lower-writer"],
        ));
    }
    cases
}

/// Runs the steps declared by `graphs`, after an undeclared step, in
/// `work_dir` with `context`, and asserts they are rejected as
/// [`assert_sections_rejected_before_activation`] describes.
async fn assert_rejected_before_activation(
    case: &str,
    work_dir: &Path,
    context: &ExecutionContext,
    graphs: &[GraphStep],
    named: &[&str],
) {
    let sections = || {
        std::iter::once(step(&[mark_started("barrier")]))
            .chain(
                graphs
                    .iter()
                    .map(|graph| declared(step(&[mark_started("graph-step")]), graph.clone())),
            )
            .collect::<Vec<_>>()
    };
    assert_sections_rejected_before_activation(case, work_dir, context, sections, named).await;
}

/// Runs the steps `sections` builds in `work_dir` with `context`, through
/// `run_steps` and `create_steps_script`. Asserts that both fail before
/// activation (whose hook the caller installed) or any step runs and before
/// they write any script, with an error naming every text of `named`. Steps
/// announce that they ran by creating `barrier.started` or
/// `graph-step.started` in the work directory. Scripts an earlier build left
/// in the work directory are the caller's to check.
async fn assert_sections_rejected_before_activation(
    case: &str,
    work_dir: &Path,
    context: &ExecutionContext,
    sections: impl Fn() -> Vec<BuildScriptSection>,
    named: &[&str],
) {
    let extension = if cfg!(windows) { "bat" } else { "sh" };
    let scripts: Vec<String> = [
        format!("conda_build.{extension}"),
        format!("build_env.{extension}"),
        "conda_build_steps".to_string(),
    ]
    .into_iter()
    .filter(|script| !work_dir.join(script).exists())
    .collect();
    let args = || execution_args(context.clone(), work_dir, sections());
    let assert_nothing_ran_or_written = |call: &str| {
        assert!(
            !work_dir.join(ACTIVATION_LOG).exists(),
            "{case}: activation ran in {call}"
        );
        for marker in ["barrier.started", "graph-step.started"] {
            assert!(
                !work_dir.join(marker).exists(),
                "{case}: a step ran in {call}"
            );
        }
        for script in &scripts {
            assert!(
                !work_dir.join(script).exists(),
                "{case}: {call} wrote {script}"
            );
        }
    };

    let message = execution_failure(run_steps(args()).await);

    for text in named {
        assert!(
            message.contains(text),
            "{case}: the error does not name {text}:\n{message}"
        );
    }
    assert_nothing_ran_or_written("run_steps");

    let written = create_steps_script(args()).await;

    assert!(written.is_err(), "{case}: create_steps_script succeeded");
    assert_nothing_ran_or_written("create_steps_script");
}

/// A graph that cannot be scheduled fails the build before activation or
/// any step runs, even an undeclared step listed before the invalid ones,
/// and before the debugging scripts are written, with an error naming the
/// steps and paths involved.
#[tokio::test]
async fn invalid_graphs_fail_before_activation_or_any_step() {
    for (case, graphs, named) in invalid_graphs() {
        let roots = Roots::new();
        install_activation_hook(&roots.host, &activation_hook("host"));
        assert_rejected_before_activation(case, &roots.work, &roots.context(), &graphs, &named)
            .await;
    }
}

/// A step whose `cwd` is its own tree output, or inside it, would have that
/// directory cleared right before it starts in it, so it could never run.
/// The graph fails before activation or any step runs, even the earlier
/// undeclared step creating the directory, and what an earlier build left in
/// the tree stays as it was.
#[tokio::test]
async fn steps_cannot_run_inside_their_own_tree_output() {
    // Each `cwd` relative to the work directory, and whether the step spells
    // it as an absolute path.
    let cwds = [
        ("cwd is the tree", "build", false),
        ("cwd is the tree as an absolute path", "build", true),
        ("cwd inside the tree", "build/cmake", false),
    ];
    for (case, cwd, absolute) in cwds {
        let roots = Roots::new();
        install_activation_hook(&roots.host, &activation_hook("host"));
        let tree = create_dir(&roots.work, "build");
        let cwd = if absolute {
            roots.work.join(cwd)
        } else {
            PathBuf::from(cwd)
        };
        let earlier = tree.join("CMakeCache.txt");
        fs::write(&earlier, "earlier configure\n").unwrap();
        let started = roots.work.join("graph-step.started");
        let sections = || {
            vec![
                step(&[make_dir(&roots.work.join(&cwd)), mark_started("barrier")]),
                BuildScriptSection {
                    cwd: Some(cwd.clone()),
                    ..declared(
                        step(&[write_line(&started, "started")]),
                        graph(
                            "compile",
                            vec![],
                            vec![tree_output(StepRoot::Work, "build")],
                        ),
                    )
                },
            ]
        };

        assert_sections_rejected_before_activation(
            case,
            &roots.work,
            &roots.context(),
            sections,
            &[],
        )
        .await;

        assert_eq!(read_records(&earlier), ["earlier configure"], "{case}");
    }
}

/// When the host and build environments share one prefix, `host` and
/// `build` paths name the same files: a `build` input waits for the
/// producer of the same `host` path.
#[tokio::test]
async fn shared_prefix_build_inputs_wait_for_host_producers() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    let shared = create_dir(&prefix, "share");

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &work_dir,
        vec![
            declared(
                step(&[
                    mark_started("consumer"),
                    copy_file(&shared.join("tool.txt"), Path::new("copied.txt")),
                ]),
                graph(
                    "consumer",
                    vec![file_input(StepRoot::Build, "share/tool.txt")],
                    vec![file_output(StepRoot::Work, "copied.txt")],
                ),
            ),
            declared(
                step(&[
                    watch_for("producer", "consumer", EXCLUSION_WAIT_SECONDS),
                    write_line(&shared.join("tool.txt"), "fresh"),
                ]),
                graph(
                    "producer",
                    vec![],
                    vec![file_output(StepRoot::Host, "share/tool.txt")],
                ),
            ),
        ],
    );

    run_steps(args).await.unwrap();

    assert_eq!(observed(&work_dir, "producer", "consumer"), [ALONE]);
    assert_eq!(read_records(&work_dir.join("copied.txt")), ["fresh"]);
}

/// When the host and build environments share one prefix, a `host` output
/// inside a `build` output tree is a second producer of files in that tree,
/// which fails the build before activation.
#[tokio::test]
async fn shared_prefix_host_and_build_outputs_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = create_dir(tmp.path(), "work");
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, &activation_hook("host"));

    assert_rejected_before_activation(
        "host and build outputs in one prefix",
        &work_dir,
        &shared_context(RuntimeEnv::current(), &prefix),
        &[
            graph(
                "host-writer",
                vec![],
                vec![file_output(StepRoot::Host, "share/out.txt")],
            ),
            graph(
                "build-writer",
                vec![],
                vec![tree_output(StepRoot::Build, "share")],
            ),
        ],
        &["host-writer", "build-writer"],
    )
    .await;
}

/// On Windows, a path component that is itself a drive path, such as
/// `scratch/C:/...`, would name a file outside the work directory once
/// joined to it. Outputs declared that way fail before activation, and the
/// files and directories they point at stay untouched, although outputs are
/// otherwise cleared before their step starts.
#[cfg(windows)]
#[tokio::test]
async fn windows_outputs_with_embedded_drives_never_touch_other_files() {
    let roots = Roots::new();
    let outside = create_dir(roots._tmp.path(), "outside");
    let victim = outside.join("victim.txt");
    let as_component = |path: &Path| format!("scratch/{}", path.display()).replace('\\', "/");
    let cases = [
        (
            "file output at a drive path",
            file_output(StepRoot::Work, &as_component(&victim)),
        ),
        (
            "tree output at a drive path",
            tree_output(StepRoot::Work, &as_component(&outside)),
        ),
    ];
    for (case, output) in cases {
        fs::write(&victim, "outside the work directory\n").unwrap();
        install_activation_hook(&roots.host, &activation_hook("host"));

        assert_rejected_before_activation(
            case,
            &roots.work,
            &roots.context(),
            &[graph("drive-claimer", vec![], vec![output])],
            &["drive-claimer"],
        )
        .await;

        assert_eq!(
            read_records(&victim),
            ["outside the work directory"],
            "{case}"
        );
    }
}

/// On Windows, trailing dots and spaces are dropped from file names, so
/// `out.txt.` and `out.txt ` name the file `out.txt`. Two steps declaring
/// such spellings as outputs would write one file, so the graph fails before
/// activation and leaves the file an earlier build wrote alone.
#[cfg(windows)]
#[tokio::test]
async fn windows_trailing_dot_and_space_aliases_do_not_create_second_owners() {
    let aliases = [
        ("trailing dot", "share/out.txt."),
        ("trailing space", "share/out.txt "),
        ("trailing dot in a directory", "share./out.txt"),
    ];
    for (case, alias) in aliases {
        let roots = Roots::new();
        install_activation_hook(&roots.host, &activation_hook("host"));
        let existing = create_dir(&roots.work, "share").join("out.txt");
        fs::write(&existing, "earlier build\n").unwrap();

        assert_rejected_before_activation(
            case,
            &roots.work,
            &roots.context(),
            &[
                graph(
                    "plain-owner",
                    vec![],
                    vec![file_output(StepRoot::Work, "share/out.txt")],
                ),
                graph(
                    "alias-owner",
                    vec![],
                    vec![file_output(StepRoot::Work, alias)],
                ),
            ],
            &["alias-owner"],
        )
        .await;

        assert_eq!(read_records(&existing), ["earlier build"], "{case}");
    }
}

/// Windows may give `conda_build_steps` the short name `CONDA_~1`, which
/// the guard on the files Rattler-Build writes would not recognize.
/// Declared paths with a component spelled like a short name fail before
/// activation, and the step scripts of an earlier build stay as they were.
#[cfg(windows)]
#[tokio::test]
async fn windows_short_names_cannot_claim_the_files_rattler_build_writes() {
    let claims = [
        (
            "step scripts by short name",
            tree_output(StepRoot::Work, "CONDA_~1"),
        ),
        (
            "step scripts by short name in another case",
            tree_output(StepRoot::Work, "conda_~1"),
        ),
        (
            "one step script below a short name",
            file_output(StepRoot::Work, "CONDA_~1/step_1/conda_build.bat"),
        ),
    ];
    for (case, output) in claims {
        let roots = Roots::new();
        install_activation_hook(&roots.host, &activation_hook("host"));
        let steps_dir = create_dir(&roots.work, "conda_build_steps");
        let script = create_dir(&steps_dir, "step_1").join("conda_build.bat");
        fs::write(&script, "earlier build\n").unwrap();
        // Where the volume creates short names, the claimed name really is
        // the step scripts directory.
        let alias = roots.work.join("CONDA_~1");
        if alias.exists() {
            assert_eq!(
                fs::canonicalize(&alias).unwrap(),
                fs::canonicalize(&steps_dir).unwrap(),
                "{case}"
            );
        }

        assert_rejected_before_activation(
            case,
            &roots.work,
            &roots.context(),
            &[graph("short-name-claimer", vec![], vec![output])],
            &[],
        )
        .await;

        assert_eq!(read_records(&script), ["earlier build"], "{case}");
    }
}

/// Declared outputs cannot claim the files Rattler-Build itself writes in
/// the work directory, which would otherwise be cleared before their step
/// starts: such a graph fails before activation, and the log of an earlier
/// build stays as it was.
#[tokio::test]
async fn outputs_cannot_claim_the_files_rattler_build_writes() {
    let mut claims = vec![
        ("build log", file_output(StepRoot::Work, "conda_build.log")),
        (
            "step scripts",
            tree_output(StepRoot::Work, "conda_build_steps"),
        ),
        (
            "one step script",
            file_output(StepRoot::Work, "conda_build_steps/step_1/conda_build.sh"),
        ),
        (
            "activation script",
            file_output(StepRoot::Work, "build_env.sh"),
        ),
        (
            "replay script",
            file_output(StepRoot::Work, "conda_build.bat"),
        ),
    ];
    if cfg!(any(windows, target_os = "macos")) {
        claims.push((
            "build log in another case",
            file_output(StepRoot::Work, "Conda_Build.LOG"),
        ));
        claims.push((
            "step scripts in another case",
            tree_output(StepRoot::Work, "Conda_Build_Steps"),
        ));
    }
    for (case, output) in claims {
        let roots = Roots::new();
        install_activation_hook(&roots.host, &activation_hook("host"));
        fs::write(roots.work.join("conda_build.log"), "earlier build\n").unwrap();

        assert_rejected_before_activation(
            case,
            &roots.work,
            &roots.context(),
            &[graph("claimer", vec![], vec![output])],
            &["claimer"],
        )
        .await;

        assert_eq!(
            read_records(&roots.work.join("conda_build.log")),
            ["earlier build"],
            "{case}"
        );
    }
}

/// A work directory that is a prefix, lies inside one, or contains one
/// would let clearing a declared `work` output remove files installed in
/// that prefix. Such a build fails before activation or any step runs and
/// before any script is written, and the installed file stays as it was.
#[tokio::test]
async fn work_directory_overlapping_a_prefix_is_rejected_before_clearing_outputs() {
    // Directories relative to a temporary directory: the work directory,
    // the host and build prefixes, and a file installed in a prefix that is
    // also inside the work directory.
    let mut layouts = vec![
        (
            "work directory is the shared prefix",
            "prefix",
            "prefix",
            "prefix",
            "prefix/bin/tool.txt",
        ),
        (
            "work directory is the build prefix",
            "build",
            "host",
            "build",
            "build/bin/tool.txt",
        ),
        (
            "work directory inside the host prefix",
            "host/work",
            "host",
            "build",
            "host/work/bin/tool.txt",
        ),
        (
            "host prefix inside the work directory",
            "work",
            "work/host",
            "build",
            "work/host/bin/tool.txt",
        ),
    ];
    if cfg!(any(windows, target_os = "macos")) {
        layouts.push((
            "work directory is the prefix spelled in another case",
            "PREFIX",
            "prefix",
            "prefix",
            "PREFIX/bin/tool.txt",
        ));
    }
    for (case, work, host, build, installed) in layouts {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = create_dir(tmp.path(), work);
        let host_prefix = create_dir(tmp.path(), host);
        let build_prefix = create_dir(tmp.path(), build);
        let installed_file = tmp.path().join(installed);
        fs::create_dir_all(installed_file.parent().unwrap()).unwrap();
        fs::write(&installed_file, "from a package\n").unwrap();
        install_activation_hook(&host_prefix, &activation_hook("host"));
        let context = if host == build {
            shared_context(RuntimeEnv::current(), &host_prefix)
        } else {
            ExecutionContext::separate(
                RuntimeEnv::current(),
                &build_prefix,
                Platform::current(),
                &host_prefix,
                Platform::current(),
            )
        };
        let output = Path::new(installed)
            .strip_prefix(work)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");

        assert_rejected_before_activation(
            case,
            &work_dir,
            &context,
            &[graph(
                "work-writer",
                vec![],
                vec![file_output(StepRoot::Work, &output)],
            )],
            &[],
        )
        .await;

        assert_eq!(read_records(&installed_file), ["from a package"], "{case}");
    }
}

/// A work directory reached through a symbolic link to a prefix is that
/// prefix: declared steps fail before activation, and the file installed
/// there stays as it was.
#[cfg(unix)]
#[tokio::test]
async fn work_directory_linked_to_a_prefix_is_rejected_before_clearing_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = create_dir(tmp.path(), "prefix");
    let installed_file = create_dir(&prefix, "bin").join("tool.txt");
    fs::write(&installed_file, "from a package\n").unwrap();
    install_activation_hook(&prefix, &activation_hook("host"));
    let work_dir = tmp.path().join("work link");
    std::os::unix::fs::symlink(&prefix, &work_dir).unwrap();

    assert_rejected_before_activation(
        "work directory linked to the prefix",
        &work_dir,
        &shared_context(RuntimeEnv::current(), &prefix),
        &[graph(
            "work-writer",
            vec![],
            vec![file_output(StepRoot::Work, "bin/tool.txt")],
        )],
        &[],
    )
    .await;

    assert_eq!(read_records(&installed_file), ["from a package"]);
}

/// Builds whose steps declare nothing never clear outputs, so they keep
/// running when the work directory is the prefix.
#[tokio::test]
async fn undeclared_steps_still_run_when_the_work_directory_is_the_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = create_dir(tmp.path(), "prefix");
    install_activation_hook(&prefix, &activation_hook("host"));

    let args = execution_args(
        shared_context(RuntimeEnv::current(), &prefix),
        &prefix,
        vec![
            step(&[mark_started("first")]),
            step(&[mark_started("second")]),
        ],
    );

    run_steps(args).await.unwrap();

    assert_eq!(read_records(&prefix.join(ACTIVATION_LOG)), ["host"]);
    assert!(
        prefix.join("first.started").exists(),
        "first step never ran"
    );
    assert!(
        prefix.join("second.started").exists(),
        "second step never ran"
    );
}

/// Declared outputs never remove anything from the host and build
/// prefixes, which hold installed packages: a step whose output already
/// exists there fails without starting and leaves the file alone, while an
/// empty directory at a tree output does not stop its producer.
#[tokio::test]
async fn existing_prefix_outputs_are_never_removed() {
    let roots = Roots::new();
    let installed = create_dir(&roots.host, "lib").join("installed.txt");
    fs::write(&installed, "from a package\n").unwrap();
    let empty_tree = create_dir(&roots.build, "empty-tree");

    let args = roots.args(vec![
        declared(
            step(&[
                make_dir(&empty_tree),
                write_line(&empty_tree.join("made.txt"), "made"),
            ]),
            graph(
                "tree-writer",
                vec![],
                vec![tree_output(StepRoot::Build, "empty-tree")],
            ),
        ),
        step(&[mark_started("barrier")]),
        declared(
            step(&[mark_started("overwriter")]),
            graph(
                "overwriter",
                vec![],
                vec![file_output(StepRoot::Host, "lib/installed.txt")],
            ),
        ),
    ]);

    let message = execution_failure(run_steps(args).await);

    assert!(message.contains("lib/installed.txt"), "{message}");
    assert_eq!(read_records(&empty_tree.join("made.txt")), ["made"]);
    assert!(
        roots.work.join("barrier.started").exists(),
        "barrier never ran"
    );
    assert!(!roots.work.join("overwriter.started").exists());
    assert_eq!(read_records(&installed), ["from a package"]);
}

/// A declared output below a linked directory (a symbolic link on Unix, a
/// junction on Windows) is never cleared through the link: its step fails
/// without starting, naming the output, and what the link points to stays
/// intact.
#[tokio::test]
async fn outputs_below_a_linked_directory_are_not_cleared_through_the_link() {
    let roots = Roots::new();
    let outside = create_dir(roots._tmp.path(), "outside");
    let kept = create_dir(&outside, "tree").join("keep.txt");
    fs::write(&kept, "keep\n").unwrap();
    link_dir(&outside, &roots.work.join("linked"));

    let args = roots.args(vec![declared(
        step(&[mark_started("through-link")]),
        graph(
            "through-link",
            vec![],
            vec![tree_output(StepRoot::Work, "linked/tree")],
        ),
    )]);

    let message = execution_failure(run_steps(args).await);

    assert!(message.contains("linked/tree"), "{message}");
    assert!(!roots.work.join("through-link.started").exists());
    assert_eq!(read_records(&kept), ["keep"]);
}

/// A link to a directory that an earlier build left at a tree output in the
/// work directory is removed as a link: the step creates a real tree there,
/// and the directory the link pointed to keeps its files and gets none of
/// the step's.
#[tokio::test]
async fn a_link_at_a_work_tree_output_is_removed_without_touching_its_target() {
    let roots = Roots::new();
    let outside = create_dir(roots._tmp.path(), "outside");
    fs::write(outside.join("keep.txt"), "keep\n").unwrap();
    let tree = roots.work.join("tree");
    link_dir(&outside, &tree);

    let args = roots.args(vec![declared(
        step(&[make_dir(&tree), write_line(&tree.join("made.txt"), "made")]),
        graph(
            "tree-writer",
            vec![],
            vec![tree_output(StepRoot::Work, "tree")],
        ),
    )]);
    let result = run_steps(args).await;

    assert_eq!(
        read_records(&outside.join("keep.txt")),
        ["keep"],
        "{result:?}"
    );
    assert!(
        !outside.join("made.txt").exists(),
        "the step wrote through the stale link: {result:?}"
    );
    result.unwrap();
    assert!(fs::symlink_metadata(&tree).unwrap().file_type().is_dir());
    assert_eq!(read_records(&tree.join("made.txt")), ["made"]);
}

/// Symbolic links that an earlier build left at file outputs in the work
/// directory are removed as links before the step starts, so writing the
/// outputs creates files in the work directory: a link's existing target
/// keeps its contents, and a dangling link's target is never created.
#[tokio::test]
async fn links_at_work_file_outputs_cannot_redirect_the_step() {
    let roots = Roots::new();
    let outside = create_dir(roots._tmp.path(), "outside");
    let existing = outside.join("existing.txt");
    fs::write(&existing, "keep\n").unwrap();
    let escaped = outside.join("escaped.txt");
    let over_existing = roots.work.join("over-existing.txt");
    let over_dangling = roots.work.join("over-dangling.txt");
    if !symlink_file(&existing, &over_existing) || !symlink_file(&escaped, &over_dangling) {
        eprintln!("skipping: this user cannot create symbolic links");
        return;
    }

    let args = roots.args(vec![declared(
        step(&[
            write_line(&over_existing, "inside"),
            write_line(&over_dangling, "inside"),
        ]),
        graph(
            "writer",
            vec![],
            vec![
                file_output(StepRoot::Work, "over-existing.txt"),
                file_output(StepRoot::Work, "over-dangling.txt"),
            ],
        ),
    )]);
    let result = run_steps(args).await;

    assert_eq!(read_records(&existing), ["keep"], "{result:?}");
    assert!(
        !escaped.exists(),
        "the step wrote through the stale link: {result:?}"
    );
    result.unwrap();
    for written in [&over_existing, &over_dangling] {
        assert!(
            fs::symlink_metadata(written).unwrap().file_type().is_file(),
            "{} is not a regular file",
            written.display()
        );
        assert_eq!(read_records(written), ["inside"]);
    }
}

/// A file output may be a symbolic link whose target does not exist, like
/// the `libfoo.so -> libfoo.so.1` links packages ship apart from their
/// targets.
#[cfg(unix)]
#[tokio::test]
async fn a_step_may_produce_a_file_output_as_a_dangling_link() {
    let roots = Roots::new();
    let link = create_dir(&roots.host, "lib").join("libdemo.so");

    let args = roots.args(vec![declared(
        step(&[format!("ln -s libdemo.so.1 '{}'", link.display())]),
        graph(
            "linker",
            vec![],
            vec![file_output(StepRoot::Host, "lib/libdemo.so")],
        ),
    )]);

    run_steps(args).await.unwrap();
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("libdemo.so.1"),
        "the output is not the link the step created"
    );
}

/// The `conda_build.<ext>` written for debugging activates once and then
/// runs the steps one at a time in dependency order rather than list order,
/// keeping barriers in place, just like `run_steps` orders them.
#[tokio::test]
async fn build_script_replays_graph_steps_serially_in_dependency_order() {
    let roots = Roots::new();
    install_activation_hook(&roots.host, &activation_hook("host"));
    let order = Path::new("order.txt");

    let args = roots.args(vec![
        step(&[
            append_line(order, "seed"),
            write_line(Path::new("seed.txt"), "seeded"),
        ]),
        declared(
            step(&[
                append_line(order, "consumer"),
                copy_file(Path::new("generated.txt"), Path::new("final.txt")),
            ]),
            graph(
                "consumer",
                vec![file_input(StepRoot::Work, "generated.txt")],
                vec![file_output(StepRoot::Work, "final.txt")],
            ),
        ),
        declared(
            step(&[
                append_line(order, "producer"),
                copy_file(Path::new("seed.txt"), Path::new("generated.txt")),
            ]),
            graph(
                "producer",
                vec![file_input(StepRoot::Work, "seed.txt")],
                vec![file_output(StepRoot::Work, "generated.txt")],
            ),
        ),
        declared(
            step(&[rendezvous("x", "y", EXCLUSION_WAIT_SECONDS)]),
            graph("x", vec![], vec![]),
        ),
        declared(
            step(&[rendezvous("y", "x", EXCLUSION_WAIT_SECONDS)]),
            graph("y", vec![], vec![]),
        ),
        step(&[append_line(order, "finish")]),
    ]);
    let outputs = [
        ACTIVATION_LOG,
        "order.txt",
        "seed.txt",
        "generated.txt",
        "final.txt",
        "x.started",
        "y.started",
        "x-y.txt",
        "y-x.txt",
    ];
    let assert_ordered = |run: &str| {
        assert_eq!(
            read_records(&roots.work.join(ACTIVATION_LOG)),
            ["host"],
            "{run}"
        );
        assert_eq!(
            read_records(&roots.work.join(order)),
            ["seed", "producer", "consumer", "finish"],
            "{run}"
        );
        assert_eq!(
            read_records(&roots.work.join("final.txt")),
            ["seeded"],
            "{run}"
        );
    };

    run_steps(args).await.unwrap();
    assert_ordered("run_steps");

    for file in outputs {
        fs::remove_file(roots.work.join(file)).unwrap();
    }
    let output = run_build_script_manually(&roots.work);
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ordered("build script replay");
    let mut independent = [
        observed(&roots.work, "x", "y"),
        observed(&roots.work, "y", "x"),
    ];
    independent.sort();
    assert_eq!(
        independent,
        [vec![ALONE.to_string()], vec![MET.to_string()]],
        "the replay ran independent steps at the same time"
    );
}

/// On Windows, paths name the same artifact whatever their case or
/// separators, so consumers spelling an input differently from its
/// producer's output still wait for that producer.
#[cfg(windows)]
#[tokio::test]
async fn windows_paths_identify_artifacts_case_insensitively() {
    let roots = Roots::new();
    let generated = create_dir(&roots.work, "gen");
    fs::write(generated.join("value.txt"), "stale\n").unwrap();

    let args = roots.args(vec![
        declared(
            step(&[
                mark_started("exact-reader"),
                copy_file(Path::new(r"GEN\Value.TXT"), Path::new("copied.txt")),
            ]),
            graph(
                "exact-reader",
                vec![file_input(StepRoot::Work, r"GEN\Value.TXT")],
                vec![file_output(StepRoot::Work, "copied.txt")],
            ),
        ),
        declared(
            step(&[concatenate(
                &generated,
                "*.txt",
                &roots.work.join("globbed.txt"),
            )]),
            graph(
                "glob-reader",
                vec![glob_input(StepRoot::Work, "Gen/*.TXT")],
                vec![file_output(StepRoot::Work, "globbed.txt")],
            ),
        ),
        declared(
            step(&[
                watch_for("producer", "exact-reader", EXCLUSION_WAIT_SECONDS),
                write_line(&generated.join("value.txt"), "fresh"),
            ]),
            graph(
                "producer",
                vec![],
                vec![file_output(StepRoot::Work, "gen/value.txt")],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "producer", "exact-reader"), [ALONE]);
    assert_eq!(read_records(&roots.work.join("copied.txt")), ["fresh"]);
    assert_eq!(read_records(&roots.work.join("globbed.txt")), ["fresh"]);
}
