use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::config_file::{load_build_table, load_workspace_table, ConfigTable, ConfigValue};
use crate::core::{
    BuildConfigKey, BuildResult, Config, Context, ExternalRequirement, FakeResolver, Tool,
    FilesystemBuilder, ResolverPlugin, RuleContext, RulePlugin,
};
use crate::exec::Executor;
use crate::plugin_abi::{
    build_config_key_to_sdk, initialize_plugin, load_dynamic_plugin, AbiDependencyPlanner,
    AbiResolverPlugin, AbiRulePlugin, LoadedAbiPlugin,
};

#[cfg(test)]
use crate::plugin_abi::loaded_builtin_plugin;

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    current_package: String,
    config: WorkspaceConfig,
}

pub struct BuildInvocationResult {
    pub targets: Vec<String>,
    pub result: BuildResult,
}

#[derive(Debug, Clone)]
struct WorkspaceConfig {
    cache_dir: PathBuf,
    rustc: String,
    tools: HashMap<String, WorkspaceToolConfig>,
    tool_fingerprints: Vec<(String, String)>,
    target_config: Vec<(BuildConfigKey, String)>,
    plugins: Vec<WorkspacePluginConfig>,
}

#[derive(Debug, Clone)]
struct WorkspaceToolConfig {
    name: String,
    kind: String,
    path: PathBuf,
    sha256: Option<String>,
    fingerprint: String,
}

#[derive(Debug, Clone)]
struct WorkspacePluginConfig {
    name: String,
    path: PathBuf,
    parameters: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceResolver {
    root: PathBuf,
    current_package: String,
    rule_plugins: Arc<Vec<Arc<dyn RulePlugin>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    package: String,
    name: String,
}

impl Workspace {
    pub fn load_from(cwd: &Path) -> std::io::Result<Self> {
        let root = find_workspace_root(cwd)?;
        let workspace_file = workspace_file(&root).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("workspace file disappeared from {}", root.display()),
            )
        })?;
        let config = load_workspace_config(&root, &workspace_file)?;
        let current_package = package_for_cwd(&root, cwd)?;
        Ok(Self {
            root,
            current_package,
            config,
        })
    }

    pub fn executor(&self) -> std::io::Result<Executor> {
        let mut context = Context::new(
            self.config.cache_dir.clone(),
            self.config.target_config.clone(),
        )
        .with_tools(context_tools(&self.config.tools))
        .with_tool_fingerprints(self.config.tool_fingerprints.clone());
        context.calculate_hash();

        let mut executor = Executor::with_context(context);
        let loaded_plugins = self.load_workspace_plugins()?;
        let rule_plugins = rule_plugins(&loaded_plugins)?;
        executor.add_builder_plugin("@filesystem", Arc::new(FilesystemBuilder {}));
        executor.add_resolver_plugin(Box::new(WorkspaceResolver {
            root: self.root.clone(),
            current_package: self.current_package.clone(),
            rule_plugins,
        }));
        add_plugin_resolvers_and_planners(&mut executor, &loaded_plugins)?;
        let mut tool_configs = vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some(self.config.rustc.clone()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
        ];
        for loaded in &loaded_plugins {
            for build_plugin in &loaded.manifest.build_plugins {
                tool_configs.push((
                    build_plugin.as_str(),
                    Ok(Config {
                        build_plugin: "@filesystem".to_string(),
                        location: Some(loaded.path.to_string_lossy().to_string()),
                        ..Default::default()
                    }),
                ));
            }
        }
        executor.add_resolver_plugin(Box::new(FakeResolver::with_configs(tool_configs)));
        Ok(executor)
    }

    pub fn expand_target_patterns(&self, targets: &[String]) -> std::io::Result<Vec<String>> {
        let rule_kinds = self.rule_kinds()?;
        let mut expanded = Vec::new();
        let mut seen = HashSet::new();
        for target in targets {
            for label in self.expand_target_pattern(target, &rule_kinds)? {
                if seen.insert(label.clone()) {
                    expanded.push(label);
                }
            }
        }
        Ok(expanded)
    }

    pub fn expand_test_patterns(&self, targets: &[String]) -> std::io::Result<Vec<String>> {
        let all_rule_kinds = self.rule_kinds()?;
        let test_rule_kinds = self.test_rule_kinds()?;
        let mut expanded = Vec::new();
        let mut seen = HashSet::new();
        for target in targets {
            for label in self.expand_test_pattern(target, &all_rule_kinds, &test_rule_kinds)? {
                if seen.insert(label.clone()) {
                    expanded.push(label);
                }
            }
        }
        Ok(expanded)
    }

    fn expand_target_pattern(
        &self,
        target: &str,
        rule_kinds: &[String],
    ) -> std::io::Result<Vec<String>> {
        if let Some(package) = recursive_package_pattern(target)? {
            return self.expand_recursive_package(&package, rule_kinds);
        }
        parse_label(target, &self.current_package).map(|label| vec![canonical_label(&label)])
    }

    fn expand_recursive_package(
        &self,
        package: &str,
        rule_kinds: &[String],
    ) -> std::io::Result<Vec<String>> {
        let package_dir = self.root.join(package);
        validate_workspace_relative(&self.root, &package_dir)?;
        if !package_dir.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("target pattern //{package}/... refers to a missing package directory"),
            ));
        }

        let mut labels = Vec::new();
        collect_package_targets(&self.root, &package_dir, rule_kinds, &mut labels)?;
        if labels.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("target pattern //{package}/... did not match any targets"),
            ));
        }
        labels.sort();
        Ok(labels)
    }

    fn expand_test_pattern(
        &self,
        target: &str,
        all_rule_kinds: &[String],
        test_rule_kinds: &[String],
    ) -> std::io::Result<Vec<String>> {
        if let Some(package) = recursive_package_pattern(target)? {
            let package_dir = self.root.join(&package);
            validate_workspace_relative(&self.root, &package_dir)?;
            if !package_dir.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("target pattern //{package}/... refers to a missing package directory"),
                ));
            }

            let mut labels = Vec::new();
            collect_package_targets(&self.root, &package_dir, test_rule_kinds, &mut labels)?;
            labels.sort();
            return Ok(labels);
        }

        let label = parse_label(target, &self.current_package)?;
        match self.label_rule_kind(&label, all_rule_kinds)? {
            Some(kind) if test_rule_kinds.iter().any(|test_kind| test_kind == &kind) => {
                Ok(vec![canonical_label(&label)])
            }
            Some(_) => Ok(Vec::new()),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("target {target} not found"),
            )),
        }
    }

    fn label_rule_kind(
        &self,
        label: &Label,
        rule_kinds: &[String],
    ) -> std::io::Result<Option<String>> {
        let package_dir = self.root.join(&label.package);
        validate_workspace_relative(&self.root, &package_dir)?;
        let Some(build_file) = build_file(&package_dir) else {
            return Ok(None);
        };
        let table = load_build_table(&self.root, &build_file)?;
        for kind in rule_kinds {
            if find_named_target(&table, kind, &label.name)?.is_some() {
                return Ok(Some(kind.clone()));
            }
        }
        Ok(None)
    }

    fn rule_kinds(&self) -> std::io::Result<Vec<String>> {
        let mut kinds = Vec::new();
        for loaded in self.load_workspace_plugins()? {
            kinds.extend(loaded.manifest.rule_kinds.clone());
        }
        kinds.sort();
        kinds.dedup();
        Ok(kinds)
    }

    fn test_rule_kinds(&self) -> std::io::Result<Vec<String>> {
        let mut kinds = Vec::new();
        for loaded in self.load_workspace_plugins()? {
            kinds.extend(loaded.manifest.test_rule_kinds.clone());
        }
        kinds.sort();
        kinds.dedup();
        Ok(kinds)
    }

    fn load_workspace_plugins(&self) -> std::io::Result<Vec<LoadedWorkspacePlugin>> {
        self.config
            .plugins
            .iter()
            .map(|plugin| {
                let loaded = load_workspace_dynamic_or_test_plugin(&plugin.path, &plugin.name)
                    .map_err(|e| {
                        std::io::Error::new(
                            e.kind(),
                            format!(
                                "failed to load workspace plugin {} at {}: {e}",
                                plugin.name,
                                plugin.path.display()
                            ),
                        )
                    })?;
                if loaded.manifest.name != plugin.name {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "workspace plugin {} loaded manifest for {}",
                            plugin.name, loaded.manifest.name
                        ),
                    ));
                }
                Ok(LoadedWorkspacePlugin {
                    path: plugin.path.clone(),
                    loaded,
                })
            })
            .collect()
    }
}

#[cfg(not(test))]
fn load_workspace_dynamic_or_test_plugin(
    path: &Path,
    _name: &str,
) -> std::io::Result<LoadedAbiPlugin> {
    load_dynamic_plugin(path)
}

#[cfg(test)]
fn load_workspace_dynamic_or_test_plugin(
    path: &Path,
    name: &str,
) -> std::io::Result<LoadedAbiPlugin> {
    match name {
        "rust" => loaded_builtin_plugin(crate::rust_plugin::cbs_plugin_v1()),
        "bus" => loaded_builtin_plugin(crate::bus::cbs_plugin_v1()),
        _ => load_dynamic_plugin(path),
    }
}

#[derive(Debug)]
struct LoadedWorkspacePlugin {
    path: PathBuf,
    loaded: LoadedAbiPlugin,
}

impl std::ops::Deref for LoadedWorkspacePlugin {
    type Target = LoadedAbiPlugin;

    fn deref(&self) -> &Self::Target {
        &self.loaded
    }
}

fn rule_plugins(
    loaded_plugins: &[LoadedWorkspacePlugin],
) -> std::io::Result<Arc<Vec<Arc<dyn RulePlugin>>>> {
    let mut plugins: Vec<Arc<dyn RulePlugin>> = Vec::new();
    for loaded in loaded_plugins {
        if loaded.manifest.rule_kinds.is_empty() {
            continue;
        }
        let rule_plugin = match loaded.library.as_ref() {
            Some(library) => AbiRulePlugin::with_library(
                loaded.plugin,
                loaded.manifest.rule_kinds.clone(),
                loaded.manifest.label_fields.clone(),
                library.clone(),
            )?,
            None => AbiRulePlugin::new(
                loaded.plugin,
                loaded.manifest.rule_kinds.clone(),
                loaded.manifest.label_fields.clone(),
            )?,
        };
        plugins.push(Arc::new(rule_plugin));
    }
    Ok(Arc::new(plugins))
}

fn add_plugin_resolvers_and_planners(
    executor: &mut Executor,
    loaded_plugins: &[LoadedWorkspacePlugin],
) -> std::io::Result<()> {
    for loaded in loaded_plugins {
        for ecosystem in &loaded.manifest.dependency_ecosystems {
            let planner = match loaded.library.as_ref() {
                Some(library) => AbiDependencyPlanner::with_library(
                    loaded.plugin,
                    ecosystem.clone(),
                    library.clone(),
                )?,
                None => AbiDependencyPlanner::new(loaded.plugin, ecosystem.clone())?,
            };
            executor.add_dependency_planner_plugin(Box::new(planner));
        }

        if !loaded.manifest.target_prefixes.is_empty() {
            let resolver = match loaded.library.as_ref() {
                Some(library) => AbiResolverPlugin::with_library(
                    loaded.plugin,
                    loaded.manifest.target_prefixes.clone(),
                    library.clone(),
                )?,
                None => {
                    AbiResolverPlugin::new(loaded.plugin, loaded.manifest.target_prefixes.clone())?
                }
            };
            executor.add_resolver_plugin(Box::new(resolver));
        }
    }
    Ok(())
}

impl ResolverPlugin for WorkspaceResolver {
    fn can_resolve(&self, target: &str) -> bool {
        target.starts_with("//") || target.starts_with(':')
    }

    fn resolve(&self, _context: Context, target: &str) -> std::io::Result<Config> {
        let label = parse_label(target, &self.current_package)?;
        let package_dir = self.root.join(&label.package);
        validate_workspace_relative(&self.root, &package_dir)?;
        let build_file = build_file(&package_dir).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no BUILD.ccl found in {}", package_dir.display()),
            )
        })?;
        let table = load_build_table(&self.root, &build_file)?;

        let rule_context = RuleContext {
            workspace_root: self.root.clone(),
            package: label.package.clone(),
            package_dir: package_dir.clone(),
        };
        for plugin in self.rule_plugins.iter() {
            for kind in plugin.rule_kinds() {
                if let Some(target_table) = find_named_target(&table, kind, &label.name)? {
                    return plugin.config_from_target(&rule_context, kind, target_table);
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("target {target} not found in {}", build_file.display()),
        ))
    }
}

pub fn build_from_current_workspace(targets: &[String]) -> std::io::Result<BuildResult> {
    Ok(build_targets_from_current_workspace(targets)?.result)
}

pub fn build_targets_from_current_workspace(
    targets: &[String],
) -> std::io::Result<BuildInvocationResult> {
    let cwd = std::env::current_dir()?;
    let workspace = Workspace::load_from(&cwd)?;
    let targets = workspace.expand_target_patterns(targets)?;
    eprintln!("[cbs] expanded to {} target(s)", targets.len());
    let mut executor = workspace.executor()?;
    let roots: Vec<_> = targets
        .iter()
        .map(|target| executor.add_task(target, None))
        .collect();
    Ok(BuildInvocationResult {
        targets,
        result: executor.run(&roots),
    })
}

pub fn build_tests_from_current_workspace(
    targets: &[String],
) -> std::io::Result<BuildInvocationResult> {
    let cwd = std::env::current_dir()?;
    let workspace = Workspace::load_from(&cwd)?;
    let targets = workspace.expand_test_patterns(targets)?;
    eprintln!("[cbs] expanded to {} test target(s)", targets.len());
    let mut executor = workspace.executor()?;
    let roots: Vec<_> = targets
        .iter()
        .map(|target| executor.add_task(target, None))
        .collect();
    Ok(BuildInvocationResult {
        targets,
        result: executor.run(&roots),
    })
}

impl RuleContext {
    pub fn source_paths(&self, target: &ConfigTable, key: &str) -> std::io::Result<Vec<String>> {
        string_list(target, key)?
            .into_iter()
            .map(|src| package_path(&self.workspace_root, &self.package_dir, &src))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|path| Ok(path.to_string_lossy().to_string()))
            .collect()
    }

    pub fn optional_source_path(
        &self,
        target: &ConfigTable,
        key: &str,
    ) -> std::io::Result<Option<String>> {
        target
            .get(key)
            .and_then(|value| value.as_str())
            .map(|src| {
                package_path(&self.workspace_root, &self.package_dir, src)
                    .map(|path| path.to_string_lossy().to_string())
            })
            .transpose()
    }

    pub fn label_list(&self, target: &ConfigTable, key: &str) -> std::io::Result<Vec<String>> {
        string_list(target, key)?
            .into_iter()
            .map(|dep| parse_label(&dep, &self.package).map(|label| canonical_label(&label)))
            .collect()
    }

    pub fn optional_label(
        &self,
        target: &ConfigTable,
        key: &str,
    ) -> std::io::Result<Option<String>> {
        target
            .get(key)
            .and_then(|value| value.as_str())
            .map(|value| parse_label(value, &self.package).map(|label| canonical_label(&label)))
            .transpose()
    }

    pub fn cargo_requirements(
        &self,
        target: &ConfigTable,
    ) -> std::io::Result<Vec<ExternalRequirement>> {
        cargo_requirements(target, &self.package)
    }

    pub fn required_string(&self, target: &ConfigTable, key: &str) -> std::io::Result<String> {
        required_string(target, key)
    }
}

fn cargo_requirements(
    target: &ConfigTable,
    package: &str,
) -> std::io::Result<Vec<ExternalRequirement>> {
    let Some(value) = target.get("cargo_deps") else {
        return Ok(Vec::new());
    };
    let deps = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cargo_deps must be an array of tables",
        )
    })?;
    deps.iter()
        .map(|dep| {
            let table = dep.as_table().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "cargo_deps entries must be tables",
                )
            })?;
            let package_name = required_string(table, "package")?;
            let version = required_string(table, "version")?;
            Ok(ExternalRequirement {
                ecosystem: "cargo".to_string(),
                package: package_name.clone(),
                version,
                features: string_list(table, "features")?,
                default_features: table
                    .get("default_features")
                    .or_else(|| table.get("default-features"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true),
                target: table
                    .get("target")
                    .and_then(|value| value.as_str())
                    .map(|target| parse_label_or_external(target, package))
                    .transpose()?
                    .or_else(|| Some(format!("cargo://{package_name}"))),
            })
        })
        .collect()
}

fn find_named_target<'a>(
    table: &'a ConfigTable,
    section: &str,
    name: &str,
) -> std::io::Result<Option<&'a ConfigTable>> {
    let Some(value) = table.get(section) else {
        return Ok(None);
    };
    let targets = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{section} must be an array of tables"),
        )
    })?;
    for target in targets {
        let target = target.as_table().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{section} entries must be tables"),
            )
        })?;
        if target.get("name").and_then(|value| value.as_str()) == Some(name) {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

fn collect_package_targets(
    root: &Path,
    dir: &Path,
    rule_kinds: &[String],
    labels: &mut Vec<String>,
) -> std::io::Result<()> {
    if let Some(build_file) = build_file(dir) {
        let package = dir
            .strip_prefix(root)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "{} is not inside workspace {}",
                        dir.display(),
                        root.display()
                    ),
                )
            })?
            .to_string_lossy()
            .trim_matches('/')
            .to_string();
        let table = load_build_table(root, &build_file)?;
        for kind in rule_kinds {
            for target in target_tables(&table, kind)? {
                let name = required_string(target, "name")?;
                labels.push(canonical_label(&Label {
                    package: package.clone(),
                    name,
                }));
            }
        }
    }

    let mut children: Vec<_> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.is_dir() && !is_hidden_path(path))
        .collect();
    children.sort();
    for child in children {
        collect_package_targets(root, &child, rule_kinds, labels)?;
    }
    Ok(())
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn target_tables<'a>(
    table: &'a ConfigTable,
    section: &str,
) -> std::io::Result<Vec<&'a ConfigTable>> {
    let Some(value) = table.get(section) else {
        return Ok(Vec::new());
    };
    let targets = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{section} must be an array of tables"),
        )
    })?;
    targets
        .iter()
        .map(|target| {
            target.as_table().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{section} entries must be tables"),
                )
            })
        })
        .collect()
}

fn load_workspace_config(root: &Path, workspace_file: &Path) -> std::io::Result<WorkspaceConfig> {
    let table = load_workspace_table(root, workspace_file)?;
    let cache_dir = table
        .get("workspace")
        .and_then(|value| value.as_table())
        .and_then(|workspace| workspace.get("cache_dir"))
        .and_then(|value| value.as_str())
        .map(|cache_dir| root.join(cache_dir))
        .unwrap_or_else(|| root.join(".cbs").join("cache"));
    let target_config = target_config(&table);
    let plugins = workspace_plugins(root, &table)?;
    let mut tools = workspace_tools(root, &table)?;
    let mut platform_fingerprints = platform_requirement_fingerprints(root, &table)?;
    let plugin_init = plugin_workspace_tools(root, &cache_dir, &target_config, &plugins)?;
    for tool in plugin_init.tools {
        insert_workspace_tool(&mut tools, tool)?;
    }
    platform_fingerprints.extend(plugin_init.fingerprints);
    let rustc_ref = table
        .get("toolchain")
        .and_then(|value| value.as_table())
        .and_then(|toolchain| toolchain.get("rust"))
        .and_then(|value| value.as_table())
        .and_then(|rust| rust.get("rustc"))
        .and_then(|value| value.as_str())
        .map(|rustc| rustc.to_string())
        .or_else(|| std::env::var("RUSTC").ok())
        .unwrap_or_else(|| "rustc".to_string());
    let rustc = resolve_tool_or_path(root, &tools, &rustc_ref);

    Ok(WorkspaceConfig {
        cache_dir,
        rustc,
        tool_fingerprints: tool_fingerprints(&tools, platform_fingerprints),
        tools,
        target_config,
        plugins,
    })
}

struct PluginWorkspaceTools {
    tools: Vec<WorkspaceToolConfig>,
    fingerprints: Vec<(String, String)>,
}

fn plugin_workspace_tools(
    root: &Path,
    cache_dir: &Path,
    target_config: &[(BuildConfigKey, String)],
    plugins: &[WorkspacePluginConfig],
) -> std::io::Result<PluginWorkspaceTools> {
    let mut tools = Vec::new();
    let mut fingerprints = Vec::new();
    let sdk_target_config: HashMap<u32, String> = target_config
        .iter()
        .map(|(key, value)| (build_config_key_to_sdk(*key), value.clone()))
        .collect();
    for plugin in plugins {
        let loaded = load_workspace_dynamic_or_test_plugin(&plugin.path, &plugin.name).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "failed to initialize workspace plugin {} at {}: {e}",
                    plugin.name,
                    plugin.path.display()
                ),
            )
        })?;
        if loaded.manifest.name != plugin.name {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "workspace plugin {} loaded manifest for {}",
                    plugin.name, loaded.manifest.name
                ),
            ));
        }
        let request = cbs_plugin_sdk::PluginInitRequest {
            name: plugin.name.clone(),
            workspace_root: root.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
            target_config: sdk_target_config.clone(),
            parameters: plugin.parameters.clone(),
        };
        match initialize_plugin(loaded.plugin, &request)? {
            cbs_plugin_sdk::PluginInitResponse::Success(init) => {
                tools.extend(init.tools.into_iter().map(|tool| WorkspaceToolConfig {
                    name: tool.name,
                    kind: tool.kind,
                    path: tool.path,
                    sha256: None,
                    fingerprint: tool.fingerprint,
                }));
                fingerprints.extend(init.fingerprints);
            }
            cbs_plugin_sdk::PluginInitResponse::Failure(error) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("workspace plugin {} initialization failed: {error}", plugin.name),
                ))
            }
        }
    }
    Ok(PluginWorkspaceTools {
        tools,
        fingerprints,
    })
}

fn workspace_tools(
    root: &Path,
    table: &ConfigTable,
) -> std::io::Result<HashMap<String, WorkspaceToolConfig>> {
    let Some(value) = table.get("tools") else {
        return Ok(HashMap::new());
    };
    let tools = value.as_table().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "tools must be a table")
    })?;
    let mut resolved = HashMap::new();
    for (name, value) in tools {
        let tool = value.as_table().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tools.{name} must be a table"),
            )
        })?;
        if name == current_os() && !is_tool_declaration(tool) {
            for (nested_name, nested_value) in tool {
                let nested_tool = nested_value.as_table().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("tools.{name}.{nested_name} must be a table"),
                    )
                })?;
                add_workspace_tool(root, nested_name, nested_tool, &mut resolved)?;
            }
        } else {
            add_workspace_tool(root, name, tool, &mut resolved)?;
        }
    }
    Ok(resolved)
}

fn is_tool_declaration(tool: &ConfigTable) -> bool {
    ["_type", "type", "path", "program", "xcode_tool", "tool", "root", "provider"]
        .iter()
        .any(|key| tool.contains_key(*key))
}

fn add_workspace_tool(
    root: &Path,
    name: &str,
    tool: &ConfigTable,
    resolved: &mut HashMap<String, WorkspaceToolConfig>,
) -> std::io::Result<()> {
    let kind = tool
        .get("_type")
        .or_else(|| tool.get("type"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tools.{name} must specify _type"),
            )
        })?;
    match kind {
        "rust_toolchain" => {
            let toolchain = rust_toolchain_tools(root, name, tool)?;
            for tool in toolchain {
                insert_workspace_tool(resolved, tool)?;
            }
        }
        "xcode_tool" => {
            let path = declared_tool_path(root, name, tool)?;
            let sha256 = tool
                .get("sha256")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            let fingerprint = declared_tool_fingerprint(name, kind, &path, sha256.as_deref())?;
            insert_workspace_tool(
                resolved,
                WorkspaceToolConfig {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    path,
                    sha256,
                    fingerprint,
                },
            )?;
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tools.{name} has unsupported type {kind:?}"),
            ))
        }
    }
    Ok(())
}

fn insert_workspace_tool(
    resolved: &mut HashMap<String, WorkspaceToolConfig>,
    tool: WorkspaceToolConfig,
) -> std::io::Result<()> {
    if resolved.contains_key(&tool.name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("tool {:?} is declared more than once", tool.name),
        ));
    }
    resolved.insert(tool.name.clone(), tool);
    Ok(())
}

fn resolve_tool_or_path(
    root: &Path,
    tools: &HashMap<String, WorkspaceToolConfig>,
    value: &str,
) -> String {
    tools
        .get(value)
        .map(|tool| tool.path.to_string_lossy().to_string())
        .unwrap_or_else(|| root_relative(root, value))
}

fn declared_tool_path(root: &Path, name: &str, tool: &ConfigTable) -> std::io::Result<PathBuf> {
    if let Some(path) = tool.get("path").and_then(|value| value.as_str()) {
        return Ok(root_relative_tool_path(root, path));
    }
    if let Some(xcode_tool) = tool
        .get("xcode_tool")
        .or_else(|| tool.get("tool"))
        .and_then(|value| value.as_str())
    {
        if current_os() == "macos" {
            return xcrun_find(xcode_tool).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to resolve Xcode tool {xcode_tool:?} for tools.{name}; install Xcode or Command Line Tools: {e}"
                    ),
                )
            });
        }
    }
    if let Some(program) = tool.get("program").and_then(|value| value.as_str()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("tools.{name} uses program = {program:?}, but generic host program search is no longer supported; use an explicit toolchain type or path"),
        ));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("tools.{name} must specify path or a platform-specific tool field"),
    ))
}

fn rust_toolchain_tools(
    root: &Path,
    name: &str,
    toolchain: &ConfigTable,
) -> std::io::Result<Vec<WorkspaceToolConfig>> {
    let root = rust_toolchain_root(root, name, toolchain)?;
    let rustc = root.join("bin").join(executable_name("rustc"));
    let cargo = root.join("bin").join(executable_name("cargo"));
    if !rustc.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "rust toolchain {name:?} does not contain rustc at {}",
                rustc.display()
            ),
        ));
    }
    if !cargo.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "rust toolchain {name:?} does not contain cargo at {}",
                cargo.display()
            ),
        ));
    }
    validate_rust_toolchain(name, toolchain, &rustc)?;

    let metadata = rust_toolchain_metadata(name, toolchain)?;
    Ok(vec![
        rust_toolchain_tool("rustc", &rustc, &metadata)?,
        rust_toolchain_tool("cargo", &cargo, &metadata)?,
    ])
}

fn rust_toolchain_root(
    root: &Path,
    name: &str,
    toolchain: &ConfigTable,
) -> std::io::Result<PathBuf> {
    if let Some(path) = toolchain.get("root").and_then(|value| value.as_str()) {
        return Ok(root_relative_tool_path(root, path));
    }
    if let Some(env) = toolchain.get("root_env").and_then(|value| value.as_str()) {
        if let Some(path) = std::env::var_os(env) {
            return Ok(PathBuf::from(path));
        }
    }
    match toolchain
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("rustup")
    {
        "rustup" => rustup_sysroot().map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("failed to resolve rust_toolchain {name:?} from rustup/current rustc: {e}"),
            )
        }),
        provider => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("rust_toolchain {name:?} has unsupported provider {provider:?}"),
        )),
    }
}

fn rustup_sysroot() -> std::io::Result<PathBuf> {
    let output = std::process::Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "rustc --print sysroot failed",
        ));
    }
    let sysroot = String::from_utf8(output.stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(PathBuf::from(sysroot.trim()))
}

fn rust_toolchain_tool(
    name: &str,
    path: &Path,
    metadata: &str,
) -> std::io::Result<WorkspaceToolConfig> {
    let fingerprint = declared_tool_fingerprint(name, "rust_toolchain", path, None)?;
    Ok(WorkspaceToolConfig {
        name: name.to_string(),
        kind: "rust_toolchain".to_string(),
        path: path.to_path_buf(),
        sha256: None,
        fingerprint: format!("{fingerprint};{metadata}"),
    })
}

fn rust_toolchain_metadata(name: &str, toolchain: &ConfigTable) -> std::io::Result<String> {
    let version = toolchain
        .get("version")
        .and_then(|value| value.as_str())
        .unwrap_or("unspecified");
    let host = toolchain
        .get("host")
        .and_then(|value| value.as_str())
        .map(|host| host.to_string())
        .unwrap_or_else(current_rust_host_triple);
    let dist_hash = if let Some(dist) = toolchain.get("dist").and_then(|value| value.as_table()) {
        let key = dist_key(&host);
        let dist = dist.get(&key).and_then(|value| value.as_table()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("rust_toolchain {name:?} has no dist entry for host {host:?} (expected key {key})"),
            )
        })?;
        dist.get("sha256")
            .or_else(|| dist.get("hash"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("rust_toolchain {name:?} dist entry for {host:?} must include sha256"),
                )
            })?
            .to_string()
    } else {
        "unspecified".to_string()
    };
    Ok(format!(
        "rust-toolchain:version={version}:host={host}:dist-sha256={dist_hash}"
    ))
}

fn current_rust_host_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        os => format!("{arch}-unknown-{os}"),
    }
}

fn dist_key(host: &str) -> String {
    host.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn validate_rust_toolchain(
    name: &str,
    toolchain: &ConfigTable,
    rustc: &Path,
) -> std::io::Result<()> {
    let output = std::process::Command::new(rustc).arg("-Vv").output()?;
    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("rust_toolchain {name:?} failed to run {} -Vv", rustc.display()),
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(expected) = toolchain.get("version").and_then(|value| value.as_str()) {
        let release = rustc_version_field(&stdout, "release").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("rust_toolchain {name:?} rustc -Vv output did not include a release"),
            )
        })?;
        if release != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("rust_toolchain {name:?} expected Rust {expected}, found {release}"),
            ));
        }
    }
    if let Some(expected) = toolchain.get("host").and_then(|value| value.as_str()) {
        let host = rustc_version_field(&stdout, "host").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("rust_toolchain {name:?} rustc -Vv output did not include a host"),
            )
        })?;
        if host != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("rust_toolchain {name:?} expected host {expected}, found {host}"),
            ));
        }
    }
    Ok(())
}

fn rustc_version_field<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(": "))
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn platform_requirement_fingerprints(
    root: &Path,
    table: &ConfigTable,
) -> std::io::Result<Vec<(String, String)>> {
    let Some(value) = table.get("platform_requirements") else {
        return Ok(Vec::new());
    };
    let requirements = value.as_table().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "platform_requirements must be a table",
        )
    })?;
    let mut active = Vec::new();
    for (name, value) in requirements {
        let requirement = value.as_table().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("platform_requirements.{name} must be a table"),
            )
        })?;
        if name == current_os() {
            for (nested_name, nested_value) in requirement {
                let nested_requirement = nested_value.as_table().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("platform_requirements.{name}.{nested_name} must be a table"),
                    )
                })?;
                active.push((
                    Some(name.as_str()),
                    nested_name.as_str(),
                    nested_requirement,
                ));
            }
        } else if is_platform_requirement(requirement) {
            active.push((None, name.as_str(), requirement));
        }
    }

    active
        .into_iter()
        .map(|(platform_scope, name, requirement)| {
            platform_requirement_fingerprint(root, platform_scope, name, requirement)
        })
        .filter_map(|result| result.transpose())
        .collect()
}

fn is_platform_requirement(requirement: &ConfigTable) -> bool {
    [
        "_type",
        "type",
        "platform",
        "path",
        "program",
        "xcode_tool",
        "tool",
    ]
        .iter()
        .any(|key| requirement.contains_key(*key))
}

fn platform_requirement_fingerprint(
    root: &Path,
    platform_scope: Option<&str>,
    name: &str,
    requirement: &ConfigTable,
) -> std::io::Result<Option<(String, String)>> {
    if platform_scope.is_some_and(|platform| platform != current_os()) {
        return Ok(None);
    }

    let kind = requirement
        .get("_type")
        .or_else(|| requirement.get("type"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("platform requirement {name} must specify _type"),
            )
        })?;
    let platform = requirement
        .get("platform")
        .and_then(|value| value.as_str())
        .unwrap_or(match kind {
            "xcode_tool" => "macos",
            _ => "",
        });
    if !platform.is_empty() && platform != current_os() {
        return Ok(None);
    }

    let fingerprint = match kind {
        "xcode_tool" => {
            let path = declared_tool_path(root, name, requirement)?;
            let sha256 = requirement.get("sha256").and_then(|value| value.as_str());
            let mut fingerprint = declared_tool_fingerprint(name, kind, &path, sha256)?;
            if requires_macos_sdk(requirement) {
                let xcode_fingerprint = macos_xcode_fingerprint(
                    name,
                    &optional_requirement_path(root, requirement, "developer_dir")?
                        .unwrap_or_else(detect_xcode_developer_dir),
                    &optional_requirement_path(root, requirement, "sdk_path")?
                        .unwrap_or_else(detect_macos_sdk_path),
                )?;
                fingerprint.push(';');
                fingerprint.push_str(&xcode_fingerprint);
            }
            fingerprint
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "platform requirement {name} has unsupported type {kind:?}; expected xcode_tool"
                ),
            ))
        }
    };

    let key = platform_scope
        .map(|platform| format!("platform_requirement:{platform}:{name}"))
        .unwrap_or_else(|| format!("platform_requirement:{name}"));
    Ok(Some((key, fingerprint)))
}

fn requires_macos_sdk(requirement: &ConfigTable) -> bool {
    current_os() == "macos"
        && (requirement.get("xcode_tool").is_some()
            || requirement
                .get("sdk")
                .and_then(|value| value.as_str())
                .is_some_and(|sdk| sdk == "macos"))
}

fn optional_requirement_path(
    root: &Path,
    requirement: &ConfigTable,
    key: &str,
) -> std::io::Result<Option<PathBuf>> {
    requirement
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| Ok(root_relative_path(root, value)))
        .transpose()
}

fn macos_xcode_fingerprint(
    name: &str,
    developer_dir: &Path,
    sdk_path: &Path,
) -> std::io::Result<String> {
    if !developer_dir.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "macOS Xcode requirement {name} is missing developer_dir {}. Install Xcode or Command Line Tools and update WORKSPACE.ccl.",
                developer_dir.display()
            ),
        ));
    }
    if !sdk_path.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "macOS Xcode requirement {name} is missing sdk_path {}. Install the macOS SDK and update WORKSPACE.ccl.",
                sdk_path.display()
            ),
        ));
    }

    let clang = developer_dir
        .join("Toolchains")
        .join("XcodeDefault.xctoolchain")
        .join("usr")
        .join("bin")
        .join("clang");
    if !clang.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "macOS Xcode requirement {name} is missing clang at {}",
                clang.display()
            ),
        ));
    }
    let target_conditionals = sdk_path.join("usr").join("include").join("TargetConditionals.h");
    if !target_conditionals.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "macOS Xcode requirement {name} is missing SDK header {}",
                target_conditionals.display()
            ),
        ));
    }

    let target_conditionals_sha = file_sha256(&target_conditionals)?;
    Ok(format!(
        "macos_xcode:developer_dir={}:sdk_path={}:TargetConditionals.h={target_conditionals_sha}",
        developer_dir.to_string_lossy(),
        sdk_path.to_string_lossy()
    ))
}

fn tool_fingerprints(
    tools: &HashMap<String, WorkspaceToolConfig>,
    platform_fingerprints: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut fingerprints: Vec<_> = tools
        .iter()
        .map(|(name, tool)| (name.clone(), tool.fingerprint.clone()))
        .collect();
    fingerprints.extend(platform_fingerprints);
    fingerprints.sort_by(|a, b| a.0.cmp(&b.0));
    fingerprints
}

fn context_tools(tools: &HashMap<String, WorkspaceToolConfig>) -> HashMap<String, Tool> {
    tools
        .iter()
        .map(|(name, tool)| {
            (
                name.clone(),
                Tool {
                    path: tool.path.clone(),
                    fingerprint: tool.fingerprint.clone(),
                },
            )
        })
        .collect()
}

fn declared_tool_fingerprint(
    name: &str,
    kind: &str,
    path: &Path,
    expected_sha256: Option<&str>,
) -> std::io::Result<String> {
    let actual_sha256 = if path.exists() && path.is_file() {
        Some(file_sha256(path)?)
    } else {
        None
    };
    if let (Some(expected), Some(actual)) = (expected_sha256, actual_sha256.as_deref()) {
        if !expected.eq_ignore_ascii_case(actual) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "sha256 mismatch for tool {name} at {}: expected {expected}, got {actual}",
                    path.display()
                ),
            ));
        }
    }
    if expected_sha256.is_some() && actual_sha256.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "tool {name} at {} is not a readable file, cannot verify sha256",
                path.display()
            ),
        ));
    }
    let digest = actual_sha256
        .as_deref()
        .or(expected_sha256)
        .unwrap_or("unverified");
    Ok(format!(
        "{kind}:{}:sha256={digest}",
        path.to_string_lossy()
    ))
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn workspace_plugins(
    root: &Path,
    table: &ConfigTable,
) -> std::io::Result<Vec<WorkspacePluginConfig>> {
    let Some(value) = table.get("plugins") else {
        return Ok(Vec::new());
    };
    let plugins = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "plugins must be an array of tables",
        )
    })?;
    plugins
        .iter()
        .map(|plugin| {
            let plugin = plugin.as_table().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "plugins entries must be tables",
                )
            })?;
            Ok(WorkspacePluginConfig {
                name: required_string(plugin, "name")?,
                path: root_relative_path(root, &required_string(plugin, "path")?),
                parameters: plugin_parameters(plugin)?,
            })
        })
        .collect()
}

fn plugin_parameters(plugin: &ConfigTable) -> std::io::Result<HashMap<String, String>> {
    let Some(parameters) = plugin.get("parameters") else {
        return Ok(HashMap::new());
    };
    let parameters = parameters.as_table().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "plugin parameters must be a table",
        )
    })?;
    parameters
        .iter()
        .map(|(key, value)| {
            let value = value.as_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("plugin parameter {key} must be a string"),
                )
            })?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

fn detect_xcode_developer_dir() -> PathBuf {
    command_output_path("/usr/bin/xcode-select", &["-p"]).unwrap_or_else(|| {
        PathBuf::from("/Applications/Xcode.app").join("Contents").join("Developer")
    })
}

fn detect_macos_sdk_path() -> PathBuf {
    command_output_path("/usr/bin/xcrun", &["--show-sdk-path"]).unwrap_or_else(|| {
        PathBuf::from("/Library")
            .join("Developer")
            .join("CommandLineTools")
            .join("SDKs")
            .join("MacOSX.sdk")
    })
}

fn xcrun_find(tool: &str) -> std::io::Result<PathBuf> {
    command_output_path("/usr/bin/xcrun", &["--find", tool]).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("xcrun could not find {tool}"),
        )
    })
}

fn command_output_path(program: &str, args: &[&str]) -> Option<PathBuf> {
    let output = std::process::Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let path = stdout.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn current_os() -> &'static str {
    std::env::consts::OS
}

fn target_config(table: &ConfigTable) -> Vec<(BuildConfigKey, String)> {
    let target = table.get("target").and_then(|value| value.as_table());
    let os = target
        .and_then(|target| target.get("os"))
        .and_then(|value| value.as_str())
        .unwrap_or(current_os());
    let family = target
        .and_then(|target| target.get("family"))
        .and_then(|value| value.as_str())
        .unwrap_or(if cfg!(windows) { "windows" } else { "unix" });
    let arch = target
        .and_then(|target| target.get("arch"))
        .and_then(|value| value.as_str())
        .unwrap_or(std::env::consts::ARCH);
    let vendor_default = match os {
        "macos" | "ios" | "tvos" | "visionos" | "watchos" => "apple",
        "windows" => "pc",
        _ => "unknown",
    };
    let env_default = match os {
        "linux" => "gnu",
        "windows" => "msvc",
        _ => "",
    };
    let vendor = target
        .and_then(|target| target.get("vendor"))
        .and_then(|value| value.as_str())
        .unwrap_or(vendor_default);
    let env = target
        .and_then(|target| target.get("env"))
        .and_then(|value| value.as_str())
        .unwrap_or(env_default);
    let endian = target
        .and_then(|target| target.get("endian"))
        .and_then(|value| value.as_str())
        .unwrap_or(if cfg!(target_endian = "little") {
            "little"
        } else {
            "big"
        });

    let mut config = vec![
        (BuildConfigKey::TargetFamily, family.to_string()),
        (BuildConfigKey::TargetOS, os.to_string()),
        (BuildConfigKey::TargetEnv, env.to_string()),
        (BuildConfigKey::TargetArch, arch.to_string()),
        (BuildConfigKey::TargetVendor, vendor.to_string()),
        (BuildConfigKey::TargetEndian, endian.to_string()),
    ];
    if os == "macos" {
        config.extend([
            (
                BuildConfigKey::MacosDeveloperDir,
                detect_xcode_developer_dir().to_string_lossy().to_string(),
            ),
            (
                BuildConfigKey::MacosSdkPath,
                detect_macos_sdk_path().to_string_lossy().to_string(),
            ),
        ]);
    }
    config
}

fn parse_label_or_external(value: &str, current_package: &str) -> std::io::Result<String> {
    if value.starts_with("//") || value.starts_with(':') {
        return parse_label(value, current_package).map(|label| canonical_label(&label));
    }
    Ok(value.to_string())
}

fn parse_label(value: &str, current_package: &str) -> std::io::Result<Label> {
    let (package, name) = if let Some(rest) = value.strip_prefix("//") {
        rest.split_once(':').ok_or_else(|| invalid_label(value))?
    } else if let Some(name) = value.strip_prefix(':') {
        (current_package, name)
    } else {
        return Err(invalid_label(value));
    };
    if name.is_empty() || package.split('/').any(|part| part == "..") || package.starts_with('/') {
        return Err(invalid_label(value));
    }
    Ok(Label {
        package: package.trim_matches('/').to_string(),
        name: name.to_string(),
    })
}

fn recursive_package_pattern(value: &str) -> std::io::Result<Option<String>> {
    let Some(rest) = value.strip_prefix("//") else {
        return Ok(None);
    };
    let package = if rest == "..." {
        ""
    } else {
        let Some(package) = rest.strip_suffix("/...") else {
            return Ok(None);
        };
        package
    };
    if package.contains(':')
        || package.split('/').any(|part| part == "..")
        || package.starts_with('/')
    {
        return Err(invalid_label(value));
    }
    Ok(Some(package.trim_matches('/').to_string()))
}

fn canonical_label(label: &Label) -> String {
    if label.package.is_empty() {
        format!("//:{}", label.name)
    } else {
        format!("//{}:{}", label.package, label.name)
    }
}

fn package_path(root: &Path, package_dir: &Path, path: &str) -> std::io::Result<PathBuf> {
    if path.starts_with('/')
        || Path::new(path)
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("workspace paths must be package-relative: {path}"),
        ));
    }
    let resolved = package_dir.join(path);
    validate_workspace_relative(root, &resolved)?;
    Ok(resolved)
}

fn validate_workspace_relative(root: &Path, path: &Path) -> std::io::Result<()> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path escapes workspace: {}", path.display()),
        ));
    }
    if path.is_absolute() && !path.starts_with(root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path escapes workspace: {}", path.display()),
        ));
    }
    Ok(())
}

fn string_list(table: &ConfigTable, key: &str) -> std::io::Result<Vec<String>> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{key} must be an array of strings"),
        )
    })?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(|value| value.to_string())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{key} must be an array of strings"),
                    )
                })
        })
        .collect()
}

fn required_string(table: &ConfigTable, key: &str) -> std::io::Result<String> {
    table
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing required string field {key}"),
            )
        })
}

fn find_workspace_root(cwd: &Path) -> std::io::Result<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        if workspace_file(&dir).is_some() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no WORKSPACE.ccl found in current directory or any parent",
            ));
        }
    }
}

fn workspace_file(root: &Path) -> Option<PathBuf> {
    let path = root.join("WORKSPACE.ccl");
    path.exists().then_some(path)
}

fn build_file(package_dir: &Path) -> Option<PathBuf> {
    let path = package_dir.join("BUILD.ccl");
    path.exists().then_some(path)
}

fn package_for_cwd(root: &Path, cwd: &Path) -> std::io::Result<String> {
    let mut package_dir = cwd.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not inside workspace {}",
                cwd.display(),
                root.display()
            ),
        )
    })?;
    loop {
        if build_file(&root.join(package_dir)).is_some() || package_dir.as_os_str().is_empty() {
            return Ok(package_dir.to_string_lossy().trim_matches('/').to_string());
        }
        package_dir = package_dir.parent().unwrap_or_else(|| Path::new(""));
    }
}

fn root_relative(root: &Path, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() || path.components().count() == 1 {
        path.to_string_lossy().to_string()
    } else {
        root.join(path).to_string_lossy().to_string()
    }
}

fn root_relative_path(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn root_relative_tool_path(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() || path.components().count() == 1 {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn invalid_label(label: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("invalid label {label}; expected //package:target or :target"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_test_workspace_targets_build() {
        for workspace_root in test_workspace_roots() {
            let labels = workspace_labels(&workspace_root);
            assert!(
                !labels.is_empty(),
                "workspace {} should declare targets",
                workspace_root.display()
            );

            let workspace = Workspace::load_from(&workspace_root).unwrap();
            let mut executor = workspace.executor().unwrap();
            let roots: Vec<_> = labels
                .iter()
                .map(|label| executor.add_task(label, None))
                .collect();
            let result = executor.run(&roots);
            let BuildResult::Success(output) = result else {
                panic!(
                    "workspace {} failed to build {:?}: {result:?}",
                    workspace_root.display(),
                    labels
                );
            };
            assert_eq!(
                output.outputs.len(),
                labels.len(),
                "workspace {} should emit one output per root target",
                workspace_root.display()
            );
        }
    }

    #[test]
    fn expands_explicit_targets_and_recursive_patterns() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_workspaces")
            .join("rand");
        let workspace = Workspace::load_from(&workspace_root).unwrap();

        assert_eq!(
            workspace
                .expand_target_patterns(&vec![
                    "//app:rand_example".to_string(),
                    "//app/...".to_string(),
                    "//app:rand_example".to_string(),
                ])
                .unwrap(),
            vec!["//app:rand_example".to_string()]
        );
    }

    #[test]
    fn expands_workspace_recursive_pattern() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_workspaces")
            .join("rand");
        let workspace = Workspace::load_from(&workspace_root).unwrap();

        assert_eq!(
            workspace
                .expand_target_patterns(&vec!["//...".to_string()])
                .unwrap(),
            vec!["//app:rand_example".to_string()]
        );
    }

    #[test]
    fn test_expansion_only_returns_test_targets() {
        let workspace_root = test_workspace_with_tests();
        let workspace = Workspace::load_from(&workspace_root).unwrap();

        assert_eq!(
            workspace
                .expand_test_patterns(&vec!["//app/...".to_string()])
                .unwrap(),
            vec!["//app:unit".to_string()]
        );
        assert_eq!(
            workspace
                .expand_test_patterns(&vec!["//app:app".to_string(), "//app:lib".to_string()])
                .unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            workspace
                .expand_test_patterns(&vec!["//app:unit".to_string()])
                .unwrap(),
            vec!["//app:unit".to_string()]
        );

        std::fs::remove_dir_all(workspace_root).unwrap();
    }

    fn test_workspace_roots() -> Vec<PathBuf> {
        let test_workspaces = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_workspaces");
        let mut roots: Vec<_> = std::fs::read_dir(test_workspaces)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| workspace_file(path).is_some())
            .collect();
        roots.sort();
        roots
    }

    fn workspace_labels(root: &Path) -> Vec<String> {
        let mut labels = Vec::new();
        collect_workspace_labels(root, root, &mut labels);
        labels.sort();
        labels
    }

    fn collect_workspace_labels(root: &Path, dir: &Path, labels: &mut Vec<String>) {
        if let Some(build_file) = build_file(dir) {
            let table = load_build_table(root, &build_file).unwrap();
            let package = dir
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .trim_matches('/')
                .to_string();
            for section in ["rust_binary", "rust_library"] {
                for target in target_tables(&table, section).unwrap() {
                    let name = target
                        .get("name")
                        .and_then(ConfigValue::as_str)
                        .expect("test workspace targets must have names");
                    labels.push(canonical_label(&Label {
                        package: package.clone(),
                        name: name.to_string(),
                    }));
                }
            }
        }

        let mut children: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        for child in children {
            collect_workspace_labels(root, &child, labels);
        }
    }

    fn test_workspace_with_tests() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cbs-workspace-test-expansion-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(
            root.join("WORKSPACE.ccl"),
            "workspace = {\n    cache_dir = \".cbs/cache\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("app").join("BUILD.ccl"),
            r#"
lib = {
    _type = "rust_library"
    srcs = ["lib.rs"]
}

app = {
    _type = "rust_binary"
    srcs = ["main.rs"]
}

unit = {
    _type = "rust_test"
    srcs = ["lib.rs"]
}
"#,
        )
        .unwrap();
        root
    }
}
