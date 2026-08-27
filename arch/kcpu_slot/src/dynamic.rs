// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::Cell,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ptr::NonNull,
};

use crate::{CpuId, PinCurrentCpu};

/// Error returned when a per-CPU dynamic initializer fails or the chunk is
/// misused.
#[derive(Debug, Eq, PartialEq)]
pub enum SlotInitError<E> {
    /// The chunk has no suitably aligned space left.
    NoSpace,
    /// The backing store or its area stride is not aligned for the requested
    /// type; this is a configuration error, not a space exhaustion.
    Misaligned,
    /// Initialization failed for one CPU.
    Init(E),
}

/// Caller-owned storage from which dynamically initialized per-CPU slots are carved.
///
/// Allocation metadata uses a non-concurrent bump cursor, so a chunk is not
/// shared between CPUs for allocation (it is not `Sync`); multiple dynamic
/// handles may remain alive while later handles are allocated. A failed
/// initializer permanently consumes its reservation; callers should size the
/// arena for retries or construct values before reserving a slot.
pub struct CpuSlotChunk {
    base: NonNull<u8>,
    cpu_count: usize,
    area_stride: usize,
    cursor: Cell<usize>,
    _not_sync: PhantomData<*mut u8>,
}

impl CpuSlotChunk {
    /// Creates a chunk over caller-owned memory.
    ///
    /// # Safety
    /// `base` must point to `cpu_count * area_stride` writable bytes for the
    /// entire lifetime of the chunk. `area_stride` must be aligned for every
    /// `T` that will be allocated from the chunk. No other chunk may alias
    /// that memory.
    pub unsafe fn from_raw_parts(
        base: *mut u8,
        cpu_count: usize,
        area_stride: usize,
    ) -> Option<Self> {
        let base = NonNull::new(base)?;
        if cpu_count == 0 || area_stride == 0 {
            return None;
        }
        let _total_size = cpu_count.checked_mul(area_stride)?;
        Some(Self {
            base,
            cpu_count,
            area_stride,
            cursor: Cell::new(0),
            _not_sync: PhantomData,
        })
    }

    /// Returns the number of CPUs represented by this chunk.
    pub const fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    /// Reserves and initializes one dynamic slot on every CPU.
    ///
    /// The initializer runs once per CPU. If it fails, values already created
    /// are dropped before the error is returned. The reservation itself is not
    /// reclaimed, which keeps live-handle ownership independent of the chunk.
    ///
    /// `init` receives the logical [`CpuId`] it must construct a value for.
    pub fn alloc<T: 'static, E>(
        &self,
        mut init: impl FnMut(CpuId) -> Result<T, E>,
    ) -> Result<DynamicCpuSlot<'_, T>, SlotInitError<E>> {
        let align = mem::align_of::<T>();
        let size = mem::size_of::<T>();
        if !(self.base.as_ptr() as usize).is_multiple_of(align)
            || !self.area_stride.is_multiple_of(align)
        {
            return Err(SlotInitError::Misaligned);
        }
        let (offset, _end) = match self.reserve(size, align) {
            Ok(reservation) => reservation,
            Err(()) => return Err(SlotInitError::NoSpace),
        };

        let mut initialized = 0;
        while initialized < self.cpu_count {
            let cpu = CpuId::new(initialized);
            let ptr = self
                .element_ptr::<T>(offset, initialized)
                .cast::<MaybeUninit<T>>();
            match init(cpu) {
                Ok(value) => {
                    // SAFETY: The chunk bounds and alignment were checked above;
                    // each CPU area is distinct and currently uninitialized.
                    unsafe { ptr.write(MaybeUninit::new(value)) };
                    initialized += 1;
                }
                Err(error) => {
                    for cpu in 0..initialized {
                        // SAFETY: These are exactly the elements initialized above.
                        unsafe { self.element_ptr::<T>(offset, cpu).as_ptr().drop_in_place() };
                    }
                    return Err(SlotInitError::Init(error));
                }
            }
        }
        Ok(DynamicCpuSlot {
            chunk: self,
            offset,
            _marker: PhantomData,
        })
    }

    fn reserve(&self, size: usize, align: usize) -> Result<(usize, usize), ()> {
        let cursor = self.cursor.get();
        let offset = align_up(cursor, align).ok_or(())?;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= self.area_stride)
            .ok_or(())?;
        self.cursor.set(end);
        Ok((offset, end))
    }

    fn element_ptr<T>(&self, offset: usize, cpu: usize) -> NonNull<T> {
        debug_assert!(cpu < self.cpu_count);
        // SAFETY: Callers validate cpu and offset; the chunk contract covers
        // the resulting pointer and alignment is checked by the allocator.
        unsafe {
            NonNull::new_unchecked(
                self.base
                    .as_ptr()
                    .add(cpu * self.area_stride + offset)
                    .cast(),
            )
        }
    }
}

/// Dynamically initialized per-CPU object owned by a [`CpuSlotChunk`].
pub struct DynamicCpuSlot<'a, T: 'static> {
    chunk: &'a CpuSlotChunk,
    offset: usize,
    _marker: PhantomData<T>,
}

impl<T: 'static> DynamicCpuSlot<'_, T> {
    /// Reads the pinned CPU's initialized value.
    ///
    /// # Safety
    /// The caller must hold a CPU pin with a valid CPU ID and obey `T`'s
    /// aliasing requirements.
    pub unsafe fn get<'a>(&'a self, pin: &'a impl PinCurrentCpu) -> Option<&'a T> {
        let cpu = pin.current_cpu().as_usize();
        if cpu >= self.chunk.cpu_count {
            return None;
        }
        // SAFETY: The chunk owns an initialized value at this offset.
        Some(unsafe { &*self.chunk.element_ptr(self.offset, cpu).as_ptr() })
    }

    /// Reads a remote CPU's value when sharing `T` is safe.
    /// The caller must ensure this does not race with another access to the
    /// same CPU's value.
    pub fn get_remote(&self, cpu: CpuId) -> Option<&T>
    where
        T: Sync,
    {
        if cpu.as_usize() >= self.chunk.cpu_count {
            return None;
        }
        // SAFETY: Every CPU value was initialized by `alloc` and remains alive
        // until this handle is dropped.
        Some(unsafe { &*self.chunk.element_ptr(self.offset, cpu.as_usize()).as_ptr() })
    }

    /// Mutably accesses the pinned CPU's value.
    ///
    /// # Safety
    /// The caller must pin execution with a valid CPU ID and provide exclusive
    /// access to this CPU's value.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut<'a>(&'a self, pin: &'a impl PinCurrentCpu) -> Option<&'a mut T> {
        let cpu = pin.current_cpu().as_usize();
        if cpu >= self.chunk.cpu_count {
            return None;
        }
        // SAFETY: The caller guarantees exclusivity.
        Some(unsafe { &mut *self.chunk.element_ptr(self.offset, cpu).as_ptr() })
    }
}

impl<T: 'static> Drop for DynamicCpuSlot<'_, T> {
    fn drop(&mut self) {
        for cpu in 0..self.chunk.cpu_count {
            // SAFETY: Each element was initialized successfully and is dropped
            // exactly once when its owning handle is destroyed.
            unsafe {
                self.chunk
                    .element_ptr::<T>(self.offset, cpu)
                    .as_ptr()
                    .drop_in_place()
            };
        }
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
