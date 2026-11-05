// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded in-flight data ownership and task-context completion per inode.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use block::completion::BioResult;
use kspin::SpinNoIrq;
use ksync::Mutex;
use ktask::WaitQueue;
use kworkng::{SubmitError, WorkHandle};

use crate::{Ext4Error, Ext4Result, Ext4WritebackError, Ext4WritebackResult};

type CompletionWork = WorkHandle<Box<dyn Fn() + Send + Sync>>;
type FinishWriteback = Box<dyn Fn(Ext4WritebackResult) + Send + Sync>;
pub(crate) const MAX_INFLIGHT_BATCHES: usize = 4;

struct QueueState {
    sequence: u64,
    slots: [Option<(u64, Arc<WritebackIo>)>; MAX_INFLIGHT_BATCHES],
}

/// The inode owns admission and the worker; each installed request owns its
/// filesystem completion captures until settlement. The worker only weakly
/// references this queue. In-flight captures may retain the inode, but the
/// guaranteed completion removes that temporary ownership cycle.
pub(crate) struct InodeWritebackIo {
    state: Mutex<QueueState>,
    work: Mutex<Option<CompletionWork>>,
    changed: WaitQueue,
}

impl InodeWritebackIo {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                sequence: 0,
                slots: core::array::from_fn(|_| None),
            }),
            work: Mutex::new(None),
            changed: WaitQueue::new(),
        }
    }

    /// Prepares all worker resources before publishing device ownership.
    pub(crate) fn prepare(self: &Arc<Self>, lengths: Vec<usize>) -> Ext4Result<Arc<WritebackIo>> {
        let mut work = self.work.lock();
        if work.is_none() {
            let weak = Arc::downgrade(self);
            let callback: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                if let Some(queue) = weak.upgrade() {
                    queue.complete_ready();
                }
            });
            *work = Some(
                WorkHandle::new(callback)
                    .map_err(|_| Ext4Error::Device(block::DriverError::BadState))?,
            );
        }
        let work = work
            .as_ref()
            .cloned()
            .ok_or(Ext4Error::Device(block::DriverError::BadState))?;
        let results = (0..lengths.len()).map(|_| None).collect();
        Ok(Arc::new(WritebackIo {
            status: SpinNoIrq::new(IoStatus {
                remaining: lengths.len(),
                results,
            }),
            lengths,
            work,
            finish: Mutex::new(None),
        }))
    }

    /// Caller serializes producers with the inode submission mutex. The worker
    /// may release slots concurrently, but never fills one.
    pub(crate) fn has_capacity(&self) -> bool {
        self.state.lock().slots.iter().any(Option::is_none)
    }

    pub(crate) fn wait_capacity(&self) {
        self.changed.wait_until(|| self.has_capacity());
    }

    /// Installs an entirely prepared request before any BIO can complete.
    pub(crate) fn install(&self, io: Arc<WritebackIo>) -> Ext4Result<()> {
        let mut state = self.state.lock();
        let index = state
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(Ext4Error::JournalBusy)?;
        let sequence = state.sequence.checked_add(1).ok_or(Ext4Error::Overflow)?;
        state.sequence = sequence;
        state.slots[index] = Some((sequence, io));
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> u64 {
        self.state.lock().sequence
    }

    /// Waits a finite submission prefix. Later batches cannot extend this wait.
    /// Never hold mount protection or any lock needed by the completion worker.
    pub(crate) fn wait_through(&self, sequence: u64) {
        self.changed.wait_until(|| {
            self.state
                .lock()
                .slots
                .iter()
                .flatten()
                .all(|(submitted, _)| *submitted > sequence)
        });
    }

    /// Called with submission excluded before mapping mutation or release.
    pub(crate) fn drain(&self) {
        self.wait_through(self.snapshot());
    }

    /// Waits for task-context capture release after a synchronous pass. Caller
    /// retains the VFS inode and holds neither submission nor mount protection,
    /// so releasing a request cannot enter final eviction and wait on this call.
    pub(crate) fn flush_completions(&self) -> Ext4Result<()> {
        let work = self.work.lock().clone();
        if let Some(work) = work {
            work.flush()
                .map_err(|_| Ext4Error::Device(block::DriverError::BadState))?;
        }
        Ok(())
    }

    // Device completions may reorder; metadata publication preserves submission order.
    fn complete_ready(&self) {
        // New completions requeue this identity while it runs. Bound one
        // occurrence so an ongoing producer cannot extend a flush forever.
        for _ in 0..MAX_INFLIGHT_BATCHES {
            let ready = self
                .state
                .lock()
                .slots
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| entry.as_ref().map(|(seq, io)| (index, *seq, io)))
                .min_by_key(|(_, sequence, _)| *sequence)
                .filter(|(_, _, io)| io.is_ready())
                .map(|(index, _, io)| (index, io.clone()));
            let Some((index, io)) = ready else { return };
            let finish = io.finish.lock().take();
            if let Some(finish) = &finish {
                finish(io.result());
            }
            self.state.lock().slots[index] = None;
            self.changed.notify_all(false);
            // The callback may still retain io in IRQ context. Remove and
            // destroy all sleeping captures here before its last Arc drops.
            drop(finish);
            // Keep captures alive through removal and wakeup: dropping the last
            // VFS inode reference can enter eviction and wait for submission.
            drop(io);
        }
    }
}

struct IoStatus {
    remaining: usize,
    results: Vec<Option<BioResult>>,
}

/// IRQ/BH only publishes terminal results into preallocated slots. Original BIO
/// buffers and filesystem captures are released by the normal worker, not end-I/O.
pub(crate) struct WritebackIo {
    status: SpinNoIrq<IoStatus>,
    lengths: Vec<usize>,
    work: CompletionWork,
    finish: Mutex<Option<FinishWriteback>>,
}

impl WritebackIo {
    /// Called once, before queue installation or submission of any data BIO.
    pub(crate) fn set_finish(&self, finish: impl Fn(Ext4WritebackResult) + Send + Sync + 'static) {
        *self.finish.lock() = Some(Box::new(finish));
    }

    pub(crate) fn end_io(&self, index: usize, result: BioResult) {
        let is_ready = {
            let mut status = self.status.lock();
            status.results[index] = Some(result);
            status.remaining -= 1;
            status.remaining == 0
        };
        if is_ready {
            match self.work.queue_default(None) {
                Ok(_) | Err(SubmitError::AlreadyQueued) => {}
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: private work is never \
                              disabled or closed; permanent default queue accepts every \
                              initialized CPU; one bounded occurrence per inode cannot overflow \
                              accounting"
                )]
                Err(error) => panic!("prepared writeback completion admission failed: {error:?}"),
            }
        }
    }

    fn is_ready(&self) -> bool {
        self.status.lock().remaining == 0
    }

    fn result(&self) -> Ext4WritebackResult {
        let results = core::mem::take(&mut self.status.lock().results);
        let mut prefix = 0;
        let mut failure = None;
        for (bytes, result) in self.lengths.iter().zip(results) {
            let (result, _buffers) =
                result.ok_or(Ext4Error::Device(block::DriverError::BadState))?;
            if failure.is_none() {
                match result {
                    Ok(()) => prefix += bytes,
                    Err(error) => failure = Some(Ext4Error::Device(error)),
                }
            }
        }
        failure.map_or(Ok(prefix), |error| {
            Err(Ext4WritebackError::new(prefix, error))
        })
    }
}
