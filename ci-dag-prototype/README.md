# DAG-parallel CI prototype (GitHub Actions only)

A cheap, self-contained prototype of the "decentralized DAG build" design for
building many recipes in parallel on GitHub Actions — **no rattler-build
required**. The graph is a checked-in JSON file and each "build" is a `sleep`
plus a text file, so the prototype isolates exactly the things we don't
control and need to validate: GitHub's trigger semantics, race behavior,
cross-run artifact handoff, and provenance.

## Architecture recap

- **Plan** (`plan.yml`) — unprivileged, fork-PR-safe. Renders the graph
  (mocked here) and uploads it as artifact `graph-<sha>`.
- **Scheduler** (`scheduler.yml`) — privileged (`actions: write`), triggered
  by every Plan/Build Node completion via `workflow_run`. Never executes PR
  code. Does a global sweep: dispatches every node whose parents' package
  artifacts exist and whose own doesn't. Cron backstop revives stalled DAGs.
- **Build Node** (`build-node.yml`) — zero write permissions, no secrets.
  Idempotency gate (own artifact exists → exit 0), downloads verified
  ancestor artifacts into `channel/`, "builds", uploads `pkgs-<node>-<sha>`.

Completion marker = artifact existence. Namespace = head sha (re-runs are
memoized; a new push starts fresh). Provenance = the creating run's workflow
file + `display_title` (set by the trusted workflow file from dispatch
inputs; a run cannot rename itself), because artifact names are forgeable by
any job and dispatched runs all report main's `head_sha`.

## Setup (~5 minutes, ~$0)

1. Create a **public** scratch repo (public = free Actions minutes).
2. Copy this directory's contents into it, with `workflows/` becoming
   `.github/workflows/`:

   ```sh
   cp -r ci-dag-prototype/{graph.json,scripts} <scratch-repo>/
   mkdir -p <scratch-repo>/.github
   cp -r ci-dag-prototype/workflows <scratch-repo>/.github/workflows
   chmod +x <scratch-repo>/scripts/artifact.sh
   ```

3. Commit and push to `main` (workflow_run/schedule triggers only arm for
   workflows on the default branch).
4. Actions tab → **Plan** → *Run workflow*. Then watch: the Scheduler fires
   on Plan's completion, dispatches the four roots (`base`, `c1`, `pytool`),
   and the DAG unrolls from there. Expect **~15–25 min wall clock** for the
   full 14-node graph — dominated by dispatch→pickup latency per hop, which
   is itself one of the measurements.

## Findings from the live run (2026-07-14, baszalmstra/rattler-build fork)

The full 14-node DAG completed green. Key empirical results:

1. **The pure event loop does NOT work with `GITHUB_TOKEN`.** Runs created
   via a GITHUB_TOKEN `workflow_dispatch` complete without emitting
   `workflow_run` events (recursion guard) — wave 1 built, then the DAG went
   silent. Fixes: (a) loop inside the scheduler + self-dispatch continuation
   (implemented here; costs one runner for the DAG duration), (b) dispatch
   with a PAT/App token, which re-enables true event-driven operation, or
   (c) trusted-pushes-only repos can skip the scheduler and let each build
   job dispatch its children directly.
2. **Title-based provenance is mandatory, not optional.** Dispatched runs
   report the default branch's `head_sha`, which moved mid-DAG when the
   scheduler fix landed — sha-based artifact matching would have broken.
3. **Artifact-as-completion-marker beats run completion**: `eqapp` was
   dispatched 1s after `eq2`'s artifact appeared, before `eq2`'s run had
   even finished.
4. **Zero duplicate dispatches** (14 nodes, exactly 14 runs) — a single
   looping scheduler serializes dispatch decisions by construction; the
   idempotency gate remains as belt-and-braces.
5. **Memoization works**: the re-kicked scheduler skipped the 3 root nodes
   already built under the same namespace sha.
6. **Throughput**: 11 nodes / 4 dependency waves in 2m51s; per-wave latency
   ~35–40s (30s sweep interval + ~10s dispatch→pickup).

## Validation checklist

| # | Question | How to check | Pass looks like |
|---|----------|--------------|-----------------|
| 1 | **Does the loop self-perpetuate?** (workflow_run chain-depth limits vs. dispatch resetting the chain) | The `c1→c2→c3→c4→c5` chain needs 5+ scheduler hops | `c5` and then `finalize` complete without manual kicks |
| 2 | **Simultaneous-parents race** | `eq1`/`eq2` have equal durations; check how many `build eqapp @ <sha>` runs exist | ≥1 run; extras exited "already built" via the gate; exactly one uploaded the artifact |
| 3 | **Staggered-parents race** | `libb` (5s) finishes long before `liba` (40s) | `app` dispatched only after `liba`; no premature dispatch |
| 4 | **Cross-run artifact handoff** | Download `pkgs-finalize-<sha>`; it lists sha256s of its inputs | Hashes of all ancestor package files present |
| 5 | **Provenance via display_title** | Scheduler/build logs show verified finds; optionally upload a decoy artifact named `pkgs-app-<sha>` from a manual run of another workflow | Decoy is ignored; only Build-Node-produced artifacts accepted |
| 6 | **Scheduler coalescing is lossless** | Chain + diamonds cause bursts of completions; some scheduler runs get queued/replaced under the `scheduler` concurrency group | DAG still completes (global sweep re-derives dropped work) |
| 7 | **Cron backstop recovers a stall** | Cancel a queued Build Node run mid-flight; wait for the `*/15` cron | Sweep re-dispatches the node; DAG completes |
| 8 | **Resume/memoization** | After a full run, re-run Plan on the same sha | Every node no-ops through the gate in seconds |
| 9 | **Fork-PR flow** (optional, needs a second account) | Open a PR from a fork touching `graph.json` | Plan runs unprivileged on the PR; Scheduler dispatches; builds run with read-only tokens |
| 10 | **API budget** | Settings → check rate limit headers in a scheduler log (`gh api rate_limit`) | Full DAG stays well under 1000 req/h |

## Deliberate design points worth reading in the source

- `scheduler.yml`: event payload fields (`display_title` = PR title for Plan
  runs = attacker-controlled) go through `env:`, never into script text —
  script-injection hardening a privileged workflow needs.
- `build-node.yml`: inputs validated against strict regexes; consumed via
  `env`. Node ids come from graph content, which fork PRs influence.
- `artifact.sh`: the provenance rules, and why name-only lookup is unsafe.

## Mapping to the real system

| Prototype | Real system |
|---|---|
| `graph.json` checked in | `rattler-build graph --recipe-dir …` in Plan |
| `sleep` + text file | `rattler-build build --recipe … --ignore-recipe-variants --variant …` |
| `channel/` of text files | rattler-build `output_dir` (auto-indexed local channel) |
| `finalize` mock node | aggregate + `rattler-build upload` (gated on push to main) |
| — | per-node check runs from the Scheduler for PR visibility |

## Cleanup

Delete the scratch repo, or at least remove the `schedule:` trigger — the
cron sweep keeps running (GitHub auto-disables it after 60 days of repo
inactivity, but don't wait for that).
