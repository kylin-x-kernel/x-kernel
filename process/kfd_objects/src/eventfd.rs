// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `eventfd` object implementation.

use alloc::sync::Arc;
use core::{
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
};

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ktask::future::block_on;
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, poll_file_io};

/// Kernel object implementing eventfd semantics.
pub struct EventFd {
    count: AtomicU64,
    semaphore: bool,
    poll_rx: PollSet,
    poll_tx: PollSet,
}

impl EventFd {
    /// Creates a counter object with the supplied initial value and read mode.
    ///
    /// Semaphore reads return one and decrement by one; ordinary reads return
    /// the entire counter and clear it. The syscall adapter supplies the
    /// ABI-bounded initial value.
    pub fn new(initval: u64, semaphore: bool) -> Arc<Self> {
        Arc::new(Self {
            count: AtomicU64::new(initval),
            semaphore,
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
        })
    }

    /// Create the anonymous-inode file used by eventfd.
    ///
    /// `cred` is captured as the new file's immutable open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits and forwards
    /// errors from anonymous-inode file creation.
    pub fn new_file(
        initval: u64,
        semaphore: bool,
        open_flags: u32,
        cred: Arc<Cred>,
    ) -> KResult<Arc<VfsFile>> {
        let state = Self::new(initval, semaphore);
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[eventfd]",
            Arc::new(EventfdFops),
            state,
            FMode::READ | FMode::WRITE | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the eventfd object attached to an eventfd file.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` when the file has no matching private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }
}

struct EventfdFops;

impl EventfdFops {
    fn state(file: &VfsFile) -> kio::Result<Arc<EventFd>> {
        EventFd::from_file(file)
    }

    async fn read_wait(file: &VfsFile, buf: &mut [u8]) -> kio::Result<usize> {
        let state = Self::state(file)?;
        if buf.len() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        poll_file_io(state.as_ref(), IoEvents::IN, file.is_nonblocking(), || {
            let result = state
                .count
                .fetch_update(Ordering::Release, Ordering::Acquire, |count| {
                    if count > 0 {
                        let dec = if state.semaphore { 1 } else { count };
                        Some(count - dec)
                    } else {
                        None
                    }
                });

            match result {
                Ok(count) => {
                    let value = if state.semaphore { 1 } else { count };
                    buf[..size_of::<u64>()].copy_from_slice(&value.to_ne_bytes());
                    state.poll_tx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        })
        .await
    }

    async fn write_wait(file: &VfsFile, buf: &[u8]) -> kio::Result<usize> {
        let state = Self::state(file)?;
        if buf.len() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        let mut value = [0; size_of::<u64>()];
        value.copy_from_slice(&buf[..size_of::<u64>()]);
        let value = u64::from_ne_bytes(value);
        if value == u64::MAX {
            return Err(KError::InvalidInput);
        }

        poll_file_io(state.as_ref(), IoEvents::OUT, file.is_nonblocking(), || {
            let result = state
                .count
                .fetch_update(Ordering::Release, Ordering::Acquire, |count| {
                    if u64::MAX - count > value {
                        Some(count + value)
                    } else {
                        None
                    }
                });

            match result {
                Ok(_) => {
                    state.poll_rx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        })
        .await
    }
}

impl FileOperations for EventfdFops {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> kio::Result<usize> {
        block_on(Self::read_wait(file, buf))
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> kio::Result<usize> {
        block_on(Self::write_wait(file, buf))
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::state(file)
            .map(|state| state.poll())
            .unwrap_or_else(|_| IoEvents::empty())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(state) = Self::state(file) {
            state.register(context, events)?;
        }
        Ok(())
    }
}

impl Pollable for EventFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let count = self.count.load(Ordering::Acquire);
        events.set(IoEvents::IN, count > 0);
        events.set(IoEvents::OUT, u64::MAX - 1 > count);
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        if events.contains(IoEvents::OUT) {
            context.register(&self.poll_tx)?;
        }
        Ok(())
    }
}

unittest::path_mod_test!(eventfd_tests, "../ut-tests/eventfd.rs");
