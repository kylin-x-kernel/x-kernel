//! Allocation helpers for loading user memory into heap buffers.
extern crate alloc;
use alloc::vec::Vec;

use bytemuck::{AnyBitPattern, Pod, bytes_of, zeroed};

use crate::{MemError, MemImpl, MemResult, VirtMemIo, read_vm_mem};

/// Load a fixed-length vector from user memory without validating pointer.
pub unsafe fn load_vec_unsafe<T>(p: *const T, count: usize) -> MemResult<Vec<T>> {
    let mut v = Vec::with_capacity(count);
    read_vm_mem(p, &mut v.spare_capacity_mut()[..count])?;
    // SAFETY: We have just initialized `count` elements.
    unsafe { v.set_len(count) }
    Ok(v)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// Load a fixed-length vector from user memory.
pub fn load_vec<T: AnyBitPattern>(p: *const T, count: usize) -> MemResult<Vec<T>> {
    // SAFETY: The caller must ensure that `p` is valid for reading `count` elements.
    unsafe { load_vec_unsafe(p, count) }
}

fn check_zero<T: Pod>(v: &T) -> bool {
    bytes_of(v) == bytes_of(&zeroed::<T>())
}

const LIMIT: usize = 128 * 1024;

/// Load elements until a zeroed terminator is found or a length limit is hit.
pub fn load_vec_until_null<T: Pod>(p: *const T) -> MemResult<Vec<T>> {
    if !p.is_aligned() {
        return Err(MemError::InvalidAddr);
    }

    let elem_sz = size_of::<T>();
    let mut res = Vec::new();
    let mut io = MemImpl::new();

    loop {
        const BATCH: usize = 32;

        let base = p.addr() + res.len() * elem_sz;
        let limit = (base + 1).next_multiple_of(BATCH);
        let n = (limit - base) / elem_sz;

        res.reserve(n);
        let dst = &mut res.spare_capacity_mut()[..n];
        io.read_mem(base, dst.as_bytes_mut())?;

        let slc = unsafe { dst.assume_init_ref() };
        let idx = slc.iter().position(check_zero);

        unsafe { res.set_len(res.len() + idx.unwrap_or(n)) };
        if res.len() >= LIMIT / elem_sz {
            return Err(MemError::NameTooLong);
        }

        if idx.is_some() {
            break;
        }
    }
    Ok(res)
}
