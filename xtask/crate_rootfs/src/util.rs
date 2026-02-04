use std::path::Path;

/// Align up to the next multiple of `align`.
pub fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    (value + align - 1) / align * align
}

/// Ensure the parent directory exists for a file path.
pub fn ensure_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create parent dir {parent:?}: {e}"))?;
        }
    }
    Ok(())
}
