//! Declaration files through which a build step declares further work.
//!
//! Every build step (but not a plain `build.script`) runs with two
//! environment variables naming files in a directory of its own:
//!
//! - [`STEP_MANIFEST_ENV`] names the file the step may write a
//!   [`StepManifest`] to: further steps to run, and additions to steps that
//!   have not started yet.
//! - [`STEP_INPUTS_ENV`] names the file the step may write an
//!   [`InputManifest`] to: the paths it found it read while it ran.
//!
//! A step does not have to write either file; a missing or empty file
//! declares nothing. The files are read only after their step succeeded, and
//! removed before it starts ([`DeclarationPaths::prepare`]), so a file left by
//! an earlier run is never taken for a new declaration. They stay in place
//! after the step succeeded, as a record of what it declared.
//!
//! Both files are JSON documents with a `version`, which has to be `1`. A step
//! manifest declares new steps with `steps` and adds to existing ones with
//! `updates`; both default to an empty list:
//!
//! ```json
//! {
//!   "version": 1,
//!   "steps": [
//!     {
//!       "id": "compile-a",
//!       "run": ["cc -c a.c -o a.o"],
//!       "inputs": [{ "root": "work", "path": "a.c" }],
//!       "outputs": [{ "root": "work", "path": "a.o" }]
//!     },
//!     {
//!       "id": "scan-b",
//!       "run": "python scan.py b.c",
//!       "interpreter": "bash",
//!       "cwd": "src",
//!       "env": { "SCAN_MODE": "fast" },
//!       "inputs": [{ "root": "work", "path": "b.c" }],
//!       "outputs": [],
//!       "depends_on": ["compile-a"],
//!       "discover_after": ["configure"]
//!     }
//!   ],
//!   "updates": [
//!     {
//!       "step": "configure/link",
//!       "inputs": [{ "root": "work", "path": "a.o" }]
//!     }
//!   ]
//! }
//! ```
//!
//! A generated step is known by its producer's id and its own: step `A`
//! declared by step `G` is `G/A`, and step `X` declared by `G/A` is `G/A/X`.
//! A reference in `depends_on`, `discover_after` or an update names another
//! step declared by the same manifest by its own id, and any other step by its
//! (qualified) id. Like any step, a generated step declares both `inputs` and
//! `outputs` or neither, runs in the activated build environment with only its
//! own `env` added, and runs in the work directory, or, like a recipe step
//! with a `cwd`, in its `cwd` relative to the host prefix.
//!
//! An update adds `inputs`, `outputs` and `depends_on` entries to a step that
//! has not started yet; the step the update names must already be known when
//! the manifest is registered.
//!
//! An input report lists the paths a step read, in the form of step inputs:
//!
//! ```json
//! {
//!   "version": 1,
//!   "inputs": [
//!     { "root": "work", "path": "include/config.h" },
//!     { "root": "host", "path": "include/**/*.h", "kind": "glob" }
//!   ]
//! }
//! ```
//!
//! Parsing a file checks its version and schema, and the ids, references and
//! declarations every step and update has on its own. Whether the references
//! resolve, whether the declared paths are valid below their roots and whether
//! the declarations fit into the build is checked when the declarations are
//! registered with the steps already known.

use std::fmt;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::step_model::{StepInput, StepOutput};

#[cfg(feature = "execution")]
pub use files::{
    DeclarationFile, DeclarationFileError, DeclarationKind, DeclarationPaths, ManifestError,
};

/// The environment variable naming the file a build step writes its
/// [`StepManifest`] to.
pub const STEP_MANIFEST_ENV: &str = "RATTLER_BUILD_STEP_MANIFEST";

/// The environment variable naming the file a build step writes its
/// [`InputManifest`] to.
pub const STEP_INPUTS_ENV: &str = "RATTLER_BUILD_STEP_INPUTS";

/// The version of the step manifest format.
pub const STEP_MANIFEST_VERSION: u64 = 1;

/// The version of the input report format.
pub const INPUT_MANIFEST_VERSION: u64 = 1;

/// The further steps and step updates a build step declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepManifest {
    /// Steps to add to the build, in the order they were declared.
    pub steps: Vec<GeneratedStep>,
    /// Additions to steps that have not started yet.
    pub updates: Vec<StepUpdate>,
}

impl StepManifest {
    /// Whether the manifest declares nothing.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.updates.is_empty()
    }
}

/// A step declared by another step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedStep {
    /// The id of the step, unique among the steps of its manifest. Other
    /// steps refer to it by this id qualified with the id of its producer.
    pub id: String,
    /// The script to run.
    pub run: GeneratedRun,
    /// The interpreter to run the script with, if not the default one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
    /// The directory to run the step in, relative to the host prefix like the
    /// `cwd` of a recipe step; the step runs in the work directory without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Environment variables set for this step only.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    /// The paths the step reads, or `None` when not declared.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub inputs: Option<Vec<StepInput>>,
    /// The paths the step writes, or `None` when not declared.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub outputs: Option<Vec<StepOutput>>,
    /// Steps that have to finish, with everything they generate, before this
    /// step starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Generating steps whose declarations have to be registered before this
    /// step starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discover_after: Vec<String>,
}

impl GeneratedStep {
    /// Whether the step declares neither inputs nor outputs, which makes it a
    /// sequential barrier.
    pub fn is_barrier(&self) -> bool {
        self.inputs.is_none() && self.outputs.is_none()
    }
}

/// The script of a generated step: always literal script content, never the
/// path of a script file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum GeneratedRun {
    /// A single script body.
    Script(String),
    /// Commands, joined by the interpreter of the step.
    Commands(Vec<String>),
}

impl<'de> Deserialize<'de> for GeneratedRun {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RunVisitor;

        impl<'de> Visitor<'de> for RunVisitor {
            type Value = GeneratedRun;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a script string or a list of command strings")
            }

            fn visit_str<E: de::Error>(self, script: &str) -> Result<Self::Value, E> {
                Ok(GeneratedRun::Script(script.to_owned()))
            }

            fn visit_string<E: de::Error>(self, script: String) -> Result<Self::Value, E> {
                Ok(GeneratedRun::Script(script))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut commands = Vec::with_capacity(seq.size_hint().unwrap_or_default());
                while let Some(command) = seq.next_element::<String>()? {
                    commands.push(command);
                }
                Ok(GeneratedRun::Commands(commands))
            }
        }

        deserializer.deserialize_any(RunVisitor)
    }
}

/// Additions to a step that has not started yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepUpdate {
    /// The (qualified) id of the step to add to.
    pub step: String,
    /// Paths the step also reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<StepInput>,
    /// Paths the step also writes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<StepOutput>,
    /// Steps that also have to finish before the step starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// The paths a build step reported to have read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputManifest {
    /// The paths read, in the form of step inputs.
    pub inputs: Vec<StepInput>,
}

/// Deserializes a field that is `None` only when it is missing (through
/// `#[serde(default)]`): an explicit `null` is rejected, so it cannot be
/// mistaken for an undeclared list.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(feature = "execution")]
mod files {
    use std::collections::HashSet;
    use std::fmt;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};

    use serde::de::IgnoredAny;
    use serde::{Deserialize, Serialize};
    use thiserror::Error;

    use super::{
        GeneratedStep, INPUT_MANIFEST_VERSION, InputManifest, STEP_INPUTS_ENV, STEP_MANIFEST_ENV,
        STEP_MANIFEST_VERSION, StepManifest, StepUpdate,
    };
    use crate::step_model::{STEP_ID_SEPARATOR, StepInput, is_valid_step_id};

    /// Which declaration file a diagnostic is about.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DeclarationKind {
        /// A [`StepManifest`].
        StepManifest,
        /// An [`InputManifest`].
        InputManifest,
    }

    impl fmt::Display for DeclarationKind {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                Self::StepManifest => "step manifest",
                Self::InputManifest => "input report",
            })
        }
    }

    /// Why the contents of a declaration file are invalid.
    ///
    /// Messages name ids, fields and environment variable names, never the
    /// values of environment variables.
    #[derive(Debug, Error)]
    pub enum ManifestError {
        /// The contents are not a JSON document.
        #[error("not valid JSON: {0}")]
        Malformed(serde_json::Error),
        /// The document is not a JSON object.
        #[error("expected a JSON object with a `version`")]
        NotAnObject,
        /// The document has no `version`.
        #[error("no `version`; expected `\"version\": {expected}`")]
        MissingVersion {
            /// The version this build reads.
            expected: u64,
        },
        /// The document has a version this build does not read.
        #[error("unsupported version `{found}`; only version {expected} is supported")]
        UnsupportedVersion {
            /// The version, as written.
            found: String,
            /// The version this build reads.
            expected: u64,
        },
        /// The document does not match the schema of its version.
        #[error("{0}")]
        Schema(serde_json::Error),
        /// A generated step has an invalid id.
        #[error(
            "the step id `{id}` is invalid; expected a non-empty name of ASCII letters, digits, `_`, `-` and `.`"
        )]
        InvalidId {
            /// The id as written.
            id: String,
        },
        /// Two generated steps have the same id.
        #[error("the step `{id}` is declared more than once; step ids must be unique")]
        DuplicateId {
            /// The id of both steps.
            id: String,
        },
        /// A generated step declares only one of `inputs` and `outputs`.
        #[error(
            "the step `{step}` declares `{declared}` but not `{missing}`; declare both (an empty list declares none) or neither to run the step as a sequential barrier"
        )]
        PartialDeclaration {
            /// The id of the step.
            step: String,
            /// The field the step declares.
            declared: &'static str,
            /// The field the step does not declare.
            missing: &'static str,
        },
        /// A reference is not a step id or a qualified step id.
        #[error(
            "the step `{step}` names `{reference}` in `{field}`, which is neither a step id nor a qualified id like `producer/step`"
        )]
        InvalidReference {
            /// The id of the step, or the step an update names.
            step: String,
            /// The field of the reference.
            field: &'static str,
            /// The reference as written.
            reference: String,
        },
        /// An update names something that is not a step id or a qualified
        /// step id.
        #[error(
            "an update names `{step}`, which is neither a step id nor a qualified id like `producer/step`"
        )]
        InvalidUpdateTarget {
            /// The name as written.
            step: String,
        },
        /// A generated step has an empty `interpreter`.
        #[error("the step `{step}` has an empty `interpreter`")]
        EmptyInterpreter {
            /// The id of the step.
            step: String,
        },
        /// A generated step has an absolute `cwd`.
        #[error(
            "the step `{step}` has the absolute `cwd` `{}`; declare it relative to the host prefix",
            cwd.display()
        )]
        AbsoluteCwd {
            /// The id of the step.
            step: String,
            /// The directory as written.
            cwd: PathBuf,
        },
        /// A generated step sets an environment variable with an invalid name.
        #[error(
            "the step `{step}` sets the environment variable `{name}`, which is not a name of the form [A-Za-z_][A-Za-z0-9_]*"
        )]
        InvalidEnvName {
            /// The id of the step.
            step: String,
            /// The name as written.
            name: String,
        },
        /// A generated step sets a variable that names its declaration files.
        #[error(
            "the step `{step}` sets `{name}`, which rattler-build sets to the declaration files of the step"
        )]
        ReservedEnvName {
            /// The id of the step.
            step: String,
            /// The name as written.
            name: String,
        },
        /// An update names a step declared by the same manifest.
        #[error(
            "an update names `{step}`, which this manifest declares; declare its inputs, outputs and dependencies on the step itself"
        )]
        UpdateOfDeclaredStep {
            /// The id of the step.
            step: String,
        },
        /// Two updates name the same step.
        #[error("the step `{step}` is updated more than once; combine its updates")]
        DuplicateUpdate {
            /// The step both updates name.
            step: String,
        },
        /// The declarations could not be serialized.
        #[error("could not serialize the declarations: {0}")]
        Serialize(serde_json::Error),
    }

    /// A declaration file that could not be read, written or removed, or is
    /// invalid.
    #[derive(Debug, Error)]
    pub enum DeclarationFileError {
        /// The file could not be read, written or removed.
        #[error("could not access the {kind}: {error}")]
        Io {
            /// Which file it is.
            kind: DeclarationKind,
            /// The path of the file.
            path: PathBuf,
            /// The error, which names the path.
            error: std::io::Error,
        },
        /// The contents of the file are invalid.
        #[error("the {kind} `{}` is invalid: {error}", path.display())]
        Invalid {
            /// Which file it is.
            kind: DeclarationKind,
            /// The path of the file.
            path: PathBuf,
            /// Why the contents are invalid.
            error: ManifestError,
        },
    }

    /// The contents of a declaration file, with the path and bytes they were
    /// read from.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DeclarationFile<T> {
        /// The path the file was read from.
        pub path: PathBuf,
        /// The bytes of the file, exactly as the step wrote them.
        pub bytes: Vec<u8>,
        /// The parsed and validated contents.
        pub contents: T,
    }

    /// The paths of the declaration files of one build step.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DeclarationPaths {
        /// The file the step may write its [`StepManifest`] to.
        pub manifest: PathBuf,
        /// The file the step may write its [`InputManifest`] to.
        pub inputs: PathBuf,
    }

    impl DeclarationPaths {
        /// The name of the step manifest in the declaration directory.
        pub const MANIFEST_FILE_NAME: &'static str = "steps.json";
        /// The name of the input report in the declaration directory.
        pub const INPUTS_FILE_NAME: &'static str = "inputs.json";

        /// The declaration files in `dir`, a directory of one step alone.
        pub fn in_dir(dir: &Path) -> Self {
            Self {
                manifest: dir.join(Self::MANIFEST_FILE_NAME),
                inputs: dir.join(Self::INPUTS_FILE_NAME),
            }
        }

        /// Creates the directories of the files and removes the files left by
        /// an earlier run, so only what the step writes is read afterwards.
        pub fn prepare(&self) -> Result<(), DeclarationFileError> {
            prepare_file(DeclarationKind::StepManifest, &self.manifest)?;
            prepare_file(DeclarationKind::InputManifest, &self.inputs)
        }

        /// The environment variables naming the files, to set for the step.
        pub fn env(&self) -> [(&'static str, &Path); 2] {
            [
                (STEP_MANIFEST_ENV, self.manifest.as_path()),
                (STEP_INPUTS_ENV, self.inputs.as_path()),
            ]
        }
    }

    fn prepare_file(kind: DeclarationKind, path: &Path) -> Result<(), DeclarationFileError> {
        let io_error = |error| DeclarationFileError::Io {
            kind,
            path: path.to_path_buf(),
            error,
        };
        if let Some(parent) = path.parent() {
            fs_err::create_dir_all(parent).map_err(io_error)?;
        }
        match fs_err::remove_file(path) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(io_error(error)),
            Ok(()) | Err(_) => Ok(()),
        }
    }

    /// Reads the declaration file at `path`: `None` when the file does not
    /// exist or is empty.
    fn read_file<T>(
        kind: DeclarationKind,
        path: &Path,
        parse: fn(&[u8]) -> Result<T, ManifestError>,
    ) -> Result<Option<DeclarationFile<T>>, DeclarationFileError> {
        let bytes = match fs_err::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DeclarationFileError::Io {
                    kind,
                    path: path.to_path_buf(),
                    error,
                });
            }
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        match parse(&bytes) {
            Ok(contents) => Ok(Some(DeclarationFile {
                path: path.to_path_buf(),
                bytes,
                contents,
            })),
            Err(error) => Err(DeclarationFileError::Invalid {
                kind,
                path: path.to_path_buf(),
                error,
            }),
        }
    }

    fn write_file(
        kind: DeclarationKind,
        path: &Path,
        bytes: Result<Vec<u8>, ManifestError>,
    ) -> Result<(), DeclarationFileError> {
        let bytes = bytes.map_err(|error| DeclarationFileError::Invalid {
            kind,
            path: path.to_path_buf(),
            error,
        })?;
        fs_err::write(path, bytes).map_err(|error| DeclarationFileError::Io {
            kind,
            path: path.to_path_buf(),
            error,
        })
    }

    /// Checks that `bytes` are a JSON object of version `expected`, so an
    /// unsupported version is reported as such rather than as a mismatch
    /// with the schema this build reads.
    fn check_version(bytes: &[u8], expected: u64) -> Result<(), ManifestError> {
        let document: serde_json::Value =
            serde_json::from_slice(bytes).map_err(ManifestError::Malformed)?;
        let object = document.as_object().ok_or(ManifestError::NotAnObject)?;
        match object.get("version") {
            None => Err(ManifestError::MissingVersion { expected }),
            Some(version) if version.as_u64() == Some(expected) => Ok(()),
            Some(version) => Err(ManifestError::UnsupportedVersion {
                found: version.to_string(),
                expected,
            }),
        }
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StepManifestDocument {
        #[serde(rename = "version")]
        _version: IgnoredAny,
        #[serde(default)]
        steps: Vec<GeneratedStep>,
        #[serde(default)]
        updates: Vec<StepUpdate>,
    }

    #[derive(Serialize)]
    struct StepManifestDocumentRef<'a> {
        version: u64,
        steps: &'a [GeneratedStep],
        updates: &'a [StepUpdate],
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InputManifestDocument {
        #[serde(rename = "version")]
        _version: IgnoredAny,
        #[serde(default)]
        inputs: Vec<StepInput>,
    }

    #[derive(Serialize)]
    struct InputManifestDocumentRef<'a> {
        version: u64,
        inputs: &'a [StepInput],
    }

    impl StepManifest {
        /// Parses and validates a step manifest.
        pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
            check_version(bytes, STEP_MANIFEST_VERSION)?;
            let document: StepManifestDocument =
                serde_json::from_slice(bytes).map_err(ManifestError::Schema)?;
            let manifest = Self {
                steps: document.steps,
                updates: document.updates,
            };
            manifest.validate()?;
            Ok(manifest)
        }

        /// Reads the step manifest at `path`: `None` when the step wrote
        /// none, that is, when the file does not exist or is empty.
        pub fn read(path: &Path) -> Result<Option<DeclarationFile<Self>>, DeclarationFileError> {
            read_file(DeclarationKind::StepManifest, path, Self::parse)
        }

        /// The manifest as a JSON document, after validating it.
        pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
            self.validate()?;
            serde_json::to_vec_pretty(&StepManifestDocumentRef {
                version: STEP_MANIFEST_VERSION,
                steps: &self.steps,
                updates: &self.updates,
            })
            .map_err(ManifestError::Serialize)
        }

        /// Validates the manifest and writes it to `path`.
        pub fn write(&self, path: &Path) -> Result<(), DeclarationFileError> {
            write_file(DeclarationKind::StepManifest, path, self.to_json())
        }

        /// Checks the declarations each step and update has on its own; see
        /// the module documentation for what is checked on registration.
        pub fn validate(&self) -> Result<(), ManifestError> {
            let mut ids = HashSet::with_capacity(self.steps.len());
            for step in &self.steps {
                validate_step(step)?;
                if !ids.insert(step.id.as_str()) {
                    return Err(ManifestError::DuplicateId {
                        id: step.id.clone(),
                    });
                }
            }

            let mut updated = HashSet::with_capacity(self.updates.len());
            for update in &self.updates {
                if !is_valid_reference(&update.step) {
                    return Err(ManifestError::InvalidUpdateTarget {
                        step: update.step.clone(),
                    });
                }
                if ids.contains(update.step.as_str()) {
                    return Err(ManifestError::UpdateOfDeclaredStep {
                        step: update.step.clone(),
                    });
                }
                if !updated.insert(update.step.as_str()) {
                    return Err(ManifestError::DuplicateUpdate {
                        step: update.step.clone(),
                    });
                }
                check_references(&update.step, "depends_on", &update.depends_on)?;
            }
            Ok(())
        }
    }

    impl InputManifest {
        /// Parses an input report.
        pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
            check_version(bytes, INPUT_MANIFEST_VERSION)?;
            let document: InputManifestDocument =
                serde_json::from_slice(bytes).map_err(ManifestError::Schema)?;
            Ok(Self {
                inputs: document.inputs,
            })
        }

        /// Reads the input report at `path`: `None` when the step wrote none,
        /// that is, when the file does not exist or is empty.
        pub fn read(path: &Path) -> Result<Option<DeclarationFile<Self>>, DeclarationFileError> {
            read_file(DeclarationKind::InputManifest, path, Self::parse)
        }

        /// The report as a JSON document.
        pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
            serde_json::to_vec_pretty(&InputManifestDocumentRef {
                version: INPUT_MANIFEST_VERSION,
                inputs: &self.inputs,
            })
            .map_err(ManifestError::Serialize)
        }

        /// Writes the report to `path`.
        pub fn write(&self, path: &Path) -> Result<(), DeclarationFileError> {
            write_file(DeclarationKind::InputManifest, path, self.to_json())
        }
    }

    fn validate_step(step: &GeneratedStep) -> Result<(), ManifestError> {
        if !is_valid_step_id(&step.id) {
            return Err(ManifestError::InvalidId {
                id: step.id.clone(),
            });
        }
        let partial = |declared, missing| ManifestError::PartialDeclaration {
            step: step.id.clone(),
            declared,
            missing,
        };
        match (&step.inputs, &step.outputs) {
            (Some(_), None) => return Err(partial("inputs", "outputs")),
            (None, Some(_)) => return Err(partial("outputs", "inputs")),
            (Some(_), Some(_)) | (None, None) => {}
        }
        if step.interpreter.as_deref().is_some_and(str::is_empty) {
            return Err(ManifestError::EmptyInterpreter {
                step: step.id.clone(),
            });
        }
        if let Some(cwd) = &step.cwd
            && is_absolute(cwd)
        {
            return Err(ManifestError::AbsoluteCwd {
                step: step.id.clone(),
                cwd: cwd.clone(),
            });
        }
        for name in step.env.keys() {
            if !is_valid_env_name(name) {
                return Err(ManifestError::InvalidEnvName {
                    step: step.id.clone(),
                    name: name.clone(),
                });
            }
            if [STEP_MANIFEST_ENV, STEP_INPUTS_ENV]
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(name))
            {
                return Err(ManifestError::ReservedEnvName {
                    step: step.id.clone(),
                    name: name.clone(),
                });
            }
        }
        check_references(&step.id, "depends_on", &step.depends_on)?;
        check_references(&step.id, "discover_after", &step.discover_after)
    }

    /// Checks that every reference is a step id or a qualified step id.
    fn check_references(
        step: &str,
        field: &'static str,
        references: &[String],
    ) -> Result<(), ManifestError> {
        match references
            .iter()
            .find(|reference| !is_valid_reference(reference))
        {
            Some(reference) => Err(ManifestError::InvalidReference {
                step: step.to_string(),
                field,
                reference: reference.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Whether `reference` is a step id, or step ids joined by
    /// [`STEP_ID_SEPARATOR`].
    fn is_valid_reference(reference: &str) -> bool {
        reference.split(STEP_ID_SEPARATOR).all(is_valid_step_id)
    }

    /// Whether `path` is absolute or starts at a root or a drive on any
    /// platform, so a manifest means the same on every one.
    fn is_absolute(path: &Path) -> bool {
        let raw = path.to_string_lossy();
        raw.starts_with(['/', '\\'])
            || matches!(raw.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
            || path.is_absolute()
    }

    fn is_valid_env_name(name: &str) -> bool {
        let mut bytes = name.bytes();
        bytes
            .next()
            .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
            && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn manifest_error(json: &str) -> String {
            StepManifest::parse(json.as_bytes())
                .unwrap_err()
                .to_string()
        }

        #[test]
        fn version_is_checked_before_the_schema() {
            let error = manifest_error(r#"{"version": 2, "steps": [], "future": true}"#);
            assert!(error.contains("unsupported version `2`"), "{error}");
            let error = manifest_error(r#"{"steps": []}"#);
            assert!(error.contains("no `version`"), "{error}");
            let error = manifest_error(r#"{"version": "1"}"#);
            assert!(error.contains("unsupported version"), "{error}");
        }

        #[test]
        fn empty_manifest_declares_nothing() {
            let manifest = StepManifest::parse(br#"{"version": 1}"#).unwrap();
            assert!(manifest.is_empty());
        }

        #[test]
        fn invalid_manifests_are_rejected() {
            let cases = [
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x", "inputs": []}]}"#,
                    "declares `inputs` but not `outputs`",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x", "inputs": null, "outputs": []}]}"#,
                    "invalid type: null",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a/b", "run": "x"}]}"#,
                    "the step id `a/b` is invalid",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x"}, {"id": "a", "run": "y"}]}"#,
                    "declared more than once",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x", "discover_after": ["g//a"]}]}"#,
                    "names `g//a` in `discover_after`",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": {"file": "x.sh"}}]}"#,
                    "a script string or a list of command strings",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x", "cmd": "y"}]}"#,
                    "unknown field `cmd`",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x", "env": {"RATTLER_BUILD_STEP_MANIFEST": "secret"}}]}"#,
                    "sets `RATTLER_BUILD_STEP_MANIFEST`",
                ),
                (
                    r#"{"version": 1, "steps": [{"id": "a", "run": "x"}], "updates": [{"step": "a", "depends_on": ["b"]}]}"#,
                    "which this manifest declares",
                ),
                (
                    r#"{"version": 1, "updates": [{"step": "g/a"}, {"step": "g/a"}]}"#,
                    "updated more than once",
                ),
            ];
            for (json, expected) in cases {
                let error = manifest_error(json);
                assert!(error.contains(expected), "{json}: {error}");
                assert!(!error.contains("secret"), "{json}: {error}");
            }
        }

        #[test]
        fn missing_or_empty_files_declare_nothing_but_malformed_ones_fail() {
            let dir = tempfile::tempdir().unwrap();
            let paths = DeclarationPaths::in_dir(dir.path());
            assert!(StepManifest::read(&paths.manifest).unwrap().is_none());

            fs_err::write(&paths.manifest, b"").unwrap();
            assert!(StepManifest::read(&paths.manifest).unwrap().is_none());

            fs_err::write(&paths.inputs, b"{\"version\": 1, \"inputs\": [").unwrap();
            let error = InputManifest::read(&paths.inputs).unwrap_err().to_string();
            assert!(error.contains("input report"), "{error}");
            assert!(error.contains("not valid JSON"), "{error}");

            paths.prepare().unwrap();
            assert!(!paths.manifest.exists());
            assert!(!paths.inputs.exists());
        }
    }
}
