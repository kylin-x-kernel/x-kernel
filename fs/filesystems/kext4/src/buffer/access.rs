// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use super::slot::MetadataBufferSlot;
use crate::{Ext4Error, Ext4Result, jbd2::TransactionId};

/// An immutable ext4 metadata block.
#[derive(Clone)]
pub(crate) struct MetadataBuffer {
    pub(super) bytes: Arc<[u8]>,
}

impl AsRef<[u8]> for MetadataBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl MetadataBuffer {
    pub(super) fn into_bytes(self) -> Arc<[u8]> {
        self.bytes
    }
}

/// Journal write access to one metadata buffer.
///
/// This token is the M3 bridge between a future transaction handle and the
/// unique metadata buffer identity. It does not by itself commit anything to
/// stable storage.
#[allow(dead_code)]
pub(crate) struct MetadataWriteAccess {
    pub(super) slot: Arc<MetadataBufferSlot>,
    pub(super) transaction: TransactionId,
}

#[allow(dead_code)]
impl MetadataWriteAccess {
    pub(crate) const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    pub(crate) fn snapshot(&self) -> Ext4Result<MetadataBuffer> {
        self.slot.result()
    }

    pub(crate) fn replace_bytes(&self, bytes: Arc<[u8]>) -> Ext4Result<()> {
        self.slot.replace_bytes(self.transaction, bytes)
    }

    pub(crate) fn update_bytes(
        &self,
        update: impl FnOnce(&[u8], &mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        let snapshot = self.snapshot()?;
        let mut bytes = Vec::from(snapshot.as_ref());
        update(snapshot.as_ref(), &mut bytes)?;
        self.replace_bytes(Arc::from(bytes.into_boxed_slice()))
    }

    pub(crate) fn mark_dirty(&self) -> Ext4Result<()> {
        self.slot.mark_dirty(self.transaction)
    }
}

/// Evidence that a dirty metadata buffer is being checkpointed.
#[allow(dead_code)]
pub(crate) struct MetadataWriteback {
    pub(super) slot: Arc<MetadataBufferSlot>,
    pub(super) transaction: TransactionId,
    pub(super) bytes: Arc<[u8]>,
}

#[allow(dead_code)]
impl MetadataWriteback {
    pub(crate) fn snapshot(&self) -> MetadataBuffer {
        MetadataBuffer {
            bytes: self.bytes.clone(),
        }
    }

    pub(crate) fn finish_checkpoint(self) -> Ext4Result<()> {
        self.slot.finish_writeback(self.transaction)
    }

    pub(crate) fn fail(self, error: Ext4Error) {
        self.slot.fail_writeback(error);
    }
}
