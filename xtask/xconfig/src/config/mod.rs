// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub mod engine;
pub mod generator;
pub mod oldconfig;
pub mod reader;
pub mod writer;

use std::path::Path;

pub use engine::*;
pub use generator::*;
pub use oldconfig::{ConfigChanges, OldConfigLoader};
pub use reader::*;
pub use writer::*;

use crate::error::{KconfigError, Result};

pub(crate) fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(false);
    }
    std::fs::write(path, content).map_err(KconfigError::Io)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::write_if_changed;

    #[test]
    fn unchanged_content_is_not_rewritten() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("generated");

        assert!(write_if_changed(&path, "value\n").unwrap());
        assert!(!write_if_changed(&path, "value\n").unwrap());
        assert!(write_if_changed(&path, "new value\n").unwrap());
    }
}
