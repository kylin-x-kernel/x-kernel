// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framework slots on an open file.
//!
//! Linux lets a kernel subsystem own a file's meaning: a dma-buf descriptor is
//! recognised through its `file->f_op`, and any subsystem that holds the
//! descriptor can recover the object behind it without knowing which filesystem
//! created the descriptor. This module is that capability for X-Kernel: a slot
//! is a kernel-wide handle for one shared object kind, a creator stores its
//! object in the slot of the file it creates, and any subsystem reads it back.
//!
//! A slot is deliberately separate from [`TypeMap`](crate::TypeMap) (the
//! per-filesystem `private_data`): private data is the creating filesystem's own
//! and is keyed by its concrete type, which nobody else can name. A slot is
//! identified by a [`FileSlot`] that every platform crate can name, so the
//! dma-buf exporter and the dma-buf importer agree without either depending on
//! the other.

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;

/// A kernel-wide slot kind on an open file.
///
/// Slots are declared as associated constants and are never compared by
/// pointer identity: two crates holding the same slot name the same storage.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FileSlot(&'static str);

impl FileSlot {
    /// The slot's name, used in diagnostics.
    pub const fn name(self) -> &'static str {
        self.0
    }
}

impl core::fmt::Display for FileSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

/// The shared dma-buf slot: the object behind a dma-buf descriptor.
pub const DMA_BUF_SLOT: FileSlot = FileSlot("dma_buf");

/// The objects stored in one open file's slots.
///
/// Storage is a small inline vector: a file carries at most a couple of shared
/// objects, and allocating for the common one-slot case would be wasteful.
#[derive(Default)]
pub(crate) struct FileSlots(Vec<(FileSlot, Arc<dyn Any + Send + Sync>)>);

impl FileSlots {
    /// Stores `value` in `slot`, replacing any object already there.
    pub(crate) fn insert(&mut self, slot: FileSlot, value: Arc<dyn Any + Send + Sync>) {
        if let Some(existing) = self.0.iter_mut().find(|(name, _)| *name == slot) {
            existing.1 = value;
        } else {
            self.0.push((slot, value));
        }
    }

    /// The object in `slot`, if one is stored.
    pub(crate) fn get(&self, slot: FileSlot) -> Option<Arc<dyn Any + Send + Sync>> {
        self.0
            .iter()
            .find(|(name, _)| *name == slot)
            .map(|(_, value)| value.clone())
    }
}
