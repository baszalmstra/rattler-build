from pathlib import Path

import pytest
from helpers import RattlerBuild, get_extracted_package


def test_build_steps(rattler_build: RattlerBuild, recipes: Path, tmp_path: Path):
    """`build.steps` run in order as independent processes.

    Every step starts from the captured activated environment: one writes via
    the build-time `$PREFIX`, one uses step-local `env`, one proves that env
    does not reach the next step, and one runs from a step-local `cwd`.
    """
    rattler_build.build(
        recipes / "build_steps", tmp_path, extra_args=["--experimental"]
    )
    pkg = get_extracted_package(tmp_path, "build_steps_test")

    step1 = pkg / "share" / "build_steps" / "step1.txt"
    step2 = pkg / "share" / "build_steps" / "step2.txt"
    step3 = pkg / "share" / "build_steps" / "step3.txt"
    cwd_pwd = pkg / "share" / "build_steps" / "cwd" / "pwd.txt"

    assert step1.exists(), "first step did not run"
    assert step2.exists(), "second step did not run"
    assert step3.exists(), "third step did not run"
    assert cwd_pwd.exists(), "cwd step did not run in its target directory"
    assert "hello-from-step" in step2.read_text(), (
        "step-local env did not reach the step"
    )
    assert "unset" in step3.read_text(), "step-local env leaked to a later step"


def test_default_build_script_still_runs(
    rattler_build: RattlerBuild, recipes: Path, tmp_path: Path
):
    """A legacy build.sh/build.bat is still discovered when no script is declared."""
    rattler_build.build(recipes / "default_build_script", tmp_path)
    pkg = get_extracted_package(tmp_path, "default_build_script_test")

    marker = pkg / "share" / "default_build_script" / "marker.txt"
    assert marker.exists(), "default build script did not run"
    assert "default-build-script" in marker.read_text()


def test_build_step_graph(rattler_build: RattlerBuild, recipes: Path, tmp_path: Path):
    """Declared `build.steps` run as a graph between undeclared barriers.

    Probe steps watch for steps that must not run yet: consumers wait for
    their producers despite stale outputs and differently cased inputs on
    Windows, `depends_on` orders steps without artifacts, tree outputs own
    their contents, globs wait for matching producers, barriers wait for the
    declared steps before them, and undeclared steps, selected per platform
    and named, stay sequential. Overlap of independent declared steps is
    covered by the native `step_execution` tests, which know the scheduler's
    parallelism.
    """
    rattler_build.build(
        recipes / "build_step_graph", tmp_path, extra_args=["--experimental"]
    )
    pkg = get_extracted_package(tmp_path, "build_step_graph_test")
    graph = pkg / "share" / "graph"

    def probe(name: str) -> list[str]:
        return (graph / "probes" / f"{name}.txt").read_text().split()

    assert probe("left") == ["left"]
    assert probe("right") == ["right"]
    assert probe("epoch") == ["left", "right"], "barrier started before its epoch"

    assert probe("producer") == ["alone"], "consumer started before its producer"
    assert (graph / "message.txt").read_text().split() == ["fresh"]

    assert probe("early") == ["alone"], "depends_on did not delay its step"
    assert probe("late") == ["seen"], "depends_on did not delay its step"

    assert probe("tree") == ["alone"], "tree consumer started before its producer"
    assert (graph / "tools.txt").read_text().split() == ["fresh"]

    assert probe("legacy-a") == ["alone"], "undeclared steps overlapped"


INVALID_STEP_GRAPHS = {
    "cycle": (
        """\
    - id: cycle-a
      inputs: [{root: work, path: a.txt}]
      outputs: [{root: work, path: b.txt}]
      run: echo a
    - id: cycle-b
      inputs: [{root: work, path: b.txt}]
      outputs: [{root: work, path: a.txt}]
      run: echo b
""",
        ["cycle-a", "cycle-b"],
    ),
    "unknown-dependency": (
        """\
    - id: orphan
      depends_on: [nowhere]
      inputs: []
      outputs: []
      run: echo orphan
""",
        ["orphan", "nowhere"],
    ),
    "file-inside-output-tree": (
        """\
    - id: tree-owner
      inputs: []
      outputs: [{root: host, path: share/generated, kind: tree}]
      run: echo tree
    - id: file-owner
      inputs: []
      outputs: [{root: host, path: share/generated/inner.txt}]
      run: echo file
""",
        ["tree-owner", "file-owner"],
    ),
    "duplicate-id": (
        """\
    - id: twin
      inputs: []
      outputs: []
      run: echo first
    - id: twin
      inputs: []
      outputs: []
      run: echo second
""",
        ["twin"],
    ),
    "partial-declaration": (
        """\
    - id: half-declared
      inputs: []
      run: echo half
""",
        ["half-declared"],
    ),
}


@pytest.mark.parametrize(
    "steps, named",
    list(INVALID_STEP_GRAPHS.values()),
    ids=list(INVALID_STEP_GRAPHS.keys()),
)
def test_invalid_build_step_graph_fails_before_any_step(
    rattler_build: RattlerBuild, tmp_path: Path, steps: str, named: list[str]
):
    """An invalid step graph fails the build, naming the steps involved,
    before any step runs, even an undeclared one listed first."""
    marker = tmp_path / "barrier-ran.txt"
    recipe_dir = tmp_path / "recipe"
    recipe_dir.mkdir()
    (recipe_dir / "recipe.yaml").write_text(
        f"""\
package:
  name: invalid-step-graph
  version: "1.0.0"

build:
  steps:
    - run: |
        echo ran > "{marker}"
{steps}"""
    )

    result = rattler_build(
        *rattler_build.build_args(
            recipe_dir, tmp_path / "output", extra_args=["--experimental"]
        ),
        capture_output=True,
        encoding="utf-8",
        errors="replace",
    )

    output = result.stdout + result.stderr
    assert result.returncode != 0, output
    for text in named:
        assert text in output, f"the error does not name {text}:\n{output}"
    assert not marker.exists(), "a step ran before the graph was validated"
