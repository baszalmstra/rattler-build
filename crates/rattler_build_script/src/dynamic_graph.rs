//! A graph of build steps that grows while the steps run.
//!
//! Every build step may declare further steps once it succeeded, in a
//! [`StepManifest`]. [`DynamicStepGraph`] starts from the listed steps,
//! validated as [`StepGraph`](crate::StepGraph) validates them, and registers
//! what a step declared right after it succeeded. The scheduler asks it which
//! steps can start; the graph itself runs nothing and reads no files.
//!
//! A step declared by step `G` has the qualified id `G/A`, where `A` is its
//! own id, and a step declared by the listed step without an id at position
//! 2 is `@2/A`, which no reference can name. A bare reference in a manifest
//! names a step of the same manifest by its own id. A qualified reference
//! always names a step by its full id, even when its first component also
//! names a step of the same manifest.
//!
//! A step waits for another step in one of two ways:
//!
//! - Until it is *done*: it succeeded and what it declared is registered.
//!   An input waits for the producers of what it covers to be done, and
//!   `discover_after` for the steps it names.
//! - Until it is *complete*: it is done and every step it declared is
//!   complete, recursively. `depends_on` waits for the steps it names to be
//!   complete, and so do barriers. A step without `inputs` and `outputs`
//!   waits for the steps before it since the previous such step, and the
//!   steps after it wait for it, where the steps before and after are the
//!   listed steps for a listed step and the steps of the same manifest for
//!   a declared one. Declared steps start only after the step declaring
//!   them, so after the barrier before it, and a barrier after it waits for
//!   all it declared.
//!
//! A consumer of a file a declared step writes thus waits for that step, not
//! for everything its declaring step declared, and two steps can declare
//! steps that depend on each other's steps in both directions (`G/A` before
//! `H/B` before `G/C`) without a cycle.
//!
//! A reference to a qualified id no step has yet waits as long as the step
//! that would declare it has not registered its declarations, and is an
//! error once it has. Registering a manifest is all or nothing: its steps and
//! updates are validated together with the steps already known (paths,
//! output ownership, references and cycles) before anything changes. And a
//! manifest cannot change what a step that may already have started reads
//! or waits for: a step can only be updated, or get a producer for what one
//! of its inputs covers, when it waits for the declaring step, directly or
//! through other steps.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::mem;

use rattler_conda_types::Platform;

use crate::execution_context::PrefixLayout;
use crate::step_graph::{
    Edge, EdgeReason, InputIndex, InputRef, LateProducer, OutputIndex, OutputRef, PathIdentity,
    PlannedInput, PlannedOutput, QualifiedRefs, RefField, StaticPlan, StepGraphError, StepPath,
    StepRef, Wait, add_barrier_edges, declared_lists, dedup_edges, output_conflict, plan_inputs,
    plan_outputs,
};
use crate::step_manifest::{GeneratedStep, StepManifest};
use crate::step_model::{GraphStep, STEP_ID_SEPARATOR, StepInput};

/// Where a step is in its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The step has not started.
    Waiting,
    /// The step has started and has not been registered as succeeded.
    Running,
    /// The step succeeded and its declarations are registered.
    Succeeded,
    /// The step failed.
    Failed,
}

/// One step of the graph.
#[derive(Debug)]
struct Node {
    /// The (qualified) id.
    id: Option<String>,
    /// The step that declared this one.
    producer: Option<usize>,
    /// The declaration of a declared step.
    declaration: Option<GeneratedStep>,
    barrier: bool,
    inputs: Vec<PlannedInput>,
    /// Inputs the step reported after running; later outputs must not turn
    /// these reads into dependencies that the step could no longer wait for.
    reported_inputs: Vec<PlannedInput>,
    outputs: Vec<PlannedOutput>,
    /// For every output, the step that declared it while the build ran:
    /// the step that declared this one, or one that updated it.
    output_declarers: Vec<Option<usize>>,
    /// What the step waits for.
    edges: Vec<Edge>,
    /// The steps this one declared.
    children: Vec<usize>,
    state: State,
    /// Whether the step and every step it declared, recursively, succeeded.
    complete: bool,
    /// Whether the step was handed out as ready to start.
    released: bool,
    /// How many edges and pending references the step still waits for.
    blockers: usize,
    /// How many of the steps this one declared are not complete.
    open_children: usize,
    /// The steps whose edges wait for this step to be done, while it is not.
    done_waiters: Vec<usize>,
    /// The steps whose edges wait for this step to be complete, while it is
    /// not.
    complete_waiters: Vec<usize>,
}

impl Node {
    fn new(
        id: Option<String>,
        producer: Option<usize>,
        declaration: Option<GeneratedStep>,
        barrier: bool,
        inputs: Vec<PlannedInput>,
        outputs: Vec<PlannedOutput>,
    ) -> Self {
        Self {
            id,
            producer,
            declaration,
            barrier,
            inputs,
            reported_inputs: Vec::new(),
            output_declarers: vec![producer; outputs.len()],
            outputs,
            edges: Vec::new(),
            children: Vec::new(),
            state: State::Waiting,
            complete: false,
            released: false,
            blockers: 0,
            open_children: 0,
            done_waiters: Vec::new(),
            complete_waiters: Vec::new(),
        }
    }
}

/// A reference to a qualified id that no step has yet, but that step
/// `declarer` may still declare.
#[derive(Debug, Clone)]
struct PendingRef {
    /// The step naming the id.
    step: usize,
    field: RefField,
    /// The qualified id, absolute.
    reference: String,
    /// The step with the longest known prefix of the id.
    declarer: usize,
}

/// A point in the life of a step that other steps wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vertex {
    /// The step is done.
    Done(usize),
    /// The step is complete.
    Complete(usize),
}

impl Vertex {
    fn of(step: usize, wait: Wait) -> Self {
        match wait {
            Wait::Done => Self::Done(step),
            Wait::Complete => Self::Complete(step),
        }
    }

    /// A distinct number for every vertex, ordered by step and then done
    /// before complete.
    fn slot(self) -> usize {
        match self {
            Self::Done(step) => 2 * step,
            Self::Complete(step) => 2 * step + 1,
        }
    }
}

/// The scheduling graph of the steps of one build, growing with the steps
/// they declare.
///
/// Steps are identified by their position: the listed steps first, in list
/// order, and then every declared step, in the order they were registered.
/// Positions never change.
#[derive(Debug)]
pub struct DynamicStepGraph {
    identity: PathIdentity,
    nodes: Vec<Node>,
    /// The position of every step with an id, by qualified id.
    ids: HashMap<String, usize>,
    owners: OutputIndex,
    readers: InputIndex,
    reported_readers: InputIndex,
    pending: Vec<PendingRef>,
    /// Steps that became ready since the last [`Self::take_ready`].
    ready: Vec<usize>,
    /// Counts registrations and retained input reports, so an [`Expansion`]
    /// prepared before either changed the graph cannot be registered on top.
    revision: u64,
}

impl DynamicStepGraph {
    /// Validates the declarations of `steps` and plans their dependencies,
    /// as [`StepGraph::new`](crate::StepGraph::new) does. A reference to a
    /// qualified id waits for the step that may declare it, which has to be
    /// one of the listed steps.
    pub fn new<'a>(
        steps: impl IntoIterator<Item = &'a GraphStep>,
        platform: Platform,
        layout: PrefixLayout,
    ) -> Result<Self, StepGraphError> {
        let identity = PathIdentity::new(platform, layout);
        let plan = StaticPlan::new(steps.into_iter().collect(), identity, QualifiedRefs::Defer)?;
        // Cycles among the listed steps are reported as `StepGraph` does.
        plan.flatten()?;
        let StaticPlan {
            steps,
            ids,
            inputs,
            outputs,
            owners,
            edges,
            qualified,
        } = plan;

        let mut graph = Self {
            identity,
            nodes: Vec::with_capacity(steps.len()),
            ids: ids
                .into_iter()
                .map(|(id, index)| (id.to_string(), index))
                .collect(),
            owners,
            readers: InputIndex::default(),
            reported_readers: InputIndex::default(),
            pending: Vec::new(),
            ready: Vec::new(),
            revision: 0,
        };
        for (index, ((step, inputs), outputs)) in steps.iter().zip(inputs).zip(outputs).enumerate()
        {
            for (position, input) in inputs.iter().enumerate() {
                graph.readers.insert(
                    InputRef {
                        step: index,
                        input: position,
                    },
                    input,
                );
            }
            graph.nodes.push(Node::new(
                step.id.clone(),
                None,
                None,
                step.is_barrier(),
                inputs,
                outputs,
            ));
        }

        let scope = Scope {
            producer: None,
            prefix: "",
            local: HashMap::new(),
        };
        let mut links = Vec::new();
        for reference in &qualified {
            let waiter = StepRef::new(reference.step, steps[reference.step]);
            let link = graph.link_absolute(
                &waiter,
                reference.field,
                reference.reference.to_string(),
                &scope,
            )?;
            links.push((reference.step, reference.field, link));
        }
        for (index, step_edges) in edges.into_iter().enumerate() {
            for edge in step_edges {
                graph.push_edge(index, edge);
            }
        }
        for (step, field, link) in links {
            graph.add_link(step, field, link);
        }
        if !graph.pending.is_empty() {
            check_cycles(&View::committed(&graph))?;
        }
        for index in 0..graph.nodes.len() {
            graph.release_if_ready(index);
        }
        Ok(graph)
    }

    /// The number of steps known so far.
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

    /// The declared inputs of step `index`, including those updates added;
    /// empty for a barrier.
    pub fn inputs(&self, index: usize) -> &[PlannedInput] {
        &self.nodes[index].inputs
    }

    /// The declared outputs of step `index`, including those updates added;
    /// empty for a barrier.
    pub fn outputs(&self, index: usize) -> &[PlannedOutput] {
        &self.nodes[index].outputs
    }

    /// The single-path inputs of step `index` that no step produces; they
    /// have to exist before the step starts. Once the step can start, no
    /// step registered later can produce them.
    pub fn source_inputs(&self, index: usize) -> impl Iterator<Item = &StepPath> {
        self.nodes[index]
            .inputs
            .iter()
            .filter(|input| input.is_source())
            .map(PlannedInput::path)
    }

    /// The id of step `index`, qualified for a declared step; `None` for a
    /// listed step without an id.
    pub fn qualified_id(&self, index: usize) -> Option<&str> {
        self.nodes[index].id.as_deref()
    }

    /// The step that declared step `index`, if one did.
    pub fn producer(&self, index: usize) -> Option<usize> {
        self.nodes[index].producer
    }

    /// The declaration of step `index`, if another step declared it: what to
    /// run, how and where.
    pub fn generated(&self, index: usize) -> Option<&GeneratedStep> {
        self.nodes[index].declaration.as_ref()
    }

    /// Names step `index` in diagnostics.
    pub fn step_ref(&self, index: usize) -> StepRef {
        StepRef {
            index,
            id: self.nodes[index].id.clone(),
        }
    }

    /// The steps that became ready to start since the last call, in
    /// ascending order. Every step is returned once.
    pub fn take_ready(&mut self) -> Vec<usize> {
        let mut ready = mem::take(&mut self.ready);
        ready.sort_unstable();
        ready
    }

    /// Records that step `index`, which was ready, started.
    pub fn start(&mut self, index: usize) {
        if let Some(node) = self.nodes.get_mut(index)
            && node.state == State::Waiting
            && node.blockers == 0
        {
            node.state = State::Running;
            node.released = true;
        }
    }

    /// Records that step `index` failed, or that its declarations were
    /// rejected. Nothing that waits for it becomes ready.
    pub fn fail(&mut self, index: usize) {
        if let Some(node) = self.nodes.get_mut(index)
            && node.state != State::Succeeded
        {
            node.state = State::Failed;
        }
    }

    /// Whether every step known succeeded.
    pub fn is_finished(&self) -> bool {
        self.nodes.iter().all(|node| node.state == State::Succeeded)
    }

    /// Validates what the running step `producer` declared in `manifest`
    /// against the steps known, without registering it: its declared steps,
    /// which get the positions from [`Self::len`] on in the order declared,
    /// and its updates. [`Self::succeed`] registers the result, as long as
    /// no other step succeeded in between.
    ///
    /// Also checks that every reference to a step `producer` may declare,
    /// made before, names a step it declared.
    pub fn prepare(
        &self,
        producer: usize,
        manifest: StepManifest,
    ) -> Result<Expansion, StepGraphError> {
        let Some(node) = self.nodes.get(producer) else {
            return Err(StepGraphError::InvalidState {
                step: StepRef {
                    index: producer,
                    id: None,
                },
                problem: "is not a build step",
            });
        };
        let producer_ref = self.step_ref(producer);
        if node.state != State::Running {
            return Err(StepGraphError::InvalidState {
                step: producer_ref,
                problem: "is not running, so it cannot declare steps",
            });
        }
        manifest
            .validate()
            .map_err(|error| StepGraphError::InvalidManifest {
                producer: producer_ref.clone(),
                error,
            })?;
        let StepManifest { steps, updates } = manifest;

        let base = self.nodes.len();
        let prefix = node.id.clone().unwrap_or_else(|| format!("@{producer}"));
        let mut new_steps = Vec::with_capacity(steps.len());
        for (offset, declaration) in steps.into_iter().enumerate() {
            let id = format!("{prefix}{STEP_ID_SEPARATOR}{}", declaration.id);
            let step_ref = StepRef {
                index: base + offset,
                id: Some(id.clone()),
            };
            let (inputs, outputs) = declared_lists(
                &step_ref,
                declaration.inputs.as_deref(),
                declaration.outputs.as_deref(),
            )?;
            let inputs = plan_inputs(&step_ref, inputs, self.identity)?;
            let outputs = plan_outputs(&step_ref, outputs, self.identity)?;
            new_steps.push(NewStep {
                id,
                barrier: declaration.is_barrier(),
                declaration,
                inputs,
                outputs,
                edges: Vec::new(),
            });
        }

        // The steps that cannot start before `producer` is done.
        let mut behind: Option<Vec<bool>> = None;
        let mut additions = Vec::with_capacity(updates.len());
        for update in &updates {
            let Some(&target) = self.ids.get(update.step.as_str()) else {
                return Err(StepGraphError::UnknownUpdateTarget {
                    producer: producer_ref,
                    target: update.step.clone(),
                });
            };
            let target_ref = self.step_ref(target);
            let target_node = &self.nodes[target];
            if target_node.state != State::Waiting {
                return Err(StepGraphError::UpdateOfStartedStep {
                    producer: producer_ref,
                    target: target_ref,
                });
            }
            if !behind.get_or_insert_with(|| self.waiting_for(producer))[target] {
                return Err(StepGraphError::UpdateOfUnorderedStep {
                    producer: producer_ref,
                    target: target_ref,
                });
            }
            if target_node.barrier && !(update.inputs.is_empty() && update.outputs.is_empty()) {
                return Err(StepGraphError::UpdateOfBarrier {
                    producer: producer_ref,
                    target: target_ref,
                });
            }
            additions.push(Additions {
                step: target,
                inputs: plan_inputs(&target_ref, &update.inputs, self.identity)?,
                outputs: plan_outputs(&target_ref, &update.outputs, self.identity)?,
            });
        }

        let scope = Scope {
            producer: Some(producer),
            prefix: &prefix,
            local: new_steps
                .iter()
                .enumerate()
                .map(|(offset, step)| (step.declaration.id.as_str(), base + offset))
                .collect(),
        };
        let mut new_edges: Vec<Vec<Edge>> = vec![Vec::new(); new_steps.len()];
        let mut edges: Vec<(usize, Edge)> = Vec::new();
        let mut links: Vec<(usize, RefField, Link)> = Vec::new();
        for (offset, step) in new_steps.iter().enumerate() {
            let index = base + offset;
            let waiter = StepRef {
                index,
                id: Some(step.id.clone()),
            };
            let fields = [
                (RefField::DependsOn, &step.declaration.depends_on),
                (RefField::DiscoverAfter, &step.declaration.discover_after),
            ];
            for (field, references) in fields {
                for reference in references {
                    links.push((index, field, self.link(&waiter, field, reference, &scope)?));
                }
            }
        }
        for (update, step_additions) in updates.iter().zip(&additions) {
            let waiter = self.step_ref(step_additions.step);
            for reference in &update.depends_on {
                let link = self.link(&waiter, RefField::DependsOn, reference, &scope)?;
                links.push((step_additions.step, RefField::DependsOn, link));
            }
        }
        // References waiting for `producer` resolve now, or never.
        for pending in self
            .pending
            .iter()
            .filter(|pending| pending.declarer == producer)
        {
            let waiter = self.step_ref(pending.step);
            let link =
                self.link_absolute(&waiter, pending.field, pending.reference.clone(), &scope)?;
            links.push((pending.step, pending.field, link));
        }
        let mut pending = Vec::new();
        for (step, field, link) in links {
            match link {
                Link::Step(from) => {
                    let edge = Edge {
                        from,
                        reason: field.reason(),
                    };
                    match step.checked_sub(base) {
                        Some(offset) => new_edges[offset].push(edge),
                        None => edges.push((step, edge)),
                    }
                }
                Link::Pending {
                    reference,
                    declarer,
                } => pending.push(PendingRef {
                    step,
                    field,
                    reference,
                    declarer,
                }),
            }
        }

        let batch = View {
            graph: self,
            producer: Some(producer),
            base,
            steps: &new_steps,
            additions: &additions,
            edges: &[],
            pending: &[],
        };
        let mut staged_outputs: Vec<(OutputRef, &StepPath)> = Vec::new();
        for (offset, step) in new_steps.iter().enumerate() {
            for (position, output) in step.outputs.iter().enumerate() {
                staged_outputs.push((
                    OutputRef::new(base + offset, position, output),
                    output.path(),
                ));
            }
        }
        for step_additions in &additions {
            let known = self.nodes[step_additions.step].outputs.len();
            for (position, output) in step_additions.outputs.iter().enumerate() {
                staged_outputs.push((
                    OutputRef::new(step_additions.step, known + position, output),
                    output.path(),
                ));
            }
        }
        let owners = self
            .owners
            .stage(&staged_outputs)
            .map_err(|(first, second)| {
                output_conflict(
                    first,
                    second,
                    |step| batch.step_ref(step),
                    |output| batch.output_at(output.step, output.output),
                )
            })?;

        // A later output must not become the producer of a file a completed
        // step reported reading. Reports are indexed separately from declared
        // inputs: they describe reads that have already happened, not edges
        // that can be added to the scheduling graph.
        let mut reported_readers = Vec::new();
        for &(output_ref, _) in &staged_outputs {
            let output = batch.output_at(output_ref.step, output_ref.output);
            reported_readers.clear();
            self.reported_readers.readers(
                output,
                |input| &self.nodes[input.step].reported_inputs[input.input],
                &mut reported_readers,
            );
            if let Some(&(reader, _)) = reported_readers.first() {
                return Err(StepGraphError::LateProducer(Box::new(LateProducer {
                    reader: self.step_ref(reader.step),
                    started: true,
                    input: self.nodes[reader.step].reported_inputs[reader.input]
                        .path()
                        .to_string(),
                    writer: batch.step_ref(output_ref.step),
                    output: output.path().to_string(),
                    producer: producer_ref.clone(),
                })));
            }
        }

        // The producers of the inputs declared now, among all outputs.
        let mut producers: Vec<(InputRef, usize, bool)> = Vec::new();
        // Inputs declared now that cover an output another step declared
        // earlier, with that step: the reader has to wait for it, so that it
        // gets the same producers whichever of the two registers first.
        let mut foreign_reads: Vec<(InputRef, OutputRef, usize)> = Vec::new();
        let mut covered = Vec::new();
        let declared_inputs = new_steps
            .iter()
            .enumerate()
            .map(|(offset, step)| (base + offset, 0, &step.inputs))
            .chain(additions.iter().map(|step_additions| {
                (
                    step_additions.step,
                    self.nodes[step_additions.step].inputs.len(),
                    &step_additions.inputs,
                )
            }));
        for (step, known, inputs) in declared_inputs {
            for (position, input) in inputs.iter().enumerate() {
                covered.clear();
                let output = |output: OutputRef| batch.output_at(output.step, output.output);
                self.owners.cover(input, output, &mut covered);
                owners.cover(input, output, &mut covered);
                let input_at = known + position;
                for &(output, owner) in &covered {
                    let reader = InputRef {
                        step,
                        input: input_at,
                    };
                    let declarer = self.nodes.get(output.step).and_then(|node| {
                        node.output_declarers.get(output.output).copied().flatten()
                    });
                    if let Some(declarer) = declarer.filter(|&declarer| declarer != producer) {
                        foreign_reads.push((reader, output, declarer));
                    }
                    producers.push((reader, output.step, owner));
                    let edge = Edge {
                        from: output.step,
                        reason: EdgeReason::Artifact {
                            input: input_at,
                            output: output.output,
                        },
                    };
                    match step.checked_sub(base) {
                        Some(offset) => new_edges[offset].push(edge),
                        None => edges.push((step, edge)),
                    }
                }
            }
        }

        // The inputs declared before that cover the outputs declared now.
        // Their steps must not have been able to start yet.
        let mut readers = Vec::new();
        for &(output_ref, _) in &staged_outputs {
            let output = batch.output_at(output_ref.step, output_ref.output);
            readers.clear();
            self.readers.readers(
                output,
                |input| &self.nodes[input.step].inputs[input.input],
                &mut readers,
            );
            for &(reader, owner) in &readers {
                // A step whose own input covers an output an update adds to
                // it waits for itself, which is reported as a cycle below.
                if reader.step != output_ref.step {
                    let late = |started| {
                        StepGraphError::LateProducer(Box::new(LateProducer {
                            reader: self.step_ref(reader.step),
                            started,
                            input: self.nodes[reader.step].inputs[reader.input]
                                .path()
                                .to_string(),
                            writer: batch.step_ref(output_ref.step),
                            output: output.path().to_string(),
                            producer: producer_ref.clone(),
                        }))
                    };
                    if self.nodes[reader.step].state != State::Waiting {
                        return Err(late(true));
                    }
                    if !behind.get_or_insert_with(|| self.waiting_for(producer))[reader.step] {
                        return Err(late(false));
                    }
                }
                producers.push((reader, output_ref.step, owner));
                edges.push((
                    reader.step,
                    Edge {
                        from: output_ref.step,
                        reason: EdgeReason::Artifact {
                            input: reader.input,
                            output: output_ref.output,
                        },
                    },
                ));
            }
        }

        add_barrier_edges(
            new_steps.iter().map(|step| step.barrier),
            base,
            &mut new_edges,
        );
        for (step, mut step_edges) in new_steps.iter_mut().zip(new_edges) {
            dedup_edges(&mut step_edges);
            step.edges = step_edges;
        }
        // Stable, so the edges of a step keep their order.
        edges.sort_by_key(|(step, _)| *step);

        let expansion = Expansion {
            producer,
            revision: self.revision,
            base,
            steps: new_steps,
            additions,
            owners,
            edges,
            producers,
            pending,
        };
        // Registering nothing but the producer's success adds no dependency.
        let adds_dependencies =
            !(expansion.is_empty() && expansion.edges.is_empty() && expansion.pending.is_empty());
        if adds_dependencies {
            let view = View::expansion(self, &expansion);
            check_cycles(&view)?;
            let mut waited_for: HashMap<usize, Vec<bool>> = HashMap::new();
            for &(reader, output, declarer) in &foreign_reads {
                let prerequisites = waited_for
                    .entry(reader.step)
                    .or_insert_with(|| view.prerequisites(reader.step));
                if !prerequisites[declarer] {
                    return Err(StepGraphError::LateProducer(Box::new(LateProducer {
                        reader: view.step_ref(reader.step),
                        started: false,
                        input: view.input_at(reader.step, reader.input).path().to_string(),
                        writer: view.step_ref(output.step),
                        output: view
                            .output_at(output.step, output.output)
                            .path()
                            .to_string(),
                        producer: self.step_ref(declarer),
                    })));
                }
            }
        }
        Ok(expansion)
    }

    /// Records that the running step `index` succeeded, and registers what it
    /// declared: `expansion`, as prepared by [`Self::prepare`] for it, or
    /// nothing. The steps waiting for it, or for the steps it completes,
    /// become ready once nothing else holds them back.
    ///
    /// Fails, without changing anything, when the step is not running, when
    /// `expansion` was prepared for another step or before another step
    /// succeeded, and, without an expansion, when a reference waits for a
    /// step the step would have had to declare.
    pub fn succeed(
        &mut self,
        index: usize,
        expansion: Option<Expansion>,
    ) -> Result<(), StepGraphError> {
        let expansion = match expansion {
            Some(expansion) => expansion,
            None => self.prepare(index, StepManifest::default())?,
        };
        let running = self
            .nodes
            .get(index)
            .is_some_and(|node| node.state == State::Running);
        if expansion.producer != index || !running {
            return Err(StepGraphError::InvalidState {
                step: self.step_ref(expansion.producer),
                problem: "cannot register declarations prepared for another step or for a step that is not running",
            });
        }
        if expansion.revision != self.revision {
            return Err(StepGraphError::InvalidState {
                step: self.step_ref(index),
                problem: "cannot register declarations prepared before another step succeeded; prepare them again",
            });
        }
        self.commit(expansion);
        Ok(())
    }

    /// Checks the inputs step `index` reported to have read: every path has
    /// to be valid, and every output a reported input covers has to be
    /// written by the step itself or by a step it waits for. Retains the
    /// report to reject outputs declared after the read.
    pub fn check_reported_inputs(
        &mut self,
        index: usize,
        inputs: &[StepInput],
    ) -> Result<(), StepGraphError> {
        let step = self.step_ref(index);
        let reported = plan_inputs(&step, inputs, self.identity)?;
        if reported.is_empty() {
            return Ok(());
        }
        let prerequisites = View::committed(self).prerequisites(index);
        let mut covered = Vec::new();
        for input in &reported {
            covered.clear();
            self.owners.cover(
                input,
                |output| &self.nodes[output.step].outputs[output.output],
                &mut covered,
            );
            if let Some(&(output, _)) = covered
                .iter()
                .find(|(output, _)| output.step != index && !prerequisites[output.step])
            {
                return Err(StepGraphError::UndeclaredRead {
                    step,
                    input: input.path().to_string(),
                    writer: self.step_ref(output.step),
                    output: self.nodes[output.step].outputs[output.output]
                        .path()
                        .to_string(),
                });
            }
        }
        for input in reported {
            let position = self.nodes[index].reported_inputs.len();
            self.reported_readers.insert(
                InputRef {
                    step: index,
                    input: position,
                },
                &input,
            );
            self.nodes[index].reported_inputs.push(input);
        }
        self.revision += 1;
        Ok(())
    }

    /// Describes why the steps that have not run cannot start, when nothing
    /// runs and no step is ready.
    pub fn stalled_error(&self) -> StepGraphError {
        let mut details = Vec::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.state != State::Waiting {
                continue;
            }
            let step = self.step_ref(index);
            let detail =
                if let Some(pending) = self.pending.iter().find(|pending| pending.step == index) {
                    format!(
                        "  {step} waits for {} to declare `{}`, named in `{}`",
                        self.step_ref(pending.declarer),
                        pending.reference,
                        pending.field.as_str()
                    )
                } else if let Some(edge) = node
                    .edges
                    .iter()
                    .find(|edge| !self.satisfied(Vertex::of(edge.from, edge.reason.wait())))
                {
                    let waited = self.step_ref(edge.from);
                    let state = match self.nodes[edge.from].state {
                        State::Failed => ", which failed",
                        State::Waiting | State::Running | State::Succeeded => "",
                    };
                    match edge.reason.wait() {
                        Wait::Done => format!("  {step} waits for {waited}{state}"),
                        Wait::Complete => {
                            format!("  {step} waits for {waited}{state} and every step it declares")
                        }
                    }
                } else {
                    format!("  {step} can start but was not started")
                };
            details.push(detail);
        }
        StepGraphError::Stalled {
            count: details.len(),
            details: details.join("\n"),
        }
    }

    /// All steps known, in an order that runs every step after the steps it
    /// waits for, after the step that declared it, and after the steps that
    /// may declare a step it names. Among the steps that can come next, the
    /// one with the lowest position comes first.
    pub fn replay_order(&self) -> Vec<usize> {
        let vertices = 2 * self.nodes.len();
        let mut waiting = vec![0_usize; vertices];
        let mut waiters: Vec<Vec<usize>> = vec![Vec::new(); vertices];
        let mut wait = |vertex: Vertex, on: Vertex| {
            waiting[vertex.slot()] += 1;
            waiters[on.slot()].push(vertex.slot());
        };
        for (step, node) in self.nodes.iter().enumerate() {
            for edge in &node.edges {
                wait(
                    Vertex::Done(step),
                    Vertex::of(edge.from, edge.reason.wait()),
                );
            }
            if let Some(producer) = node.producer {
                wait(Vertex::Done(step), Vertex::Done(producer));
            }
            wait(Vertex::Complete(step), Vertex::Done(step));
            for &child in &node.children {
                wait(Vertex::Complete(step), Vertex::Complete(child));
            }
        }
        for pending in &self.pending {
            wait(Vertex::Done(pending.step), Vertex::Done(pending.declarer));
        }

        // Registration rejects cycles, so every vertex is ordered.
        let mut ready: BinaryHeap<Reverse<usize>> = (0..vertices)
            .filter(|&slot| waiting[slot] == 0)
            .map(Reverse)
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(Reverse(slot)) = ready.pop() {
            if slot % 2 == 0 {
                order.push(slot / 2);
            }
            for &waiter in &waiters[slot] {
                waiting[waiter] -= 1;
                if waiting[waiter] == 0 {
                    ready.push(Reverse(waiter));
                }
            }
        }
        order
    }

    /// Whether the steps waiting for `vertex` can go on.
    fn satisfied(&self, vertex: Vertex) -> bool {
        match vertex {
            Vertex::Done(step) => self.nodes[step].state == State::Succeeded,
            Vertex::Complete(step) => self.nodes[step].complete,
        }
    }

    /// Makes step `step` wait for what `edge` names, unless it already waits
    /// for it.
    fn add_edge(&mut self, step: usize, edge: Edge) {
        let wait = edge.reason.wait();
        let known = self.nodes[step]
            .edges
            .iter()
            .any(|known| known.from == edge.from && known.reason.wait() == wait);
        if !known {
            self.push_edge(step, edge);
        }
    }

    /// Makes step `step` wait for what `edge` names, which it does not wait
    /// for yet.
    fn push_edge(&mut self, step: usize, edge: Edge) {
        let wait = edge.reason.wait();
        if !self.satisfied(Vertex::of(edge.from, wait)) {
            self.nodes[step].blockers += 1;
            let from = &mut self.nodes[edge.from];
            match wait {
                Wait::Done => from.done_waiters.push(step),
                Wait::Complete => from.complete_waiters.push(step),
            }
        }
        self.nodes[step].edges.push(edge);
    }

    /// Makes step `step` wait for what a reference in `field` resolved to.
    fn add_link(&mut self, step: usize, field: RefField, link: Link) {
        match link {
            Link::Step(from) => self.add_edge(
                step,
                Edge {
                    from,
                    reason: field.reason(),
                },
            ),
            Link::Pending {
                reference,
                declarer,
            } => {
                self.nodes[step].blockers += 1;
                self.pending.push(PendingRef {
                    step,
                    field,
                    reference,
                    declarer,
                });
            }
        }
    }

    /// Hands step `step` out as ready once nothing holds it back.
    fn release_if_ready(&mut self, step: usize) {
        let node = &mut self.nodes[step];
        if node.blockers == 0 && node.state == State::Waiting && !node.released {
            node.released = true;
            self.ready.push(step);
        }
    }

    /// Registers a validated expansion.
    fn commit(&mut self, expansion: Expansion) {
        let Expansion {
            producer,
            base,
            steps,
            additions,
            owners,
            edges,
            producers,
            pending,
            ..
        } = expansion;
        self.revision += 1;
        let mut changed = Vec::new();

        // The references waiting for the producer resolve to `edges` and
        // `pending`.
        let mut kept = Vec::with_capacity(self.pending.len());
        for reference in mem::take(&mut self.pending) {
            if reference.declarer == producer {
                self.nodes[reference.step].blockers -= 1;
                changed.push(reference.step);
            } else {
                kept.push(reference);
            }
        }
        self.pending = kept;

        let mut new_edges = Vec::with_capacity(steps.len());
        for (offset, step) in steps.into_iter().enumerate() {
            let index = base + offset;
            for (position, input) in step.inputs.iter().enumerate() {
                self.readers.insert(
                    InputRef {
                        step: index,
                        input: position,
                    },
                    input,
                );
            }
            self.ids.insert(step.id.clone(), index);
            self.nodes.push(Node::new(
                Some(step.id),
                Some(producer),
                Some(step.declaration),
                step.barrier,
                step.inputs,
                step.outputs,
            ));
            new_edges.push(step.edges);
        }
        self.owners.merge(owners);
        for step_additions in additions {
            let node = &mut self.nodes[step_additions.step];
            let known = node.inputs.len();
            for (position, input) in step_additions.inputs.iter().enumerate() {
                self.readers.insert(
                    InputRef {
                        step: step_additions.step,
                        input: known + position,
                    },
                    input,
                );
            }
            node.inputs.extend(step_additions.inputs);
            node.output_declarers
                .extend(step_additions.outputs.iter().map(|_| Some(producer)));
            node.outputs.extend(step_additions.outputs);
        }
        for (input, step, owner) in producers {
            self.nodes[input.step].inputs[input.input].add_producer(step, owner);
        }

        let children: Vec<usize> = (base..self.nodes.len()).collect();
        // Deduplicated when prepared.
        for (&index, step_edges) in children.iter().zip(new_edges) {
            for edge in step_edges {
                self.push_edge(index, edge);
            }
        }
        for (step, edge) in edges {
            self.add_edge(step, edge);
        }
        for reference in pending {
            self.nodes[reference.step].blockers += 1;
            self.pending.push(reference);
        }

        let node = &mut self.nodes[producer];
        node.state = State::Succeeded;
        node.open_children = children.len();
        let done_waiters = mem::take(&mut node.done_waiters);
        for waiter in done_waiters {
            self.nodes[waiter].blockers -= 1;
            changed.push(waiter);
        }
        if children.is_empty() {
            self.complete(producer, &mut changed);
        }
        changed.extend(children.iter().copied());
        self.nodes[producer].children = children;

        changed.sort_unstable();
        changed.dedup();
        for step in changed {
            self.release_if_ready(step);
        }
    }

    /// Marks step `step` complete, and so every step that declared it once
    /// all it declared is complete; adds the steps waiting for them to
    /// `changed`.
    fn complete(&mut self, step: usize, changed: &mut Vec<usize>) {
        let mut step = step;
        loop {
            let node = &mut self.nodes[step];
            node.complete = true;
            let waiters = mem::take(&mut node.complete_waiters);
            let producer = node.producer;
            for waiter in waiters {
                self.nodes[waiter].blockers -= 1;
                changed.push(waiter);
            }
            let Some(parent) = producer else {
                break;
            };
            let parent_node = &mut self.nodes[parent];
            parent_node.open_children -= 1;
            if parent_node.open_children > 0 {
                break;
            }
            step = parent;
        }
    }

    /// The steps that cannot start before step `step` is done, as they wait
    /// for it directly or through other steps; not `step` itself.
    fn waiting_for(&self, step: usize) -> Vec<bool> {
        let mut seen = vec![false; 2 * self.nodes.len()];
        let start = Vertex::Done(step);
        seen[start.slot()] = true;
        let mut queue = vec![start];
        while let Some(vertex) = queue.pop() {
            let mut visit = |vertex: Vertex| {
                if !mem::replace(&mut seen[vertex.slot()], true) {
                    queue.push(vertex);
                }
            };
            match vertex {
                Vertex::Done(done) => {
                    for &waiter in &self.nodes[done].done_waiters {
                        visit(Vertex::Done(waiter));
                    }
                    visit(Vertex::Complete(done));
                    for pending in self
                        .pending
                        .iter()
                        .filter(|pending| pending.declarer == done)
                    {
                        visit(Vertex::Done(pending.step));
                    }
                }
                Vertex::Complete(complete) => {
                    let node = &self.nodes[complete];
                    for &waiter in &node.complete_waiters {
                        visit(Vertex::Done(waiter));
                    }
                    if let Some(parent) = node.producer {
                        visit(Vertex::Complete(parent));
                    }
                }
            }
        }
        (0..self.nodes.len())
            .map(|index| index != step && seen[Vertex::Done(index).slot()])
            .collect()
    }

    /// The step with the id `id`, known or declared in `scope`.
    fn find(&self, id: &str, scope: &Scope<'_>) -> Option<usize> {
        self.ids.get(id).copied().or_else(|| {
            let local = id
                .strip_prefix(scope.prefix)?
                .strip_prefix(STEP_ID_SEPARATOR)?;
            scope.local.get(local).copied()
        })
    }

    /// Resolves `reference`, as written by `waiter` in `field` of a manifest
    /// or listed step in `scope`.
    fn link(
        &self,
        waiter: &StepRef,
        field: RefField,
        reference: &str,
        scope: &Scope<'_>,
    ) -> Result<Link, StepGraphError> {
        if let Some(&step) = scope.local.get(reference) {
            return Ok(Link::Step(step));
        }
        self.link_absolute(waiter, field, reference.to_string(), scope)
            .map_err(|error| match error {
                // Name the reference as written.
                StepGraphError::UnknownDependency { .. }
                | StepGraphError::UnknownDiscovery { .. } => {
                    field.unknown(waiter.clone(), reference.to_string())
                }
                error => error,
            })
    }

    /// Resolves the absolute id `reference`, named by `waiter` in `field`:
    /// to its step, to a pending reference when the step that would declare
    /// it has not registered its declarations yet, or to an error.
    fn link_absolute(
        &self,
        waiter: &StepRef,
        field: RefField,
        reference: String,
        scope: &Scope<'_>,
    ) -> Result<Link, StepGraphError> {
        if let Some(step) = self.find(&reference, scope) {
            return Ok(Link::Step(step));
        }
        // The step with the longest known prefix of the id is the one that
        // declares, or would have declared, the next step on the way to it.
        let declarer = reference
            .rmatch_indices(STEP_ID_SEPARATOR)
            .find_map(|(end, _)| self.find(&reference[..end], scope));
        let Some(declarer) = declarer else {
            return Err(field.unknown(waiter.clone(), reference));
        };
        let settled = scope.producer == Some(declarer)
            || self
                .nodes
                .get(declarer)
                .is_some_and(|node| node.state == State::Succeeded);
        if settled {
            Err(StepGraphError::UndeclaredReference {
                step: waiter.clone(),
                field: field.as_str(),
                reference,
                declarer: self.step_ref(declarer),
            })
        } else {
            Ok(Link::Pending {
                reference,
                declarer,
            })
        }
    }
}

/// The steps of a batch of declarations, for resolving references.
struct Scope<'s> {
    /// The step that declared the batch, which declares nothing more.
    producer: Option<usize>,
    /// The qualified id of the steps of the batch, without their own ids.
    prefix: &'s str,
    /// The position of every step of the batch, by its own id.
    local: HashMap<&'s str, usize>,
}

/// What a reference resolved to.
#[derive(Debug)]
enum Link {
    /// A known or newly declared step.
    Step(usize),
    /// A qualified id step `declarer` may still declare.
    Pending { reference: String, declarer: usize },
}

/// What a step declared, validated against the steps known when it was
/// prepared, to register with [`DynamicStepGraph::succeed`].
#[derive(Debug)]
pub struct Expansion {
    producer: usize,
    revision: u64,
    /// The position of the first declared step.
    base: usize,
    steps: Vec<NewStep>,
    additions: Vec<Additions>,
    /// The outputs declared now.
    owners: OutputIndex,
    /// Dependencies added to known steps, by step.
    edges: Vec<(usize, Edge)>,
    /// Producers found for inputs of known steps and for inputs declared
    /// now, with whether their output is the input's path or contains it.
    producers: Vec<(InputRef, usize, bool)>,
    /// References that still wait for a step to declare what they name.
    pending: Vec<PendingRef>,
}

/// A declared step.
#[derive(Debug)]
struct NewStep {
    /// The qualified id.
    id: String,
    declaration: GeneratedStep,
    barrier: bool,
    inputs: Vec<PlannedInput>,
    outputs: Vec<PlannedOutput>,
    edges: Vec<Edge>,
}

/// What an update adds to a known step.
#[derive(Debug)]
struct Additions {
    step: usize,
    inputs: Vec<PlannedInput>,
    outputs: Vec<PlannedOutput>,
}

impl Expansion {
    /// The step that declared the expansion.
    pub fn producer(&self) -> usize {
        self.producer
    }

    /// Whether the step declared no steps and no updates.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.additions.is_empty()
    }

    /// The declared steps, with the positions they get, their qualified ids
    /// and their declarations, in the order declared.
    pub fn steps(&self) -> impl ExactSizeIterator<Item = (usize, &str, &GeneratedStep)> {
        let base = self.base;
        self.steps
            .iter()
            .enumerate()
            .map(move |(offset, step)| (base + offset, step.id.as_str(), &step.declaration))
    }

    /// Every output the expansion declares, with the position of the step
    /// writing it: the outputs of the declared steps, then those updates add
    /// to known steps.
    pub fn added_outputs(&self) -> impl Iterator<Item = (usize, &PlannedOutput)> {
        let base = self.base;
        let declared = self
            .steps
            .iter()
            .enumerate()
            .flat_map(move |(offset, step)| {
                step.outputs
                    .iter()
                    .map(move |output| (base + offset, output))
            });
        let added = self.additions.iter().flat_map(|step_additions| {
            step_additions
                .outputs
                .iter()
                .map(move |output| (step_additions.step, output))
        });
        declared.chain(added)
    }

    /// Whether the expansion declares any path: a declared step that is not
    /// a barrier, or an update adding inputs or outputs.
    pub fn declares_paths(&self) -> bool {
        self.steps.iter().any(|step| !step.barrier)
            || self.additions.iter().any(|step_additions| {
                !(step_additions.inputs.is_empty() && step_additions.outputs.is_empty())
            })
    }
}

/// The graph as it would be with an expansion registered, for validation.
struct View<'v> {
    graph: &'v DynamicStepGraph,
    /// The step that declared the expansion, which is done with it.
    producer: Option<usize>,
    base: usize,
    steps: &'v [NewStep],
    additions: &'v [Additions],
    /// Dependencies added to known steps, sorted by step.
    edges: &'v [(usize, Edge)],
    /// References added.
    pending: &'v [PendingRef],
}

/// How a vertex waits for the next one on a path through the graph.
#[derive(Debug, Clone, Copy)]
enum Via<'v> {
    /// A dependency of the step.
    Edge(Edge),
    /// A reference to a step the other step may declare.
    Pending(&'v PendingRef),
    /// A step is complete only once it is done.
    Own,
    /// A step is complete only once a step it declared is.
    Child,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    Unvisited,
    OnPath,
    Finished,
}

impl<'v> View<'v> {
    /// The graph as it is.
    fn committed(graph: &'v DynamicStepGraph) -> Self {
        Self {
            graph,
            producer: None,
            base: graph.nodes.len(),
            steps: &[],
            additions: &[],
            edges: &[],
            pending: &[],
        }
    }

    /// The graph with `expansion` registered.
    fn expansion(graph: &'v DynamicStepGraph, expansion: &'v Expansion) -> Self {
        Self {
            graph,
            producer: Some(expansion.producer),
            base: expansion.base,
            steps: &expansion.steps,
            additions: &expansion.additions,
            edges: &expansion.edges,
            pending: &expansion.pending,
        }
    }

    fn len(&self) -> usize {
        self.base + self.steps.len()
    }

    fn new_step(&self, step: usize) -> Option<&'v NewStep> {
        step.checked_sub(self.base)
            .and_then(|offset| self.steps.get(offset))
    }

    fn step_ref(&self, step: usize) -> StepRef {
        match self.new_step(step) {
            Some(new_step) => StepRef {
                index: step,
                id: Some(new_step.id.clone()),
            },
            None => self.graph.step_ref(step),
        }
    }

    fn is_barrier(&self, step: usize) -> bool {
        self.new_step(step).map_or_else(
            || self.graph.nodes[step].barrier,
            |new_step| new_step.barrier,
        )
    }

    fn additions(&self, step: usize) -> Option<&'v Additions> {
        self.additions
            .iter()
            .find(|step_additions| step_additions.step == step)
    }

    /// Output `position` of step `step`, declared or added.
    fn output_at(&self, step: usize, position: usize) -> &'v PlannedOutput {
        if let Some(new_step) = self.new_step(step) {
            return &new_step.outputs[position];
        }
        let known = &self.graph.nodes[step].outputs;
        match known.get(position) {
            Some(output) => output,
            None => {
                let added = self
                    .additions(step)
                    .map_or(&[][..], |step_additions| step_additions.outputs.as_slice());
                &added[position - known.len()]
            }
        }
    }

    /// Input `position` of step `step`, declared or added.
    fn input_at(&self, step: usize, position: usize) -> &'v PlannedInput {
        if let Some(new_step) = self.new_step(step) {
            return &new_step.inputs[position];
        }
        let known = &self.graph.nodes[step].inputs;
        match known.get(position) {
            Some(input) => input,
            None => {
                let added = self
                    .additions(step)
                    .map_or(&[][..], |step_additions| step_additions.inputs.as_slice());
                &added[position - known.len()]
            }
        }
    }

    /// Whether `vertex` is reached with the expansion registered.
    fn satisfied(&self, vertex: Vertex) -> bool {
        let (step, done) = match vertex {
            Vertex::Done(step) => (step, true),
            Vertex::Complete(step) => (step, false),
        };
        if step >= self.base {
            return false;
        }
        let node = &self.graph.nodes[step];
        let producer = self.producer == Some(step);
        if done {
            producer || node.state == State::Succeeded
        } else {
            node.complete || (producer && self.steps.is_empty())
        }
    }

    /// The dependencies of step `step`, known and added.
    fn edges_of(&self, step: usize) -> impl Iterator<Item = &'v Edge> {
        let (own, added): (&'v [Edge], &'v [(usize, Edge)]) = match self.new_step(step) {
            Some(new_step) => (new_step.edges.as_slice(), &[][..]),
            None => {
                let start = self.edges.partition_point(|(waiter, _)| *waiter < step);
                let end = self.edges.partition_point(|(waiter, _)| *waiter <= step);
                (
                    self.graph.nodes[step].edges.as_slice(),
                    &self.edges[start..end],
                )
            }
        };
        own.iter().chain(added.iter().map(|(_, edge)| edge))
    }

    /// The references of step `step` still waiting for a step to declare
    /// what they name.
    fn pending_of(&self, step: usize) -> impl Iterator<Item = &'v PendingRef> {
        let resolved = self.producer;
        self.graph
            .pending
            .iter()
            .filter(move |pending| Some(pending.declarer) != resolved)
            .chain(self.pending)
            .filter(move |pending| pending.step == step)
    }

    /// The steps step `step` declared, known and new.
    fn children_of(&self, step: usize) -> impl Iterator<Item = usize> {
        let known: &'v [usize] = match self.new_step(step) {
            Some(_) => &[][..],
            None => self.graph.nodes[step].children.as_slice(),
        };
        let declared = if self.producer == Some(step) {
            self.base..self.len()
        } else {
            0..0
        };
        known.iter().copied().chain(declared)
    }

    /// The step that declared step `step`.
    fn producer_of(&self, step: usize) -> Option<usize> {
        match self.new_step(step) {
            Some(_) => self.producer,
            None => self.graph.nodes[step].producer,
        }
    }

    /// What `vertex` waits for that is not reached yet.
    fn successors(&self, vertex: Vertex) -> Vec<(Vertex, Via<'v>)> {
        let mut next = Vec::new();
        match vertex {
            Vertex::Done(step) => {
                for edge in self.edges_of(step) {
                    next.push((Vertex::of(edge.from, edge.reason.wait()), Via::Edge(*edge)));
                }
                for pending in self.pending_of(step) {
                    next.push((Vertex::Done(pending.declarer), Via::Pending(pending)));
                }
            }
            Vertex::Complete(step) => {
                next.push((Vertex::Done(step), Via::Own));
                for child in self.children_of(step) {
                    next.push((Vertex::Complete(child), Via::Child));
                }
            }
        }
        next.retain(|(target, _)| !self.satisfied(*target));
        next
    }

    /// The steps step `step` waits for, directly or through other steps,
    /// including the steps that declared it, but not `step` itself.
    fn prerequisites(&self, step: usize) -> Vec<bool> {
        let mut seen = vec![false; 2 * self.len()];
        let start = Vertex::Done(step);
        seen[start.slot()] = true;
        let mut queue = vec![start];
        let mut next = Vec::new();
        while let Some(vertex) = queue.pop() {
            next.clear();
            match vertex {
                Vertex::Done(done) => {
                    next.extend(
                        self.edges_of(done)
                            .map(|edge| Vertex::of(edge.from, edge.reason.wait())),
                    );
                    next.extend(
                        self.pending_of(done)
                            .map(|pending| Vertex::Done(pending.declarer)),
                    );
                    next.extend(self.producer_of(done).map(Vertex::Done));
                }
                Vertex::Complete(complete) => {
                    next.push(Vertex::Done(complete));
                    next.extend(self.children_of(complete).map(Vertex::Complete));
                }
            }
            for &vertex in &next {
                if !mem::replace(&mut seen[vertex.slot()], true) {
                    queue.push(vertex);
                }
            }
        }
        (0..self.len())
            .map(|index| index != step && seen[Vertex::Done(index).slot()])
            .collect()
    }

    /// Describes one step of a cycle, from `vertex` to `target`.
    fn describe(&self, vertex: Vertex, target: Vertex, via: Via<'_>) -> Option<String> {
        match (vertex, via) {
            (Vertex::Done(step), Via::Edge(edge)) => {
                let reason = edge.reason.describe(
                    self.is_barrier(step),
                    |input| self.input_at(step, input).path().to_string(),
                    |output| self.output_at(edge.from, output).path().to_string(),
                );
                let waited = self.step_ref(edge.from);
                Some(match target {
                    Vertex::Done(_) => {
                        format!("  {} waits for {waited}: {reason}", self.step_ref(step))
                    }
                    Vertex::Complete(_) => format!(
                        "  {} waits for {waited} and every step it declares: {reason}",
                        self.step_ref(step)
                    ),
                })
            }
            (Vertex::Done(step), Via::Pending(pending)) => Some(format!(
                "  {} waits for {} to declare `{}`: it is named in `{}`",
                self.step_ref(step),
                self.step_ref(pending.declarer),
                pending.reference,
                pending.field.as_str()
            )),
            (Vertex::Complete(step), Via::Child) => {
                let Vertex::Complete(child) = target else {
                    return None;
                };
                Some(format!(
                    "  {} is complete only once {}, which it declared, is",
                    self.step_ref(step),
                    self.step_ref(child)
                ))
            }
            (Vertex::Done(_), Via::Own | Via::Child)
            | (Vertex::Complete(_), Via::Own | Via::Edge(_) | Via::Pending(_)) => None,
        }
    }
}

/// A vertex on the search path, with what it waits for and how many of
/// those were followed.
type Frame<'v> = (Vertex, Vec<(Vertex, Via<'v>)>, usize);

/// Reports a cycle among the vertices of `view` that are not reached yet.
fn check_cycles(view: &View<'_>) -> Result<(), StepGraphError> {
    let mut color = vec![Color::Unvisited; 2 * view.len()];
    for step in 0..view.len() {
        for root in [Vertex::Done(step), Vertex::Complete(step)] {
            if view.satisfied(root) || color[root.slot()] != Color::Unvisited {
                continue;
            }
            color[root.slot()] = Color::OnPath;
            let mut path: Vec<Frame<'_>> = vec![(root, view.successors(root), 0)];
            while let Some(frame) = path.last_mut() {
                let Some(&(target, _)) = frame.1.get(frame.2) else {
                    color[frame.0.slot()] = Color::Finished;
                    path.pop();
                    continue;
                };
                frame.2 += 1;
                match color[target.slot()] {
                    Color::Unvisited => {
                        color[target.slot()] = Color::OnPath;
                        path.push((target, view.successors(target), 0));
                    }
                    Color::OnPath => return Err(cycle_error(view, &path, target)),
                    Color::Finished => {}
                }
            }
        }
    }
    Ok(())
}

/// Describes the cycle that closes when the last vertex of `path` waits for
/// `target`, which is on `path`.
fn cycle_error(view: &View<'_>, path: &[Frame<'_>], target: Vertex) -> StepGraphError {
    let start = path
        .iter()
        .position(|(vertex, _, _)| *vertex == target)
        .unwrap_or_default();
    let chain = path[start..]
        .iter()
        .filter_map(|(vertex, next, followed)| {
            let &(to, via) = next.get(followed.checked_sub(1)?)?;
            view.describe(*vertex, to, via)
        })
        .collect::<Vec<_>>()
        .join("\n");
    StepGraphError::Cycle { chain }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indexmap::IndexMap;
    use rattler_conda_types::Platform;

    use super::DynamicStepGraph;
    use crate::execution_context::PrefixLayout;
    use crate::step_graph::StepGraphError;
    use crate::step_manifest::{GeneratedRun, GeneratedStep, StepManifest, StepUpdate};
    use crate::step_model::{
        GraphStep, StepInput, StepInputKind, StepOutput, StepOutputKind, StepRoot,
    };

    fn file(path: &str) -> StepInput {
        StepInput {
            root: StepRoot::Work,
            path: PathBuf::from(path),
            kind: StepInputKind::File,
        }
    }

    fn glob(path: &str) -> StepInput {
        StepInput {
            kind: StepInputKind::Glob,
            ..file(path)
        }
    }

    fn out_file(path: &str) -> StepOutput {
        StepOutput {
            root: StepRoot::Work,
            path: PathBuf::from(path),
            kind: StepOutputKind::File,
        }
    }

    fn out_tree(path: &str) -> StepOutput {
        StepOutput {
            kind: StepOutputKind::Tree,
            ..out_file(path)
        }
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    fn listed(id: &str, inputs: Vec<StepInput>, outputs: Vec<StepOutput>) -> GraphStep {
        GraphStep {
            id: Some(id.to_string()),
            inputs: Some(inputs),
            outputs: Some(outputs),
            depends_on: Vec::new(),
            discover_after: Vec::new(),
        }
    }

    fn step(id: &str, inputs: Vec<StepInput>, outputs: Vec<StepOutput>) -> GeneratedStep {
        GeneratedStep {
            id: id.to_string(),
            run: GeneratedRun::Script(format!("echo {id}")),
            interpreter: None,
            cwd: None,
            env: IndexMap::new(),
            inputs: Some(inputs),
            outputs: Some(outputs),
            depends_on: Vec::new(),
            discover_after: Vec::new(),
        }
    }

    fn manifest(steps: Vec<GeneratedStep>) -> StepManifest {
        StepManifest {
            steps,
            updates: Vec::new(),
        }
    }

    fn plan(steps: &[GraphStep]) -> DynamicStepGraph {
        DynamicStepGraph::new(steps, Platform::Linux64, PrefixLayout::Separate).unwrap()
    }

    fn name(graph: &DynamicStepGraph, step: usize) -> String {
        graph
            .qualified_id(step)
            .map_or_else(|| format!("@{step}"), ToString::to_string)
    }

    fn index(graph: &DynamicStepGraph, id: &str) -> usize {
        (0..graph.len())
            .find(|&step| graph.qualified_id(step) == Some(id))
            .unwrap()
    }

    /// Starts the steps that became ready and returns their ids.
    fn start_ready(graph: &mut DynamicStepGraph) -> Vec<String> {
        let ready = graph.take_ready();
        for &step in &ready {
            graph.start(step);
        }
        ready.iter().map(|&step| name(graph, step)).collect()
    }

    /// Registers that the running step `id` succeeded and declared `declared`.
    fn succeed(
        graph: &mut DynamicStepGraph,
        id: &str,
        declared: StepManifest,
    ) -> Result<(), StepGraphError> {
        let step = index(graph, id);
        let expansion = graph.prepare(step, declared)?;
        graph.succeed(step, Some(expansion))
    }

    fn done(graph: &mut DynamicStepGraph, id: &str) {
        succeed(graph, id, StepManifest::default()).unwrap();
    }

    fn prepare_error(graph: &DynamicStepGraph, id: &str, declared: StepManifest) -> String {
        graph
            .prepare(index(graph, id), declared)
            .unwrap_err()
            .to_string()
    }

    fn source_inputs(graph: &DynamicStepGraph, id: &str) -> Vec<String> {
        graph
            .source_inputs(index(graph, id))
            .map(ToString::to_string)
            .collect()
    }

    /// `g` declares `a` and `c` and `h` declares `b`, and their files flow
    /// `g/a -> h/b -> g/c`. Neither generator waits for everything the other
    /// declares, so that order works whichever registers first.
    #[test]
    fn generators_interleave_their_declared_steps() {
        let a = || step("a", vec![], vec![out_file("a.txt")]);
        let c = || GeneratedStep {
            depends_on: strings(&["h/b"]),
            ..step("c", vec![file("b.txt")], vec![out_file("c.txt")])
        };
        let b = || step("b", vec![file("a.txt")], vec![out_file("b.txt")]);
        let report = GraphStep {
            depends_on: strings(&["g", "h"]),
            ..listed("report", vec![file("c.txt")], vec![])
        };

        // `h` waits for what `g` declares, and `g/c` for `h/b`, which `h`
        // has yet to declare.
        let mut graph = plan(&[
            listed("g", vec![], vec![]),
            GraphStep {
                discover_after: strings(&["g"]),
                ..listed("h", vec![], vec![])
            },
            report.clone(),
        ]);
        assert_eq!(start_ready(&mut graph), ["g"]);
        succeed(&mut graph, "g", manifest(vec![a(), c()])).unwrap();
        assert_eq!(start_ready(&mut graph), ["h", "g/a"]);
        succeed(&mut graph, "h", manifest(vec![b()])).unwrap();
        assert_eq!(start_ready(&mut graph), Vec::<String>::new());
        done(&mut graph, "g/a");
        assert_eq!(start_ready(&mut graph), ["h/b"]);
        done(&mut graph, "h/b");
        assert_eq!(start_ready(&mut graph), ["g/c"]);
        done(&mut graph, "g/c");
        assert_eq!(start_ready(&mut graph), ["report"]);
        done(&mut graph, "report");
        assert!(graph.is_finished());
        let replay: Vec<String> = graph
            .replay_order()
            .into_iter()
            .map(|step| name(&graph, step))
            .collect();
        assert_eq!(replay, ["g", "h", "g/a", "h/b", "g/c", "report"]);

        // Registered the other way around, `h/b` waits for what `g`
        // declares, and `g/c` names the known `h/b`.
        let mut graph = plan(&[
            GraphStep {
                discover_after: strings(&["h"]),
                ..listed("g", vec![], vec![])
            },
            listed("h", vec![], vec![]),
            report,
        ]);
        assert_eq!(start_ready(&mut graph), ["h"]);
        let gated_b = GeneratedStep {
            discover_after: strings(&["g"]),
            ..b()
        };
        succeed(&mut graph, "h", manifest(vec![gated_b])).unwrap();
        assert_eq!(start_ready(&mut graph), ["g"]);
        succeed(&mut graph, "g", manifest(vec![a(), c()])).unwrap();
        assert_eq!(start_ready(&mut graph), ["g/a"]);
        done(&mut graph, "g/a");
        assert_eq!(start_ready(&mut graph), ["h/b"]);
        done(&mut graph, "h/b");
        assert_eq!(start_ready(&mut graph), ["g/c"]);
        done(&mut graph, "g/c");
        assert_eq!(start_ready(&mut graph), ["report"]);
    }

    /// The shape of a Ninja dyndep build: `discover_after` waits for the
    /// scanner's declarations and then for the declared producer of an
    /// input, while `depends_on` waits for everything the scanner declared.
    #[test]
    fn discover_after_waits_for_declarations_and_depends_on_for_all_declared() {
        let mut graph = plan(&[
            listed("scan", vec![], vec![out_file("out/archive.dd")]),
            GraphStep {
                discover_after: strings(&["scan"]),
                ..listed(
                    "install-members",
                    vec![file("out/archive.stamp"), glob("out/extract/**")],
                    vec![out_tree("share/files")],
                )
            },
            GraphStep {
                depends_on: strings(&["scan"]),
                ..listed(
                    "install-report",
                    vec![file("out/summary.txt")],
                    vec![out_file("share/summary.txt")],
                )
            },
        ]);
        // Before `scan` declares its steps nothing produces the stamp, but
        // `install-members` waits for `scan` rather than for the file.
        assert_eq!(
            source_inputs(&graph, "install-members"),
            ["work:out/archive.stamp"]
        );
        assert_eq!(start_ready(&mut graph), ["scan"]);
        let declared = manifest(vec![
            step(
                "untar",
                vec![file("out/archive.dd")],
                vec![
                    out_file("out/archive.stamp"),
                    out_tree("out/extract/sample"),
                ],
            ),
            step(
                "digest",
                vec![glob("out/extract/**")],
                vec![out_file("out/SHA256SUMS")],
            ),
            step(
                "summarize",
                vec![file("out/SHA256SUMS")],
                vec![out_file("out/summary.txt")],
            ),
        ]);
        succeed(&mut graph, "scan", declared).unwrap();
        assert_eq!(start_ready(&mut graph), ["scan/untar"]);
        assert_eq!(
            source_inputs(&graph, "install-members"),
            Vec::<String>::new()
        );
        let members = index(&graph, "install-members");
        assert_eq!(
            graph.inputs(members)[0].producers(),
            [index(&graph, "scan/untar")]
        );

        done(&mut graph, "scan/untar");
        assert_eq!(start_ready(&mut graph), ["install-members", "scan/digest"]);
        done(&mut graph, "install-members");
        done(&mut graph, "scan/digest");
        assert_eq!(start_ready(&mut graph), ["scan/summarize"]);
        done(&mut graph, "scan/summarize");
        assert_eq!(start_ready(&mut graph), ["install-report"]);
    }

    /// Like Ninja's dyndep for Fortran modules: the scanner adds the module
    /// a compiler writes to its outputs and to the inputs of the compiler
    /// that uses it, before either starts.
    #[test]
    fn updates_add_dependencies_before_their_steps_start() {
        let mut graph = plan(&[
            listed("scan", vec![], vec![out_file("deps.dd")]),
            GraphStep {
                discover_after: strings(&["scan"]),
                ..listed(
                    "compile-main",
                    vec![file("main.f90")],
                    vec![out_file("main.o")],
                )
            },
            GraphStep {
                discover_after: strings(&["scan"]),
                ..listed(
                    "compile-geometry",
                    vec![file("geometry.f90")],
                    vec![out_file("geometry.o")],
                )
            },
        ]);
        assert_eq!(start_ready(&mut graph), ["scan"]);
        let update = |step: &str, inputs, outputs| StepUpdate {
            step: step.to_string(),
            inputs,
            outputs,
            depends_on: Vec::new(),
        };
        let declared = StepManifest {
            steps: Vec::new(),
            updates: vec![
                update("compile-main", vec![file("geometry.mod")], vec![]),
                update("compile-geometry", vec![], vec![out_file("geometry.mod")]),
            ],
        };
        succeed(&mut graph, "scan", declared).unwrap();

        assert_eq!(start_ready(&mut graph), ["compile-geometry"]);
        let main = index(&graph, "compile-main");
        assert_eq!(
            graph.inputs(main)[1].producers(),
            [index(&graph, "compile-geometry")]
        );
        assert_eq!(source_inputs(&graph, "compile-main"), ["work:main.f90"]);
        assert_eq!(
            graph.outputs(index(&graph, "compile-geometry"))[1]
                .path()
                .to_string(),
            "work:geometry.mod"
        );
        done(&mut graph, "compile-geometry");
        assert_eq!(start_ready(&mut graph), ["compile-main"]);
    }

    /// `depends_on` waits for the steps declared by declared steps too, and
    /// barriers order only the steps of their own list or manifest.
    #[test]
    fn expansion_is_recursive_and_barriers_are_scoped() {
        let mut graph = plan(&[
            listed("g", vec![], vec![]),
            listed("h", vec![], vec![]),
            GraphStep {
                discover_after: strings(&["g"]),
                ..listed("peek", vec![], vec![])
            },
            GraphStep {
                id: Some("barrier".to_string()),
                ..GraphStep::default()
            },
            listed("after", vec![], vec![]),
        ]);
        assert_eq!(start_ready(&mut graph), ["g", "h"]);
        let wall = GeneratedStep {
            inputs: None,
            outputs: None,
            ..step("wall", vec![], vec![])
        };
        let declared = manifest(vec![
            step("x", vec![], vec![]),
            wall,
            step("y", vec![], vec![]),
        ]);
        succeed(&mut graph, "g", declared).unwrap();
        // `g/wall` holds back `g/y` only.
        assert_eq!(start_ready(&mut graph), ["peek", "g/x"]);
        done(&mut graph, "peek");
        succeed(&mut graph, "h", manifest(vec![step("z", vec![], vec![])])).unwrap();
        assert_eq!(start_ready(&mut graph), ["h/z"]);
        done(&mut graph, "h/z");
        succeed(
            &mut graph,
            "g/x",
            manifest(vec![step("deep", vec![], vec![])]),
        )
        .unwrap();
        // The barrier among the steps of `g` waits for what `g/x` declared.
        assert_eq!(start_ready(&mut graph), ["g/x/deep"]);
        done(&mut graph, "g/x/deep");
        assert_eq!(start_ready(&mut graph), ["g/wall"]);
        done(&mut graph, "g/wall");
        assert_eq!(start_ready(&mut graph), ["g/y"]);
        done(&mut graph, "g/y");
        // The listed barrier waits for everything `g` and `h` declared.
        assert_eq!(start_ready(&mut graph), ["barrier"]);
        done(&mut graph, "barrier");
        assert_eq!(start_ready(&mut graph), ["after"]);
    }

    /// A listed step can name a step another listed step declares later,
    /// and waits for it; a first id no step has is rejected up front.
    #[test]
    fn listed_steps_wait_for_steps_declared_later() {
        let use_declared = |reference: &str| GraphStep {
            depends_on: strings(&[reference]),
            ..listed("use", vec![], vec![])
        };
        let steps = [listed("g", vec![], vec![]), use_declared("g/a")];
        let mut graph = plan(&steps);
        let replay: Vec<String> = graph
            .replay_order()
            .into_iter()
            .map(|step| name(&graph, step))
            .collect();
        assert_eq!(replay, ["g", "use"]);
        assert_eq!(start_ready(&mut graph), ["g"]);
        succeed(&mut graph, "g", manifest(vec![step("a", vec![], vec![])])).unwrap();
        assert_eq!(start_ready(&mut graph), ["g/a"]);
        done(&mut graph, "g/a");
        assert_eq!(start_ready(&mut graph), ["use"]);

        let unknown = [listed("g", vec![], vec![]), use_declared("nosuch/a")];
        assert_eq!(
            DynamicStepGraph::new(&unknown, Platform::Linux64, PrefixLayout::Separate)
                .unwrap_err()
                .to_string(),
            "step 1 (`use`) depends on `nosuch/a`, but no step has that id"
        );
    }

    #[test]
    fn references_resolve_once_their_declarer_registers_or_fail() {
        let mut graph = plan(&[listed("g", vec![], vec![]), listed("h", vec![], vec![])]);
        assert_eq!(start_ready(&mut graph), ["g", "h"]);

        let naming = |field: &str, reference: &str| {
            let mut step = step("c", vec![], vec![]);
            match field {
                "depends_on" => step.depends_on = strings(&[reference]),
                _ => step.discover_after = strings(&[reference]),
            }
            manifest(vec![step])
        };
        assert_eq!(
            prepare_error(&graph, "g", naming("depends_on", "nosuch/x")),
            "step `g/c` depends on `nosuch/x`, but no step has that id"
        );
        // `h` may still declare `h/b`.
        succeed(&mut graph, "g", naming("discover_after", "h/b")).unwrap();
        assert_eq!(start_ready(&mut graph), Vec::<String>::new());

        let h_declared =
            "step `g/c` names `h/b` in `discover_after`, but step 1 (`h`) declared no step `b`";
        assert_eq!(
            prepare_error(&graph, "h", manifest(vec![step("d", vec![], vec![])])),
            h_declared
        );
        let h = index(&graph, "h");
        assert_eq!(graph.succeed(h, None).unwrap_err().to_string(), h_declared);
        graph.fail(h);
        assert_eq!(
            graph.stalled_error().to_string(),
            "1 build step(s) can never start:
  step `g/c` waits for step 1 (`h`) to declare `h/b`, named in `discover_after`"
        );
    }

    #[test]
    fn qualified_references_name_global_steps_even_when_a_sibling_shares_their_prefix() {
        let mut graph = plan(&[listed("a", vec![], vec![]), listed("g", vec![], vec![])]);
        assert_eq!(start_ready(&mut graph), ["a", "g"]);

        let mut consumer = step("x", vec![], vec![]);
        consumer.depends_on = strings(&["a/b"]);
        succeed(
            &mut graph,
            "g",
            manifest(vec![step("a", vec![], vec![]), consumer]),
        )
        .unwrap();
        assert_eq!(start_ready(&mut graph), ["g/a"]);
        succeed(&mut graph, "a", manifest(vec![step("b", vec![], vec![])])).unwrap();
        assert_eq!(start_ready(&mut graph), ["a/b"]);
        done(&mut graph, "a/b");
        assert_eq!(start_ready(&mut graph), ["g/x"]);
        done(&mut graph, "g/a");
        done(&mut graph, "g/x");
        assert!(graph.is_finished());
    }

    #[test]
    fn producers_cannot_be_declared_for_steps_that_may_have_started() {
        let mut graph = plan(&[
            listed("g", vec![], vec![]),
            listed("early", vec![file("gen.h")], vec![]),
            listed("unordered", vec![glob("gen/*.h")], vec![]),
            GraphStep {
                depends_on: strings(&["g"]),
                ..listed("final", vec![], vec![])
            },
        ]);
        // `unordered` is ready but has not started yet.
        assert_eq!(graph.take_ready(), [0, 1, 2]);
        graph.start(0);
        graph.start(1);

        let writes = |path: &str| manifest(vec![step("gen", vec![], vec![out_file(path)])]);
        assert_eq!(
            prepare_error(&graph, "g", writes("gen.h")),
            "step 0 (`g`) declares that step `g/gen` writes `work:gen.h`, but step 1 (`early`), whose input `work:gen.h` covers it, has already started; a step cannot get a producer for its input after it started"
        );
        assert_eq!(
            prepare_error(&graph, "g", writes("gen/x.h")),
            "step 0 (`g`) declares that step `g/gen` writes `work:gen/x.h`, which step 2 (`unordered`) reads through its input `work:gen/*.h`, but step 2 (`unordered`) does not wait for step 0 (`g`), so whether step 2 (`unordered`) waits for step `g/gen` would depend on the order the steps run in; make step 2 (`unordered`) wait for step 0 (`g`), for example through `discover_after`"
        );

        // The rejected declarations left nothing behind.
        assert_eq!(graph.len(), 4);
        assert_eq!(source_inputs(&graph, "early"), ["work:gen.h"]);
        graph.succeed(0, None).unwrap();
        assert_eq!(start_ready(&mut graph), ["final"]);
    }

    #[test]
    fn updates_reach_only_steps_that_wait_for_their_producer() {
        let mut graph = plan(&[
            listed("scan", vec![], vec![]),
            GraphStep {
                discover_after: strings(&["scan"]),
                ..listed("gated", vec![], vec![])
            },
            listed("free", vec![], vec![]),
            listed("started", vec![], vec![]),
            GraphStep {
                id: Some("wall".to_string()),
                ..GraphStep::default()
            },
        ]);
        assert_eq!(graph.take_ready(), [0, 2, 3]);
        graph.start(0);
        graph.start(3);

        let update = |target: &str| StepManifest {
            steps: Vec::new(),
            updates: vec![StepUpdate {
                step: target.to_string(),
                inputs: vec![file("x.h")],
                outputs: Vec::new(),
                depends_on: Vec::new(),
            }],
        };
        let cases = [
            (
                "started",
                "step 0 (`scan`) updates step 3 (`started`), which has already started; only a step that waits for step 0 (`scan`), for example through `discover_after`, can be updated",
            ),
            (
                "free",
                "step 0 (`scan`) updates step 2 (`free`), which does not wait for it and so could start before the update; make step 2 (`free`) wait for step 0 (`scan`), for example through `discover_after`",
            ),
            (
                "missing",
                "step 0 (`scan`) updates `missing`, but no step has that id",
            ),
            (
                "wall",
                "step 0 (`scan`) adds inputs or outputs to step 4 (`wall`), which declares neither and runs as a sequential barrier",
            ),
        ];
        for (target, expected) in cases {
            assert_eq!(prepare_error(&graph, "scan", update(target)), expected);
        }
        succeed(&mut graph, "scan", update("gated")).unwrap();
        let gated = index(&graph, "gated");
        assert_eq!(graph.inputs(gated)[0].path().to_string(), "work:x.h");
        assert_eq!(start_ready(&mut graph), ["gated"]);
    }

    #[test]
    fn declared_outputs_and_dependencies_are_validated_with_the_known_steps() {
        let mut graph = plan(&[
            listed("lib", vec![], vec![out_tree("lib")]),
            listed("g", vec![], vec![]),
            GraphStep {
                depends_on: strings(&["g"]),
                ..listed("h", vec![], vec![])
            },
        ]);
        assert_eq!(start_ready(&mut graph), ["lib", "g"]);

        let depending = |reference: &str| GeneratedStep {
            depends_on: strings(&[reference]),
            ..step("c", vec![], vec![])
        };
        let cases = [
            (
                manifest(vec![step("site", vec![], vec![out_file("lib/site.py")])]),
                "step 0 (`lib`) writes `work:lib` and step `g/site` writes `work:lib/site.py`; declared outputs must not be the same path or lie inside one another",
            ),
            (
                manifest(vec![
                    step("a", vec![], vec![out_file("x")]),
                    step("b", vec![], vec![out_file("x")]),
                ]),
                "step `g/a` writes `work:x` and step `g/b` writes `work:x`; declared outputs must not be the same path or lie inside one another",
            ),
            (
                manifest(vec![step("a", vec![file("../x")], vec![])]),
                "step `g/a` declares the invalid path `../x`: the path contains a `..` component; declare it without `..`",
            ),
            (
                manifest(vec![depending("g")]),
                "build steps form a dependency cycle:
  step 1 (`g`) is complete only once step `g/c`, which it declared, is
  step `g/c` waits for step 1 (`g`) and every step it declares: it is named in `depends_on`",
            ),
            // `h` waits for everything `g` declares, so `g/c` cannot wait
            // for a step `h` declares.
            (
                manifest(vec![depending("h/b")]),
                "build steps form a dependency cycle:
  step 1 (`g`) is complete only once step `g/c`, which it declared, is
  step `g/c` waits for step 2 (`h`) to declare `h/b`: it is named in `depends_on`
  step 2 (`h`) waits for step 1 (`g`) and every step it declares: it is named in `depends_on`",
            ),
        ];
        for (declared, expected) in cases {
            assert_eq!(prepare_error(&graph, "g", declared), expected);
        }
    }

    #[test]
    fn declarations_are_registered_against_the_graph_they_were_prepared_for() {
        let mut graph = plan(&[listed("a", vec![], vec![]), listed("b", vec![], vec![])]);
        assert_eq!(start_ready(&mut graph), ["a", "b"]);
        let a = index(&graph, "a");
        let expansion = graph
            .prepare(a, manifest(vec![step("x", vec![], vec![out_file("x")])]))
            .unwrap();
        assert_eq!(
            expansion
                .steps()
                .map(|(step, id, _)| (step, id))
                .collect::<Vec<_>>(),
            [(2, "a/x")]
        );
        done(&mut graph, "b");
        assert_eq!(
            graph.succeed(a, Some(expansion)).unwrap_err().to_string(),
            "step 0 (`a`) cannot register declarations prepared before another step succeeded; prepare them again"
        );
        succeed(
            &mut graph,
            "a",
            manifest(vec![step("x", vec![], vec![out_file("x")])]),
        )
        .unwrap();
        assert_eq!(start_ready(&mut graph), ["a/x"]);
    }

    #[test]
    fn reported_inputs_must_come_from_steps_waited_for() {
        let mut graph = plan(&[
            listed("gen", vec![], vec![out_file("gen.h")]),
            listed("other", vec![], vec![out_file("other.h")]),
            listed("cc", vec![file("gen.h")], vec![out_file("a.o")]),
        ]);
        assert_eq!(start_ready(&mut graph), ["gen", "other"]);
        done(&mut graph, "gen");
        assert_eq!(start_ready(&mut graph), ["cc"]);
        let cc = index(&graph, "cc");

        graph
            .check_reported_inputs(cc, &[file("gen.h"), file("a.o"), file("src/a.c")])
            .unwrap();
        assert_eq!(
            graph
                .check_reported_inputs(cc, &[glob("*.h")])
                .unwrap_err()
                .to_string(),
            "step 2 (`cc`) reports reading `work:*.h`, which covers `work:other.h` of step 1 (`other`), but step 2 (`cc`) does not wait for step 1 (`other`); declare the input, or make step 2 (`cc`) wait for step 1 (`other`)"
        );
        assert_eq!(
            graph
                .check_reported_inputs(cc, &[file("/usr/include/stdio.h")])
                .unwrap_err()
                .to_string(),
            "step 2 (`cc`) declares the invalid path `/usr/include/stdio.h`: the path is absolute; declare it relative to its root"
        );
    }
}
