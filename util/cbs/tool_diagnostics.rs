use std::path::Path;

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
