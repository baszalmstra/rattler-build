//! Declarations of build steps for graph scheduling.
//!
//! A build step either declares both the files it reads (`inputs`) and the
//! files it writes (`outputs`), or neither. Declared steps are scheduled by
//! the dependencies their declarations imply and may run in parallel; a step
//! that declares neither is a sequential barrier. An explicitly empty list
//! declares that there are no such files, which is different from a missing
//! list. The validation and planning of these declarations lives in
//! `StepGraph` (with the `execution` feature).
//!
//! A step can declare further steps once it succeeded (see `StepManifest`).
//! `depends_on` waits for a step together with every step it generates,
//! recursively, while `discover_after` waits only until the steps it
//! generates are registered.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The directory a declared step path is relative to.
///
/// Paths of different roots are distinct artifacts, except that `host` and
/// `build` paths are the same artifacts when host and build share one prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepRoot {
    /// The work directory the build runs in.
    Work,
    /// The host prefix.
    Host,
    /// The build prefix.
    Build,
}

impl StepRoot {
    /// The name of the root as written in a recipe.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Host => "host",
            Self::Build => "build",
        }
    }
}

impl fmt::Display for StepRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the path of a step input is interpreted.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum StepInputKind {
    /// A single path: a file, or a directory with everything in it.
    #[default]
    File,
    /// A glob pattern selecting any number of paths, possibly none.
    Glob,
}

/// How the path of a step output is interpreted.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum StepOutputKind {
    /// A single file.
    #[default]
    File,
    /// A directory, and everything below it.
    Tree,
}

/// A path a build step reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepInput {
    /// The directory `path` is relative to.
    pub root: StepRoot,
    /// The path, or glob pattern, relative to `root`.
    pub path: PathBuf,
    /// Whether `path` is a single path or a glob pattern.
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: StepInputKind,
}

/// A path a build step writes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepOutput {
    /// The directory `path` is relative to.
    pub root: StepRoot,
    /// The path relative to `root`.
    pub path: PathBuf,
    /// Whether `path` is a single file or a whole directory tree.
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: StepOutputKind,
}

/// The scheduling declarations of one build step.
///
/// The default declares nothing: an undeclared step, which runs as a
/// sequential barrier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStep {
    /// Name other steps use to refer to this step in `depends_on`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The paths the step reads, or `None` when not declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<StepInput>>,
    /// The paths the step writes, or `None` when not declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<StepOutput>>,
    /// Ids of steps that have to finish before this step starts. For a step
    /// that generates further steps, finishing includes every step it
    /// generates, recursively.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Ids of generating steps whose declarations have to be registered
    /// before this step starts: the generating step itself has succeeded and
    /// the steps it declared are known, but they need not have run yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discover_after: Vec<String>,
}

impl GraphStep {
    /// Whether the step declares neither inputs nor outputs, which makes it a
    /// sequential barrier.
    pub fn is_barrier(&self) -> bool {
        self.inputs.is_none() && self.outputs.is_none()
    }
}

/// Separates the id of a generating step and the id of a step it generates
/// in a qualified id: step `A` generated by step `G` is `G/A`.
pub const STEP_ID_SEPARATOR: char = '/';

/// Whether `id` is a valid step id: a non-empty name of ASCII letters,
/// digits, `_`, `-` and `.`.
pub fn is_valid_step_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
