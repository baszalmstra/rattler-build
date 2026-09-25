//! Script execution and sandbox configuration for Rattler-Build, supporting bash, cmd,
//! python, and other interpreters.
//!
//! This crate provides functionality for defining, parsing, and executing build scripts
//! in various interpreters as part of the Rattler-Build process.
//!
//! Execution model: a script runs through a platform-native wrapper (bash on
//! Unix, cmd on Windows) that first performs prefix activation, then invokes
//! the chosen interpreter. Inline or file-backed scripts for specialized
//! interpreters (python, perl, etc.) are written out and executed by the
//! activated wrapper. Independent build steps (`run_steps`) activate once,
//! capture the exported environment, and run every step in its own wrapper
//! process started from that environment. The steps are scheduled by their
//! `DynamicStepGraph`: steps that declare their inputs and outputs run as
//! soon as the steps they depend on have succeeded, possibly in parallel,
//! while steps that declare neither run as sequential barriers.
//!
//! A build step can declare further steps after it succeeded, by writing the
//! declaration files named by its environment (see [`StepManifest`] and
//! [`InputManifest`]). The graph registers them once their step succeeded,
//! and schedules them like the listed steps.

pub mod sandbox;
mod script;
mod step_manifest;
mod step_model;

pub use sandbox::{SandboxArguments, SandboxConfiguration};
pub use script::{
    Script, ScriptContent, determine_interpreter_from_path, platform_script_extensions,
};
#[cfg(feature = "execution")]
pub use step_manifest::{
    DeclarationFile, DeclarationFileError, DeclarationKind, DeclarationPaths, ManifestError,
};
pub use step_manifest::{
    GeneratedRun, GeneratedStep, INPUT_MANIFEST_VERSION, InputManifest, STEP_INPUTS_ENV,
    STEP_MANIFEST_ENV, STEP_MANIFEST_VERSION, StepManifest, StepUpdate,
};
pub use step_model::{
    GraphStep, STEP_ID_SEPARATOR, StepInput, StepInputKind, StepOutput, StepOutputKind, StepRoot,
    is_valid_step_id,
};

#[cfg(feature = "execution")]
mod activation;
#[cfg(feature = "execution")]
mod dynamic_graph;
#[cfg(feature = "execution")]
mod execution;
#[cfg(feature = "execution")]
mod execution_context;
#[cfg(feature = "execution")]
mod interpreter;
#[cfg(feature = "execution")]
pub mod runner;
#[cfg(feature = "execution")]
mod runtime;
#[cfg(feature = "execution")]
mod shell_dialect;
#[cfg(feature = "execution")]
mod step_graph;
#[cfg(feature = "execution")]
mod steps;
#[cfg(feature = "execution")]
mod windows_machine;

#[cfg(feature = "execution")]
pub use dynamic_graph::{DynamicStepGraph, Expansion};
#[cfg(feature = "execution")]
pub use execution::{
    BuildScriptSection, EnvironmentIsolation, ExecutionArgs, ResolvedScriptContents,
    create_build_script, run_script,
};
#[cfg(feature = "execution")]
pub use execution_context::{ExecutionContext, PrefixLayout, PrefixWithPlatform};
#[cfg(feature = "execution")]
pub use interpreter::{InterpreterError, closest_interpreter};
#[cfg(feature = "execution")]
pub use runtime::RuntimeEnv;
#[cfg(feature = "execution")]
pub use step_graph::{
    LateProducer, PlannedInput, PlannedOutput, StepGraph, StepGraphError, StepPath, StepPathError,
    StepRef,
};
#[cfg(feature = "execution")]
pub use steps::{create_steps_script, run_steps};
