use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use cbs_plugin_sdk as sdk;

use crate::core::{
    BuildConfigKey, BuildOutput, BuildPlugin, BuildResult, Config, Context, DependencyPlan,
    DependencyPlannerPlugin, ExternalRequirement, ResolverPlugin, RuleContext, RulePlugin, Task,
};

pub struct AbiRulePlugin {
    plugin: sdk::CbsPluginV1,
    rule_kinds: Vec<String>,
    label_fields: Vec<String>,
    _library: Option<Arc<libloading::Library>>,
}

impl std::fmt::Debug for AbiRulePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiRulePlugin")
            .field("abi_version", &self.plugin.abi_version)
            .field("rule_kinds", &self.rule_kinds)
            .finish()
    }
}

impl AbiRulePlugin {
    pub fn new(
        plugin: sdk::CbsPluginV1,
        rule_kinds: Vec<String>,
        label_fields: Vec<String>,
    ) -> std::io::Result<Self> {
        validate_abi(plugin)?;
        Ok(Self {
            plugin,
            rule_kinds,
            label_fields,
            _library: None,
        })
    }

    pub fn with_library(
        plugin: sdk::CbsPluginV1,
        rule_kinds: Vec<String>,
        label_fields: Vec<String>,
        library: Arc<libloading::Library>,
    ) -> std::io::Result<Self> {
        let mut plugin = Self::new(plugin, rule_kinds, label_fields)?;
        plugin._library = Some(library);
        Ok(plugin)
    }
}

impl RulePlugin for AbiRulePlugin {
    fn rule_kinds(&self) -> Vec<&str> {
        self.rule_kinds.iter().map(|kind| kind.as_str()).collect()
    }

    fn config_from_target(
        &self,
        context: &RuleContext,
        kind: &str,
        target: &toml::Table,
    ) -> std::io::Result<Config> {
        let request = sdk::ParseRuleRequest {
            workspace_root: context.workspace_root.clone(),
            package: context.package.clone(),
            package_dir: context.package_dir.clone(),
            kind: kind.to_string(),
            name: context.required_string(target, "name")?,
            sources: context.source_paths(target, "srcs")?,
            dependencies: context.label_list(target, "deps")?,
            cargo_requirements: context
                .cargo_requirements(target)?
                .into_iter()
                .map(external_requirement_to_sdk)
                .collect(),
            string_fields: string_fields(target),
            label_fields: label_fields(context, target, &self.label_fields)?,
        };
        let request = sdk::encode_parse_rule_request(&request);
        let response_buffer = (self.plugin.parse_rule)(sdk::CbsSlice::from_slice(&request));
        let response = owned_buffer_bytes(response_buffer, self.plugin.free_buffer);
        match sdk::decode_parse_rule_response(&response)? {
            sdk::ParseRuleResponse::Success(config) => Ok(config_from_sdk(config)),
            sdk::ParseRuleResponse::Failure(error) => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }
        }
    }
}

pub struct AbiDependencyPlanner {
    plugin: sdk::CbsPluginV1,
    ecosystem: String,
    _library: Option<Arc<libloading::Library>>,
}

impl std::fmt::Debug for AbiDependencyPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiDependencyPlanner")
            .field("abi_version", &self.plugin.abi_version)
            .field("ecosystem", &self.ecosystem)
            .finish()
    }
}

impl AbiDependencyPlanner {
    pub fn new(plugin: sdk::CbsPluginV1, ecosystem: String) -> std::io::Result<Self> {
        validate_abi(plugin)?;
        Ok(Self {
            plugin,
            ecosystem,
            _library: None,
        })
    }

    pub fn with_library(
        plugin: sdk::CbsPluginV1,
        ecosystem: String,
        library: Arc<libloading::Library>,
    ) -> std::io::Result<Self> {
        let mut planner = Self::new(plugin, ecosystem)?;
        planner._library = Some(library);
        Ok(planner)
    }
}

impl DependencyPlannerPlugin for AbiDependencyPlanner {
    fn ecosystem(&self) -> &str {
        &self.ecosystem
    }

    fn plan(
        &self,
        context: Context,
        requirements: &[ExternalRequirement],
    ) -> std::io::Result<DependencyPlan> {
        let request = sdk::PlanDependenciesRequest {
            ecosystem: self.ecosystem.clone(),
            requirements: requirements
                .iter()
                .cloned()
                .map(external_requirement_to_sdk)
                .collect(),
            context: plugin_context_to_sdk(&context),
        };
        let request = sdk::encode_plan_dependencies_request(&request);
        let response_buffer = (self.plugin.plan_dependencies)(sdk::CbsSlice::from_slice(&request));
        let response = owned_buffer_bytes(response_buffer, self.plugin.free_buffer);
        match sdk::decode_plan_dependencies_response(&response)? {
            sdk::PlanDependenciesResponse::Success(plan) => Ok(dependency_plan_from_sdk(plan)),
            sdk::PlanDependenciesResponse::Failure(error) => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }
        }
    }
}

pub struct AbiResolverPlugin {
    plugin: sdk::CbsPluginV1,
    target_prefixes: Vec<String>,
    _library: Option<Arc<libloading::Library>>,
}

impl std::fmt::Debug for AbiResolverPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiResolverPlugin")
            .field("abi_version", &self.plugin.abi_version)
            .field("target_prefixes", &self.target_prefixes)
            .finish()
    }
}

impl AbiResolverPlugin {
    pub fn new(plugin: sdk::CbsPluginV1, target_prefixes: Vec<String>) -> std::io::Result<Self> {
        validate_abi(plugin)?;
        Ok(Self {
            plugin,
            target_prefixes,
            _library: None,
        })
    }

    pub fn with_library(
        plugin: sdk::CbsPluginV1,
        target_prefixes: Vec<String>,
        library: Arc<libloading::Library>,
    ) -> std::io::Result<Self> {
        let mut resolver = Self::new(plugin, target_prefixes)?;
        resolver._library = Some(library);
        Ok(resolver)
    }
}

impl ResolverPlugin for AbiResolverPlugin {
    fn can_resolve(&self, target: &str) -> bool {
        self.target_prefixes
            .iter()
            .any(|prefix| target.starts_with(prefix))
    }

    fn resolve(&self, context: Context, target: &str) -> std::io::Result<Config> {
        let request = sdk::ResolveTargetRequest {
            target: target.to_string(),
            context: plugin_context_to_sdk(&context.with_target(target)),
        };
        let request = sdk::encode_resolve_target_request(&request);
        let response_buffer = (self.plugin.resolve_target)(sdk::CbsSlice::from_slice(&request));
        let response = owned_buffer_bytes(response_buffer, self.plugin.free_buffer);
        match sdk::decode_resolve_target_response(&response)? {
            sdk::ResolveTargetResponse::Success(config) => Ok(config_from_sdk(config)),
            sdk::ResolveTargetResponse::Failure(error) => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }
        }
    }
}

pub struct AbiBuildPlugin {
    plugin: sdk::CbsPluginV1,
    _library: Option<Arc<libloading::Library>>,
}

impl std::fmt::Debug for AbiBuildPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiBuildPlugin")
            .field("abi_version", &self.plugin.abi_version)
            .finish()
    }
}

impl AbiBuildPlugin {
    pub fn new(plugin: sdk::CbsPluginV1) -> std::io::Result<Self> {
        validate_abi(plugin)?;
        Ok(Self {
            plugin,
            _library: None,
        })
    }

    pub fn with_library(
        plugin: sdk::CbsPluginV1,
        library: Arc<libloading::Library>,
    ) -> std::io::Result<Self> {
        let mut plugin = Self::new(plugin)?;
        plugin._library = Some(library);
        Ok(plugin)
    }
}

impl BuildPlugin for AbiBuildPlugin {
    fn build(
        &self,
        context: Context,
        task: Task,
        dependencies: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        let Some(config) = task.config.as_ref() else {
            return BuildResult::Failure("ABI build requires a resolved config".to_string());
        };
        let dependencies = dependencies
            .iter()
            .map(|(target, output)| (target.clone(), build_output_to_sdk(output)))
            .collect();
        let working_directory = context.working_directory();
        let request = sdk::encode_build_request_parts(
            &task.target,
            &config_to_sdk(config),
            &dependencies,
            &working_directory,
        );
        let response_buffer = (self.plugin.build)(sdk::CbsSlice::from_slice(&request));
        let response = owned_buffer_bytes(response_buffer, self.plugin.free_buffer);
        match sdk::decode_build_response(&response) {
            Ok(sdk::BuildResponse::Success(output)) => {
                BuildResult::Success(build_output_from_sdk(output))
            }
            Ok(sdk::BuildResponse::Failure(error)) => BuildResult::Failure(error),
            Ok(sdk::BuildResponse::Delegate(config)) => {
                let mut task = task;
                task.config = Some(config_from_sdk(config));
                match load_delegated_rust_plugin() {
                    Ok(plugin) => plugin.build(context, task, dependencies_from_sdk(dependencies)),
                    Err(e) => {
                        BuildResult::Failure(format!("failed to load delegated rust plugin: {e}"))
                    }
                }
            }
            Err(e) => BuildResult::Failure(format!("failed to decode ABI plugin response: {e}")),
        }
    }
}

#[cfg(not(test))]
fn load_delegated_rust_plugin() -> std::io::Result<AbiBuildPlugin> {
    load_build_plugin(Path::new("/tmp/rust.cdylib"))
}

#[cfg(test)]
fn load_delegated_rust_plugin() -> std::io::Result<AbiBuildPlugin> {
    AbiBuildPlugin::new(crate::rust_plugin::cbs_plugin_v1())
        .or_else(|_| load_build_plugin(Path::new("/tmp/rust.cdylib")))
}

#[derive(Debug)]
pub struct LoadedAbiPlugin {
    pub plugin: sdk::CbsPluginV1,
    pub manifest: sdk::PluginManifest,
    pub library: Option<Arc<libloading::Library>>,
}

type CbsPluginV1Entrypoint = unsafe extern "C" fn() -> sdk::CbsPluginV1;

#[cfg(test)]
pub fn loaded_builtin_plugin(plugin: sdk::CbsPluginV1) -> std::io::Result<LoadedAbiPlugin> {
    Ok(LoadedAbiPlugin {
        plugin,
        manifest: read_plugin_manifest(plugin)?,
        library: None,
    })
}

pub fn load_dynamic_plugin(path: &Path) -> std::io::Result<LoadedAbiPlugin> {
    let library = Arc::new(unsafe { libloading::Library::new(path) }.map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to open dynamic library {}: {e}", path.display()),
        )
    })?);
    let entrypoint =
        unsafe { library.get::<CbsPluginV1Entrypoint>(b"cbs_plugin_v1") }.map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to load cbs_plugin_v1 from {}: {e}", path.display()),
            )
        })?;
    let plugin = unsafe { entrypoint() };
    drop(entrypoint);
    let manifest = read_plugin_manifest(plugin)?;
    Ok(LoadedAbiPlugin {
        plugin,
        manifest,
        library: Some(library),
    })
}

pub fn load_build_plugin(path: &Path) -> std::io::Result<AbiBuildPlugin> {
    let loaded = load_dynamic_plugin(path)?;
    match loaded.library {
        Some(library) => AbiBuildPlugin::with_library(loaded.plugin, library),
        None => AbiBuildPlugin::new(loaded.plugin),
    }
}

fn read_plugin_manifest(plugin: sdk::CbsPluginV1) -> std::io::Result<sdk::PluginManifest> {
    let buffer = (plugin.manifest)();
    let bytes = owned_buffer_bytes(buffer, plugin.free_buffer);
    sdk::decode_plugin_manifest(&bytes)
}

fn validate_abi(plugin: sdk::CbsPluginV1) -> std::io::Result<()> {
    if plugin.abi_version != sdk::CBS_PLUGIN_ABI_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unsupported plugin ABI version {}; expected {}",
                plugin.abi_version,
                sdk::CBS_PLUGIN_ABI_VERSION
            ),
        ));
    }
    Ok(())
}

fn owned_buffer_bytes(
    buffer: sdk::CbsOwnedBuffer,
    free: extern "C" fn(sdk::CbsOwnedBuffer),
) -> Vec<u8> {
    let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len).to_vec() };
    free(buffer);
    bytes
}

fn string_fields(target: &toml::Table) -> HashMap<String, String> {
    target
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.to_string(), value.to_string()))
        })
        .collect()
}

fn label_fields(
    context: &RuleContext,
    target: &toml::Table,
    fields: &[String],
) -> std::io::Result<HashMap<String, String>> {
    let mut labels = HashMap::new();
    for field in fields {
        if let Some(label) = context.optional_label(target, field)? {
            labels.insert(field.clone(), label);
        }
    }
    Ok(labels)
}

fn config_to_sdk(config: &Config) -> sdk::Config {
    sdk::Config {
        dependencies: config.dependencies.clone(),
        external_requirements: config
            .external_requirements
            .iter()
            .cloned()
            .map(external_requirement_to_sdk)
            .collect(),
        build_plugin: config.build_plugin.clone(),
        location: config.location.clone(),
        sources: config.sources.clone(),
        build_dependencies: config.build_dependencies.clone(),
        kind: config.kind.clone(),
        extras: config.extras.clone(),
    }
}

fn config_from_sdk(config: sdk::Config) -> Config {
    Config {
        dependencies: config.dependencies,
        external_requirements: config
            .external_requirements
            .into_iter()
            .map(external_requirement_from_sdk)
            .collect(),
        build_plugin: config.build_plugin,
        location: config.location,
        sources: config.sources,
        build_dependencies: config.build_dependencies,
        kind: config.kind,
        extras: config.extras,
        ..Default::default()
    }
}

fn external_requirement_to_sdk(requirement: ExternalRequirement) -> sdk::ExternalRequirement {
    sdk::ExternalRequirement {
        ecosystem: requirement.ecosystem,
        package: requirement.package,
        version: requirement.version,
        features: requirement.features,
        default_features: requirement.default_features,
        target: requirement.target,
    }
}

fn external_requirement_from_sdk(requirement: sdk::ExternalRequirement) -> ExternalRequirement {
    ExternalRequirement {
        ecosystem: requirement.ecosystem,
        package: requirement.package,
        version: requirement.version,
        features: requirement.features,
        default_features: requirement.default_features,
        target: requirement.target,
    }
}

fn build_output_to_sdk(output: &BuildOutput) -> sdk::BuildOutput {
    sdk::BuildOutput {
        outputs: output.outputs.clone(),
        extras: output.extras.clone(),
    }
}

fn build_output_from_sdk(output: sdk::BuildOutput) -> BuildOutput {
    BuildOutput {
        outputs: output.outputs,
        extras: output.extras,
    }
}

fn dependencies_from_sdk(
    dependencies: HashMap<String, sdk::BuildOutput>,
) -> HashMap<String, BuildOutput> {
    dependencies
        .into_iter()
        .map(|(target, output)| (target, build_output_from_sdk(output)))
        .collect()
}

fn dependency_plan_from_sdk(plan: sdk::DependencyPlan) -> DependencyPlan {
    DependencyPlan {
        lockfile: plan.lockfile,
        locked_dependencies: plan.locked_dependencies,
    }
}

fn plugin_context_to_sdk(context: &Context) -> sdk::PluginContext {
    sdk::PluginContext {
        cache_dir: context.cache_dir.clone(),
        context_hash: context.hash,
        target_config: context
            .config
            .iter()
            .map(|(key, value)| (build_config_key_to_sdk(*key), value.clone()))
            .collect(),
        lockfile: context.lockfile.as_ref().clone(),
        locked_dependencies: context.locked_dependencies.as_ref().clone(),
        target: context.target.clone(),
    }
}

fn build_config_key_to_sdk(key: BuildConfigKey) -> u32 {
    match key {
        BuildConfigKey::TargetFamily => sdk::build_config_key::TARGET_FAMILY,
        BuildConfigKey::TargetEnv => sdk::build_config_key::TARGET_ENV,
        BuildConfigKey::TargetOS => sdk::build_config_key::TARGET_OS,
        BuildConfigKey::TargetArch => sdk::build_config_key::TARGET_ARCH,
        BuildConfigKey::TargetVendor => sdk::build_config_key::TARGET_VENDOR,
        BuildConfigKey::TargetEndian => sdk::build_config_key::TARGET_ENDIAN,
    }
}
