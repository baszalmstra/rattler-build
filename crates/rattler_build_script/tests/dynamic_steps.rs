//! Real-process coverage for build steps that declare further steps once they
//! succeeded: the step manifest and input report a step may write to the files
//! named by `RATTLER_BUILD_STEP_MANIFEST` and `RATTLER_BUILD_STEP_INPUTS`, the
//! registration of what it declares, recursive expansion, the difference
//! between `depends_on` (a generating step with everything it generated) and
//! `discover_after` (only its registration), the `conda_build.<ext>` replay of
//! a build that generated steps, and how invalid declarations fail the build
//! without releasing any step.
//!
//! Most generating steps copy a declaration file the test prepared outside the
//! work directory, so every test shows the exact JSON a step declares. The
//! steps they declare do the work whose files the tests check.

#![cfg(feature = "execution")]

use std::path::{Path, PathBuf};

use fs_err as fs;
use indexmap::IndexMap;
use rattler_build_script::{
    BuildScriptSection, DeclarationPaths, EnvironmentIsolation, ExecutionArgs, ExecutionContext,
    GraphStep, ResolvedScriptContents, RuntimeEnv, STEP_INPUTS_ENV, STEP_MANIFEST_ENV, StepInput,
    StepInputKind, StepOutput, StepOutputKind, StepRoot, create_steps_script, run_steps,
};
use rattler_conda_types::Platform;
use serde_json::{Value, json};

const ADDED: &str = "RB_STEP_ADDED";
const CHANGED: &str = "RB_STEP_CHANGED";
const LOCAL: &str = "RB_STEP_LOCAL";
const LEAK: &str = "RB_STEP_LEAK";
const SECRET: &str = "RB_STEP_SECRET";
const SECRET_VALUE: &str = "rb-secret-value";

/// File the activation hook appends to each time it runs, in the work
/// directory, where activation runs.
const ACTIVATION_LOG: &str = "activations.txt";

/// Activation hook contents: records `name` in [`ACTIVATION_LOG`], exports
/// [`ADDED`] and changes [`CHANGED`].
fn activation_hook(name: &str) -> String {
    if cfg!(windows) {
        format!(
            "@echo {name}>>\"{ACTIVATION_LOG}\"\r\n\
             @set \"{ADDED}=added\"\r\n\
             @set \"{CHANGED}=after\"\r\n"
        )
    } else {
        format!(
            "echo {name} >> {ACTIVATION_LOG}\n\
             export {ADDED}=added\n\
             export {CHANGED}=after\n"
        )
    }
}

/// Installs `contents` as an activation hook in `prefix/etc/conda/activate.d`.
fn install_activation_hook(prefix: &Path, contents: &str) {
    let extension = if cfg!(windows) { "bat" } else { "sh" };
    let hook_dir = prefix.join("etc").join("conda").join("activate.d");
    fs::create_dir_all(&hook_dir).unwrap();
    fs::write(hook_dir.join(format!("rb_step_test.{extension}")), contents).unwrap();
}

/// `path`, written with `/`, spelled with the separator native commands
/// expect.
fn file(path: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.replace('/', "\\"))
    } else {
        PathBuf::from(path)
    }
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

/// Native command appending the contents of the file `from` to `to`.
fn append_file(from: &Path, to: &Path) -> String {
    if cfg!(windows) {
        format!("@type \"{}\">>\"{}\"", from.display(), to.display())
    } else {
        format!("cat '{}' >> '{}'", from.display(), to.display())
    }
}

/// Native command writing the lines of `from` that contain `pattern` to `to`.
fn extract_lines(pattern: &str, from: &Path, to: &Path) -> String {
    if cfg!(windows) {
        format!(
            "@findstr /c:\"{pattern}\" \"{}\">\"{}\"",
            from.display(),
            to.display()
        )
    } else {
        format!(
            "grep -F '{pattern}' '{}' > '{}'",
            from.display(),
            to.display()
        )
    }
}

/// Native command writing `seen` to `record` when `path` exists, and
/// `missing` otherwise.
fn record_presence(path: &Path, record: &str) -> String {
    if cfg!(windows) {
        format!(
            "@if exist \"{}\" (echo seen>\"{record}\") else (echo missing>\"{record}\")",
            path.display()
        )
    } else {
        format!(
            "if [ -e '{}' ]; then echo seen; else echo missing; fi > '{record}'",
            path.display()
        )
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

/// Native command copying the prepared declaration file `plan` to the file
/// the environment variable `variable` names.
fn declare(variable: &str, plan: &Path) -> String {
    if cfg!(windows) {
        format!("@copy /y \"{}\" \"%{variable}%\" >nul", plan.display())
    } else {
        format!("cp '{}' \"${variable}\"", plan.display())
    }
}

/// Native command declaring the step manifest prepared at `plan`.
fn emit(plan: &Path) -> String {
    declare(STEP_MANIFEST_ENV, plan)
}

/// Native command declaring the input report prepared at `plan`.
fn report(plan: &Path) -> String {
    declare(STEP_INPUTS_ENV, plan)
}

/// Native command declaring the step manifest prepared at `plan` only while
/// that file exists.
fn emit_if_present(plan: &Path) -> String {
    if cfg!(windows) {
        format!(
            "@if exist \"{0}\" copy /y \"{0}\" \"%{STEP_MANIFEST_ENV}%\" >nul",
            plan.display()
        )
    } else {
        format!(
            "if [ -e '{0}' ]; then cp '{0}' \"${STEP_MANIFEST_ENV}\"; fi",
            plan.display()
        )
    }
}

/// Native command creating the file the environment variable `variable`
/// names, empty.
fn truncate(variable: &str) -> String {
    if cfg!(windows) {
        format!("@type nul>\"%{variable}%\"")
    } else {
        format!(": > \"${variable}\"")
    }
}

/// Native commands appending the paths of the step's declaration files, the
/// manifest first, to `file`.
fn record_declaration_paths(file: &str) -> String {
    if cfg!(windows) {
        format!("@echo %{STEP_MANIFEST_ENV}%>>\"{file}\"\n@echo %{STEP_INPUTS_ENV}%>>\"{file}\"")
    } else {
        format!("printf '%s\\n' \"${STEP_MANIFEST_ENV}\" \"${STEP_INPUTS_ENV}\" >> '{file}'")
    }
}

/// The declaration files a step recorded in `file` of the work directory with
/// [`record_declaration_paths`].
fn recorded_declaration_paths(work: &Path, file: &str) -> DeclarationPaths {
    let records = read_records(&work.join(file));
    let [manifest, inputs] = records.as_slice() else {
        panic!("{file} does not hold two paths: {records:?}");
    };
    DeclarationPaths {
        manifest: PathBuf::from(manifest),
        inputs: PathBuf::from(inputs),
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

/// Seconds a step waits for a peer it has to run at the same time as. A
/// passing run returns as soon as the peer starts; the tests that wait this
/// long skip on hosts that run steps one at a time.
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

/// Native command creating `<name>.started` in the step's working directory,
/// announcing that step `name` runs.
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
/// `seconds` (see [`watch_for`]).
fn rendezvous(me: &str, peer: &str, seconds: u32) -> String {
    format!("{}\n{}", mark_started(me), watch_for(me, peer, seconds))
}

/// What step `me` recorded about `peer` with [`watch_for`].
fn observed(work_dir: &Path, me: &str, peer: &str) -> Vec<String> {
    read_records(&work_dir.join(format!("{me}-{peer}.txt")))
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

/// Declaration of a graph step named `id` with `inputs` and `outputs`.
fn graph(id: &str, inputs: Vec<StepInput>, outputs: Vec<StepOutput>) -> GraphStep {
    GraphStep {
        id: Some(id.to_string()),
        inputs: Some(inputs),
        outputs: Some(outputs),
        depends_on: Vec::new(),
        discover_after: Vec::new(),
    }
}

/// `graph`, additionally waiting for the steps named `ids`, with everything
/// they generate.
fn after(graph: GraphStep, ids: &[&str]) -> GraphStep {
    GraphStep {
        depends_on: ids.iter().map(|id| id.to_string()).collect(),
        ..graph
    }
}

/// `graph`, additionally waiting until the steps named `ids` have registered
/// the steps they declare.
fn discovering(graph: GraphStep, ids: &[&str]) -> GraphStep {
    GraphStep {
        discover_after: ids.iter().map(|id| id.to_string()).collect(),
        ..graph
    }
}

/// `section` scheduled as `graph`.
fn declared(section: BuildScriptSection, graph: GraphStep) -> BuildScriptSection {
    BuildScriptSection { graph, ..section }
}

/// The message of the error `result` must be.
fn failure<E: std::fmt::Display>(result: Result<(), E>) -> String {
    match result {
        Ok(()) => panic!("the build succeeded"),
        Err(error) => error.to_string(),
    }
}

/// A manifest input or output `path` of the work directory.
fn work(path: &str) -> Value {
    json!({ "root": "work", "path": path })
}

/// A manifest input or output `path` of the host prefix.
fn host(path: &str) -> Value {
    json!({ "root": "host", "path": path })
}

/// A generated step `id` running the native commands `lines`, declaring
/// `inputs` and `outputs`.
fn generated(id: &str, lines: &[String], inputs: Vec<Value>, outputs: Vec<Value>) -> Value {
    json!({
        "id": id,
        "run": lines.join("\n"),
        "inputs": inputs,
        "outputs": outputs,
    })
}

/// A generated step `id` running the native commands `lines`, declaring
/// neither inputs nor outputs.
fn undeclared(id: &str, lines: &[String]) -> Value {
    json!({ "id": id, "run": lines.join("\n") })
}

/// `step` with `field` set to `value`.
fn with(mut step: Value, field: &str, value: Value) -> Value {
    step[field] = value;
    step
}

/// A version 1 step manifest declaring `steps`.
fn manifest(steps: Vec<Value>) -> Value {
    json!({ "version": 1, "steps": steps })
}

fn document(value: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(value).unwrap()
}

/// A work directory with separate host and build prefixes, and a directory
/// outside all of them for the declaration files steps copy into place.
struct Roots {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    host: PathBuf,
    build: PathBuf,
    plans: PathBuf,
}

impl Roots {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let work = create_dir(tmp.path(), "work");
        let host = create_dir(tmp.path(), "host prefix");
        let build = create_dir(tmp.path(), "build prefix");
        let plans = create_dir(tmp.path(), "plans");
        Self {
            _tmp: tmp,
            work,
            host,
            build,
            plans,
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

    /// Writes `contents` to the prepared declaration file `name` and returns
    /// its path.
    fn plan(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.plans.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
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

/// The status, standard output and standard error of `output`, for messages.
fn describe(output: &std::process::Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A step's manifest is read once the step succeeded: the steps it declares
/// run after it, a consumer with `discover_after` waits for that registration
/// even though a stale copy of its input exists, and then for the generated
/// producer of that input. Every step, generated or not, gets its own
/// declaration files in the work directory's `conda_build_steps`, and a
/// manifest stays there, byte for byte, after the build.
#[tokio::test]
async fn generated_steps_run_once_their_generator_succeeded() {
    let roots = Roots::new();
    fs::write(roots.work.join("staged.txt"), "stale\n").unwrap();
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![generated(
            "stage",
            &[
                record_declaration_paths("stage-paths.txt"),
                copy_file(&file("seed.txt"), &file("staged.txt")),
                append_line(&file("staged.txt"), "staged"),
            ],
            vec![work("seed.txt")],
            vec![work("staged.txt"), work("stage-paths.txt")],
        )])),
    );

    let args = roots.args(vec![
        declared(
            step(&[
                mark_started("consumer"),
                record_declaration_paths("consumer-paths.txt"),
                copy_file(&file("staged.txt"), &file("final.txt")),
            ]),
            discovering(
                graph(
                    "consumer",
                    vec![file_input(StepRoot::Work, "staged.txt")],
                    vec![
                        file_output(StepRoot::Work, "final.txt"),
                        file_output(StepRoot::Work, "consumer-paths.txt"),
                    ],
                ),
                &["gen"],
            ),
        ),
        declared(
            step(&[
                record_declaration_paths("gen-paths.txt"),
                write_line(&file("seed.txt"), "seed"),
                watch_for("gen", "consumer", EXCLUSION_WAIT_SECONDS),
                emit(&plan),
            ]),
            graph(
                "gen",
                vec![],
                vec![
                    file_output(StepRoot::Work, "seed.txt"),
                    file_output(StepRoot::Work, "gen-paths.txt"),
                ],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "gen", "consumer"), [ALONE]);
    assert_eq!(
        read_records(&roots.work.join("final.txt")),
        ["seed", "staged"]
    );

    let steps_dir = fs::canonicalize(roots.work.join("conda_build_steps")).unwrap();
    let mut dirs = Vec::new();
    for record in ["gen-paths.txt", "stage-paths.txt", "consumer-paths.txt"] {
        let paths = recorded_declaration_paths(&roots.work, record);
        let dir = paths.manifest.parent().unwrap().to_path_buf();
        assert_eq!(paths, DeclarationPaths::in_dir(&dir), "{record}");
        assert_eq!(
            fs::canonicalize(&dir).unwrap().parent(),
            Some(steps_dir.as_path()),
            "{record}: declaration files outside conda_build_steps"
        );
        dirs.push(dir);
    }
    dirs.sort();
    dirs.dedup();
    assert_eq!(dirs.len(), 3, "steps share declaration files: {dirs:?}");

    let gen_paths = recorded_declaration_paths(&roots.work, "gen-paths.txt");
    assert_eq!(
        fs::read(&gen_paths.manifest).unwrap(),
        fs::read(plan).unwrap()
    );
    assert!(!gen_paths.inputs.exists());
    let stage_paths = recorded_declaration_paths(&roots.work, "stage-paths.txt");
    assert!(!stage_paths.manifest.exists());
}

/// A step does not have to declare anything: a missing or empty manifest or
/// input report, and a manifest of only a version, declare nothing, and
/// `discover_after` such steps waits only for them to succeed.
#[tokio::test]
async fn missing_and_empty_declaration_files_declare_nothing() {
    let roots = Roots::new();
    let bare = roots.plan("bare.json", r#"{"version": 1}"#);

    let args = roots.args(vec![
        declared(
            step(&[mark_started("silent")]),
            graph("silent", vec![], vec![]),
        ),
        declared(
            step(&[truncate(STEP_MANIFEST_ENV), truncate(STEP_INPUTS_ENV)]),
            graph("empty", vec![], vec![]),
        ),
        declared(
            step(&[emit(&bare), report(&bare)]),
            graph("bare", vec![], vec![]),
        ),
        declared(
            step(&[mark_started("consumer")]),
            discovering(
                graph("consumer", vec![], vec![]),
                &["silent", "empty", "bare"],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert!(roots.work.join("silent.started").exists());
    assert!(roots.work.join("consumer.started").exists());
}

/// A manifest an earlier build left behind is never taken for a new
/// declaration: once the generating step stops writing one, neither the
/// build nor the `conda_build.<ext>` replay of that build runs what the
/// earlier manifest declared.
#[tokio::test]
async fn declaration_files_of_an_earlier_build_are_never_read() {
    let roots = Roots::new();
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![generated(
            "extra",
            &[write_line(&file("extra.txt"), "extra")],
            vec![],
            vec![work("extra.txt")],
        )])),
    );
    let sections = || {
        vec![declared(
            step(&[emit_if_present(&plan)]),
            graph("gen", vec![], vec![]),
        )]
    };

    run_steps(roots.args(sections())).await.unwrap();
    assert_eq!(read_records(&roots.work.join("extra.txt")), ["extra"]);

    fs::remove_file(&plan).unwrap();
    fs::remove_file(roots.work.join("extra.txt")).unwrap();
    run_steps(roots.args(sections())).await.unwrap();
    assert!(
        !roots.work.join("extra.txt").exists(),
        "the second build ran a step the first build's manifest declared"
    );

    let output = run_build_script_manually(&roots.work);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        !roots.work.join("extra.txt").exists(),
        "the replay ran a step the first build's manifest declared"
    );
}

/// Generated steps generate further steps: a step declares a child, whose
/// own manifest declares a grandchild. Activation runs once, and every
/// generated step starts from its exported environment with only its own
/// `env`, never with what its generator changed in its shell. A generated
/// `cwd` is relative to the host prefix, like the `cwd` of a recipe step.
/// `depends_on` the first generator, and a barrier listed after it, wait for
/// the grandchild.
#[tokio::test]
async fn generated_steps_generate_further_steps_from_the_captured_environment() {
    let roots = Roots::new();
    install_activation_hook(&roots.host, &activation_hook("host"));
    let located = create_dir(&roots.host, "located");
    let child_plan = roots.plan(
        "child.json",
        document(&manifest(vec![with(
            generated(
                "grandchild",
                &[
                    record_variables("grandchild-env.txt", &[ADDED, CHANGED, LOCAL, LEAK]),
                    copy_file(&file("child-env.txt"), &file("lineage.txt")),
                    append_line(&file("lineage.txt"), "grandchild"),
                ],
                vec![work("child-env.txt")],
                vec![work("grandchild-env.txt"), work("lineage.txt")],
            ),
            "env",
            json!({ CHANGED: "grandchild-override" }),
        )])),
    );
    let root_plan = roots.plan(
        "root.json",
        document(&manifest(vec![
            with(
                generated(
                    "child",
                    &[
                        record_variables("child-env.txt", &[ADDED, CHANGED, LOCAL, LEAK]),
                        set_variable(LEAK, "child-leak"),
                        emit(&child_plan),
                    ],
                    vec![],
                    vec![work("child-env.txt")],
                ),
                "env",
                json!({ LOCAL: "child-local" }),
            ),
            with(
                generated(
                    "located",
                    &[write_line(Path::new("where.txt"), "located")],
                    vec![],
                    vec![host("located/where.txt")],
                ),
                "cwd",
                json!("located"),
            ),
        ])),
    );

    let mut args = roots.args(vec![
        declared(
            step(&[
                set_variable(LEAK, "root-leak"),
                set_variable(CHANGED, "root-mutated"),
                emit(&root_plan),
            ]),
            graph("root", vec![], vec![]),
        ),
        declared(
            step(&[copy_file(&file("lineage.txt"), &file("summary.txt"))]),
            after(
                graph(
                    "summary",
                    vec![file_input(StepRoot::Work, "lineage.txt")],
                    vec![file_output(StepRoot::Work, "summary.txt")],
                ),
                &["root"],
            ),
        ),
        step(&[copy_file(&file("lineage.txt"), &file("barrier.txt"))]),
    ]);
    args.env_vars = IndexMap::from([(CHANGED.to_string(), "before".to_string())]);

    run_steps(args).await.unwrap();

    assert_eq!(read_records(&roots.work.join(ACTIVATION_LOG)), ["host"]);
    let child_env = [
        format!("{ADDED}=added"),
        format!("{CHANGED}=after"),
        format!("{LOCAL}=child-local"),
        format!("{LEAK}=unset"),
    ];
    assert_eq!(read_records(&roots.work.join("child-env.txt")), child_env);
    assert_eq!(
        read_records(&roots.work.join("grandchild-env.txt")),
        [
            format!("{ADDED}=added"),
            format!("{CHANGED}=grandchild-override"),
            format!("{LOCAL}=unset"),
            format!("{LEAK}=unset"),
        ]
    );
    let lineage: Vec<String> = child_env
        .into_iter()
        .chain(["grandchild".to_string()])
        .collect();
    assert_eq!(read_records(&roots.work.join("lineage.txt")), lineage);
    assert_eq!(read_records(&roots.work.join("summary.txt")), lineage);
    assert_eq!(read_records(&roots.work.join("barrier.txt")), lineage);
    assert_eq!(read_records(&located.join("where.txt")), ["located"]);
    assert!(!roots.work.join("where.txt").exists());
}

/// `discover_after` waits only until the generator registered its steps: the
/// early consumer, whose input a stale file already provides, does not start
/// before that, and then runs as soon as the generated producer of its input
/// is done, while another generated step still runs. `depends_on` waits for
/// every step the generator declared.
#[tokio::test]
async fn depends_on_waits_for_the_expansion_but_discover_after_only_for_its_registration() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();
    fs::write(roots.work.join("fast.txt"), "stale\n").unwrap();
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![
            generated(
                "fast",
                &[write_line(&file("fast.txt"), "fresh")],
                vec![],
                vec![work("fast.txt")],
            ),
            generated(
                "slow",
                &[
                    rendezvous("slow", "early", OVERLAP_WAIT_SECONDS),
                    watch_for("slow", "late", EXCLUSION_WAIT_SECONDS),
                    write_line(&file("slow.txt"), "slow"),
                ],
                vec![],
                vec![work("slow.txt")],
            ),
        ])),
    );

    let args = roots.args(vec![
        declared(
            step(&[
                watch_for("gen", "early", EXCLUSION_WAIT_SECONDS),
                emit(&plan),
            ]),
            graph("gen", vec![], vec![]),
        ),
        declared(
            step(&[
                rendezvous("early", "slow", OVERLAP_WAIT_SECONDS),
                copy_file(&file("fast.txt"), &file("early.txt")),
            ]),
            discovering(
                graph(
                    "early",
                    vec![file_input(StepRoot::Work, "fast.txt")],
                    vec![file_output(StepRoot::Work, "early.txt")],
                ),
                &["gen"],
            ),
        ),
        declared(
            step(&[
                mark_started("late"),
                copy_file(&file("slow.txt"), &file("late.txt")),
            ]),
            after(
                graph(
                    "late",
                    vec![file_input(StepRoot::Work, "slow.txt")],
                    vec![file_output(StepRoot::Work, "late.txt")],
                ),
                &["gen"],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "gen", "early"), [ALONE]);
    assert_eq!(observed(&roots.work, "early", "slow"), [MET]);
    assert_eq!(observed(&roots.work, "slow", "early"), [MET]);
    assert_eq!(observed(&roots.work, "slow", "late"), [ALONE]);
    assert_eq!(read_records(&roots.work.join("early.txt")), ["fresh"]);
    assert_eq!(read_records(&roots.work.join("late.txt")), ["slow"]);
}

/// Generator `g` declares `a` and `c`, generator `h` declares `b`, and their
/// files flow `g/a -> h/b -> g/c`. `h/b` waits for `g`'s registration and
/// `g/c` names `h/b`, which `h` may declare later, so neither waits for the
/// other generator's whole expansion, which would be a cycle. That holds
/// whichever generator registers first; a generator waiting for the other one
/// with `discover_after` waits for its registration, not its expansion.
#[tokio::test]
async fn generators_interleave_without_waiting_for_each_others_expansion() {
    let orders: [(&str, &[&str], &[&str]); 2] = [
        ("g registers first", &[], &["g"]),
        ("h registers first", &["h"], &[]),
    ];
    for (case, g_waits_for, h_waits_for) in orders {
        let roots = Roots::new();
        let g_plan = roots.plan(
            "g.json",
            document(&manifest(vec![
                generated(
                    "a",
                    &[write_line(&file("a.txt"), "alpha")],
                    vec![],
                    vec![work("a.txt")],
                ),
                with(
                    generated(
                        "c",
                        &[
                            copy_file(&file("b.txt"), &file("c.txt")),
                            append_line(&file("c.txt"), "gamma"),
                        ],
                        vec![work("b.txt")],
                        vec![work("c.txt")],
                    ),
                    "depends_on",
                    json!(["h/b"]),
                ),
            ])),
        );
        let h_plan = roots.plan(
            "h.json",
            document(&manifest(vec![with(
                generated(
                    "b",
                    &[
                        copy_file(&file("a.txt"), &file("b.txt")),
                        append_line(&file("b.txt"), "beta"),
                    ],
                    vec![work("a.txt")],
                    vec![work("b.txt")],
                ),
                "discover_after",
                json!(["g"]),
            )])),
        );
        let published = create_dir(&roots.host, "share");

        let args = roots.args(vec![
            declared(
                step(&[emit(&g_plan)]),
                discovering(graph("g", vec![], vec![]), g_waits_for),
            ),
            declared(
                step(&[emit(&h_plan)]),
                discovering(graph("h", vec![], vec![]), h_waits_for),
            ),
            declared(
                step(&[copy_file(
                    &file("c.txt"),
                    &published.join("interleaved.txt"),
                )]),
                after(
                    graph(
                        "publish",
                        vec![file_input(StepRoot::Work, "c.txt")],
                        vec![file_output(StepRoot::Host, "share/interleaved.txt")],
                    ),
                    &["g", "h"],
                ),
            ),
        ]);

        run_steps(args)
            .await
            .unwrap_or_else(|err| panic!("{case}: {err}"));

        assert_eq!(
            read_records(&published.join("interleaved.txt")),
            ["alpha", "beta", "gamma"],
            "{case}"
        );
    }
}

/// The interface line a Fortran compiler would write to `geometry.mod`.
const GEOMETRY_INTERFACE: &str = "  pure function circle_area(radius) result(area)";

/// The dyndep information a Fortran scanner finds, as a step manifest with `@`
/// for the name of the module `main.f90` uses: compiling the module's source
/// also writes its `.mod` file, which compiling `main.f90` reads.
const MODULE_UPDATES: &str = concat!(
    r#"{"version": 1, "updates": ["#,
    r#"{"step": "compile-@", "outputs": [{"root": "work", "path": "mod/@.mod"}]}, "#,
    r#"{"step": "compile-main", "inputs": [{"root": "work", "path": "mod/@.mod"}]}"#,
    "]}",
);

/// Native commands scanning `src/main.f90` for the module it uses and
/// declaring [`MODULE_UPDATES`] for it.
fn scan_module_dependencies() -> String {
    if cfg!(windows) {
        format!(
            "@for /f \"tokens=2\" %%m in ('findstr /b /c:\"  use \" \"src\\main.f90\"') do @set \"RB_MODULE=%%m\"\n\
             @echo {}>\"%{STEP_MANIFEST_ENV}%\"",
            MODULE_UPDATES.replace('@', "%RB_MODULE%")
        )
    } else {
        format!(
            "module=$(sed -n 's/^ *use \\([A-Za-z_][A-Za-z0-9_]*\\)$/\\1/p' src/main.f90)\n\
             printf '{}\\n' \"$module\" \"$module\" \"$module\" > \"${STEP_MANIFEST_ENV}\"",
            MODULE_UPDATES.replace('@', "%s")
        )
    }
}

/// Fortran modules through Ninja-style dynamic dependencies: the recipe's
/// compile steps only declare their sources, and a scanner found in the
/// sources that `main.f90` uses the module `geometry.f90` defines. The
/// scanner's updates add `mod/geometry.mod` as an output of the module's
/// compile step and an input of `main.f90`'s before either starts, so
/// `compile-main`, listed first, waits for `compile-geometry` and reads the
/// module interface it wrote, not the stale one an earlier build left.
#[tokio::test]
async fn updates_add_module_dependencies_before_the_compilers_start() {
    let roots = Roots::new();
    let src = create_dir(&roots.work, "src");
    create_dir(&roots.work, "obj");
    let modules = create_dir(&roots.work, "mod");
    let main_source = [
        "program main",
        "  use geometry",
        "  implicit none",
        "  print *, circle_area(2.0)",
        "end program main",
    ];
    fs::write(src.join("main.f90"), main_source.join("\n") + "\n").unwrap();
    fs::write(
        src.join("geometry.f90"),
        [
            "module geometry",
            "  implicit none",
            "contains",
            GEOMETRY_INTERFACE,
            "    real, intent(in) :: radius",
            "    real :: area",
            "    area = 3.14159 * radius**2",
            "  end function circle_area",
            "end module geometry",
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    fs::write(modules.join("geometry.mod"), "stale interface\n").unwrap();

    let args = roots.args(vec![
        declared(
            step(&[scan_module_dependencies()]),
            graph(
                "scan",
                vec![glob_input(StepRoot::Work, "src/*.f90")],
                vec![],
            ),
        ),
        declared(
            step(&[
                mark_started("compile-main"),
                copy_file(&file("src/main.f90"), &file("obj/main.o")),
                append_file(&file("mod/geometry.mod"), &file("obj/main.o")),
            ]),
            discovering(
                graph(
                    "compile-main",
                    vec![file_input(StepRoot::Work, "src/main.f90")],
                    vec![file_output(StepRoot::Work, "obj/main.o")],
                ),
                &["scan"],
            ),
        ),
        declared(
            step(&[
                watch_for("compile-geometry", "compile-main", EXCLUSION_WAIT_SECONDS),
                copy_file(&file("src/geometry.f90"), &file("obj/geometry.o")),
                extract_lines(
                    "result(",
                    &file("src/geometry.f90"),
                    &file("mod/geometry.mod"),
                ),
            ]),
            discovering(
                graph(
                    "compile-geometry",
                    vec![file_input(StepRoot::Work, "src/geometry.f90")],
                    vec![file_output(StepRoot::Work, "obj/geometry.o")],
                ),
                &["scan"],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(
        observed(&roots.work, "compile-geometry", "compile-main"),
        [ALONE]
    );
    assert_eq!(
        read_records(&modules.join("geometry.mod")),
        [GEOMETRY_INTERFACE]
    );
    let expected: Vec<&str> = main_source
        .iter()
        .copied()
        .chain([GEOMETRY_INTERFACE])
        .collect();
    assert_eq!(read_records(&roots.work.join("obj/main.o")), expected);
}

/// Every step may report the inputs it read, exact paths and globs, which
/// stay in its declaration directory byte for byte after the build, for
/// generated steps as well.
#[tokio::test]
async fn input_reports_stay_with_the_step_that_wrote_them() {
    let roots = Roots::new();
    let src = create_dir(&roots.work, "src");
    fs::write(src.join("a.c"), "int a(void) { return 1; }\n").unwrap();
    fs::write(src.join("b.c"), "int b(void) { return 2; }\n").unwrap();
    let include = create_dir(&roots.work, "include");
    fs::write(include.join("config.h"), "#define ANSWER 42\n").unwrap();
    let gen_report = roots.plan(
        "gen-inputs.json",
        document(&json!({
            "version": 1,
            "inputs": [
                { "root": "work", "path": "src/*.c", "kind": "glob" },
                work("include/config.h"),
            ],
        })),
    );
    let bundle_report = roots.plan(
        "bundle-inputs.json",
        document(&json!({ "version": 1, "inputs": [work("include/config.h")] })),
    );
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![generated(
            "bundle",
            &[
                record_declaration_paths("bundle-paths.txt"),
                copy_file(&file("include/config.h"), &file("bundle.txt")),
                report(&bundle_report),
            ],
            vec![work("include/config.h")],
            vec![work("bundle.txt"), work("bundle-paths.txt")],
        )])),
    );

    let args = roots.args(vec![declared(
        step(&[
            record_declaration_paths("gen-paths.txt"),
            emit(&plan),
            report(&gen_report),
        ]),
        graph(
            "gen",
            vec![
                glob_input(StepRoot::Work, "src/*.c"),
                file_input(StepRoot::Work, "include/config.h"),
            ],
            vec![file_output(StepRoot::Work, "gen-paths.txt")],
        ),
    )]);

    run_steps(args).await.unwrap();

    assert_eq!(
        read_records(&roots.work.join("bundle.txt")),
        ["#define ANSWER 42"]
    );
    for (record, expected) in [
        ("gen-paths.txt", &gen_report),
        ("bundle-paths.txt", &bundle_report),
    ] {
        let paths = recorded_declaration_paths(&roots.work, record);
        assert_eq!(
            fs::read(&paths.inputs).unwrap(),
            fs::read(expected).unwrap(),
            "{record}"
        );
    }
}

/// A step's reported reads must remain valid when another step later
/// declares an output that covers them. The report was checked before that
/// output had an owner, but it must not have read an earlier copy of it.
#[tokio::test]
async fn reported_reads_reject_outputs_declared_after_the_reader_succeeded() {
    for reported in [
        json!({ "root": "work", "path": "obsolete.txt" }),
        json!({ "root": "work", "path": "obsolete.*", "kind": "glob" }),
    ] {
        let roots = Roots::new();
        let source = roots.work.join("obsolete.txt");
        fs::write(&source, "old contents\n").unwrap();
        let input_report = roots.plan(
            "reader-inputs.json",
            document(&json!({ "version": 1, "inputs": [reported] })),
        );
        let plan = roots.plan(
            "gen.json",
            document(&manifest(vec![generated(
                "overwrite",
                &[write_line(&file("obsolete.txt"), "new contents")],
                vec![],
                vec![work("obsolete.txt")],
            )])),
        );
        let args = roots.args(vec![
            declared(
                step(&[
                    copy_file(&file("obsolete.txt"), &file("observed.txt")),
                    report(&input_report),
                ]),
                graph(
                    "reader",
                    vec![],
                    vec![file_output(StepRoot::Work, "observed.txt")],
                ),
            ),
            declared(
                step(&[emit(&plan)]),
                after(graph("gen", vec![], vec![]), &["reader"]),
            ),
        ]);

        let error = failure(run_steps(args).await);
        for name in ["reader", "gen/overwrite", "obsolete.txt"] {
            assert!(error.contains(name), "{error}");
        }
        assert_eq!(fs::read_to_string(&source).unwrap(), "old contents\n");
        assert_eq!(
            fs::read_to_string(roots.work.join("observed.txt")).unwrap(),
            "old contents\n"
        );
    }
}

/// The declaration paths are absolute engine-owned values, not shell code:
/// work directory characters the native shell interprets must not change
/// where steps write their manifests or what their consumers read.
#[tokio::test]
async fn declaration_paths_are_literal_with_shell_metacharacters_in_the_work_directory() {
    let mut roots = Roots::new();
    let directory = if cfg!(windows) {
        "pct%RB_DYNAMIC_TEST_NONEXISTENT_XYZ%dollar$RB_DYNAMIC_TEST_NONEXISTENT_XYZ work"
    } else {
        "apostrophe'percent% amp& work"
    };
    roots.work = create_dir(roots._tmp.path(), directory);
    let plan = roots.plan(
        "child.json",
        document(&manifest(vec![generated(
            "child",
            &[write_line(&file("child.txt"), "fresh value")],
            vec![],
            vec![work("child.txt")],
        )])),
    );
    let args = roots.args(vec![
        declared(step(&[emit(&plan)]), graph("gen", vec![], vec![])),
        declared(
            step(&[copy_file(&file("child.txt"), &file("consumed.txt"))]),
            discovering(
                graph(
                    "consume",
                    vec![file_input(StepRoot::Work, "child.txt")],
                    vec![file_output(StepRoot::Work, "consumed.txt")],
                ),
                &["gen"],
            ),
        ),
    ]);

    run_steps(args).await.unwrap();
    assert_eq!(
        read_records(&roots.work.join("consumed.txt")),
        ["fresh value"]
    );
}

/// A generating step `gen` whose declarations the build rejects.
struct Rejected {
    case: &'static str,
    /// What `gen` writes to its step manifest, if anything.
    manifest: Option<String>,
    /// What `gen` writes to its input report, if anything.
    report: Option<String>,
    /// Steps listed before `gen`.
    before: Vec<BuildScriptSection>,
    /// The steps of `before` that `gen` depends on.
    gen_after: Vec<&'static str>,
    /// Texts the error has to contain, besides the id of `gen`.
    named: Vec<&'static str>,
}

impl Rejected {
    /// `gen` writing `manifest`, which is rejected with an error naming
    /// `named`.
    fn manifest(
        case: &'static str,
        manifest: impl std::fmt::Display,
        named: Vec<&'static str>,
    ) -> Self {
        Self {
            case,
            manifest: Some(manifest.to_string()),
            report: None,
            before: Vec::new(),
            gen_after: Vec::new(),
            named,
        }
    }
}

/// Declarations the build rejects once `gen` succeeded, each with the texts
/// its error has to contain. Every manifest also declares a valid step `ok`,
/// which must not run either.
fn rejected_declarations() -> Vec<Rejected> {
    let ok = || generated("ok", &[mark_started("generated")], vec![], vec![]);
    let writes = |id: &str, outputs: Vec<Value>| {
        generated(id, &[mark_started("generated")], vec![], outputs)
    };
    let owner = || {
        declared(
            step(&[write_line(&file("owned.txt"), "owned")]),
            graph(
                "owner",
                vec![],
                vec![file_output(StepRoot::Work, "owned.txt")],
            ),
        )
    };
    let mut cases = vec![
        Rejected::manifest(
            "malformed manifest",
            r#"{"version": 1, "steps": ["#,
            vec!["step manifest", "not valid JSON"],
        ),
        Rejected::manifest(
            "unsupported manifest version",
            json!({ "version": 2, "steps": [ok()] }),
            vec!["step manifest", "unsupported version `2`"],
        ),
        Rejected::manifest(
            "manifest without a version",
            json!({ "steps": [ok()] }),
            vec!["step manifest", "no `version`"],
        ),
        Rejected::manifest(
            "partial declaration",
            manifest(vec![
                ok(),
                json!({ "id": "half", "run": "exit 0", "inputs": [] }),
            ]),
            vec!["half", "outputs"],
        ),
        Rejected {
            before: vec![owner()],
            gen_after: vec!["owner"],
            ..Rejected::manifest(
                "output of a static step",
                manifest(vec![ok(), writes("dup", vec![work("owned.txt")])]),
                vec!["owner", "gen/dup", "owned.txt"],
            )
        },
        Rejected::manifest(
            "overlapping generated outputs",
            manifest(vec![
                ok(),
                writes(
                    "tree",
                    vec![json!({ "root": "work", "path": "out", "kind": "tree" })],
                ),
                writes("inner", vec![work("out/inner.txt")]),
            ]),
            vec!["gen/tree", "gen/inner", "out/inner.txt"],
        ),
        Rejected::manifest(
            "output rattler-build writes",
            manifest(vec![
                ok(),
                writes("thief", vec![work("conda_build_steps/stolen.txt")]),
            ]),
            vec!["gen/thief", "conda_build_steps"],
        ),
        Rejected::manifest(
            "output outside the work directory",
            manifest(vec![ok(), writes("escape", vec![work("../escaped.txt")])]),
            vec!["gen/escape"],
        ),
        Rejected::manifest(
            "cycle between generated steps",
            manifest(vec![
                ok(),
                with(writes("a", vec![]), "depends_on", json!(["b"])),
                with(writes("b", vec![]), "depends_on", json!(["a"])),
            ]),
            vec!["gen/a", "gen/b"],
        ),
        Rejected::manifest(
            "reference no step can declare",
            manifest(vec![
                ok(),
                with(writes("orphan", vec![]), "depends_on", json!(["nosuch/x"])),
            ]),
            vec!["gen/orphan", "nosuch/x"],
        ),
        Rejected::manifest(
            "update of an unknown step",
            json!({
                "version": 1,
                "steps": [ok()],
                "updates": [{ "step": "nowhere", "inputs": [work("owned.txt")] }],
            }),
            vec!["nowhere"],
        ),
        Rejected {
            before: vec![declared(
                step(&[mark_started("independent")]),
                graph("independent", vec![], vec![]),
            )],
            ..Rejected::manifest(
                "update of a step that does not wait for its generator",
                json!({
                    "version": 1,
                    "steps": [ok()],
                    "updates": [{ "step": "independent", "inputs": [work("late.txt")] }],
                }),
                vec!["independent"],
            )
        },
        Rejected {
            before: vec![declared(
                step(&[mark_started("reader")]),
                graph(
                    "reader",
                    vec![file_input(StepRoot::Work, "source.txt")],
                    vec![],
                ),
            )],
            gen_after: vec!["reader"],
            ..Rejected::manifest(
                "new producer of an input a started step read",
                manifest(vec![ok(), writes("late", vec![work("source.txt")])]),
                vec!["reader", "source.txt", "gen/late"],
            )
        },
        Rejected {
            report: Some(r#"{"version": 1, "inputs": ["#.to_string()),
            ..Rejected::manifest(
                "malformed input report",
                manifest(vec![ok()]),
                vec!["input report", "not valid JSON"],
            )
        },
        Rejected {
            report: Some(json!({ "version": 2, "inputs": [] }).to_string()),
            ..Rejected::manifest(
                "unsupported input report version",
                manifest(vec![ok()]),
                vec!["input report", "unsupported version `2`"],
            )
        },
        Rejected {
            report: Some(json!({ "version": 1, "inputs": [work("owned.txt")] }).to_string()),
            before: vec![owner()],
            ..Rejected::manifest(
                "input report naming an output gen did not wait for",
                manifest(vec![ok()]),
                vec!["input report", "owned.txt"],
            )
        },
    ];
    if cfg!(any(windows, target_os = "macos")) {
        cases.push(Rejected {
            before: vec![owner()],
            gen_after: vec!["owner"],
            ..Rejected::manifest(
                "output differing from a static one only in case",
                manifest(vec![ok(), writes("upper", vec![work("OWNED.txt")])]),
                vec!["owner", "gen/upper"],
            )
        });
    }
    cases
}

/// Declarations that are invalid or do not fit into the build fail it once
/// the step that wrote them succeeded, naming that step and what is wrong.
/// Nothing the step declared runs, even the valid steps of the same
/// manifest, and neither does a step waiting for the registration, nor any
/// step listed later.
#[tokio::test]
async fn invalid_declarations_fail_the_build_without_releasing_any_step() {
    for rejected in rejected_declarations() {
        let case = rejected.case;
        let roots = Roots::new();
        fs::write(roots.work.join("source.txt"), "source\n").unwrap();
        let mut commands = vec![mark_started("gen")];
        if let Some(contents) = &rejected.manifest {
            commands.push(emit(&roots.plan("gen.json", contents)));
        }
        if let Some(contents) = &rejected.report {
            commands.push(report(&roots.plan("gen-inputs.json", contents)));
        }
        let mut sections = rejected.before;
        sections.extend([
            declared(
                step(&commands),
                after(graph("gen", vec![], vec![]), &rejected.gen_after),
            ),
            declared(
                step(&[mark_started("waiter")]),
                discovering(graph("waiter", vec![], vec![]), &["gen"]),
            ),
            step(&[mark_started("barrier")]),
        ]);

        let message = failure(run_steps(roots.args(sections)).await);

        for text in std::iter::once("`gen`").chain(rejected.named.iter().copied()) {
            assert!(
                message.contains(text),
                "{case}: the error does not name {text}:\n{message}"
            );
        }
        assert!(
            roots.work.join("gen.started").exists(),
            "{case}: gen never ran"
        );
        for marker in ["generated", "waiter", "barrier"] {
            assert!(
                !roots.work.join(format!("{marker}.started")).exists(),
                "{case}: {marker} ran:\n{message}"
            );
        }
    }
}

/// A reference to a generated step that its generator did not declare fails
/// the build once that generator registered its steps, whether it did so
/// before or after the referring step was declared, and the referring step
/// never runs.
#[tokio::test]
async fn references_to_steps_a_generator_did_not_declare_fail_the_build() {
    let orders: [(&str, &[&str], &[&str]); 2] = [
        ("h registers after the reference", &[], &["g"]),
        ("h registers before the reference", &["h"], &[]),
    ];
    for (case, g_waits_for, h_waits_for) in orders {
        let roots = Roots::new();
        let g_plan = roots.plan(
            "g.json",
            document(&manifest(vec![with(
                generated("wait", &[mark_started("wait")], vec![], vec![]),
                "depends_on",
                json!(["h/missing"]),
            )])),
        );
        let h_plan = roots.plan(
            "h.json",
            document(&manifest(vec![generated(
                "present",
                &[write_line(&file("present.txt"), "present")],
                vec![],
                vec![work("present.txt")],
            )])),
        );

        let args = roots.args(vec![
            declared(
                step(&[emit(&g_plan)]),
                discovering(graph("g", vec![], vec![]), g_waits_for),
            ),
            declared(
                step(&[emit(&h_plan)]),
                discovering(graph("h", vec![], vec![]), h_waits_for),
            ),
        ]);

        let message = failure(run_steps(args).await);

        for text in ["g/wait", "h/missing"] {
            assert!(
                message.contains(text),
                "{case}: the error does not name {text}:\n{message}"
            );
        }
        assert!(
            !roots.work.join("wait.started").exists(),
            "{case}: the step with the unresolved reference ran"
        );
    }
}

/// A step that fails is not asked for its declarations: what it declared
/// never runs, and neither do steps waiting for its registration or its
/// expansion, nor later barriers. Steps already running finish before the
/// build fails with the generator's status.
#[tokio::test]
async fn failing_generators_release_nothing_and_running_steps_finish() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![generated(
            "child",
            &[mark_started("generated")],
            vec![],
            vec![],
        )])),
    );

    let args = roots.args(vec![
        declared(
            step(&[
                rendezvous("gen", "running", OVERLAP_WAIT_SECONDS),
                emit(&plan),
                exit_with(3),
            ]),
            graph("gen", vec![], vec![]),
        ),
        declared(
            step(&[
                rendezvous("running", "gen", OVERLAP_WAIT_SECONDS),
                pause(1),
                mark_started("running-finishing"),
            ]),
            graph("running", vec![], vec![]),
        ),
        declared(
            step(&[mark_started("waiter")]),
            discovering(graph("waiter", vec![], vec![]), &["gen"]),
        ),
        declared(
            step(&[mark_started("dependent")]),
            after(graph("dependent", vec![], vec![]), &["gen"]),
        ),
        step(&[mark_started("barrier")]),
    ]);

    let message = failure(run_steps(args).await);

    assert!(
        message.contains("gen") && message.contains("status 3"),
        "the error does not report the failing generator:\n{message}"
    );
    assert!(
        roots.work.join("running-finishing.started").exists(),
        "the build failed before the running step finished"
    );
    for marker in ["generated", "waiter", "dependent", "barrier"] {
        assert!(
            !roots.work.join(format!("{marker}.started")).exists(),
            "{marker} ran"
        );
    }
}

/// Generated steps can read secrets, whose values the build log masks in
/// their output. A declaration file containing a secret's value is rejected
/// without that value showing up in the error or the log.
#[tokio::test]
async fn generated_steps_read_secrets_that_the_log_and_errors_mask() {
    let secrets = || IndexMap::from([(SECRET.to_string(), SECRET_VALUE.to_string())]);

    let roots = Roots::new();
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![generated(
            "reveal",
            &[
                echo("reveal-output"),
                echo_variable(SECRET),
                record_variables("revealed.txt", &[SECRET]),
            ],
            vec![],
            vec![work("revealed.txt")],
        )])),
    );
    let mut args = roots.args(vec![declared(
        step(&[emit(&plan)]),
        graph("gen", vec![], vec![]),
    )]);
    args.secrets = secrets();

    run_steps(args).await.unwrap();

    assert_eq!(
        read_records(&roots.work.join("revealed.txt")),
        [format!("{SECRET}={SECRET_VALUE}")]
    );
    let log = fs::read_to_string(roots.work.join("conda_build.log")).unwrap();
    assert!(log.contains("reveal-output"), "log:\n{log}");
    assert!(!log.contains(SECRET_VALUE), "log:\n{log}");

    let leaky_manifest = json!({
        "version": 1,
        "steps": [{ "id": "leaky", "run": "exit 0", "inputs": SECRET_VALUE, "outputs": [] }],
    });
    let leaky_report = json!({ "version": 1, "inputs": SECRET_VALUE });
    for (case, declaration, variable) in [
        ("manifest", leaky_manifest, STEP_MANIFEST_ENV),
        ("input report", leaky_report, STEP_INPUTS_ENV),
    ] {
        let roots = Roots::new();
        let plan = roots.plan("leaky.json", declaration.to_string());
        let mut args = roots.args(vec![declared(
            step(&[declare(variable, &plan)]),
            graph("gen", vec![], vec![]),
        )]);
        args.secrets = secrets();

        let message = failure(run_steps(args).await);

        assert!(message.contains("`gen`"), "{case}:\n{message}");
        assert!(!message.contains(SECRET_VALUE), "{case}:\n{message}");
        let log = fs::read_to_string(roots.work.join("conda_build.log")).unwrap_or_default();
        assert!(!log.contains(SECRET_VALUE), "{case}: log:\n{log}");
    }
}

/// A generated step without `inputs` and `outputs` is a barrier among the
/// steps of its own manifest: it waits for the ones declared before it and
/// the ones declared after it wait for it. It does not hold back the steps
/// of another generator, but a recipe barrier after the generators waits for
/// everything they generated.
#[tokio::test]
async fn generated_barriers_order_only_their_own_manifest() {
    if !steps_can_overlap() {
        eprintln!("skipping: steps run one at a time without available parallelism");
        return;
    }
    let roots = Roots::new();
    let g_plan = roots.plan(
        "g.json",
        document(&manifest(vec![
            generated(
                "first",
                &[write_line(&file("first.txt"), "first")],
                vec![],
                vec![work("first.txt")],
            ),
            undeclared(
                "fence",
                &[
                    rendezvous("fence", "peer", OVERLAP_WAIT_SECONDS),
                    record_presence(&file("first.txt"), "fence.txt"),
                    watch_for("fence", "second", EXCLUSION_WAIT_SECONDS),
                ],
            ),
            generated(
                "second",
                &[
                    mark_started("second"),
                    write_line(&file("second.txt"), "second"),
                ],
                vec![],
                vec![work("second.txt")],
            ),
        ])),
    );
    let h_plan = roots.plan(
        "h.json",
        document(&manifest(vec![generated(
            "peer",
            &[rendezvous("peer", "fence", OVERLAP_WAIT_SECONDS)],
            vec![],
            vec![],
        )])),
    );

    let args = roots.args(vec![
        declared(step(&[emit(&g_plan)]), graph("g", vec![], vec![])),
        declared(step(&[emit(&h_plan)]), graph("h", vec![], vec![])),
        step(&[record_presence(&file("second.txt"), "after.txt")]),
    ]);

    run_steps(args).await.unwrap();

    assert_eq!(observed(&roots.work, "fence", "peer"), [MET]);
    assert_eq!(observed(&roots.work, "peer", "fence"), [MET]);
    assert_eq!(read_records(&roots.work.join("fence.txt")), ["seen"]);
    assert_eq!(observed(&roots.work, "fence", "second"), [ALONE]);
    assert_eq!(read_records(&roots.work.join("after.txt")), ["seen"]);
}

/// `discover_after` has to name a step of the recipe; otherwise the build
/// fails before activation or any step runs, like it does for `depends_on`.
#[tokio::test]
async fn discover_after_an_unknown_step_fails_before_activation() {
    let roots = Roots::new();
    install_activation_hook(&roots.host, &activation_hook("host"));
    let sections = || {
        vec![
            step(&[mark_started("barrier")]),
            declared(
                step(&[mark_started("waiter")]),
                discovering(graph("waiter", vec![], vec![]), &["nowhere"]),
            ),
        ]
    };

    let message = failure(run_steps(roots.args(sections())).await);
    let written = create_steps_script(roots.args(sections())).await;

    for text in ["waiter", "nowhere", "discover_after"] {
        assert!(
            message.contains(text),
            "the error does not name {text}:\n{message}"
        );
    }
    assert!(written.is_err(), "create_steps_script succeeded");
    assert!(!roots.work.join(ACTIVATION_LOG).exists());
    for marker in ["barrier", "waiter"] {
        assert!(!roots.work.join(format!("{marker}.started")).exists());
    }
}

/// The `conda_build.<ext>` a build writes replays the steps it generated,
/// one at a time in dependency order rather than manifest order, after one
/// activation. When a generator declares different steps during the replay
/// than it did in the build, the replay stops right after it rather than
/// run steps the build never registered or skip the new ones.
#[tokio::test]
async fn build_script_replays_the_steps_the_build_generated() {
    let roots = Roots::new();
    install_activation_hook(&roots.host, &activation_hook("host"));
    let order = file("order.txt");
    let steps = |consumed: &str| {
        manifest(vec![
            generated(
                "consume",
                &[
                    append_line(&order, "consume"),
                    copy_file(&file("produced.txt"), &file("consumed.txt")),
                    append_line(&file("consumed.txt"), consumed),
                ],
                vec![work("produced.txt")],
                vec![work("consumed.txt")],
            ),
            generated(
                "produce",
                &[
                    append_line(&order, "produce"),
                    write_line(&file("produced.txt"), "produced"),
                ],
                vec![],
                vec![work("produced.txt")],
            ),
        ])
    };
    let plan = roots.plan("gen.json", document(&steps("consumed")));

    let args = roots.args(vec![
        declared(
            step(&[append_line(&order, "gen"), emit(&plan)]),
            graph("gen", vec![], vec![]),
        ),
        step(&[append_line(&order, "finish")]),
    ]);
    let outputs = [ACTIVATION_LOG, "order.txt", "produced.txt", "consumed.txt"];
    let clean = || {
        for output in outputs {
            let path = roots.work.join(output);
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
        }
    };
    let assert_replayed = |run: &str| {
        assert_eq!(
            read_records(&roots.work.join(ACTIVATION_LOG)),
            ["host"],
            "{run}"
        );
        assert_eq!(
            read_records(&roots.work.join(&order)),
            ["gen", "produce", "consume", "finish"],
            "{run}"
        );
        assert_eq!(
            read_records(&roots.work.join("consumed.txt")),
            ["produced", "consumed"],
            "{run}"
        );
    };

    run_steps(args).await.unwrap();
    assert_replayed("run_steps");

    clean();
    let output = run_build_script_manually(&roots.work);
    assert!(output.status.success(), "{}", describe(&output));
    assert_replayed("build script replay");

    fs::write(plan, document(&steps("changed"))).unwrap();
    clean();
    let output = run_build_script_manually(&roots.work);
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        printed.contains("declared different build steps than the build this script replays"),
        "{}",
        describe(&output)
    );
    assert_eq!(read_records(&roots.work.join(&order)), ["gen"]);
    assert!(!roots.work.join("produced.txt").exists());
}

/// A `conda_build.<ext>` written without running the build cannot know the
/// steps a generator declares: its replay runs steps that declare nothing,
/// but stops with an error after a step that declares further steps rather
/// than silently skip them, and runs nothing that waits for it.
#[tokio::test]
async fn build_script_written_before_a_build_stops_at_generated_steps() {
    let roots = Roots::new();
    let plan = roots.plan(
        "gen.json",
        document(&manifest(vec![generated(
            "produce",
            &[write_line(&file("produced.txt"), "produced")],
            vec![],
            vec![work("produced.txt")],
        )])),
    );

    create_steps_script(roots.args(vec![
        step(&[mark_started("quiet"), truncate(STEP_MANIFEST_ENV)]),
        declared(step(&[emit(&plan)]), graph("gen", vec![], vec![])),
        declared(
            step(&[mark_started("waiter")]),
            discovering(graph("waiter", vec![], vec![]), &["gen"]),
        ),
    ]))
    .await
    .unwrap();

    let output = run_build_script_manually(&roots.work);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(roots.work.join("quiet.started").exists());
    assert!(!roots.work.join("waiter.started").exists());
    assert!(!roots.work.join("produced.txt").exists());
}
