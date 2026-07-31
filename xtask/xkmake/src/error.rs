// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{io, path::PathBuf, process::ExitStatus};

use thiserror::Error;

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("{0}")]
    Message(String),

    #[error("configuration failed: {0}")]
    Configuration(#[from] xconfig::KconfigError),

    #[error("I/O operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },

    #[error("{program} exited with status {status}")]
    CommandFailed { program: String, status: ExitStatus },

    #[error("failed to serialize bundle manifest: {0}")]
    Manifest(#[from] toml::ser::Error),
}

impl Error {
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::CommandFailed { status, .. } => status.code().unwrap_or(1),
            _ => 1,
        }
    }
}

pub(crate) trait IoResultExt<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}
