// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Arbitrary-address synchronous I/O for backends requiring `u32` alignment.
//!
//! Staging uses fallible ordinary CPU memory, not a DMA allocation or mapping.
//! CPU alignment does not establish a DMA mapping or cache coherency. Before a
//! callback submits DMA, its backend must establish a supported DMA/cache
//! protocol for both direct and staged buffers; this helper cannot supply one.
//! Allocation occurs before the transfer callback; device locks belong inside
//! that callback. These helpers require a context in which allocation is valid.

use core::{
    alloc::Layout,
    mem::{align_of, size_of},
};

use device_res::{CpuBuffer, CpuMemoryOp, ResError};

use crate::{DriverError, DriverResult};

// Bound temporary storage independently of the request length. A device block
// larger than this still requires one complete block; never split a block.
const BOUNCE_BYTES: usize = 64 * 1024;
const WORD_BYTES: usize = size_of::<u32>();

fn chunk_size(block: u64, len: usize, block_size: usize, capacity: u64) -> DriverResult<usize> {
    if block_size == 0 || !block_size.is_multiple_of(WORD_BYTES) || !len.is_multiple_of(block_size)
    {
        return Err(DriverError::InvalidInput);
    }
    let blocks = u64::try_from(len / block_size).map_err(|_| DriverError::InvalidInput)?;
    if block.checked_add(blocks).is_none_or(|end| end > capacity) {
        return Err(DriverError::InvalidInput);
    }
    let bounded = (BOUNCE_BYTES / block_size)
        .max(1)
        .checked_mul(block_size)
        .ok_or(DriverError::InvalidInput)?;
    Ok(len.min(bounded))
}

fn staging(provider: &dyn CpuMemoryOp, bytes: usize) -> DriverResult<CpuBuffer<'_>> {
    let layout =
        Layout::from_size_align(bytes, align_of::<u32>()).map_err(|_| DriverError::InvalidInput)?;
    CpuBuffer::alloc_with(provider, layout).map_err(|error| match error {
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Unsupported => DriverError::Unsupported,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
    })
}

/// Reads arbitrary-address buffers with fallible, provider-owned CPU staging.
///
/// `block_size` must be nonzero and divisible by four; `block` and `capacity`
/// use device-block units. The complete range is checked before any I/O.
/// An aligned, nonempty buffer is passed to `transfer` unchanged, without
/// allocation or an intermediate copy. Otherwise one initialized, word-aligned
/// CPU buffer is reused for chunks of at most 64 KiB or one complete device
/// block, whichever is larger. Only successful chunks are copied to `output`.
///
/// The CPU provider neither maps DMA nor changes cache attributes; staging
/// exposes no device bus address. A DMA `transfer` must first establish a
/// supported mapping, contiguity and cache protocol; a barrier alone is not
/// data-cache maintenance. This applies to aligned callers too. The callback
/// must complete all access to the borrow before returning. Take device locks
/// inside the callback, after allocation.
///
/// # Errors
/// Returns geometry errors, provider acquisition errors mapped to DriverError,
/// or the first I/O error without retry. Allocation failure performs no I/O.
/// Earlier chunks may already be copied; failure is not atomic or rolled back.
/// Aligned I/O can modify its destination before returning an error.
pub fn read_blocks_with_provider(
    provider: &dyn CpuMemoryOp,
    mut block: u64,
    block_size: usize,
    capacity: u64,
    output: &mut [u8],
    mut transfer: impl FnMut(u64, &mut [u8]) -> DriverResult,
) -> DriverResult {
    let chunk = chunk_size(block, output.len(), block_size, capacity)?;
    if output.is_empty() {
        return Ok(());
    }
    if (output.as_ptr() as usize).is_multiple_of(align_of::<u32>()) {
        return transfer(block, output);
    }
    let mut bounce = staging(provider, chunk)?;
    for target in output.chunks_mut(chunk) {
        let source = &mut bounce.as_mut_slice()[..target.len()];
        transfer(block, source)?;
        target.copy_from_slice(source);
        // chunk_size checked the complete range before any transfer.
        block += (target.len() / block_size) as u64;
    }
    Ok(())
}

/// Writes arbitrary-address buffers with fallible, provider-owned CPU staging.
///
/// Geometry, allocation bounds, ownership, direct forwarding and callback
/// lifetime follow [`read_blocks_with_provider`]. Unaligned input is copied
/// into one word-aligned CPU buffer before each transfer; that buffer is reused
/// and released on success or error. No memory is acquired under a device lock
/// taken by the callback. Staging grants no DMA or cache-coherency guarantees.
///
/// # Errors
/// Returns geometry/provider errors or the first transfer error unchanged.
/// Already completed writes are not undone or retried and do not imply durability.
pub fn write_blocks_with_provider(
    provider: &dyn CpuMemoryOp,
    mut block: u64,
    block_size: usize,
    capacity: u64,
    input: &[u8],
    mut transfer: impl FnMut(u64, &[u8]) -> DriverResult,
) -> DriverResult {
    let chunk = chunk_size(block, input.len(), block_size, capacity)?;
    if input.is_empty() {
        return Ok(());
    }
    if (input.as_ptr() as usize).is_multiple_of(align_of::<u32>()) {
        return transfer(block, input);
    }
    let mut bounce = staging(provider, chunk)?;
    for source in input.chunks(chunk) {
        let target = &mut bounce.as_mut_slice()[..source.len()];
        target.copy_from_slice(source);
        transfer(block, target)?;
        block += (source.len() / block_size) as u64;
    }
    Ok(())
}

unittest::path_mod_test!(mod tests, "../ut-tests/word_io.rs");
unittest::path_mod_test!(pub(crate) mod provider_fixture, "../ut-tests/word_io_provider.rs");

/// CPU-only resource provider for kernel integration-test fixtures.
///
/// Uses ordinary owned test memory and no hardware. Production drivers receive
/// their CPU provider explicitly from the host framework instead.
#[cfg(unittest)]
pub fn test_provider() -> &'static dyn CpuMemoryOp {
    &provider_fixture::DEFAULT
}
