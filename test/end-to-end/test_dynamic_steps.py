"""Dynamic `build.steps`: steps declaring further steps through the files
named by `RATTLER_BUILD_STEP_MANIFEST` and `RATTLER_BUILD_STEP_INPUTS`.

These tests check the packages real builds produce. When steps start, how
generators interleave and how `conda_build.sh` / `conda_build.bat` replays
them is covered by the native `dynamic_steps` tests of rattler_build_script.
"""

import hashlib
import importlib.util
import json
import shutil
import sys
from pathlib import Path
from types import ModuleType

import pytest
import yaml
from helpers import RattlerBuild, get_extracted_package

EXAMPLES = Path(__file__).parent.parent.parent / "examples" / "dynamic-steps"


def archive_steps() -> ModuleType:
    """The ninja-archive example's own `archive_steps.py`."""
    path = EXAMPLES / "ninja-archive" / "archive_steps.py"
    spec = importlib.util.spec_from_file_location("ninja_archive_steps", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def assert_packages_sample(pkg: Path) -> None:
    """`pkg` holds every file of the sample archive byte for byte, with the
    checksums and statistics the generated steps computed from exactly
    those files."""
    share = pkg / "share" / "ninja-archive"
    sample = dict(archive_steps().sample_files())

    files = share / "files"
    installed = {
        path.relative_to(files).as_posix(): path.read_bytes()
        for path in files.rglob("*")
        if path.is_file()
    }
    assert installed == sample

    checksums = {}
    for line in (share / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
        digest, name = line.split("  ", 1)
        checksums[name] = digest
    assert checksums == {
        name: hashlib.sha256(data).hexdigest() for name, data in sample.items()
    }

    stats = json.loads((share / "stats.json").read_text(encoding="utf-8"))
    assert {entry["path"]: entry["bytes"] for entry in stats["files"]} == {
        name: len(data) for name, data in sample.items()
    }
    binary = [entry["path"] for entry in stats["files"] if not entry["text"]]
    assert binary == ["sample/data/bytes.bin"]

    summary = (share / "summary.txt").read_text(encoding="utf-8").splitlines()
    text_files = len(sample) - len(binary)
    assert f"files: {len(sample)} ({text_files} text, 1 binary)" in summary
    assert f"bytes: {sum(len(data) for data in sample.values())}" in summary


def test_ninja_archive_example(rattler_build: RattlerBuild, tmp_path: Path):
    """The Ninja dyndep example packages every file of its sample archive.

    No recipe or Ninja file lists the archive members: the `scan` step
    runs the Ninja scanner edge and declares the other Ninja edges as
    generated steps, with the members the scanner found as outputs of the
    extraction and inputs of the checksum and statistics steps. The package
    holds the members byte for byte, with checksums and statistics computed
    from exactly those members.
    """
    rattler_build.build(
        EXAMPLES / "ninja-archive", tmp_path, extra_args=["--experimental"]
    )
    assert_packages_sample(get_extracted_package(tmp_path, "ninja-archive-dyndep"))


def test_ninja_archive_with_shell_operators_in_its_name(
    rattler_build: RattlerBuild, tmp_path: Path
):
    """An archive named `a&b^c.tar` is unpacked like any other.

    Ninja passes `$in` to CreateProcess on Windows, where `&` and `^` are
    plain characters, while the generated steps run in cmd.exe (bash
    elsewhere), which would split the command at `&`. The package holds the
    members of that archive: the recipe only writes `sample.tar` itself, so
    here `scan/untar` and the steps after it can only have read the renamed
    archive.
    """
    recipe_dir = tmp_path / "recipe"
    shutil.copytree(
        EXAMPLES / "ninja-archive",
        recipe_dir,
        ignore=shutil.ignore_patterns("__pycache__"),
    )
    archive = "a&b^c.tar"
    assert archive_steps().main(["sample", str(recipe_dir / archive)]) == 0
    recipe_path = recipe_dir / "recipe.yaml"
    recipe = yaml.safe_load(recipe_path.read_text(encoding="utf-8"))
    recipe["context"]["archive"] = archive
    recipe_path.write_text(yaml.safe_dump(recipe, sort_keys=False), encoding="utf-8")

    output = tmp_path / "output"
    rattler_build.build(recipe_dir, output, extra_args=["--experimental"])
    assert_packages_sample(get_extracted_package(output, "ninja-archive-dyndep"))


CIRCLE_AREA = "pure function circle_area(radius) result(area)"
RING_AREA = "pure function ring_area(outer, inner) result(area)"


def test_dynamic_build_steps(
    rattler_build: RattlerBuild, recipes: Path, tmp_path: Path
):
    """A CMake-style Fortran build of `dynamic_steps` as dynamic steps.

    `configure` generates scan, compile, collate and link steps; `collate`
    adds the module files to the generated compile steps before they start
    and generates the module installation. Each compile step fails when a
    module it uses has not been compiled yet, so the objects show that the
    updates ordered the compilers. `scan-report` (`discover_after`) sees
    every scan result, and `summary` (`depends_on`) sees the modules that the
    generated `collate` step's own generated step installed.
    """
    rattler_build.build(
        recipes / "dynamic_steps", tmp_path, extra_args=["--experimental"]
    )
    pkg = get_extracted_package(tmp_path, "dynamic_steps_test")
    share = pkg / "share" / "dynamic-steps"

    modules = share / "modules"
    assert sorted(path.name for path in modules.iterdir()) == [
        "geometry.mod",
        "shapes.mod",
    ]
    assert (modules / "geometry.mod").read_text(encoding="utf-8") == CIRCLE_AREA + "\n"
    assert (modules / "shapes.mod").read_text(encoding="utf-8") == RING_AREA + "\n"

    objects = json.loads((share / "program.json").read_text(encoding="utf-8"))[
        "objects"
    ]
    assert {name: obj["source"] for name, obj in objects.items()} == {
        "geometry": "src/geometry.f90",
        "main": "src/main.f90",
        "shapes": "src/shapes.f90",
    }
    assert {name: obj["provides"] for name, obj in objects.items()} == {
        "geometry": ["geometry"],
        "main": [],
        "shapes": ["shapes"],
    }
    assert {name: obj["uses"] for name, obj in objects.items()} == {
        "geometry": {},
        "main": {"shapes": [RING_AREA]},
        "shapes": {"geometry": [CIRCLE_AREA]},
    }

    assert json.loads((share / "scan.json").read_text(encoding="utf-8")) == {
        "geometry": {"provides": ["geometry"], "requires": []},
        "main": {"provides": [], "requires": ["shapes"]},
        "shapes": {"provides": ["shapes"], "requires": ["geometry"]},
    }
    assert (share / "summary.txt").read_text(encoding="utf-8").splitlines() == [
        "objects: geometry main shapes",
        "modules: geometry.mod shapes.mod",
    ]


GENERATOR = """\
    - id: gen
      inputs: []
      outputs: []
      run:
        - if: unix
          then: |
            cat > "$RATTLER_BUILD_STEP_MANIFEST" <<'EOF'
            @MANIFEST@
            EOF
          else: |
            echo @MANIFEST@> "%RATTLER_BUILD_STEP_MANIFEST%"
"""

INVALID_DECLARATIONS = {
    "unsupported-version": (
        "",
        '{"version": 2, "steps": []}',
        ["gen", "unsupported version `2`"],
    ),
    "generated-output-of-a-recipe-step": (
        """\
    - id: owner
      inputs: []
      outputs: [{root: work, path: owned.txt}]
      run: echo owned > owned.txt
""",
        (
            '{"version": 1, "steps": [{"id": "dup", "run": "echo dup", "inputs": [],'
            ' "outputs": [{"root": "work", "path": "owned.txt"}]}]}'
        ),
        ["owner", "gen/dup", "owned.txt"],
    ),
    "update-of-a-step-not-waiting-for-the-generator": (
        """\
    - id: independent
      inputs: []
      outputs: []
      run: echo independent
""",
        (
            '{"version": 1, "updates": [{"step": "independent",'
            ' "inputs": [{"root": "work", "path": "late.txt"}]}]}'
        ),
        ["gen", "independent"],
    ),
}


@pytest.mark.parametrize(
    "before, manifest, named",
    list(INVALID_DECLARATIONS.values()),
    ids=list(INVALID_DECLARATIONS.keys()),
)
def test_invalid_declarations_fail_the_build(
    rattler_build: RattlerBuild,
    tmp_path: Path,
    before: str,
    manifest: str,
    named: list[str],
):
    """A manifest that is invalid or does not fit into the build fails it,
    naming the steps and paths involved. Neither a step waiting for the
    generator's registration nor any later step runs."""
    waiter = tmp_path / "waiter-ran.txt"
    barrier = tmp_path / "barrier-ran.txt"
    generator = GENERATOR.replace("@MANIFEST@", manifest)
    recipe_dir = tmp_path / "recipe"
    recipe_dir.mkdir()
    (recipe_dir / "recipe.yaml").write_text(
        f"""\
package:
  name: invalid-step-declarations
  version: "1.0.0"

build:
  steps:
{before}{generator}\
    - id: waiter
      discover_after: [gen]
      inputs: []
      outputs: []
      run: echo ran > "{waiter}"
    - run: echo ran > "{barrier}"
"""
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
    assert not waiter.exists(), "a step waiting for the generator ran"
    assert not barrier.exists(), "a step after the failure ran"


def test_discover_after_unknown_step_fails_before_any_step(
    rattler_build: RattlerBuild, tmp_path: Path
):
    """`discover_after` naming no step of the recipe fails the build before
    any step runs, even an undeclared one listed first."""
    marker = tmp_path / "barrier-ran.txt"
    recipe_dir = tmp_path / "recipe"
    recipe_dir.mkdir()
    (recipe_dir / "recipe.yaml").write_text(
        f"""\
package:
  name: unknown-discover-after
  version: "1.0.0"

build:
  steps:
    - run: echo ran > "{marker}"
    - id: waiter
      discover_after: [nowhere]
      inputs: []
      outputs: []
      run: echo waiting
"""
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
    for text in ["waiter", "nowhere", "discover_after"]:
        assert text in output, f"the error does not name {text}:\n{output}"
    assert not marker.exists(), "a step ran before the graph was validated"
