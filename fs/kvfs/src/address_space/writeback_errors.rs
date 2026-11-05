// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mapping and filesystem writeback error generations, sampled by each open file.

use crate::{Mutex, VfsError, VfsResult};

struct ErrorState {
    generation: u64,
    error: Option<VfsError>,
    is_seen: bool,
}

/// Owned by a mapping or superblock, independently of open files.
/// Each open file has an independent cursor; checking one never consumes another
/// file's error. Multiple unobserved failures coalesce to the latest error.
/// All operations require sleepable task context without spinlocks or
/// IRQ/preemption-disable guards; no current process is required.
pub struct WritebackErrors {
    state: Mutex<ErrorState>,
}

impl WritebackErrors {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(ErrorState {
                generation: 0,
                error: None,
                is_seen: true,
            }),
        }
    }

    /// Samples a cursor without consuming an unseen error, including at open.
    /// A new observer skips only failures already observed by another cursor.
    pub fn sample(&self) -> u64 {
        let state = self.state.lock();
        if state.is_seen {
            state.generation
        } else {
            state.generation.wrapping_sub(1)
        }
    }

    /// Reads the current generation without inheriting or consuming old errors.
    ///
    /// Bounded operations take this snapshot before waiting for existing work,
    /// then use [`Self::check`] to report failures recorded during the operation.
    /// File-open observers must use [`Self::sample`] to inherit unseen history.
    pub fn current_generation(&self) -> u64 {
        self.state.lock().generation
    }

    /// Records a failed data or filesystem completion before ending page writeback.
    pub fn record(&self, error: VfsError) {
        let mut state = self.state.lock();
        state.generation = state.generation.wrapping_add(1);
        state.error = Some(error);
        state.is_seen = false;
    }

    /// Checks for a failure without consuming it or changing any observer.
    /// Writeback passes check [`Self::current_generation`] snapshots; file sync
    /// advances its own cursor, initially obtained with [`Self::sample`].
    ///
    /// # Errors
    /// Returns the latest failure when the generation differs from `cursor`.
    pub fn check(&self, cursor: u64) -> VfsResult<()> {
        let state = self.state.lock();
        if cursor == state.generation {
            Ok(())
        } else {
            state.error.map_or(Ok(()), Err)
        }
    }

    /// Reports a failure, marks it observed, and advances only this cursor.
    /// Existing observers still see it; later samples skip it. As with any
    /// finite generation counter, an entire wrapping cycle can alias a cursor.
    ///
    /// # Errors
    /// Returns the last recorded writeback error when the generation changed.
    pub fn check_and_advance(&self, cursor: &mut u64) -> VfsResult<()> {
        let mut state = self.state.lock();
        if *cursor == state.generation {
            return Ok(());
        }
        *cursor = state.generation;
        state.is_seen = true;
        state.error.map_or(Ok(()), Err)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/address_space/writeback_errors.rs");
