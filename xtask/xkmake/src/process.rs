// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    ffi::OsString,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

    pub(crate) fn run(&mut self) -> Result<()> {
        if self.verbosity > 0 || self.dry_run {
            println!("+ {:#?}", self.command);
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

fn remove_pending_output(pending_stdout: Option<&(PathBuf, PathBuf)>) {
    if let Some((pending_path, _)) = pending_stdout {
        let _ = fs::remove_file(pending_path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Process, pending_output_path};

    fn output_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("xkmake-process-{}-{name}", std::process::id()))
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
