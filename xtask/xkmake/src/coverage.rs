// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel unit-test coverage extraction and report generation.

use std::{
    ffi::OsString, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use lcov2cobertura::{RustDemangler, coverage_to_file, parse_file};

use crate::{
    build::{self, Bundle},
    cli::RunArgs,
    error::{Error, IoResultExt, Result},
    process::Process,
};

const GUEST_PROFRAW_PATH: &str = "/.llvm-cov/default.profraw";
const COVERAGE_IGNORE_REGEX: &str =
    "/target/|/api/kapi/src/syscall/|/api/linux_sysno/src/|/boot/kernel-boot/";

pub(crate) fn generate(bundle: &Bundle, args: &RunArgs) -> Result<()> {
    if !bundle.context.unittest {
        return Ok(());
    }

    let context = &bundle.context;
    // Follow the image's build-time instrumentation flag (bundle.toml), not the
    // possibly different XKERNEL_ENABLE_COVERAGE at run time.
    let coverage_enabled = build::unittest_coverage_enabled(bundle)?;

    let artifacts = CoverageArtifacts::new(bundle);
    if !context.dry_run {
        // Scrub before guest-log validation so a failed check cannot leave last
        // run's reports looking like this run's output.
        fs::create_dir_all(&artifacts.directory).with_path(&artifacts.directory)?;
        artifacts.remove_stale_files()?;
    }

    // Always validate guest test outcome from qemu.log. Coverage persistence is
    // only required when the image was instrumented; otherwise a failed guest
    // would still look like success to local `make UNITTEST=y run`.
    if !context.dry_run {
        let log_path = crate::qemu::log_path(bundle);
        let output = fs::read_to_string(&log_path).with_path(&log_path)?;
        validate_guest_tests_passed(&output)?;
        if coverage_enabled {
            validate_guest_coverage_persisted(&output)?;
        }
    }

    if !coverage_enabled {
        println!("Skipping coverage generation (unittest image built without coverage)");
        return Ok(());
    }

    println!("Generating unit-test coverage reports...");
    extract_profraw(bundle, args, &artifacts)?;
    merge_profraw(bundle, &artifacts)?;
    write_text_report(bundle, &artifacts)?;
    export_lcov(bundle, &artifacts)?;
    if !context.dry_run {
        write_cobertura_report(bundle, &artifacts)?;
    } else {
        println!(
            "+ convert {} -> {}",
            artifacts.lcov.display(),
            artifacts.cobertura.display()
        );
    }

    if !context.dry_run {
        println!(
            "Coverage reports generated in {}",
            artifacts.directory.display()
        );
    }
    Ok(())
}

/// Whether the guest printed `status` as a line of its own.
///
/// The console carries SGR colour sequences, and a record whose text ends with a
/// newline leaves its reset at the start of the following line; strip escapes
/// before comparing so a coloured log line cannot hide a status marker.
fn has_guest_status(output: &str, status: &str) -> bool {
    output.lines().any(|line| strip_ansi(line).trim() == status)
}

/// Removes ANSI SGR sequences (`ESC [ ... m`) from one line.
fn strip_ansi(line: &str) -> String {
    let mut cleaned = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            cleaned.push(c);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        // Consume parameters/digits/';' until the final byte of the sequence.
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    cleaned
}

// QEMU can exit successfully after a guest error. Require evidence from the
// newly captured log that unit tests actually passed.
fn validate_guest_tests_passed(output: &str) -> Result<()> {
    if !has_guest_status(output, "=== UNITTEST_STATUS: ALL_TESTS_PASSED ===")
        || has_guest_status(output, "=== UNITTEST_STATUS: TESTS_FAILED ===")
    {
        return Err(Error::Message("guest unit tests did not pass".to_string()));
    }
    Ok(())
}

// debugfs may read a valid profile left by an earlier run. Require this round's
// coverage write + sync confirmation before trusting host-side reports.
fn validate_guest_coverage_persisted(output: &str) -> Result<()> {
    if !has_guest_status(output, "=== UNITTEST_STATUS: COVERAGE_PERSISTED ===") {
        return Err(Error::Message(
            "guest did not confirm coverage write and filesystem sync; refusing stale profile"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn validate_guest_completion(output: &str) -> Result<()> {
    validate_guest_tests_passed(output)?;
    validate_guest_coverage_persisted(output)
}

struct CoverageArtifacts {
    directory: PathBuf,
    profraw: PathBuf,
    profdata: PathBuf,
    text: PathBuf,
    lcov: PathBuf,
    cobertura: PathBuf,
    html: PathBuf,
}

impl CoverageArtifacts {
    fn new(bundle: &Bundle) -> Self {
        let context = &bundle.context;
        let directory = context
            .target_dir
            .join(context.config.target())
            .join(context.config.profile().as_str());
        Self {
            profraw: directory.join("default.profraw"),
            profdata: directory.join("default.profdata"),
            text: directory.join("coverage.txt"),
            lcov: directory.join("coverage.info"),
            cobertura: directory.join("coverage.xml"),
            // Produced by CI `genhtml` beside the other reports; scrub so a
            // coverage-off run cannot leave last run's HTML looking current.
            html: directory.join("coverage-html"),
            directory,
        }
    }

    fn remove_stale_files(&self) -> Result<()> {
        for path in [
            &self.profraw,
            &self.profdata,
            &self.text,
            &self.lcov,
            &self.cobertura,
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
        // Prefer remove_dir_all for the usual genhtml tree. If a prior run left
        // a regular file at this path, remove_dir_all fails with NotADirectory —
        // fall back to remove_file. When remove_file then says "is a directory",
        // the real failure was remove_dir_all (e.g. permission/busy on a dir);
        // otherwise surface the remove_file error (permission on a file, etc.).
        match fs::remove_dir_all(&self.html) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(dir_err) => match fs::remove_file(&self.html) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(file_err) => {
                    let source = if is_directory_io_error(&file_err) {
                        dir_err
                    } else {
                        file_err
                    };
                    return Err(Error::Io {
                        path: self.html.clone(),
                        source,
                    });
                }
            },
        }
        Ok(())
    }
}

fn is_directory_io_error(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::IsADirectory)
        || error.raw_os_error() == Some(libc::EISDIR)
}

fn extract_profraw(bundle: &Bundle, args: &RunArgs, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let disk_image = context.workspace_root.join(&args.disk_image);
    let request = format!(
        "dump {} {}",
        debugfs_quote(GUEST_PROFRAW_PATH),
        debugfs_quote(&artifacts.profraw.to_string_lossy())
    );
    let mut command = Process::new("debugfs", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .args(["-R", &request])
        .arg(&disk_image)
        .run()
        .map_err(|error| {
            Error::Message(format!(
                "failed to extract {GUEST_PROFRAW_PATH} from {}: {error}",
                disk_image.display()
            ))
        })?;
    if !context.dry_run {
        let size_bytes = fs::metadata(&artifacts.profraw)
            .with_path(&artifacts.profraw)?
            .len();
        if size_bytes == 0 {
            return Err(Error::Message(format!(
                "extracted coverage profile is empty: {}",
                artifacts.profraw.display()
            )));
        }
        println!(
            "Extracted coverage profile to {} ({size_bytes} bytes)",
            artifacts.profraw.display()
        );
    }
    Ok(())
}

fn merge_profraw(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    Process::new("rust-profdata", context.dry_run, context.verbosity)
        .current_dir(&context.workspace_root)
        .args(["merge", "-o"])
        .arg(&artifacts.profdata)
        .arg(&artifacts.profraw)
        .run()
}

fn write_text_report(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let mut command = Process::new("rust-cov", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg("report")
        .arg(&context.bundle_elf)
        .arg(format!("--instr-profile={}", artifacts.profdata.display()))
        .arg(format!("--ignore-filename-regex={COVERAGE_IGNORE_REGEX}"))
        .arg(&context.workspace_root)
        .stdout_to_file(&artifacts.text)
        .run()
}

fn export_lcov(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let mut command = Process::new("rust-cov", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg("export")
        .arg(&context.bundle_elf)
        .arg(format!("--instr-profile={}", artifacts.profdata.display()))
        .args(["--format=lcov"])
        .arg(format!("--ignore-filename-regex={COVERAGE_IGNORE_REGEX}"))
        .arg(&context.workspace_root)
        .stdout_to_file(&artifacts.lcov)
        .run()
}

fn write_cobertura_report(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let coverage = parse_file(&artifacts.lcov, &context.workspace_root, &[]).map_err(|error| {
        Error::Message(format!(
            "failed to parse LCOV tracefile {}: {error}",
            artifacts.lcov.display()
        ))
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Message(format!("system clock error: {error}")))?
        .as_secs();
    let pending_output = pending_output_path(&artifacts.cobertura)?;
    let _ = fs::remove_file(&pending_output);
    if let Err(error) =
        coverage_to_file(&pending_output, &coverage, timestamp, RustDemangler::new())
    {
        let _ = fs::remove_file(&pending_output);
        return Err(Error::Message(format!(
            "failed to write Cobertura XML to {}: {error}",
            artifacts.cobertura.display()
        )));
    }
    promote_output(&pending_output, &artifacts.cobertura)?;
    println!(
        "Converted {} to {}",
        artifacts.lcov.display(),
        artifacts.cobertura.display()
    );
    Ok(())
}

fn pending_output_path(output: &Path) -> Result<PathBuf> {
    let file_name = output.file_name().ok_or_else(|| {
        Error::Message(format!(
            "coverage output path has no file name: {}",
            output.display()
        ))
    })?;
    let mut pending_name = OsString::from(file_name);
    pending_name.push(format!(".tmp.{}", std::process::id()));
    Ok(output.with_file_name(pending_name))
}

fn promote_output(pending_output: &Path, output: &Path) -> Result<()> {
    match fs::remove_file(output) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            let _ = fs::remove_file(pending_output);
            return Err(Error::Io {
                path: output.to_path_buf(),
                source,
            });
        }
    }
    if let Err(source) = fs::rename(pending_output, output) {
        let _ = fs::remove_file(pending_output);
        return Err(Error::Io {
            path: output.to_path_buf(),
            source,
        });
    }
    Ok(())
}

fn debugfs_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::{
        debugfs_quote, validate_guest_completion, validate_guest_coverage_persisted,
        validate_guest_tests_passed,
    };

    #[test]
    fn coverage_requires_passing_tests_and_persisted_guest_output() {
        let passed = "=== UNITTEST_STATUS: ALL_TESTS_PASSED ===";
        let persisted = "=== UNITTEST_STATUS: COVERAGE_PERSISTED ===";
        assert!(validate_guest_completion("").is_err());
        assert!(validate_guest_completion(passed).is_err());
        assert!(validate_guest_completion(persisted).is_err());
        assert!(validate_guest_completion(&format!("{passed}\r\n{persisted}\r\n")).is_ok());
        assert!(validate_guest_completion(&format!(
            "{passed}\n=== UNITTEST_STATUS: TESTS_FAILED ===\n{persisted}\n"
        ))
        .is_err());
        assert!(validate_guest_completion(&format!(
            "{passed}\nFailed to write coverage data: Io\nFailed to flush filesystem: Io\n"
        ))
        .is_err());
    }

    #[test]
    fn tests_passed_check_does_not_require_coverage_persisted() {
        let passed = "=== UNITTEST_STATUS: ALL_TESTS_PASSED ===";
        assert!(validate_guest_tests_passed(passed).is_ok());
        assert!(validate_guest_coverage_persisted(passed).is_err());
        assert!(validate_guest_tests_passed("=== UNITTEST_STATUS: TESTS_FAILED ===").is_err());
    }

    #[test]
    fn debugfs_request_values_escape_quotes_and_backslashes() {
        assert_eq!(debugfs_quote(r#"a\b"c"#), r#""a\\b\"c""#);
    }
}
