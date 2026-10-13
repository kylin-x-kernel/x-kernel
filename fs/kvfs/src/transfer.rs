// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered file transfers: one settlement pump, explicit-offset steps.
//!
//! The engine below never touches `f_pos`. [`transfer_files`] and
//! [`copy_file_range`] are the boundary: they resolve the caller's position
//! protocol ([`TransferPosition`]) into explicit start offsets, run one of
//! the step adapters through [`pump`], and publish the results back. Every
//! shape shares the same settlement loop, so "errors after progress return
//! the completed count" and "only the destination-accepted prefix commits"
//! hold in exactly one place.

use alloc::{sync::Arc, vec};

use kerrno::{KError, KResult, LinuxError};

use crate::{FMode, MutexGuard, VfsFile, pipe::PipeObject};

/// Position policy for one side of a transfer. This is the POSIX boundary
/// shim: the engine below only ever sees explicit start offsets.
pub enum TransferPosition<'a> {
    /// The boundary reads `f_pos` under the position lock and publishes the
    /// transferred count back to it, mirroring Linux `fdget_pos`: checks and
    /// copy share the critical section with the position they validated.
    Managed,
    /// An explicit offset: used as the transfer start and advanced by the
    /// transferred count; the open file description's `f_pos` is untouched.
    At(&'a mut u64),
}

/// One resolved side of a file pair: the open file description's `f_pos`
/// under its held lock, or a plain position. A `Managed` side of a
/// non-seekable file has no lockable position and resolves to an inert
/// scratch slot — stream operations ignore positions.
enum PositionSlot<'a> {
    Managed(MutexGuard<'a, u64>),
    At(&'a mut u64),
}

impl PositionSlot<'_> {
    /// The transfer start for this side.
    fn start(&self) -> u64 {
        match self {
            PositionSlot::Managed(guard) => **guard,
            PositionSlot::At(target) => **target,
        }
    }

    /// Publishes the transferred position; completion protocol only.
    fn commit(&mut self, value: u64) {
        match self {
            PositionSlot::Managed(guard) => **guard = value,
            PositionSlot::At(target) => **target = value,
        }
    }
}

/// A resolved file pair. Address-order lock acquisition, same-description
/// sharing and non-seekable inertness are settled once at resolution; the
/// engines below speak only in `(input, output)` terms.
enum PairPositions<'a> {
    Distinct {
        input: PositionSlot<'a>,
        output: PositionSlot<'a>,
    },
    /// Both sides `Managed` on one open file description: a single lock
    /// guards one logical position shared by the pair.
    Shared(PositionSlot<'a>),
}

impl PairPositions<'_> {
    /// The `(input, output)` transfer starts.
    fn starts(&self) -> (u64, u64) {
        match self {
            PairPositions::Distinct { input, output } => (input.start(), output.start()),
            PairPositions::Shared(slot) => {
                let start = slot.start();
                (start, start)
            }
        }
    }

    /// Completion protocol: publish the transferred positions.
    fn commit(&mut self, input_value: u64, output_value: u64) {
        match self {
            PairPositions::Distinct { input, output } => {
                input.commit(input_value);
                output.commit(output_value);
            }
            PairPositions::Shared(slot) => slot.commit(output_value),
        }
    }
}

/// Resolves a file pair's position protocol: `Managed` sides take their
/// position locks in address order, both-`Managed` sides of one open file
/// description share a single lock (one logical position), and `Managed`
/// sides of non-seekable files resolve to inert scratches. `At` sides keep
/// the caller's offset untouched here.
fn resolve_pair_positions<'a>(
    input: &'a Arc<VfsFile>,
    input_position: TransferPosition<'a>,
    output: &'a Arc<VfsFile>,
    output_position: TransferPosition<'a>,
    input_scratch: &'a mut u64,
    output_scratch: &'a mut u64,
) -> PairPositions<'a> {
    let input_at = match input_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let output_at = match output_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    // A `Managed` side needs a lock exactly when its file is seekable.
    let input_lockable = input_at.is_none() && !input.is_stream();
    let output_lockable = output_at.is_none() && !output.is_stream();

    let same_description = Arc::ptr_eq(input, output);
    let (mut first_guard, mut second_guard) = if same_description {
        // One open file description: at most one lock, recorded where either
        // side can claim it. Two `Managed` sides are one logical position.
        let lockable = input_lockable || output_lockable;
        (lockable.then(|| input.position_lock()), None)
    } else {
        // Distinct descriptions lock in address order.
        let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
        let (first, second) = if input_first {
            (input, output)
        } else {
            (output, input)
        };
        let (first_lockable, second_lockable) = if input_first {
            (input_lockable, output_lockable)
        } else {
            (output_lockable, input_lockable)
        };
        let first_guard = first_lockable.then(|| first.position_lock());
        let second_guard = second_lockable.then(|| second.position_lock());
        (first_guard, second_guard)
    };

    // Both sides `Managed` on one description: a single logical position
    // served by the single lock.
    if same_description && input_lockable && output_lockable {
        let guard = first_guard
            .take()
            .expect("shared description took one lock");
        return PairPositions::Shared(PositionSlot::Managed(guard));
    }

    // Which stored guard each side claims. One description: the single lock
    // sits in `first_guard` and either lockable side claims it. Distinct
    // files: the address-order placement, disjoint between the sides.
    let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
    let input_claims_first = same_description || input_first;
    let output_claims_first = same_description || !input_first;
    let mut claim = |first: bool| -> MutexGuard<'a, u64> {
        let stored = if first {
            &mut first_guard
        } else {
            &mut second_guard
        };
        stored.take().expect("a lockable side owns its lock")
    };
    let input_slot = match (input_at, input_lockable) {
        (Some(offset), _) => PositionSlot::At(offset),
        (None, true) => PositionSlot::Managed(claim(input_claims_first)),
        (None, false) => PositionSlot::At(input_scratch),
    };
    let output_slot = match (output_at, output_lockable) {
        (Some(offset), _) => PositionSlot::At(offset),
        (None, true) => PositionSlot::Managed(claim(output_claims_first)),
        (None, false) => PositionSlot::At(output_scratch),
    };
    PairPositions::Distinct {
        input: input_slot,
        output: output_slot,
    }
}

/// One pump step: move up to `chunk` bytes through the shared buffer.
enum Step {
    /// The source is exhausted; stop with the completed prefix. Nothing was
    /// confirmed and no destination side effect happened for this step.
    Eof,
    /// `confirmed` bytes landed at the destination. `stop` ends the pump
    /// after this step — a short source chunk, or a stream source whose
    /// published data must not be followed by a second blocking read.
    Data { confirmed: usize, stop: bool },
}

/// The one settlement loop shared by every transfer shape. Each step moves
/// at most `chunk` bytes through the buffer and reports what the destination
/// confirmed (`total` flows back in so pipe steps can downgrade to
/// non-blocking after the first byte). An error after progress returns the
/// completed prefix; only a zero-progress failure propagates.
fn pump(
    len: usize,
    mut step: impl FnMut(&mut [u8], usize, usize) -> KResult<Step>,
) -> KResult<usize> {
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total = 0;
    let mut remaining = len;
    let mut result = Ok(0);
    while remaining > 0 {
        let chunk = buf.len().min(remaining);
        match step(&mut buf, chunk, total) {
            Ok(Step::Eof) => break,
            Ok(Step::Data { confirmed, stop }) => {
                total += confirmed;
                remaining -= confirmed;
                result = Ok(total);
                if stop || confirmed == 0 {
                    break;
                }
            }
            Err(error) => {
                result = if total > 0 { Ok(total) } else { Err(error) };
                break;
            }
        }
    }
    result
}

/// The buffered file-pair copy step shared by [`transfer_files`] and
/// [`copy_file_range`]: one read at the local source position, one write at
/// the local destination position, the oversized-backend-count guards, and
/// the accepted-prefix rewind. [`pump`] owns the settlement around it.
fn file_pair_step(
    input: &Arc<VfsFile>,
    output: &Arc<VfsFile>,
    input_pos: &mut u64,
    output_pos: &mut u64,
    buf: &mut [u8],
    chunk: usize,
) -> KResult<Step> {
    let mut read_pos = *input_pos;
    let read = input.read_from(&mut buf[..chunk], &mut read_pos)?;
    if read > chunk {
        return Err(KError::Io);
    }
    if read == 0 {
        return Ok(Step::Eof);
    }
    let mut write_pos = *output_pos;
    let written = output.write_from(&buf[..read], &mut write_pos)?;
    if written > read {
        return Err(KError::Io);
    }
    // Only the accepted prefix commits: the tail stays at the source
    // for the next step to re-read.
    *input_pos += written as u64;
    *output_pos = write_pos;
    Ok(Step::Data {
        confirmed: written,
        stop: written < read,
    })
}

/// Transfers up to `len` bytes, committing only bytes accepted by the destination.
///
/// [`TransferPosition::Managed`] sides use the open file descriptions'
/// positions, read and committed under the position locks; explicit offsets
/// are kernel values advanced by this operation, leaving `f_pos` untouched.
/// User-memory copyout and syscall restart decisions belong to the caller.
/// Errors after progress return the completed byte count; a zero-progress
/// wait preserves `IoWaitInterrupted`. File-to-file backend counts must fit
/// the supplied buffers. An oversized result is EIO and does not commit that
/// step's positions; earlier progress is retained. Backend data side effects
/// cannot be rolled back.
///
/// `nonblocking` controls pipe-readiness waits only; backend operations and
/// mutex acquisition may still block. Explicit offsets on streams, and a
/// non-pipe stream source with a non-pipe destination, return `ESPIPE`.
///
/// Runs in sleepable task context. Callers must not hold file-position or pipe
/// locks. The boundary dispatches on shape: a pipe pair negotiates the direct
/// store-to-store exchange, single pipe sides run through the pump with their
/// cursor re-locked per step (never across a pipe-readiness wait), and plain
/// file pairs run through the pump under the locks resolved by
/// `resolve_pair_positions`.
pub fn transfer_files(
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    input.verify_mode(FMode::READ)?;
    output.verify_mode(FMode::WRITE)?;
    let source_pipe = PipeObject::from_file(input).ok();
    let destination_pipe = PipeObject::from_file(output).ok();
    if matches!(&input_position, TransferPosition::At(_)) && input.is_stream() {
        return Err(LinuxError::ESPIPE.into());
    }
    if matches!(&output_position, TransferPosition::At(_)) && output.is_stream() {
        return Err(LinuxError::ESPIPE.into());
    }
    if let (Some(source), Some(destination)) = (&source_pipe, &destination_pipe) {
        return source.splice_to(
            destination,
            len,
            nonblocking || input.is_nonblocking() || output.is_nonblocking(),
        );
    }
    // A non-seekable source needs a destination that accepts its entire read.
    if input.is_stream() && source_pipe.is_none() && destination_pipe.is_none() {
        return Err(LinuxError::ESPIPE.into());
    }
    if let Some(pipe) = &destination_pipe {
        return pump_file_to_pipe(pipe, output, input, input_position, len, nonblocking);
    }
    if let Some(pipe) = &source_pipe {
        return pump_pipe_to_file(pipe, input, output, output_position, len, nonblocking);
    }
    // Plain file pair: the boundary resolves the cursor protocol, the pump
    // runs on local positions, and the results publish back through the
    // resolved slots.
    let mut input_scratch = 0;
    let mut output_scratch = 0;
    let mut positions = resolve_pair_positions(
        input,
        input_position,
        output,
        output_position,
        &mut input_scratch,
        &mut output_scratch,
    );
    let (input_start, output_start) = positions.starts();
    let mut input_pos = input_start;
    let mut output_pos = output_start;
    let result = pump(len, |buf, chunk, _total| {
        file_pair_step(input, output, &mut input_pos, &mut output_pos, buf, chunk)
    });
    positions.commit(input_pos, output_pos);
    result
}

/// File → pipe boundary: the pipe stages each chunk, the source fills it,
/// and the pipe publishes exactly the confirmed prefix. The input's
/// `Managed` position re-locks per step, so the lock never spans a
/// pipe-readiness wait; the pipe side itself is always `Managed` (an `At`
/// position on a stream was rejected upstream).
fn pump_file_to_pipe(
    pipe: &PipeObject,
    output: &Arc<VfsFile>,
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    let input_offset = match input_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let mut input_pos = input_offset.as_deref().copied().unwrap_or(0);
    let input_managed = input_offset.is_none();
    let result = pump(len, |buf, chunk, total| {
        let wait = nonblocking || output.is_nonblocking() || total > 0;
        let confirmed = pipe.splice_write(wait, &mut buf[..chunk], |staged| {
            if input_managed && !input.is_stream() {
                input.read_from(staged, &mut input.position_lock())
            } else {
                input.read_from(staged, &mut input_pos)
            }
        })?;
        // Do not start a second blocking source read after publishing
        // stream data into the pipe; return that completed prefix.
        Ok(Step::Data {
            confirmed,
            stop: confirmed < chunk || input.is_stream(),
        })
    });
    if let Some(offset) = input_offset {
        *offset = input_pos;
    }
    result
}

/// Pipe → file boundary: the pipe stages its data, the sink confirms how
/// much landed, and the pipe consumes exactly that prefix. The output's
/// `Managed` position re-locks per step, so the lock never spans a
/// pipe-readiness wait; the pipe side itself is always `Managed` (an `At`
/// position on a stream was rejected upstream).
fn pump_pipe_to_file(
    pipe: &PipeObject,
    input: &Arc<VfsFile>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    let output_offset = match output_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let mut output_pos = output_offset.as_deref().copied().unwrap_or(0);
    let output_managed = output_offset.is_none();
    let result = pump(len, |buf, chunk, total| {
        let wait = nonblocking || input.is_nonblocking() || total > 0;
        let confirmed = pipe.splice_read(wait, &mut buf[..chunk], |staged| {
            if output_managed && !output.is_stream() {
                output.write_from(staged, &mut output.position_lock())
            } else {
                output.write_from(staged, &mut output_pos)
            }
        })?;
        Ok(Step::Data {
            confirmed,
            stop: confirmed < chunk,
        })
    });
    if let Some(offset) = output_offset {
        *offset = output_pos;
    }
    result
}

/// Runs the full Linux check table in order and returns the effective copy
/// length: directories are rejected with `EISDIR`, every non-regular fd
/// (pipes, sockets, devices) with `EINVAL`, a destination opened `O_APPEND`
/// with `EBADF` (the copy would land at the wrong offset), and a pair the
/// destination cannot serve with `EXDEV`. The range checks then run in
/// Linux's order: an unsigned wrap of the request is reported as
/// `EOVERFLOW`, the request is shortened to the source EOF, and the
/// destination's own ceiling applies — a start at or past the maximum file
/// size is `EFBIG` and the count is clamped to what the destination can
/// hold. Overlapping ranges within one inode are rejected with `EINVAL` so
/// the chunked copy can never read bytes it has already rewritten; a
/// zero-length request never overlaps and remains a no-op.
fn verify_copy_file_range(
    input: &VfsFile,
    output: &VfsFile,
    input_start: u64,
    output_start: u64,
    requested: u64,
) -> KResult<u64> {
    let input_inode = input.inode();
    let output_inode = output.inode();
    if input_inode.node_type() == crate::NodeType::Directory
        || output_inode.node_type() == crate::NodeType::Directory
    {
        return Err(LinuxError::EISDIR.into());
    }
    if input_inode.node_type() != crate::NodeType::RegularFile
        || output_inode.node_type() != crate::NodeType::RegularFile
    {
        return Err(LinuxError::EINVAL.into());
    }
    if output.flags().contains(crate::OpenFlags::APPEND) {
        return Err(LinuxError::EBADF.into());
    }
    // Pair admission mirrors Linux `generic_copy_file_checks`
    // (`fs/read_write.c:1506`) and belongs to the checks, not to the copy
    // loop: Linux decides it before it shortens the request to the source
    // EOF, so an unservable pair still reports `EXDEV` when the shortened
    // request is empty. That empty shape is how `copy_file_range01` probes
    // cross-device support, and answering it with `Ok(0)` would claim a
    // capability the pair does not have. Linux branches on the DESTINATION
    // table — a native primitive judges its own inputs, a table without one
    // falls back to the same-superblock rule. An empty range is the probe:
    // `None` means the table carries no primitive, `Some` means the
    // primitive answered for this input.
    let pair_is_servable = match output.copy_chunk_from(input, input_start, output_start, 0) {
        Some(admission) => {
            admission?;
            true
        }
        None => Arc::ptr_eq(&input_inode.super_block()?, &output_inode.super_block()?),
    };
    if !pair_is_servable {
        return Err(LinuxError::EXDEV.into());
    }
    // Linux reports `EOVERFLOW` when the range wraps its unsigned
    // arithmetic (`pos_in + count < pos_in || pos_out + count <
    // pos_out`); the ABI layer already rejected negative starts.
    // Requests beyond the source EOF are shortened below, not rejected.
    if input_start.checked_add(requested).is_none() || output_start.checked_add(requested).is_none()
    {
        return Err(LinuxError::EOVERFLOW.into());
    }
    // Shorten to the source EOF before the destination ceiling and the
    // overlap check, so a request past EOF is a no-op rather than a
    // false overlap. The size is snapshotted once: a concurrent truncate
    // between two reads would underflow the subtraction.
    let size = input.size();
    let count = if input_start >= size {
        0
    } else {
        requested.min(size - input_start)
    };
    // Linux `generic_write_check_limits`: the destination's own ceiling
    // applies in the checks phase — a start at or past the maximum file
    // size is `EFBIG` even for an already-empty request, and the count
    // is clamped to what the destination can hold from here.
    let output_max = output.max_file_size();
    if output_start >= output_max {
        return Err(LinuxError::EFBIG.into());
    }
    let count = count.min(output_max - output_start);
    // Overlapping ranges within one inode are rejected so the chunked
    // copy can never read bytes it has already rewritten; a zero-length
    // request never overlaps and remains a no-op success.
    if Arc::ptr_eq(input_inode, output_inode)
        && count != 0
        && input_start < output_start.saturating_add(count)
        && output_start < input_start.saturating_add(count)
    {
        return Err(LinuxError::EINVAL.into());
    }
    Ok(count)
}

/// `copy_file_range(2)` boundary: the caller's position protocol resolves
/// into explicit starts under the position locks, `verify_copy_file_range`
/// runs the check table against those starts — pair admission included, so
/// an unservable pair reports `EXDEV` even when the request is empty — and
/// the pump copies between the local positions. Resolution, checks, copy and
/// the position commit share one critical section (Linux `fdget_pos`), so
/// concurrent transfers on a shared open file description neither duplicate
/// ranges nor clobber a concurrent seek. [`TransferPosition::At`] sides run at the given offsets and are
/// advanced by the transferred count; `f_pos` is untouched for them. Like
/// Linux `vfs_copy_file_range`, the copy allows partial success: an error
/// after progress returns the completed prefix and commits it, so the
/// published positions always describe the bytes the destination actually
/// accepted.
///
/// `nonblocking` is accepted for symmetry with [`transfer_files`]; regular
/// file backends ignore it.
pub fn copy_file_range(
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    _nonblocking: bool,
) -> KResult<usize> {
    input.verify_mode(FMode::READ)?;
    output.verify_mode(FMode::WRITE)?;

    let mut input_scratch = 0;
    let mut output_scratch = 0;
    let mut positions = resolve_pair_positions(
        input,
        input_position,
        output,
        output_position,
        &mut input_scratch,
        &mut output_scratch,
    );
    let (input_start, output_start) = positions.starts();
    let count = verify_copy_file_range(input, output, input_start, output_start, len as u64)?;
    let mut input_pos = input_start;
    let mut output_pos = output_start;
    let result = pump(count as usize, |buf, chunk, _total| {
        match output.copy_chunk_from(input, input_pos, output_pos, chunk) {
            Some(native) => {
                let confirmed = native?;
                if confirmed > chunk {
                    return Err(KError::Io);
                }
                input_pos += confirmed as u64;
                output_pos += confirmed as u64;
                Ok(Step::Data {
                    confirmed,
                    stop: confirmed < chunk,
                })
            }
            None => file_pair_step(input, output, &mut input_pos, &mut output_pos, buf, chunk),
        }
    });

    // Position commit belongs to the completion protocol: publish only
    // after real progress — a zero-byte return leaves `f_pos` alone.
    if matches!(result, Ok(count) if count > 0) {
        positions.commit(input_pos, output_pos);
    }
    result
}
