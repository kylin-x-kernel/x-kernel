// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::sync::OnceLock;

// Helper function to check if debug logging is enabled
// Cached to avoid repeated environment lookups
pub fn is_debug_enabled() -> bool {
    static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();
    *DEBUG_ENABLED.get_or_init(|| std::env::var("XCONFIG_DEBUG").is_ok())
}

/// Validate a debug log path (CWE-22). Reject empty, NUL, control characters,
/// and `..` components so a log path set via `XCONFIG_DEBUG_LOG` (or derived
/// from `HOME`) cannot be made to truncate an arbitrary location via traversal.
fn validate_debug_log_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("debug log path must not be empty".to_string());
    }
    if path.contains('\0') {
        return Err(format!("debug log path must not contain NUL bytes: {path}"));
    }
    if path.chars().any(char::is_control) {
        return Err(format!(
            "debug log path must not contain control characters: {path}"
        ));
    }
    if std::path::Path::new(path)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "debug log path must not contain parent-directory (..) components: {path}"
        ));
    }
    Ok(())
}

// Get debug log file handle (lazily opened)
pub fn debug_log_file() -> Option<&'static std::sync::Mutex<std::fs::File>> {
    static DEBUG_FILE: OnceLock<Option<std::sync::Mutex<std::fs::File>>> = OnceLock::new();
    DEBUG_FILE
        .get_or_init(|| {
            if !is_debug_enabled() {
                return None;
            }

            // Allow configuration via environment variable, otherwise use a secure default
            let log_path = std::env::var("XCONFIG_DEBUG_LOG").unwrap_or_else(|_| {
                // Try user-specific path first, fall back to /tmp with process ID
                if let Ok(home) = std::env::var("HOME") {
                    format!("{}/.xconfig_debug.log", home)
                } else {
                    format!("/tmp/xconfig_debug_{}.log", std::process::id())
                }
            });

            if let Err(err) = validate_debug_log_path(&log_path) {
                eprintln!("Warning: ignoring invalid debug log path '{log_path}': {err}");
                return None;
            }

            let mut options = std::fs::OpenOptions::new();
            options.create(true).write(true).truncate(true);

            // Set restrictive permissions on Unix-like systems
            #[cfg(unix)]
            options.mode(0o600); // Only owner can read/write

            match options.open(&log_path) {
                Ok(file) => Some(std::sync::Mutex::new(file)),
                Err(e) => {
                    // Log error to stderr only if debug is explicitly enabled
                    eprintln!("Warning: Failed to open debug log at '{}': {}", log_path, e);
                    None
                }
            }
        })
        .as_ref()
}

// Helper macro for debug logging to file
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if crate::log::is_debug_enabled() {
            if let Some(file_mutex) = crate::log::debug_log_file() {
                if let Ok(mut file) = file_mutex.lock() {
                    let _ = writeln!(file, $($arg)*);
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::validate_debug_log_path;

    #[test]
    fn accepts_normal_log_paths() {
        assert!(validate_debug_log_path("/tmp/x.log").is_ok());
        assert!(validate_debug_log_path("xconfig_debug.log").is_ok());
    }

    #[test]
    fn rejects_traversal_and_control_chars() {
        assert!(validate_debug_log_path("../evil").is_err());
        assert!(validate_debug_log_path("/tmp/a/../b").is_err());
        assert!(validate_debug_log_path("/tmp/a\nb").is_err());
        assert!(validate_debug_log_path("/tmp/a\0b").is_err());
        assert!(validate_debug_log_path("").is_err());
    }
}
