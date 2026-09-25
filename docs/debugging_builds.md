# Debugging Builds

This guide covers how to debug conda package builds with Rattler-Build when
things go wrong. It's designed for both humans and AI agents working with
recipes.

## Debugging Workflow

Suppose you have a recipe that fails to build:

```yaml title="recipe.yaml"
package:
  name: test
  version: "1.0"

build:
  script:
    - exit 1
```

Running `rattler-build build` will fail. When a build
fails, the build directory is automatically preserved so you can investigate.
There are multiple things you can do to investigate

### Enter the Debug Shell

Jump straight into the failed build environment:

```bash
rattler-build debug shell
```

This opens an interactive shell in the work directory with the build environment
loaded. All environment variables (`$PREFIX`, `$BUILD_PREFIX`, etc.) are set up
exactly as they were during the build.

Now you can modify files and run individual commands to isolate the issue:

```bash
./configure --prefix=$PREFIX
make VERBOSE=1
make install
```

### Re-run the Build Script

Use `debug run` to re-execute the build script with the full environment already
loaded:

```bash
# Re-run the build script
rattler-build debug run

# Re-run with shell tracing (bash -x) for verbose output
rattler-build debug run --trace
```

You can find the working directory by running the following:

```
rattler-build debug workdir
```

Modify files inside that directory and run `rattler-build debug run` to check whether that fixed the problem.

### Modify Dependencies

If you need additional packages in the host or build environment, you can add
them without re-running the full setup:

```bash
# Add packages to the host environment
rattler-build debug host-add libfoo libbar

# Add build tools
rattler-build debug build-add gdb valgrind
```

Remember to add them to your recipe.yaml once you found the right set of dependencies.


### Create a Patch for Fixes

After fixing issues in the source code:

```bash
# Create a patch from your changes
rattler-build debug create-patch \
  --directory . \
  --name my-fix \
  --exclude "*.o,*.so,*.pyc"

# Preview what would be included
rattler-build debug create-patch \
  --directory . \
  --name my-fix \
  --dry-run
```

To include new files:

```bash
rattler-build debug create-patch \
  --directory . \
  --name my-fix \
  --add "*.txt,src/new_file.c"
```

In the end, you will have a patch file that you can include in your recipe


### Update Recipe and Rebuild

Add the patch to your recipe:

```yaml
source:
  - url: https://example.com/source.tar.gz
    sha256: ...
    patches:
      # this needs to be manually added
      - my-fix.patch
```

Then rebuild:

```bash
rattler-build build --recipe recipe.yaml
```

### Debugging a Successful Build

If your recipe builds *successfully* but you still want to inspect the
environment, use `--keep-build` to prevent cleanup:

```bash
rattler-build build --recipe recipe.yaml --keep-build
rattler-build debug shell
```


### Setting Up a Debug Environment Without Building

If you want to prepare a debug environment without running the build script at
all, use `debug setup`. This resolves dependencies, downloads sources, and
creates the build script — but doesn't execute it:

```bash
rattler-build debug setup --recipe recipe.yaml
rattler-build debug shell
```

This is useful when you want to inspect or modify sources before running the
build for the first time.

## Inspecting and Extracting Packages

The `rattler-build package` subcommand provides utilities for inspecting and extracting built packages, which is useful for debugging package contents.


### Inspecting Packages

Use `package inspect` to view package metadata without extracting:

```bash
# Basic package information
rattler-build package inspect mypackage-1.0-h12345.conda

# Show all information including file listing
rattler-build package inspect mypackage-1.0-h12345.conda --all

# Show specific sections
rattler-build package inspect mypackage-1.0-h12345.conda --paths      # File listing with hashes
rattler-build package inspect mypackage-1.0-h12345.conda --about      # Extended about info
rattler-build package inspect mypackage-1.0-h12345.conda --run-exports # Run exports

# Output as JSON for scripting
rattler-build package inspect mypackage-1.0-h12345.conda --json
```

### Extracting Packages

Use `package extract` to extract a package to a directory for inspection:

```bash
# Extract to a directory named after the package
rattler-build package extract mypackage-1.0-h12345.conda

# Extract to a custom destination
rattler-build package extract mypackage-1.0-h12345.conda -d my-extracted

# Extract directly from a URL (supports authenticated channels)
rattler-build package extract https://conda.anaconda.org/conda-forge/linux-64/python-3.11.0-h12345.conda
```

After extraction, the command reports the SHA256/MD5 checksums and file size, which is useful for verifying package integrity.

Both `.conda` and `.tar.bz2` package formats are supported.

## Build Directory Structure

When Rattler-Build builds a package, it creates:

```txt
output/
└─ rattler-build-log.txt            # Append-only log of build directories (latest at bottom)
└─ bld/                             # Build directories
│   └─ rattler-build_<name>_<timestamp>/
│       └─ work/                    # Source code and working directory
│       │   └─ .source_info.json    # Source information (extracted folders, etc.)
│       │   └─ build_env.sh         # Environment setup script
│       │   └─ conda_build.sh       # Build script, or standalone replay for `build.steps`
│       │   └─ conda_build_steps/   # Wrapper and declaration files per step with `build.steps`
│       │   └─ conda_build.log      # Complete build output
│       └─ host_env_placehold_.../  # Host environment (runtime dependencies)
│       └─ build_env/               # Build environment (build-time dependencies)
└─ src_cache/                       # Downloaded and extracted sources
└─ build_cache/                     # Staging cache
│   └─ staging_<sha256>/            # Per-staging-output cache
│       └─ metadata.json            # Cache metadata (deps, sources, variant)
│       └─ prefix/                  # Cached prefix files from staging build
│       └─ work_dir/                # Cached work directory from staging build
└─ <platform>/                      # Built packages
```

### Builds using `build.steps`

With [`build.steps`](build_script.md#experimental-build-steps), the files
above work as for `build.script`, with these differences:

- `conda_build.sh` / `conda_build.bat` replays the whole build: it activates
  the environment once and then runs every step in its own process, like
  Rattler-Build does. `rattler-build debug run` therefore reruns all steps.
- The replay is serial. It runs one step at a time in a fixed order that
  respects the [step graph](reference/recipe_file.md#step-graph): every step
  runs after the steps it depends on, and undeclared steps stay barriers.
  Steps that ran in parallel during the build run one after another in the
  replay, so their output is not interleaved, and the order is the same on
  every replay. The replay stops at the first failing step. Unlike the build,
  it does not check that declared inputs and outputs exist.
- A failing step names its own script,
  `conda_build_steps/step_<index>/conda_build.sh` (or `.bat`), which runs
  only that step. It does not activate the environment, so run it with `bash`
  (or `cmd.exe /d /c`) from a shell in which the build environment is already
  active, such as `rattler-build debug shell` or one that has sourced
  `build_env.sh` (or called `build_env.bat`). `<index>` numbers steps in
  recipe order, which can differ from the replay order. Steps generated during
  the build are numbered after the recipe steps, in the order in which they
  were registered.

With `build.script`, `conda_build.sh` / `conda_build.bat` is still the single
build script, and `rattler-build debug run` runs it as before.

#### Generated steps

A step that declares further steps through `RATTLER_BUILD_STEP_MANIFEST` (see
[Generated steps](reference/recipe_file.md#generated-steps)) writes its
declarations to `conda_build_steps/step_<index>/steps.json`, and the paths it
read to `inputs.json` next to it. Both remain in the work directory while it
exists; use `--keep-build` to retain them after a successful package build.
Before a new build runs its steps, once the recipe steps have passed the
checks, Rattler-Build clears `conda_build_steps/`, so no directory of a step
generated by an earlier build remains. In addition, each step's declaration
files are removed right before that step starts.

After the steps ran, successfully or not, Rattler-Build rewrites
`conda_build.sh` / `conda_build.bat` so that it replays every registered step,
generated steps included, in the same serial order as above. The replay runs
the steps that were generated in the build; it does not register steps again.
Rattler-Build keeps a byte-for-byte copy of every manifest it registered as
`conda_build_steps/step_<index>/recorded_steps.json`, and the replay checks
that the generating steps still declare the same thing:

- Right after a step that declared steps in the build, the replay compares
  the `steps.json` the step just wrote with `recorded_steps.json`. If they
  differ, for example because a source file changed, the replay stops with
  exit code 1 and reports that the step declared different build steps than
  the build the script replays. No later step runs.
- If a step that wrote no manifest in the build now writes a non-empty
  `steps.json`, the replay stops: the new manifest was not recorded when the
  replay was written, even if it declares no further steps.

The replay does not go past a changed manifest using the old graph. It stops
right after the step whose declarations changed. Steps that ran before that
point, including that step itself, have already run and are not undone. To
run a changed graph, build again.

`rattler-build debug setup` writes `conda_build.sh` / `conda_build.bat` before
any step has run, so no generated steps are known yet. `rattler-build debug run`
then runs the recipe steps, and the first step that writes a non-empty step
manifest stops the replay with the error above. To debug a recipe with
generating steps, build it first (with `--keep-build` if the build succeeds),
then run `rattler-build debug run` to replay the script that build wrote, or
run the scripts of single steps as described above.

## Environment Variables Available in the Debug Shell

Inside the debug shell, you have access to:

| Variable                       | Description                                |
| ------------------------------ | ------------------------------------------ |
| `$PREFIX`                      | Host prefix (where packages get installed) |
| `$BUILD_PREFIX`                | Build prefix (tools for building)          |
| `$SRC_DIR`                     | Source directory (same as work directory)   |
| `$RATTLER_BUILD_DIRECTORIES`   | Full JSON with all directory info           |
| `$RATTLER_BUILD_RECIPE_PATH`   | Path to the recipe file                    |
| `$RATTLER_BUILD_RECIPE_DIR`    | Directory containing the recipe            |
| `$RATTLER_BUILD_BUILD_DIR`     | The build directory root                   |
| `$RATTLER_BUILD_HOST_PREFIX`   | Path to the host prefix                    |
| `$RATTLER_BUILD_BUILD_PREFIX`  | Path to the build prefix                   |

## Common Debugging Scenarios

### Compilation Failures

```bash
# Re-run the build script with tracing to see where it fails
rattler-build debug run --trace

# Or enter the shell and run specific build commands
rattler-build debug shell
make VERBOSE=1
cmake --build . --verbose
```

### Missing Files or Dependencies

```bash
# Check source information
cat .source_info.json | jq .

# List what's in the work directory
find . -type f | head -30

# Check what's in the environments
ls $PREFIX/lib/
ls $BUILD_PREFIX/bin/

# Add a missing dependency on the fly
rattler-build debug host-add libmissing
```

### Library Not Found Errors

```bash
# Check if the library exists in PREFIX
find $PREFIX -name "lib*.so*" -o -name "lib*.dylib*"

# Check pkg-config paths
echo $PKG_CONFIG_PATH
pkg-config --libs --cflags libfoo
```

### Build Log Analysis

All build output is saved to `conda_build.log`:

```bash
# View the full log
less conda_build.log

# Search for errors
grep -i error conda_build.log
grep -i "undefined reference" conda_build.log
```

## Debugging with AI Agents

The `debug` subcommands are designed to work well with AI coding agents (Claude
Code, Codex, etc.) that cannot use interactive shells. The key principle is:
**set up once, then iterate fast by re-running the build script**.

### Agent Workflow

```bash
# 1. Set up the debug environment (slow, only once)
rattler-build debug setup --recipe recipe.yaml

# 2. Get the work directory
rattler-build debug workdir

# 3. Edit source files in work directory to fix the issue
#    (agent edits files directly)

# 4. Re-run the build script (fast — no dependency resolution)
rattler-build debug run

# 5. If the build fails, go back to step 3
# 6. Once it works, create a patch
rattler-build debug create-patch --name my-fix
```

### Key Points for Agents

- **`debug setup`** is non-interactive — it sets up everything and exits. Use
  this instead of `debug shell` which opens an interactive shell.
- **`debug workdir`** prints the work directory path to stdout — no `jq` or log
  parsing needed.
- **`debug run`** re-runs the build script with the environment already set up.
  Use `--trace` for verbose `bash -x` output. This is the fast inner loop — it
  takes seconds, not minutes, because dependencies are already installed.
- **`debug host-add` / `debug build-add`** let agents add missing dependencies
  without re-running the full setup.
- **`debug create-patch`** generates a unified diff from changes in the work
  directory. The agent can then add the patch to the recipe.

### Parsing the Build Log

The last line of `output/rattler-build-log.txt` is JSON:

```json
{
  "work_dir": "/path/to/output/bld/rattler-build_pkg_1234/work",
  "build_dir": "/path/to/output/bld/rattler-build_pkg_1234",
  "host_prefix": "/path/to/output/bld/rattler-build_pkg_1234/host_env_placehold_...",
  "build_prefix": "/path/to/output/bld/rattler-build_pkg_1234/build_env",
  "recipe_dir": "/path/to/recipe/dir",
  "recipe_path": "/path/to/recipe.yaml",
  "output_dir": "/path/to/output"
}
```

## Understanding Relocatability

Rattler-Build makes packages relocatable through:

1. **RPATH patching** - Changes `.dylib` and `.so` files to use relative paths (`$ORIGIN`, `@loader_path`) using `patchelf` or `install_name_tool`

2. **Placeholder replacement** - At install time, replaces placeholder strings in binaries and text files with the actual prefix

**Important**: The placeholder is a long string (`placehold_placehol_...`). If your code has small buffer optimization or assumes static string lengths for file paths, you may need to adjust it. The `$PREFIX` length will differ at installation time. The placeholder replacement in binary files will overwrite the placeholder string, move the remainder until `\0` is found in the original string, and pad with `\0` bytes.

## Useful Commands Reference

```bash
# Build commands
rattler-build build --recipe recipe.yaml --keep-build
rattler-build build --recipe recipe.yaml --channel conda-forge --no-test

# Debug commands
rattler-build debug setup --recipe recipe.yaml         # Set up environment
rattler-build debug shell                              # Open shell in last build
rattler-build debug shell --work-dir /path/to/work     # Open shell in specific build
rattler-build debug workdir                            # Print work directory path
rattler-build debug run                                # Re-run build script
rattler-build debug run --trace                        # Re-run with bash -x tracing
rattler-build debug host-add python numpy              # Add packages to host env
rattler-build debug build-add cmake                    # Add packages to build env
rattler-build debug create-patch --name fix            # Create patch from changes
rattler-build debug create-patch --name fix --dry-run  # Preview patch

# Test commands
rattler-build test --package-file output/linux-64/mypackage-1.0.tar.bz2
```
