// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered file transfers and pipe-owned source consumption.

use alloc::{sync::Arc, vec};

use kerrno::{KError, KResult, LinuxError};

use crate::{FMode, VfsFile, pipe::PipeObject};

/// Transfers up to `len` bytes, committing only bytes accepted by the destination.
///
/// Explicit offsets are kernel values updated by this operation; absent offsets
/// use the open file descriptions' positions. User-memory copyout and syscall
/// restart decisions belong to the caller. Errors after progress return the
/// completed byte count; a zero-progress wait preserves `IoWaitInterrupted`.
/// File-to-file backend counts must fit the supplied buffers. An oversized
/// result is EIO and does not commit that iteration's positions; earlier
/// progress is retained. Backend data side effects cannot be rolled back.
///
/// `nonblocking` controls pipe-readiness waits only; backend operations and
/// mutex acquisition may still block. Explicit offsets on streams, and a
/// non-pipe stream source with a non-pipe destination, return `ESPIPE`.
///
/// Runs in sleepable task context. Callers must not hold file-position or pipe
/// locks. File positions are locked in address order; pipe paths acquire a
/// non-pipe position only inside the pipe-owned transfer, never across a
/// pipe-readiness wait.
#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: TODO(cg): legacy file transfer; split position-lock \
              setup, the per-chunk copy step and position write-back"
)]
pub fn transfer_files(
    input: &Arc<VfsFile>,
    input_offset: Option<&mut u64>,
    output: &Arc<VfsFile>,
    output_offset: Option<&mut u64>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    input.verify_mode(FMode::READ)?;
    output.verify_mode(FMode::WRITE)?;
    let source_pipe = PipeObject::from_file(input).ok();
    let destination_pipe = PipeObject::from_file(output).ok();
    if input_offset.is_some() && input.is_stream() {
        return Err(LinuxError::ESPIPE.into());
    }
    if output_offset.is_some() && output.is_stream() {
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
    // Pipe-to-file uses the pipe-owned consumption operation below instead.
    if input.is_stream() && source_pipe.is_none() && destination_pipe.is_none() {
        return Err(LinuxError::ESPIPE.into());
    }
    // File-to-file copies lock positions in address order, sharing one guard
    // for the same open description. Pipe paths acquire the non-pipe position
    // only inside the transfer callback, never across a pipe-readiness wait.
    let is_file_pair = source_pipe.is_none() && destination_pipe.is_none();
    let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
    let (first, second) = if input_first {
        (input, output)
    } else {
        (output, input)
    };
    let mut first_pos = (is_file_pair && !first.is_stream()).then(|| first.position_lock());
    let mut second_pos = (is_file_pair && !Arc::ptr_eq(first, second) && !second.is_stream())
        .then(|| second.position_lock());
    let first_start = first_pos.as_deref().copied().unwrap_or(0);
    let second_start = if Arc::ptr_eq(first, second) {
        first_start
    } else {
        second_pos.as_deref().copied().unwrap_or(0)
    };
    let (input_start, output_start) = if input_first {
        (first_start, second_start)
    } else {
        (second_start, first_start)
    };
    let mut input_pos = input_offset.as_deref().copied().unwrap_or(input_start);
    let mut output_pos = output_offset.as_deref().copied().unwrap_or(output_start);
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = len;
    let mut result = Ok(0);
    while remaining > 0 {
        let count = buf.len().min(remaining);
        let transfer = if let Some(pipe) = &destination_pipe {
            pipe.splice_write(
                nonblocking || output.is_nonblocking() || total_written > 0,
                &mut buf[..count],
                |bytes| {
                    if input_offset.is_none() && !input.is_stream() {
                        input.read_from(bytes, &mut input.position_lock())
                    } else {
                        input.read_from(bytes, &mut input_pos)
                    }
                },
            )
        } else if let Some(pipe) = &source_pipe {
            pipe.splice_read(
                nonblocking || input.is_nonblocking() || total_written > 0,
                &mut buf[..count],
                |bytes| {
                    if output_offset.is_none() && !output.is_stream() {
                        output.write_from(bytes, &mut output.position_lock())
                    } else {
                        output.write_from(bytes, &mut output_pos)
                    }
                },
            )
        } else {
            let mut read_pos = input_pos;
            input
                .read_from(&mut buf[..count], &mut read_pos)
                .and_then(|read| {
                    if read > count {
                        return Err(KError::Io);
                    }
                    if read == 0 {
                        return Ok(0);
                    }
                    let mut write_pos = output_pos;
                    let written = output.write_from(&buf[..read], &mut write_pos)?;
                    if written > read {
                        return Err(KError::Io);
                    }
                    // The buffered suffix was never committed to the destination.
                    input_pos += written as u64;
                    output_pos = write_pos;
                    Ok(written)
                })
        };
        match transfer {
            Ok(written) => {
                total_written += written;
                remaining -= written;
                result = Ok(total_written);
                // Do not start a second blocking source read after publishing
                // stream data into the pipe; return that completed prefix.
                if written < count || (input.is_stream() && source_pipe.is_none()) {
                    break;
                }
            }
            Err(error) => {
                result = if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(error)
                };
                break;
            }
        }
    }
    if input_offset.is_none() {
        let position = if input_first {
            &mut first_pos
        } else {
            &mut second_pos
        };
        if let Some(position) = position {
            **position = input_pos;
        }
    }
    if output_offset.is_none() {
        let position = if !input_first || Arc::ptr_eq(input, output) {
            &mut first_pos
        } else {
            &mut second_pos
        };
        if let Some(position) = position {
            **position = output_pos;
        }
    }
    drop(second_pos);
    drop(first_pos);
    if let Some(offset) = input_offset {
        *offset = input_pos;
    }
    if let Some(offset) = output_offset {
        *offset = output_pos;
    }
    result
}
