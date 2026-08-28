// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::Cow, sync::Arc};

use khal::kprintln;
use kvfs::{DirMapping, RwFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsResult};

fn read_status() -> VfsResult<Cow<'static, [u8]>> {
    Ok(Cow::Owned(kwork::stress_status_text().into_bytes()))
}

fn write_command(data: &[u8]) -> VfsResult<()> {
    match kwork::run_stress_command(data) {
        Ok(summary) => {
            kprintln!("{}", summary);
            Ok(())
        }
        Err(error) => {
            kprintln!("kwork stress failed: {:?}", error);
            Err(map_error(error))
        }
    }
}

fn map_error(error: kwork::StressError) -> VfsError {
    match error {
        kwork::StressError::InvalidCommand | kwork::StressError::InvalidArgument => {
            VfsError::InvalidInput
        }
        kwork::StressError::PoolNotReady
        | kwork::StressError::QueueFailed(_)
        | kwork::StressError::FlushFailed(_)
        | kwork::StressError::Timeout
        | kwork::StressError::Unbalanced
        | kwork::StressError::Incomplete
        | kwork::StressError::CaseFailed { .. } => VfsError::ResourceBusy,
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "kwork_stress",
        SimpleFile::new_regular(
            fs,
            RwFile::new(|op| match op {
                SimpleFileOperation::Read => read_status().map(Some),
                SimpleFileOperation::Write(data) => {
                    write_command(data)?;
                    Ok(None)
                }
            }),
        ),
    );
}
