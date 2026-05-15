use std::path::Path;

use crate::core::{BuildActions, Context};

impl BuildActions {
    pub fn new() -> Self {
        Self {}
    }

    pub fn run_process<P: Into<std::path::PathBuf>, S>(
        &self,
        context: &Context,
        bin: P,
        args: &[S],
    ) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
    {
        self.run_process_with_env(context, bin, args, std::iter::empty::<(&str, &str)>())
    }

    pub fn run_process_with_env<P: Into<std::path::PathBuf>, S, E, K, V>(
        &self,
        context: &Context,
        bin: P,
        args: &[S],
        env: E,
    ) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let bin = bin.into();
        if bin.components().count() == 1 {
            if let Some(tool) = bin.to_str().filter(|tool| context.tools.contains_key(*tool)) {
                return self.run_tool_with_env(context, tool, args, env);
            }
        }
        crate::tool_diagnostics::diagnose_command(&bin, "CBS action")?;
        self.run_resolved_process(context, bin, args, env, false)
    }

    pub fn run_tool<S>(&self, context: &Context, tool: &str, args: &[S]) -> std::io::Result<Vec<u8>>
    where
        S: AsRef<str>,
    {
        self.run_tool_with_env(context, tool, args, std::iter::empty::<(&str, &str)>())
    }

    pub fn run_tool_with_env<S, E, K, V>(
        &self,
        context: &Context,
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
        let bin = materialize_declared_tools(context)?.join(tool);
        if !bin.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("tool {tool:?} is not declared"),
            ));
        }
        self.run_resolved_process(context, bin, args, env, true)
    }

    fn run_resolved_process<S, E, K, V>(
        &self,
        context: &Context,
        bin: std::path::PathBuf,
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
        let mut cmd_debug = format!("{}", bin.to_string_lossy());
        let mut c = std::process::Command::new(bin);
        if hermetic {
            configure_hermetic_env(context, &mut c)?;
        }
        for (key, value) in env {
            cmd_debug.push(' ');
            cmd_debug.push_str(key.as_ref());
            cmd_debug.push_str("=<env>");
            c.env(key.as_ref(), value.as_ref());
        }
        for arg in args {
            cmd_debug.push(' ');
            cmd_debug.push_str(arg.as_ref());
            c.arg(arg.as_ref());
        }
        eprintln!(
            "[cbs] action {}: {}",
            context.target.as_deref().unwrap_or("workspace"),
            command_name(&cmd_debug)
        );
        context.log(format!("command: {cmd_debug}"));

        let out = c.output()?;
        if !out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.trim().is_empty() {
                context.log(format!("stdout:\n{}", stdout.trim_end()));
            }
            if !stderr.trim().is_empty() {
                context.log(format!("stderr:\n{}", stderr.trim_end()));
            }
            let mut message = format!("command exited with {}\ncommand: {cmd_debug}", out.status);
            if context.target.is_none() && !stderr.trim().is_empty() {
                message.push_str("\nstderr:\n");
                message.push_str(stderr.trim_end());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            ));
        }

        Ok(out.stdout)
    }

    pub fn download<S: Into<String>, P: Into<std::path::PathBuf>>(
        &self,
        context: &Context,
        url: S,
        dest: P,
    ) -> std::io::Result<()> {
        let dest = dest.into();
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p).ok();
        }
        let url = url.into();
        context.log(format!("download URL: {url}"));

        let args = [
            "--fail".to_string(),
            "--location".to_string(),
            "--silent".to_string(),
            "--show-error".to_string(),
            "--output".to_string(),
            dest.to_string_lossy().to_string(),
            url,
        ];

        if context.tools.contains_key("curl") {
            self.run_tool(context, "curl", &args).map(|_| ())
        } else {
            self.run_process(context, curl_command()?, &args).map(|_| ())
        }
    }
}

fn configure_hermetic_env(
    context: &Context,
    command: &mut std::process::Command,
) -> std::io::Result<()> {
    let bin_dir = materialize_declared_tools(context)?;
    let workdir = context.working_directory();
    let tmpdir = workdir.join("tmp");
    let home = workdir.join("home");
    std::fs::create_dir_all(&tmpdir)?;
    std::fs::create_dir_all(&home)?;
    command.env_clear();
    command.env("PATH", bin_dir);
    command.env("TMPDIR", tmpdir);
    command.env("HOME", home);
    Ok(())
}

fn materialize_declared_tools(context: &Context) -> std::io::Result<std::path::PathBuf> {
    let bin_dir = context
        .working_directory()
        .join(".cbs-tools")
        .join(format!("{:016x}", context.hash));
    std::fs::create_dir_all(&bin_dir)?;
    for (name, tool) in context.tools.iter() {
        let link = bin_dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link)?;
        }
        symlink_or_copy(&tool.path, &link)?;
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

fn curl_command() -> std::io::Result<&'static str> {
    const DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin"];
    crate::tool_diagnostics::diagnose_host_path_search("curl", DIRS)?;
    for candidate in ["/usr/bin/curl", "/bin/curl", "/usr/local/bin/curl"] {
        if Path::new(candidate).exists() {
            return Ok(candidate);
        }
    }
    Ok("curl")
}

fn command_name(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .and_then(|bin| Path::new(bin).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}
