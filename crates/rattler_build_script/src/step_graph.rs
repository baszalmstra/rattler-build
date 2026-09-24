//! Static scheduling graph of build steps.
//!
//! Steps that declare both `inputs` and `outputs` run once everything they
//! depend on has succeeded, independently of their position in the list:
//!
//! - A step depends on every step whose outputs its inputs cover. An input
//!   covers its path and everything below it; a glob input covers every path
//!   it can match and everything below those. An output provides its path,
//!   and a tree output also everything below it. Declared outputs count
//!   whether or not a file already exists: a stale file from an earlier build
//!   never stands in for its producer. A step whose inputs cover its own
//!   outputs depends on itself, which is reported as a cycle.
//! - A step depends on every step its `depends_on` names. This orders the
//!   steps without making any file an input.
//!
//! A step that declares neither `inputs` nor `outputs` is a sequential
//! barrier. It waits for every step listed before it, and every step listed
//! after it waits for it, so it never overlaps other steps.
//!
//! Paths are identified by their root and their lexically normalized path
//! below it. The `work` root is always distinct from the prefixes, and `host`
//! and `build` are distinct unless both roots are the same prefix, in which
//! case `host:x` and `build:x` are the same path. Both `/` and `\` separate
//! components, and `.` and empty components are dropped. Absolute paths,
//! drive paths, paths with a `..` component or a `:` anywhere, and paths
//! naming the root itself are rejected, glob patterns included. On Windows and
//! macOS paths compare case-insensitively, as their file systems usually do.
//! On Windows components ending in `.` or a space are also rejected, as
//! Windows would name the file without them, and so are components spelled
//! like a DOS short name, with `~` and digits ending the name before its
//! extension (`CONDA_~1`, `FOO~12.BAR`), as they may name another file or
//! directory under its short alias.
//!
//! Glob inputs use [`globset`] syntax, where `*` and `?` never match `/`, a
//! `**` component matches any number of components and `\` separates
//! components instead of escaping (use `[*]` for a literal `*`). Alternatives
//! (`{a,b}`) and classes (`[ab]`) have to stay within one component.
//!
//! Declared outputs, of different steps or of the same step, must not name the
//! same path or paths inside one another, so every path has one producer and
//! belongs to one output.

use std::cmp::Reverse;
use std::collections::btree_map::Entry;
use std::collections::hash_map::Entry as HashEntry;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::fmt;
use std::ops::Bound;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use rattler_conda_types::Platform;
use thiserror::Error;

use crate::execution_context::PrefixLayout;
use crate::step_model::{GraphStep, StepInputKind, StepOutputKind, StepRoot};

/// A step named in a diagnostic: its position in the step list and its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRef {
    /// The position of the step in the list of steps, starting at 0.
    pub index: usize,
    /// The id of the step, if it has one.
    pub id: Option<String>,
}

impl StepRef {
    fn new(index: usize, step: &GraphStep) -> Self {
        Self {
            index,
            id: step.id.clone(),
        }
    }
}

impl fmt::Display for StepRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(f, "step {} (`{id}`)", self.index),
            None => write!(f, "step {}", self.index),
        }
    }
}

/// Why a declared path is not a valid path relative to its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StepPathError {
    /// The path is empty or normalizes to the root itself.
    #[error("the path is empty or names the root directory itself")]
    Empty,
    /// The path starts with a separator.
    #[error("the path is absolute; declare it relative to its root")]
    Absolute,
    /// The path starts with a drive letter.
    #[error("the path starts with a drive; declare it relative to its root")]
    DrivePrefix,
    /// A component is `..`.
    #[error("the path contains a `..` component; declare it without `..`")]
    ParentComponent,
    /// A component contains `:`, which Windows reads as a drive or a stream.
    #[error(
        "the path contains `:`, which Windows reads as a drive or a file stream; declare it without `:`"
    )]
    Colon,
    /// On Windows, a component ends in `.` or a space, which Windows drops
    /// from file names.
    #[error(
        "a component of the path ends in `.` or a space, which Windows drops from file names; declare it without them"
    )]
    TrailingDotOrSpace,
    /// On Windows, a component is spelled like a DOS short name, which may
    /// be the alias of another file or directory.
    #[error(
        "a component of the path ends in `~` and digits before its extension, which Windows may read as the short name of another file; declare the full name"
    )]
    ShortName,
    /// The path is not valid Unicode.
    #[error("the path is not valid Unicode")]
    NotUnicode,
}

/// An invalid set of step declarations.
#[derive(Debug, Error)]
pub enum StepGraphError {
    /// A step has an empty id.
    #[error("{step} has an empty `id`")]
    EmptyId {
        /// The step with the empty id.
        step: StepRef,
    },
    /// Two steps have the same id.
    #[error("step {first} and step {second} both have the id `{id}`; step ids must be unique")]
    DuplicateId {
        /// The id both steps have.
        id: String,
        /// The position of the first step with the id.
        first: usize,
        /// The position of the second step with the id.
        second: usize,
    },
    /// A step declares only one of `inputs` and `outputs`.
    #[error(
        "{step} declares `{declared}` but not `{missing}`; declare both (an empty list declares none) or neither to run the step as a sequential barrier"
    )]
    PartialDeclaration {
        /// The step with the partial declaration.
        step: StepRef,
        /// The field the step declares.
        declared: &'static str,
        /// The field the step does not declare.
        missing: &'static str,
    },
    /// A declared path is not a valid path relative to its root.
    #[error("{step} declares the invalid path `{path}`: {reason}")]
    InvalidPath {
        /// The step declaring the path.
        step: StepRef,
        /// The path as declared.
        path: String,
        /// Why the path is invalid.
        reason: StepPathError,
    },
    /// A glob input is not a valid glob pattern.
    #[error("{step} declares the invalid glob `{pattern}`: {reason}")]
    InvalidGlob {
        /// The step declaring the glob.
        step: StepRef,
        /// The normalized pattern, with its root.
        pattern: String,
        /// Why the pattern is invalid.
        reason: String,
    },
    /// `depends_on` names an id no step has.
    #[error("{step} depends on `{dependency}`, but no step has that id")]
    UnknownDependency {
        /// The step naming the id.
        step: StepRef,
        /// The id no step has.
        dependency: String,
    },
    /// Two outputs are the same path or lie inside one another.
    #[error(
        "{first_step} writes `{first_path}` and {second_step} writes `{second_path}`; declared outputs must not be the same path or lie inside one another"
    )]
    ConflictingOutputs {
        /// The producer of the first output, in list order.
        first_step: StepRef,
        /// The first output, with its root.
        first_path: String,
        /// The producer of the second output, in list order; the same as
        /// `first_step` when one step declares both outputs.
        second_step: StepRef,
        /// The second output, with its root.
        second_path: String,
    },
    /// The steps depend on each other in a cycle.
    #[error("build steps form a dependency cycle:\n{chain}")]
    Cycle {
        /// One line per dependency of the cycle, with its reason.
        chain: String,
    },
}

/// How declared paths are identified on the machine the steps run on.
#[derive(Debug, Clone, Copy)]
struct PathIdentity {
    /// Whether paths compare case-insensitively: on Windows and macOS, whose
    /// file systems usually are, even where a volume is case-sensitive.
    case_insensitive: bool,
    /// Whether components must not end in `.` or a space, which Windows drops
    /// from file names, nor be spelled like DOS short names.
    windows: bool,
    /// Whether the host and build roots are the same directory.
    shared_prefix: bool,
}

impl PathIdentity {
    fn new(platform: Platform, layout: PrefixLayout) -> Self {
        Self {
            case_insensitive: platform.is_windows() || platform.is_osx(),
            windows: platform.is_windows(),
            shared_prefix: match layout {
                PrefixLayout::Separate => false,
                PrefixLayout::Shared => true,
            },
        }
    }

    /// The root that identifies paths below `root`: the host root stands for
    /// the build root when both are the same directory.
    fn root(self, root: StepRoot) -> StepRoot {
        match root {
            StepRoot::Build if self.shared_prefix => StepRoot::Host,
            StepRoot::Work | StepRoot::Host | StepRoot::Build => root,
        }
    }
}

/// A declared path, normalized below its root.
#[derive(Debug, Clone)]
pub struct StepPath {
    /// The root as declared.
    root: StepRoot,
    /// The root identifying the path, which differs from `root` when it names
    /// the same directory as another root.
    identity_root: StepRoot,
    /// Normalized components joined with `/`.
    normalized: String,
    /// Normalized components joined with the separator of the host.
    relative: PathBuf,
    /// `normalized`, case-folded when paths compare case-insensitively.
    key: String,
}

impl StepPath {
    /// Normalizes `raw` below `root`; see the module documentation.
    fn new(root: StepRoot, raw: &Path, identity: PathIdentity) -> Result<Self, StepPathError> {
        let raw = raw.to_str().ok_or(StepPathError::NotUnicode)?;
        if raw.starts_with(['/', '\\']) {
            return Err(StepPathError::Absolute);
        }
        if let [drive, b':', ..] = raw.as_bytes()
            && drive.is_ascii_alphabetic()
        {
            return Err(StepPathError::DrivePrefix);
        }

        let mut components = Vec::new();
        for component in raw.split(['/', '\\']) {
            match component {
                "" | "." => {}
                ".." => return Err(StepPathError::ParentComponent),
                name if name.contains(':') => return Err(StepPathError::Colon),
                name if identity.windows && name.ends_with(['.', ' ']) => {
                    return Err(StepPathError::TrailingDotOrSpace);
                }
                name if identity.windows && is_short_name(name) => {
                    return Err(StepPathError::ShortName);
                }
                name => components.push(name),
            }
        }
        if components.is_empty() {
            return Err(StepPathError::Empty);
        }

        let normalized = components.join("/");
        let key = if identity.case_insensitive {
            normalized.to_lowercase()
        } else {
            normalized.clone()
        };
        Ok(Self {
            root,
            identity_root: identity.root(root),
            relative: components.into_iter().collect(),
            normalized,
            key,
        })
    }

    /// The root the path is relative to.
    pub fn root(&self) -> StepRoot {
        self.root
    }

    /// The normalized path, with `/` separators.
    pub fn as_str(&self) -> &str {
        &self.normalized
    }

    /// The normalized path, with the separators of the host.
    pub fn relative_path(&self) -> &Path {
        &self.relative
    }

    /// The path below `root_dir`, the directory of [`Self::root`].
    pub fn resolve(&self, root_dir: &Path) -> PathBuf {
        root_dir.join(&self.relative)
    }
}

/// Whether `component` is spelled like a DOS 8.3 short name: `~` and one or
/// more digits end its name before the extension, as in `CONDA_~1`,
/// `CONDA_~1.TXT` and `FOO~12.BAR`. Short names have at most one `.`, so the
/// extension starts after the last one.
fn is_short_name(component: &str) -> bool {
    let stem = component
        .rsplit_once('.')
        .map_or(component, |(stem, _extension)| stem);
    let digits = stem.bytes().rev().take_while(u8::is_ascii_digit).count();
    digits > 0 && stem[..stem.len() - digits].ends_with('~')
}

impl fmt::Display for StepPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.root, self.normalized)
    }
}

/// A declared input of a step in the graph.
#[derive(Debug, Clone)]
pub struct PlannedInput {
    path: StepPath,
    kind: StepInputKind,
    producers: Vec<usize>,
    source: bool,
}

impl PlannedInput {
    /// The normalized path, or glob pattern.
    pub fn path(&self) -> &StepPath {
        &self.path
    }

    /// Whether the path is a single path or a glob pattern.
    pub fn kind(&self) -> StepInputKind {
        self.kind
    }

    /// The steps producing outputs this input covers, in list order.
    pub fn producers(&self) -> &[usize] {
        &self.producers
    }

    /// Whether this is a single path no step produces, neither as a
    /// file output nor inside a tree output. Such a path has to exist before
    /// the step starts.
    pub fn is_source(&self) -> bool {
        self.source
    }
}

/// A declared output of a step in the graph.
#[derive(Debug, Clone)]
pub struct PlannedOutput {
    path: StepPath,
    kind: StepOutputKind,
}

impl PlannedOutput {
    /// The normalized path.
    pub fn path(&self) -> &StepPath {
        &self.path
    }

    /// Whether the path is a single file or a whole directory tree.
    pub fn kind(&self) -> StepOutputKind {
        self.kind
    }
}

/// One step of the graph.
#[derive(Debug)]
struct Node {
    barrier: bool,
    inputs: Vec<PlannedInput>,
    outputs: Vec<PlannedOutput>,
    dependencies: Vec<usize>,
    dependents: Vec<usize>,
}

/// The validated scheduling graph of a list of build steps.
///
/// Steps are identified by their position in the list the graph was created
/// from.
#[derive(Debug)]
pub struct StepGraph {
    nodes: Vec<Node>,
    order: Vec<usize>,
}

impl StepGraph {
    /// Validates the declarations of `steps` and plans their dependencies.
    ///
    /// Paths compare case-insensitively when `platform`, the platform the
    /// steps run on, is Windows or macOS. With a [`PrefixLayout::Shared`] `layout`,
    /// `host` and `build` paths name the same files. Nothing is looked up on
    /// disk: whether source inputs exist can only be checked when their step
    /// is about to start.
    pub fn new<'a>(
        steps: impl IntoIterator<Item = &'a GraphStep>,
        platform: Platform,
        layout: PrefixLayout,
    ) -> Result<Self, StepGraphError> {
        let steps: Vec<&GraphStep> = steps.into_iter().collect();
        let identity = PathIdentity::new(platform, layout);

        let ids = step_ids(&steps)?;
        let (mut inputs, outputs): (Vec<StepInputs>, Vec<Vec<PlannedOutput>>) = steps
            .iter()
            .enumerate()
            .map(|(index, step)| declarations(index, step, identity))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .unzip();

        let mut edges: Vec<Vec<Edge>> = vec![Vec::new(); steps.len()];
        for (index, step) in steps.iter().enumerate() {
            for dependency in &step.depends_on {
                let Some(&from) = ids.get(dependency.as_str()) else {
                    return Err(StepGraphError::UnknownDependency {
                        step: StepRef::new(index, step),
                        dependency: dependency.clone(),
                    });
                };
                edges[index].push(Edge {
                    from,
                    reason: EdgeReason::DependsOn,
                });
            }
        }

        let owners = OutputOwners::new(&steps, &outputs)?;
        for (step_inputs, step_edges) in inputs.iter_mut().zip(&mut edges) {
            step_inputs.resolve(&owners, step_edges);
        }
        add_barrier_edges(&steps, &mut edges);

        let mut nodes: Vec<Node> = inputs
            .into_iter()
            .zip(outputs)
            .zip(&steps)
            .map(|((step_inputs, outputs), step)| Node {
                barrier: step.is_barrier(),
                inputs: step_inputs.planned,
                outputs,
                dependencies: Vec::new(),
                dependents: Vec::new(),
            })
            .collect();
        for (index, step_edges) in edges.iter_mut().enumerate() {
            // Stable, so the first reason recorded for a dependency is kept.
            step_edges.sort_by_key(|edge| edge.from);
            step_edges.dedup_by_key(|edge| edge.from);
            nodes[index].dependencies = step_edges.iter().map(|edge| edge.from).collect();
            for edge in step_edges.iter() {
                nodes[edge.from].dependents.push(index);
            }
        }

        let order = topological_order(&nodes)
            .map_err(|remaining| cycle_error(&steps, &nodes, &edges, &remaining))?;
        Ok(Self { nodes, order })
    }

    /// The number of steps.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there are no steps.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Whether step `index` declares neither inputs nor outputs, which makes
    /// it a sequential barrier.
    pub fn is_barrier(&self, index: usize) -> bool {
        self.nodes[index].barrier
    }

    /// The steps that have to succeed before step `index` starts, in list
    /// order. Includes the ordering around barriers.
    pub fn dependencies(&self, index: usize) -> &[usize] {
        &self.nodes[index].dependencies
    }

    /// The steps that wait for step `index`, in list order.
    pub fn dependents(&self, index: usize) -> &[usize] {
        &self.nodes[index].dependents
    }

    /// All steps in an order that runs every step after its dependencies.
    ///
    /// Among the steps whose dependencies come earlier, the one listed first
    /// comes first, so the order is deterministic and keeps the list order
    /// where the dependencies allow it.
    pub fn topological_order(&self) -> &[usize] {
        &self.order
    }

    /// The declared inputs of step `index`; empty for a barrier.
    pub fn inputs(&self, index: usize) -> &[PlannedInput] {
        &self.nodes[index].inputs
    }

    /// The single-path inputs of step `index` that no step produces; they
    /// have to exist before the step starts.
    pub fn source_inputs(&self, index: usize) -> impl Iterator<Item = &StepPath> {
        self.nodes[index]
            .inputs
            .iter()
            .filter(|input| input.source)
            .map(|input| &input.path)
    }

    /// The declared outputs of step `index`; empty for a barrier.
    pub fn outputs(&self, index: usize) -> &[PlannedOutput] {
        &self.nodes[index].outputs
    }
}

/// Maps the id of every step with an id to its position.
fn step_ids<'a>(steps: &[&'a GraphStep]) -> Result<HashMap<&'a str, usize>, StepGraphError> {
    let mut ids = HashMap::new();
    for (index, step) in steps.iter().copied().enumerate() {
        let Some(id) = step.id.as_deref() else {
            continue;
        };
        if id.is_empty() {
            return Err(StepGraphError::EmptyId {
                step: StepRef { index, id: None },
            });
        }
        match ids.entry(id) {
            HashEntry::Occupied(entry) => {
                return Err(StepGraphError::DuplicateId {
                    id: id.to_string(),
                    first: *entry.get(),
                    second: index,
                });
            }
            HashEntry::Vacant(entry) => {
                entry.insert(index);
            }
        }
    }
    Ok(ids)
}

/// Why a step has to wait for another one.
#[derive(Debug, Clone, Copy)]
enum EdgeReason {
    /// The step names the other one in `depends_on`.
    DependsOn,
    /// One of the two steps is a barrier.
    Barrier,
    /// Input `input` of the step covers output `output` of the other one.
    Artifact { input: usize, output: usize },
}

/// A dependency of a step on step `from`.
#[derive(Debug, Clone, Copy)]
struct Edge {
    from: usize,
    reason: EdgeReason,
}

/// The normalized inputs of one step.
struct StepInputs {
    planned: Vec<PlannedInput>,
    /// The compiled pattern of every glob input; `None` for single paths.
    patterns: Vec<Option<GlobPattern>>,
}

impl StepInputs {
    /// Finds the producers of the inputs of a step, records them on the
    /// inputs and adds a dependency on each of them to `edges`, the
    /// dependencies of the step.
    fn resolve(&mut self, owners: &OutputOwners<'_>, edges: &mut Vec<Edge>) {
        let mut covered = Vec::new();
        for (position, (input, pattern)) in self.planned.iter_mut().zip(&self.patterns).enumerate()
        {
            covered.clear();
            match pattern {
                None => {
                    let owner = owners.owner(&input.path);
                    input.source = owner.is_none();
                    covered.extend(owner);
                    covered.extend(owners.below(&input.path));
                }
                Some(pattern) => covered.extend(
                    owners
                        .all()
                        .filter(|owner| pattern.covers(&input.path, owners.output(owner))),
                ),
            }

            edges.extend(covered.iter().map(|owner| Edge {
                from: owner.step,
                reason: EdgeReason::Artifact {
                    input: position,
                    output: owner.output,
                },
            }));
            input.producers = covered.iter().map(|owner| owner.step).collect();
            input.producers.sort_unstable();
            input.producers.dedup();
        }
    }
}

/// Normalizes the declarations of step `index`.
fn declarations(
    index: usize,
    step: &GraphStep,
    identity: PathIdentity,
) -> Result<(StepInputs, Vec<PlannedOutput>), StepGraphError> {
    let partial = |declared, missing| StepGraphError::PartialDeclaration {
        step: StepRef::new(index, step),
        declared,
        missing,
    };
    let (inputs, outputs) = match (&step.inputs, &step.outputs) {
        (Some(inputs), Some(outputs)) => (inputs.as_slice(), outputs.as_slice()),
        (None, None) => (&[][..], &[][..]),
        (Some(_), None) => return Err(partial("inputs", "outputs")),
        (None, Some(_)) => return Err(partial("outputs", "inputs")),
    };
    let normalize = |root, raw: &Path| {
        StepPath::new(root, raw, identity).map_err(|reason| StepGraphError::InvalidPath {
            step: StepRef::new(index, step),
            path: raw.display().to_string(),
            reason,
        })
    };

    let mut step_inputs = StepInputs {
        planned: Vec::with_capacity(inputs.len()),
        patterns: Vec::with_capacity(inputs.len()),
    };
    for input in inputs {
        let path = normalize(input.root, &input.path)?;
        let pattern = match input.kind {
            StepInputKind::File => None,
            StepInputKind::Glob => Some(
                GlobPattern::new(path.as_str(), identity.case_insensitive).map_err(|reason| {
                    StepGraphError::InvalidGlob {
                        step: StepRef::new(index, step),
                        pattern: path.to_string(),
                        reason,
                    }
                })?,
            ),
        };
        step_inputs.planned.push(PlannedInput {
            path,
            kind: input.kind,
            producers: Vec::new(),
            source: false,
        });
        step_inputs.patterns.push(pattern);
    }
    let planned_outputs = outputs
        .iter()
        .map(|output| {
            Ok(PlannedOutput {
                path: normalize(output.root, &output.path)?,
                kind: output.kind,
            })
        })
        .collect::<Result<Vec<_>, StepGraphError>>()?;

    Ok((step_inputs, planned_outputs))
}

/// A compiled glob input.
struct GlobPattern {
    /// Matches whole paths.
    matcher: GlobMatcher,
    /// The components of the pattern, to find matches below a tree output.
    components: Vec<ComponentPattern>,
}

/// One `/`-separated component of a glob pattern.
enum ComponentPattern {
    /// `**`: any number of components.
    Recursive,
    /// Exactly one component.
    Single(GlobMatcher),
}

impl GlobPattern {
    fn new(pattern: &str, case_insensitive: bool) -> Result<Self, String> {
        let matcher = compile_glob(pattern, case_insensitive)?;
        let components = pattern
            .split('/')
            .map(|component| match component {
                "**" => Ok(ComponentPattern::Recursive),
                component => compile_glob(component, case_insensitive)
                    .map(ComponentPattern::Single)
                    .map_err(|_| {
                        format!(
                            "`{component}` is not a complete pattern; alternatives (`{{a,b}}`) and classes (`[ab]`) must not contain a path separator"
                        )
                    }),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            matcher,
            components,
        })
    }

    /// Whether the glob input `input` covers `output`: the pattern matches the
    /// output or a directory containing it, or, for a tree output, can match
    /// a path inside the tree.
    fn covers(&self, input: &StepPath, output: &PlannedOutput) -> bool {
        let path = output.path.as_str();
        if input.identity_root != output.path.identity_root {
            return false;
        }
        if self.matcher.is_match(path) || ancestors(path).any(|dir| self.matcher.is_match(dir)) {
            return true;
        }
        match output.kind {
            StepOutputKind::File => false,
            StepOutputKind::Tree => self.may_match_below(path),
        }
    }

    /// Whether the pattern can match a path strictly below `dir`.
    fn may_match_below(&self, dir: &str) -> bool {
        let dir: Vec<&str> = dir.split('/').collect();
        let columns = dir.len() + 1;
        let mut visited = vec![false; (self.components.len() + 1) * columns];
        // (pattern component, directory component) pairs still to explore.
        let mut pending = vec![(0, 0)];
        while let Some((pattern_at, dir_at)) = pending.pop() {
            let state = pattern_at * columns + dir_at;
            if visited[state] {
                continue;
            }
            visited[state] = true;
            if dir_at == dir.len() {
                // All of `dir` matched; any remaining component matches below it.
                if pattern_at < self.components.len() {
                    return true;
                }
                continue;
            }
            match self.components.get(pattern_at) {
                Some(ComponentPattern::Recursive) => {
                    pending.push((pattern_at + 1, dir_at));
                    pending.push((pattern_at, dir_at + 1));
                }
                Some(ComponentPattern::Single(matcher)) if matcher.is_match(dir[dir_at]) => {
                    pending.push((pattern_at + 1, dir_at + 1));
                }
                Some(ComponentPattern::Single(_)) | None => {}
            }
        }
        false
    }
}

fn compile_glob(pattern: &str, case_insensitive: bool) -> Result<GlobMatcher, String> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .case_insensitive(case_insensitive)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|err| err.kind().to_string())
}

/// The proper ancestors of a normalized path, outermost first.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(end, _)| &path[..end])
}

/// Output `output` of step `step`.
#[derive(Debug, Clone, Copy)]
struct OutputRef {
    step: usize,
    output: usize,
    kind: StepOutputKind,
}

/// The producer of every declared output, by root and normalized path.
struct OutputOwners<'a> {
    /// The outputs of every step.
    outputs: &'a [Vec<PlannedOutput>],
    work: BTreeMap<&'a str, OutputRef>,
    host: BTreeMap<&'a str, OutputRef>,
    build: BTreeMap<&'a str, OutputRef>,
}

impl<'a> OutputOwners<'a> {
    /// Indexes the outputs of all steps, rejecting any two outputs that are
    /// the same path or lie inside one another.
    fn new(
        steps: &[&GraphStep],
        outputs: &'a [Vec<PlannedOutput>],
    ) -> Result<Self, StepGraphError> {
        let mut owners = Self {
            outputs,
            work: BTreeMap::new(),
            host: BTreeMap::new(),
            build: BTreeMap::new(),
        };
        let conflict = |first: OutputRef, second: OutputRef| {
            let (first, second) = if (first.step, first.output) <= (second.step, second.output) {
                (first, second)
            } else {
                (second, first)
            };
            StepGraphError::ConflictingOutputs {
                first_step: StepRef::new(first.step, steps[first.step]),
                first_path: outputs[first.step][first.output].path.to_string(),
                second_step: StepRef::new(second.step, steps[second.step]),
                second_path: outputs[second.step][second.output].path.to_string(),
            }
        };

        for output in Self::refs(outputs) {
            let path = &outputs[output.step][output.output].path;
            match owners.root_mut(path.identity_root).entry(path.key.as_str()) {
                Entry::Vacant(entry) => {
                    entry.insert(output);
                }
                Entry::Occupied(entry) => return Err(conflict(*entry.get(), output)),
            }
        }
        for output in Self::refs(outputs) {
            let path = &outputs[output.step][output.output].path;
            let map = owners.root(path.identity_root);
            for dir in ancestors(&path.key) {
                if let Some(owner) = map.get(dir) {
                    return Err(conflict(*owner, output));
                }
            }
        }
        Ok(owners)
    }

    fn refs(outputs: &[Vec<PlannedOutput>]) -> impl Iterator<Item = OutputRef> + '_ {
        outputs.iter().enumerate().flat_map(|(step, step_outputs)| {
            step_outputs
                .iter()
                .enumerate()
                .map(move |(output, planned)| OutputRef {
                    step,
                    output,
                    kind: planned.kind,
                })
        })
    }

    fn root(&self, root: StepRoot) -> &BTreeMap<&'a str, OutputRef> {
        match root {
            StepRoot::Work => &self.work,
            StepRoot::Host => &self.host,
            StepRoot::Build => &self.build,
        }
    }

    fn root_mut(&mut self, root: StepRoot) -> &mut BTreeMap<&'a str, OutputRef> {
        match root {
            StepRoot::Work => &mut self.work,
            StepRoot::Host => &mut self.host,
            StepRoot::Build => &mut self.build,
        }
    }

    /// Every output of every step.
    fn all(&self) -> impl Iterator<Item = OutputRef> + 'a {
        Self::refs(self.outputs)
    }

    fn output(&self, output: &OutputRef) -> &'a PlannedOutput {
        &self.outputs[output.step][output.output]
    }

    /// The output that is `path`, or the tree output containing it.
    fn owner(&self, path: &StepPath) -> Option<OutputRef> {
        let map = self.root(path.identity_root);
        if let Some(owner) = map.get(path.key.as_str()) {
            return Some(*owner);
        }
        ancestors(&path.key)
            .filter_map(|dir| map.get(dir))
            .find(|owner| matches!(owner.kind, StepOutputKind::Tree))
            .copied()
    }

    /// The outputs strictly below `path`.
    fn below(&self, path: &StepPath) -> impl Iterator<Item = OutputRef> {
        // Every key that starts with `<path>/` sorts at or after `<path>/`
        // and before `<path>0`, as `0` directly follows `/`.
        let start = format!("{}/", path.key);
        let end = format!("{}0", path.key);
        self.root(path.identity_root)
            .range::<str, _>((
                Bound::Included(start.as_str()),
                Bound::Excluded(end.as_str()),
            ))
            .map(|(_, owner)| *owner)
    }
}

/// Makes every barrier wait for the steps listed since the previous barrier,
/// or for the previous barrier when there are none, and every step wait for
/// the barrier listed last before it.
fn add_barrier_edges(steps: &[&GraphStep], edges: &mut [Vec<Edge>]) {
    let barrier_edge = |from| Edge {
        from,
        reason: EdgeReason::Barrier,
    };
    let mut last_barrier = None;
    let mut since_barrier = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        if step.is_barrier() {
            if since_barrier.is_empty() {
                edges[index].extend(last_barrier.map(barrier_edge));
            }
            edges[index].extend(since_barrier.drain(..).map(barrier_edge));
            last_barrier = Some(index);
        } else {
            edges[index].extend(last_barrier.map(barrier_edge));
            since_barrier.push(index);
        }
    }
}

/// Orders the nodes so that every node follows its dependencies, taking the
/// ready node listed first at every point. Returns the nodes that cannot be
/// ordered when there is a cycle.
fn topological_order(nodes: &[Node]) -> Result<Vec<usize>, Vec<bool>> {
    let mut waiting_for: Vec<usize> = nodes.iter().map(|node| node.dependencies.len()).collect();
    let mut ready: BinaryHeap<Reverse<usize>> = waiting_for
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(index, _)| Reverse(index))
        .collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(Reverse(index)) = ready.pop() {
        order.push(index);
        for &dependent in &nodes[index].dependents {
            waiting_for[dependent] -= 1;
            if waiting_for[dependent] == 0 {
                ready.push(Reverse(dependent));
            }
        }
    }
    if order.len() == nodes.len() {
        Ok(order)
    } else {
        Err(waiting_for.into_iter().map(|count| count > 0).collect())
    }
}

/// Describes a cycle among the `remaining` nodes, which could not be ordered.
fn cycle_error(
    steps: &[&GraphStep],
    nodes: &[Node],
    edges: &[Vec<Edge>],
    remaining: &[bool],
) -> StepGraphError {
    // Every remaining node waits for a remaining node, so following those
    // dependencies from any remaining node runs into a cycle.
    let unordered_dependency = |index: usize| {
        edges[index]
            .iter()
            .find(|edge| remaining[edge.from])
            .copied()
    };
    let mut seen_at = vec![None; nodes.len()];
    let mut path: Vec<(usize, Edge)> = Vec::new();
    let mut current = remaining.iter().position(|&left| left);
    while let Some(index) = current {
        if let Some(start) = seen_at[index] {
            path.drain(..start);
            break;
        }
        let Some(edge) = unordered_dependency(index) else {
            break;
        };
        seen_at[index] = Some(path.len());
        path.push((index, edge));
        current = Some(edge.from);
    }

    let step_ref = |index: usize| StepRef::new(index, steps[index]);
    let chain = path
        .iter()
        .map(|&(waiting, edge)| {
            let reason = match edge.reason {
                EdgeReason::DependsOn => "it is named in `depends_on`".to_string(),
                EdgeReason::Barrier if nodes[waiting].barrier => {
                    "a step without `inputs` and `outputs` waits for every step listed before it"
                        .to_string()
                }
                EdgeReason::Barrier => {
                    "every step waits for the last step without `inputs` and `outputs` listed before it"
                        .to_string()
                }
                EdgeReason::Artifact { input, output } => format!(
                    "input `{}` needs its output `{}`",
                    nodes[waiting].inputs[input].path, nodes[edge.from].outputs[output].path
                ),
            };
            format!(
                "  {} waits for {}: {reason}",
                step_ref(waiting),
                step_ref(edge.from)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    StepGraphError::Cycle { chain }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rattler_conda_types::Platform;

    use super::{StepGraph, StepGraphError, is_short_name};
    use crate::execution_context::PrefixLayout;
    use crate::step_model::{
        GraphStep, StepInput, StepInputKind, StepOutput, StepOutputKind, StepRoot,
    };

    fn input(root: StepRoot, path: &str, kind: StepInputKind) -> StepInput {
        StepInput {
            root,
            path: PathBuf::from(path),
            kind,
        }
    }

    fn file(path: &str) -> StepInput {
        input(StepRoot::Work, path, StepInputKind::File)
    }

    fn glob(path: &str) -> StepInput {
        input(StepRoot::Work, path, StepInputKind::Glob)
    }

    fn output(root: StepRoot, path: &str, kind: StepOutputKind) -> StepOutput {
        StepOutput {
            root,
            path: PathBuf::from(path),
            kind,
        }
    }

    fn out_file(path: &str) -> StepOutput {
        output(StepRoot::Work, path, StepOutputKind::File)
    }

    fn out_tree(path: &str) -> StepOutput {
        output(StepRoot::Work, path, StepOutputKind::Tree)
    }

    fn declared(id: &str, inputs: Vec<StepInput>, outputs: Vec<StepOutput>) -> GraphStep {
        GraphStep {
            id: Some(id.to_string()),
            inputs: Some(inputs),
            outputs: Some(outputs),
            depends_on: Vec::new(),
        }
    }

    fn barrier() -> GraphStep {
        GraphStep::default()
    }

    fn plan(steps: &[GraphStep]) -> StepGraph {
        StepGraph::new(steps, Platform::Linux64, PrefixLayout::Separate).unwrap()
    }

    fn error(steps: &[GraphStep], platform: Platform) -> String {
        StepGraph::new(steps, platform, PrefixLayout::Separate)
            .unwrap_err()
            .to_string()
    }

    fn dependencies(graph: &StepGraph) -> Vec<Vec<usize>> {
        (0..graph.len())
            .map(|index| graph.dependencies(index).to_vec())
            .collect()
    }

    fn source_inputs(graph: &StepGraph, index: usize) -> Vec<String> {
        graph
            .source_inputs(index)
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn undeclared_steps_separate_the_declared_steps_around_them() {
        let steps = [
            declared("a", vec![], vec![]),
            declared("b", vec![], vec![]),
            barrier(),
            declared("d", vec![], vec![]),
            declared("e", vec![], vec![]),
            barrier(),
            barrier(),
        ];
        let graph = plan(&steps);

        // Explicitly empty declarations leave `a`, `b`, `d` and `e` free to
        // overlap within their group; the undeclared steps overlap nothing.
        assert_eq!(
            dependencies(&graph),
            [
                vec![],
                vec![],
                vec![0, 1],
                vec![2],
                vec![2],
                vec![3, 4],
                vec![5]
            ]
        );
        assert_eq!(graph.dependents(2), [3, 4]);
        assert_eq!(graph.topological_order(), [0, 1, 2, 3, 4, 5, 6]);
        assert!(graph.is_barrier(2));
        assert!(!graph.is_barrier(0));
    }

    #[test]
    fn consumers_wait_for_the_producers_of_what_they_read() {
        let steps = [
            declared(
                "link",
                vec![file("obj/a.o"), glob("gen/**/*.h")],
                vec![out_file("bin/app")],
            ),
            declared("compile", vec![file("src/a.c")], vec![out_file("obj/a.o")]),
            declared("headers", vec![], vec![out_tree("gen")]),
            // A directory input covers everything produced inside it.
            declared("package", vec![file("bin")], vec![out_file("app.tar")]),
            declared(
                "docs",
                vec![file("README.md")],
                vec![out_file("index.html")],
            ),
        ];
        let graph = plan(&steps);

        assert_eq!(
            dependencies(&graph),
            [vec![1, 2], vec![], vec![], vec![0], vec![]]
        );
        // List order does not delay independent steps behind their consumers.
        assert_eq!(graph.topological_order(), [1, 2, 0, 3, 4]);
        assert_eq!(graph.inputs(0)[1].producers(), [2]);

        assert_eq!(source_inputs(&graph, 0), Vec::<String>::new());
        assert_eq!(source_inputs(&graph, 1), ["work:src/a.c"]);
        assert_eq!(source_inputs(&graph, 3), ["work:bin"]);
    }

    #[test]
    fn globs_wait_only_for_trees_they_can_match_inside() {
        let archive = declared(
            "archive",
            vec![file("build/obj/a.o")],
            vec![out_tree("build/lib")],
        );

        let configure = declared(
            "configure",
            vec![glob("build/*.cfg")],
            vec![out_tree("build/obj")],
        );
        let graph = plan(&[configure, archive.clone()]);
        assert_eq!(dependencies(&graph), [vec![], vec![0]]);

        // `**` can match inside `build/lib`, which is only written after
        // `build/obj`.
        let configure = declared(
            "configure",
            vec![glob("build/**/*.cfg")],
            vec![out_tree("build/obj")],
        );
        assert!(matches!(
            StepGraph::new(
                &[configure, archive],
                Platform::Linux64,
                PrefixLayout::Separate
            ),
            Err(StepGraphError::Cycle { .. })
        ));
    }

    #[test]
    fn paths_are_normalized_lexically() {
        let steps = [
            declared("produce", vec![], vec![out_file("./obj//main.o/")]),
            declared("consume", vec![file(r"obj\.\main.o")], vec![]),
        ];
        let graph = plan(&steps);

        assert_eq!(graph.dependencies(1), [0]);
        let produced = graph.outputs(0)[0].path();
        assert_eq!(produced.to_string(), "work:obj/main.o");
        assert_eq!(produced.relative_path(), Path::new("obj").join("main.o"));
        assert_eq!(
            produced.resolve(Path::new("work")),
            Path::new("work").join("obj").join("main.o")
        );
    }

    #[test]
    fn paths_are_identified_by_root_and_by_case_only_off_windows_and_macos() {
        let steps = [
            declared(
                "host",
                vec![],
                vec![output(StepRoot::Host, "Lib/libz.so", StepOutputKind::File)],
            ),
            declared(
                "build",
                vec![],
                vec![output(StepRoot::Build, "Lib/libz.so", StepOutputKind::File)],
            ),
            declared(
                "exact",
                vec![input(StepRoot::Host, "lib/LIBZ.so", StepInputKind::File)],
                vec![],
            ),
            declared(
                "glob",
                vec![input(StepRoot::Host, "lib/*.SO", StepInputKind::Glob)],
                vec![],
            ),
        ];

        let linux = StepGraph::new(&steps, Platform::Linux64, PrefixLayout::Separate).unwrap();
        assert_eq!(dependencies(&linux), vec![Vec::<usize>::new(); 4]);
        for platform in [Platform::Win64, Platform::OsxArm64, Platform::Osx64] {
            let graph = StepGraph::new(&steps, platform, PrefixLayout::Separate).unwrap();
            assert_eq!(dependencies(&graph), [vec![], vec![], vec![0], vec![0]]);
        }

        let differing_case = [
            declared("a", vec![], vec![out_file("Lib/Foo.dll")]),
            declared("b", vec![], vec![out_file("lib/foo.dll")]),
        ];
        plan(&differing_case);
        for platform in [Platform::Win64, Platform::OsxArm64, Platform::Osx64] {
            assert_eq!(
                error(&differing_case, platform),
                "step 0 (`a`) writes `work:Lib/Foo.dll` and step 1 (`b`) writes `work:lib/foo.dll`; declared outputs must not be the same path or lie inside one another"
            );
        }
        // Only Windows drops trailing dots and spaces.
        let trailing_dot = [declared("a", vec![], vec![out_file("obj/a.")])];
        StepGraph::new(&trailing_dot, Platform::OsxArm64, PrefixLayout::Separate).unwrap();
    }

    #[test]
    fn host_and_build_paths_are_the_same_in_a_shared_prefix() {
        let steps = [
            declared(
                "install",
                vec![],
                vec![output(StepRoot::Host, "lib/libz.so", StepOutputKind::File)],
            ),
            declared(
                "exact",
                vec![input(StepRoot::Build, "lib", StepInputKind::File)],
                vec![],
            ),
            declared(
                "glob",
                vec![input(StepRoot::Build, "lib/*.so", StepInputKind::Glob)],
                vec![],
            ),
        ];
        let separate = StepGraph::new(&steps, Platform::Linux64, PrefixLayout::Separate).unwrap();
        assert_eq!(dependencies(&separate), vec![Vec::<usize>::new(); 3]);
        let shared = StepGraph::new(&steps, Platform::Linux64, PrefixLayout::Shared).unwrap();
        assert_eq!(dependencies(&shared), [vec![], vec![0], vec![0]]);
        // Diagnostics and resolution keep the declared root.
        assert_eq!(source_inputs(&shared, 1), ["build:lib"]);

        let overlapping = [
            declared(
                "host",
                vec![],
                vec![output(StepRoot::Host, "bin", StepOutputKind::Tree)],
            ),
            declared(
                "build",
                vec![],
                vec![output(StepRoot::Build, "bin/tool", StepOutputKind::File)],
            ),
        ];
        StepGraph::new(&overlapping, Platform::Linux64, PrefixLayout::Separate).unwrap();
        assert_eq!(
            StepGraph::new(&overlapping, Platform::Linux64, PrefixLayout::Shared)
                .unwrap_err()
                .to_string(),
            "step 0 (`host`) writes `host:bin` and step 1 (`build`) writes `build:bin/tool`; declared outputs must not be the same path or lie inside one another"
        );
    }

    #[test]
    fn declared_outputs_must_not_overlap() {
        let site = declared("site", vec![], vec![out_file("lib/python/site.py")]);
        let lib = declared("lib", vec![], vec![out_tree("lib")]);
        let copy = declared("copy", vec![], vec![out_file("lib/python/site.py")]);
        let install = declared(
            "install",
            vec![],
            vec![out_tree("lib"), out_file("lib/python/site.py")],
        );

        assert_eq!(
            error(&[site.clone(), lib], Platform::Linux64),
            "step 0 (`site`) writes `work:lib/python/site.py` and step 1 (`lib`) writes `work:lib`; declared outputs must not be the same path or lie inside one another"
        );
        assert_eq!(
            error(&[site, copy], Platform::Linux64),
            "step 0 (`site`) writes `work:lib/python/site.py` and step 1 (`copy`) writes `work:lib/python/site.py`; declared outputs must not be the same path or lie inside one another"
        );
        assert_eq!(
            error(&[install], Platform::Linux64),
            "step 0 (`install`) writes `work:lib` and step 0 (`install`) writes `work:lib/python/site.py`; declared outputs must not be the same path or lie inside one another"
        );
    }

    #[test]
    fn a_step_cannot_read_its_own_outputs() {
        let rewrite = declared(
            "rewrite",
            vec![glob("src/*.c")],
            vec![out_file("src/gen.c")],
        );
        assert_eq!(
            error(&[rewrite], Platform::Linux64),
            "build steps form a dependency cycle:
  step 0 (`rewrite`) waits for step 0 (`rewrite`): input `work:src/*.c` needs its output `work:src/gen.c`"
        );
    }

    #[test]
    fn cycles_name_every_dependency_and_its_reason() {
        let steps = [
            GraphStep {
                depends_on: vec!["link".to_string()],
                ..declared("compile", vec![file("src/a.c")], vec![out_file("obj/a.o")])
            },
            declared("link", vec![file("obj/a.o")], vec![out_file("bin/app")]),
        ];
        assert_eq!(
            error(&steps, Platform::Linux64),
            "build steps form a dependency cycle:
  step 0 (`compile`) waits for step 1 (`link`): it is named in `depends_on`
  step 1 (`link`) waits for step 0 (`compile`): input `work:obj/a.o` needs its output `work:obj/a.o`"
        );

        let depends_on_itself = [GraphStep {
            depends_on: vec!["self".to_string()],
            ..declared("self", vec![], vec![])
        }];
        assert_eq!(
            error(&depends_on_itself, Platform::Linux64),
            "build steps form a dependency cycle:
  step 0 (`self`) waits for step 0 (`self`): it is named in `depends_on`"
        );
    }

    #[test]
    fn a_step_cannot_read_what_is_written_after_a_later_barrier() {
        let steps = [
            declared("use", vec![file("gen.h")], vec![]),
            barrier(),
            declared("generate", vec![], vec![out_file("gen.h")]),
        ];
        assert_eq!(
            error(&steps, Platform::Linux64),
            "build steps form a dependency cycle:
  step 0 (`use`) waits for step 2 (`generate`): input `work:gen.h` needs its output `work:gen.h`
  step 2 (`generate`) waits for step 1: every step waits for the last step without `inputs` and `outputs` listed before it
  step 1 waits for step 0 (`use`): a step without `inputs` and `outputs` waits for every step listed before it"
        );
    }

    #[test]
    fn invalid_declarations_are_rejected() {
        let only_inputs = GraphStep {
            inputs: Some(Vec::new()),
            ..barrier()
        };
        let unknown_dependency = GraphStep {
            depends_on: vec!["missing".to_string()],
            ..barrier()
        };
        let empty_id = GraphStep {
            id: Some(String::new()),
            ..barrier()
        };
        let with_input = |input| declared("a", vec![input], vec![]);

        let cases = [
            (
                vec![only_inputs],
                "step 0 declares `inputs` but not `outputs`; declare both (an empty list declares none) or neither to run the step as a sequential barrier",
            ),
            (
                vec![
                    declared("a", vec![], vec![]),
                    barrier(),
                    declared("a", vec![], vec![]),
                ],
                "step 0 and step 2 both have the id `a`; step ids must be unique",
            ),
            (vec![empty_id], "step 0 has an empty `id`"),
            (
                vec![unknown_dependency],
                "step 0 depends on `missing`, but no step has that id",
            ),
            (
                vec![with_input(file("/usr/include"))],
                "step 0 (`a`) declares the invalid path `/usr/include`: the path is absolute; declare it relative to its root",
            ),
            (
                vec![with_input(file("C:/include"))],
                "step 0 (`a`) declares the invalid path `C:/include`: the path starts with a drive; declare it relative to its root",
            ),
            (
                vec![with_input(file("src/../include"))],
                "step 0 (`a`) declares the invalid path `src/../include`: the path contains a `..` component; declare it without `..`",
            ),
            (
                vec![with_input(glob("src/../*.h"))],
                "step 0 (`a`) declares the invalid path `src/../*.h`: the path contains a `..` component; declare it without `..`",
            ),
            (
                vec![with_input(file("scratch/C:/victim"))],
                "step 0 (`a`) declares the invalid path `scratch/C:/victim`: the path contains `:`, which Windows reads as a drive or a file stream; declare it without `:`",
            ),
            (
                vec![declared("a", vec![], vec![out_file("obj/a.o:stream")])],
                "step 0 (`a`) declares the invalid path `obj/a.o:stream`: the path contains `:`, which Windows reads as a drive or a file stream; declare it without `:`",
            ),
            (
                vec![with_input(file("./"))],
                "step 0 (`a`) declares the invalid path `./`: the path is empty or names the root directory itself",
            ),
            (
                vec![with_input(glob("src/{a,b"))],
                "step 0 (`a`) declares the invalid glob `work:src/{a,b`: unclosed alternate group; missing '}' (maybe escape '{' with '[{]'?)",
            ),
            (
                vec![with_input(glob("src/{a,b/c}.h"))],
                "step 0 (`a`) declares the invalid glob `work:src/{a,b/c}.h`: `{a,b` is not a complete pattern; alternatives (`{a,b}`) and classes (`[ab]`) must not contain a path separator",
            ),
        ];
        for (steps, expected) in cases {
            assert_eq!(error(&steps, Platform::Linux64), expected);
        }
    }

    #[test]
    fn windows_rejects_components_it_would_rename() {
        let aliases = |alias: &str| {
            [
                declared("plain", vec![], vec![out_file("obj/a")]),
                declared("alias", vec![], vec![out_file(alias)]),
            ]
        };

        for alias in [
            "obj/a.",
            "obj/a ",
            "obj./a.o",
            r"obj \a.o",
            "obj/*.",
            "obj/a...",
        ] {
            // Elsewhere these are names of their own.
            let linux = plan(&aliases(alias));
            assert_eq!(dependencies(&linux), vec![Vec::<usize>::new(); 2]);

            assert_eq!(
                error(&aliases(alias), Platform::Win64),
                format!(
                    "step 1 (`alias`) declares the invalid path `{alias}`: a component of the path ends in `.` or a space, which Windows drops from file names; declare it without them"
                )
            );
        }

        let glob_base = [declared("a", vec![glob("gen ./*.h")], vec![])];
        assert_eq!(
            error(&glob_base, Platform::Win64),
            "step 0 (`a`) declares the invalid path `gen ./*.h`: a component of the path ends in `.` or a space, which Windows drops from file names; declare it without them"
        );
    }

    #[test]
    fn windows_rejects_components_spelled_like_short_names() {
        for name in [
            "CONDA_~1",
            "CONDA_~1.TXT",
            "FOO~12.BAR",
            "a~1.b",
            "~1",
            "~123.x",
        ] {
            assert!(is_short_name(name), "{name}");
        }
        for name in [
            "~", "a~", "a~.b", "a~b1", "~1a", "a~1.b.c", "a.~1", "~/1", "a1", "*~?",
        ] {
            assert!(!is_short_name(name), "{name}");
        }

        let short_output = |output: StepOutput| [declared("clean", vec![], vec![output])];
        for alias in ["CONDA_~1", r"obj\CONDA_~1.TXT/a.o", "FOO~12.BAR"] {
            // Elsewhere these are names of their own.
            plan(&short_output(out_tree(alias)));

            assert_eq!(
                error(&short_output(out_tree(alias)), Platform::Win64),
                format!(
                    "step 0 (`clean`) declares the invalid path `{alias}`: a component of the path ends in `~` and digits before its extension, which Windows may read as the short name of another file; declare the full name"
                )
            );
        }

        let glob_base = [declared("a", vec![glob("GEN~1/*.h")], vec![])];
        assert_eq!(
            error(&glob_base, Platform::Win64),
            "step 0 (`a`) declares the invalid path `GEN~1/*.h`: a component of the path ends in `~` and digits before its extension, which Windows may read as the short name of another file; declare the full name"
        );

        // A `~` that does not end the name before its extension is a plain
        // character.
        let tildes = [declared(
            "tildes",
            vec![file("a~b/c~"), glob("gen~/*~?.h")],
            vec![out_file("backup.txt~"), out_tree("x~1y.tar.gz")],
        )];
        StepGraph::new(&tildes, Platform::Win64, PrefixLayout::Separate).unwrap();
    }
}
