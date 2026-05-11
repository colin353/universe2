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
        let mut cmd_debug = format!("{}", bin.to_string_lossy());
        let mut c = std::process::Command::new(bin);
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

        self.run_process(
            context,
            curl_command(),
            &[
                "--fail".to_string(),
                "--location".to_string(),
                "--silent".to_string(),
                "--show-error".to_string(),
                "--output".to_string(),
                dest.to_string_lossy().to_string(),
                url,
            ],
        )
        .map(|_| ())
    }
}

fn curl_command() -> &'static str {
    for candidate in ["/usr/bin/curl", "/bin/curl", "/usr/local/bin/curl"] {
        if Path::new(candidate).exists() {
            return candidate;
        }
    }
    "curl"
}

fn command_name(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .and_then(|bin| Path::new(bin).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}
