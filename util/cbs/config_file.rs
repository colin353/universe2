use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    String(String),
    Bool(bool),
    Number(f64),
    Array(Vec<ConfigValue>),
    Table(ConfigTable),
    Null,
}

pub type ConfigTable = BTreeMap<String, ConfigValue>;

impl ConfigValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[ConfigValue]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&ConfigTable> {
        match self {
            Self::Table(value) => Some(value),
            _ => None,
        }
    }
}

pub fn load_workspace_table(root: &Path, file: &Path) -> std::io::Result<ConfigTable> {
    load_ccl_table(root, file)
}

pub fn load_build_table(root: &Path, file: &Path) -> std::io::Result<ConfigTable> {
    normalize_ccl_build_table(load_ccl_table(root, file)?)
}

fn load_ccl_table(root: &Path, file: &Path) -> std::io::Result<ConfigTable> {
    let content = std::fs::read_to_string(file)?;
    let module = ccl::get_ast(&content).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "failed to parse {}:\n{}",
                file.display(),
                e.render(&content)
            ),
        )
    })?;
    let resolver = Arc::new(WorkspaceCclImportResolver {
        root: root.to_path_buf(),
    });
    let value = ccl::exec_with_import_resolvers_and_context(
        module,
        &content,
        "",
        vec![resolver],
        Some(file.to_string_lossy().to_string()),
    )
    .map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "failed to evaluate {}:\n{}",
                file.display(),
                e.render(&content)
            ),
        )
    })?;
    ccl_value_to_config(value).and_then(|value| match value {
        ConfigValue::Table(table) => Ok(table),
        other => Err(invalid_config(format!(
            "{} must evaluate to a dictionary, got {}",
            file.display(),
            config_type_name(&other)
        ))),
    })
}

fn normalize_ccl_build_table(table: ConfigTable) -> std::io::Result<ConfigTable> {
    let mut normalized = ConfigTable::new();
    for (name, value) in table {
        if name.starts_with('_') {
            continue;
        }
        let ConfigValue::Table(mut target) = value else {
            return Err(invalid_config(format!(
                "BUILD.ccl target {name} must be a dictionary"
            )));
        };
        let kind = target
            .get("_type")
            .and_then(ConfigValue::as_str)
            .ok_or_else(|| invalid_config(format!("BUILD.ccl target {name} is missing _type")))?
            .to_string();
        target.insert("name".to_string(), ConfigValue::String(name));
        let entry = normalized
            .entry(kind)
            .or_insert_with(|| ConfigValue::Array(Vec::new()));
        let ConfigValue::Array(targets) = entry else {
            unreachable!("normalized rule-kind entries are always arrays");
        };
        targets.push(ConfigValue::Table(target));
    }
    Ok(normalized)
}

fn ccl_value_to_config(value: ccl::Value) -> std::io::Result<ConfigValue> {
    match value {
        ccl::Value::String(value) => Ok(ConfigValue::String(value)),
        ccl::Value::Bool(value) => Ok(ConfigValue::Bool(value)),
        ccl::Value::Number(value) => Ok(ConfigValue::Number(value)),
        ccl::Value::Null => Ok(ConfigValue::Null),
        ccl::Value::Array(values) => values
            .into_iter()
            .map(ccl_value_to_config)
            .collect::<std::io::Result<Vec<_>>>()
            .map(ConfigValue::Array),
        ccl::Value::Dictionary(dict) => dict
            .kv_pairs
            .into_iter()
            .map(|(key, value)| ccl_value_to_config(value).map(|value| (key, value)))
            .collect::<std::io::Result<ConfigTable>>()
            .map(ConfigValue::Table),
    }
}

#[derive(Debug)]
struct WorkspaceCclImportResolver {
    root: PathBuf,
}

impl ccl::ImportResolver for WorkspaceCclImportResolver {
    fn resolve_import(
        &self,
        name: &str,
        context: Option<&str>,
    ) -> Result<ccl::ImportResolution, ccl::ExecError> {
        let path = resolve_import_path(&self.root, name, context)?;
        let content = std::fs::read_to_string(&path).map_err(|e| {
            ccl::ExecError::ImportResolutionError(format!(
                "unable to open import at {}: {e}",
                path.display()
            ))
        })?;
        let module = ccl::get_ast(&content).map_err(ccl::ExecError::ImportParsingError)?;
        Ok(ccl::ImportResolution {
            module,
            content,
            context: Some(path.to_string_lossy().to_string()),
        })
    }
}

fn resolve_import_path(
    root: &Path,
    name: &str,
    context: Option<&str>,
) -> Result<PathBuf, ccl::ExecError> {
    let path = if let Some(label) = name.strip_prefix("//") {
        let (package, file) = label.split_once(':').ok_or_else(|| {
            ccl::ExecError::ImportResolutionError(format!(
                "invalid CCL import label {name}; expected //package:path"
            ))
        })?;
        let package_path = relative_path(package)?;
        let file_path = relative_path(file)?;
        root.join(package_path).join(file_path)
    } else {
        let relative = relative_path(name)?;
        match context {
            Some(context) => Path::new(context).parent().unwrap_or(root).join(relative),
            None => root.join(relative),
        }
    };
    validate_workspace_relative(root, &path)?;
    Ok(path)
}

fn relative_path(value: &str) -> Result<&Path, ccl::ExecError> {
    let path = Path::new(value);
    if path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(ccl::ExecError::ImportResolutionError(format!(
            "CCL import path must be workspace-relative: {value}"
        )));
    }
    Ok(path)
}

fn validate_workspace_relative(root: &Path, path: &Path) -> Result<(), ccl::ExecError> {
    if path.components().any(|part| part == Component::ParentDir)
        || (path.is_absolute() && !path.starts_with(root))
    {
        return Err(ccl::ExecError::ImportResolutionError(format!(
            "CCL import path escapes workspace: {}",
            path.display()
        )));
    }
    Ok(())
}

fn config_type_name(value: &ConfigValue) -> &'static str {
    match value {
        ConfigValue::String(_) => "a string",
        ConfigValue::Bool(_) => "a bool",
        ConfigValue::Number(_) => "a number",
        ConfigValue::Array(_) => "an array",
        ConfigValue::Table(_) => "a dictionary",
        ConfigValue::Null => "null",
    }
}

fn invalid_config(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}
