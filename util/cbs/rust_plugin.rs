use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[path = "cargo.rs"]
mod cargo;
#[path = "cargo_recipes.rs"]
mod cargo_recipes;

use cbs_plugin_sdk::*;

const RUST_LIBRARY: &str = "rust_library";
const RUST_BINARY: &str = "rust_binary";
const RUST_TEST: &str = "rust_test";

#[no_mangle]
pub extern "C" fn cbs_plugin_v1() -> CbsPluginV1 {
    CbsPluginV1 {
        abi_version: CBS_PLUGIN_ABI_VERSION,
        manifest: rust_manifest,
        initialize: empty_plugin_initialize,
        parse_rule: rust_parse_rule,
        build: rust_build,
        plan_dependencies: rust_plan_dependencies,
        resolve_target: rust_resolve_target,
        free_buffer: free_owned_buffer,
    }
}

extern "C" fn rust_manifest() -> CbsOwnedBuffer {
    CbsOwnedBuffer::from_vec(encode_plugin_manifest(&PluginManifest {
        name: "rust".to_string(),
        rule_kinds: vec![
            RUST_LIBRARY.to_string(),
            RUST_BINARY.to_string(),
            RUST_TEST.to_string(),
        ],
        test_rule_kinds: vec![RUST_TEST.to_string()],
        build_plugins: vec!["@rust_plugin".to_string()],
        label_fields: Vec::new(),
        dependency_ecosystems: vec!["cargo".to_string()],
        target_prefixes: vec!["cargo://".to_string()],
    }))
}

extern "C" fn rust_parse_rule(request: CbsSlice) -> CbsOwnedBuffer {
    let response = match decode_parse_rule_request(unsafe { request.as_slice() }) {
        Ok(request) => parse_rust_rule_request(request),
        Err(e) => ParseRuleResponse::Failure(format!("failed to decode parse-rule request: {e}")),
    };
    CbsOwnedBuffer::from_vec(encode_parse_rule_response(&response))
}

extern "C" fn rust_build(request: CbsSlice) -> CbsOwnedBuffer {
    let response = match decode_build_request(unsafe { request.as_slice() }) {
        Ok(request) => build_rust_request(request),
        Err(e) => BuildResponse::Failure(format!("failed to decode build request: {e}")),
    };
    CbsOwnedBuffer::from_vec(encode_build_response(&response))
}

extern "C" fn rust_plan_dependencies(request: CbsSlice) -> CbsOwnedBuffer {
    let response = match decode_plan_dependencies_request(unsafe { request.as_slice() }) {
        Ok(request) if request.ecosystem == "cargo" => cargo::CargoDependencyPlanner::new()
            .plan(request.context, &request.requirements)
            .map(PlanDependenciesResponse::Success)
            .unwrap_or_else(|e| PlanDependenciesResponse::Failure(e.to_string())),
        Ok(request) => PlanDependenciesResponse::Failure(format!(
            "rust plugin does not plan ecosystem {}",
            request.ecosystem
        )),
        Err(e) => PlanDependenciesResponse::Failure(format!(
            "failed to decode dependency-plan request: {e}"
        )),
    };
    CbsOwnedBuffer::from_vec(encode_plan_dependencies_response(&response))
}

extern "C" fn rust_resolve_target(request: CbsSlice) -> CbsOwnedBuffer {
    let response = match decode_resolve_target_request(unsafe { request.as_slice() }) {
        Ok(request) if request.target.starts_with("cargo://") => {
            let context = request.context.with_target(request.target.clone());
            cargo::CargoResolver::new()
                .resolve(context, &request.target)
                .map(ResolveTargetResponse::Success)
                .unwrap_or_else(|e| ResolveTargetResponse::Failure(e.to_string()))
        }
        Ok(request) => ResolveTargetResponse::Failure(format!(
            "rust plugin does not resolve target {}",
            request.target
        )),
        Err(e) => {
            ResolveTargetResponse::Failure(format!("failed to decode resolve-target request: {e}"))
        }
    };
    CbsOwnedBuffer::from_vec(encode_resolve_target_response(&response))
}

fn parse_rust_rule_request(request: ParseRuleRequest) -> ParseRuleResponse {
    let mut extras = HashMap::new();
    if let Some(edition) = request.string_fields.get("edition") {
        extras.insert(config_extra_keys::EDITION, vec![edition.clone()]);
    }
    if let Some(crate_name) = request.string_fields.get("crate_name") {
        extras.insert(config_extra_keys::CRATE_NAME, vec![crate_name.clone()]);
    }
    if let Some(crate_type) = request.string_fields.get("crate_type") {
        extras.insert(config_extra_keys::CRATE_TYPE, vec![crate_type.clone()]);
    }
    if let Some(root_source) = request.string_fields.get("root_source") {
        match package_source_path(&request.package_dir, root_source) {
            Ok(root_source) => {
                extras.insert(config_extra_keys::ROOT_SOURCE, vec![root_source]);
            }
            Err(e) => return ParseRuleResponse::Failure(e.to_string()),
        }
    }

    ParseRuleResponse::Success(Config {
        dependencies: request.dependencies,
        external_requirements: request.cargo_requirements,
        build_plugin: "@rust_plugin".to_string(),
        sources: request.sources,
        build_dependencies: vec!["@rust_compiler".to_string()],
        kind: request.kind,
        extras,
        ..Default::default()
    })
}

fn package_source_path(package_dir: &Path, path: &str) -> std::io::Result<String> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "workspace paths must be package-relative: {}",
                path.display()
            ),
        ));
    }
    Ok(package_dir.join(path).to_string_lossy().to_string())
}

fn build_rust_request(request: BuildRequest) -> BuildResponse {
    let name = rust_name(&request.target);
    let config = request.config;
    let tool_paths = request.tool_paths;
    match config.kind.as_str() {
        RUST_LIBRARY => build_library(
            &request.working_directory,
            &name,
            config,
            request.dependencies,
            &tool_paths,
        ),
        RUST_BINARY => build_executable(
            &request.working_directory,
            &name,
            config,
            request.dependencies,
            false,
            &tool_paths,
        ),
        RUST_TEST => build_executable(
            &request.working_directory,
            &name,
            config,
            request.dependencies,
            true,
            &tool_paths,
        ),
        kind => BuildResponse::Failure(format!("unsupported target kind: {kind:?}")),
    }
}

fn build_library(
    working_directory: &Path,
    name: &str,
    config: Config,
    deps: HashMap<String, BuildOutput>,
    tool_paths: &HashMap<String, String>,
) -> BuildResponse {
    let compiler = match compiler(&config, &deps) {
        Ok(compiler) => compiler,
        Err(e) => return BuildResponse::Failure(e),
    };
    if let Err(e) = std::fs::create_dir_all(working_directory) {
        return BuildResponse::Failure(format!("failed to create working directory: {e}"));
    }

    let crate_name = config
        .get(config_extra_keys::CRATE_NAME)
        .first()
        .map(|s| s.as_str())
        .unwrap_or(name);
    let crate_type = config
        .get(config_extra_keys::CRATE_TYPE)
        .first()
        .map(|s| s.as_str())
        .unwrap_or("rlib");
    let edition = config
        .get(config_extra_keys::EDITION)
        .first()
        .map(|s| s.as_str())
        .unwrap_or("2018");
    let metadata = metadata_from_working_directory(working_directory);
    let out_file = library_output_file(working_directory, crate_name, &metadata, crate_type);

    let root_source = match library_root_source(&config, name) {
        Ok(root_source) => root_source,
        Err(e) => return BuildResponse::Failure(e),
    };
    let native_libs = match build_native_static_libs(&config, working_directory, tool_paths) {
        Ok(libs) => libs,
        Err(e) => {
            return BuildResponse::Failure(format!("failed to build native static libs: {e:?}"))
        }
    };
    let rustc_env = rustc_env(&config);
    let mut args = common_rustc_args(&config, &deps);
    args.push(root_source);
    args.extend(native_link_args(&native_libs));
    args.push(format!("--edition={edition}"));
    args.extend([
        "-C".to_string(),
        format!("metadata={metadata}"),
        "--crate-type".to_string(),
        crate_type.to_string(),
        "--crate-name".to_string(),
        crate_name.to_string(),
        "-o".to_string(),
        out_file.to_string_lossy().to_string(),
        "--cap-lints".to_string(),
        "allow".to_string(),
        "--color=always".to_string(),
    ]);

    if let Err(e) = run_process(&compiler, &args, &rustc_env, working_directory, tool_paths) {
        return BuildResponse::Failure(format!("failed to invoke compiler:\n{e}"));
    }

    let transitive_deps = transitive_deps(&config, &deps);
    let tdeps = transitive_deps
        .into_iter()
        .chain(native_libs.iter().map(|lib| {
            (
                format!("native_{}", lib.name),
                lib.path.display().to_string(),
            )
        }))
        .map(|(name, path)| format!("{name}:{path}"))
        .collect();
    let mut extras = HashMap::new();
    extras.insert(build_output_kind::TRANSITIVE_PRODUCTS, tdeps);

    BuildResponse::Success(BuildOutput {
        outputs: vec![out_file],
        extras,
    })
}

fn build_executable(
    working_directory: &Path,
    name: &str,
    config: Config,
    deps: HashMap<String, BuildOutput>,
    test: bool,
    tool_paths: &HashMap<String, String>,
) -> BuildResponse {
    let compiler = match compiler(&config, &deps) {
        Ok(compiler) => compiler,
        Err(e) => return BuildResponse::Failure(e),
    };
    if let Err(e) = std::fs::create_dir_all(working_directory) {
        return BuildResponse::Failure(format!("failed to create working directory: {e}"));
    }
    let out_file = working_directory.join(name);
    let edition = config
        .get(config_extra_keys::EDITION)
        .first()
        .map(|s| s.as_str())
        .unwrap_or("2021");
    let root_source = match executable_root_source(&config, name, test) {
        Ok(root_source) => root_source,
        Err(e) => return BuildResponse::Failure(e),
    };
    let rustc_env = rustc_env(&config);
    let mut args = common_rustc_args(&config, &deps);
    args.push(root_source);
    if test {
        args.push("--test".to_string());
    }
    args.extend(["-o".to_string(), out_file.to_string_lossy().to_string()]);
    args.push(format!("--edition={edition}"));
    args.push("--color=always".to_string());

    if let Err(e) = run_process(&compiler, &args, &rustc_env, working_directory, tool_paths) {
        return BuildResponse::Failure(format!("failed to invoke compiler:\n{e}"));
    }

    BuildResponse::Success(BuildOutput {
        outputs: vec![out_file],
        ..Default::default()
    })
}

fn compiler(config: &Config, deps: &HashMap<String, BuildOutput>) -> Result<PathBuf, String> {
    config
        .build_dependencies
        .first()
        .and_then(|target| deps.get(target))
        .and_then(|output| output.outputs.first())
        .cloned()
        .ok_or_else(|| "the rust compiler must be specified as a build_dependency!".to_string())
}

fn common_rustc_args(config: &Config, deps: &HashMap<String, BuildOutput>) -> Vec<String> {
    let libraries = libraries(config, deps);
    let mut args = Vec::new();
    args.extend(
        libraries
            .iter()
            .flat_map(|(name, path)| ["--extern".to_string(), format!("{name}={path}")]),
    );
    args.extend(transitive_deps(config, deps).iter().flat_map(|(_, path)| {
        [
            "-L".to_string(),
            Path::new(path)
                .parent()
                .expect("must have a parent")
                .to_string_lossy()
                .to_string(),
        ]
    }));
    args.extend(native_link_args_from_products(&transitive_deps(
        config, deps,
    )));
    args.extend(
        config
            .get(config_extra_keys::FEATURES)
            .iter()
            .flat_map(|s| ["--cfg".to_string(), format!("feature=\"{s}\"")]),
    );
    args.extend(
        config
            .get(config_extra_keys::RUSTC_CFGS)
            .iter()
            .flat_map(|s| ["--cfg".to_string(), s.to_string()]),
    );
    if config
        .get(config_extra_keys::CRATE_TYPE)
        .first()
        .is_some_and(|crate_type| crate_type == "proc-macro")
    {
        args.extend(["--extern".to_string(), "proc_macro".to_string()]);
    }
    args
}

fn library_root_source(config: &Config, name: &str) -> Result<String, String> {
    if let Some(root_source) = config.get(config_extra_keys::ROOT_SOURCE).first() {
        return Ok(root_source.clone());
    }
    let mut candidates: Vec<_> = config
        .sources
        .iter()
        .filter(|s| s.ends_with("lib.rs") || s.ends_with(&format!("{name}.rs")))
        .collect();
    candidates.sort_by_key(|c| c.split('/').count());
    candidates
        .into_iter()
        .next()
        .cloned()
        .ok_or_else(|| format!("no lib.rs or {name}.rs source file specified!"))
}

fn executable_root_source(config: &Config, name: &str, test: bool) -> Result<String, String> {
    if let Some(root_source) = config.get(config_extra_keys::ROOT_SOURCE).first() {
        return Ok(root_source.clone());
    }
    let mut candidates: Vec<_> = config
        .sources
        .iter()
        .filter(|s| {
            s.ends_with(&format!("/{name}.rs"))
                || s.ends_with("/main.rs")
                || (test && s.ends_with("/lib.rs"))
        })
        .collect();
    candidates.sort_by_key(|c| c.split('/').count());
    candidates
        .into_iter()
        .next()
        .cloned()
        .ok_or_else(|| format!("no main.rs, lib.rs, or {name}.rs source file specified!"))
}

fn rust_name(target: &str) -> String {
    target_shortname(target)
        .split('@')
        .next()
        .unwrap_or("")
        .replace('-', "_")
}

fn target_shortname(target: &str) -> &str {
    target
        .split("//")
        .last()
        .and_then(|s| s.split('/').last())
        .and_then(|s| s.split(':').last())
        .unwrap_or(target)
}

fn runtime_dependencies(config: &Config) -> Vec<String> {
    let mut deps = config.dependencies.clone();
    deps.extend(config.external_requirements.iter().map(|requirement| {
        requirement
            .target
            .clone()
            .unwrap_or_else(|| format!("{}://{}", requirement.ecosystem, requirement.package))
    }));
    deps.sort();
    deps.dedup();
    deps
}

fn dependency_aliases(config: &Config) -> HashMap<String, String> {
    config
        .get(config_extra_keys::DEPENDENCY_ALIASES)
        .iter()
        .filter_map(|alias| {
            let (target, crate_name) = alias.rsplit_once('=')?;
            Some((target.to_string(), crate_name.to_string()))
        })
        .collect()
}

fn libraries(config: &Config, deps: &HashMap<String, BuildOutput>) -> Vec<(String, String)> {
    let aliases = dependency_aliases(config);
    runtime_dependencies(config)
        .iter()
        .flat_map(|target| {
            deps.get(target)
                .expect("dependencies must be built by now")
                .outputs
                .iter()
                .map(|path| {
                    (
                        aliases
                            .get(target)
                            .cloned()
                            .unwrap_or_else(|| rust_name(target)),
                        path.display().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn transitive_deps(config: &Config, deps: &HashMap<String, BuildOutput>) -> Vec<(String, String)> {
    runtime_dependencies(config)
        .iter()
        .flat_map(|target| {
            deps.get(target)
                .expect("dependencies must be built by now")
                .extras
                .get(&build_output_kind::TRANSITIVE_PRODUCTS)
                .into_iter()
                .flatten()
                .filter_map(|d| {
                    let mut components = d.splitn(2, ':');
                    Some((
                        components.next()?.to_string(),
                        components.next()?.to_string(),
                    ))
                })
                .collect::<Vec<_>>()
        })
        .chain(libraries(config, deps))
        .collect()
}

fn rustc_env(config: &Config) -> Vec<(String, String)> {
    config
        .get(config_extra_keys::RUSTC_ENV)
        .iter()
        .filter_map(|encoded| {
            let (key, value) = encoded.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn library_output_file(
    working_directory: &Path,
    crate_name: &str,
    metadata: &str,
    crate_type: &str,
) -> PathBuf {
    match crate_type {
        "proc-macro" | "cdylib" | "dylib" => {
            working_directory.join(format!("lib{crate_name}-{metadata}.{}", dylib_extension()))
        }
        "staticlib" => working_directory.join(format!("lib{crate_name}-{metadata}.a")),
        _ => working_directory.join(format!("lib{crate_name}-{metadata}.rlib")),
    }
}

fn dylib_extension() -> &'static str {
    match std::env::consts::OS {
        "macos" => "dylib",
        "windows" => "dll",
        _ => "so",
    }
}

fn metadata_from_working_directory(working_directory: &Path) -> String {
    working_directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once('-').map(|(_, metadata)| metadata))
        .unwrap_or("cbs")
        .to_string()
}

fn run_process(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    working_directory: &Path,
    tool_paths: &HashMap<String, String>,
) -> Result<(), String> {
    if !is_declared_bare_tool(program, tool_paths) {
        diagnose_command(program, "rust plugin action").map_err(|e| e.to_string())?;
    }
    let program = resolved_program(program, working_directory, tool_paths)?;
    let bin_dir = materialize_declared_tools(working_directory, tool_paths)?;
    let tmpdir = working_directory.join("tmp");
    let home = working_directory.join("home");
    std::fs::create_dir_all(&tmpdir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let output = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", bin_dir)
        .env("TMPDIR", tmpdir)
        .env("HOME", home)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut message = format!("process exited with {}", output.status);
    if !stdout.trim().is_empty() {
        message.push_str("\nstdout:\n");
        message.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        message.push_str("\nstderr:\n");
        message.push_str(stderr.trim_end());
    }
    Err(message)
}

fn is_declared_bare_tool(program: &Path, tool_paths: &HashMap<String, String>) -> bool {
    program.components().count() == 1
        && program
            .to_str()
            .is_some_and(|name| tool_paths.contains_key(name))
}

fn resolved_program(
    program: &Path,
    working_directory: &Path,
    tool_paths: &HashMap<String, String>,
) -> Result<PathBuf, String> {
    if program.components().count() != 1 {
        return Ok(program.to_path_buf());
    }
    let Some(name) = program.to_str() else {
        return Ok(program.to_path_buf());
    };
    if tool_paths.contains_key(name) {
        return Ok(materialize_declared_tools(working_directory, tool_paths)?.join(name));
    }
    Ok(program.to_path_buf())
}

fn materialize_declared_tools(
    working_directory: &Path,
    tool_paths: &HashMap<String, String>,
) -> Result<PathBuf, String> {
    let bin_dir = working_directory.join(".cbs-tools");
    std::fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
    for (name, path) in tool_paths {
        let link = bin_dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        }
        symlink_or_copy(Path::new(path), &link).map_err(|e| e.to_string())?;
    }
    Ok(bin_dir)
}

#[cfg(unix)]
fn symlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn symlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

fn diagnose_command(program: &Path, context: &str) -> std::io::Result<()> {
    if program.components().count() == 1 {
        return report_tool_violation(format!(
            "{context} uses undeclared bare host tool `{}`",
            program.display()
        ));
    }

    if is_known_host_tool_path(program) {
        return report_tool_violation(format!(
            "{context} uses undeclared host tool path `{}`",
            program.display()
        ));
    }

    Ok(())
}

fn report_tool_violation(message: String) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("non-hermetic tool use rejected: {message}"),
    ))
}

fn is_known_host_tool_path(program: &Path) -> bool {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(Path::new)
        .any(|dir| program.starts_with(dir))
}

struct NativeStaticLib {
    name: String,
    sources: Vec<String>,
    include_dirs: Vec<String>,
    flags: Vec<String>,
}

struct NativeStaticLibOutput {
    name: String,
    path: PathBuf,
}

fn build_native_static_libs(
    config: &Config,
    working_directory: &Path,
    tool_paths: &HashMap<String, String>,
) -> std::io::Result<Vec<NativeStaticLibOutput>> {
    let crate_root = match config.get(config_extra_keys::CRATE_ROOT).first() {
        Some(root) => PathBuf::from(root),
        None => return Ok(Vec::new()),
    };

    let native_dir = working_directory.join("native");
    std::fs::create_dir_all(&native_dir)?;

    config
        .get(config_extra_keys::NATIVE_STATIC_LIBS)
        .iter()
        .map(|encoded| {
            let lib = parse_native_static_lib(encoded)?;
            let lib_dir = native_dir.join(&lib.name);
            std::fs::create_dir_all(&lib_dir)?;

            let mut objects = Vec::new();
            for (idx, source) in lib.sources.iter().enumerate() {
                let source_path = crate_root.join(source);
                let object_path = lib_dir.join(format!("{idx}-{}.o", sanitize_path(source)));
                let mut args = vec![
                    "-c".to_string(),
                    source_path.to_string_lossy().to_string(),
                    "-o".to_string(),
                    object_path.to_string_lossy().to_string(),
                ];
                for include_dir in &lib.include_dirs {
                    args.push("-I".to_string());
                    args.push(crate_root.join(include_dir).to_string_lossy().to_string());
                }
                args.extend(lib.flags.iter().cloned());
                run_process(Path::new("cc"), &args, &[], working_directory, tool_paths)
                    .map_err(std::io::Error::other)?;
                objects.push(object_path);
            }

            let archive_path = lib_dir.join(format!("lib{}.a", lib.name));
            let mut args = vec![
                "crs".to_string(),
                archive_path.to_string_lossy().to_string(),
            ];
            args.extend(
                objects
                    .iter()
                    .map(|object| object.to_string_lossy().to_string()),
            );
            run_process(Path::new("ar"), &args, &[], working_directory, tool_paths)
                .map_err(std::io::Error::other)?;

            Ok(NativeStaticLibOutput {
                name: lib.name,
                path: archive_path,
            })
        })
        .collect()
}

fn parse_native_static_lib(encoded: &str) -> std::io::Result<NativeStaticLib> {
    let mut parts = encoded.split('|');
    let name = parts.next().unwrap_or_default();
    if name.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "native static lib is missing a name",
        ));
    }

    Ok(NativeStaticLib {
        name: name.to_string(),
        sources: split_recipe_list(parts.next().unwrap_or_default()),
        include_dirs: split_recipe_list(parts.next().unwrap_or_default()),
        flags: split_recipe_list(parts.next().unwrap_or_default()),
    })
}

fn split_recipe_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn sanitize_path(path: &str) -> String {
    path.replace(['/', '.', '-'], "_")
}

fn native_link_args(libs: &[NativeStaticLibOutput]) -> Vec<String> {
    libs.iter()
        .flat_map(|lib| {
            vec![
                "-L".to_string(),
                format!(
                    "native={}",
                    lib.path
                        .parent()
                        .expect("native lib must have a parent")
                        .to_string_lossy()
                ),
                "-l".to_string(),
                format!("static={}", lib.name),
            ]
        })
        .collect()
}

fn native_link_args_from_products(products: &[(String, String)]) -> Vec<String> {
    products
        .iter()
        .filter_map(|(name, path)| {
            Some((
                name.strip_prefix("native_")?,
                Path::new(path).parent()?.to_string_lossy(),
            ))
        })
        .flat_map(|(name, parent)| {
            vec![
                "-L".to_string(),
                format!("native={parent}"),
                "-l".to_string(),
                format!("static={name}"),
            ]
        })
        .collect()
}
