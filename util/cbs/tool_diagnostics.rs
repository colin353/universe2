use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

pub fn strict_tools_enabled() -> bool {
    std::env::var_os("CBS_STRICT_TOOLS").is_some_and(|value| value != "0")
}

pub fn diagnose_command(program: &Path, context: &str) -> std::io::Result<()> {
    if program.components().count() == 1 {
        return report(format!(
            "{context} uses undeclared bare host tool `{}`",
            program.display()
        ));
    }

    if is_known_host_tool_path(program) {
        return report(format!(
            "{context} uses undeclared host tool path `{}`",
            program.display()
        ));
    }

    Ok(())
}

pub fn diagnose_host_path_search(tool: &str, dirs: &[&str]) -> std::io::Result<()> {
    report(format!(
        "searching host paths [{}] for undeclared tool `{tool}`",
        dirs.join(", ")
    ))
}

fn report(message: String) -> std::io::Result<()> {
    if strict_tools_enabled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("strict tools violation: {message}"),
        ));
    }

    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    if warned.lock().expect("tool warning lock poisoned").insert(message.clone()) {
        eprintln!("[cbs] warning: non-hermetic tool use: {message}");
    }
    Ok(())
}

fn is_known_host_tool_path(program: &Path) -> bool {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(Path::new)
        .any(|dir| program.starts_with(dir))
}
