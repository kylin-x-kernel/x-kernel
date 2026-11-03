// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};
use core::cell::{Cell, RefCell};

use kerrno::{KResult, LinuxError};

use crate::{QuotaFormat, QuotaId, QuotaInfo, QuotaIo, QuotaState, QuotaType};

pub(super) struct MemoryFiles {
    pub(super) files: RefCell<[Vec<u8>; 3]>,
    pub(super) fail_write_at: Cell<Option<u64>>,
    pub(super) has_short_reads: Cell<bool>,
    pub(super) has_short_writes: Cell<bool>,
    pub(super) writes: Cell<usize>,
}

impl MemoryFiles {
    pub(super) fn new(format: QuotaFormat) -> Self {
        let files = core::array::from_fn(|index| {
            let mut bytes = vec![0; 2048];
            let magic: u32 = [0xd9c0_1f11, 0xd9c0_1927, 0xd9c0_3f14][index];
            bytes[0..4].copy_from_slice(&magic.to_le_bytes());
            let version: u32 = if format == QuotaFormat::VfsV0 { 0 } else { 1 };
            bytes[4..8].copy_from_slice(&version.to_le_bytes());
            bytes[8..12].copy_from_slice(&60_u32.to_le_bytes());
            bytes[12..16].copy_from_slice(&30_u32.to_le_bytes());
            bytes[20..24].copy_from_slice(&2_u32.to_le_bytes());
            bytes
        });
        Self {
            files: RefCell::new(files),
            fail_write_at: Cell::new(None),
            has_short_reads: Cell::new(false),
            has_short_writes: Cell::new(false),
            writes: Cell::new(0),
        }
    }

    pub(super) fn enable_all(&mut self, format: QuotaFormat) -> QuotaInfo<Arc<QuotaType>> {
        let mut quota = QuotaInfo::new();
        for kind in QuotaType::ALL {
            quota
                .enable(
                    self,
                    kind,
                    &Arc::new(kind),
                    format,
                    QuotaState::USAGE_ENABLED | QuotaState::LIMITS_ENABLED,
                )
                .unwrap();
        }
        quota
    }

    pub(super) fn put_u32(&self, kind: QuotaType, offset: usize, value: u32) {
        self.files.borrow_mut()[kind.index()][offset..offset + 4]
            .copy_from_slice(&value.to_le_bytes());
    }

    pub(super) fn u32_at(&self, kind: QuotaType, offset: usize) -> u32 {
        let files = self.files.borrow();
        u32::from_le_bytes(core::array::from_fn(|index| {
            files[kind.index()][offset + index]
        }))
    }
}

impl QuotaIo for MemoryFiles {
    type File = Arc<QuotaType>;

    fn read(&self, kind: &Self::File, offset: u64, buffer: &mut [u8]) -> KResult<usize> {
        if self.has_short_reads.get() {
            return Ok(0);
        }
        let files = self.files.borrow();
        let file = &files[kind.index()];
        let start = offset as usize;
        let count = file.len().saturating_sub(start).min(buffer.len());
        if count != 0 {
            buffer[..count].copy_from_slice(&file[start..start + count]);
        }
        Ok(count)
    }

    fn write(&mut self, kind: &Self::File, offset: u64, buffer: &[u8]) -> KResult<usize> {
        self.writes.set(self.writes.get() + 1);
        if self.fail_write_at.get() == Some(offset) {
            return Err(LinuxError::EIO.into());
        }
        if self.has_short_writes.get() {
            return Ok(0);
        }
        let mut files = self.files.borrow_mut();
        let file = &mut files[kind.index()];
        let start = offset as usize;
        file.resize(file.len().max(start + buffer.len()), 0);
        file[start..start + buffer.len()].copy_from_slice(buffer);
        Ok(buffer.len())
    }

    fn file_len(&self, kind: &Self::File) -> KResult<u64> {
        Ok(self.files.borrow()[kind.index()].len() as u64)
    }
}

pub(super) fn ids(id: u32) -> [QuotaId; 3] {
    QuotaType::ALL.map(|kind| QuotaId::new(kind, id).unwrap())
}
