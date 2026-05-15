#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const CBS_PLUGIN_ABI_VERSION: u32 = 2;

pub mod config_extra_keys {
    pub const FEATURES: u32 = 0;
    pub const CRATE_NAME: u32 = 1;
    pub const CRATE_TYPE: u32 = 2;
    pub const EDITION: u32 = 3;
    pub const ROOT_SOURCE: u32 = 4;
    pub const DEPENDENCY_ALIASES: u32 = 5;
    pub const RUSTC_CFGS: u32 = 6;
    pub const CRATE_ROOT: u32 = 7;
    pub const NATIVE_STATIC_LIBS: u32 = 8;
    pub const RUSTC_ENV: u32 = 9;
}

pub mod build_output_kind {
    pub const TRANSITIVE_PRODUCTS: u32 = 0;
}

pub mod build_config_key {
    pub const TARGET_FAMILY: u32 = 1;
    pub const TARGET_ENV: u32 = 2;
    pub const TARGET_OS: u32 = 3;
    pub const TARGET_ARCH: u32 = 4;
    pub const TARGET_VENDOR: u32 = 5;
    pub const TARGET_ENDIAN: u32 = 6;
    pub const MACOS_DEVELOPER_DIR: u32 = 7;
    pub const MACOS_SDK_PATH: u32 = 8;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CbsSlice {
    pub ptr: *const u8,
    pub len: usize,
}

impl CbsSlice {
    pub fn from_slice(slice: &[u8]) -> Self {
        Self {
            ptr: slice.as_ptr(),
            len: slice.len(),
        }
    }

    pub unsafe fn as_slice<'a>(&self) -> &'a [u8] {
        std::slice::from_raw_parts(self.ptr, self.len)
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct CbsOwnedBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl CbsOwnedBuffer {
    pub fn from_vec(mut value: Vec<u8>) -> Self {
        let buffer = Self {
            ptr: value.as_mut_ptr(),
            len: value.len(),
            cap: value.capacity(),
        };
        std::mem::forget(value);
        buffer
    }

    pub unsafe fn into_vec(self) -> Vec<u8> {
        Vec::from_raw_parts(self.ptr, self.len, self.cap)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CbsPluginV1 {
    pub abi_version: u32,
    pub manifest: extern "C" fn() -> CbsOwnedBuffer,
    pub initialize: extern "C" fn(CbsSlice) -> CbsOwnedBuffer,
    pub parse_rule: extern "C" fn(CbsSlice) -> CbsOwnedBuffer,
    pub build: extern "C" fn(CbsSlice) -> CbsOwnedBuffer,
    pub plan_dependencies: extern "C" fn(CbsSlice) -> CbsOwnedBuffer,
    pub resolve_target: extern "C" fn(CbsSlice) -> CbsOwnedBuffer,
    pub free_buffer: extern "C" fn(CbsOwnedBuffer),
}

pub extern "C" fn free_owned_buffer(buffer: CbsOwnedBuffer) {
    if buffer.ptr.is_null() {
        return;
    }
    unsafe {
        drop(buffer.into_vec());
    }
}

pub extern "C" fn empty_plugin_initialize(_request: CbsSlice) -> CbsOwnedBuffer {
    CbsOwnedBuffer::from_vec(encode_plugin_init_response(&PluginInitResponse::Success(
        PluginInit {
            tools: Vec::new(),
            fingerprints: Vec::new(),
        },
    )))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub dependencies: Vec<String>,
    pub external_requirements: Vec<ExternalRequirement>,
    pub build_plugin: String,
    pub location: Option<String>,
    pub sources: Vec<String>,
    pub build_dependencies: Vec<String>,
    pub kind: String,
    pub extras: HashMap<u32, Vec<String>>,
}

impl Config {
    pub fn get(&self, key: u32) -> &[String] {
        self.extras.get(&key).map(|s| s.as_slice()).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalRequirement {
    pub ecosystem: String,
    pub package: String,
    pub version: String,
    pub features: Vec<String>,
    pub default_features: bool,
    pub target: Option<String>,
}

impl ExternalRequirement {
    pub fn target(&self) -> String {
        self.target
            .clone()
            .unwrap_or_else(|| format!("{}://{}", self.ecosystem, self.package))
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct BuildOutput {
    pub outputs: Vec<PathBuf>,
    pub extras: HashMap<u32, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BuildRequest {
    pub target: String,
    pub config: Config,
    pub dependencies: HashMap<String, BuildOutput>,
    pub target_config: HashMap<u32, String>,
    pub working_directory: PathBuf,
    pub tool_paths: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildResponse {
    Success(BuildOutput),
    Delegate(Config),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseRuleRequest {
    pub workspace_root: PathBuf,
    pub package: String,
    pub package_dir: PathBuf,
    pub kind: String,
    pub name: String,
    pub sources: Vec<String>,
    pub dependencies: Vec<String>,
    pub cargo_requirements: Vec<ExternalRequirement>,
    pub string_fields: HashMap<String, String>,
    pub label_fields: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseRuleResponse {
    Success(Config),
    Failure(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PluginContext {
    pub cache_dir: PathBuf,
    pub context_hash: u64,
    pub target_config: HashMap<u32, String>,
    pub tool_paths: HashMap<String, String>,
    pub lockfile: HashMap<String, String>,
    pub locked_dependencies: HashMap<String, HashMap<String, String>>,
    pub target: Option<String>,
}

impl PluginContext {
    pub fn get_config(&self, key: u32) -> Option<&str> {
        self.target_config.get(&key).map(|value| value.as_str())
    }

    pub fn get_locked_version(&self, target: &str) -> std::io::Result<String> {
        self.lockfile.get(target).cloned().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{target} does not have a lockfile entry!"),
            )
        })
    }

    pub fn get_locked_dependency(&self, target: &str, package: &str) -> Option<String> {
        self.locked_dependencies
            .get(target)
            .and_then(|dependencies| dependencies.get(package))
            .cloned()
    }

    pub fn with_target(mut self, target: String) -> Self {
        self.target = Some(target);
        self
    }

    pub fn working_directory(&self) -> PathBuf {
        match self.target.as_ref() {
            Some(target) => {
                let version = self.get_locked_version(target).unwrap_or_default();
                self.cache_dir.join("resolve").join(format!(
                    "{}-{}",
                    to_dir(target),
                    version_dir(&version)
                ))
            }
            None => self.cache_dir.clone(),
        }
    }

    pub fn run_tool<S>(&self, tool: &str, args: &[S]) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
    {
        self.run_tool_with_env(tool, args, std::iter::empty::<(&str, &str)>())
    }

    pub fn run_tool_with_env<S, E, K, V>(
        &self,
        tool: &str,
        args: &[S],
        env: E,
    ) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let Some(path) = self.tool_paths.get(tool) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("tool {tool:?} is not declared"),
            ));
        };
        let bin = materialize_declared_tools(self)?.join(tool);
        if !bin.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("declared tool {tool:?} is not available at {path}"),
            ));
        }
        self.run_resolved_process(bin, args, env, true)
    }

    pub fn run_process<P: Into<PathBuf>, S>(&self, bin: P, args: &[S]) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
    {
        let bin = bin.into();
        if bin.components().count() == 1 {
            if let Some(tool) = bin.to_str().filter(|tool| self.tool_paths.contains_key(*tool)) {
                return self.run_tool(tool, args);
            }
        }
        let bin = resolve_command(bin)?;
        self.run_resolved_process(bin, args, std::iter::empty::<(&str, &str)>(), true)
    }

    fn run_resolved_process<S, E, K, V>(
        &self,
        bin: PathBuf,
        args: &[S],
        env: E,
        hermetic: bool,
    ) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut command = std::process::Command::new(&bin);
        if hermetic {
            configure_hermetic_env(self, &mut command)?;
        }
        let mut cmd_debug = bin.to_string_lossy().to_string();
        for (key, value) in env {
            cmd_debug.push(' ');
            cmd_debug.push_str(key.as_ref());
            cmd_debug.push_str("=<env>");
            command.env(key.as_ref(), value.as_ref());
        }
        for arg in args {
            cmd_debug.push(' ');
            cmd_debug.push_str(arg.as_ref());
            command.arg(arg.as_ref());
        }
        eprintln!(
            "[cbs] action {}: {}",
            self.target.as_deref().unwrap_or("workspace"),
            command_name(&cmd_debug)
        );
        let output = command.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut message = format!(
                "command exited with {}\ncommand: {cmd_debug}",
                output.status
            );
            if !stderr.trim().is_empty() {
                message.push_str("\nstderr:\n");
                message.push_str(stderr.trim_end());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            ));
        }
        Ok(output.stdout)
    }

}

fn configure_hermetic_env(
    context: &PluginContext,
    command: &mut std::process::Command,
) -> std::io::Result<()> {
    let bin_dir = materialize_declared_tools(context)?;
    command.env_clear();
    command.env("PATH", &bin_dir);
    command.env("TMPDIR", context.working_directory().join("tmp"));
    command.env("HOME", context.working_directory().join("home"));
    Ok(())
}

fn materialize_declared_tools(context: &PluginContext) -> std::io::Result<PathBuf> {
    let bin_dir = tool_bin_dir(context)?;
    std::fs::create_dir_all(&bin_dir)?;
    std::fs::create_dir_all(context.working_directory().join("tmp"))?;
    std::fs::create_dir_all(context.working_directory().join("home"))?;
    for (name, path) in &context.tool_paths {
        let link = bin_dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link)?;
        }
        symlink_or_copy(Path::new(path), &link)?;
    }
    Ok(bin_dir)
}

fn tool_bin_dir(context: &PluginContext) -> std::io::Result<PathBuf> {
    Ok(context
        .working_directory()
        .join(".cbs-tools")
        .join(format!("{:016x}", context.context_hash)))
}

#[cfg(unix)]
fn symlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn symlink_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

fn resolve_command(bin: PathBuf) -> std::io::Result<PathBuf> {
    if bin.components().count() != 1 {
        diagnose_command(&bin, "plugin action")?;
        return Ok(bin);
    }
    diagnose_command(&bin, "plugin action")?;
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "plugin action uses undeclared bare host tool `{}`; declare the tool in WORKSPACE.ccl and call PluginContext::run_tool",
            bin.display()
        ),
    ))
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

fn diagnose_host_path_search(tool: &str, dirs: &[&str]) -> std::io::Result<()> {
    report_tool_violation(format!(
        "searching host paths [{}] for undeclared tool `{tool}`",
        dirs.join(", ")
    ))
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

fn to_dir(s: &str) -> String {
    s.replace("/", "_")
        .replace(":", "_")
        .replace("@", "_")
        .replace(".", "_")
        .trim_matches('_')
        .to_string()
}

fn version_dir(s: &str) -> String {
    if s.len() <= 64 {
        return s.to_string();
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:x}")
}

fn command_name(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .and_then(|bin| Path::new(bin).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DependencyPlan {
    pub lockfile: HashMap<String, String>,
    pub locked_dependencies: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDependenciesRequest {
    pub ecosystem: String,
    pub requirements: Vec<ExternalRequirement>,
    pub context: PluginContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDependenciesResponse {
    Success(DependencyPlan),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveTargetRequest {
    pub target: String,
    pub context: PluginContext,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveTargetResponse {
    Success(Config),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: String,
    pub rule_kinds: Vec<String>,
    pub test_rule_kinds: Vec<String>,
    pub build_plugins: Vec<String>,
    pub label_fields: Vec<String>,
    pub dependency_ecosystems: Vec<String>,
    pub target_prefixes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInitRequest {
    pub name: String,
    pub workspace_root: PathBuf,
    pub cache_dir: PathBuf,
    pub target_config: HashMap<u32, String>,
    pub parameters: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInit {
    pub tools: Vec<ToolRequirement>,
    pub fingerprints: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequirement {
    pub name: String,
    pub kind: String,
    pub path: PathBuf,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginInitResponse {
    Success(PluginInit),
    Failure(String),
}

pub fn encode_config(config: &Config) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_config(&mut fbb, config);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_config(bytes: &[u8]) -> std::io::Result<Config> {
    let config = unsafe { flatbuffers::root_unchecked::<fb::Config<'_>>(bytes) };
    Ok(fb::read_config(config))
}

pub fn encode_build_output(output: &BuildOutput) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_build_output(&mut fbb, output);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_build_output(bytes: &[u8]) -> std::io::Result<BuildOutput> {
    let output = unsafe { flatbuffers::root_unchecked::<fb::BuildOutput<'_>>(bytes) };
    Ok(fb::read_build_output(output))
}

pub fn encode_build_request(request: &BuildRequest) -> Vec<u8> {
    encode_build_request_parts(
        &request.target,
        &request.config,
        &request.dependencies,
        &request.target_config,
        &request.working_directory,
        &request.tool_paths,
    )
}

pub fn encode_build_request_parts(
    target: &str,
    config: &Config,
    dependencies: &HashMap<String, BuildOutput>,
    target_config: &HashMap<u32, String>,
    working_directory: &Path,
    tool_paths: &HashMap<String, String>,
) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_build_request_parts(
        &mut fbb,
        target,
        config,
        dependencies,
        target_config,
        working_directory,
        tool_paths,
    );
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_build_request(bytes: &[u8]) -> std::io::Result<BuildRequest> {
    let request = unsafe { flatbuffers::root_unchecked::<fb::BuildRequest<'_>>(bytes) };
    Ok(fb::read_build_request(request))
}

pub fn encode_build_response(response: &BuildResponse) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_build_response(&mut fbb, response);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_build_response(bytes: &[u8]) -> std::io::Result<BuildResponse> {
    let response = unsafe { flatbuffers::root_unchecked::<fb::BuildResponse<'_>>(bytes) };
    Ok(fb::read_build_response(response))
}

pub fn encode_parse_rule_request(request: &ParseRuleRequest) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_parse_rule_request(&mut fbb, request);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_parse_rule_request(bytes: &[u8]) -> std::io::Result<ParseRuleRequest> {
    let request = unsafe { flatbuffers::root_unchecked::<fb::ParseRuleRequest<'_>>(bytes) };
    Ok(fb::read_parse_rule_request(request))
}

pub fn encode_parse_rule_response(response: &ParseRuleResponse) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_parse_rule_response(&mut fbb, response);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_parse_rule_response(bytes: &[u8]) -> std::io::Result<ParseRuleResponse> {
    let response = unsafe { flatbuffers::root_unchecked::<fb::ParseRuleResponse<'_>>(bytes) };
    Ok(fb::read_parse_rule_response(response))
}

pub fn encode_plan_dependencies_request(request: &PlanDependenciesRequest) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_plan_dependencies_request(&mut fbb, request);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_plan_dependencies_request(bytes: &[u8]) -> std::io::Result<PlanDependenciesRequest> {
    let request = unsafe { flatbuffers::root_unchecked::<fb::PlanDependenciesRequest<'_>>(bytes) };
    Ok(fb::read_plan_dependencies_request(request))
}

pub fn encode_plan_dependencies_response(response: &PlanDependenciesResponse) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_plan_dependencies_response(&mut fbb, response);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_plan_dependencies_response(
    bytes: &[u8],
) -> std::io::Result<PlanDependenciesResponse> {
    let response =
        unsafe { flatbuffers::root_unchecked::<fb::PlanDependenciesResponse<'_>>(bytes) };
    Ok(fb::read_plan_dependencies_response(response))
}

pub fn encode_resolve_target_request(request: &ResolveTargetRequest) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_resolve_target_request(&mut fbb, request);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_resolve_target_request(bytes: &[u8]) -> std::io::Result<ResolveTargetRequest> {
    let request = unsafe { flatbuffers::root_unchecked::<fb::ResolveTargetRequest<'_>>(bytes) };
    Ok(fb::read_resolve_target_request(request))
}

pub fn encode_resolve_target_response(response: &ResolveTargetResponse) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_resolve_target_response(&mut fbb, response);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_resolve_target_response(bytes: &[u8]) -> std::io::Result<ResolveTargetResponse> {
    let response = unsafe { flatbuffers::root_unchecked::<fb::ResolveTargetResponse<'_>>(bytes) };
    Ok(fb::read_resolve_target_response(response))
}

pub fn encode_plugin_manifest(manifest: &PluginManifest) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let root = fb::create_plugin_manifest(&mut fbb, manifest);
    fbb.finish_minimal(root);
    fbb.finished_data().to_vec()
}

pub fn decode_plugin_manifest(bytes: &[u8]) -> std::io::Result<PluginManifest> {
    let manifest = unsafe { flatbuffers::root_unchecked::<fb::PluginManifest<'_>>(bytes) };
    Ok(fb::read_plugin_manifest(manifest))
}

pub fn read_plugin_manifest(plugin: CbsPluginV1) -> std::io::Result<PluginManifest> {
    let buffer = (plugin.manifest)();
    let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len).to_vec() };
    (plugin.free_buffer)(buffer);
    decode_plugin_manifest(&bytes)
}

pub fn encode_plugin_init_request(request: &PluginInitRequest) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_string(&mut bytes, &request.name);
    write_string(&mut bytes, &request.workspace_root.display().to_string());
    write_string(&mut bytes, &request.cache_dir.display().to_string());
    write_u32(&mut bytes, request.target_config.len() as u32);
    for (key, value) in &request.target_config {
        write_u32(&mut bytes, *key);
        write_string(&mut bytes, value);
    }
    write_u32(&mut bytes, request.parameters.len() as u32);
    for (key, value) in &request.parameters {
        write_string(&mut bytes, key);
        write_string(&mut bytes, value);
    }
    bytes
}

pub fn decode_plugin_init_request(bytes: &[u8]) -> std::io::Result<PluginInitRequest> {
    let mut reader = BinaryReader { bytes, position: 0 };
    let name = reader.read_string()?;
    let workspace_root = PathBuf::from(reader.read_string()?);
    let cache_dir = PathBuf::from(reader.read_string()?);
    let target_config = reader.read_u32_string_map()?;
    let parameters = reader.read_string_map()?;
    reader.finish()?;
    Ok(PluginInitRequest {
        name,
        workspace_root,
        cache_dir,
        target_config,
        parameters,
    })
}

pub fn encode_plugin_init_response(response: &PluginInitResponse) -> Vec<u8> {
    let mut bytes = Vec::new();
    match response {
        PluginInitResponse::Success(init) => {
            bytes.push(1);
            write_u32(&mut bytes, init.tools.len() as u32);
            for tool in &init.tools {
                write_string(&mut bytes, &tool.name);
                write_string(&mut bytes, &tool.kind);
                write_string(&mut bytes, &tool.path.display().to_string());
                write_string(&mut bytes, &tool.fingerprint);
            }
            write_u32(&mut bytes, init.fingerprints.len() as u32);
            for (key, value) in &init.fingerprints {
                write_string(&mut bytes, key);
                write_string(&mut bytes, value);
            }
        }
        PluginInitResponse::Failure(error) => {
            bytes.push(0);
            write_string(&mut bytes, error);
        }
    }
    bytes
}

pub fn decode_plugin_init_response(bytes: &[u8]) -> std::io::Result<PluginInitResponse> {
    let mut reader = BinaryReader { bytes, position: 0 };
    let success = reader.read_byte()?;
    let response = match success {
        0 => PluginInitResponse::Failure(reader.read_string()?),
        1 => {
            let mut tools = Vec::new();
            for _ in 0..reader.read_u32()? {
                tools.push(ToolRequirement {
                    name: reader.read_string()?,
                    kind: reader.read_string()?,
                    path: PathBuf::from(reader.read_string()?),
                    fingerprint: reader.read_string()?,
                });
            }
            let mut fingerprints = Vec::new();
            for _ in 0..reader.read_u32()? {
                fingerprints.push((reader.read_string()?, reader.read_string()?));
            }
            PluginInitResponse::Success(PluginInit {
                tools,
                fingerprints,
            })
        }
        value => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid plugin init response tag {value}"),
            ))
        }
    };
    reader.finish()?;
    Ok(response)
}

fn write_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn write_string(bytes: &mut Vec<u8>, value: &str) {
    write_u32(bytes, value.len() as u32);
    bytes.extend(value.as_bytes());
}

struct BinaryReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BinaryReader<'a> {
    fn read_byte(&mut self) -> std::io::Result<u8> {
        if self.position >= self.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected end of plugin init message",
            ));
        }
        let value = self.bytes[self.position];
        self.position += 1;
        Ok(value)
    }

    fn read_u32(&mut self) -> std::io::Result<u32> {
        if self.position + 4 > self.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected end of plugin init message",
            ));
        }
        let mut value = [0; 4];
        value.copy_from_slice(&self.bytes[self.position..self.position + 4]);
        self.position += 4;
        Ok(u32::from_le_bytes(value))
    }

    fn read_string(&mut self) -> std::io::Result<String> {
        let len = self.read_u32()? as usize;
        if self.position + len > self.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected end of plugin init string",
            ));
        }
        let value = std::str::from_utf8(&self.bytes[self.position..self.position + len])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            .to_string();
        self.position += len;
        Ok(value)
    }

    fn read_u32_string_map(&mut self) -> std::io::Result<HashMap<u32, String>> {
        let mut values = HashMap::new();
        for _ in 0..self.read_u32()? {
            values.insert(self.read_u32()?, self.read_string()?);
        }
        Ok(values)
    }

    fn read_string_map(&mut self) -> std::io::Result<HashMap<String, String>> {
        let mut values = HashMap::new();
        for _ in 0..self.read_u32()? {
            values.insert(self.read_string()?, self.read_string()?);
        }
        Ok(values)
    }

    fn finish(self) -> std::io::Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "plugin init message has trailing bytes",
            ))
        }
    }
}

mod fb {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use flatbuffers::{
        FlatBufferBuilder, Follow, ForwardsUOffset, Table, TableFinishedWIPOffset,
        TableUnfinishedWIPOffset, VOffsetT, Vector, WIPOffset,
    };

    use crate::{
        BuildOutput as CoreBuildOutput, BuildRequest as CoreBuildRequest,
        BuildResponse as CoreBuildResponse, Config as CoreConfig,
        DependencyPlan as CoreDependencyPlan, ExternalRequirement,
        ParseRuleRequest as CoreParseRuleRequest, ParseRuleResponse as CoreParseRuleResponse,
        PlanDependenciesRequest as CorePlanDependenciesRequest,
        PlanDependenciesResponse as CorePlanDependenciesResponse,
        PluginContext as CorePluginContext, PluginManifest as CorePluginManifest,
        ResolveTargetRequest as CoreResolveTargetRequest,
        ResolveTargetResponse as CoreResolveTargetResponse,
    };

    const VT_FIRST: VOffsetT = 4;

    macro_rules! table_type {
        ($name:ident) => {
            #[derive(Copy, Clone, Debug)]
            pub struct $name<'a> {
                table: Table<'a>,
            }

            impl<'a> Follow<'a> for $name<'a> {
                type Inner = $name<'a>;

                unsafe fn follow(buf: &'a [u8], loc: usize) -> Self::Inner {
                    Self {
                        table: Table::new(buf, loc),
                    }
                }
            }
        };
    }

    table_type!(Extra);
    table_type!(StringField);
    table_type!(PluginManifest);
    table_type!(ExternalRequirementFb);
    table_type!(Config);
    table_type!(BuildOutput);
    table_type!(DependencyOutput);
    table_type!(BuildRequest);
    table_type!(BuildResponse);
    table_type!(ParseRuleRequest);
    table_type!(ParseRuleResponse);
    table_type!(PluginContext);
    table_type!(DependencyEdgeSet);
    table_type!(DependencyPlan);
    table_type!(PlanDependenciesRequest);
    table_type!(PlanDependenciesResponse);
    table_type!(ResolveTargetRequest);
    table_type!(ResolveTargetResponse);

    type FbStringVector<'a> = Vector<'a, ForwardsUOffset<&'a str>>;

    impl<'a> Extra<'a> {
        const VT_KEY: VOffsetT = VT_FIRST;
        const VT_VALUES: VOffsetT = VT_FIRST + 2;

        fn key(&self) -> u32 {
            unsafe { self.table.get::<u32>(Self::VT_KEY, Some(0)).unwrap_or(0) }
        }

        fn values(&self) -> Vec<String> {
            string_vector_to_vec(unsafe {
                self.table
                    .get::<ForwardsUOffset<FbStringVector<'a>>>(Self::VT_VALUES, None)
            })
        }
    }

    impl<'a> StringField<'a> {
        const VT_KEY: VOffsetT = VT_FIRST;
        const VT_VALUE: VOffsetT = VT_FIRST + 2;

        fn read(&self) -> (String, String) {
            (
                string_slot(self.table, Self::VT_KEY),
                string_slot(self.table, Self::VT_VALUE),
            )
        }
    }

    impl<'a> PluginManifest<'a> {
        const VT_NAME: VOffsetT = VT_FIRST;
        const VT_RULE_KINDS: VOffsetT = VT_FIRST + 2;
        const VT_BUILD_PLUGINS: VOffsetT = VT_FIRST + 4;
        const VT_LABEL_FIELDS: VOffsetT = VT_FIRST + 6;
        const VT_TEST_RULE_KINDS: VOffsetT = VT_FIRST + 8;
        const VT_DEPENDENCY_ECOSYSTEMS: VOffsetT = VT_FIRST + 10;
        const VT_TARGET_PREFIXES: VOffsetT = VT_FIRST + 12;
    }

    impl<'a> ExternalRequirementFb<'a> {
        const VT_ECOSYSTEM: VOffsetT = VT_FIRST;
        const VT_PACKAGE: VOffsetT = VT_FIRST + 2;
        const VT_VERSION: VOffsetT = VT_FIRST + 4;
        const VT_FEATURES: VOffsetT = VT_FIRST + 6;
        const VT_DEFAULT_FEATURES: VOffsetT = VT_FIRST + 8;
        const VT_TARGET: VOffsetT = VT_FIRST + 10;

        fn read(&self) -> ExternalRequirement {
            ExternalRequirement {
                ecosystem: string_slot(self.table, Self::VT_ECOSYSTEM),
                package: string_slot(self.table, Self::VT_PACKAGE),
                version: string_slot(self.table, Self::VT_VERSION),
                features: string_vector_to_vec(unsafe {
                    self.table
                        .get::<ForwardsUOffset<FbStringVector<'a>>>(Self::VT_FEATURES, None)
                }),
                default_features: unsafe {
                    self.table
                        .get::<bool>(Self::VT_DEFAULT_FEATURES, Some(false))
                        .unwrap_or(false)
                },
                target: optional_string_slot(self.table, Self::VT_TARGET),
            }
        }
    }

    pub fn create_plugin_manifest<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        manifest: &CorePluginManifest,
    ) -> WIPOffset<PluginManifest<'a>> {
        let name = fbb.create_string(&manifest.name);
        let rule_kinds = create_string_vector(fbb, &manifest.rule_kinds);
        let test_rule_kinds = create_string_vector(fbb, &manifest.test_rule_kinds);
        let build_plugins = create_string_vector(fbb, &manifest.build_plugins);
        let label_fields = create_string_vector(fbb, &manifest.label_fields);
        let dependency_ecosystems = create_string_vector(fbb, &manifest.dependency_ecosystems);
        let target_prefixes = create_string_vector(fbb, &manifest.target_prefixes);

        let start = fbb.start_table();
        fbb.push_slot_always(PluginManifest::VT_NAME, name);
        fbb.push_slot_always(PluginManifest::VT_RULE_KINDS, rule_kinds);
        fbb.push_slot_always(PluginManifest::VT_TEST_RULE_KINDS, test_rule_kinds);
        fbb.push_slot_always(PluginManifest::VT_BUILD_PLUGINS, build_plugins);
        fbb.push_slot_always(PluginManifest::VT_LABEL_FIELDS, label_fields);
        fbb.push_slot_always(
            PluginManifest::VT_DEPENDENCY_ECOSYSTEMS,
            dependency_ecosystems,
        );
        fbb.push_slot_always(PluginManifest::VT_TARGET_PREFIXES, target_prefixes);
        finish_table(fbb, start)
    }

    pub fn read_plugin_manifest(manifest: PluginManifest<'_>) -> CorePluginManifest {
        CorePluginManifest {
            name: string_slot(manifest.table, PluginManifest::VT_NAME),
            rule_kinds: string_vector_to_vec(unsafe {
                manifest
                    .table
                    .get::<ForwardsUOffset<FbStringVector<'_>>>(PluginManifest::VT_RULE_KINDS, None)
            }),
            test_rule_kinds: string_vector_to_vec(unsafe {
                manifest.table.get::<ForwardsUOffset<FbStringVector<'_>>>(
                    PluginManifest::VT_TEST_RULE_KINDS,
                    None,
                )
            }),
            build_plugins: string_vector_to_vec(unsafe {
                manifest.table.get::<ForwardsUOffset<FbStringVector<'_>>>(
                    PluginManifest::VT_BUILD_PLUGINS,
                    None,
                )
            }),
            label_fields: string_vector_to_vec(unsafe {
                manifest.table.get::<ForwardsUOffset<FbStringVector<'_>>>(
                    PluginManifest::VT_LABEL_FIELDS,
                    None,
                )
            }),
            dependency_ecosystems: string_vector_to_vec(unsafe {
                manifest.table.get::<ForwardsUOffset<FbStringVector<'_>>>(
                    PluginManifest::VT_DEPENDENCY_ECOSYSTEMS,
                    None,
                )
            }),
            target_prefixes: string_vector_to_vec(unsafe {
                manifest.table.get::<ForwardsUOffset<FbStringVector<'_>>>(
                    PluginManifest::VT_TARGET_PREFIXES,
                    None,
                )
            }),
        }
    }

    impl<'a> Config<'a> {
        const VT_DEPENDENCIES: VOffsetT = VT_FIRST;
        const VT_EXTERNAL_REQUIREMENTS: VOffsetT = VT_FIRST + 2;
        const VT_BUILD_PLUGIN: VOffsetT = VT_FIRST + 4;
        const VT_LOCATION: VOffsetT = VT_FIRST + 6;
        const VT_SOURCES: VOffsetT = VT_FIRST + 8;
        const VT_BUILD_DEPENDENCIES: VOffsetT = VT_FIRST + 10;
        const VT_KIND: VOffsetT = VT_FIRST + 12;
        const VT_EXTRAS: VOffsetT = VT_FIRST + 14;
    }

    impl<'a> BuildOutput<'a> {
        const VT_OUTPUTS: VOffsetT = VT_FIRST;
        const VT_EXTRAS: VOffsetT = VT_FIRST + 2;
    }

    impl<'a> DependencyOutput<'a> {
        const VT_TARGET: VOffsetT = VT_FIRST;
        const VT_OUTPUT: VOffsetT = VT_FIRST + 2;

        fn read(&self) -> (String, CoreBuildOutput) {
            let output = unsafe {
                self.table
                    .get::<ForwardsUOffset<BuildOutput<'a>>>(Self::VT_OUTPUT, None)
            }
            .map(read_build_output)
            .unwrap_or_default();
            (string_slot(self.table, Self::VT_TARGET), output)
        }
    }

    impl<'a> BuildRequest<'a> {
        const VT_TARGET: VOffsetT = VT_FIRST;
        const VT_CONFIG: VOffsetT = VT_FIRST + 2;
        const VT_DEPENDENCIES: VOffsetT = VT_FIRST + 4;
        const VT_WORKING_DIRECTORY: VOffsetT = VT_FIRST + 6;
        const VT_TOOL_PATHS: VOffsetT = VT_FIRST + 8;
        const VT_TARGET_CONFIG: VOffsetT = VT_FIRST + 10;
    }

    impl<'a> BuildResponse<'a> {
        const VT_SUCCESS: VOffsetT = VT_FIRST;
        const VT_ERROR: VOffsetT = VT_FIRST + 2;
        const VT_OUTPUT: VOffsetT = VT_FIRST + 4;
        const VT_DELEGATE_CONFIG: VOffsetT = VT_FIRST + 6;
    }

    impl<'a> ParseRuleRequest<'a> {
        const VT_WORKSPACE_ROOT: VOffsetT = VT_FIRST;
        const VT_PACKAGE: VOffsetT = VT_FIRST + 2;
        const VT_PACKAGE_DIR: VOffsetT = VT_FIRST + 4;
        const VT_KIND: VOffsetT = VT_FIRST + 6;
        const VT_NAME: VOffsetT = VT_FIRST + 8;
        const VT_SOURCES: VOffsetT = VT_FIRST + 10;
        const VT_DEPENDENCIES: VOffsetT = VT_FIRST + 12;
        const VT_CARGO_REQUIREMENTS: VOffsetT = VT_FIRST + 14;
        const VT_STRING_FIELDS: VOffsetT = VT_FIRST + 16;
        const VT_LABEL_FIELDS: VOffsetT = VT_FIRST + 18;
    }

    impl<'a> ParseRuleResponse<'a> {
        const VT_SUCCESS: VOffsetT = VT_FIRST;
        const VT_ERROR: VOffsetT = VT_FIRST + 2;
        const VT_CONFIG: VOffsetT = VT_FIRST + 4;
    }

    impl<'a> PluginContext<'a> {
        const VT_CACHE_DIR: VOffsetT = VT_FIRST;
        const VT_CONTEXT_HASH: VOffsetT = VT_FIRST + 2;
        const VT_TARGET_CONFIG: VOffsetT = VT_FIRST + 4;
        const VT_LOCKFILE: VOffsetT = VT_FIRST + 6;
        const VT_LOCKED_DEPENDENCIES: VOffsetT = VT_FIRST + 8;
        const VT_TOOL_PATHS: VOffsetT = VT_FIRST + 10;
    }

    impl<'a> DependencyEdgeSet<'a> {
        const VT_TARGET: VOffsetT = VT_FIRST;
        const VT_DEPENDENCIES: VOffsetT = VT_FIRST + 2;

        fn read(&self) -> (String, HashMap<String, String>) {
            let dependencies = read_string_fields(unsafe {
                self.table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        Self::VT_DEPENDENCIES,
                        None,
                    )
            });
            (string_slot(self.table, Self::VT_TARGET), dependencies)
        }
    }

    impl<'a> DependencyPlan<'a> {
        const VT_LOCKFILE: VOffsetT = VT_FIRST;
        const VT_LOCKED_DEPENDENCIES: VOffsetT = VT_FIRST + 2;
    }

    impl<'a> PlanDependenciesRequest<'a> {
        const VT_ECOSYSTEM: VOffsetT = VT_FIRST;
        const VT_REQUIREMENTS: VOffsetT = VT_FIRST + 2;
        const VT_CONTEXT: VOffsetT = VT_FIRST + 4;
    }

    impl<'a> PlanDependenciesResponse<'a> {
        const VT_SUCCESS: VOffsetT = VT_FIRST;
        const VT_ERROR: VOffsetT = VT_FIRST + 2;
        const VT_PLAN: VOffsetT = VT_FIRST + 4;
    }

    impl<'a> ResolveTargetRequest<'a> {
        const VT_TARGET: VOffsetT = VT_FIRST;
        const VT_CONTEXT: VOffsetT = VT_FIRST + 2;
    }

    impl<'a> ResolveTargetResponse<'a> {
        const VT_SUCCESS: VOffsetT = VT_FIRST;
        const VT_ERROR: VOffsetT = VT_FIRST + 2;
        const VT_CONFIG: VOffsetT = VT_FIRST + 4;
    }

    pub fn create_config<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        config: &CoreConfig,
    ) -> WIPOffset<Config<'a>> {
        let dependencies = create_string_vector(fbb, &config.dependencies);
        let external_requirements =
            create_external_requirement_vector(fbb, &config.external_requirements);
        let build_plugin = fbb.create_string(&config.build_plugin);
        let location = config
            .location
            .as_ref()
            .map(|location| fbb.create_string(location));
        let sources = create_string_vector(fbb, &config.sources);
        let build_dependencies = create_string_vector(fbb, &config.build_dependencies);
        let kind = fbb.create_string(&config.kind);
        let extras = create_extra_vector(fbb, &config.extras);

        let start = fbb.start_table();
        fbb.push_slot_always(Config::VT_DEPENDENCIES, dependencies);
        fbb.push_slot_always(Config::VT_EXTERNAL_REQUIREMENTS, external_requirements);
        fbb.push_slot_always(Config::VT_BUILD_PLUGIN, build_plugin);
        if let Some(location) = location {
            fbb.push_slot_always(Config::VT_LOCATION, location);
        }
        fbb.push_slot_always(Config::VT_SOURCES, sources);
        fbb.push_slot_always(Config::VT_BUILD_DEPENDENCIES, build_dependencies);
        fbb.push_slot_always(Config::VT_KIND, kind);
        fbb.push_slot_always(Config::VT_EXTRAS, extras);
        finish_table(fbb, start)
    }

    pub fn read_config(config: Config<'_>) -> CoreConfig {
        let dependencies = string_vector_to_vec(unsafe {
            config
                .table
                .get::<ForwardsUOffset<FbStringVector<'_>>>(Config::VT_DEPENDENCIES, None)
        });
        let external_requirements = unsafe {
            config
                .table
                .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<ExternalRequirementFb<'_>>>>>(
                    Config::VT_EXTERNAL_REQUIREMENTS,
                    None,
                )
        }
        .map(|requirements| {
            requirements
                .iter()
                .map(|requirement| requirement.read())
                .collect()
        })
        .unwrap_or_default();
        let extras = read_extras(unsafe {
            config
                .table
                .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Extra<'_>>>>>(
                    Config::VT_EXTRAS,
                    None,
                )
        });

        CoreConfig {
            dependencies,
            external_requirements,
            build_plugin: string_slot(config.table, Config::VT_BUILD_PLUGIN),
            location: optional_string_slot(config.table, Config::VT_LOCATION),
            sources: string_vector_to_vec(unsafe {
                config
                    .table
                    .get::<ForwardsUOffset<FbStringVector<'_>>>(Config::VT_SOURCES, None)
            }),
            build_dependencies: string_vector_to_vec(unsafe {
                config
                    .table
                    .get::<ForwardsUOffset<FbStringVector<'_>>>(Config::VT_BUILD_DEPENDENCIES, None)
            }),
            kind: string_slot(config.table, Config::VT_KIND),
            extras,
        }
    }

    pub fn create_build_output<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        output: &CoreBuildOutput,
    ) -> WIPOffset<BuildOutput<'a>> {
        let outputs: Vec<_> = output
            .outputs
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let outputs = create_string_vector(fbb, &outputs);
        let extras = create_extra_vector(fbb, &output.extras);

        let start = fbb.start_table();
        fbb.push_slot_always(BuildOutput::VT_OUTPUTS, outputs);
        fbb.push_slot_always(BuildOutput::VT_EXTRAS, extras);
        finish_table(fbb, start)
    }

    pub fn read_build_output(output: BuildOutput<'_>) -> CoreBuildOutput {
        CoreBuildOutput {
            outputs: string_vector_to_vec(unsafe {
                output
                    .table
                    .get::<ForwardsUOffset<FbStringVector<'_>>>(BuildOutput::VT_OUTPUTS, None)
            })
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            extras: read_extras(unsafe {
                output
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Extra<'_>>>>>(
                        BuildOutput::VT_EXTRAS,
                        None,
                    )
            }),
        }
    }

    pub fn create_build_request<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        request: &CoreBuildRequest,
    ) -> WIPOffset<BuildRequest<'a>> {
        create_build_request_parts(
            fbb,
            &request.target,
            &request.config,
            &request.dependencies,
            &request.target_config,
            &request.working_directory,
            &request.tool_paths,
        )
    }

    pub fn create_build_request_parts<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        target: &str,
        config: &CoreConfig,
        dependencies: &HashMap<String, CoreBuildOutput>,
        target_config: &HashMap<u32, String>,
        working_directory: &Path,
        tool_paths: &HashMap<String, String>,
    ) -> WIPOffset<BuildRequest<'a>> {
        let target = fbb.create_string(target);
        let config = create_config(fbb, config);
        let dependencies = create_dependency_output_vector(fbb, dependencies);
        let target_config = create_target_config_vector(fbb, target_config);
        let working_directory = fbb.create_string(&working_directory.display().to_string());
        let tool_paths = create_string_field_vector(fbb, tool_paths);

        let start = fbb.start_table();
        fbb.push_slot_always(BuildRequest::VT_TARGET, target);
        fbb.push_slot_always(BuildRequest::VT_CONFIG, config);
        fbb.push_slot_always(BuildRequest::VT_DEPENDENCIES, dependencies);
        fbb.push_slot_always(BuildRequest::VT_WORKING_DIRECTORY, working_directory);
        fbb.push_slot_always(BuildRequest::VT_TOOL_PATHS, tool_paths);
        fbb.push_slot_always(BuildRequest::VT_TARGET_CONFIG, target_config);
        finish_table(fbb, start)
    }

    pub fn read_build_request(request: BuildRequest<'_>) -> CoreBuildRequest {
        let config = unsafe {
            request
                .table
                .get::<ForwardsUOffset<Config<'_>>>(BuildRequest::VT_CONFIG, None)
        }
        .map(read_config)
        .unwrap_or_default();
        let dependencies = unsafe {
            request
                .table
                .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<DependencyOutput<'_>>>>>(
                    BuildRequest::VT_DEPENDENCIES,
                    None,
                )
        }
        .map(|dependencies| {
            dependencies
                .iter()
                .map(|dependency| dependency.read())
                .collect()
        })
        .unwrap_or_default();

        CoreBuildRequest {
            target: string_slot(request.table, BuildRequest::VT_TARGET),
            config,
            dependencies,
            target_config: read_target_config(unsafe {
                request
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Extra<'_>>>>>(
                        BuildRequest::VT_TARGET_CONFIG,
                        None,
                    )
            }),
            working_directory: PathBuf::from(string_slot(
                request.table,
                BuildRequest::VT_WORKING_DIRECTORY,
            )),
            tool_paths: read_string_fields(unsafe {
                request
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        BuildRequest::VT_TOOL_PATHS,
                        None,
                    )
            }),
        }
    }

    pub fn create_build_response<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        response: &CoreBuildResponse,
    ) -> WIPOffset<BuildResponse<'a>> {
        let success = matches!(response, CoreBuildResponse::Success(_));
        let error = match response {
            CoreBuildResponse::Success(_) => None,
            CoreBuildResponse::Delegate(_) => None,
            CoreBuildResponse::Failure(error) => Some(fbb.create_string(error)),
        };
        let output = match response {
            CoreBuildResponse::Success(output) => Some(create_build_output(fbb, output)),
            CoreBuildResponse::Delegate(_) | CoreBuildResponse::Failure(_) => None,
        };
        let delegate_config = match response {
            CoreBuildResponse::Delegate(config) => Some(create_config(fbb, config)),
            CoreBuildResponse::Success(_) | CoreBuildResponse::Failure(_) => None,
        };

        let start = fbb.start_table();
        fbb.push_slot(BuildResponse::VT_SUCCESS, success, false);
        if let Some(error) = error {
            fbb.push_slot_always(BuildResponse::VT_ERROR, error);
        }
        if let Some(output) = output {
            fbb.push_slot_always(BuildResponse::VT_OUTPUT, output);
        }
        if let Some(delegate_config) = delegate_config {
            fbb.push_slot_always(BuildResponse::VT_DELEGATE_CONFIG, delegate_config);
        }
        finish_table(fbb, start)
    }

    pub fn read_build_response(response: BuildResponse<'_>) -> CoreBuildResponse {
        if let Some(config) = unsafe {
            response
                .table
                .get::<ForwardsUOffset<Config<'_>>>(BuildResponse::VT_DELEGATE_CONFIG, None)
        } {
            return CoreBuildResponse::Delegate(read_config(config));
        }

        let success = unsafe {
            response
                .table
                .get::<bool>(BuildResponse::VT_SUCCESS, Some(false))
                .unwrap_or(false)
        };
        if success {
            let output = unsafe {
                response
                    .table
                    .get::<ForwardsUOffset<BuildOutput<'_>>>(BuildResponse::VT_OUTPUT, None)
            }
            .map(read_build_output)
            .unwrap_or_default();
            CoreBuildResponse::Success(output)
        } else {
            CoreBuildResponse::Failure(string_slot(response.table, BuildResponse::VT_ERROR))
        }
    }

    pub fn create_parse_rule_request<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        request: &CoreParseRuleRequest,
    ) -> WIPOffset<ParseRuleRequest<'a>> {
        let workspace_root = fbb.create_string(&request.workspace_root.display().to_string());
        let package = fbb.create_string(&request.package);
        let package_dir = fbb.create_string(&request.package_dir.display().to_string());
        let kind = fbb.create_string(&request.kind);
        let name = fbb.create_string(&request.name);
        let sources = create_string_vector(fbb, &request.sources);
        let dependencies = create_string_vector(fbb, &request.dependencies);
        let cargo_requirements =
            create_external_requirement_vector(fbb, &request.cargo_requirements);
        let string_fields = create_string_field_vector(fbb, &request.string_fields);
        let label_fields = create_string_field_vector(fbb, &request.label_fields);

        let start = fbb.start_table();
        fbb.push_slot_always(ParseRuleRequest::VT_WORKSPACE_ROOT, workspace_root);
        fbb.push_slot_always(ParseRuleRequest::VT_PACKAGE, package);
        fbb.push_slot_always(ParseRuleRequest::VT_PACKAGE_DIR, package_dir);
        fbb.push_slot_always(ParseRuleRequest::VT_KIND, kind);
        fbb.push_slot_always(ParseRuleRequest::VT_NAME, name);
        fbb.push_slot_always(ParseRuleRequest::VT_SOURCES, sources);
        fbb.push_slot_always(ParseRuleRequest::VT_DEPENDENCIES, dependencies);
        fbb.push_slot_always(ParseRuleRequest::VT_CARGO_REQUIREMENTS, cargo_requirements);
        fbb.push_slot_always(ParseRuleRequest::VT_STRING_FIELDS, string_fields);
        fbb.push_slot_always(ParseRuleRequest::VT_LABEL_FIELDS, label_fields);
        finish_table(fbb, start)
    }

    pub fn read_parse_rule_request(request: ParseRuleRequest<'_>) -> CoreParseRuleRequest {
        CoreParseRuleRequest {
            workspace_root: PathBuf::from(string_slot(
                request.table,
                ParseRuleRequest::VT_WORKSPACE_ROOT,
            )),
            package: string_slot(request.table, ParseRuleRequest::VT_PACKAGE),
            package_dir: PathBuf::from(string_slot(
                request.table,
                ParseRuleRequest::VT_PACKAGE_DIR,
            )),
            kind: string_slot(request.table, ParseRuleRequest::VT_KIND),
            name: string_slot(request.table, ParseRuleRequest::VT_NAME),
            sources: string_vector_to_vec(unsafe {
                request
                    .table
                    .get::<ForwardsUOffset<FbStringVector<'_>>>(ParseRuleRequest::VT_SOURCES, None)
            }),
            dependencies: string_vector_to_vec(unsafe {
                request.table.get::<ForwardsUOffset<FbStringVector<'_>>>(
                    ParseRuleRequest::VT_DEPENDENCIES,
                    None,
                )
            }),
            cargo_requirements: unsafe {
                request
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<ExternalRequirementFb<'_>>>>>(
                        ParseRuleRequest::VT_CARGO_REQUIREMENTS,
                        None,
                    )
            }
            .map(|requirements| {
                requirements
                    .iter()
                    .map(|requirement| requirement.read())
                    .collect()
            })
            .unwrap_or_default(),
            string_fields: read_string_fields(unsafe {
                request
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        ParseRuleRequest::VT_STRING_FIELDS,
                        None,
                    )
            }),
            label_fields: read_string_fields(unsafe {
                request
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        ParseRuleRequest::VT_LABEL_FIELDS,
                        None,
                    )
            }),
        }
    }

    pub fn create_parse_rule_response<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        response: &CoreParseRuleResponse,
    ) -> WIPOffset<ParseRuleResponse<'a>> {
        let success = matches!(response, CoreParseRuleResponse::Success(_));
        let error = match response {
            CoreParseRuleResponse::Success(_) => None,
            CoreParseRuleResponse::Failure(error) => Some(fbb.create_string(error)),
        };
        let config = match response {
            CoreParseRuleResponse::Success(config) => Some(create_config(fbb, config)),
            CoreParseRuleResponse::Failure(_) => None,
        };

        let start = fbb.start_table();
        fbb.push_slot(ParseRuleResponse::VT_SUCCESS, success, false);
        if let Some(error) = error {
            fbb.push_slot_always(ParseRuleResponse::VT_ERROR, error);
        }
        if let Some(config) = config {
            fbb.push_slot_always(ParseRuleResponse::VT_CONFIG, config);
        }
        finish_table(fbb, start)
    }

    pub fn read_parse_rule_response(response: ParseRuleResponse<'_>) -> CoreParseRuleResponse {
        let success = unsafe {
            response
                .table
                .get::<bool>(ParseRuleResponse::VT_SUCCESS, Some(false))
                .unwrap_or(false)
        };
        if success {
            let config = unsafe {
                response
                    .table
                    .get::<ForwardsUOffset<Config<'_>>>(ParseRuleResponse::VT_CONFIG, None)
            }
            .map(read_config)
            .unwrap_or_default();
            CoreParseRuleResponse::Success(config)
        } else {
            CoreParseRuleResponse::Failure(string_slot(response.table, ParseRuleResponse::VT_ERROR))
        }
    }

    pub fn create_plan_dependencies_request<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        request: &CorePlanDependenciesRequest,
    ) -> WIPOffset<PlanDependenciesRequest<'a>> {
        let ecosystem = fbb.create_string(&request.ecosystem);
        let requirements = create_external_requirement_vector(fbb, &request.requirements);
        let context = create_plugin_context(fbb, &request.context);

        let start = fbb.start_table();
        fbb.push_slot_always(PlanDependenciesRequest::VT_ECOSYSTEM, ecosystem);
        fbb.push_slot_always(PlanDependenciesRequest::VT_REQUIREMENTS, requirements);
        fbb.push_slot_always(PlanDependenciesRequest::VT_CONTEXT, context);
        finish_table(fbb, start)
    }

    pub fn read_plan_dependencies_request(
        request: PlanDependenciesRequest<'_>,
    ) -> CorePlanDependenciesRequest {
        let requirements = unsafe {
            request
                .table
                .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<ExternalRequirementFb<'_>>>>>(
                    PlanDependenciesRequest::VT_REQUIREMENTS,
                    None,
                )
        }
        .map(|requirements| {
            requirements
                .iter()
                .map(|requirement| requirement.read())
                .collect()
        })
        .unwrap_or_default();
        let context = unsafe {
            request.table.get::<ForwardsUOffset<PluginContext<'_>>>(
                PlanDependenciesRequest::VT_CONTEXT,
                None,
            )
        }
        .map(read_plugin_context)
        .unwrap_or_default();

        CorePlanDependenciesRequest {
            ecosystem: string_slot(request.table, PlanDependenciesRequest::VT_ECOSYSTEM),
            requirements,
            context,
        }
    }

    pub fn create_plan_dependencies_response<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        response: &CorePlanDependenciesResponse,
    ) -> WIPOffset<PlanDependenciesResponse<'a>> {
        let success = matches!(response, CorePlanDependenciesResponse::Success(_));
        let error = match response {
            CorePlanDependenciesResponse::Success(_) => None,
            CorePlanDependenciesResponse::Failure(error) => Some(fbb.create_string(error)),
        };
        let plan = match response {
            CorePlanDependenciesResponse::Success(plan) => Some(create_dependency_plan(fbb, plan)),
            CorePlanDependenciesResponse::Failure(_) => None,
        };

        let start = fbb.start_table();
        fbb.push_slot(PlanDependenciesResponse::VT_SUCCESS, success, false);
        if let Some(error) = error {
            fbb.push_slot_always(PlanDependenciesResponse::VT_ERROR, error);
        }
        if let Some(plan) = plan {
            fbb.push_slot_always(PlanDependenciesResponse::VT_PLAN, plan);
        }
        finish_table(fbb, start)
    }

    pub fn read_plan_dependencies_response(
        response: PlanDependenciesResponse<'_>,
    ) -> CorePlanDependenciesResponse {
        let success = unsafe {
            response
                .table
                .get::<bool>(PlanDependenciesResponse::VT_SUCCESS, Some(false))
                .unwrap_or(false)
        };
        if success {
            let plan = unsafe {
                response.table.get::<ForwardsUOffset<DependencyPlan<'_>>>(
                    PlanDependenciesResponse::VT_PLAN,
                    None,
                )
            }
            .map(read_dependency_plan)
            .unwrap_or_default();
            CorePlanDependenciesResponse::Success(plan)
        } else {
            CorePlanDependenciesResponse::Failure(string_slot(
                response.table,
                PlanDependenciesResponse::VT_ERROR,
            ))
        }
    }

    pub fn create_resolve_target_request<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        request: &CoreResolveTargetRequest,
    ) -> WIPOffset<ResolveTargetRequest<'a>> {
        let target = fbb.create_string(&request.target);
        let context = create_plugin_context(fbb, &request.context);

        let start = fbb.start_table();
        fbb.push_slot_always(ResolveTargetRequest::VT_TARGET, target);
        fbb.push_slot_always(ResolveTargetRequest::VT_CONTEXT, context);
        finish_table(fbb, start)
    }

    pub fn read_resolve_target_request(
        request: ResolveTargetRequest<'_>,
    ) -> CoreResolveTargetRequest {
        let context = unsafe {
            request
                .table
                .get::<ForwardsUOffset<PluginContext<'_>>>(ResolveTargetRequest::VT_CONTEXT, None)
        }
        .map(read_plugin_context)
        .unwrap_or_default();

        CoreResolveTargetRequest {
            target: string_slot(request.table, ResolveTargetRequest::VT_TARGET),
            context,
        }
    }

    pub fn create_resolve_target_response<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        response: &CoreResolveTargetResponse,
    ) -> WIPOffset<ResolveTargetResponse<'a>> {
        let success = matches!(response, CoreResolveTargetResponse::Success(_));
        let error = match response {
            CoreResolveTargetResponse::Success(_) => None,
            CoreResolveTargetResponse::Failure(error) => Some(fbb.create_string(error)),
        };
        let config = match response {
            CoreResolveTargetResponse::Success(config) => Some(create_config(fbb, config)),
            CoreResolveTargetResponse::Failure(_) => None,
        };

        let start = fbb.start_table();
        fbb.push_slot(ResolveTargetResponse::VT_SUCCESS, success, false);
        if let Some(error) = error {
            fbb.push_slot_always(ResolveTargetResponse::VT_ERROR, error);
        }
        if let Some(config) = config {
            fbb.push_slot_always(ResolveTargetResponse::VT_CONFIG, config);
        }
        finish_table(fbb, start)
    }

    pub fn read_resolve_target_response(
        response: ResolveTargetResponse<'_>,
    ) -> CoreResolveTargetResponse {
        let success = unsafe {
            response
                .table
                .get::<bool>(ResolveTargetResponse::VT_SUCCESS, Some(false))
                .unwrap_or(false)
        };
        if success {
            let config = unsafe {
                response
                    .table
                    .get::<ForwardsUOffset<Config<'_>>>(ResolveTargetResponse::VT_CONFIG, None)
            }
            .map(read_config)
            .unwrap_or_default();
            CoreResolveTargetResponse::Success(config)
        } else {
            CoreResolveTargetResponse::Failure(string_slot(
                response.table,
                ResolveTargetResponse::VT_ERROR,
            ))
        }
    }

    fn create_plugin_context<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        context: &CorePluginContext,
    ) -> WIPOffset<PluginContext<'a>> {
        let cache_dir = fbb.create_string(&context.cache_dir.display().to_string());
        let target_config = create_target_config_vector(fbb, &context.target_config);
        let tool_paths = create_string_field_vector(fbb, &context.tool_paths);
        let lockfile = create_string_field_vector(fbb, &context.lockfile);
        let locked_dependencies =
            create_locked_dependencies_vector(fbb, &context.locked_dependencies);

        let start = fbb.start_table();
        fbb.push_slot_always(PluginContext::VT_CACHE_DIR, cache_dir);
        fbb.push_slot(PluginContext::VT_CONTEXT_HASH, context.context_hash, 0);
        fbb.push_slot_always(PluginContext::VT_TARGET_CONFIG, target_config);
        fbb.push_slot_always(PluginContext::VT_LOCKFILE, lockfile);
        fbb.push_slot_always(PluginContext::VT_LOCKED_DEPENDENCIES, locked_dependencies);
        fbb.push_slot_always(PluginContext::VT_TOOL_PATHS, tool_paths);
        finish_table(fbb, start)
    }

    fn read_plugin_context(context: PluginContext<'_>) -> CorePluginContext {
        CorePluginContext {
            cache_dir: PathBuf::from(string_slot(context.table, PluginContext::VT_CACHE_DIR)),
            context_hash: unsafe {
                context
                    .table
                    .get::<u64>(PluginContext::VT_CONTEXT_HASH, Some(0))
                    .unwrap_or(0)
            },
            target_config: read_target_config(unsafe {
                context
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Extra<'_>>>>>(
                        PluginContext::VT_TARGET_CONFIG,
                        None,
                    )
            }),
            tool_paths: read_string_fields(unsafe {
                context
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        PluginContext::VT_TOOL_PATHS,
                        None,
                    )
            }),
            lockfile: read_string_fields(unsafe {
                context
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        PluginContext::VT_LOCKFILE,
                        None,
                    )
            }),
            locked_dependencies: read_locked_dependencies(unsafe {
                context
                    .table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<DependencyEdgeSet<'_>>>>>(
                        PluginContext::VT_LOCKED_DEPENDENCIES,
                        None,
                    )
            }),
            target: None,
        }
    }

    fn create_dependency_plan<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        plan: &CoreDependencyPlan,
    ) -> WIPOffset<DependencyPlan<'a>> {
        let lockfile = create_string_field_vector(fbb, &plan.lockfile);
        let locked_dependencies = create_locked_dependencies_vector(fbb, &plan.locked_dependencies);

        let start = fbb.start_table();
        fbb.push_slot_always(DependencyPlan::VT_LOCKFILE, lockfile);
        fbb.push_slot_always(DependencyPlan::VT_LOCKED_DEPENDENCIES, locked_dependencies);
        finish_table(fbb, start)
    }

    fn read_dependency_plan(plan: DependencyPlan<'_>) -> CoreDependencyPlan {
        CoreDependencyPlan {
            lockfile: read_string_fields(unsafe {
                plan.table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<StringField<'_>>>>>(
                        DependencyPlan::VT_LOCKFILE,
                        None,
                    )
            }),
            locked_dependencies: read_locked_dependencies(unsafe {
                plan.table
                    .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<DependencyEdgeSet<'_>>>>>(
                        DependencyPlan::VT_LOCKED_DEPENDENCIES,
                        None,
                    )
            }),
        }
    }

    fn create_external_requirement_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        requirements: &[ExternalRequirement],
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<ExternalRequirementFb<'a>>>> {
        let values: Vec<_> = requirements
            .iter()
            .map(|requirement| create_external_requirement(fbb, requirement))
            .collect();
        fbb.create_vector(&values)
    }

    fn create_external_requirement<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        requirement: &ExternalRequirement,
    ) -> WIPOffset<ExternalRequirementFb<'a>> {
        let ecosystem = fbb.create_string(&requirement.ecosystem);
        let package = fbb.create_string(&requirement.package);
        let version = fbb.create_string(&requirement.version);
        let features = create_string_vector(fbb, &requirement.features);
        let target = requirement
            .target
            .as_ref()
            .map(|target| fbb.create_string(target));

        let start = fbb.start_table();
        fbb.push_slot_always(ExternalRequirementFb::VT_ECOSYSTEM, ecosystem);
        fbb.push_slot_always(ExternalRequirementFb::VT_PACKAGE, package);
        fbb.push_slot_always(ExternalRequirementFb::VT_VERSION, version);
        fbb.push_slot_always(ExternalRequirementFb::VT_FEATURES, features);
        fbb.push_slot(
            ExternalRequirementFb::VT_DEFAULT_FEATURES,
            requirement.default_features,
            false,
        );
        if let Some(target) = target {
            fbb.push_slot_always(ExternalRequirementFb::VT_TARGET, target);
        }
        finish_table(fbb, start)
    }

    fn create_dependency_output_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        dependencies: &HashMap<String, CoreBuildOutput>,
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<DependencyOutput<'a>>>> {
        let mut dependencies: Vec<_> = dependencies.iter().collect();
        dependencies.sort_by_key(|(target, _)| target.as_str());
        let values: Vec<_> = dependencies
            .into_iter()
            .map(|(target, output)| create_dependency_output(fbb, target, output))
            .collect();
        fbb.create_vector(&values)
    }

    fn create_dependency_output<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        target: &str,
        output: &CoreBuildOutput,
    ) -> WIPOffset<DependencyOutput<'a>> {
        let target = fbb.create_string(target);
        let output = create_build_output(fbb, output);

        let start = fbb.start_table();
        fbb.push_slot_always(DependencyOutput::VT_TARGET, target);
        fbb.push_slot_always(DependencyOutput::VT_OUTPUT, output);
        finish_table(fbb, start)
    }

    fn create_extra_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        extras: &HashMap<u32, Vec<String>>,
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<Extra<'a>>>> {
        let mut extras: Vec<_> = extras.iter().collect();
        extras.sort_by_key(|(key, _)| **key);
        let values: Vec<_> = extras
            .into_iter()
            .map(|(key, values)| create_extra(fbb, *key, values))
            .collect();
        fbb.create_vector(&values)
    }

    fn create_string_field_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        fields: &HashMap<String, String>,
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<StringField<'a>>>> {
        let mut fields: Vec<_> = fields.iter().collect();
        fields.sort_by_key(|(key, _)| key.as_str());
        let values: Vec<_> = fields
            .into_iter()
            .map(|(key, value)| create_string_field(fbb, key, value))
            .collect();
        fbb.create_vector(&values)
    }

    fn create_string_field<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        key: &str,
        value: &str,
    ) -> WIPOffset<StringField<'a>> {
        let key = fbb.create_string(key);
        let value = fbb.create_string(value);

        let start = fbb.start_table();
        fbb.push_slot_always(StringField::VT_KEY, key);
        fbb.push_slot_always(StringField::VT_VALUE, value);
        finish_table(fbb, start)
    }

    fn read_string_fields(
        fields: Option<Vector<'_, ForwardsUOffset<StringField<'_>>>>,
    ) -> HashMap<String, String> {
        fields
            .map(|fields| fields.iter().map(|field| field.read()).collect())
            .unwrap_or_default()
    }

    fn create_target_config_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        config: &HashMap<u32, String>,
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<Extra<'a>>>> {
        let values: HashMap<_, _> = config
            .iter()
            .map(|(key, value)| (*key, vec![value.clone()]))
            .collect();
        create_extra_vector(fbb, &values)
    }

    fn read_target_config(
        values: Option<Vector<'_, ForwardsUOffset<Extra<'_>>>>,
    ) -> HashMap<u32, String> {
        read_extras(values)
            .into_iter()
            .filter_map(|(key, values)| values.into_iter().next().map(|value| (key, value)))
            .collect()
    }

    fn create_locked_dependencies_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        locked_dependencies: &HashMap<String, HashMap<String, String>>,
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<DependencyEdgeSet<'a>>>> {
        let mut locked_dependencies: Vec<_> = locked_dependencies.iter().collect();
        locked_dependencies.sort_by_key(|(target, _)| target.as_str());
        let values: Vec<_> = locked_dependencies
            .into_iter()
            .map(|(target, dependencies)| create_dependency_edge_set(fbb, target, dependencies))
            .collect();
        fbb.create_vector(&values)
    }

    fn create_dependency_edge_set<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        target: &str,
        dependencies: &HashMap<String, String>,
    ) -> WIPOffset<DependencyEdgeSet<'a>> {
        let target = fbb.create_string(target);
        let dependencies = create_string_field_vector(fbb, dependencies);

        let start = fbb.start_table();
        fbb.push_slot_always(DependencyEdgeSet::VT_TARGET, target);
        fbb.push_slot_always(DependencyEdgeSet::VT_DEPENDENCIES, dependencies);
        finish_table(fbb, start)
    }

    fn read_locked_dependencies(
        dependencies: Option<Vector<'_, ForwardsUOffset<DependencyEdgeSet<'_>>>>,
    ) -> HashMap<String, HashMap<String, String>> {
        dependencies
            .map(|dependencies| {
                dependencies
                    .iter()
                    .map(|dependency| dependency.read())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn create_extra<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        key: u32,
        values: &[String],
    ) -> WIPOffset<Extra<'a>> {
        let values = create_string_vector(fbb, values);

        let start = fbb.start_table();
        fbb.push_slot(Extra::VT_KEY, key, 0);
        fbb.push_slot_always(Extra::VT_VALUES, values);
        finish_table(fbb, start)
    }

    fn read_extras(
        values: Option<Vector<'_, ForwardsUOffset<Extra<'_>>>>,
    ) -> HashMap<u32, Vec<String>> {
        values
            .map(|values| {
                values
                    .iter()
                    .map(|extra| (extra.key(), extra.values()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn create_string_vector<'a>(
        fbb: &mut FlatBufferBuilder<'a>,
        values: &[String],
    ) -> WIPOffset<Vector<'a, ForwardsUOffset<&'a str>>> {
        let values: Vec<_> = values
            .iter()
            .map(|value| fbb.create_string(value))
            .collect();
        fbb.create_vector(&values)
    }

    fn string_vector_to_vec(values: Option<FbStringVector<'_>>) -> Vec<String> {
        values
            .map(|values| values.iter().map(|value| value.to_string()).collect())
            .unwrap_or_default()
    }

    fn string_slot(table: Table<'_>, slot: VOffsetT) -> String {
        optional_string_slot(table, slot).unwrap_or_default()
    }

    fn optional_string_slot(table: Table<'_>, slot: VOffsetT) -> Option<String> {
        unsafe { table.get::<ForwardsUOffset<&str>>(slot, None) }.map(|value| value.to_string())
    }

    fn finish_table<'a, T>(
        fbb: &mut FlatBufferBuilder<'a>,
        start: WIPOffset<TableUnfinishedWIPOffset>,
    ) -> WIPOffset<T> {
        let table: WIPOffset<TableFinishedWIPOffset> = fbb.end_table(start);
        WIPOffset::new(table.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSITIVE_PRODUCTS: u32 = 0;
    const EDITION: u32 = 3;
    const ROOT_SOURCE: u32 = 4;

    #[test]
    fn config_round_trips_through_flatbuffers() {
        let mut extras = HashMap::new();
        extras.insert(EDITION, vec!["2021".to_string()]);
        extras.insert(ROOT_SOURCE, vec!["/tmp/generated.rs".to_string()]);

        let config = Config {
            dependencies: vec!["//util/bus:bus".to_string()],
            external_requirements: vec![ExternalRequirement {
                ecosystem: "cargo".to_string(),
                package: "futures".to_string(),
                version: "=0.3.31".to_string(),
                features: vec!["std".to_string()],
                default_features: false,
                target: Some("cargo://futures".to_string()),
            }],
            build_plugin: "@bus_plugin".to_string(),
            location: None,
            sources: vec!["schema.bus".to_string()],
            build_dependencies: vec![
                "@rust_compiler".to_string(),
                "//util/bus/codegen:codegen".to_string(),
            ],
            kind: "rust_bus_library".to_string(),
            extras,
        };

        let decoded = decode_config(&encode_config(&config)).unwrap();
        assert_eq!(decoded.dependencies, config.dependencies);
        assert_eq!(decoded.external_requirements, config.external_requirements);
        assert_eq!(decoded.build_plugin, config.build_plugin);
        assert_eq!(decoded.location, config.location);
        assert_eq!(decoded.sources, config.sources);
        assert_eq!(decoded.build_dependencies, config.build_dependencies);
        assert_eq!(decoded.kind, config.kind);
        assert_eq!(decoded.extras, config.extras);
    }

    #[test]
    fn build_request_and_response_round_trip_through_flatbuffers() {
        let mut output_extras = HashMap::new();
        output_extras.insert(
            TRANSITIVE_PRODUCTS,
            vec!["bus:/tmp/libbus.rlib".to_string()],
        );
        let output = BuildOutput {
            outputs: vec![PathBuf::from("/tmp/libschema.rlib")],
            extras: output_extras,
        };

        let mut dependencies = HashMap::new();
        dependencies.insert("//util/bus:bus".to_string(), output.clone());
        let request = BuildRequest {
            target: "//util/bus/example:schema".to_string(),
            config: Config {
                build_plugin: "@bus_plugin".to_string(),
                kind: "rust_bus_library".to_string(),
                sources: vec!["schema.bus".to_string()],
                ..Default::default()
            },
            dependencies,
            target_config: [(build_config_key::TARGET_OS, "macos".to_string())]
                .into_iter()
                .collect(),
            working_directory: PathBuf::from("/tmp/cbs/schema"),
            tool_paths: [("busfmt".to_string(), "/declared/busfmt".to_string())]
                .into_iter()
                .collect(),
        };

        assert_eq!(
            decode_build_request(&encode_build_request(&request)).unwrap(),
            request
        );
        assert_eq!(
            decode_build_response(&encode_build_response(&BuildResponse::Success(
                output.clone()
            )))
            .unwrap(),
            BuildResponse::Success(output)
        );
        assert_eq!(
            decode_build_response(&encode_build_response(&BuildResponse::Failure(
                "boom".to_string()
            )))
            .unwrap(),
            BuildResponse::Failure("boom".to_string())
        );
        let delegate = Config {
            build_plugin: "@rust_plugin".to_string(),
            kind: "rust_library".to_string(),
            sources: vec!["/tmp/generated.rs".to_string()],
            ..Default::default()
        };
        assert_eq!(
            decode_build_response(&encode_build_response(&BuildResponse::Delegate(
                delegate.clone()
            )))
            .unwrap(),
            BuildResponse::Delegate(delegate)
        );
    }

    #[test]
    fn plugin_manifest_round_trips_test_rule_kinds() {
        let manifest = PluginManifest {
            name: "example".to_string(),
            rule_kinds: vec!["example_library".to_string(), "example_test".to_string()],
            test_rule_kinds: vec!["example_test".to_string()],
            build_plugins: vec!["@example_plugin".to_string()],
            label_fields: vec!["tool".to_string()],
            dependency_ecosystems: vec!["example".to_string()],
            target_prefixes: vec!["example://".to_string()],
        };

        assert_eq!(
            decode_plugin_manifest(&encode_plugin_manifest(&manifest)).unwrap(),
            manifest
        );
    }

    #[test]
    fn planner_and_resolver_messages_round_trip() {
        let mut target_config = HashMap::new();
        target_config.insert(build_config_key::TARGET_OS, "macos".to_string());
        let mut lockfile = HashMap::new();
        lockfile.insert("cargo://rand".to_string(), "0.8.5,std".to_string());
        let mut locked_dependencies = HashMap::new();
        locked_dependencies.insert(
            "cargo://rand".to_string(),
            [("rand_core".to_string(), "cargo://rand_core".to_string())]
                .into_iter()
                .collect(),
        );
        let context = PluginContext {
            cache_dir: PathBuf::from("/tmp/cache"),
            context_hash: 123,
            target_config,
            tool_paths: [("cargo".to_string(), "/declared/cargo".to_string())]
                .into_iter()
                .collect(),
            lockfile,
            locked_dependencies,
            target: None,
        };
        let request = PlanDependenciesRequest {
            ecosystem: "cargo".to_string(),
            requirements: vec![ExternalRequirement {
                ecosystem: "cargo".to_string(),
                package: "rand".to_string(),
                version: "=0.8.5".to_string(),
                features: vec!["std".to_string()],
                default_features: true,
                target: None,
            }],
            context: context.clone(),
        };
        assert_eq!(
            decode_plan_dependencies_request(&encode_plan_dependencies_request(&request)).unwrap(),
            request
        );

        let plan = DependencyPlan {
            lockfile: context.lockfile.clone(),
            locked_dependencies: context.locked_dependencies.clone(),
        };
        assert_eq!(
            decode_plan_dependencies_response(&encode_plan_dependencies_response(
                &PlanDependenciesResponse::Success(plan.clone())
            ))
            .unwrap(),
            PlanDependenciesResponse::Success(plan)
        );

        let resolve = ResolveTargetRequest {
            target: "cargo://rand".to_string(),
            context,
        };
        assert_eq!(
            decode_resolve_target_request(&encode_resolve_target_request(&resolve)).unwrap(),
            resolve
        );
        let config = Config {
            build_plugin: "@rust_plugin".to_string(),
            kind: "rust_library".to_string(),
            ..Default::default()
        };
        assert_eq!(
            decode_resolve_target_response(&encode_resolve_target_response(
                &ResolveTargetResponse::Success(config.clone())
            ))
            .unwrap(),
            ResolveTargetResponse::Success(config)
        );
    }

    #[test]
    fn default_rejects_bare_command_resolution() {
        let err = resolve_command(PathBuf::from("definitely-not-declared-tool")).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err
            .to_string()
            .contains("non-hermetic tool use rejected"));
    }
}
