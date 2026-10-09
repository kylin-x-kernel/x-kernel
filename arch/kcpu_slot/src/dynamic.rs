// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::Cell,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ptr::NonNull,
};

use crate::{CpuId, CpuLocalGuard, CpuPin};

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
    ///
    /// # Errors
    /// Returns [`SlotInitError::NoSpace`] when the chunk has no suitably
    /// aligned space left, [`SlotInitError::Misaligned`] when the backing base
    /// or the stride is not area-aligned, and [`SlotInitError::Init`] when
    /// `init` fails for one CPU (values already constructed are dropped).
    pub fn alloc<T: 'static, E>(
        &self,
        mut init: impl FnMut(CpuId) -> Result<T, E>,
    ) -> Result<DynamicCpuSlot<'_, T>, SlotInitError<E>> {
        let align = mem::align_of::<T>();
        let size = mem::size_of::<T>();
        // Requiring the area alignment (not just `align_of::<T>()`) keeps the
        // chunk's per-CPU regions from sharing a cache line, so a lock word in
        // one CPU's object cannot false-share with another CPU's data.
        let base = self.base.as_ptr() as usize;
        if !base.is_multiple_of(align)
            || !self.area_stride.is_multiple_of(align)
            || !base.is_multiple_of(crate::layout::AREA_ALIGN)
            || !self.area_stride.is_multiple_of(crate::layout::AREA_ALIGN)
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
        let element = unsafe {
            self.base
                .as_ptr()
                .add(cpu * self.area_stride + offset)
                .cast()
        };
        // SAFETY: The validated CPU/offset locates an element within the live,
        // non-null chunk allocation, so its address cannot be null.
        unsafe { NonNull::new_unchecked(element) }
    }
}

/// Dynamically initialized per-CPU object owned by a [`CpuSlotChunk`].
pub struct DynamicCpuSlot<'a, T: 'static> {
    chunk: &'a CpuSlotChunk,
    offset: usize,
    _marker: PhantomData<T>,
}

impl<T: 'static> DynamicCpuSlot<'_, T> {
    /// Shares the pinned CPU's initialized value through a guard that excludes
    /// local IRQ reentry.
    ///
    /// Returns `None` when the guard reports a CPU outside the chunk.
    pub fn get<'a>(&'a self, guard: &'a impl CpuLocalGuard) -> Option<&'a T> {
        // SAFETY: The guard pins this CPU and excludes local IRQ reentry for
        // the duration of `'a`, so no conflicting access can overlap the
        // returned reference.
        unsafe { self.get_unchecked(guard) }
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

    /// Reads the pinned CPU's initialized value through a plain
    /// [`CpuPin`].
    ///
    /// # Safety
    /// The caller must hold a CPU pin with a valid CPU ID and obey `T`'s
    /// aliasing requirements, including exclusion of local IRQ handlers that
    /// can access this value.
    pub unsafe fn get_unchecked<'a>(&'a self, pin: &'a impl CpuPin) -> Option<&'a T> {
        let cpu = pin.current_cpu().as_usize();
        if cpu >= self.chunk.cpu_count {
            return None;
        }
        // SAFETY: The chunk owns an initialized value at this offset.
        Some(unsafe { &*self.chunk.element_ptr(self.offset, cpu).as_ptr() })
    }

    /// Mutably accesses the pinned CPU's value through a plain
    /// [`CpuPin`].
    ///
    /// # Safety
    /// The caller must pin execution with a valid CPU ID and provide exclusive
    /// access to this CPU's value, including exclusion of local IRQ handlers
    /// that can access it.
    #[expect(
        clippy::mut_from_ref,
        reason = "CG-024-lint-exceptions: clippy::mut_from_ref: The unsafe caller guarantees \
                  exclusive access to the pinned CPU slot for the returned lifetime"
    )]
    pub unsafe fn get_mut_unchecked<'a>(&'a self, pin: &'a impl CpuPin) -> Option<&'a mut T> {
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

// SAFETY: A handle's fields are immutable once `alloc` returns. Each thread
// only observes its own CPU's value through the supplied guard, so sharing
// `&DynamicCpuSlot<T>` across threads never shares one `T` instance and
// requires only `T: Send`; the one accessor that does share a value across
// threads (`get_remote`) independently requires `T: Sync`.
unsafe impl<T: Send + 'static> Sync for DynamicCpuSlot<'_, T> {}

// SAFETY: The handle uniquely owns its per-CPU values (its `Drop` runs
// exactly once, wherever the owner moves it), so moving it between threads
// moves `T` values and requires only `T: Send`. The shared `&CpuSlotChunk`
// is only read through the immutable element addressing that post-allocation
// access uses; the allocation-time bump cursor that makes `CpuSlotChunk`
// `!Sync` is never touched through a handle.
unsafe impl<T: Send + 'static> Send for DynamicCpuSlot<'_, T> {}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
