// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
    vec,
    vec::Vec,
};

use kerrno::{KError, KResult};

use crate::{Handle, IPC_MAX_MSG_HANDLES};

/// Metadata returned before a message is read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcMsgInfo {
    /// Complete message length in bytes.
    pub len: usize,
    /// Message slot identifier used by `ipc_read_msg` and `ipc_put_msg`.
    pub id: usize,
    /// Number of attached handles available to install into the receiver table.
    pub num_handles: u32,
}

#[derive(Debug)]
enum MsgItemState {
    /// Slot is available for a sender.
    Free,
    /// Slot contains a complete message that has not been claimed yet.
    Filled,
    /// Receiver has called `get`; `put` releases the slot back to `Free`.
    Read,
}

struct MsgItem {
    state: MsgItemState,
    len: usize,
    data: Vec<u8>,
    handles: Vec<Arc<dyn Handle>>,
}

pub(crate) struct ReadMsg {
    pub(crate) data: Vec<u8>,
    pub(crate) handles: Vec<Arc<dyn Handle>>,
}

/// A fixed-slot message queue preserving Trusty message boundaries.
pub(crate) struct IpcMsgQueue {
    items: Vec<MsgItem>,
    num_items: usize,
    item_sz: usize,
    free_list: VecDeque<usize>,
    filled_list: VecDeque<usize>,
    read_list: BTreeSet<usize>,
}

impl IpcMsgQueue {
    pub(crate) fn new(num_items: usize, item_size: usize) -> KResult<Self> {
        if num_items == 0 || item_size == 0 || num_items > u32::MAX as usize {
            return Err(KError::InvalidInput);
        }

        let mut items = Vec::with_capacity(num_items);
        let mut free_list = VecDeque::with_capacity(num_items);
        for id in 0..num_items {
            items.push(MsgItem {
                state: MsgItemState::Free,
                len: 0,
                data: vec![0; item_size],
                handles: Vec::new(),
            });
            free_list.push_back(id);
        }
        Ok(Self {
            items,
            num_items,
            item_sz: item_size,
            free_list,
            filled_list: VecDeque::with_capacity(num_items),
            read_list: BTreeSet::new(),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.filled_list.is_empty()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.free_list.is_empty()
    }

    /// Returns the byte capacity of each message slot.
    pub(crate) fn item_size(&self) -> usize {
        self.item_sz
    }

    pub(crate) fn has_read_messages(&self) -> bool {
        !self.read_list.is_empty()
    }

    pub(crate) fn push(&mut self, data: &[u8], handles: &[Arc<dyn Handle>]) -> KResult<usize> {
        if data.len() > self.item_sz {
            return Err(KError::OutOfRange);
        }
        if handles.len() > IPC_MAX_MSG_HANDLES {
            return Err(KError::OutOfRange);
        }
        if handles.iter().any(|handle| !handle.is_sendable()) {
            return Err(KError::PermissionDenied);
        }
        let id = self.free_list.pop_front().ok_or(KError::WouldBlock)?;
        let item = &mut self.items[id];
        item.data[..data.len()].copy_from_slice(data);
        item.len = data.len();
        item.handles.extend(handles.iter().cloned());
        item.state = MsgItemState::Filled;
        self.filled_list.push_back(id);
        Ok(data.len())
    }

    pub(crate) fn peek_next_filled(&self) -> KResult<IpcMsgInfo> {
        let id = *self.filled_list.front().ok_or(KError::WouldBlock)?;
        let item = &self.items[id];
        debug_assert!(matches!(item.state, MsgItemState::Filled));
        Ok(IpcMsgInfo {
            len: item.len,
            id,
            num_handles: item.handles.len() as u32,
        })
    }

    pub(crate) fn get_filled(&mut self, id: usize) -> KResult {
        if self.filled_list.front().copied() != Some(id) {
            return Err(KError::BadState);
        }
        let filled_id = self.filled_list.pop_front().ok_or(KError::WouldBlock)?;
        debug_assert_eq!(filled_id, id);
        let item = &mut self.items[id];
        debug_assert!(matches!(item.state, MsgItemState::Filled));
        item.state = MsgItemState::Read;
        let inserted = self.read_list.insert(id);
        debug_assert!(inserted);
        Ok(())
    }

    pub(crate) fn get(&mut self) -> KResult<IpcMsgInfo> {
        let info = self.peek_next_filled()?;
        self.get_filled(info.id)?;
        Ok(info)
    }

    pub(crate) fn read(&self, id: usize, offset: usize, out: &mut [u8]) -> KResult<usize> {
        if id >= self.num_items {
            return Err(KError::NotFound);
        }
        let item = &self.items[id];
        if !matches!(item.state, MsgItemState::Read) {
            return Err(KError::BadState);
        }
        if offset > item.len {
            return Err(KError::OutOfRange);
        }
        let len = out.len().min(item.len - offset);
        out[..len].copy_from_slice(&item.data[offset..offset + len]);
        Ok(len)
    }

    pub(crate) fn read_handles(
        &self,
        id: usize,
        max_handles: usize,
    ) -> KResult<Vec<Arc<dyn Handle>>> {
        if max_handles > IPC_MAX_MSG_HANDLES {
            return Err(KError::OutOfRange);
        }
        if id >= self.num_items {
            return Err(KError::NotFound);
        }
        let item = &self.items[id];
        if !matches!(item.state, MsgItemState::Read) {
            return Err(KError::BadState);
        }
        Ok(item.handles.iter().take(max_handles).cloned().collect())
    }

    pub(crate) fn read_with_handles(
        &self,
        id: usize,
        offset: usize,
        max_len: usize,
        max_handles: usize,
    ) -> KResult<ReadMsg> {
        if max_handles > IPC_MAX_MSG_HANDLES {
            return Err(KError::OutOfRange);
        }
        if id >= self.num_items {
            return Err(KError::NotFound);
        }
        let item = &self.items[id];
        if !matches!(item.state, MsgItemState::Read) {
            return Err(KError::BadState);
        }
        if offset > item.len {
            return Err(KError::OutOfRange);
        }

        let len = max_len.min(item.len - offset);
        let mut data = Vec::new();
        data.try_reserve(len).map_err(|_| KError::NoMemory)?;
        data.extend_from_slice(&item.data[offset..offset + len]);

        let handles = item.handles.iter().take(max_handles).cloned().collect();
        Ok(ReadMsg { data, handles })
    }

    pub(crate) fn put(&mut self, id: usize) -> KResult<bool> {
        let was_full = self.is_full();
        if id >= self.num_items {
            return Err(KError::NotFound);
        }
        let item = &mut self.items[id];
        if !matches!(item.state, MsgItemState::Read) {
            return Err(KError::BadState);
        }
        if !self.read_list.remove(&id) {
            return Err(KError::BadState);
        }
        item.state = MsgItemState::Free;
        item.len = 0;
        item.handles.clear();
        self.free_list.push_front(id);
        Ok(was_full)
    }
}
