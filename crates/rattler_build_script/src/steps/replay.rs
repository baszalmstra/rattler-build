//! The work directory's `conda_build.<ext>`, which replays the steps of a
//! build one at a time.
//!
//! The replay runs every step known when it is written, in the serial
//! topological order of the step graph, each through its own wrapper, so
//! the steps a build registered run like the steps of the recipe. It cannot
//! register steps itself. Instead, it removes the declaration files of every
//! step before starting it, and right after the step checks the step
//! manifest it wrote: a step whose manifest was registered in the build the
//! replay was written after has to write exactly the same bytes again, which
//! the replay compares with a copy it keeps next to the manifest; any other
//! step must not declare anything. Otherwise the replay stops with status 1
//! before any later step, as the steps after it may not be the ones the step
//! declares now.

use std::io;
use std::path::{Path, PathBuf};

use rattler_shell::shell::Shell;

use super::{StepRecord, redact, remove_entry};
use crate::{
    InterpreterError,
    dynamic_graph::DynamicStepGraph,
    execution::{ExecutionArgs, write_native_wrapper},
    shell_dialect::ShellDialect,
};

/// The name of the copy of a registered step manifest in the directory of
/// its step, which the replay compares what the step declares with.
const RECORDED_MANIFEST_FILE_NAME: &str = "recorded_steps.json";

/// Writes the replay wrapper of the steps of `graph`, whose wrappers
/// `steps` holds by index, activating with `activation`, and returns its
/// path. The step manifests `steps` recorded are copied next to the
/// manifests of their steps for the replay to compare with.
pub(super) async fn write_replay(
    args: &ExecutionArgs,
    dialect: &dyn ShellDialect,
    activation: &Path,
    graph: &DynamicStepGraph,
    steps: &[StepRecord],
) -> Result<PathBuf, InterpreterError> {
    let order = graph.replay_order();
    let mut fragments = Vec::with_capacity(order.len());
    for node in order {
        let step = steps.get(node).ok_or_else(|| {
            io::Error::other(format!(
                "no wrapper was written for build {}",
                graph.step_ref(node)
            ))
        })?;
        fragments.push(replay_step(args, dialect, step).await?);
    }

    let replay = args
        .work_dir
        .join(format!("conda_build.{}", dialect.shell().extension()));
    let preamble = dialect.replay_preamble(&replay, activation, &args.context);
    write_native_wrapper(dialect, &replay, &preamble, &fragments).await?;
    Ok(replay)
}

/// Returns the replay lines running `step`: removing its declaration files,
/// running its wrapper, and checking the step manifest it wrote against
/// the one it recorded, if any.
async fn replay_step(
    args: &ExecutionArgs,
    dialect: &dyn ShellDialect,
    step: &StepRecord,
) -> io::Result<String> {
    let manifest = &step.declarations.manifest;
    let recorded_path = step.dir.join(RECORDED_MANIFEST_FILE_NAME);
    let recorded = match &step.manifest {
        Some(recorded) => {
            tokio::fs::write(&recorded_path, &recorded.bytes).await?;
            Some(recorded_path)
        }
        None => {
            remove_entry(&recorded_path).await?;
            None
        }
    };
    let message = match &recorded {
        Some(recorded) => format!(
            "Build {} declared different build steps than the build this script replays: its \
             step manifest {} differs from {}, which it wrote in that build. Run the build \
             again to run the steps it declares now.",
            step.name,
            manifest.display(),
            recorded.display()
        ),
        None => format!(
            "Build {} wrote an unrecorded step manifest at {}. This replay cannot run \
             declarations that differ from its recorded graph; run the build again \
             to regenerate the replay.",
            step.name,
            manifest.display()
        ),
    };

    let mut lines = dialect.remove_files(&[manifest.as_path(), step.declarations.inputs.as_path()]);
    lines.push_str(&dialect.child_script_command(&step.wrapper, &args.context));
    // The replay prints the message itself, where rattler-build cannot mask
    // secrets the way it masks what the steps print.
    let message = redact(args, &message);
    lines.push_str(&dialect.check_step_manifest(manifest, recorded.as_deref(), &message));
    Ok(lines)
}
