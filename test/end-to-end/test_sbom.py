"""Tests for --sbom: SBOM generation and collection into info/sboms/."""

import json
from pathlib import Path

from helpers import RattlerBuild, get_extracted_package

# BOM-Link derived from serialNumber/version of test-data/recipes/sbom/vendored.cdx.json
VENDORED_BOM_LINK = "urn:cdx:b0dcbf7a-e1a2-4f43-8ac2-72d5a0389c15/1"


def test_sbom_generation(rattler_build: RattlerBuild, recipes: Path, tmp_path: Path):
    """--experimental --sbom packages the generated CycloneDX document and the
    files the build script dropped into $SBOM_DIR."""
    rattler_build.build(
        recipes / "sbom", tmp_path, extra_args=["--experimental", "--sbom"]
    )
    pkg = get_extracted_package(tmp_path, "sbom-test")

    assert (pkg / "sbom-payload.txt").exists()
    # the build script saw $SBOM_DIR
    assert (pkg / "sbom-dir-marker.txt").exists()

    assert (pkg / "info/sboms/vendored.cdx.json").exists()
    generated = pkg / "info/sboms/rattler-build.cdx.json"
    assert generated.exists()

    doc = json.loads(generated.read_text())
    assert doc["bomFormat"] == "CycloneDX"
    assert doc["specVersion"] == "1.5"

    primary = doc["metadata"]["component"]
    assert primary["name"] == "sbom-test"
    assert primary["version"] == "1.0.0"
    assert primary["purl"].startswith("pkg:conda/sbom-test@1.0.0?")

    components = {c["name"]: c for c in doc["components"]}
    # host environment package
    assert components["tzdata"]["scope"] == "required"
    # build-only environment package
    assert components["ca-certificates"]["scope"] == "excluded"

    # the collected file is referenced from the primary component via BOM-Link
    bom_refs = [r for r in primary["externalReferences"] if r["type"] == "bom"]
    assert [r["url"] for r in bom_refs] == [VENDORED_BOM_LINK]


def test_sbom_disabled_by_default(
    rattler_build: RattlerBuild, recipes: Path, tmp_path: Path
):
    """Without --sbom no info/sboms/ is packaged and $SBOM_DIR is not set."""
    rattler_build.build(recipes / "sbom", tmp_path)
    pkg = get_extracted_package(tmp_path, "sbom-test")

    assert (pkg / "sbom-payload.txt").exists()
    # the build script writes the marker only when $SBOM_DIR is set
    assert not (pkg / "sbom-dir-marker.txt").exists()
    assert not (pkg / "info/sboms").exists()


def test_sbom_requires_experimental(
    rattler_build: RattlerBuild, recipes: Path, tmp_path: Path
):
    """--sbom without --experimental fails early with a clear error."""
    result = rattler_build(
        "build",
        "--recipe",
        str(recipes / "sbom"),
        "--output-dir",
        str(tmp_path),
        "--sbom",
        need_result_object=True,
        capture_output=True,
    )
    assert result.returncode != 0
    assert "SBOM generation is an experimental feature" in result.stderr
    assert "--experimental" in result.stderr
