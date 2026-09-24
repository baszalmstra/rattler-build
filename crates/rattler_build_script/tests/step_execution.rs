//! Real-process coverage for `run_steps`, which activates once and starts
//! every build step as an independent process from the captured exported
//! environment, and for the legacy single-wrapper `run_script`.

#![cfg(feature = "execution")]

use std::path::{Path, PathBuf};

use fs_err as fs;
use indexmap::IndexMap;
use rattler_build_script::{
    BuildScriptSection, EnvironmentIsolation, ExecutionArgs, ExecutionContext, InterpreterError,
    ResolvedScriptContents, RuntimeEnv, run_steps,
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
