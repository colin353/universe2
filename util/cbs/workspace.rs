use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::core::{
    BuildConfigKey, BuildResult, Config, Context, ExternalRequirement, FakeResolver,
    FilesystemBuilder, ResolverPlugin, RuleContext, RulePlugin,
};
use crate::exec::Executor;
use crate::plugin_abi::{
    load_dynamic_plugin, AbiDependencyPlanner, AbiResolverPlugin, AbiRulePlugin, LoadedAbiPlugin,
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
    target_config: Vec<(BuildConfigKey, String)>,
    plugins: Vec<WorkspacePluginConfig>,
}

#[derive(Debug, Clone)]
struct WorkspacePluginConfig {
    name: String,
    path: PathBuf,
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
        );
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
        let build_file = package_dir.join("BUILD.toml");
        if !build_file.exists() {
            return Ok(None);
        }
        let table = std::fs::read_to_string(&build_file)?
            .parse::<toml::Table>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
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
        let mut plugins = vec![self.load_implicit_rust_plugin()?];
        plugins.extend(
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
                .collect::<std::io::Result<Vec<_>>>()?,
        );
        Ok(plugins)
    }

    fn load_implicit_rust_plugin(&self) -> std::io::Result<LoadedWorkspacePlugin> {
        let path = PathBuf::from("/tmp/rust.cdylib");
        let loaded = load_workspace_dynamic_or_test_plugin(&path, "rust")?;
        if loaded.manifest.name != "rust" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "rust plugin path loaded manifest for {}",
                    loaded.manifest.name
                ),
            ));
        }
        Ok(LoadedWorkspacePlugin { path, loaded })
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
        let build_file = package_dir.join("BUILD.toml");
        let table = std::fs::read_to_string(&build_file)?
            .parse::<toml::Table>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

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
    pub fn source_paths(&self, target: &toml::Table, key: &str) -> std::io::Result<Vec<String>> {
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
        target: &toml::Table,
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

    pub fn label_list(&self, target: &toml::Table, key: &str) -> std::io::Result<Vec<String>> {
        string_list(target, key)?
            .into_iter()
            .map(|dep| parse_label(&dep, &self.package).map(|label| canonical_label(&label)))
            .collect()
    }

    pub fn optional_label(
        &self,
        target: &toml::Table,
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
        target: &toml::Table,
    ) -> std::io::Result<Vec<ExternalRequirement>> {
        cargo_requirements(target, &self.package)
    }

    pub fn required_string(&self, target: &toml::Table, key: &str) -> std::io::Result<String> {
        required_string(target, key)
    }
}

fn cargo_requirements(
    target: &toml::Table,
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
    table: &'a toml::Table,
    section: &str,
    name: &str,
) -> std::io::Result<Option<&'a toml::Table>> {
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
    let build_file = dir.join("BUILD.toml");
    if build_file.exists() {
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
        let table = std::fs::read_to_string(&build_file)?
            .parse::<toml::Table>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
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
    table: &'a toml::Table,
    section: &str,
) -> std::io::Result<Vec<&'a toml::Table>> {
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
    let table = std::fs::read_to_string(workspace_file)?
        .parse::<toml::Table>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let rustc = table
        .get("toolchain")
        .and_then(|value| value.as_table())
        .and_then(|toolchain| toolchain.get("rust"))
        .and_then(|value| value.as_table())
        .and_then(|rust| rust.get("rustc"))
        .and_then(|value| value.as_str())
        .map(|rustc| root_relative(root, rustc))
        .unwrap_or_else(|| std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()));
    let cache_dir = table
        .get("workspace")
        .and_then(|value| value.as_table())
        .and_then(|workspace| workspace.get("cache_dir"))
        .and_then(|value| value.as_str())
        .map(|cache_dir| root.join(cache_dir))
        .unwrap_or_else(|| root.join(".cbs").join("cache"));

    Ok(WorkspaceConfig {
        cache_dir,
        rustc,
        target_config: target_config(&table),
        plugins: workspace_plugins(root, &table)?,
    })
}

fn workspace_plugins(
    root: &Path,
    table: &toml::Table,
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
            })
        })
        .collect()
}

fn target_config(table: &toml::Table) -> Vec<(BuildConfigKey, String)> {
    let target = table.get("target").and_then(|value| value.as_table());
    let os = target
        .and_then(|target| target.get("os"))
        .and_then(|value| value.as_str())
        .unwrap_or(std::env::consts::OS);
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

    vec![
        (BuildConfigKey::TargetFamily, family.to_string()),
        (BuildConfigKey::TargetOS, os.to_string()),
        (BuildConfigKey::TargetEnv, env.to_string()),
        (BuildConfigKey::TargetArch, arch.to_string()),
        (BuildConfigKey::TargetVendor, vendor.to_string()),
        (BuildConfigKey::TargetEndian, endian.to_string()),
    ]
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

fn string_list(table: &toml::Table, key: &str) -> std::io::Result<Vec<String>> {
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

fn required_string(table: &toml::Table, key: &str) -> std::io::Result<String> {
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
                "no WORKSPACE.toml found in current directory or any parent",
            ));
        }
    }
}

fn workspace_file(root: &Path) -> Option<PathBuf> {
    ["WORKSPACE.toml", "WORKSPACE"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
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
        if root.join(package_dir).join("BUILD.toml").exists() || package_dir.as_os_str().is_empty()
        {
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
            .filter(|path| path.join("WORKSPACE.toml").exists())
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
        let build_file = dir.join("BUILD.toml");
        if build_file.exists() {
            let table = std::fs::read_to_string(&build_file)
                .unwrap()
                .parse::<toml::Table>()
                .unwrap();
            let package = dir
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .trim_matches('/')
                .to_string();
            for section in ["rust_binary", "rust_library"] {
                for target in table
                    .get(section)
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
                {
                    let name = target
                        .as_table()
                        .and_then(|target| target.get("name"))
                        .and_then(|name| name.as_str())
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
            root.join("WORKSPACE.toml"),
            "[workspace]\ncache_dir = \".cbs/cache\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("app").join("BUILD.toml"),
            r#"
[[rust_library]]
name = "lib"
srcs = ["lib.rs"]

[[rust_binary]]
name = "app"
srcs = ["main.rs"]

[[rust_test]]
name = "unit"
srcs = ["lib.rs"]
"#,
        )
        .unwrap();
        root
    }
}
