mod actions;
#[cfg(test)]
mod bus;
#[cfg(test)]
mod cargo;
#[cfg(test)]
mod cargo_recipes;
mod config_file;
mod context;
mod core;
mod exec;
mod plugin_abi;
mod plugins;
#[cfg(test)]
mod rust_plugin;
mod workspace;

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(usage_error());
    };
    match command.as_str() {
        "build" => {
            let targets: Vec<String> = args.collect();
            if targets.is_empty() {
                return Err(usage_error());
            };
            eprintln!("[cbs] build requested: {}", targets.join(", "));
            match workspace::build_from_current_workspace(&targets)? {
                core::BuildResult::Success(output) => {
                    for output in output.outputs {
                        println!("{}", output.display());
                    }
                    Ok(())
                }
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        "test" => {
            let targets: Vec<String> = args.collect();
            if targets.is_empty() {
                return Err(usage_error());
            };
            eprintln!("[cbs] test requested: {}", targets.join(", "));
            let build = workspace::build_tests_from_current_workspace(&targets)?;
            match build.result {
                core::BuildResult::Success(output) => run_tests(build.targets, output.outputs),
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        "install" => {
            let Some(target) = args.next() else {
                return Err(usage_error());
            };
            if args.next().is_some() {
                return Err(usage_error());
            }
            eprintln!("[cbs] install requested: {target}");
            match workspace::build_from_current_workspace(std::slice::from_ref(&target))? {
                core::BuildResult::Success(output) => install_output(&target, output.outputs),
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        "run" => {
            let Some(target) = args.next() else {
                return Err(usage_error());
            };
            let run_args: Vec<String> = match args.next().as_deref() {
                Some("--") => args.collect(),
                Some(arg) => std::iter::once(arg.to_string()).chain(args).collect(),
                None => Vec::new(),
            };
            eprintln!("[cbs] run requested: {target}");
            match workspace::build_from_current_workspace(std::slice::from_ref(&target))? {
                core::BuildResult::Success(output) => {
                    let executable = output.outputs.first().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("{target} did not produce an executable output"),
                        )
                    })?;
                    eprintln!("[cbs] execute {}", executable.display());
                    let status = std::process::Command::new(executable)
                        .args(run_args)
                        .status()?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("{target} exited with {status}"),
                        ))
                    }
                }
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        _ => Err(usage_error()),
    }
}

fn install_output(target: &str, outputs: Vec<std::path::PathBuf>) -> std::io::Result<()> {
    if outputs.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{target} must produce exactly one output to install, got {}",
                outputs.len()
            ),
        ));
    }
    let executable = &outputs[0];
    ensure_executable_output(target, executable)?;
    let name = executable.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} does not have a file name", executable.display()),
        )
    })?;
    let home = std::env::var_os("HOME").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME is not set; cannot determine install directory",
        )
    })?;
    let bin_dir = std::path::PathBuf::from(home).join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let installed = bin_dir.join(name);
    copy_executable_replacing(executable, &installed)?;

    println!("{}", installed.display());
    eprintln!("[cbs] installed {}", installed.display());
    if !path_contains_dir(&bin_dir) {
        eprintln!(
            "[cbs] warning: ~/bin ({}) is not on PATH; add ~/bin to PATH in ~/.bashrc or ~/.zshrc",
            bin_dir.display()
        );
    }
    Ok(())
}

fn copy_executable_replacing(
    source: &std::path::Path,
    installed: &std::path::Path,
) -> std::io::Result<()> {
    let tmp = replacement_temp_path(installed)?;
    let result = (|| {
        std::fs::copy(source, &tmp)?;
        set_permissions_from(source, &tmp)?;
        #[cfg(windows)]
        if installed.exists() {
            std::fs::remove_file(installed)?;
        }
        std::fs::rename(&tmp, installed)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn replacement_temp_path(installed: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let name = installed.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} does not have a file name", installed.display()),
        )
    })?;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), suffix));
    Ok(installed.with_file_name(tmp_name))
}

fn set_permissions_from(source: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(source)?.permissions().mode();
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = source;
        let _ = dest;
    }
    Ok(())
}

fn ensure_executable_output(target: &str, executable: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(executable)?.permissions().mode();
        if mode & 0o111 == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{target} output is not executable: {}",
                    executable.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        let _ = executable;
    }
    Ok(())
}

fn path_contains_dir(dir: &std::path::Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let dir = normalize_path_for_compare(dir);
    std::env::split_paths(&path).any(|entry| normalize_path_for_compare(&entry) == dir)
}

fn normalize_path_for_compare(path: &std::path::Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn run_tests(targets: Vec<String>, executables: Vec<std::path::PathBuf>) -> std::io::Result<()> {
    if targets.len() != executables.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expected one executable per test target, got {} target(s) and {} output(s)",
                targets.len(),
                executables.len()
            ),
        ));
    }

    let mut failed = Vec::new();
    for (target, executable) in targets.iter().zip(executables.iter()) {
        eprintln!("[cbs] test {target}");
        let output = match std::process::Command::new(executable).output() {
            Ok(output) => output,
            Err(e) => {
                eprintln!("[cbs] test FAIL {target}");
                eprintln!("--- {target} execution error ---");
                eprintln!("failed to execute {}: {e}", executable.display());
                failed.push(target.clone());
                continue;
            }
        };
        if output.status.success() {
            eprintln!("[cbs] test PASS {target}");
        } else {
            eprintln!("[cbs] test FAIL {target}");
            print_test_failure_logs(target, &output);
            failed.push(target.clone());
        }
    }

    if failed.is_empty() {
        eprintln!("[cbs] test result: {} passed", targets.len());
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("{} test(s) failed: {}", failed.len(), failed.join(", ")),
        ))
    }
}

fn print_test_failure_logs(target: &str, output: &std::process::Output) {
    eprintln!("--- {target} status ---");
    eprintln!("{}", output.status);
    eprintln!("--- {target} stdout ---");
    if output.stdout.is_empty() {
        eprintln!("<empty>");
    } else {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        if !output.stdout.ends_with(b"\n") {
            eprintln!();
        }
    }
    eprintln!("--- {target} stderr ---");
    if output.stderr.is_empty() {
        eprintln!("<empty>");
    } else {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        if !output.stderr.ends_with(b"\n") {
            eprintln!();
        }
    }
}

fn usage_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "usage: cbs build <target-or-pattern>...\n       cbs test <target-or-pattern>...\n       cbs install //package:target | :target\n       cbs run //package:target | :target [-- args...]",
    )
}
