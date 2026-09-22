// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    // The file module is `crate::tests` (lib.rs declares `mod tests;`), so the
    // crate-root exports — including the `#[macro_export]` macros — resolve
    // through `crate`, not `super`.
    use crate::*;

    cpu_slot! {
        static TEST_SLOT: usize = 0;
    }

    cpu_slot_cell! {
        static TEST_CELL: usize = 0;
    }

    /// A pin that reports a fixed logical CPU and an explicit slot-area base,
    /// for host-side tests. The dynamic accessors only need the CPU identity;
    /// the static accessors read `base()`, so a caller-owned area is supplied.
    struct TestPin {
        cpu: CpuId,
        base: usize,
    }
    // SAFETY: The test never migrates; the reported CPU and base are whatever
    // the caller chose for the duration of the test.
    unsafe impl CpuPin for TestPin {
        fn current_cpu(&self) -> CpuId {
            self.cpu
        }

        fn base(&self) -> usize {
            self.base
        }
    }

    /// A [`TestPin`] that also claims the stronger local-access capability.
    struct TestLocalGuard(TestPin);
    // SAFETY: The test never migrates and never runs IRQ handlers, which is
    // exactly the capability this impl claims.
    unsafe impl CpuPin for TestLocalGuard {
        fn current_cpu(&self) -> CpuId {
            self.0.current_cpu()
        }

        fn base(&self) -> usize {
            self.0.base()
        }
    }
    // SAFETY: See the `CpuPin` impl above.
    unsafe impl CpuLocalGuard for TestLocalGuard {}

    #[test]
    fn area_stride_is_cache_line_aligned() {
        for size in [1usize, 63, 64, 65, 128, 2322, 2368, 4096] {
            let area_stride = stride(size);
            assert!(area_stride >= size);
            assert!(
                area_stride.is_multiple_of(kaddr_layout::CPU_SLOT_AREA_ALIGN),
                "stride({size}) = {area_stride} is not area-aligned"
            );
        }
    }

    #[repr(align(128))]
    struct Backing([u8; 256]);

    #[test]
    fn dynamic_slot_initializes_each_cpu_and_reads_remotely() {
        let mut backing = Backing([0; 256]);
        // SAFETY: `backing` is aligned, writable, and outlives the chunk and
        // dynamic handle; two 128-byte CPU areas fit in the backing store.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 128).unwrap() };
        let slot = chunk
            .alloc::<usize, ()>(|cpu| Ok(cpu.as_usize() + 10))
            .unwrap();
        let second = chunk
            .alloc::<u32, ()>(|cpu| Ok((cpu.as_usize() + 20) as u32))
            .unwrap();
        assert_eq!(slot.get_remote(CpuId::new(0)), Some(&10));
        assert_eq!(slot.get_remote(CpuId::new(1)), Some(&11));
        assert_eq!(slot.get_remote(CpuId::new(2)), None);
        assert_eq!(second.get_remote(CpuId::new(0)), Some(&20));
        assert_eq!(second.get_remote(CpuId::new(1)), Some(&21));

        // The dynamic handle uses the CPU identity carried by the pin; the
        // architecture base is unrelated to this caller-owned chunk.
        let guard = TestLocalGuard(TestPin {
            cpu: CpuId::new(1),
            base: 0,
        });
        assert_eq!(*slot.get(&guard).unwrap(), 11);
        // SAFETY: CPU 1 was initialized by `alloc` and the pin is pinned to it.
        assert_eq!(*unsafe { slot.get_unchecked(&guard.0) }.unwrap(), 11);
    }

    #[test]
    fn static_slot_reads_and_writes_via_pin_base() {
        // SAFETY: `from_offset_fn(0)` is valid only for a base that points at
        // the slot's own cell; the test pins `base` to the area being accessed.
        let slot = unsafe { CpuSlot::<usize>::from_offset_fn(|| 0) };
        let cell = unsafe { CpuSlotCell::<usize>::from_offset_fn(|| 0) };
        // SAFETY: The array is unique, aligned, initialized, and outlives the
        // guard and the accesses below.
        let mut area = [0usize; 8];
        let mut guard = TestLocalGuard(TestPin {
            cpu: CpuId::new(0),
            base: area.as_mut_ptr() as usize,
        });
        // SAFETY: `area` is unique and no other reference to this slot exists.
        *unsafe { slot.get_mut_unchecked(&guard) } = 42;
        assert_eq!(*slot.get(&guard), 42);
        cell.write(&guard, 7);
        assert_eq!(cell.read(&guard), 7);

        // The unchecked entries reach the same storage for a plain pin.
        // SAFETY: `area` remains valid, and the references below do not
        // outlive the exclusive uses in this test.
        unsafe {
            *slot.get_mut_unchecked(&guard.0) = 43;
            assert_eq!(*slot.get_unchecked(&guard.0), 43);
            cell.write_unchecked(&guard.0, 8);
            assert_eq!(cell.read_unchecked(&guard.0), 8);
        }
    }

    #[test]
    fn chunk_reports_misalignment_separately() {
        let mut backing = Backing([0; 256]);
        // SAFETY: `backing` outlives the chunk; the stride below is not a
        // multiple of 8 so the u64 allocation must be rejected as Misaligned.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 100).unwrap() };
        let result = chunk.alloc::<u64, ()>(|_| Ok(0u64));
        assert!(matches!(result, Err(SlotInitError::Misaligned)));
    }

    #[test]
    fn chunk_rejects_stride_that_breaks_area_alignment() {
        // 192 is a multiple of 8 but not of the area alignment, so per-CPU
        // objects would share cache lines across CPUs.
        let mut backing = Backing([0; 256]);
        // SAFETY: `backing` outlives the chunk and provides enough space.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 192).unwrap() };
        assert!(matches!(
            chunk.alloc::<u64, ()>(|_| Ok(0u64)),
            Err(SlotInitError::Misaligned)
        ));
    }

    #[test]
    fn dynamic_handle_is_sync() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<DynamicCpuSlot<'static, usize>>();
    }

    #[test]
    fn chunk_rejects_invalid_backing_arguments() {
        let mut backing = Backing([0; 256]);
        // SAFETY: Null and zero-sized arguments are rejected before any
        // memory access; the non-null pointer refers to the local backing.
        assert!(unsafe { CpuSlotChunk::from_raw_parts(core::ptr::null_mut(), 2, 128) }.is_none());
        assert!(unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 0, 128) }.is_none());
        assert!(unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 0) }.is_none());
    }

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct DropProbe;
    impl Drop for DropProbe {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn failed_dynamic_initialization_rolls_back() {
        DROPS.store(0, Ordering::Relaxed);
        let mut backing = Backing([0; 256]);
        // SAFETY: The backing provides two exclusive CPU areas and outlives
        // the chunk and failed allocation.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 128).unwrap() };
        let result = chunk.alloc::<DropProbe, usize>(|cpu| {
            if cpu.as_usize() == 1 {
                Err(1)
            } else {
                Ok(DropProbe)
            }
        });
        assert!(matches!(result, Err(SlotInitError::Init(1))));
        assert_eq!(DROPS.load(Ordering::Relaxed), 1);
    }
}
