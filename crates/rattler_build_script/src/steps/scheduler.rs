//! Scheduling the steps of one build as their graph grows.
//!
//! The [`DynamicStepGraph`] decides which steps are ready; [`StepRunner`]
//! runs them, at most as many at once as the machine has cores, and feeds
//! back what they did. Once a step succeeded, the declaration files it wrote
//! are read: its input report is checked against the graph, and the steps and
//! updates of its step manifest are validated by the graph, checked like the
//! steps of the recipe, and given wrappers before the graph registers them in
//! one go. Only then can a step that waits for the declarations start.
//! Declarations that cannot be registered fail the build like a failing step:
//! nothing they declare runs, and no step waiting for them starts.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use futures::stream::{FuturesUnordered, StreamExt};

use super::{
    CheckError, Launcher, PathChecks, ProcessEnv, StepRecord, clear_output, entry_metadata,
    output_kind_name, root_dir, write_step_wrapper,
};
use crate::{
    BuildScriptSection, DeclarationFile, DeclarationFileError, DeclarationPaths, GeneratedRun,
    GeneratedStep, GraphStep, InputManifest, InterpreterError, PlannedOutput,
    ResolvedScriptContents, StepManifest, StepOutputKind, StepPath, StepRoot,
    dynamic_graph::{DynamicStepGraph, Expansion},
    execution::script_generation_error,
};

/// Runs the steps of one build from their activated environment, in the
/// order their [`DynamicStepGraph`] allows, and registers what they declare.
pub(super) struct StepRunner<'a> {
    launcher: &'a Launcher<'a>,
    checks: &'a PathChecks<'a>,
    activated_env: &'a ProcessEnv,
}

/// What running one step needs, taken from the graph when the step starts:
/// nothing registered while it runs can change a started step.
struct StepRun {
    node: usize,
    name: String,
    wrapper: PathBuf,
    declarations: DeclarationPaths,
    /// Whether the step declares inputs and outputs, so it may overlap other
    /// steps.
    declared: bool,
    source_inputs: Vec<StepPath>,
    outputs: Vec<PlannedOutput>,
}

/// The declaration files a step wrote, read after it succeeded.
struct Declared {
    manifest: Option<DeclarationFile<StepManifest>>,
    inputs: Option<DeclarationFile<InputManifest>>,
}

/// A step declared by a step manifest, before it is registered.
struct PendingStep {
    index: usize,
    name: String,
    section: BuildScriptSection,
}

/// Why a build step did not succeed.
enum StepFailure {
    /// A declared input that no step produces did not exist when the step
    /// was about to start.
    MissingInput(StepPath),
    /// The step process could not be run, or a declared path could not be
    /// inspected.
    Error(InterpreterError),
    /// The step process exited unsuccessfully.
    Status(ExitStatus),
    /// What was at a declared output before the step started could not be
    /// cleared.
    OccupiedOutput(PlannedOutput, io::Error),
    /// The step process succeeded without creating a declared output.
    MissingOutput(PlannedOutput),
    /// The step process succeeded, but what it declared cannot be read or
    /// registered, for the reason given.
    InvalidDeclarations(String),
}

impl<'a> StepRunner<'a> {
    pub(super) fn new(
        launcher: &'a Launcher<'a>,
        checks: &'a PathChecks<'a>,
        activated_env: &'a ProcessEnv,
    ) -> Self {
        Self {
            launcher,
            checks,
            activated_env,
        }
    }

    /// Runs every step of `graph` once it is ready, lowest step index first
    /// among the steps that can start, with at most as many steps running
    /// as the machine has cores. `steps` holds the wrapper of every step of
    /// the graph, by index; the steps registered while the build runs are
    /// added to both, and every step keeps what it declared.
    ///
    /// After the first failure no further step starts; the steps that are
    /// running are waited for, what they declare is still registered so the
    /// replay knows it, and the first failure is returned. When no step can
    /// start although some have not run, the graph names what they wait for.
    pub(super) async fn run_all(
        &self,
        graph: &mut DynamicStepGraph,
        steps: &mut Vec<StepRecord>,
    ) -> Result<(), InterpreterError> {
        let max_running = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        // The steps of the recipe were checked together with the prefixes
        // when one of them declares paths.
        let mut prefixes_checked = (0..graph.len()).any(|node| !graph.is_barrier(node));
        let mut ready = BinaryHeap::new();
        let mut running = FuturesUnordered::new();
        let mut first_error = None;

        loop {
            if first_error.is_none() {
                ready.extend(graph.take_ready().into_iter().map(Reverse));
                while running.len() < max_running
                    && let Some(Reverse(node)) = ready.pop()
                {
                    let Some(step) = steps.get(node) else {
                        first_error = Some(self.unknown_step(graph, node));
                        break;
                    };
                    let run = StepRun {
                        node,
                        name: step.name.clone(),
                        wrapper: step.wrapper.clone(),
                        declarations: step.declarations.clone(),
                        declared: !graph.is_barrier(node),
                        source_inputs: graph.source_inputs(node).cloned().collect(),
                        outputs: graph.outputs(node).to_vec(),
                    };
                    graph.start(node);
                    running.push(async move {
                        let result = self.run_step(&run).await;
                        (run, result)
                    });
                }
            }
            let Some((run, result)) = running.next().await else {
                break;
            };
            let outcome = match result {
                Ok(declared) => {
                    self.register(graph, steps, &run, declared, &mut prefixes_checked)
                        .await
                }
                Err(failure) => Err(failure),
            };
            let Err(failure) = outcome else {
                continue;
            };
            graph.fail(run.node);
            if first_error.is_none() {
                first_error = Some(self.report(&run, failure));
                if !running.is_empty() {
                    tracing::info!(
                        "Waiting for the {} build step(s) still running to finish",
                        running.len()
                    );
                }
            } else {
                tracing::error!(
                    "{} after an earlier build step failed",
                    self.launcher.redact(&self.describe(&run, &failure))
                );
            }
        }

        match first_error {
            Some(error) => Err(error),
            None if graph.is_finished() => Ok(()),
            None => {
                let message = self
                    .launcher
                    .redact(&format!("invalid build steps: {}", graph.stalled_error()));
                tracing::error!("{message}");
                Err(InterpreterError::ExecutionFailed(io::Error::other(message)))
            }
        }
    }

    /// Runs the step of `run`, whose dependencies are all satisfied: checks
    /// that its declared inputs that no step produces exist, removes its
    /// declaration files and clears its declared outputs, runs its wrapper,
    /// checks that it created its declared outputs, and reads the
    /// declaration files it wrote.
    async fn run_step(&self, run: &StepRun) -> Result<Declared, StepFailure> {
        for input in &run.source_inputs {
            let metadata = entry_metadata(&self.resolve(input), false)
                .await
                .map_err(|err| StepFailure::Error(err.into()))?;
            if metadata.is_none() {
                return Err(StepFailure::MissingInput(input.clone()));
            }
        }

        // Declaration files an earlier run left must not be taken for what
        // this run declares.
        run.declarations
            .prepare()
            .map_err(|err| StepFailure::Error(io::Error::other(err.to_string()).into()))?;

        // Only what this step creates may satisfy its outputs. The graph
        // rejects declared inputs and other declared outputs at or below
        // them, so clearing them touches nothing another step declares.
        for output in &run.outputs {
            clear_output(self.root_dir(output.path().root()), output)
                .await
                .map_err(|err| StepFailure::OccupiedOutput(output.clone(), err))?;
        }

        // Declared steps can overlap, so mark where each one starts and ends
        // in the log; barriers run alone and log as before.
        if run.declared {
            tracing::info!("Starting build {}", self.launcher.redact(&run.name));
        }
        let status = self
            .launcher
            .run(&run.wrapper, self.activated_env)
            .await
            .map_err(StepFailure::Error)?;
        if !status.success() {
            return Err(StepFailure::Status(status));
        }

        for output in &run.outputs {
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
                return Err(StepFailure::MissingOutput(output.clone()));
            }
        }

        if run.declared {
            tracing::info!("Finished build {}", self.launcher.redact(&run.name));
        }

        // Only a step that succeeded declares anything, and only once its
        // declaration files are complete.
        let invalid = |err: DeclarationFileError| StepFailure::InvalidDeclarations(err.to_string());
        let manifest = StepManifest::read(&run.declarations.manifest).map_err(invalid)?;
        let inputs = InputManifest::read(&run.declarations.inputs).map_err(invalid)?;
        Ok(Declared { manifest, inputs })
    }

    /// Registers what the step of `run`, which succeeded, declared: checks
    /// its input report against the graph, then validates the steps and
    /// updates of its step manifest, checks their outputs, writes the
    /// wrappers of the new steps, and registers them all with the graph at
    /// once. Nothing is registered when anything fails.
    async fn register(
        &self,
        graph: &mut DynamicStepGraph,
        steps: &mut Vec<StepRecord>,
        run: &StepRun,
        declared: Declared,
        prefixes_checked: &mut bool,
    ) -> Result<(), StepFailure> {
        if let Some(inputs) = &declared.inputs {
            graph
                .check_reported_inputs(run.node, &inputs.contents.inputs)
                .map_err(|err| {
                    StepFailure::InvalidDeclarations(format!(
                        "the input report `{}` is invalid: {err}",
                        inputs.path.display()
                    ))
                })?;
        }

        let mut new_steps = Vec::new();
        let expansion = match &declared.manifest {
            Some(manifest) if !manifest.contents.is_empty() => {
                let invalid = |message: String| {
                    StepFailure::InvalidDeclarations(format!(
                        "the steps declared in `{}` are invalid: {message}",
                        manifest.path.display()
                    ))
                };
                let expansion = graph
                    .prepare(run.node, manifest.contents.clone())
                    .map_err(|err| invalid(err.to_string()))?;
                let pending = self.pending_steps(steps, &expansion).map_err(invalid)?;
                self.check_outputs(steps, &pending, &expansion, prefixes_checked)
                    .map_err(|err| match err {
                        CheckError::Invalid(message) => invalid(message),
                        CheckError::Io(err) => invalid(err.to_string()),
                    })?;
                let args = self.launcher.args;
                for PendingStep {
                    index,
                    name,
                    section,
                } in pending
                {
                    let wrapper_error =
                        |err| invalid(format!("{name}: {}", script_generation_error(err)));
                    let record = write_step_wrapper(
                        args,
                        self.launcher.dialect,
                        index,
                        name.clone(),
                        &section,
                    )
                    .await
                    .map_err(wrapper_error)?;
                    new_steps.push(record);
                }
                Some(expansion)
            }
            _ => None,
        };

        graph.succeed(run.node, expansion).map_err(|err| {
            StepFailure::InvalidDeclarations(format!(
                "its declarations cannot be registered: {err}"
            ))
        })?;
        if !new_steps.is_empty() {
            tracing::info!(
                "Build {} declared {} further build step(s)",
                self.launcher.redact(&run.name),
                new_steps.len()
            );
        }
        steps.extend(new_steps);
        if let Some(step) = steps.get_mut(run.node) {
            step.manifest = declared.manifest;
        }
        Ok(())
    }

    /// Returns the steps `expansion` adds, with the sections their wrappers
    /// run, numbered after the `steps` known so far.
    fn pending_steps(
        &self,
        steps: &[StepRecord],
        expansion: &Expansion,
    ) -> Result<Vec<PendingStep>, String> {
        let mut pending: Vec<PendingStep> = Vec::new();
        for (index, id, step) in expansion.steps() {
            let expected = steps.len() + pending.len();
            if index != expected {
                return Err(format!(
                    "step `{id}` would be registered as step {index}, but the next step is \
                     step {expected}"
                ));
            }
            pending.push(PendingStep {
                index,
                name: format!("step `{id}`"),
                section: generated_section(self.launcher.args, id, step),
            });
        }
        Ok(pending)
    }

    /// Checks every output `expansion` declares, for a new step or an
    /// existing one, like the outputs of the steps of the recipe. The
    /// prefixes are checked once any step declares paths.
    fn check_outputs(
        &self,
        steps: &[StepRecord],
        pending: &[PendingStep],
        expansion: &Expansion,
        prefixes_checked: &mut bool,
    ) -> Result<(), CheckError> {
        if expansion.declares_paths() && !*prefixes_checked {
            self.checks.check_prefixes()?;
            *prefixes_checked = true;
        }
        for (node, output) in expansion.added_outputs() {
            let (name, cwd) = match steps.get(node) {
                Some(step) => (step.name.as_str(), step.cwd.as_deref()),
                None => match pending.iter().find(|step| step.index == node) {
                    Some(step) => (step.name.as_str(), step.section.cwd.as_deref()),
                    None => {
                        return Err(CheckError::Invalid(format!(
                            "the output `{}` is declared for step {node}, which does not exist",
                            output.path()
                        )));
                    }
                },
            };
            self.checks.check_claim(name, output)?;
            self.checks
                .check_tree_outputs(name, cwd, std::iter::once(output))?;
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

    /// The error for step `node` of `graph`, which became ready without a
    /// wrapper.
    fn unknown_step(&self, graph: &DynamicStepGraph, node: usize) -> InterpreterError {
        let message = self.launcher.redact(&format!(
            "{} became ready to run, but no wrapper was written for it",
            graph.step_ref(node)
        ));
        tracing::error!("{message}");
        InterpreterError::ExecutionFailed(io::Error::other(message))
    }

    /// Describes the failure of the step of `run` in one line. The step and
    /// its paths may come from what another step wrote, so the description
    /// is masked like step output where it is logged or returned.
    fn describe(&self, run: &StepRun, failure: &StepFailure) -> String {
        let name = &run.name;
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
            StepFailure::InvalidDeclarations(message) => {
                format!("Build {name} succeeded, but {message}")
            }
        }
    }

    /// Logs the failure of the step of `run` and returns it as the error of
    /// the build, with the secrets of the build masked.
    fn report(&self, run: &StepRun, failure: StepFailure) -> InterpreterError {
        match failure {
            StepFailure::Status(status) => {
                self.launcher
                    .failed(&format!("Build {}", run.name), status, Some(&run.wrapper))
            }
            StepFailure::Error(err) => {
                let err = self.launcher.redact_error(err);
                tracing::error!("Build {} failed: {err}", self.launcher.redact(&run.name));
                err
            }
            StepFailure::MissingInput(_)
            | StepFailure::OccupiedOutput(..)
            | StepFailure::MissingOutput(_)
            | StepFailure::InvalidDeclarations(_) => {
                let message = self.launcher.redact(&self.describe(run, &failure));
                tracing::error!("{message}");
                InterpreterError::ExecutionFailed(io::Error::other(message))
            }
        }
    }
}

/// Returns the section the wrapper of the generated step `id`, declared as
/// `step`, runs: its script as written, with the interpreter and `env` it
/// declares, in its `cwd` resolved against the host prefix like the `cwd` of
/// a recipe step, or in the work directory when it declares none.
fn generated_section(
    args: &crate::ExecutionArgs,
    id: &str,
    step: &GeneratedStep,
) -> BuildScriptSection {
    BuildScriptSection {
        interpreter: step.interpreter.clone(),
        content: match &step.run {
            GeneratedRun::Script(script) => ResolvedScriptContents::Inline(script.clone()),
            GeneratedRun::Commands(commands) => ResolvedScriptContents::Commands(commands.clone()),
        },
        env: step.env.clone(),
        cwd: step
            .cwd
            .as_ref()
            .map(|cwd| args.context.host().path().join(cwd)),
        label: Some(format!("step {id}")),
        graph: GraphStep::default(),
    }
}
