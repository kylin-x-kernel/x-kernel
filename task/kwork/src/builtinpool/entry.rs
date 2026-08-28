// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Conversion between workqueue executor entries and worker-pool entries.

use kworkerpool::{
    EntryKey as PoolEntryKey, EntryOwner as PoolEntryOwner, EntryPayload as PoolEntryPayload,
    EntrySource as PoolEntrySource, PoolEntry,
};

pub(crate) fn pool_entry(entry: kworkqueue::ExecutorEntry) -> PoolEntry {
    PoolEntry::new(
        pool_source(entry.binding),
        pool_owner(entry.owner),
        pool_key(entry.key),
        pool_payload(entry.payload),
    )
}

pub(crate) const fn pool_source(binding: kworkqueue::BindingId) -> PoolEntrySource {
    PoolEntrySource::new(binding.get())
}

pub(crate) const fn pool_owner(owner: kworkqueue::EntryOwner) -> PoolEntryOwner {
    PoolEntryOwner::new(owner.get())
}

pub(crate) const fn pool_key(key: kworkqueue::EntryKey) -> PoolEntryKey {
    PoolEntryKey::new(key.get())
}

pub(crate) const fn pool_payload(payload: kworkqueue::EntryPayload) -> PoolEntryPayload {
    PoolEntryPayload::new(payload.get())
}

pub(crate) fn executor_entry(entry: PoolEntry) -> Option<kworkqueue::ExecutorEntry> {
    Some(kworkqueue::ExecutorEntry {
        binding: executor_binding(entry.source())?,
        owner: executor_owner(entry.owner())?,
        key: executor_key(entry.key())?,
        payload: executor_payload(entry.into_payload())?,
    })
}

pub(crate) fn executor_binding(source: PoolEntrySource) -> Option<kworkqueue::BindingId> {
    core::num::NonZeroUsize::new(source.as_usize()).map(kworkqueue::BindingId::new)
}

pub(crate) fn executor_owner(owner: PoolEntryOwner) -> Option<kworkqueue::EntryOwner> {
    core::num::NonZeroUsize::new(owner.as_usize()).map(kworkqueue::EntryOwner::new)
}

pub(crate) fn executor_key(key: PoolEntryKey) -> Option<kworkqueue::EntryKey> {
    core::num::NonZeroUsize::new(key.as_usize()).map(kworkqueue::EntryKey::new)
}

pub(crate) fn executor_payload(payload: PoolEntryPayload) -> Option<kworkqueue::EntryPayload> {
    core::num::NonZeroUsize::new(payload.as_usize()).map(kworkqueue::EntryPayload::new)
}
