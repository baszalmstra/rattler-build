# Experimental features

!!! warning
    These are experimental features of Rattler-Build and may change or go away completely.


Currently only the `build` and `rebuild` commands support the following experimental features.

To enable them, use the `--experimental` flag with the command.
Or, use the environment variable, `RATTLER_BUILD_EXPERIMENTAL=true`.

## Sigstore source attestation

The `attestation` field on URL sources allows verifying that downloaded source archives were produced by a trusted publisher using [Sigstore](https://sigstore.dev) attestations. This is supported for PyPI packages (where the bundle URL is automatically derived) and GitHub releases (where you specify the `bundle_url` manually).

```yaml
source:
  url: https://files.pythonhosted.org/packages/.../flask-3.1.1.tar.gz
  sha256: "6489f1..."
  attestation:
    publishers:
      - github:pallets/flask
```

See the [Sigstore source attestation documentation](sigstore.md#source-attestation-verification) for more details.

## SBOM generation

The `--sbom` flag (or the environment variable `RATTLER_BUILD_SBOM=true`) embeds SBOM
(Software Bill of Materials) documents into every package that is built:

```bash
rattler-build build --experimental --sbom --recipe ./recipe.yaml
```

The resulting packages contain an `info/sboms/` directory, mirroring the `.dist-info/sboms/`
directory that [PEP 770](https://peps.python.org/pep-0770/) standardizes for Python wheels
(bringing the same layout to conda packages is discussed in
[conda/ceps#127](https://github.com/conda/ceps/issues/127)). The directory holds:

1. `rattler-build.cdx.json`: a [CycloneDX](https://cyclonedx.org) 1.5 JSON document generated
   by rattler-build itself.
2. Any files the build script placed into `$SBOM_DIR` (see below).

Files under `info/` are metadata, so the SBOM documents do not show up in the package's
`paths.json` contents.

### The generated document

`rattler-build.cdx.json` describes:

- The package itself as the primary component: its `pkg:conda/...` purl, summary, license,
  homepage and repository, plus the rendered run dependency specs as `rattler-build:depends`
  and `rattler-build:constrains` properties.
- One component per package in the resolved host environment (scope `required`) and in the
  resolved build environment (scope `excluded`), each with a `pkg:conda` purl (including the
  channel when it can be derived), MD5 and SHA-256 hashes, license, the download URL as a
  `distribution` reference, and a `rattler-build:environment` property (`host` and/or
  `build`). Packages from a resolved [staging cache](multiple_output_cache.md) environment are
  recorded the same way, with `rattler-build:environment` set to `cache-host` or `cache-build`.
- A dependency graph: the package depends on the records matching its direct host and build
  specs, and every environment package depends on its own resolved dependencies within the
  same environment.
- One external reference of type `bom` per collected `$SBOM_DIR` file, so all SBOM documents
  shipped in the package are discoverable from the generated one.

The document is deterministic: its `serialNumber` is derived from the package identity and
the build timestamp, so rebuilding the same package produces an identical SBOM.

### `$SBOM_DIR`

When SBOM generation is enabled, the build script runs with `SBOM_DIR` pointing at an empty
writable directory. After the build, every regular file in that directory is copied into
`info/sboms/` with its name preserved. Subdirectories are not collected, and the name
`rattler-build.cdx.json` is reserved: a file with that name fails the build.

Collected files are treated as opaque, self-describing documents in the spirit of PEP 770:
they are shipped as-is, never merged or rewritten. If a `*.json` file identifies itself as a
CycloneDX document (a top-level `serialNumber` and `version`), the generated document
references it via a BOM-Link (`urn:cdx:<serial>/<version>`); any other file is referenced via
a relative IRI (`./<filename>`). Note that some tools omit the serial number when
`SOURCE_DATE_EPOCH` is set, which rattler-build always does; such documents get the relative
reference.

`SBOM_DIR` is only set when `--sbom` is active, so recipes that should also build without the
flag can guard the extra tooling:

```bash
if [ -n "${SBOM_DIR:-}" ]; then
  # generate SBOM documents here
fi
```

In multi-output recipes every output has its own `SBOM_DIR`. The build scripts of
[staging outputs](multiple_output_cache.md) do not receive `SBOM_DIR`; only files dropped by
the package output's own build script are collected.

### Build script examples

A Rust package using [`cargo-cyclonedx`](https://github.com/CycloneDX/cyclonedx-rust-cargo)
(available on conda-forge) to describe the crate dependency graph. The full example lives in
[`examples/cargo-sbom`](https://github.com/prefix-dev/rattler-build/tree/main/examples/cargo-sbom):

```yaml title="recipe.yaml"
build:
  script:
    # writes ${PKG_NAME}.cdx.json next to Cargo.toml
    - cargo cyclonedx --format json --spec-version 1.5
    - cp ${PKG_NAME}.cdx.json ${SBOM_DIR}/
    - cargo install --locked --bins --root ${PREFIX} --path .

requirements:
  build:
    - ${{ compiler('rust') }}
    - cargo-cyclonedx
```

A Go package using [`cyclonedx-gomod`](https://github.com/CycloneDX/cyclonedx-gomod), which is
not packaged on conda-forge and is therefore installed with `go install`:

```yaml title="recipe.yaml"
build:
  script:
    - go install github.com/CycloneDX/cyclonedx-gomod/cmd/cyclonedx-gomod@latest
    - $(go env GOPATH)/bin/cyclonedx-gomod app -json -output ${SBOM_DIR}/${PKG_NAME}.cdx.json .
    - go build -o ${PREFIX}/bin/${PKG_NAME} .

requirements:
  build:
    - go
```

## Jinja functions

### `load_from_file(<file_path>)`

The Jinja function `load_from_file` allows loading from files; specifically, it allows loading from `toml`, `json`,
and `yaml` file types to an object to allow it to fetch things directly from the file.
It loads all other files as strings.

#### Usage

`load_from_file` is useful when there is a project description in a well-defined project file such as `Cargo.toml`, `package.json`, `pyproject.toml`, `package.yaml`, or `stack.yaml`. It enables the recipe to be preserved in as simple a state as possible, especially when there is no need to keep the changes in sync; some example use cases for this are with CI/CD infrastructure or when there is a well-defined output format.

Below is an example loading a `Cargo.toml` inside of the `rattler-build` GitHub repository:

``` yaml title="recipe.yaml"
context:
  name: ${{ load_from_file("Cargo.toml").package.name }}
  version: ${{ load_from_file("Cargo.toml").package.version }}
  source_url: ${{ load_from_file("Cargo.toml").package.homepage }}
  rust_toolchain: ${{ load_from_file("rust-toolchains") }}

package:
  name: ${{ name }}
  version: ${{ version }}

source:
  git: ${{ source_url }}
  tag: ${{ source_tag }}

requirements:
  build:
    - rust ==${{ rust_toolchain }}

build:
  script: cargo build --release -p ${{ name }}

test:
  - script: cargo test -p ${{ name }}
  - script: cargo test -p rust-test -- --test-threads=1

about:
  home: ${{ source_url }}
  repository: ${{ source_url }}
  documentation: ${{ load_from_file("Cargo.toml").package.documentation }}
  summary: ${{ load_from_file("Cargo.toml").package.description }}
  license: ${{ load_from_file("Cargo.toml").package.license }}
```

### `git` functions

`git` functions are useful for getting the latest tag and commit hash.
These can be used in the `context` section of the recipe, to fetch version information
from a repository.

???+ example "Examples"
    ```python
    # latest tag in the repo
    git.latest_tag(<git_repo_url>)

    # latest tag revision(aka, hash of tag commit) in the repo
    git.latest_tag_rev(<git_repo_url>)

    # latest commit revision(aka, hash of head commit) in the repo
    git.head_rev(<git_repo_url>)

    # latest tag distance(aka, number of commits between latest tag and head) in the repo
    git.latest_tag_distance(<git_repo_url>)
    ```

#### Usage

These can be useful for automating minor things inside of the recipe itself, such as if the current version is the latest version or if the current hash is the latest hash, etc.

``` yaml title="recipe.yaml"
context:
  git_repo_url: "https://github.com/prefix-dev/rattler-build"
  latest_tag: ${{ git.latest_tag( git_repo_url ) }}

package:
  name: "rattler-build"
  version: ${{ latest_tag }}

source:
  git: ${{ git_repo_url }}
  tag: ${{ latest_tag }}
```

There is currently no guarantee of caching for repo fetches when using `git` functions. This may lead to some performance issues.
