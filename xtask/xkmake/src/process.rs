// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use crate::error::{Error, IoResultExt, Result};

pub(crate) struct Process {
    command: Command,
    dry_run: bool,
    verbosity: u8,
    stdout_path: Option<PathBuf>,
}

impl Process {
    pub(crate) fn new(program: impl Into<OsString>, dry_run: bool, verbosity: u8) -> Self {
        Self {
            command: Command::new(program.into()),
            dry_run,
            verbosity,
            stdout_path: None,
        }
    }

    pub(crate) fn current_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(path);
        self
    }

    pub(crate) fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.command.arg(arg.into());
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command.args(args.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(
        &mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> &mut Self {
        self.command.env(key.into(), value.into());
        self
    }

    pub(crate) fn env_remove(&mut self, key: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.command.env_remove(key);
        self
    }

    pub(crate) fn stdout_to_file(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.stdout_path = Some(path.into());
        self
    }

    /// Renders the command line wrapped across multiple lines for display.
    ///
    /// Each `-flag value` group starts a new line terminated by a shell line
    /// continuation (`\`), so the output stays readable on a narrow terminal
    /// while remaining copy-pasteable into a shell.
    pub(crate) fn command_lines(&self) -> String {
        let parts = self.command_parts();
        let mut lines: Vec<String> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        for part in parts {
            let is_flag = part.starts_with('-');
            if is_flag && !current.is_empty() {
                lines.push(current.join(" "));
                current.clear();
            }
            current.push(part);
        }
        if !current.is_empty() {
            lines.push(current.join(" "));
        }
        lines.join(" \\\n  ")
    }

    /// Returns the program followed by shell-quoted arguments, plus an optional
    /// stdout redirection suffix.
    fn command_parts(&self) -> Vec<String> {
        let program = self.command.get_program().to_string_lossy();
        let mut parts: Vec<String> = std::iter::once(program.into_owned())
            .chain(
                self.command
                    .get_args()
                    .map(|arg| shell_quote(arg.to_string_lossy().as_ref())),
            )
            .collect();
        if let Some(path) = &self.stdout_path {
            parts.push(format!("> {}", shell_quote(&path.display().to_string())));
        }
        parts
    }

    /// Run the command while mirroring stdout to the terminal and to `path`.
    ///
    /// The child's stdout is piped and forwarded by a reader thread to both
    /// the parent's stdout and the file, so the run stays live on the
    /// terminal while the full output is preserved for post-processing
    /// (e.g. automatic panic backtrace symbolication). stderr and stdin stay
    /// inherited so interactive QEMU sessions keep working.
    pub(crate) fn run_tee(&mut self, path: impl Into<PathBuf>) -> Result<()> {
        let output_path = path.into();
        if self.verbosity > 0 || self.dry_run {
            println!("+ {:#?}", self.command);
            println!("  stdout tee -> {}", output_path.display());
        }
        if self.dry_run {
            return Ok(());
        }

        let file = File::create(&output_path).with_path(&output_path)?;
        self.command.stdout(Stdio::piped());
        let program = self.command.get_program().to_string_lossy().into_owned();
        let mut child = self.command.spawn().map_err(|source| Error::Spawn {
            program: program.clone(),
            source,
        })?;
        let mut pipe = child
            .stdout
            .take()
            .ok_or_else(|| Error::Message(format!("failed to capture {program} stdout")))?;
        let tee_file = Arc::new(Mutex::new(file));
        let tee_file = Arc::clone(&tee_file);
        let tee_handle = std::thread::spawn(move || {
            let mut stdout = io::stdout().lock();
            let mut buffer = [0u8; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        let _ = stdout.write_all(&buffer[..length]);
                        let _ = stdout.flush();
                        if let Ok(mut file) = tee_file.lock() {
                            let _ = file.write_all(&buffer[..length]);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let status = match child.wait() {
            Ok(status) => status,
            Err(source) => {
                let _ = tee_handle.join();
                return Err(Error::Spawn {
                    program: program.clone(),
                    source,
                });
            }
        };
        let _ = tee_handle.join();
        if !status.success() {
            return Err(Error::CommandFailed { program, status });
        }
        Ok(())
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        if self.verbosity > 0 || self.dry_run {
            // Show the command once, in shell form, so it can be copy-pasted.
            println!("{}", self.command_lines());
            if let Some(path) = &self.stdout_path {
                println!("  stdout -> {}", path.display());
            }
        }
        if self.dry_run {
            return Ok(());
        }

        let pending_stdout = if let Some(path) = &self.stdout_path {
            let pending_path = pending_output_path(path)?;
            let output = File::create(&pending_path).with_path(path)?;
            self.command.stdout(Stdio::from(output));
            Some((pending_path, path.clone()))
        } else {
            None
        };

        let program = self.command.get_program().to_string_lossy().into_owned();
        let status = match self.command.status() {
            Ok(status) => status,
            Err(source) => {
                self.command.stdout(Stdio::inherit());
                remove_pending_output(pending_stdout.as_ref());
                return Err(Error::Spawn {
                    program: program.clone(),
                    source,
                });
            }
        };
        self.command.stdout(Stdio::inherit());
        if !status.success() {
            remove_pending_output(pending_stdout.as_ref());
            return Err(Error::CommandFailed { program, status });
        }
        if let Some((pending_path, output_path)) = pending_stdout {
            match fs::remove_file(&output_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    let _ = fs::remove_file(&pending_path);
                    return Err(Error::Io {
                        path: output_path,
                        source,
                    });
                }
            }
            if let Err(source) = fs::rename(&pending_path, &output_path) {
                let _ = fs::remove_file(&pending_path);
                return Err(Error::Io {
                    path: output_path,
                    source,
                });
            }
        }
        Ok(())
    }
}

fn pending_output_path(output_path: &Path) -> Result<PathBuf> {
    let file_name = output_path.file_name().ok_or_else(|| {
        Error::Message(format!(
            "stdout output path has no file name: {}",
            output_path.display()
        ))
    })?;
    let mut pending_name = file_name.to_os_string();
    pending_name.push(format!(".tmp.{}", std::process::id()));
    Ok(output_path.with_file_name(pending_name))
}

/// Quotes a single command-line argument for shell display.
///
/// Arguments without whitespace or shell metacharacters are returned as-is;
/// anything else is wrapped in single quotes (with embedded single quotes
/// escaped via the standard `'\''` idiom).
fn shell_quote(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\'' | '"' | '\\' | '$' | '`' | '|' | '&' | ';' | '<' | '>' | '(' | ')'
                        | '*' | '?' | '[' | ']' | '~' | '#' | '!' | '^' | '{' | '}' | '=' | ':'
                        | '%' | ',' | '+' | '@'
                )
        });
    if !needs_quoting {
        return arg.to_string();
    }
    let escaped = arg.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn remove_pending_output(pending_stdout: Option<&(PathBuf, PathBuf)>) {
    if let Some((pending_path, _)) = pending_stdout {
        let _ = fs::remove_file(pending_path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Process, pending_output_path, shell_quote};

    fn output_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("xkmake-process-{}-{name}", std::process::id()))
    }

    #[test]
    fn shell_quote_leaves_safe_args_unquoted() {
        assert_eq!(shell_quote("qemu-system-aarch64"), "qemu-system-aarch64");
        assert_eq!(shell_quote("-m"), "-m");
        assert_eq!(shell_quote("2G"), "2G");
    }

    #[test]
    fn shell_quote_wraps_args_with_whitespace_or_metacharacters() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("file with space.img"), "'file with space.img'");
        // An empty argument must be quoted so it is not dropped.
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_wraps_posix_metacharacters() {
        // Each of these is a POSIX shell metacharacter that must trigger
        // quoting so the rendered command line stays copy-pasteable even when
        // a workspace or image path contains one.
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("a(b)c"), "'a(b)c'");
        assert_eq!(shell_quote("*.rs"), "'*.rs'");
        assert_eq!(shell_quote("a?b"), "'a?b'");
        assert_eq!(shell_quote("a[b]c"), "'a[b]c'");
        assert_eq!(shell_quote("~root"), "'~root'");
        assert_eq!(shell_quote("a#b"), "'a#b'");
        assert_eq!(shell_quote("a!b"), "'a!b'");
        assert_eq!(shell_quote("{a,b}"), "'{a,b}'");
        assert_eq!(shell_quote("k=v"), "'k=v'");
        assert_eq!(shell_quote("a:b"), "'a:b'");
    }

    #[test]
    fn shell_quote_leaves_dotted_identifiers_unquoted() {
        // Plain flags and dotted identifiers (version numbers, dotted paths)
        // must still pass through untouched — only metacharacters trigger
        // quoting, and `.` / `_` are not among them.
        assert_eq!(shell_quote("qemu-system-x86_64"), "qemu-system-x86_64");
        assert_eq!(shell_quote("--machine"), "--machine");
        assert_eq!(shell_quote("2.5.1"), "2.5.1");
    }

    #[test]
    fn command_lines_quotes_stdout_redirection_path() {
        let mut process = Process::new("qemu-system-x86_64", false, 0);
        process
            .arg("-m")
            .arg("2G")
            .stdout_to_file("/tmp/xkmake output.log");
        // The redirection target contains a space and must be shell-quoted so
        // the rendered line stays copy-pasteable into a shell.
        assert_eq!(
            process.command_lines(),
            "qemu-system-x86_64 \\\n  -m 2G > '/tmp/xkmake output.log'"
        );
    }

    #[test]
    fn successful_stdout_is_promoted_to_the_requested_path() {
        let output = output_path("success.txt");
        let _ = fs::remove_file(&output);

        Process::new("rustc", false, 0)
            .arg("--version")
            .stdout_to_file(&output)
            .run()
            .unwrap();

        assert!(fs::read_to_string(&output).unwrap().starts_with("rustc "));
        assert!(!pending_output_path(&output).unwrap().exists());
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn command_lines_wraps_each_flag_group_with_continuation() {
        let mut process = Process::new("qemu-system-aarch64", false, 0);
        process
            .arg("-m")
            .arg("2G")
            .arg("-smp")
            .arg("4")
            .arg("-kernel")
            .arg("kernel.bin");
        // The program name starts the first line; each `-flag value` group then
        // breaks onto its own line, joined by shell continuations so the whole
        // block stays copy-pasteable.
        assert_eq!(
            process.command_lines(),
            "qemu-system-aarch64 \\\n  -m 2G \\\n  -smp 4 \\\n  -kernel kernel.bin"
        );
    }

    #[test]
    fn failed_command_removes_pending_stdout() {
        let output = output_path("failure.txt");
        let pending = pending_output_path(&output).unwrap();
        let _ = fs::remove_file(&output);
        let _ = fs::remove_file(&pending);

        let result = Process::new("false", false, 0)
            .stdout_to_file(&output)
            .run();

        assert!(result.is_err());
        assert!(!output.exists());
        assert!(!pending.exists());
    }
}
