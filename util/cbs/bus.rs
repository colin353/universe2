use std::collections::HashMap;

use crate::plugins::plugin_kind;
use cbs_plugin_sdk::*;

pub fn cbs_plugin_v1() -> CbsPluginV1 {
    CbsPluginV1 {
        abi_version: CBS_PLUGIN_ABI_VERSION,
        manifest: bus_manifest,
        initialize: empty_plugin_initialize,
        parse_rule: bus_parse_rule,
        build: bus_build,
        plan_dependencies: bus_plan_dependencies,
        resolve_target: bus_resolve_target,
        free_buffer: free_owned_buffer,
    }
}

extern "C" fn bus_manifest() -> CbsOwnedBuffer {
    CbsOwnedBuffer::from_vec(encode_plugin_manifest(&PluginManifest {
        name: "bus".to_string(),
        rule_kinds: vec![plugin_kind::RUST_BUS_LIBRARY.to_string()],
        test_rule_kinds: Vec::new(),
        build_plugins: vec!["@bus_plugin".to_string()],
        label_fields: vec!["bus_runtime".to_string(), "codegen".to_string()],
        dependency_ecosystems: Vec::new(),
        target_prefixes: Vec::new(),
    }))
}

extern "C" fn bus_parse_rule(request: CbsSlice) -> CbsOwnedBuffer {
    let response = match decode_parse_rule_request(unsafe { request.as_slice() }) {
        Ok(request) => parse_bus_rule_request(request),
        Err(e) => ParseRuleResponse::Failure(format!("failed to decode parse-rule request: {e}")),
    };
    CbsOwnedBuffer::from_vec(encode_parse_rule_response(&response))
}

extern "C" fn bus_build(request: CbsSlice) -> CbsOwnedBuffer {
    let response = match decode_build_request(unsafe { request.as_slice() }) {
        Ok(request) => build_bus_request(request),
        Err(e) => BuildResponse::Failure(format!("failed to decode build request: {e}")),
    };
    CbsOwnedBuffer::from_vec(encode_build_response(&response))
}

extern "C" fn bus_plan_dependencies(_request: CbsSlice) -> CbsOwnedBuffer {
    CbsOwnedBuffer::from_vec(encode_plan_dependencies_response(
        &PlanDependenciesResponse::Failure("bus plugin does not plan dependencies".to_string()),
    ))
}

extern "C" fn bus_resolve_target(_request: CbsSlice) -> CbsOwnedBuffer {
    CbsOwnedBuffer::from_vec(encode_resolve_target_response(
        &ResolveTargetResponse::Failure("bus plugin does not resolve targets".to_string()),
    ))
}

fn parse_bus_rule_request(request: ParseRuleRequest) -> ParseRuleResponse {
    let mut dependencies = request.dependencies;
    let bus_runtime = request
        .label_fields
        .get("bus_runtime")
        .cloned()
        .unwrap_or_else(|| "//util/bus:bus".to_string());
    if !dependencies.iter().any(|dep| dep == &bus_runtime) {
        dependencies.push(bus_runtime);
    }

    let mut external_requirements = request.cargo_requirements;
    if !external_requirements
        .iter()
        .any(|requirement| requirement.package == "futures")
    {
        external_requirements.push(ExternalRequirement {
            ecosystem: "cargo".to_string(),
            package: "futures".to_string(),
            version: "=0.3.31".to_string(),
            features: vec!["std".to_string()],
            default_features: false,
            target: Some("cargo://futures".to_string()),
        });
    }

    let codegen = request
        .label_fields
        .get("codegen")
        .cloned()
        .unwrap_or_else(|| "//util/bus/codegen:codegen".to_string());
    if request.sources.len() != 1 {
        return ParseRuleResponse::Failure(
            "rust_bus_library currently expects exactly one .bus source".to_string(),
        );
    }

    let mut extras = HashMap::new();
    extras.insert(config_extra_keys::CRATE_NAME, vec![request.name]);
    extras.insert(
        config_extra_keys::EDITION,
        vec![request
            .string_fields
            .get("edition")
            .cloned()
            .unwrap_or_else(|| "2021".to_string())],
    );

    ParseRuleResponse::Success(Config {
        dependencies,
        external_requirements,
        build_plugin: "@bus_plugin".to_string(),
        sources: request.sources,
        build_dependencies: vec!["@rust_compiler".to_string(), codegen],
        kind: request.kind,
        extras,
        ..Default::default()
    })
}

fn build_bus_request(request: BuildRequest) -> BuildResponse {
    let mut config = request.config;
    if config.kind != plugin_kind::RUST_BUS_LIBRARY {
        return BuildResponse::Failure(format!("unsupported target kind: {:?}", config.kind));
    }

    let codegen_target = match config.build_dependencies.get(1) {
        Some(target) => target,
        None => {
            return BuildResponse::Failure(
                "rust_bus_library requires a bus codegen build dependency".to_string(),
            )
        }
    };
    let codegen = match request
        .dependencies
        .get(codegen_target)
        .and_then(|output| output.outputs.first())
    {
        Some(path) => path,
        None => {
            return BuildResponse::Failure(format!(
                "bus codegen dependency {codegen_target} did not produce an executable"
            ))
        }
    };
    let bus_source = match config.sources.as_slice() {
        [source] => source.clone(),
        _ => {
            return BuildResponse::Failure(
                "rust_bus_library currently expects exactly one .bus source".to_string(),
            )
        }
    };

    let crate_name = config
        .get(config_extra_keys::CRATE_NAME)
        .first()
        .cloned()
        .unwrap_or_else(|| target_shortname(&request.target).replace('-', "_"));
    if let Err(e) = std::fs::create_dir_all(&request.working_directory) {
        return BuildResponse::Failure(format!("failed to create working directory: {e}"));
    }
    let generated_source = request.working_directory.join(format!("{crate_name}.rs"));
    let output = match std::process::Command::new(codegen)
        .arg("--language=rust")
        .arg(&bus_source)
        .output()
    {
        Ok(output) => output,
        Err(e) => return BuildResponse::Failure(format!("failed to run bus codegen:\n{e}")),
    };
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut message = format!("bus codegen exited with {}", output.status);
        if !stdout.trim().is_empty() {
            message.push_str("\nstdout:\n");
            message.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            message.push_str("\nstderr:\n");
            message.push_str(stderr.trim_end());
        }
        return BuildResponse::Failure(message);
    }
    if let Err(e) = std::fs::write(&generated_source, output.stdout) {
        return BuildResponse::Failure(format!(
            "failed to write generated rust source {}: {e}",
            generated_source.display()
        ));
    }

    config.build_plugin = "@rust_plugin".to_string();
    config.kind = plugin_kind::RUST_LIBRARY.to_string();
    config.sources = vec![generated_source.to_string_lossy().to_string()];
    config.build_dependencies = vec!["@rust_compiler".to_string()];
    config.extras.insert(
        config_extra_keys::ROOT_SOURCE,
        vec![generated_source.to_string_lossy().to_string()],
    );

    BuildResponse::Delegate(config)
}

fn target_shortname(target: &str) -> &str {
    target
        .split("//")
        .last()
        .and_then(|s| s.split('/').last())
        .and_then(|s| s.split(':').last())
        .unwrap_or(target)
}
