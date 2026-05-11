use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::Digest;

use cbs_plugin_sdk::{
    build_config_key, config_extra_keys, Config, DependencyPlan, ExternalRequirement, PluginContext,
};

use super::cargo_recipes;

const RUST_LIBRARY: &str = "rust_library";

#[derive(Debug)]
pub struct CargoResolver {
    locked_dependencies: HashMap<String, HashMap<String, String>>,
    build_recipes: HashMap<String, CargoBuildRecipe>,
}

#[derive(Debug)]
pub struct CargoDependencyPlanner {}

#[derive(Debug, Clone, Default)]
pub struct CargoBuildRecipe {
    pub rustc_cfgs: Vec<String>,
    pub native_static_libs: Vec<CargoNativeStaticLib>,
}

#[derive(Debug, Clone, Default)]
pub struct CargoNativeStaticLib {
    pub name: String,
    pub sources: Vec<String>,
    pub include_dirs: Vec<String>,
    pub flags: Vec<String>,
}

impl CargoResolver {
    pub fn new() -> Self {
        Self {
            locked_dependencies: HashMap::new(),
            build_recipes: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub fn with_build_recipes<I, S>(mut self, recipes: I) -> Self
    where
        I: IntoIterator<Item = (S, CargoBuildRecipe)>,
        S: Into<String>,
    {
        self.build_recipes.extend(
            recipes
                .into_iter()
                .map(|(target, recipe)| (target.into(), recipe)),
        );
        self
    }

    #[allow(dead_code)]
    pub fn with_locked_dependencies<I, S, J, K, V>(mut self, dependencies: I) -> Self
    where
        I: IntoIterator<Item = (S, J)>,
        S: Into<String>,
        J: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.locked_dependencies
            .extend(dependencies.into_iter().map(|(target, deps)| {
                (
                    target.into(),
                    deps.into_iter()
                        .map(|(package, target)| (package.into(), target.into()))
                        .collect(),
                )
            }));
        self
    }

    fn build_recipe(
        &self,
        context: &PluginContext,
        target: &str,
        package: &str,
        version: &str,
    ) -> Option<CargoBuildRecipe> {
        self.build_recipes
            .get(target)
            .or_else(|| self.build_recipes.get(&format!("cargo://{package}")))
            .cloned()
            .or_else(|| cargo_recipes::build_recipe(context, package, version))
    }

    pub fn from_cargo_lock<P: AsRef<std::path::Path>>(
        lockfile: P,
    ) -> std::io::Result<(Self, HashMap<String, String>)> {
        let content = std::fs::read_to_string(lockfile)?;
        let table = content
            .parse::<toml::Table>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let packages = table
            .get("package")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Cargo.lock does not contain packages",
                )
            })?;

        let mut package_counts: HashMap<String, usize> = HashMap::new();
        let mut parsed_packages = Vec::new();
        for package in packages {
            let package = package.as_table().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid package entry")
            })?;
            let name = package
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "package missing name")
                })?
                .to_string();
            let version = package
                .get("version")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "package missing version")
                })?
                .to_string();
            let dependencies = package
                .get("dependencies")
                .and_then(|v| v.as_array())
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| dep.as_str().map(|dep| dep.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            *package_counts.entry(name.clone()).or_default() += 1;
            parsed_packages.push(LockedPackage {
                name,
                version,
                dependencies,
            });
        }

        let mut package_targets: HashMap<(String, String), String> = HashMap::new();
        let mut lock_entries = HashMap::new();
        for package in &parsed_packages {
            let target = cargo_target_name(&package.name, &package.version, &package_counts);
            package_targets.insert(
                (package.name.clone(), package.version.clone()),
                target.clone(),
            );
            lock_entries.insert(target.clone(), package.version.clone());

            let versioned_target = format!("cargo://{}@{}", package.name, package.version);
            lock_entries.insert(versioned_target, package.version.clone());
        }

        let mut locked_dependencies = HashMap::new();
        for package in &parsed_packages {
            let mut deps = HashMap::new();
            for dep in &package.dependencies {
                let (dep_name, dep_version) = parse_lock_dependency(dep);
                let dep_target = match dep_version {
                    Some(version) => package_targets
                        .get(&(dep_name.to_string(), version.to_string()))
                        .cloned(),
                    None => parsed_packages
                        .iter()
                        .filter(|p| p.name == dep_name)
                        .map(|p| (p.name.clone(), p.version.clone()))
                        .next()
                        .and_then(|key| package_targets.get(&key).cloned()),
                };
                if let Some(dep_target) = dep_target {
                    deps.insert(dep_name.to_string(), dep_target);
                }
            }

            let target = cargo_target_name(&package.name, &package.version, &package_counts);
            locked_dependencies.insert(target.clone(), deps.clone());
            locked_dependencies.insert(
                format!("cargo://{}@{}", package.name, package.version),
                deps,
            );
        }

        Ok((
            Self {
                locked_dependencies,
                build_recipes: HashMap::new(),
            },
            lock_entries,
        ))
    }
}

#[cfg(test)]
impl crate::core::DependencyPlannerPlugin for CargoDependencyPlanner {
    fn ecosystem(&self) -> &str {
        CargoDependencyPlanner::ecosystem(self)
    }

    fn plan(
        &self,
        context: crate::core::Context,
        requirements: &[crate::core::ExternalRequirement],
    ) -> std::io::Result<crate::core::DependencyPlan> {
        let requirements: Vec<_> = requirements
            .iter()
            .cloned()
            .map(core_requirement_to_sdk)
            .collect();
        let plan =
            CargoDependencyPlanner::plan(self, core_context_to_sdk(&context), &requirements)?;
        Ok(crate::core::DependencyPlan {
            lockfile: plan.lockfile,
            locked_dependencies: plan.locked_dependencies,
        })
    }
}

#[cfg(test)]
impl crate::core::ResolverPlugin for CargoResolver {
    fn can_resolve(&self, target: &str) -> bool {
        CargoResolver::can_resolve(self, target)
    }

    fn resolve(
        &self,
        context: crate::core::Context,
        target: &str,
    ) -> std::io::Result<crate::core::Config> {
        CargoResolver::resolve(self, core_context_to_sdk(&context), target).map(sdk_config_to_core)
    }
}

#[cfg(test)]
fn core_context_to_sdk(context: &crate::core::Context) -> PluginContext {
    PluginContext {
        cache_dir: context.cache_dir.clone(),
        context_hash: context.hash,
        target_config: context
            .config
            .iter()
            .map(|(key, value)| (*key as u32, value.clone()))
            .collect(),
        lockfile: context.lockfile.as_ref().clone(),
        locked_dependencies: context.locked_dependencies.as_ref().clone(),
        target: context.target.clone(),
    }
}

#[cfg(test)]
fn core_requirement_to_sdk(requirement: crate::core::ExternalRequirement) -> ExternalRequirement {
    ExternalRequirement {
        ecosystem: requirement.ecosystem,
        package: requirement.package,
        version: requirement.version,
        features: requirement.features,
        default_features: requirement.default_features,
        target: requirement.target,
    }
}

#[cfg(test)]
fn sdk_config_to_core(config: Config) -> crate::core::Config {
    crate::core::Config {
        dependencies: config.dependencies,
        external_requirements: config
            .external_requirements
            .into_iter()
            .map(|requirement| crate::core::ExternalRequirement {
                ecosystem: requirement.ecosystem,
                package: requirement.package,
                version: requirement.version,
                features: requirement.features,
                default_features: requirement.default_features,
                target: requirement.target,
            })
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

impl CargoDependencyPlanner {
    pub fn new() -> Self {
        Self {}
    }
}

impl CargoDependencyPlanner {
    pub fn ecosystem(&self) -> &str {
        "cargo"
    }

    pub fn plan(
        &self,
        context: PluginContext,
        requirements: &[ExternalRequirement],
    ) -> std::io::Result<DependencyPlan> {
        if requirements.is_empty() {
            return Ok(DependencyPlan::default());
        }

        let manifest = synthetic_manifest(requirements)?;
        let workdir = context
            .cache_dir
            .join("dependency-plans")
            .join("cargo")
            .join(plan_id(context.context_hash, requirements));
        std::fs::create_dir_all(workdir.join("src"))?;
        let plan_cache = workdir.join("plan.json");
        if let Some(plan) = read_cached_dependency_plan(&plan_cache)? {
            eprintln!("[cbs] cache hit cargo plan");
            return Ok(plan);
        }

        let manifest_path = workdir.join("Cargo.toml");
        std::fs::write(&manifest_path, manifest)?;
        std::fs::write(workdir.join("src").join("lib.rs"), "")?;

        let mut args = vec![
            "metadata".to_string(),
            "--format-version=1".to_string(),
            "--manifest-path".to_string(),
            manifest_path.to_string_lossy().to_string(),
        ];
        if let Some(target) = cargo_filter_platform(&context) {
            args.push("--filter-platform".to_string());
            args.push(target);
        }

        let output = context
            .run_process("cargo", &args)
            .map_err(|e| std::io::Error::new(e.kind(), format!("cargo metadata failed: {e}")))?;
        let plan = metadata_to_dependency_plan(&context, requirements, &output)?;
        write_cached_dependency_plan(&plan_cache, &plan)?;
        Ok(plan)
    }
}

fn read_cached_dependency_plan(path: &std::path::Path) -> std::io::Result<Option<DependencyPlan>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(DependencyPlan {
        lockfile: json_string_map(value.get("lockfile"))?,
        locked_dependencies: json_nested_string_map(value.get("locked_dependencies"))?,
    }))
}

fn write_cached_dependency_plan(
    path: &std::path::Path,
    plan: &DependencyPlan,
) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::json!({
            "lockfile": plan.lockfile,
            "locked_dependencies": plan.locked_dependencies,
        })
        .to_string(),
    )?;
    std::fs::rename(tmp, path)
}

fn json_string_map(value: Option<&serde_json::Value>) -> std::io::Result<HashMap<String, String>> {
    let Some(object) = value.and_then(|value| value.as_object()) else {
        return Ok(HashMap::new());
    };
    object
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                value
                    .as_str()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("cache value for {key} must be a string"),
                        )
                    })?
                    .to_string(),
            ))
        })
        .collect()
}

fn json_nested_string_map(
    value: Option<&serde_json::Value>,
) -> std::io::Result<HashMap<String, HashMap<String, String>>> {
    let Some(object) = value.and_then(|value| value.as_object()) else {
        return Ok(HashMap::new());
    };
    object
        .iter()
        .map(|(key, value)| Ok((key.clone(), json_string_map(Some(value))?)))
        .collect()
}

fn plan_id(context_hash: u64, requirements: &[ExternalRequirement]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(context_hash.to_be_bytes());
    for req in requirements {
        hasher.update(req.ecosystem.as_bytes());
        hasher.update([0]);
        hasher.update(req.package.as_bytes());
        hasher.update([0]);
        hasher.update(req.version.as_bytes());
        hasher.update([0]);
        hasher.update([req.default_features as u8]);
        for feature in &req.features {
            hasher.update(feature.as_bytes());
            hasher.update([0]);
        }
        if let Some(target) = &req.target {
            hasher.update(target.as_bytes());
        }
        hasher.update([0xff]);
    }
    format!(
        "{:x}",
        u64::from_be_bytes(
            hasher.finalize()[..8]
                .try_into()
                .expect("invalid hash size")
        )
    )
}

#[derive(Debug)]
struct CargoMetadataPackage {
    id: String,
    name: String,
    version: String,
    manifest_path: std::path::PathBuf,
}

fn synthetic_manifest(requirements: &[ExternalRequirement]) -> std::io::Result<String> {
    #[derive(Default)]
    struct RootRequirement {
        version: String,
        features: Vec<String>,
        default_features: bool,
    }

    let mut deps: BTreeMap<String, RootRequirement> = BTreeMap::new();
    for req in requirements {
        if req.ecosystem != "cargo" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "cargo dependency planner cannot resolve {} requirement {}",
                    req.ecosystem, req.package
                ),
            ));
        }
        let dep = deps
            .entry(req.package.clone())
            .or_insert_with(|| RootRequirement {
                version: req.version.clone(),
                default_features: req.default_features,
                ..Default::default()
            });
        if dep.version != req.version {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "conflicting cargo requirements for {}: {} and {}",
                    req.package, dep.version, req.version
                ),
            ));
        }
        dep.default_features |= req.default_features;
        dep.features.extend(req.features.iter().cloned());
        dep.features.sort();
        dep.features.dedup();
    }

    let mut manifest = r#"[package]
name = "cbs-cargo-plan"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
"#
    .to_string();
    for (package, dep) in deps {
        manifest.push_str(&format!(
            "\"{}\" = {{ package = \"{}\", version = \"{}\", default-features = {}, features = [{}] }}\n",
            toml_escape(&package),
            toml_escape(&package),
            toml_escape(&dep.version),
            dep.default_features,
            dep.features
                .iter()
                .map(|feature| format!("\"{}\"", toml_escape(feature)))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    Ok(manifest)
}

fn metadata_to_dependency_plan(
    context: &PluginContext,
    requirements: &[ExternalRequirement],
    metadata: &[u8],
) -> std::io::Result<DependencyPlan> {
    let metadata: serde_json::Value = serde_json::from_slice(metadata)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let root_id = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("root"))
        .and_then(|root| root.as_str())
        .unwrap_or_default()
        .to_string();
    let packages = metadata
        .get("packages")
        .and_then(|packages| packages.as_array())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "metadata missing packages")
        })?;

    let mut package_infos = HashMap::new();
    let mut package_counts: HashMap<String, usize> = HashMap::new();
    for package in packages {
        let info = parse_metadata_package(package)?;
        if info.id != root_id {
            *package_counts.entry(info.name.clone()).or_default() += 1;
        }
        package_infos.insert(info.id.clone(), info);
    }

    let mut id_to_target = HashMap::new();
    let mut lockfile = HashMap::new();
    for info in package_infos.values().filter(|info| info.id != root_id) {
        let target = cargo_target_name(&info.name, &info.version, &package_counts);
        let versioned_target = format!("cargo://{}@{}", info.name, info.version);
        id_to_target.insert(info.id.clone(), target.clone());
        lockfile.insert(target, info.version.clone());
        lockfile.insert(versioned_target, info.version.clone());
    }

    let nodes = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(|nodes| nodes.as_array())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "metadata resolve missing nodes",
            )
        })?;
    let mut locked_dependencies = HashMap::new();
    for node in nodes {
        let id = node.get("id").and_then(|id| id.as_str()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "metadata node missing id")
        })?;
        if id == root_id {
            continue;
        }
        let Some(info) = package_infos.get(id) else {
            continue;
        };
        let Some(target) = id_to_target.get(id).cloned() else {
            continue;
        };
        let features = node_features(node)?;
        lockfile.insert(target.clone(), lockstring(&info.version, &features));
        lockfile.insert(
            format!("cargo://{}@{}", info.name, info.version),
            lockstring(&info.version, &features),
        );
        validate_metadata_package(context, info, &features)?;

        let mut deps = HashMap::new();
        for dep in node
            .get("deps")
            .and_then(|deps| deps.as_array())
            .into_iter()
            .flatten()
        {
            let Some(dep_id) = dep.get("pkg").and_then(|pkg| pkg.as_str()) else {
                continue;
            };
            let Some(dep_target) = id_to_target.get(dep_id) else {
                continue;
            };
            if let Some(dep_info) = package_infos.get(dep_id) {
                deps.insert(dep_info.name.clone(), dep_target.clone());
            }
            if let Some(dep_name) = dep.get("name").and_then(|name| name.as_str()) {
                deps.insert(dep_name.replace('_', "-"), dep_target.clone());
                deps.insert(dep_name.to_string(), dep_target.clone());
            }
        }
        locked_dependencies.insert(target.clone(), deps.clone());
        locked_dependencies.insert(format!("cargo://{}@{}", info.name, info.version), deps);
    }

    for req in requirements {
        let target = req.target();
        let Some(info) = package_infos
            .values()
            .find(|info| info.id != root_id && info.name == req.package)
        else {
            continue;
        };
        let resolved_target = cargo_target_name(&info.name, &info.version, &package_counts);
        if let Some(lockstring) = lockfile.get(&resolved_target).cloned() {
            lockfile.insert(target.clone(), lockstring);
        }
        if let Some(deps) = locked_dependencies.get(&resolved_target).cloned() {
            locked_dependencies.insert(target, deps);
        }
    }

    Ok(DependencyPlan {
        lockfile,
        locked_dependencies,
    })
}

fn parse_metadata_package(package: &serde_json::Value) -> std::io::Result<CargoMetadataPackage> {
    let string_field = |field: &str| {
        package
            .get(field)
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("metadata package missing {field}"),
                )
            })
    };
    Ok(CargoMetadataPackage {
        id: string_field("id")?,
        name: string_field("name")?,
        version: string_field("version")?,
        manifest_path: std::path::PathBuf::from(string_field("manifest_path")?),
    })
}

fn node_features(node: &serde_json::Value) -> std::io::Result<Vec<String>> {
    let mut features: Vec<_> = node
        .get("features")
        .and_then(|features| features.as_array())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "metadata node missing features",
            )
        })?
        .iter()
        .filter_map(|feature| feature.as_str().map(|feature| feature.to_string()))
        .collect();
    features.sort();
    features.dedup();
    Ok(features)
}

fn validate_metadata_package(
    context: &PluginContext,
    info: &CargoMetadataPackage,
    features: &[String],
) -> std::io::Result<()> {
    let feature_refs: Vec<_> = features.iter().map(|feature| feature.as_str()).collect();
    let manifest = parse_cargo_toml(context, &info.manifest_path, &feature_refs)?;
    if manifest.has_build_script
        && cargo_recipes::build_recipe(context, &info.name, &info.version).is_none()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} {} declares build.rs, but no hermetic Cargo build recipe is available",
                info.name, info.version
            ),
        ));
    }
    Ok(())
}

fn lockstring(version: &str, features: &[String]) -> String {
    if features.is_empty() {
        return version.to_string();
    }
    format!("{version},{}", features.join(","))
}

fn cargo_filter_platform(context: &PluginContext) -> Option<String> {
    let arch = context.get_config(build_config_key::TARGET_ARCH)?;
    let vendor = context.get_config(build_config_key::TARGET_VENDOR)?;
    let os = match context.get_config(build_config_key::TARGET_OS)? {
        "macos" => "darwin",
        os => os,
    };
    let env = context
        .get_config(build_config_key::TARGET_ENV)
        .unwrap_or_default();
    if env.is_empty() {
        Some(format!("{arch}-{vendor}-{os}"))
    } else {
        Some(format!("{arch}-{vendor}-{os}-{env}"))
    }
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn get_rust_files(
    path: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            get_rust_files(&entry.path(), out)?;
        }

        if let Some(ext) = entry.path().extension() {
            if ext == "rs" {
                out.push(entry.path());
            }
        }
    }
    Ok(())
}

fn parse_lockstring(l: &str) -> (&str, Vec<&str>) {
    let mut components = l.split(",");
    let version = components.next().expect("always get at least one split");
    let features = components.collect();
    (version, features)
}

#[derive(Debug)]
struct CargoToml {
    dependencies: Vec<CargoDependency>,
    crate_name: String,
    crate_type: String,
    edition: String,
    root_source: std::path::PathBuf,
    features: Vec<String>,
    has_build_script: bool,
    rustc_env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoDependency {
    alias: String,
    package: String,
}

#[derive(Debug)]
struct DependencySpec {
    alias: String,
    package: String,
    optional: bool,
}

#[derive(Debug)]
struct LockedPackage {
    name: String,
    version: String,
    dependencies: Vec<String>,
}

fn parse_cargo_toml(
    context: &PluginContext,
    filename: &std::path::Path,
    features: &[&str],
) -> std::io::Result<CargoToml> {
    let content = std::fs::read_to_string(filename)?;
    let table = content
        .parse::<toml::Table>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let manifest_dir = filename.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("manifest has no parent directory: {}", filename.display()),
        )
    })?;

    let package = table
        .get("package")
        .and_then(|v| v.as_table())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cargo.toml missing [package]",
            )
        })?;
    let package_name = package
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cargo.toml package missing name",
            )
        })?;
    let package_version = package
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");
    let edition = package
        .get("edition")
        .and_then(|v| v.as_str())
        .unwrap_or("2015")
        .to_string();
    let has_build_script = match package.get("build") {
        Some(toml::Value::Boolean(false)) => false,
        Some(toml::Value::String(_)) => true,
        Some(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cargo.toml package build key must be a string or false",
            ))
        }
        None => manifest_dir.join("build.rs").exists(),
    };

    let lib = table.get("lib").and_then(|v| v.as_table());
    let crate_name = lib
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or(package_name)
        .replace('-', "_");
    let crate_type = if lib
        .and_then(|t| t.get("proc-macro"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "proc-macro".to_string()
    } else {
        "rlib".to_string()
    };
    let root_source = manifest_dir.join(
        lib.and_then(|t| t.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("src/lib.rs"),
    );

    let mut dependency_specs = Vec::new();
    if let Some(toml::Value::Table(deps_table)) = table.get("dependencies") {
        for (k, v) in deps_table {
            dependency_specs.push(parse_dependency_spec(k, v));
        }
    }
    if let Some(toml::Value::Table(targets)) = table.get("target") {
        for (target, target_table) in targets {
            let include = if target.starts_with("cfg(") && target.ends_with(')') {
                resolve_cfg_directive(context, &target[4..target.len() - 1])?
            } else {
                false
            };
            if !include {
                continue;
            }

            if let Some(toml::Value::Table(deps_table)) = target_table.get("dependencies") {
                for (k, v) in deps_table {
                    dependency_specs.push(parse_dependency_spec(k, v));
                }
            }
        }
    }

    let mut features_table = HashMap::new();
    if let Some(toml::Value::Table(t)) = table.get("features") {
        for (k, v) in t {
            let members = v
                .as_array()
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| dep.as_str().map(|dep| dep.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            features_table.insert(k.to_string(), members);
        }
    }

    let enabled_features = expand_features(&features_table, features);
    let mut optional_deps = HashSet::new();
    let optional_aliases: HashSet<_> = dependency_specs
        .iter()
        .filter(|dep| dep.optional)
        .map(|dep| dep.alias.as_str())
        .collect();
    for feature in &enabled_features {
        if optional_aliases.contains(feature.as_str()) {
            optional_deps.insert(feature.to_string());
        }
    }
    for feature in &enabled_features {
        let Some(members) = features_table.get(feature) else {
            continue;
        };
        for member in members {
            if let Some(dep) = member.strip_prefix("dep:") {
                optional_deps.insert(dep.to_string());
                continue;
            }

            if let Some((dep, _)) = member.split_once('/') {
                let dep = dep.trim_end_matches('?');
                if optional_aliases.contains(dep) && !member.contains("?/") {
                    optional_deps.insert(dep.to_string());
                }
                continue;
            }

            if !features_table.contains_key(member) && optional_aliases.contains(member.as_str()) {
                optional_deps.insert(member.to_string());
            }
        }
    }

    let mut seen = HashSet::new();
    let mut dependencies = Vec::new();
    for dep in dependency_specs {
        if dep.optional && !optional_deps.contains(&dep.alias) {
            continue;
        }
        if seen.insert(dep.alias.clone()) {
            dependencies.push(CargoDependency {
                alias: dep.alias,
                package: dep.package,
            });
        }
    }

    let mut features: Vec<_> = enabled_features.into_iter().collect();
    features.sort();

    Ok(CargoToml {
        dependencies,
        crate_name,
        crate_type,
        edition,
        root_source,
        features,
        has_build_script,
        rustc_env: cargo_rustc_env(package, manifest_dir, package_name, package_version),
    })
}

fn cargo_rustc_env(
    package: &toml::map::Map<String, toml::Value>,
    manifest_dir: &std::path::Path,
    package_name: &str,
    package_version: &str,
) -> Vec<String> {
    let (version_core, version_pre) = package_version
        .split_once('-')
        .map(|(core, pre)| (core, pre))
        .unwrap_or((package_version, ""));
    let mut version_parts = version_core.split('.');
    let version_major = version_parts.next().unwrap_or("0");
    let version_minor = version_parts.next().unwrap_or("0");
    let version_patch = version_parts.next().unwrap_or("0");

    let mut env = vec![
        (
            "CARGO_MANIFEST_DIR".to_string(),
            manifest_dir.display().to_string(),
        ),
        ("CARGO_PKG_NAME".to_string(), package_name.to_string()),
        (
            "CARGO_CRATE_NAME".to_string(),
            package_name.replace('-', "_"),
        ),
        ("CARGO_PKG_VERSION".to_string(), package_version.to_string()),
        (
            "CARGO_PKG_VERSION_MAJOR".to_string(),
            version_major.to_string(),
        ),
        (
            "CARGO_PKG_VERSION_MINOR".to_string(),
            version_minor.to_string(),
        ),
        (
            "CARGO_PKG_VERSION_PATCH".to_string(),
            version_patch.to_string(),
        ),
        ("CARGO_PKG_VERSION_PRE".to_string(), version_pre.to_string()),
        (
            "CARGO_PKG_AUTHORS".to_string(),
            package_string_array(package, "authors").join(":"),
        ),
        (
            "CARGO_PKG_DESCRIPTION".to_string(),
            package_string(package, "description"),
        ),
        (
            "CARGO_PKG_HOMEPAGE".to_string(),
            package_string(package, "homepage"),
        ),
        (
            "CARGO_PKG_REPOSITORY".to_string(),
            package_string(package, "repository"),
        ),
        (
            "CARGO_PKG_LICENSE".to_string(),
            package_string(package, "license"),
        ),
        (
            "CARGO_PKG_LICENSE_FILE".to_string(),
            package_string(package, "license-file"),
        ),
        (
            "CARGO_PKG_README".to_string(),
            package_string(package, "readme"),
        ),
        (
            "CARGO_PKG_RUST_VERSION".to_string(),
            package_string(package, "rust-version"),
        ),
    ];
    if let Some(links) = package.get("links").and_then(|v| v.as_str()) {
        env.push(("CARGO_MANIFEST_LINKS".to_string(), links.to_string()));
    }

    env.into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

fn package_string(package: &toml::map::Map<String, toml::Value>, key: &str) -> String {
    package
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn package_string_array(package: &toml::map::Map<String, toml::Value>, key: &str) -> Vec<String> {
    package
        .get(key)
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_cfg_directive(context: &PluginContext, directive: &str) -> std::io::Result<bool> {
    let directive = directive.trim();
    if let Some(args) = strip_cfg_call(directive, "all") {
        for arg in split_cfg_args(args)? {
            if !resolve_cfg_directive(context, arg)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(args) = strip_cfg_call(directive, "any") {
        for arg in split_cfg_args(args)? {
            if resolve_cfg_directive(context, arg)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if let Some(args) = strip_cfg_call(directive, "not") {
        let args = split_cfg_args(args)?;
        if args.len() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("not() expects one cfg argument: {directive}"),
            ));
        }
        return Ok(!resolve_cfg_directive(context, args[0])?);
    }

    if directive == "unix" {
        return Ok(context.get_config(build_config_key::TARGET_FAMILY) == Some("unix"));
    }
    if directive == "windows" {
        return Ok(context.get_config(build_config_key::TARGET_FAMILY) == Some("windows"));
    }

    if let Some((key, value)) = directive.split_once('=') {
        let value = value.trim().trim_matches('"');
        return Ok(match key.trim() {
            "target_family" => context.get_config(build_config_key::TARGET_FAMILY) == Some(value),
            "target_os" => context.get_config(build_config_key::TARGET_OS) == Some(value),
            "target_env" => context.get_config(build_config_key::TARGET_ENV) == Some(value),
            "target_arch" => context.get_config(build_config_key::TARGET_ARCH) == Some(value),
            "target_vendor" => context.get_config(build_config_key::TARGET_VENDOR) == Some(value),
            "target_endian" => context.get_config(build_config_key::TARGET_ENDIAN) == Some(value),
            _ => false,
        });
    }

    Ok(false)
}

fn parse_dependency_spec(alias: &str, value: &toml::Value) -> DependencySpec {
    let (package, optional) = match value.as_table() {
        Some(table) => (
            table
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or(alias)
                .to_string(),
            table
                .get("optional")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        None => (alias.to_string(), false),
    };

    DependencySpec {
        alias: alias.to_string(),
        package,
        optional,
    }
}

fn expand_features(
    features_table: &HashMap<String, Vec<String>>,
    requested_features: &[&str],
) -> HashSet<String> {
    let mut enabled = HashSet::new();
    let mut stack: Vec<String> = requested_features
        .iter()
        .filter(|feature| !feature.is_empty())
        .map(|feature| feature.to_string())
        .collect();

    while let Some(feature) = stack.pop() {
        if !enabled.insert(feature.clone()) {
            continue;
        }

        let Some(members) = features_table.get(&feature) else {
            continue;
        };
        for member in members {
            if member.starts_with("dep:") || member.contains('/') {
                continue;
            }
            if features_table.contains_key(member) {
                stack.push(member.to_string());
            }
        }
    }

    enabled
}

fn strip_cfg_call<'a>(directive: &'a str, name: &str) -> Option<&'a str> {
    directive
        .strip_prefix(name)
        .and_then(|rest| rest.trim_start().strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
}

fn split_cfg_args(args: &str) -> std::io::Result<Vec<&str>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut start = 0usize;
    for (idx, ch) in args.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unbalanced cfg directive: {args}"),
                    )
                })?;
            }
            ',' if !in_string && depth == 0 => {
                out.push(args[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    if in_string || depth != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unbalanced cfg directive: {args}"),
        ));
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    Ok(out)
}

fn parse_cargo_target(target: &str) -> std::io::Result<(&str, Option<&str>)> {
    let crate_name = target
        .strip_prefix("cargo://")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "invalid target name"))?;
    Ok(match crate_name.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (crate_name, None),
    })
}

fn parse_lock_dependency(dependency: &str) -> (&str, Option<&str>) {
    let mut parts = dependency.split_whitespace();
    let name = parts.next().unwrap_or(dependency);
    let version = parts.next();
    (name, version)
}

fn cargo_target_name(
    package_name: &str,
    package_version: &str,
    package_counts: &HashMap<String, usize>,
) -> String {
    if package_counts.get(package_name).copied().unwrap_or(0) > 1 {
        format!("cargo://{package_name}@{package_version}")
    } else {
        format!("cargo://{package_name}")
    }
}

fn encode_native_static_lib(lib: &CargoNativeStaticLib) -> String {
    [
        lib.name.as_str(),
        &lib.sources.join(";"),
        &lib.include_dirs.join(";"),
        &lib.flags.join(";"),
    ]
    .join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let module = module_path!().replace("::", "_");
        let dir = std::env::temp_dir().join(format!("cbs-{module}-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn context<I>(config: I) -> PluginContext
    where
        I: IntoIterator<Item = (u32, String)>,
    {
        PluginContext {
            cache_dir: std::env::temp_dir(),
            context_hash: 0,
            target_config: config.into_iter().collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_parse_manifest_features_aliases_and_target_cfgs() {
        let dir = temp_dir("manifest");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/custom.rs"), "").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo-crate"
version = "1.0.0"
edition = "2021"

[lib]
name = "demo_lib"
path = "src/custom.rs"
proc-macro = true

[dependencies]
bytes = "1"
serde_alias = { package = "serde", version = "1", optional = true }

[target.'cfg(all(unix, target_os = "linux", not(target_env = "musl")))'.dependencies]
libc = { version = "0.2", optional = true }

[target.'cfg(windows)'.dependencies]
winapi = "0.3"

[features]
default = ["std"]
std = ["serde_alias", "dep:libc"]
"#,
        )
        .unwrap();

        let context = context([
            (build_config_key::TARGET_FAMILY, "unix".to_string()),
            (build_config_key::TARGET_OS, "linux".to_string()),
            (build_config_key::TARGET_ENV, "gnu".to_string()),
        ]);

        let manifest = parse_cargo_toml(&context, &dir.join("Cargo.toml"), &["default"]).unwrap();
        assert_eq!(manifest.crate_name, "demo_lib");
        assert_eq!(manifest.crate_type, "proc-macro");
        assert_eq!(manifest.edition, "2021");
        assert_eq!(manifest.root_source, dir.join("src/custom.rs"));
        assert!(!manifest.has_build_script);
        assert_eq!(
            manifest.features,
            vec!["default".to_string(), "std".to_string()]
        );
        assert_eq!(
            manifest.dependencies,
            vec![
                CargoDependency {
                    alias: "bytes".to_string(),
                    package: "bytes".to_string(),
                },
                CargoDependency {
                    alias: "serde_alias".to_string(),
                    package: "serde".to_string(),
                },
                CargoDependency {
                    alias: "libc".to_string(),
                    package: "libc".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_parse_manifest_detects_build_script() {
        let dir = temp_dir("build-script");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("build.rs"), "").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "1.0.0"
"#,
        )
        .unwrap();

        let context = context(std::iter::empty::<(u32, String)>());
        let manifest = parse_cargo_toml(&context, &dir.join("Cargo.toml"), &[]).unwrap();
        assert!(manifest.has_build_script);

        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "1.0.0"
build = false
"#,
        )
        .unwrap();
        let manifest = parse_cargo_toml(&context, &dir.join("Cargo.toml"), &[]).unwrap();
        assert!(!manifest.has_build_script);
    }

    #[test]
    fn test_cargo_lock_uses_version_qualified_duplicate_targets() {
        let dir = temp_dir("lock");
        let lockfile = dir.join("Cargo.lock");
        std::fs::write(
            &lockfile,
            r#"
version = 3

[[package]]
name = "bytes"
version = "0.5.6"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "bytes"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "hyper"
version = "0.13.10"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "bytes 0.5.6",
]
"#,
        )
        .unwrap();

        let (resolver, lock_entries) = CargoResolver::from_cargo_lock(&lockfile).unwrap();
        assert_eq!(lock_entries.get("cargo://bytes@0.5.6").unwrap(), "0.5.6");
        assert_eq!(lock_entries.get("cargo://bytes@1.0.0").unwrap(), "1.0.0");
        assert_eq!(lock_entries.get("cargo://hyper").unwrap(), "0.13.10");
        assert_eq!(
            resolver
                .locked_dependencies
                .get("cargo://hyper")
                .and_then(|deps| deps.get("bytes")),
            Some(&"cargo://bytes@0.5.6".to_string())
        );
    }
}

impl CargoResolver {
    pub fn can_resolve(&self, target: &str) -> bool {
        target.starts_with("cargo://")
    }

    pub fn resolve(&self, context: PluginContext, target: &str) -> std::io::Result<Config> {
        let (crate_name, target_version) = parse_cargo_target(target)?;

        let lockstring = match context.get_locked_version(target) {
            Ok(lockstring) => lockstring,
            Err(e) => match target_version {
                Some(version) => version.to_string(),
                None => return Err(e),
            },
        };
        let lockstring = &lockstring;
        let (crate_version, features) = parse_lockstring(&lockstring);

        let workdir = context.working_directory();
        std::fs::create_dir_all(&workdir).ok();

        // Download the crate tarball
        let tar_dest = workdir.join("crate.tar");

        if !tar_dest.exists() || tar_dest.metadata().map(|m| m.len() == 0).unwrap_or(true) {
            context.download(
                format!(
                    "https://crates.io/api/v1/crates/{}/{}/download",
                    crate_name, crate_version
                ),
                &tar_dest,
            )?;
        }

        // Untar the crate tarball
        let dest = workdir.join("crate");
        if !dest.join("Cargo.toml").exists() {
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            std::fs::create_dir_all(&dest)?;
            context.run_process(
                "tar",
                &[
                    "xzvf",
                    &tar_dest.to_string_lossy(),
                    "-C",
                    &dest.to_string_lossy(),
                    "--strip-components=1",
                ],
            )?;
        }

        let mut rust_files = Vec::new();
        get_rust_files(&dest.join("src"), &mut rust_files)?;

        let toml = parse_cargo_toml(&context, &dest.join("Cargo.toml"), &features)?;
        let build_recipe = self.build_recipe(&context, target, crate_name, crate_version);
        if toml.has_build_script && build_recipe.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{target} declares build.rs, but no hermetic Cargo build recipe was provided"
                ),
            ));
        }

        let mut deps = Vec::new();
        let mut dependency_aliases = Vec::new();
        for dep in toml.dependencies {
            let dep_target = self
                .locked_dependencies
                .get(target)
                .and_then(|deps| deps.get(&dep.package))
                .cloned()
                .or_else(|| context.get_locked_dependency(target, &dep.package))
                .unwrap_or_else(|| format!("cargo://{}", dep.package));
            dependency_aliases.push(format!("{dep_target}={}", dep.alias.replace('-', "_")));
            deps.push(dep_target);
        }

        let mut extras = HashMap::new();
        extras.insert(config_extra_keys::FEATURES, toml.features);
        extras.insert(config_extra_keys::CRATE_NAME, vec![toml.crate_name]);
        extras.insert(config_extra_keys::CRATE_TYPE, vec![toml.crate_type]);
        extras.insert(config_extra_keys::EDITION, vec![toml.edition]);
        extras.insert(
            config_extra_keys::ROOT_SOURCE,
            vec![toml.root_source.to_string_lossy().to_string()],
        );
        extras.insert(
            config_extra_keys::CRATE_ROOT,
            vec![dest.to_string_lossy().to_string()],
        );
        extras.insert(config_extra_keys::DEPENDENCY_ALIASES, dependency_aliases);
        extras.insert(config_extra_keys::RUSTC_CFGS, {
            let mut cfgs = build_recipe
                .as_ref()
                .map(|recipe| recipe.rustc_cfgs.clone())
                .unwrap_or_default();
            if crate_name == "serde" && crate_version == "1.0.228" {
                cfgs.push("if_docsrs_then_no_serde_core".to_string());
            }
            cfgs
        });
        extras.insert(
            config_extra_keys::NATIVE_STATIC_LIBS,
            build_recipe
                .as_ref()
                .map(|recipe| {
                    recipe
                        .native_static_libs
                        .iter()
                        .map(encode_native_static_lib)
                        .collect()
                })
                .unwrap_or_default(),
        );
        let mut rustc_env = toml.rustc_env;
        if let Some(out_dir) = hermetic_cargo_out_dir(&workdir, crate_name, crate_version)? {
            rustc_env.push(format!("OUT_DIR={}", out_dir.display()));
        }
        extras.insert(config_extra_keys::RUSTC_ENV, rustc_env);

        Ok(Config {
            dependencies: deps,
            external_requirements: Vec::new(),
            build_plugin: "@rust_plugin".to_string(),
            location: None,
            sources: rust_files
                .into_iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect(),
            build_dependencies: vec!["@rust_compiler".to_string()],
            kind: RUST_LIBRARY.to_string(),
            extras,
        })
    }
}

fn hermetic_cargo_out_dir(
    workdir: &std::path::Path,
    package: &str,
    version: &str,
) -> std::io::Result<Option<std::path::PathBuf>> {
    match (package, version) {
        ("serde_core", "1.0.228") => {
            let out_dir = workdir.join("out");
            std::fs::create_dir_all(&out_dir)?;
            std::fs::write(
                out_dir.join("private.rs"),
                "\
#[doc(hidden)]
pub mod __private228 {
    #[doc(hidden)]
    pub use crate::private::*;
}
",
            )?;
            Ok(Some(out_dir))
        }
        ("serde", "1.0.228") => {
            let out_dir = workdir.join("out");
            std::fs::create_dir_all(&out_dir)?;
            std::fs::write(
                out_dir.join("private.rs"),
                "\
#[doc(hidden)]
pub mod __private228 {
    #[doc(hidden)]
    pub use crate::private::*;
}
use serde_core::__private228 as serde_core_private;
",
            )?;
            Ok(Some(out_dir))
        }
        _ => Ok(None),
    }
}
