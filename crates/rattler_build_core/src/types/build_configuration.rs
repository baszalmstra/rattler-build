//! All the metadata that makes up a recipe file
use std::collections::BTreeMap;

use rattler_build_jinja::{JinjaConfig, Variable};
use rattler_build_recipe::stage1::HashInfo;
use rattler_build_types::NormalizedKey;
use rattler_conda_types::{ChannelUrl, PackageName, Platform, RepodataRevision};
use rattler_solve::{ChannelPriority, SolveStrategy};
use serde::{Deserialize, Serialize};

use crate::types::{
    Directories, PackageIdentifier, PackagingSettings, PlatformWithVirtualPackages,
};

use rattler_build_script::{EnvironmentIsolation, SandboxConfiguration};

/// Default value for store recipe for backwards compatibility
fn default_true() -> bool {
    true
}
/// The configuration for a build of a package
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfiguration {
    /// The target platform for the build
    pub target_platform: Platform,
    /// The host platform (usually target platform, but for `noarch` it's the
    /// build platform)
    pub host_platform: PlatformWithVirtualPackages,
    /// The build platform (the platform that the build is running on)
    pub build_platform: PlatformWithVirtualPackages,
    /// The selected variant for this build
    pub variant: BTreeMap<NormalizedKey, Variable>,
    /// The computed hash of the variant
    pub hash: HashInfo,
    /// The directories for the build (work, source, build, host, ...)
    pub directories: Directories,
    /// The channels to use when resolving environments
    pub channels: Vec<ChannelUrl>,
    /// The channel priority that is used to resolve dependencies
    pub channel_priority: ChannelPriority,
    /// The solve strategy to use when resolving dependencies
    pub solve_strategy: SolveStrategy,
    /// The timestamp to use for the build
    pub timestamp: jiff::Timestamp,
    /// All subpackages coming from this output or other outputs from the same
    /// recipe
    pub subpackages: BTreeMap<PackageName, PackageIdentifier>,
    /// Package format (.tar.bz2 or .conda)
    pub packaging_settings: PackagingSettings,
    /// Whether to store the recipe and build instructions in the final package
    /// or not
    #[serde(skip_serializing, default = "default_true")]
    pub store_recipe: bool,
    /// Whether to set additional environment variables to force colors in the
    /// build script or not
    #[serde(skip_serializing, default = "default_true")]
    pub force_colors: bool,

    /// The environment isolation mode for build scripts
    #[serde(skip_serializing, default)]
    pub env_isolation: EnvironmentIsolation,

    /// The configuration for the sandbox
    #[serde(skip_serializing, default)]
    pub sandbox_config: Option<SandboxConfiguration>,
    /// Exclude packages newer than this date from the solver
    #[serde(skip_serializing, default)]
    pub exclude_newer: Option<jiff::Timestamp>,
    /// Repodata revision to target when writing package metadata.
    #[serde(skip_serializing, default)]
    pub repodata_revision: RepodataRevision,
    /// Whether to generate SBOM documents into the final package
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sbom: bool,
}

impl BuildConfiguration {
    /// true if the build is cross-compiling
    pub fn cross_compilation(&self) -> bool {
        self.target_platform != self.build_platform.platform
    }

    /// Retrieve the sandbox configuration for this output
    pub fn sandbox_config(&self) -> Option<&SandboxConfiguration> {
        self.sandbox_config.as_ref()
    }

    /// Construct a `JinjaConfig` from the given `BuildConfiguration`
    pub fn selector_config(&self) -> JinjaConfig {
        JinjaConfig {
            target_platform: self.target_platform,
            host_platform: self.host_platform.platform,
            build_platform: self.build_platform.platform,
            variant: self.variant.clone(),
            experimental: false,
            undefined_behavior: rattler_build_jinja::UndefinedBehavior::Lenient,
            recipe_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A `BuildConfiguration` from a rendered recipe fixture that has no `sbom`
    /// field.
    fn base_configuration() -> BuildConfiguration {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/rendered_recipes/curl_recipe.yaml");
        let content = fs_err::read_to_string(path).unwrap();
        let recipe: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        serde_yaml::from_value(recipe["build_configuration"].clone()).unwrap()
    }

    #[test]
    fn test_build_configuration_sbom_defaults_to_false() {
        // a document without the field deserializes with sbom disabled
        assert!(!base_configuration().sbom);
    }

    #[test]
    fn test_build_configuration_sbom_serialization_is_conditional() {
        let mut configuration = base_configuration();

        // sbom = false is omitted from the serialized form
        let json = serde_json::to_value(&configuration).unwrap();
        assert!(json.get("sbom").is_none());

        // sbom = true is included
        configuration.sbom = true;
        let json = serde_json::to_value(&configuration).unwrap();
        assert_eq!(json.get("sbom"), Some(&serde_json::Value::Bool(true)));
    }
}
