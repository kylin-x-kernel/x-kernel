use std::sync::OnceLock;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

// Helper function to check if debug logging is enabled
// Cached to avoid repeated environment lookups
pub fn is_debug_enabled() -> bool {
    static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();
    *DEBUG_ENABLED.get_or_init(|| std::env::var("XCONFIG_DEBUG").is_ok())
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
